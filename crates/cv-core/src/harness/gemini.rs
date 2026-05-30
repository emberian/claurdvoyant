//! Gemini / Antigravity adapter — best-effort, multi-format.
//!
//! `~/.gemini/` holds several distinct on-disk shapes; we read every readable one:
//!
//! 1. **Chat recordings** (richest) — `~/.gemini/tmp/<projectHash>/chats/session-<ts>-<id>.json`
//!    (legacy, a single whole-file `ConversationRecord`) or `.jsonl` (modern, append-only: a metadata
//!    first line, then per-message records, `{"$set":…}` metadata patches, and `{"$rewindTo":id}`
//!    truncation markers). These carry real assistant turns, thinking, tool calls *and* tool results
//!    plus per-message token usage and model. Subagent recordings live under
//!    `chats/<parentId>/<id>.jsonl`. (See gemini-cli `core/src/services/chatRecordingService.ts`.)
//! 2. **gemini-cli checkpoints** — `~/.gemini/tmp/<hash>/checkpoint-<tag>.json`, either
//!    `{ history: Content[], authType? }` or a bare legacy `Content[]` array, where each `Content` is
//!    `{ role: "user"|"model", parts: Part[] }`. (See gemini-cli `core/src/core/logger.ts`.) These are
//!    transient (deleted on resume) so they may not be present, but we parse them when they are.
//! 3. **Readable fallback** — `~/.gemini/tmp/<hash>/logs.json`, an array of
//!    `{sessionId, messageId, type, message, timestamp}` (user prompts reliably; weak on assistant).
//!    One `logs.json` may interleave several `sessionId`s; each becomes its own IR session.
//!
//! The closed Antigravity IDE's `~/.gemini/antigravity/conversations/*.pb` are opaque (compressed /
//! no on-disk schema, no readable strings) and are *not* parsed.
//!
//! `parse_all_str` (used by the wasm ingest path) handles all three readable JSON/JSONL shapes purely,
//! with no filesystem access.

use super::Adapter;
use crate::ir::*;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct Gemini {
    root: Option<PathBuf>,
}

impl Gemini {
    pub fn new() -> Self {
        let root = dirs::home_dir()
            .map(|h| h.join(".gemini").join("tmp"))
            .filter(|p| p.exists());
        Gemini { root }
    }
}

