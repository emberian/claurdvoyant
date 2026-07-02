//! OpenCode adapter — `~/.local/share/opencode/storage/`.
//!
//! Layout: `session/**/ses_*.json` (metadata, `directory` = cwd), `message/<sid>/<msgid>.json`
//! (per-message metadata), `part/<msgid>/<partid>.json` (the actual content parts — parts are
//! keyed by *messageID*, not sessionID).
//!
//! ## Two storage generations
//!
//! OpenCode rewrote its on-disk model. We sniff by content (presence of separate part files /
//! an inline `summary` object) rather than any version field:
//!
//! * **Inline-summary generation (older):** a message carried an AI-generated `summary`
//!   *object* `{title, body, diffs[]}` inline; there were no separate part files. We render
//!   `summary.body` (falling back to `title`) as the message text and stash `diffs` in `extra`.
//! * **Parts generation (newer):** message content lives in `part/<msgid>/<partid>.json`
//!   files, one per part. The message record holds only metadata. The richest variant.
//!   Confusingly, a *boolean* `summary: true` here is a compaction marker on an assistant
//!   message (mode/agent == "compaction"), NOT inline content — we keep it in `extra`.
//!
//! The on-disk part taxonomy (source of truth: opencode `session/message-v2.ts`) is:
//! `text` (may be `synthetic`/`ignored`), `reasoning` (carries `metadata.anthropic.signature`),
//! `tool` (with a `state` union pending|running|completed|error, each holding
//! input/output/title/metadata/error/time), `file` (file/dir/image attachment with `mime`,
//! `url`, `source`), `agent` (an `@agent` mention / sub-agent reference), `subtask` (a spawned
//! sub-session with its own prompt/agent/model), `patch`, `snapshot`, `step-start`,
//! `step-finish` (per-step cost/tokens), `retry`, `compaction`.

use super::Adapter;
use crate::ir::*;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::PathBuf;
use walkdir::WalkDir;

pub struct OpenCode {
    root: Option<PathBuf>,
}

impl OpenCode {
    pub fn new() -> Self {
        let root = dirs::home_dir()
            .map(|h| h.join(".local/share/opencode/storage"))
            .filter(|p| p.exists());
        OpenCode { root }
    }

    /// An adapter rooted at an explicit storage dir (`…/opencode/storage`) instead of `$HOME` —
    /// lets emit-verification and tests re-parse a just-written session without mutating the
    /// process environment (env writes race other threads' `getenv`).
    pub fn with_root(root: PathBuf) -> Self {
        OpenCode { root: Some(root) }
    }

    fn message_dir(&self, sid: &str) -> Option<PathBuf> {
        self.root.as_ref().map(|r| r.join("message").join(sid))
    }
    fn part_dir(&self, mid: &str) -> Option<PathBuf> {
        // Parts are keyed by messageID only: storage/part/<messageID>/<partid>.json
        self.root.as_ref().map(|r| r.join("part").join(mid))
    }
}

