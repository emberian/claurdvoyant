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

    // --- Gemini: several distinct readable shapes, all routed to `gemini::parse_all_str`.
    // We classify these *before* Claude/Codex because their markers (`parts`/`role`,
    // `projectHash`, `$set`/`$rewindTo`) are unambiguous and don't overlap with the
    // line-delimited `parentUuid`/`session_meta` shapes used by Claude/Codex.
    if let Some(first) = first_non_empty_line(text) {
        // logs.json array, a bare checkpoint `Content[]`, or a top-level object.
        if first.starts_with('[') {
            if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(text) {
                // logs.json: items with sessionId + messageId + message.
                let looks_logs = items.iter().take(8).any(|it| {
                    it.get("sessionId").is_some() && it.get("messageId").is_some() && it.get("message").is_some()
                });
                // bare checkpoint: a top-level array of `{role, parts}` Content objects.
                let looks_checkpoint = items
                    .iter()
                    .take(8)
                    .any(|it| it.get("role").is_some() && it.get("parts").is_some());
                if looks_logs || looks_checkpoint {
                    return Sniff::Gemini;
                }
            }
        } else if first.starts_with('{') {
            // A *whole-file* JSON object: either a legacy `ConversationRecord` recording
            // (`messages` + `sessionId`/`projectHash`) or a checkpoint (`{history:[{role,parts}…]}`).
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) {
                let looks_recording =
                    map.contains_key("messages") && (map.contains_key("sessionId") || map.contains_key("projectHash"));
                let looks_checkpoint = map
                    .get("history")
                    .and_then(Value::as_array)
                    .map(|h| {
                        h.iter()
                            .take(8)
                            .any(|it| it.get("role").is_some() && it.get("parts").is_some())
                    })
                    .unwrap_or(false);
                if looks_recording || looks_checkpoint {
                    return Sniff::Gemini;
                }
            }

            // A modern append-only `.jsonl` recording: a whole-file object parse fails (multi-line),
            // so we inspect the first line and (if needed) scan a few lines for gemini markers.
            if is_jsonl {
                if let Ok(v) = serde_json::from_str::<Value>(first) {
                    // First line is a metadata record (projectHash/sessionId) or a message record.
                    let first_is_gemini = v.get("projectHash").is_some()
                        || (v.get("sessionId").is_some() && v.get("messages").is_some())
                        || v.get("messages").is_some();
                    if first_is_gemini {
                        return Sniff::Gemini;
                    }
                }
                // Otherwise look for gemini per-message / control markers in the early lines.
                let line_is_gemini =
                    text.lines().map(str::trim).filter(|l| !l.is_empty()).take(8).any(
                        |line| match serde_json::from_str::<Value>(line) {
                            Ok(v) => {
                                v.get("$set").is_some()
                                    || v.get("$rewindTo").is_some()
                                    || v.get("projectHash").is_some()
                                    || v.get("thoughts").is_some()
                                    || v.get("toolCalls").is_some()
                                    || ((v.get("parts").is_some()) && v.get("parentUuid").is_none())
                            }
                            Err(_) => false,
                        },
                    );
                if line_is_gemini {
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
            let is_header = v.get("type").is_none() && v.get("id").is_some() && v.get("timestamp").is_some();
            if is_header && (text.contains("\"response_item\"") || text.contains("\"event_msg\"")) {
                return Sniff::Codex { is_jsonl };
            }
        }
    }

    // --- Claude: JSONL lines with parentUuid/sessionId and type user|assistant. Real transcripts
    // can open with a long run of meta records (`summary` compaction pointers,
    // `file-history-snapshot`, queue/UI bookkeeping — the same set claude.rs treats as
    // non-conversational), so those don't consume the 8-line sniff window; a hard line cap keeps
    // the scan bounded on meta-only or foreign files.
    let mut inspected = 0;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()).take(64) {
        if inspected >= 8 {
            break;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(
                ty,
                "summary"
                    | "file-history-snapshot"
                    | "ai-title"
                    | "mode"
                    | "permission-mode"
                    | "last-prompt"
                    | "attachment"
                    | "progress"
                    | "started"
                    | "result"
                    | "queue-operation"
                    | "x-quota"
            ) {
                continue; // known Claude meta record — doesn't count against the window
            }
            let has_thread = v.get("parentUuid").is_some() || v.get("sessionId").is_some();
            if has_thread && matches!(ty, "user" | "assistant") {
                return Sniff::Claude;
            }
        }
        inspected += 1;
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
    fn claude_with_many_leading_meta_records_still_sniffs() {
        // Real transcripts can open with >8 summary / file-history-snapshot records; they must not
        // exhaust the sniff window and drop the upload as Unknown.
        let mut lines: Vec<String> = (0..9)
            .map(|i| format!(r#"{{"type":"summary","summary":"topic {i}","leafUuid":"l{i}"}}"#))
            .collect();
        lines.push(r#"{"type":"file-history-snapshot","messageId":"m0","snapshot":{"trackedFileBackups":{}}}"#.into());
        lines.push(CLAUDE_SAMPLE.to_string());
        let text = lines.join("\n");

        let sessions = ingest_files(vec![("abc.jsonl".to_string(), text.into_bytes())]);
        assert_eq!(sessions.len(), 1, "meta-heavy claude transcript should ingest");
        assert_eq!(sessions[0].harness, Harness::Claude);
        assert_eq!(sessions[0].messages.len(), 2);
    }

    #[test]
    fn skips_unclassifiable() {
        let files = vec![("random.txt".to_string(), b"just some text".to_vec())];
        assert!(ingest_files(files).is_empty());
    }

    // A legacy whole-file ConversationRecord object (sessionId + projectHash + messages).
    const GEMINI_LEGACY_RECORDING: &str = r#"{"sessionId":"g-legacy-1","projectHash":"deadbeef","startTime":"2025-03-01T00:00:00Z","lastUpdated":"2025-03-01T00:01:00Z","messages":[{"id":"m1","type":"user","timestamp":"2025-03-01T00:00:00Z","content":"hi gemini"},{"id":"m2","type":"gemini","timestamp":"2025-03-01T00:00:30Z","model":"gemini-3-flash","content":[{"text":"hello back"}]}]}"#;

    // A modern append-only .jsonl recording: metadata line + message records + $set + $rewindTo.
    const GEMINI_MODERN_JSONL: &str = r#"{"sessionId":"g-modern-1","projectHash":"cafef00d","startTime":"2025-03-02T00:00:00Z"}
{"id":"u1","type":"user","timestamp":"2025-03-02T00:00:00Z","content":"list files"}
{"id":"a1","type":"gemini","timestamp":"2025-03-02T00:00:10Z","model":"gemini-3","content":[{"text":"sure"}],"toolCalls":[{"id":"t1","name":"list_directory","args":{},"result":"main.py"}]}
{"$set":{"summary":"Listed the project files."}}"#;

    // A checkpoint: {history: Content[]} with {role, parts}.
    const GEMINI_CHECKPOINT: &str =
        r#"{"history":[{"role":"user","parts":[{"text":"what is 2+2"}]},{"role":"model","parts":[{"text":"4"}]}]}"#;

    #[test]
    fn ingests_gemini_legacy_recording() {
        let files = vec![(
            "session-123.json".to_string(),
            GEMINI_LEGACY_RECORDING.as_bytes().to_vec(),
        )];
        let sessions = ingest_files(files);
        assert!(!sessions.is_empty(), "legacy recording should yield ≥1 session");
        assert!(sessions.iter().all(|s| s.harness == Harness::Gemini));
        assert_eq!(sessions[0].id, "g-legacy-1");
    }

    #[test]
    fn ingests_gemini_modern_jsonl() {
        let files = vec![("session-456.jsonl".to_string(), GEMINI_MODERN_JSONL.as_bytes().to_vec())];
        let sessions = ingest_files(files);
        assert!(!sessions.is_empty(), "modern jsonl recording should yield ≥1 session");
        assert!(sessions.iter().all(|s| s.harness == Harness::Gemini));
        assert_eq!(sessions[0].id, "g-modern-1");
    }

    #[test]
    fn ingests_gemini_checkpoint() {
        let files = vec![(
            "checkpoint-test.json".to_string(),
            GEMINI_CHECKPOINT.as_bytes().to_vec(),
        )];
        let sessions = ingest_files(files);
        assert!(!sessions.is_empty(), "checkpoint should yield ≥1 session");
        assert!(sessions.iter().all(|s| s.harness == Harness::Gemini));
    }

    #[test]
    fn gemini_shapes_dont_regress_claude_codex() {
        // Claude and Codex samples must still classify correctly alongside the gemini widening.
        let files = vec![
            ("abc.jsonl".to_string(), CLAUDE_SAMPLE.as_bytes().to_vec()),
            (
                "rollout-2025-01-02-sess.jsonl".to_string(),
                CODEX_SAMPLE.as_bytes().to_vec(),
            ),
        ];
        let sessions = ingest_files(files);
        assert!(sessions.iter().any(|s| s.harness == Harness::Claude));
        assert!(sessions.iter().any(|s| s.harness == Harness::Codex));
        assert!(!sessions.iter().any(|s| s.harness == Harness::Gemini));
    }
}
