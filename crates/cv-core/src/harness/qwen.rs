//! Qwen Code CLI adapter — Qwen Code is a fork of gemini-cli and uses the **same** on-disk
//! session/checkpoint format, just rooted at `~/.qwen` instead of `~/.gemini`:
//!
//! - `~/.qwen/tmp/<projectHash>/logs.json` — readable fallback array of
//!   `{sessionId, messageId, type, message, timestamp}` entries (many sessions per file).
//! - `~/.qwen/tmp/<projectHash>/chats/session-<ts>-<id>.json` (legacy whole-file `ConversationRecord`)
//!   or `.jsonl` (modern append-only: metadata line + message records + `$set`/`$rewindTo` controls),
//!   plus subagent recordings under `chats/<parentId>/<id>.jsonl`.
//! - `~/.qwen/tmp/<projectHash>/checkpoint-<tag>.json` — `{history: Content[]}` or a bare `Content[]`.
//!
//! Because the format is identical, this adapter delegates all parsing to
//! [`crate::harness::gemini::parse_all_str`] and then rewrites the resulting sessions'
//! `harness` to [`Harness::Qwen`]. Missing dirs degrade gracefully to empty results.

use super::Adapter;
use crate::ir::*;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct Qwen {
    root: Option<PathBuf>,
}

impl Qwen {
    pub fn new() -> Self {
        let root = dirs::home_dir()
            .map(|h| h.join(".qwen").join("tmp"))
            .filter(|p| p.exists());
        Qwen { root }
    }
}

impl Default for Qwen {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for Qwen {
    fn harness(&self) -> Harness {
        Harness::Qwen
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.root.clone()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(root) = &self.root else {
            return Ok(vec![]);
        };
        let mut out = Vec::new();
        for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if !is_session_file(path) {
                continue;
            }
            if let Ok(text) = fs::read_to_string(path) {
                for s in parse_qwen_str(&text, Some(path.to_path_buf())) {
                    out.push(session_ref(&s, path));
                }
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        let text = fs::read_to_string(&r.path)
            .with_context(|| format!("reading {}", r.path.display()))?;
        let sessions = parse_qwen_str(&text, Some(r.path.clone()));
        // logs.json holds many sessions; chat recordings / checkpoints hold one.
        if let Some(s) = sessions.iter().find(|s| s.id == r.id).cloned() {
            return Ok(s);
        }
        if let Some(s) = sessions.into_iter().next() {
            return Ok(s);
        }
        anyhow::bail!("could not parse Qwen session {} from {}", r.id, r.path.display())
    }

    fn can_emit(&self) -> bool {
        false
    }
}

/// Parse Qwen session text by delegating to the (identical) Gemini parser, then re-tag every
/// resulting session as [`Harness::Qwen`].
fn parse_qwen_str(text: &str, source_path: Option<PathBuf>) -> Vec<Session> {
    let mut sessions = crate::harness::gemini::parse_all_str(text, source_path);
    for s in &mut sessions {
        s.harness = Harness::Qwen;
    }
    sessions
}

/// Whether a file under `~/.qwen/tmp` is one of the shapes we parse: `logs.json`, a chat recording
/// (`chats/…json|jsonl`), or a `checkpoint-*.json`.
fn is_session_file(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    if name == "logs.json" {
        return true;
    }
    if name.starts_with("checkpoint-") && name.ends_with(".json") {
        return true;
    }
    let in_chats = path
        .components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("chats"));
    in_chats
        && matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("json") | Some("jsonl")
        )
}

fn session_ref(s: &Session, path: &Path) -> SessionRef {
    SessionRef {
        id: s.id.clone(),
        harness: Harness::Qwen,
        path: path.to_path_buf(),
        cwd: s.cwd.clone(),
        title: s.title.clone(),
        created_at: s.created_at,
        updated_at: s.updated_at,
        message_count: s.messages.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        // Reuse the Gemini fixtures — Qwen shares the on-disk format.
        let path = format!(
            "{}/tests/fixtures/gemini/{}",
            env!("CARGO_MANIFEST_DIR"),
            name
        );
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"))
    }

    #[test]
    fn parses_logs_json_as_qwen() {
        let text = fixture("logs.json");
        let mut sessions = parse_qwen_str(&text, None);
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().all(|s| s.harness == Harness::Qwen));
        assert_eq!(sessions[0].id, "sess-a");
    }

    #[test]
    fn parses_legacy_chat_recording_as_qwen() {
        let text = fixture("session_legacy.json");
        let sessions = parse_qwen_str(&text, None);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.harness, Harness::Qwen);
        assert_eq!(s.id, "9aeb2942-7c46-47b7-aded-13772d4d4e63");
        assert!(!s.messages.is_empty());
    }

    #[test]
    fn parses_modern_jsonl_as_qwen() {
        let text = fixture("session_modern.jsonl");
        let sessions = parse_qwen_str(&text, None);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].harness, Harness::Qwen);
        assert_eq!(sessions[0].id, "11112222-3333-4444-5555-666677778888");
    }

    #[test]
    fn tolerates_garbage() {
        assert!(parse_qwen_str("not json", None).is_empty());
        assert!(parse_qwen_str("{}", None).is_empty());
    }

    #[test]
    fn is_session_file_detection() {
        assert!(is_session_file(Path::new("/x/tmp/h/logs.json")));
        assert!(is_session_file(Path::new("/x/tmp/h/checkpoint-foo.json")));
        assert!(is_session_file(Path::new("/x/tmp/h/chats/session-1-2.json")));
        assert!(is_session_file(Path::new("/x/tmp/h/chats/session-1-2.jsonl")));
        assert!(is_session_file(Path::new("/x/tmp/h/chats/parent/child.jsonl")));
        assert!(!is_session_file(Path::new("/x/tmp/h/other.json")));
        assert!(!is_session_file(Path::new("/x/tmp/h/notes.txt")));
    }
}