impl Default for OpenCode {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for OpenCode {
    fn harness(&self) -> Harness {
        Harness::OpenCode
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.root.clone()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(root) = &self.root else {
            return Ok(vec![]);
        };
        let session_dir = root.join("session");
        let mut out = Vec::new();
        for entry in WalkDir::new(&session_dir).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.starts_with("ses_") || !name.ends_with(".json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(path) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let id = v
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(name.trim_end_matches(".json"))
                .to_string();
            let message_count = self.message_dir(&id).map(|d| count_turns(&d)).unwrap_or(0);
            out.push(SessionRef {
                id,
                harness: Harness::OpenCode,
                path: path.to_path_buf(),
                cwd: v.get("directory").and_then(Value::as_str).map(PathBuf::from),
                title: v
                    .get("title")
                    .and_then(Value::as_str)
                    .map(|t| crate::ir::truncate(t, 80)),
                created_at: v.pointer("/time/created").and_then(super::ts_from_value),
                updated_at: v.pointer("/time/updated").and_then(super::ts_from_value),
                message_count,
            });
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        crate::stream::collect(self, r)
    }

    fn stream(&self, r: &SessionRef, _opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        // OpenCode stores a session as a *directory* of per-message JSON files (plus per-message
        // part files). The old `parse` slurped every message body into a `Vec` to sort it — for a
        // large session that resident-spikes the whole transcript. Here we instead:
        //   1. read the small session meta file,
        //   2. cheaply order the message files (read each only to pull its `time/created` key, then
        //      drop the body — peak is O(one message file), not the whole session),
        //   3. re-read each file in order, build its IR message(s), emit to `sink`, and drop it
        //      before touching the next.
        // `load_parts` already loads one message's parts at a time, so peak stays O(one message).
        let text = fs::read_to_string(&r.path).with_context(|| format!("reading {}", r.path.display()))?;
        let meta: Value = serde_json::from_str(&text).unwrap_or(Value::Null);

        let mut s = Session {
            id: r.id.clone(),
            harness: Harness::OpenCode,
            cwd: meta.get("directory").and_then(Value::as_str).map(PathBuf::from),
            title: meta.get("title").and_then(Value::as_str).map(str::to_string),
            created_at: meta.pointer("/time/created").and_then(super::ts_from_value),
            updated_at: meta.pointer("/time/updated").and_then(super::ts_from_value),
            model: None,
            git: None,
            messages: Vec::new(),
            source_path: Some(r.path.clone()),
            extra: serde_json::Map::new(),
        };

        // Order message files by `time/created` (same ordering as before) while holding only the
        // sort key + path per file — never all message bodies at once.
        let mut order: Vec<(i64, PathBuf)> = Vec::new();
        if let Some(mdir) = self.message_dir(&r.id) {
            if let Ok(rd) = fs::read_dir(&mdir) {
                for e in rd.filter_map(|e| e.ok()) {
                    let path = e.path();
                    if let Ok(t) = fs::read_to_string(&path) {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            let when = v.pointer("/time/created").and_then(Value::as_i64).unwrap_or(0);
                            order.push((when, path));
                        }
                    }
                }
            }
        }
        order.sort_by_key(|(t, _)| *t);

        sink.meta(&s);

        // Re-read one message file at a time, emit, then drop before the next.
        for (_, path) in &order {
            let Ok(t) = fs::read_to_string(path) else { continue };
            let Ok(mv) = serde_json::from_str::<Value>(&t) else {
                continue;
            };
            let msg_id = mv.get("id").and_then(Value::as_str).unwrap_or("");
            let parts = self.load_parts(msg_id);
            for built in build_messages(&mv, &parts) {
                if s.model.is_none() {
                    s.model = built.model.clone();
                }
                if sink.message(built) == Flow::Stop {
                    return Ok(s);
                }
            }
        }

        Ok(s)
    }

}

/// Count a session's conversational turns: message records whose `role` is `user`/`assistant`.
/// The [`SessionRef::message_count`] contract is user+assistant turns only — counting directory
/// entries (the old behavior) also picked up system/summary records and any stray files, so the
/// number disagreed with every other adapter. Message files are small metadata records (the heavy
/// content lives in part files, which this never touches), so the peek stays cheap.
fn count_turns(dir: &std::path::Path) -> usize {
    let Ok(rd) = fs::read_dir(dir) else { return 0 };
    rd.filter_map(|e| e.ok())
        .filter(|e| {
            let Ok(text) = fs::read_to_string(e.path()) else {
                return false;
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                return false;
            };
            matches!(v.get("role").and_then(Value::as_str), Some("user") | Some("assistant"))
        })
        .count()
}

impl OpenCode {
    fn load_parts(&self, mid: &str) -> Vec<Value> {
        let Some(pdir) = self.part_dir(mid) else {
            return vec![];
        };
        let mut parts: Vec<(String, Value)> = Vec::new();
        if let Ok(rd) = fs::read_dir(&pdir) {
            for e in rd.filter_map(|e| e.ok()) {
                let fname = e.file_name().to_string_lossy().to_string();
                if let Ok(t) = fs::read_to_string(e.path()) {
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        parts.push((fname, v));
                    }
                }
            }
        }
        parts.sort_by(|a, b| a.0.cmp(&b.0));
        parts.into_iter().map(|(_, v)| v).collect()
    }
}

// ── pure mapping (filesystem-free, so it's unit-testable for both generations) ──────────────