impl Default for Gemini {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for Gemini {
    fn harness(&self) -> Harness {
        Harness::Gemini
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
            let name = entry.file_name().to_string_lossy();
            let path = entry.path();

            if name == "logs.json" {
                if let Ok(text) = fs::read_to_string(path) {
                    out.extend(refs_from_logs(&text, path));
                }
            } else if is_chat_recording(path) {
                if let Ok(text) = fs::read_to_string(path) {
                    if let Some(s) = parse_chat_recording(&text, Some(path.to_path_buf())) {
                        out.push(session_ref(&s, path));
                    }
                }
            } else if is_checkpoint(&name) {
                if let Ok(text) = fs::read_to_string(path) {
                    if let Some(s) = parse_checkpoint(&text, &name, Some(path.to_path_buf())) {
                        out.push(session_ref(&s, path));
                    }
                }
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        let text = fs::read_to_string(&r.path)
            .with_context(|| format!("reading {}", r.path.display()))?;
        let name = r
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        // Chat recording (legacy .json / modern .jsonl): one session per file, id is authoritative.
        if is_chat_recording(&r.path) {
            if let Some(s) = parse_chat_recording(&text, Some(r.path.clone())) {
                return Ok(s);
            }
        }
        // Checkpoint: one session per file.
        if is_checkpoint(&name) {
            if let Some(s) = parse_checkpoint(&text, &name, Some(r.path.clone())) {
                return Ok(s);
            }
        }
        // logs.json: many sessions per file — pick out the one matching r.id.
        let sessions = parse_logs_str(&text, Some(r.path.clone()));
        if let Some(s) = sessions.into_iter().find(|s| s.id == r.id) {
            return Ok(s);
        }

        anyhow::bail!("could not parse Gemini session {} from {}", r.id, r.path.display())
    }

    fn can_emit(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Format detection
// ---------------------------------------------------------------------------

/// A chat-recording file: `chats/session-*.json` or `chats/session-*.jsonl`, or a subagent recording
/// `chats/<parentId>/<id>.jsonl`. We key off the `chats/` directory in the path.
fn is_chat_recording(path: &Path) -> bool {
    let in_chats = path
        .components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("chats"));
    if !in_chats {
        return false;
    }
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("json") | Some("jsonl")
    )
}

fn is_checkpoint(name: &str) -> bool {
    name.starts_with("checkpoint-") && name.ends_with(".json")
}

// ---------------------------------------------------------------------------
// Pure entry point (WASM-safe): handles all three readable JSON/JSONL shapes.
// ---------------------------------------------------------------------------

/// Parse any readable Gemini session text into IR [`Session`]s, with no filesystem access.
///
/// Sniffs the shape:
/// - a chat-recording `ConversationRecord` (`.json` object with `messages[]`, or `.jsonl` with a
///   `sessionId`+`projectHash` metadata line),
/// - a gemini-cli checkpoint (`{history:[…]}` or a bare `Content[]` array),
/// - or the `logs.json` array of `{sessionId,messageId,message,…}` entries (possibly several sessions).
pub fn parse_all_str(text: &str, source_path: Option<PathBuf>) -> Vec<Session> {
    // Chat recording? (object with sessionId+messages, or jsonl with a metadata line)
    if let Some(s) = parse_chat_recording(text, source_path.clone()) {
        return vec![s];
    }

    let trimmed = text.trim_start();
    // Checkpoint object `{history:[…]}`.
    if trimmed.starts_with('{') {
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) {
            if map.contains_key("history") {
                if let Some(s) = parse_checkpoint(text, "", source_path.clone()) {
                    return vec![s];
                }
            }
        }
    }
    // Array: either a logs.json (entries have messageId/message) or a bare checkpoint Content[].
    if trimmed.starts_with('[') {
        if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(text) {
            let looks_logs = items.iter().take(8).any(|it| {
                it.get("messageId").is_some() && it.get("message").is_some()
            });
            if looks_logs {
                return parse_logs_str(text, source_path);
            }
            let looks_checkpoint = items
                .iter()
                .take(8)
                .any(|it| it.get("role").is_some() && it.get("parts").is_some());
            if looks_checkpoint {
                if let Some(s) = parse_checkpoint(text, "", source_path) {
                    return vec![s];
                }
            }
        }
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// logs.json
// ---------------------------------------------------------------------------

/// Parse a Gemini `logs.json` from its text contents into one [`Session`] per distinct `sessionId`.
fn parse_logs_str(text: &str, source_path: Option<PathBuf>) -> Vec<Session> {
    let entries: Vec<Value> = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    let mut by_session: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for e in &entries {
        let sid = e
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        by_session.entry(sid).or_default().push(e);
    }

    let mut out = Vec::new();
    for (sid, mut items) in by_session {
        if sid.is_empty() {
            continue;
        }
        items.sort_by_key(|e| e.get("messageId").and_then(Value::as_i64).unwrap_or(0));

        let title = items
            .iter()
            .find(|e| e.get("type").and_then(Value::as_str) == Some("user"))
            .and_then(|e| e.get("message").and_then(Value::as_str))
            .map(|t| crate::ir::truncate(t, 80));
        let times: Vec<DateTime<Utc>> = items
            .iter()
            .filter_map(|e| e.get("timestamp").and_then(Value::as_str).and_then(parse_ts))
            .collect();

        let mut s = Session {
            id: sid,
            harness: Harness::Gemini,
            cwd: None,
            title,
            created_at: times.iter().min().copied(),
            updated_at: times.iter().max().copied(),
            model: None,
            git: None,
            messages: Vec::new(),
            source_path: source_path.clone(),
            extra: serde_json::Map::new(),
        };
        for e in items {
            let role = match e.get("type").and_then(Value::as_str) {
                Some("assistant") | Some("model") | Some("gemini") => Role::Assistant,
                Some("system") => Role::System,
                _ => Role::User,
            };
            let text = e
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if text.is_empty() {
                continue;
            }
            let mut m = Message::new(role);
            m.timestamp = e.get("timestamp").and_then(Value::as_str).and_then(parse_ts);
            m.content.push(Block::Text { text });
            s.messages.push(m);
        }
        out.push(s);
    }
    out
}

// ---------------------------------------------------------------------------
// Chat recordings (ConversationRecord) — legacy whole-file .json or modern .jsonl
// ---------------------------------------------------------------------------

/// Parse a gemini-cli chat recording. Handles both the legacy whole-file JSON object
/// (`{sessionId, messages:[…]}`) and the modern append-only JSONL log (metadata line + message
/// records + `$set` / `$rewindTo` control records). Returns `None` if the text is not a recording.
fn parse_chat_recording(text: &str, source_path: Option<PathBuf>) -> Option<Session> {
    let trimmed = text.trim_start();

    // Legacy: a single JSON object spanning the whole file.
    if trimmed.starts_with('{') {
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) {
            if map.contains_key("sessionId") && map.contains_key("messages") {
                return Some(record_to_session(&map, source_path));
            }
            // A modern recording happens to also start with `{` on its metadata first line,
            // but a *whole-file* parse of multi-line JSONL fails, so we fall through to JSONL below.
        }
    }

    // Modern JSONL: replay metadata + message records + control records.
    let mut metadata = serde_json::Map::new();
    // Preserve insertion order of message ids while allowing replacement / truncation.
    let mut order: Vec<String> = Vec::new();
    let mut msgs: BTreeMap<String, Value> = BTreeMap::new();
    let mut saw_meta = false;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            continue; // tolerate junk lines
        };
        let Some(obj) = rec.as_object() else { continue };

