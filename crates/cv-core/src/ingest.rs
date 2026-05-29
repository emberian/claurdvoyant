//! In-memory ingestion of an uploaded harness file tree — no filesystem access, so it works on
//! wasm32 (the browser app). Given `(relative_path, bytes)` pairs, sniff each file's harness by
//! content and parse it into IR [`Session`]s.
//!
//! OWNED BY the wasm-ingest work. `cv-core::lib` declares this module.

use crate::harness::{claude, codex, gemini};
use crate::ir::Session;
use serde_json::Value;

/// Which single-file harness a given file looks like.
enum Sniff {
    Codex { is_jsonl: bool },
    Claude,
    Gemini,
    Unknown,
}

/// The file stem (basename without extension), used as a fallback session id.
fn file_stem(name: &str) -> &str {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    match base.rsplit_once('.') {
        Some((stem, _ext)) if !stem.is_empty() => stem,
        _ => base,
    }
}

fn first_non_empty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|l| !l.is_empty())
}

/// Sniff the harness from filename + content. More robust than relying on the path layout, since an
/// uploaded zip may have been re-rooted or renamed.
fn sniff(name: &str, text: &str) -> Sniff {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let lower = base.to_ascii_lowercase();
    let is_jsonl = lower.ends_with(".jsonl");

    // --- Gemini: a top-level JSON array whose items have sessionId + messageId + message.
    if let Some(first) = first_non_empty_line(text) {
        if first.starts_with('[') {
            if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(text) {
                let looks_gemini = items.iter().take(8).any(|it| {
                    it.get("sessionId").is_some()
                        && it.get("messageId").is_some()
                        && it.get("message").is_some()
                });
                if looks_gemini {
                    return Sniff::Gemini;
                }
            }
        }
    }

    // --- Codex: filename, session_meta, or bare {id,timestamp} header + response_item/event_msg.
    if lower.starts_with("rollout-") {
        return Sniff::Codex { is_jsonl };
    }
    if let Some(first) = first_non_empty_line(text) {
        if first.contains("\"session_meta\"") {
            return Sniff::Codex { is_jsonl };
        }
        if let Ok(v) = serde_json::from_str::<Value>(first) {
            // bare {id, timestamp} header with no "type"
            let is_header = v.get("type").is_none()
                && v.get("id").is_some()
                && v.get("timestamp").is_some();
            if is_header
                && (text.contains("\"response_item\"") || text.contains("\"event_msg\""))
            {
                return Sniff::Codex { is_jsonl };
            }
        }
    }

    // --- Claude: JSONL lines with parentUuid/sessionId and type user|assistant.
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()).take(8) {
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            let has_thread = v.get("parentUuid").is_some() || v.get("sessionId").is_some();
            let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
            if has_thread && matches!(ty, "user" | "assistant") {
                return Sniff::Claude;
            }
        }
    }

    Sniff::Unknown
}

/// Pull a session id out of a Codex transcript's meta/header if present, else fall back to stem.
fn codex_id(name: &str, text: &str) -> String {
    if let Some(first) = first_non_empty_line(text) {
        if let Ok(v) = serde_json::from_str::<Value>(first) {
            // session_meta record: { type: "session_meta", payload: { id, .. } }
            if let Some(id) = v.pointer("/payload/id").and_then(Value::as_str) {
                return id.to_string();
            }
            // bare header: { id, timestamp, .. }
            if v.get("type").is_none() {
                if let Some(id) = v.get("id").and_then(Value::as_str) {
                    return id.to_string();
                }
            }
        }
    }
    file_stem(name).to_string()
}

/// Ingest a set of in-memory files (e.g. the contents of an uploaded zip) into sessions.
///
/// Each file's bytes are decoded as UTF-8 (lossy) and its harness sniffed by content. Files that
/// can't be classified are skipped. Works without any filesystem access (wasm-safe).
pub fn ingest_files(files: Vec<(String, Vec<u8>)>) -> Vec<Session> {
    let mut out = Vec::new();
    for (name, bytes) in files {
        let text = String::from_utf8_lossy(&bytes);
        match sniff(&name, &text) {
            Sniff::Codex { is_jsonl } => {
                let id = codex_id(&name, &text);
                out.push(codex::parse_str(&id, &text, is_jsonl, None));
            }
            Sniff::Claude => {
                let id = file_stem(&name).to_string();
                out.push(claude::parse_str(&id, &text, None));
            }
            Sniff::Gemini => {
                out.extend(gemini::parse_all_str(&text, None));
            }
            Sniff::Unknown => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Harness;

    const CLAUDE_SAMPLE: &str = r#"{"type":"user","sessionId":"abc","parentUuid":null,"uuid":"u1","cwd":"/home/x","timestamp":"2025-01-01T00:00:00Z","message":{"role":"user","content":"hello"}}
{"type":"assistant","sessionId":"abc","parentUuid":"u1","uuid":"u2","timestamp":"2025-01-01T00:00:01Z","message":{"role":"assistant","model":"claude-x","content":[{"type":"text","text":"hi there"}]}}"#;

    const CODEX_SAMPLE: &str = r#"{"timestamp":"2025-01-02T00:00:00Z","type":"session_meta","payload":{"id":"sess-123","cwd":"/work","timestamp":"2025-01-02T00:00:00Z"}}
{"timestamp":"2025-01-02T00:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"do a thing"}}
{"timestamp":"2025-01-02T00:00:02Z","type":"event_msg","payload":{"type":"agent_message","message":"done"}}"#;

    #[test]
    fn ingests_claude_and_codex() {
        let files = vec![
            ("abc.jsonl".to_string(), CLAUDE_SAMPLE.as_bytes().to_vec()),
            (
                "rollout-2025-01-02-sess.jsonl".to_string(),
                CODEX_SAMPLE.as_bytes().to_vec(),
            ),
        ];
        let sessions = ingest_files(files);
        assert_eq!(sessions.len(), 2);

        let claude = sessions
            .iter()
            .find(|s| s.harness == Harness::Claude)
            .expect("claude session");
        assert_eq!(claude.id, "abc");
        assert_eq!(claude.messages.len(), 2);

        let codex = sessions
            .iter()
            .find(|s| s.harness == Harness::Codex)
            .expect("codex session");
        assert_eq!(codex.id, "sess-123");
        assert_eq!(codex.messages.len(), 2);
    }

    #[test]
    fn skips_unclassifiable() {
        let files = vec![("random.txt".to_string(), b"just some text".to_vec())];
        assert!(ingest_files(files).is_empty());
    }
}