/// Map one OpenCode message record + its parts into IR messages. Tool results are split into a
/// trailing `Role::Tool` message (mirroring the Claude adapter) so conversions re-encode them
/// correctly. Returns 0–2 messages.
fn build_messages(mv: &Value, parts: &[Value]) -> Vec<Message> {
    let role = match mv.get("role").and_then(Value::as_str) {
        Some("assistant") => Role::Assistant,
        Some("system") => Role::System,
        _ => Role::User,
    };

    let msg_id = mv.get("id").and_then(Value::as_str).unwrap_or("");
    let mut m = Message::new(role);
    m.id = (!msg_id.is_empty()).then(|| msg_id.to_string());
    m.parent_id = mv.get("parentID").and_then(Value::as_str).map(str::to_string);
    m.timestamp = mv.pointer("/time/created").and_then(super::ts_from_value);
    // Newer records carry `modelID`; user records nest it under `model.modelID`.
    m.model = mv
        .get("modelID")
        .and_then(Value::as_str)
        .or_else(|| mv.pointer("/model/modelID").and_then(Value::as_str))
        .map(str::to_string);
    m.usage = parse_tokens(mv.get("tokens"));
    collect_message_extra(mv, &mut m.extra);

    let mut tool_results: Vec<Block> = Vec::new();
    for part in parts {
        match part.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    if !t.is_empty() {
                        m.content.push(Block::Text {
                            text: t.to_string().into(),
                        });
                    }
                }
            }
            "reasoning" => {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    if !t.is_empty() {
                        // Providers stash an opaque signature under
                        // `metadata.<provider>.signature` (e.g. anthropic). Preserve it so a
                        // thinking block can round-trip.
                        let signature = part
                            .pointer("/metadata/anthropic/signature")
                            .and_then(Value::as_str)
                            .or_else(|| find_signature(part.get("metadata")))
                            .map(str::to_string);
                        m.content.push(Block::Thinking {
                            text: t.to_string().into(),
                            signature,
                            encrypted: None,
                            redacted: false,
                        });
                    }
                }
            }
            "tool" => {
                let (use_block, result) = tool_blocks(part);
                m.content.push(use_block);
                if let Some(r) = result {
                    tool_results.push(r);
                }
            }
            "file" => {
                m.content.push(file_block(part));
            }
            "agent" => {
                // An `@agent` mention / sub-agent reference embedded in a user prompt. No IR
                // block fits; surface its name as text and keep the structured form in extra.
                if let Some(name) = part.get("name").and_then(Value::as_str) {
                    m.content.push(Block::Text {
                        text: format!("@{name}").into(),
                    });
                }
                push_extra_array(&mut m.extra, "agent_refs", part.clone());
            }
            "subtask" => {
                // A spawned sub-session (Task tool style). Render its prompt + which agent ran
                // it; keep the full record (agent/model/command/description) in extra.
                let agent = part.get("agent").and_then(Value::as_str).unwrap_or("agent");
                let prompt = part.get("prompt").and_then(Value::as_str).unwrap_or("");
                m.content.push(Block::Text {
                    text: format!("[subtask → {agent}] {prompt}").into(),
                });
                push_extra_array(&mut m.extra, "subtasks", part.clone());
            }
            "patch" => push_extra_array(&mut m.extra, "patches", part.clone()),
            "snapshot" => push_extra_array(&mut m.extra, "snapshots", part.clone()),
            "step-start" | "step-finish" => push_extra_array(&mut m.extra, "steps", part.clone()),
            "retry" => push_extra_array(&mut m.extra, "retries", part.clone()),
            "compaction" => push_extra_array(&mut m.extra, "compactions", part.clone()),
            other if !other.is_empty() => {
                // Unknown / future part type: never drop it silently — stash verbatim.
                push_extra_array(&mut m.extra, "unknown_parts", part.clone());
            }
            _ => {}
        }
    }

    // Older inline-summary generation stored no parts — fall back to the AI summary so the turn
    // isn't blank. (`summary` must be an *object* here; a boolean `summary: true` is a newer
    // compaction marker handled in collect_message_extra.)
    if m.content.is_empty() {
        let summary = mv
            .pointer("/summary/body")
            .or_else(|| mv.pointer("/summary/title"))
            .and_then(Value::as_str);
        if let Some(text) = summary {
            if !text.is_empty() {
                m.content.push(Block::Text {
                    text: text.to_string().into(),
                });
            }
        }
    }

    let mut out = Vec::new();
    if !m.content.is_empty() || !m.extra.is_empty() {
        out.push(m);
    }
    if !tool_results.is_empty() {
        let mut tm = Message::new(Role::Tool);
        tm.content = tool_results;
        out.push(tm);
    }
    out
}