        if let Some(rewind) = obj.get("$rewindTo").and_then(Value::as_str) {
            // Drop the rewind target and everything after it.
            if let Some(pos) = order.iter().position(|id| id == rewind) {
                for id in order.drain(pos..) {
                    msgs.remove(&id);
                }
            } else {
                order.clear();
                msgs.clear();
            }
            continue;
        }
        if let Some(set) = obj.get("$set").and_then(Value::as_object) {
            for (k, v) in set {
                if k == "messages" {
                    // Checkpoint: rebuild the whole message list.
                    order.clear();
                    msgs.clear();
                    if let Some(arr) = v.as_array() {
                        for m in arr {
                            if let Some(id) = m.get("id").and_then(Value::as_str) {
                                if !msgs.contains_key(id) {
                                    order.push(id.to_string());
                                }
                                msgs.insert(id.to_string(), m.clone());
                            }
                        }
                    }
                } else {
                    metadata.insert(k.clone(), v.clone());
                }
            }
            continue;
        }
        if let Some(id) = obj.get("id").and_then(Value::as_str) {
            // A message record (must have an id and a type/content).
            if obj.contains_key("type") || obj.contains_key("content") {
                if !msgs.contains_key(id) {
                    order.push(id.to_string());
                }
                msgs.insert(id.to_string(), rec.clone());
                continue;
            }
            // else: junk record with an id but no message shape — ignore.
            continue;
        }
        // Metadata line(s): a real recording's metadata carries BOTH sessionId and projectHash
        // (gemini-cli's `isPartialMetadataRecord`). This guards against mistaking a `logs.json`
        // entry (sessionId + messageId + message, no projectHash) for a recording.
        if obj.contains_key("sessionId") && obj.contains_key("projectHash") {
            saw_meta = true;
            for (k, v) in obj {
                metadata.insert(k.clone(), v.clone());
            }
        }
    }

    if !saw_meta && msgs.is_empty() {
        return None;
    }

    // Reassemble a ConversationRecord-shaped map.
    let messages: Vec<Value> = order.iter().filter_map(|id| msgs.get(id).cloned()).collect();
    metadata.insert("messages".into(), Value::Array(messages));
    Some(record_to_session(&metadata, source_path))
}

/// Map a `ConversationRecord` (as a JSON object) onto an IR [`Session`].
fn record_to_session(
    rec: &serde_json::Map<String, Value>,
    source_path: Option<PathBuf>,
) -> Session {
    let id = rec
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let created_at = rec
        .get("startTime")
        .and_then(Value::as_str)
        .and_then(parse_ts);
    let updated_at = rec
        .get("lastUpdated")
        .and_then(Value::as_str)
        .and_then(parse_ts);
    let summary = rec
        .get("summary")
        .and_then(Value::as_str)
        .map(str::to_string);

    // cwd: gemini stores only an opaque projectHash, but `directories[]` (added via /dir) may carry
    // real absolute paths; use the first as a best-effort cwd.
    let cwd = rec
        .get("directories")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(PathBuf::from);

    let empty = vec![];
    let raw_msgs = rec.get("messages").and_then(Value::as_array).unwrap_or(&empty);

    let mut messages = Vec::new();
    let mut session_model: Option<String> = None;
    let mut title: Option<String> = None;

    for rm in raw_msgs {
        let Some(obj) = rm.as_object() else { continue };
        let mty = obj.get("type").and_then(Value::as_str).unwrap_or("user");
        let ts = obj.get("timestamp").and_then(Value::as_str).and_then(parse_ts);
        let id = obj.get("id").and_then(Value::as_str).map(str::to_string);

        match mty {
            "gemini" | "model" | "assistant" => {
                let mut m = Message::new(Role::Assistant);
                m.id = id;
                m.timestamp = ts;
                if let Some(model) = obj.get("model").and_then(Value::as_str) {
                    m.model = Some(model.to_string());
                    if session_model.is_none() {
                        session_model = Some(model.to_string());
                    }
                }
                m.usage = obj.get("tokens").and_then(parse_tokens);

                // Thoughts → Thinking blocks (subject + description).
                if let Some(thoughts) = obj.get("thoughts").and_then(Value::as_array) {
                    for t in thoughts {
                        let subj = t.get("subject").and_then(Value::as_str).unwrap_or("");
                        let desc = t.get("description").and_then(Value::as_str).unwrap_or("");
                        let text = if subj.is_empty() {
                            desc.to_string()
                        } else if desc.is_empty() {
                            subj.to_string()
                        } else {
                            format!("**{subj}** {desc}")
                        };
                        if !text.is_empty() {
                            m.content.push(Block::Thinking {
                                text,
                                signature: None,
                                encrypted: None,
                                redacted: false,
                            });
                        }
                    }
                }

                // Assistant content: string, or array of Parts (text / inlineData / functionCall).
                push_content_blocks(&mut m.content, obj.get("content"));

                // Tool calls (gemini-cli ToolCallRecord) → ToolUse + paired ToolResult.
                let mut tool_results: Vec<Block> = Vec::new();
                if let Some(calls) = obj.get("toolCalls").and_then(Value::as_array) {
                    for c in calls {
                        let name = c.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let call_id = c
                            .get("id")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("{name}-call"));
                        let input = c.get("args").cloned().unwrap_or(Value::Null);
                        m.content.push(Block::ToolUse {
                            id: call_id.clone(),
                            name: name.to_string(),
                            input,
                        });
                        let status = c.get("status").and_then(Value::as_str);
                        let is_error = status
                            .map(|s| s.eq_ignore_ascii_case("error"))
                            .unwrap_or(false);
                        let content = tool_result_text(c.get("result"))
                            .or_else(|| {
                                c.get("resultDisplay")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                            })
                            .unwrap_or_default();
                        if !content.is_empty() {
                            tool_results.push(Block::ToolResult {
                                tool_use_id: call_id,
                                content,
                                is_error,
                                tool_name: Some(name.to_string()),
                                status: status.map(str::to_string),
                                details: None,
                            });
                        }
                    }
                }

                if !m.content.is_empty() {
                    messages.push(m);
                }
                // Emit tool results as a separate Tool-role turn (mirrors how Gemini feeds
                // functionResponses back as a user turn, but kept distinct in the IR).
                if !tool_results.is_empty() {
                    let mut tm = Message::new(Role::Tool);
                    tm.timestamp = ts;
                    tm.content = tool_results;
                    messages.push(tm);
                }
            }
            "info" | "error" | "warning" => {
                // Synthetic/system notices. Keep as System text so they're searchable but tagged.
                let mut m = Message::new(Role::System);
                m.id = id;
                m.timestamp = ts;
                m.extra.insert("gemini_kind".into(), Value::String(mty.to_string()));
                push_content_blocks(&mut m.content, obj.get("content"));
                if !m.content.is_empty() {
                    messages.push(m);
                }
            }
            _ => {
                // user (and anything else) → user turn.
                let mut m = Message::new(Role::User);
                m.id = id;
                m.timestamp = ts;
                push_content_blocks(&mut m.content, obj.get("content"));
                if title.is_none() {
                    title = m.text().map(|t| crate::ir::truncate(&t, 80));
                }
                if !m.content.is_empty() {
                    messages.push(m);
                }
            }
        }
    }

    Session {
        id,
        harness: Harness::Gemini,
        cwd,
        title: summary.or(title),
        created_at,
        updated_at,
        model: session_model,
        git: None,
        messages,
        source_path,
        extra: serde_json::Map::new(),
    }
}