/// Build the `ToolUse` block (and optional `ToolResult`) for a `tool` part. Tolerant of both the
/// `completed`/`error`/`running`/`pending` state shapes.
fn tool_blocks(part: &Value) -> (Block, Option<Block>) {
    let call_id = part
        .get("callID")
        .or_else(|| part.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let name = part.get("tool").and_then(Value::as_str).unwrap_or("").to_string();
    let status = part.pointer("/state/status").and_then(Value::as_str).unwrap_or("");

    let tool_name = (!name.is_empty()).then(|| name.clone());
    let status_field = (!status.is_empty()).then(|| status.to_string());
    let use_block = Block::ToolUse {
        id: call_id.clone(),
        name,
        input: part.pointer("/state/input").cloned().unwrap_or(Value::Null),
    };

    // A completed tool carries `output`; an error tool carries `error`.
    let result = match status {
        "completed" | "running" | "pending" => part.pointer("/state/output").map(|out| Block::ToolResult {
            tool_use_id: call_id.clone(),
            content: coerce_text(out).into(),
            is_error: false,
            tool_name: tool_name.clone(),
            status: status_field.clone(),
            details: None,
        }),
        "error" => {
            let content = part
                .pointer("/state/error")
                .or_else(|| part.pointer("/state/output"))
                .map(coerce_text)
                .unwrap_or_default();
            Some(Block::ToolResult {
                tool_use_id: call_id.clone(),
                content: content.into(),
                is_error: true,
                tool_name: tool_name.clone(),
                status: status_field.clone(),
                details: None,
            })
        }
        // Unknown status but an output is present — still surface it.
        _ => part.pointer("/state/output").map(|out| Block::ToolResult {
            tool_use_id: call_id.clone(),
            content: coerce_text(out).into(),
            is_error: false,
            tool_name: tool_name.clone(),
            status: status_field.clone(),
            details: None,
        }),
    };

    (use_block, result)
}

/// Map a `file` part to a block. Genuine images become `Block::Image`; everything else
/// (text files, directories via `@path`, symbol/resource refs) becomes a first-class
/// `Block::File` reference.
fn file_block(part: &Value) -> Block {
    let mime = part.get("mime").and_then(Value::as_str).unwrap_or("");
    if mime.starts_with("image/") {
        Block::Image {
            media_type: (!mime.is_empty()).then(|| mime.to_string()),
            data_ref: part.get("url").and_then(Value::as_str).map(str::to_string),
        }
    } else {
        // `source.path` is the workspace-relative path when present; `filename`/`url` are fallbacks.
        let path = part
            .pointer("/source/path")
            .or_else(|| part.get("filename"))
            .and_then(Value::as_str)
            .map(str::to_string);
        Block::File {
            mime: (!mime.is_empty()).then(|| mime.to_string()),
            path,
            source: part.get("url").and_then(Value::as_str).map(str::to_string),
        }
    }
}

/// Preserve message-level fields the IR has no home for, verbatim, in `Message.extra`.
fn collect_message_extra(mv: &Value, extra: &mut Map<String, Value>) {
    for key in [
        "agent",      // which agent ran this turn (Sisyphus, build, compaction, …)
        "mode",       // deprecated alias of agent; kept for old records
        "providerID", // amazon-bedrock, anthropic, …
        "cost",       // USD cost of the turn
        "finish",     // finish reason: stop | tool-calls | length | …
        "error",      // assistant error object (aborted / overflow / api error)
        "variant",    // model variant
        "structured", // structured-output result
        "format",     // requested output format
        "system",     // per-message system prompt (user records)
        "tools",      // enabled-tools map (user records)
    ] {
        if let Some(v) = mv.get(key) {
            if !v.is_null() {
                extra.insert(key.to_string(), v.clone());
            }
        }
    }
    // `path: {cwd, root}` — per-turn working dir.
    if let Some(cwd) = mv.pointer("/path/cwd").and_then(Value::as_str) {
        extra.insert("cwd".to_string(), json!(cwd));
    }
    // A boolean `summary: true` marks a compaction summary message (newer generation). Don't
    // confuse it with the older inline `summary` object.
    if mv.get("summary").and_then(Value::as_bool) == Some(true) {
        extra.insert("is_compaction_summary".to_string(), json!(true));
    }
    // The older inline summary's `diffs[]` are worth keeping even though we render body as text.
    if let Some(diffs) = mv.pointer("/summary/diffs") {
        if diffs.is_array() && !diffs.as_array().map(|a| a.is_empty()).unwrap_or(true) {
            extra.insert("summary_diffs".to_string(), diffs.clone());
        }
    }
    // tokens.reasoning isn't on Usage; keep it visible.
    if let Some(r) = mv.pointer("/tokens/reasoning").and_then(Value::as_u64) {
        if r > 0 {
            extra.insert("reasoning_tokens".to_string(), json!(r));
        }
    }
}

fn push_extra_array(extra: &mut Map<String, Value>, key: &str, v: Value) {
    match extra.get_mut(key) {
        Some(Value::Array(a)) => a.push(v),
        _ => {
            extra.insert(key.to_string(), Value::Array(vec![v]));
        }
    }
}

/// Look for a `signature` string anywhere one provider-level deep inside `metadata`
/// (`metadata.<provider>.signature`), without hard-coding the provider name.
fn find_signature(metadata: Option<&Value>) -> Option<&str> {
    let obj = metadata?.as_object()?;
    for v in obj.values() {
        if let Some(sig) = v.pointer("/signature").and_then(Value::as_str) {
            return Some(sig);
        }
    }
    None
}

fn coerce_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn parse_tokens(v: Option<&Value>) -> Option<Usage> {
    let v = v?;
    Some(Usage {
        input_tokens: v.get("input").and_then(Value::as_u64),
        output_tokens: v.get("output").and_then(Value::as_u64),
        cache_read_tokens: v.pointer("/cache/read").and_then(Value::as_u64),
        cache_creation_tokens: v.pointer("/cache/write").and_then(Value::as_u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(json_strs: &[&str]) -> Vec<Value> {
        json_strs.iter().map(|s| serde_json::from_str(s).unwrap()).collect()
    }

    #[test]
    fn parts_generation_text_reasoning_tool_split() {
        let mv: Value = serde_json::from_str(
            r#"{"id":"msg_1","role":"assistant","modelID":"claude","agent":"build",
                "providerID":"anthropic","cost":0.01,"finish":"tool-calls",
                "time":{"created":1767000000000},
                "tokens":{"input":10,"output":5,"reasoning":3,"cache":{"read":1,"write":2}}}"#,
        )
        .unwrap();
        let p = parts(&[
            r#"{"type":"reasoning","text":"thinking...","metadata":{"anthropic":{"signature":"SIG"}},"time":{"start":1}}"#,
            r#"{"type":"text","text":"Hello"}"#,
            r#"{"type":"tool","callID":"c1","tool":"bash","state":{"status":"completed","input":{"command":"ls"},"output":"a\nb","title":"List","metadata":{"exit":0}}}"#,
        ]);
        let out = build_messages(&mv, &p);
        // assistant message + a tool-role message
        assert_eq!(out.len(), 2);
        let asst = &out[0];
        assert_eq!(asst.role, Role::Assistant);
        assert_eq!(asst.model.as_deref(), Some("claude"));
        // thinking signature preserved
        match &asst.content[0] {
            Block::Thinking { text, signature, .. } => {
                assert_eq!(text, "thinking...");
                assert_eq!(signature.as_deref(), Some("SIG"));
            }
            b => panic!("expected thinking, got {b:?}"),
        }
        assert!(matches!(&asst.content[1], Block::Text { text } if text == "Hello"));
        assert!(matches!(&asst.content[2], Block::ToolUse { name, .. } if name == "bash"));
        // message-level extras
        assert_eq!(asst.extra.get("agent").and_then(Value::as_str), Some("build"));
        assert_eq!(asst.extra.get("finish").and_then(Value::as_str), Some("tool-calls"));
        assert_eq!(asst.extra.get("reasoning_tokens").and_then(Value::as_u64), Some(3));
        // usage
        let u = asst.usage.as_ref().unwrap();
        assert_eq!(u.input_tokens, Some(10));
        assert_eq!(u.cache_creation_tokens, Some(2));
        // tool result became its own Tool message
        assert_eq!(out[1].role, Role::Tool);
        match &out[1].content[0] {
            Block::ToolResult {
                content,
                is_error,
                tool_use_id,
                tool_name,
                status,
                ..
            } => {
                assert_eq!(content, "a\nb");
                assert!(!is_error);
                assert_eq!(tool_use_id, "c1");
                assert_eq!(tool_name.as_deref(), Some("bash"));
                assert_eq!(status.as_deref(), Some("completed"));
            }
            b => panic!("expected tool result, got {b:?}"),
        }
    }

    #[test]
    fn tool_error_state_marks_is_error() {
        let mv: Value = serde_json::from_str(r#"{"id":"m","role":"assistant","time":{"created":1}}"#).unwrap();
        let p = parts(&[
            r#"{"type":"tool","callID":"c","tool":"edit","state":{"status":"error","input":{},"error":"oldString not found","time":{"start":1,"end":2}}}"#,
        ]);
        let out = build_messages(&mv, &p);
        let res = out.iter().flat_map(|m| &m.content).find_map(|b| match b {
            Block::ToolResult { content, is_error, .. } => Some((content.to_string(), *is_error)),
            _ => None,
        });
        assert_eq!(res, Some(("oldString not found".to_string(), true)));
    }

    #[test]
    fn file_part_image_vs_reference() {
        let mv: Value = serde_json::from_str(r#"{"id":"m","role":"user","time":{"created":1}}"#).unwrap();
        let p = parts(&[
            r#"{"type":"file","mime":"image/png","url":"file:///x.png","filename":"x.png"}"#,
            r#"{"type":"file","mime":"text/plain","url":"file:///Makefile","filename":"Makefile","source":{"type":"file","path":"Makefile"}}"#,
            r#"{"type":"file","mime":"application/x-directory","url":"file:///tools/","filename":"tools/"}"#,
        ]);
        let out = build_messages(&mv, &p);
        let m = &out[0];
        // image -> Image block
        assert!(matches!(&m.content[0], Block::Image { media_type, .. } if media_type.as_deref()==Some("image/png")));
        // text file -> File reference, NOT an image
        assert!(matches!(&m.content[1], Block::File { path, .. } if path.as_deref()==Some("Makefile")));
        // directory -> File reference, NOT an image
        assert!(
            matches!(&m.content[2], Block::File { source, .. } if source.as_deref().unwrap_or("").contains("tools/"))
        );
    }

    #[test]
    fn agent_subtask_patch_snapshot_steps_preserved() {
        let mv: Value = serde_json::from_str(r#"{"id":"m","role":"assistant","time":{"created":1}}"#).unwrap();
        let p = parts(&[
            r#"{"type":"agent","name":"oracle","source":{"value":"@oracle","start":0,"end":7}}"#,
            r#"{"type":"subtask","prompt":"do the thing","description":"d","agent":"build"}"#,
            r#"{"type":"patch","hash":"abc","files":["/a.rs"]}"#,
            r#"{"type":"snapshot","snapshot":"snap123"}"#,
            r#"{"type":"step-start"}"#,
            r#"{"type":"step-finish","reason":"tool-calls","cost":0,"tokens":{"input":1,"output":1,"reasoning":0,"cache":{"read":0,"write":0}}}"#,
            r#"{"type":"retry","attempt":1,"error":{"name":"APIError"},"time":{"created":9}}"#,
            r#"{"type":"compaction","auto":false}"#,
            r#"{"type":"galaxy-brain-future-part","wat":true}"#,
        ]);
        let out = build_messages(&mv, &p);
        let m = &out[0];
        assert_eq!(
            m.extra.get("patches").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(1)
        );
        assert_eq!(
            m.extra.get("snapshots").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(1)
        );
        assert_eq!(
            m.extra.get("steps").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(2)
        );
        assert_eq!(
            m.extra.get("retries").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(1)
        );
        assert_eq!(
            m.extra.get("compactions").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(1)
        );
        assert_eq!(
            m.extra.get("agent_refs").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(1)
        );
        assert_eq!(
            m.extra.get("subtasks").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(1)
        );
        // unknown part type is never dropped
        assert_eq!(
            m.extra.get("unknown_parts").and_then(|v| v.as_array()).map(|a| a.len()),
            Some(1)
        );
        // agent + subtask also surface as text
        assert!(m
            .content
            .iter()
            .any(|b| matches!(b, Block::Text { text } if text == "@oracle")));
        assert!(m
            .content
            .iter()
            .any(|b| matches!(b, Block::Text { text } if text.contains("subtask → build"))));
    }

    #[test]
    fn inline_summary_generation_fallback() {
        // Older generation: no parts, inline summary object with diffs.
        let mv: Value = serde_json::from_str(
            r#"{"id":"m","role":"user","time":{"created":1},
                "summary":{"title":"Cleanup","body":"Removed debug comments.",
                "diffs":[{"file":"a.rs","before":"x","after":"y"}]}}"#,
        )
        .unwrap();
        let out = build_messages(&mv, &[]);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0].content[0], Block::Text { text } if text == "Removed debug comments."));
        // diffs preserved in extra
        assert!(out[0].extra.get("summary_diffs").is_some());
    }

    #[test]
    fn boolean_summary_is_compaction_marker_not_text() {
        // Newer generation: summary:true is a compaction flag, not inline content.
        let mv: Value = serde_json::from_str(
            r#"{"id":"m","role":"assistant","time":{"created":1},"agent":"compaction","summary":true}"#,
        )
        .unwrap();
        let p = parts(&[r#"{"type":"text","text":"compacted history…"}"#]);
        let out = build_messages(&mv, &p);
        let m = &out[0];
        assert_eq!(
            m.extra.get("is_compaction_summary").and_then(Value::as_bool),
            Some(true)
        );
        // the boolean must NOT be rendered as a text body; the real text comes from parts
        assert!(matches!(&m.content[0], Block::Text { text } if text == "compacted history…"));
    }

    #[test]
    fn synthetic_text_still_rendered() {
        let mv: Value = serde_json::from_str(r#"{"id":"m","role":"user","time":{"created":1}}"#).unwrap();
        let p = parts(&[r#"{"type":"text","text":"[search-mode] ...","synthetic":true}"#]);
        let out = build_messages(&mv, &p);
        assert!(matches!(&out[0].content[0], Block::Text { text } if text.starts_with("[search-mode]")));
    }

    #[test]
    fn user_model_nested_under_model_object() {
        let mv: Value = serde_json::from_str(
            r#"{"id":"m","role":"user","time":{"created":1},
                "model":{"providerID":"amazon-bedrock","modelID":"claude-opus"}}"#,
        )
        .unwrap();
        let p = parts(&[r#"{"type":"text","text":"hi"}"#]);
        let out = build_messages(&mv, &p);
        assert_eq!(out[0].model.as_deref(), Some("claude-opus"));
    }

    /// End-to-end against on-disk fixtures for BOTH storage generations.
    #[test]
    fn parses_fixtures_both_generations() {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode");

        // Newer parts generation.
        let oc = OpenCode {
            root: Some(base.join("parts_gen")),
        };
        let refs = oc.discover().unwrap();
        assert_eq!(refs.len(), 1, "one session in parts_gen fixture");
        let s = oc.parse(&refs[0]).unwrap();
        assert!(s.messages.iter().any(|m| m.role == Role::Assistant));
        assert!(s
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, Block::ToolUse { name, .. } if name == "bash")));
        assert!(s.messages.iter().any(|m| m.role == Role::Tool && !m.content.is_empty()));
        assert_eq!(s.model.as_deref(), Some("anthropic.claude-sonnet-4-5"));

        // Older inline-summary generation (message records only, no part files).
        let oc2 = OpenCode {
            root: Some(base.join("summary_gen")),
        };
        let refs2 = oc2.discover().unwrap();
        assert_eq!(refs2.len(), 1, "one session in summary_gen fixture");
        let s2 = oc2.parse(&refs2[0]).unwrap();
        assert!(s2
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, Block::Text { text } if text.contains("debug comments"))));
    }
}