/// Append IR blocks for a Gemini `content` value, which is a `PartListUnion`: a bare string, a single
/// Part object, or an array of Parts. Each Part may carry `text`, `inlineData`, `functionCall`, or
/// `functionResponse`.
fn push_content_blocks(out: &mut Vec<Block>, content: Option<&Value>) {
    let Some(content) = content else { return };
    match content {
        Value::String(s) => {
            if !s.is_empty() {
                out.push(Block::Text { text: s.clone() });
            }
        }
        Value::Array(parts) => {
            for p in parts {
                push_part(out, p);
            }
        }
        Value::Object(_) => push_part(out, content),
        _ => {}
    }
}

fn push_part(out: &mut Vec<Block>, part: &Value) {
    if let Some(s) = part.as_str() {
        if !s.is_empty() {
            out.push(Block::Text { text: s.to_string() });
        }
        return;
    }
    let Some(obj) = part.as_object() else { return };

    if let Some(fc) = obj.get("functionCall").and_then(Value::as_object) {
        let name = fc.get("name").and_then(Value::as_str).unwrap_or("tool");
        let id = fc
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{name}-call"));
        out.push(Block::ToolUse {
            id,
            name: name.to_string(),
            input: fc.get("args").cloned().unwrap_or(Value::Null),
        });
        return;
    }
    if let Some(fr) = obj.get("functionResponse").and_then(Value::as_object) {
        let name = fr.get("name").and_then(Value::as_str).unwrap_or("tool");
        let id = fr
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{name}-call"));
        let content = fr
            .get("response")
            .map(value_to_text)
            .unwrap_or_default();
        out.push(Block::ToolResult {
            tool_use_id: id,
            content,
            is_error: false,
            tool_name: Some(name.to_string()),
            status: None,
            details: None,
        });
        return;
    }
    if let Some(inline) = obj.get("inlineData").and_then(Value::as_object) {
        out.push(Block::Image {
            media_type: inline.get("mimeType").and_then(Value::as_str).map(str::to_string),
            data_ref: None, // we don't inline base64 bytes into the IR
        });
        return;
    }
    if let Some(fd) = obj.get("fileData").and_then(Value::as_object) {
        out.push(Block::File {
            mime: fd.get("mimeType").and_then(Value::as_str).map(str::to_string),
            path: None,
            source: fd.get("fileUri").and_then(Value::as_str).map(str::to_string),
        });
        return;
    }
    if let Some(text) = obj.get("text").and_then(Value::as_str) {
        let is_thought = obj.get("thought").and_then(Value::as_bool).unwrap_or(false);
        if text.is_empty() {
            return;
        }
        if is_thought {
            out.push(Block::Thinking {
                text: text.to_string(),
                signature: None,
                encrypted: None,
                redacted: false,
            });
        } else {
            out.push(Block::Text { text: text.to_string() });
        }
    }
}

/// Extract human-readable text from a `toolCall.result` (a `PartListUnion`, typically a list of
/// `{functionResponse:{response:{output}}}` parts).
fn tool_result_text(result: Option<&Value>) -> Option<String> {
    let result = result?;
    match result {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let mut out = String::new();
            for p in parts {
                let chunk = if let Some(fr) = p.get("functionResponse") {
                    fr.get("response").map(value_to_text).unwrap_or_default()
                } else if let Some(t) = p.get("text").and_then(Value::as_str) {
                    t.to_string()
                } else {
                    value_to_text(p)
                };
                if !chunk.is_empty() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&chunk);
                }
            }
            (!out.is_empty()).then_some(out)
        }
        Value::Object(_) => {
            if let Some(fr) = result.get("functionResponse") {
                Some(fr.get("response").map(value_to_text).unwrap_or_default())
            } else {
                Some(value_to_text(result))
            }
        }
        _ => None,
    }
}

/// Turn an arbitrary JSON value into display text, unwrapping the common `{output: "…"}` wrapper.
fn value_to_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(map) => {
            if let Some(Value::String(o)) = map.get("output") {
                o.clone()
            } else if let Some(Value::String(e)) = map.get("error") {
                e.clone()
            } else {
                v.to_string()
            }
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn parse_tokens(v: &Value) -> Option<Usage> {
    let obj = v.as_object()?;
    let get = |k: &str| obj.get(k).and_then(Value::as_u64);
    Some(Usage {
        input_tokens: get("input"),
        output_tokens: get("output"),
        cache_read_tokens: get("cached"),
        cache_creation_tokens: None,
    })
}

// ---------------------------------------------------------------------------
// gemini-cli checkpoints — `{history: Content[]}` or bare `Content[]`
// ---------------------------------------------------------------------------

/// Parse a gemini-cli checkpoint into an IR [`Session`]. The id is derived from the `checkpoint-<tag>`
/// filename when available (the `tag` is percent-encoded by gemini-cli; we decode it best-effort).
fn parse_checkpoint(text: &str, file_name: &str, source_path: Option<PathBuf>) -> Option<Session> {
    let v: Value = serde_json::from_str(text).ok()?;
    let history = match &v {
        Value::Array(a) => a.clone(),
        Value::Object(map) => map.get("history").and_then(Value::as_array).cloned()?,
        _ => return None,
    };

    let id = checkpoint_id(file_name);
    let mut messages = Vec::new();
    let mut title: Option<String> = None;

    for content in &history {
        let Some(obj) = content.as_object() else { continue };
        let role = match obj.get("role").and_then(Value::as_str) {
            Some("model") | Some("assistant") => Role::Assistant,
            Some("system") => Role::System,
            _ => Role::User, // gemini uses "user" for both prompts and tool responses
        };
        let mut m = Message::new(role);
        push_content_blocks(&mut m.content, obj.get("parts"));
        // If this "user" turn is purely tool results, retag it as a Tool turn.
        if role == Role::User
            && !m.content.is_empty()
            && m.content.iter().all(|b| matches!(b, Block::ToolResult { .. }))
        {
            m.role = Role::Tool;
        }
        if m.role == Role::User && title.is_none() {
            title = m.text().map(|t| crate::ir::truncate(&t, 80));
        }
        if !m.content.is_empty() {
            messages.push(m);
        }
    }

    if messages.is_empty() {
        return None;
    }

    Some(Session {
        id,
        harness: Harness::Gemini,
        cwd: None,
        title,
        created_at: None,
        updated_at: None,
        model: None,
        git: None,
        messages,
        source_path,
        extra: serde_json::Map::new(),
    })
}

fn checkpoint_id(file_name: &str) -> String {
    let tag = file_name
        .strip_prefix("checkpoint-")
        .and_then(|s| s.strip_suffix(".json"))
        .unwrap_or("");
    if tag.is_empty() {
        return "checkpoint".to_string();
    }
    let decoded = percent_encoding::percent_decode_str(tag)
        .decode_utf8()
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| tag.to_string());
    format!("checkpoint-{decoded}")
}

// ---------------------------------------------------------------------------
// SessionRef helpers
// ---------------------------------------------------------------------------

fn refs_from_logs(text: &str, path: &Path) -> Vec<SessionRef> {
    parse_logs_str(text, Some(path.to_path_buf()))
        .iter()
        .map(|s| session_ref(s, path))
        .collect()
}

fn session_ref(s: &Session, path: &Path) -> SessionRef {
    SessionRef {
        id: s.id.clone(),
        harness: Harness::Gemini,
        path: path.to_path_buf(),
        cwd: s.cwd.clone(),
        title: s.title.clone(),
        created_at: s.created_at,
        updated_at: s.updated_at,
        message_count: s.messages.len(),
    }
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/gemini/{}",
            env!("CARGO_MANIFEST_DIR"),
            name
        );
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"))
    }

    #[test]
    fn parses_logs_json_grouped_by_session() {
        let text = fixture("logs.json");
        let mut sessions = parse_all_str(&text, None);
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "sess-a");
        assert_eq!(sessions[0].messages.len(), 2);
        assert_eq!(sessions[1].id, "sess-b");
        assert_eq!(sessions[1].harness, Harness::Gemini);
    }

    #[test]
    fn parses_legacy_chat_recording() {
        let text = fixture("session_legacy.json");
        let sessions = parse_all_str(&text, None);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.id, "9aeb2942-7c46-47b7-aded-13772d4d4e63");
        assert_eq!(s.model.as_deref(), Some("gemini-3-flash-preview"));
        assert_eq!(s.title.as_deref(), Some("Investigated whether brorb is a useful meditation aid."));
        assert!(s.created_at.is_some() && s.updated_at.is_some());

        // user, gemini(with thinking+text+tooluse), tool(result), gemini(text), info(system)
        let roles: Vec<Role> = s.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant, Role::System]
        );

        let asst = &s.messages[1];
        assert!(asst.content.iter().any(|b| matches!(b, Block::Thinking { .. })));
        assert!(asst.content.iter().any(|b| matches!(b, Block::Text { .. })));
        assert!(asst
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolUse { name, .. } if name == "read_file")));
        assert!(asst.usage.is_some());

        // tool result carried in the following Tool turn
        let tool = &s.messages[2];
        let got = tool.content.iter().any(|b| matches!(b, Block::ToolResult { content, .. } if content.contains("meditation timer")));
        assert!(got, "tool result text should be extracted");

        // info message kept as System with a kind marker
        let info = &s.messages[4];
        assert_eq!(info.role, Role::System);
        assert_eq!(info.extra.get("gemini_kind").and_then(Value::as_str), Some("info"));
    }

    #[test]
    fn parses_modern_jsonl_with_rewind_and_set() {
        let text = fixture("session_modern.jsonl");
        let sessions = parse_all_str(&text, None);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.id, "11112222-3333-4444-5555-666677778888");
        // $set provided a summary used as title
        assert_eq!(s.title.as_deref(), Some("Listed the project files."));

        // The $rewindTo a3 dropped the first a3, then a3 was re-appended with new text.
        let a3s: Vec<&Message> = s.messages.iter().filter(|m| m.id.as_deref() == Some("a3")).collect();
        assert_eq!(a3s.len(), 1, "rewind should have de-duplicated a3");
        assert!(a3s[0]
            .content
            .iter()
            .any(|b| matches!(b, Block::Text { text } if text.contains("exactly two files"))));

        // tool call + result present
        assert!(s.messages.iter().any(|m| m
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolUse { name, .. } if name == "list_directory"))));
        assert!(s.messages.iter().any(|m| m.role == Role::Tool
            && m.content.iter().any(|b| matches!(b, Block::ToolResult { content, .. } if content.contains("main.py")))));
    }

    #[test]
    fn parses_checkpoint_history() {
        let text = fixture("checkpoint.json");
        let s = parse_checkpoint(&text, "checkpoint-my%20tag.json", None).expect("checkpoint");
        assert_eq!(s.id, "checkpoint-my tag");
        // user, model(text+thinking+tooluse), tool(result), model(text)
        let roles: Vec<Role> = s.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant]
        );
        let model_turn = &s.messages[1];
        assert!(model_turn.content.iter().any(|b| matches!(b, Block::Thinking { .. })));
        assert!(model_turn
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolUse { name, .. } if name == "calculator")));
        // the tool turn carries the functionResponse output
        assert!(s.messages[2]
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolResult { content, .. } if content == "4")));
    }

    #[test]
    fn parses_legacy_array_checkpoint_via_parse_all() {
        let text = fixture("checkpoint_legacy_array.json");
        let sessions = parse_all_str(&text, None);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[1].role, Role::Assistant);
        assert_eq!(s.title.as_deref(), Some("hi"));
    }

    #[test]
    fn tolerates_garbage() {
        assert!(parse_all_str("not json at all", None).is_empty());
        assert!(parse_all_str("{}", None).is_empty());
        assert!(parse_all_str("[]", None).is_empty());
    }
}
