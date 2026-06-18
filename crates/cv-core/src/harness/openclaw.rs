//! OpenClaw adapter — `~/.openclaw/agents/<agentId>/sessions/`.
//!
//! See `docs/FORMATS.md`. A `sessions.json` index maps session keys → metadata; each session is a
//! `<sessionId>.jsonl` (or `<sessionId>-topic-<topicId>.jsonl`) transcript.
//!
//! ## On-disk shape (reverse-engineered from the OpenClaw TS source)
//!
//! Source of truth lives in `openclaw/packages/agent-core/src/llm.ts` (the LLM message union),
//! `.../harness/messages.ts` (the custom-message declaration-merge union), and
//! `openclaw/src/config/sessions/transcript*.ts` (read/write/append).
//!
//! - **Header line** (always first): `{type:"session", version:N, id, timestamp(ISO), cwd}`.
//!   `CURRENT_SESSION_VERSION = 3`. Older transcripts: **v1/v2 are "linear"** (entries have no
//!   `parentId`); on first append OpenClaw migrates them in place to **v3 "parent-linked"** by
//!   assigning each entry a `parentId` pointing at the prior entry's `id` (root = `null`). So on
//!   disk you'll see a mix: v3 with `parentId` threading, and legacy v1/v2 without it.
//! - **Message line**: `{type:"message", id, parentId, timestamp(ISO), message:{…}}`. `id` is an
//!   8-hex-char slice of a UUID (or full UUID on collision). `message.timestamp` is epoch-ms.
//! - **`message.role`** is the AgentMessage union discriminator:
//!   - `user`     — content: `string | (TextContent|ImageContent)[]`.
//!   - `assistant`— content: `(TextContent|ThinkingContent|ToolCall)[]`; plus `api`, `provider`,
//!     `model`, `responseModel?`, `responseId?`, `stopReason`, `errorMessage?`, `diagnostics?`,
//!     `usage{input,output,cacheRead,cacheWrite,totalTokens,cost{…}}`.
//!   - `toolResult` — `{toolCallId, toolName, content:(TextContent|ImageContent)[], isError,
//!     details?}`.
//!   - **Custom (declaration-merged) roles** persisted alongside normal history:
//!     `bashExecution`, `branchSummary`, `compactionSummary`, `custom` (with `customType`, e.g.
//!     `openclaw.cache-ttl`). These are NOT LLM messages; OpenClaw flattens them to user text only
//!     when building the model context.
//! - **Content blocks**: `text{text, textSignature?}`, `thinking{thinking, thinkingSignature?,
//!   redacted?}`, `toolCall{id,name,arguments,thoughtSignature?,executionMode?}`,
//!   `image{data, mimeType}`.
//! - **Secret redaction** is applied at write time (`redactTranscriptMessage`): secrets become
//!   `prefix…suffix` / `***` / `…redacted…` (PEM). There is no fixed sentinel string, so we cannot
//!   reliably detect/flag redactions — they just look like truncated values.
//! - **ACP-bridged sessions** (OpenClaw fronting an external agent like Claude Code / Codex via the
//!   ACP control plane): the rich turn (tool calls, thinking, real usage) lives in the *external*
//!   agent's own store, NOT here. OpenClaw only persists a `user` prompt and a single text
//!   `assistant` reply tagged `provider:"openclaw", model:"acp-runtime"`. Embedded-CLI turns use
//!   `api:"cli"` with the real provider/model. Delivery mirrors use
//!   `provider:"openclaw", model:"delivery-mirror"` (or `gateway-injected`) — transcript-only echoes.

use super::{parse_ts, ts_from_value, Adapter};
use crate::ir::*;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct OpenClaw {
    roots: Vec<PathBuf>,
}

impl OpenClaw {
    pub fn new() -> Self {
        let mut roots = Vec::new();
        for base in [
            dirs::home_dir().map(|h| h.join(".openclaw")),
            dirs::home_dir().map(|h| h.join("elide-home").join(".openclaw")),
        ]
        .into_iter()
        .flatten()
        {
            let agents = base.join("agents");
            if agents.exists() {
                roots.push(agents);
            }
        }
        OpenClaw { roots }
    }
}

impl Default for OpenClaw {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for OpenClaw {
    fn harness(&self) -> Harness {
        Harness::OpenClaw
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.roots.first().cloned()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let mut out = Vec::new();
        for root in &self.roots {
            // sessions live at agents/<agentId>/sessions/<sid>.jsonl
            for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                if path.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()) != Some("sessions") {
                    continue;
                }
                // Skip compaction-checkpoint sidecar transcripts (e.g. `<sid>-compaction-<id>.jsonl`);
                // these are pre/post-compaction snapshots, not the live thread.
                if is_compaction_checkpoint(path) {
                    continue;
                }
                let index = load_index(path.parent().unwrap());
                match scan(path, &index) {
                    Ok(r) => out.push(r),
                    Err(e) => eprintln!("cv: skipping {}: {e:#}", path.display()),
                }
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        crate::stream::collect(self, r)
    }

    fn stream(&self, r: &SessionRef, _opts: &ParseOptions, sink: &mut dyn MessageSink) -> Result<Session> {
        // One record per line; stream them so peak memory is O(largest line), not O(transcript).
        // `filter_map`, not `map_while`: a single undecodable line (stray non-UTF8 bytes) must be
        // skipped like any other corrupt record — `map_while` would silently TRUNCATE the whole
        // rest of the transcript at it. The lint's run-forever concern doesn't apply: on a regular
        // file an invalid-UTF8 `Err` still consumes that line's bytes, so the iterator advances to
        // EOF (same tolerance as `for_each_json_line`).
        #[allow(clippy::lines_filter_map_ok)]
        let lines = {
            let file = fs::File::open(&r.path).with_context(|| format!("reading {}", r.path.display()))?;
            BufReader::new(file).lines().filter_map(Result::ok)
        };
        Ok(stream_core(lines, r, sink))
    }

    fn can_emit(&self) -> bool {
        false
    }
}

/// Core parse, split out so tests can drive it from a fixture string. Full fidelity (collects).
/// Test-only: production reads go through [`Adapter::stream`]/[`Adapter::parse`] → [`stream_core`].
#[cfg(test)]
fn parse_text(text: &str, r: &SessionRef) -> Session {
    let mut sink = crate::stream::CollectSink::default();
    let mut s = stream_core(text.lines().map(str::to_string), r, &mut sink);
    s.messages = sink.messages;
    s
}

/// Streaming core shared by the on-disk [`Adapter::stream`] and the string-driven `parse_text`:
/// fold each record's metadata into the session and emit any message to `sink`. Returns the session
/// with empty `messages`.
fn stream_core<I: Iterator<Item = String>>(lines: I, r: &SessionRef, sink: &mut dyn MessageSink) -> Session {
    let mut s = Session {
        id: r.id.clone(),
        harness: Harness::OpenClaw,
        cwd: r.cwd.clone(),
        title: r.title.clone(),
        created_at: r.created_at,
        updated_at: r.updated_at,
        model: None,
        git: None,
        messages: Vec::new(),
        source_path: Some(r.path.clone()),
        extra: serde_json::Map::new(),
    };
    let mut header_version: Option<i64> = None;
    let mut meta_sent = false;

    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("session") => {
                header_version = v.get("version").and_then(Value::as_i64);
                if s.cwd.is_none() {
                    s.cwd = v.get("cwd").and_then(Value::as_str).map(PathBuf::from);
                }
                if s.created_at.is_none() {
                    s.created_at = v.get("timestamp").and_then(Value::as_str).and_then(parse_ts);
                }
            }
            Some("message") => {
                if let Some(mut m) = parse_entry(&v) {
                    // The session model is the first real model we see; ignore the synthetic
                    // openclaw transcript-only providers (delivery mirrors / acp-runtime).
                    if s.model.is_none() {
                        if let Some(model) = m.model.as_deref() {
                            if !is_synthetic_openclaw_model(model) {
                                s.model = Some(model.to_string());
                            }
                        }
                    }
                    // Stash the transcript schema version on the first message so a downstream
                    // consumer can tell v1/v2 (linear) from v3 (parent-linked) sessions.
                    if let Some(ver) = header_version.take() {
                        m.extra.insert("openclaw_session_version".into(), Value::from(ver));
                    }
                    // Hand session metadata to the sink before the first message (header consumers).
                    if !meta_sent {
                        sink.meta(&s);
                        meta_sent = true;
                    }
                    if sink.message(m) == Flow::Stop {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    if !meta_sent {
        sink.meta(&s);
    }
    s
}

/// Models OpenClaw writes for transcript-only / bridged turns that should not be reported as the
/// session's "real" model.
fn is_synthetic_openclaw_model(model: &str) -> bool {
    matches!(model, "delivery-mirror" | "gateway-injected" | "acp-runtime")
}

/// `<sid>-compaction-<id>.jsonl` checkpoint sidecars (see `artifacts.ts`).
fn is_compaction_checkpoint(path: &Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.contains("-compaction-"))
        .unwrap_or(false)
}

/// Load `sessions.json` (sessionId → metadata) from a sessions dir, if present.
fn load_index(sessions_dir: &Path) -> HashMap<String, Value> {
    let mut map = HashMap::new();
    if let Ok(text) = fs::read_to_string(sessions_dir.join("sessions.json")) {
        if let Ok(Value::Object(entries)) = serde_json::from_str::<Value>(&text) {
            for (_key, entry) in entries {
                if let Some(sid) = entry.get("sessionId").and_then(Value::as_str) {
                    map.insert(sid.to_string(), entry);
                }
            }
        }
    }
    map
}

fn session_id_from_filename(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    // strip a `-topic-<id>` suffix if present
    match stem.split_once("-topic-") {
        Some((sid, _)) => sid.to_string(),
        None => stem.to_string(),
    }
}

fn scan(path: &Path, index: &HashMap<String, Value>) -> Result<SessionRef> {
    let text = fs::read_to_string(path)?;
    let mut id = String::new();
    let mut cwd = None;
    let mut created_at: Option<DateTime<Utc>> = None;
    let mut updated_at: Option<DateTime<Utc>> = None;
    let mut first_user: Option<String> = None;
    let mut message_count = 0usize;

    super::for_each_json_line_str(&text, |v| {
        match v.get("type").and_then(Value::as_str) {
            Some("session") => {
                if id.is_empty() {
                    id = v.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                }
                cwd = v.get("cwd").and_then(Value::as_str).map(PathBuf::from);
                created_at = v.get("timestamp").and_then(Value::as_str).and_then(parse_ts);
            }
            Some("message") => {
                message_count += 1;
                if let Some(ts) = entry_timestamp(&v) {
                    updated_at = Some(ts);
                }
                if first_user.is_none() && v.pointer("/message/role").and_then(Value::as_str) == Some("user") {
                    let t = coerce_content_text(v.pointer("/message/content"));
                    if !t.trim().is_empty() {
                        first_user = Some(crate::ir::truncate(&t, 80));
                    }
                }
            }
            _ => {}
        }
        Flow::Continue
    });

    if id.is_empty() {
        id = session_id_from_filename(path);
    }
    let entry = index.get(&id);
    let title = entry
        .and_then(|e| e.get("label").or_else(|| e.get("subject")))
        .and_then(Value::as_str)
        .map(|s| crate::ir::truncate(s, 80))
        .or(first_user);
    if cwd.is_none() {
        cwd = entry
            .and_then(|e| e.get("cwd"))
            .and_then(Value::as_str)
            .map(PathBuf::from);
    }
    // `updatedAt` in the index is epoch-ms.
    let entry_updated = entry.and_then(|e| e.get("updatedAt")).and_then(ts_from_value);

    Ok(SessionRef {
        id,
        harness: Harness::OpenClaw,
        path: path.to_path_buf(),
        cwd,
        title,
        created_at,
        updated_at: entry_updated.or(updated_at),
        message_count,
    })
}

/// Prefer the entry-level ISO `timestamp`, fall back to the inner epoch-ms `message.timestamp`.
fn entry_timestamp(v: &Value) -> Option<DateTime<Utc>> {
    v.get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_ts)
        .or_else(|| v.pointer("/message/timestamp").and_then(ts_from_value))
}

fn parse_entry(v: &Value) -> Option<Message> {
    let msg = v.get("message")?;
    let role_str = msg.get("role").and_then(Value::as_str)?;
    let role = match role_str {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "toolResult" => Role::Tool,
        "system" => Role::System,
        // Custom (declaration-merged) message types. Modeled as System turns so they ride along
        // in the IR with their semantics preserved in `extra`/blocks rather than being dropped.
        "bashExecution" | "branchSummary" | "compactionSummary" | "custom" => Role::System,
        _ => return None,
    };
    let mut m = Message::new(role);
    m.id = v.get("id").and_then(Value::as_str).map(str::to_string);
    m.parent_id = v.get("parentId").and_then(Value::as_str).map(str::to_string);
    m.timestamp = entry_timestamp(v);
    m.model = msg.get("model").and_then(Value::as_str).map(str::to_string);

    match role_str {
        "toolResult" => parse_tool_result(&mut m, msg),
        "assistant" => {
            parse_content_array(&mut m, msg.get("content"));
            capture_assistant_meta(&mut m, msg);
        }
        "user" | "system" => match msg.get("content") {
            Some(Value::String(s)) => m.content.push(Block::Text { text: s.clone().into() }),
            content => parse_content_array(&mut m, content),
        },
        "bashExecution" => parse_bash_execution(&mut m, msg),
        "branchSummary" => parse_branch_summary(&mut m, msg),
        "compactionSummary" => parse_compaction_summary(&mut m, msg),
        "custom" => parse_custom(&mut m, msg),
        _ => {}
    }

    (!m.content.is_empty() || !m.extra.is_empty()).then_some(m)
}

fn parse_tool_result(m: &mut Message, msg: &Value) {
    let tool_use_id = msg.get("toolCallId").and_then(Value::as_str).unwrap_or("").to_string();
    m.content.push(Block::ToolResult {
        tool_use_id,
        content: coerce_content_text(msg.get("content")).into(),
        is_error: msg.get("isError").and_then(Value::as_bool).unwrap_or(false),
        tool_name: msg.get("toolName").and_then(Value::as_str).map(str::to_string),
        status: None,
        details: msg.get("details").filter(|d| !d.is_null()).cloned(),
    });
    // Image parts in a tool result aren't captured by the text coercion above; surface them.
    if let Some(Value::Array(items)) = msg.get("content") {
        for item in items {
            if item.get("type").and_then(Value::as_str) == Some("image") {
                if let Some(b) = content_block(item) {
                    m.content.push(b);
                }
            }
        }
    }
    if let Some(name) = msg.get("toolName").and_then(Value::as_str) {
        m.extra.insert("tool_name".into(), Value::from(name));
    }
    // `details` is an arbitrary structured payload (e.g. diffs, file lists) — preserve verbatim.
    if let Some(details) = msg.get("details") {
        if !details.is_null() {
            m.extra.insert("tool_details".into(), details.clone());
        }
    }
}

/// Parse a `(TextContent|ThinkingContent|ToolCall|ImageContent)[]` content array.
fn parse_content_array(m: &mut Message, content: Option<&Value>) {
    if let Some(Value::Array(items)) = content {
        for item in items {
            if let Some(b) = content_block(item) {
                m.content.push(b);
            }
        }
    }
}

/// Capture assistant-level metadata (provider/api/model/usage/stop/diagnostics) into `extra`.
fn capture_assistant_meta(m: &mut Message, msg: &Value) {
    for key in [
        "api",
        "provider",
        "responseModel",
        "responseId",
        "stopReason",
        "errorMessage",
    ] {
        if let Some(val) = msg.get(key) {
            if !val.is_null() {
                m.extra.insert(snake(key), val.clone());
            }
        }
    }
    if let Some(diags) = msg.get("diagnostics") {
        if !diags.is_null() {
            m.extra.insert("diagnostics".into(), diags.clone());
        }
    }
    if let Some(usage) = msg.get("usage") {
        m.usage = parse_usage(usage);
        // The full usage object also carries cost breakdowns; keep it verbatim.
        m.extra.insert("usage_raw".into(), usage.clone());
    }
}

fn parse_usage(usage: &Value) -> Option<Usage> {
    let get = |k: &str| usage.get(k).and_then(Value::as_u64);
    let u = Usage {
        input_tokens: get("input"),
        output_tokens: get("output"),
        cache_read_tokens: get("cacheRead"),
        cache_creation_tokens: get("cacheWrite"),
    };
    let any = u.input_tokens.is_some()
        || u.output_tokens.is_some()
        || u.cache_read_tokens.is_some()
        || u.cache_creation_tokens.is_some();
    any.then_some(u)
}

fn parse_bash_execution(m: &mut Message, msg: &Value) {
    let command = msg.get("command").and_then(Value::as_str).unwrap_or("");
    let output = msg.get("output").and_then(Value::as_str).unwrap_or("");
    let mut text = format!("$ {command}");
    if !output.is_empty() {
        text.push('\n');
        text.push_str(output);
    }
    m.content.push(Block::Text { text: text.into() });
    m.extra
        .insert("openclaw_message_role".into(), Value::from("bashExecution"));
    for key in [
        "exitCode",
        "cancelled",
        "truncated",
        "fullOutputPath",
        "excludeFromContext",
    ] {
        if let Some(val) = msg.get(key) {
            if !val.is_null() {
                m.extra.insert(snake(key), val.clone());
            }
        }
    }
}

fn parse_branch_summary(m: &mut Message, msg: &Value) {
    let summary = msg.get("summary").and_then(Value::as_str).unwrap_or("");
    m.content.push(Block::Text {
        text: summary.to_string().into(),
    });
    m.extra
        .insert("openclaw_message_role".into(), Value::from("branchSummary"));
    if let Some(from_id) = msg.get("fromId").and_then(Value::as_str) {
        m.extra.insert("branch_from_id".into(), Value::from(from_id));
    }
}

fn parse_compaction_summary(m: &mut Message, msg: &Value) {
    let summary = msg.get("summary").and_then(Value::as_str).unwrap_or("");
    m.content.push(Block::Text {
        text: summary.to_string().into(),
    });
    m.extra
        .insert("openclaw_message_role".into(), Value::from("compactionSummary"));
    for key in ["tokensBefore", "tokensAfter", "firstKeptEntryId"] {
        if let Some(val) = msg.get(key) {
            if !val.is_null() {
                m.extra.insert(snake(key), val.clone());
            }
        }
    }
}

fn parse_custom(m: &mut Message, msg: &Value) {
    let text = coerce_content_text(msg.get("content"));
    if !text.is_empty() {
        m.content.push(Block::Text { text: text.into() });
    }
    m.extra.insert("openclaw_message_role".into(), Value::from("custom"));
    if let Some(ct) = msg.get("customType").and_then(Value::as_str) {
        m.extra.insert("custom_type".into(), Value::from(ct));
    }
    if let Some(display) = msg.get("display") {
        if !display.is_null() {
            m.extra.insert("display".into(), display.clone());
        }
    }
    if let Some(details) = msg.get("details") {
        if !details.is_null() {
            m.extra.insert("custom_details".into(), details.clone());
        }
    }
}

fn content_block(item: &Value) -> Option<Block> {
    match item.get("type").and_then(Value::as_str)? {
        "text" => Some(Block::Text {
            text: item.get("text").and_then(Value::as_str)?.to_string().into(),
        }),
        "thinking" => Some(Block::Thinking {
            text: item
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
                .into(),
            // OpenClaw uses `thinkingSignature`; `redacted` flags a server-redacted reasoning block.
            signature: item
                .get("thinkingSignature")
                .and_then(Value::as_str)
                .map(str::to_string),
            encrypted: item
                .get("redacted")
                .and_then(Value::as_bool)
                .filter(|&r| r)
                .map(|_| "redacted".to_string()),
            redacted: item.get("redacted").and_then(Value::as_bool).unwrap_or(false),
        }),
        "toolCall" => Some(Block::ToolUse {
            id: item.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
            name: item.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
            input: item.get("arguments").cloned().unwrap_or(Value::Null),
        }),
        "image" => Some(Block::Image {
            media_type: item.get("mimeType").and_then(Value::as_str).map(str::to_string),
            data_ref: item
                .get("data")
                .and_then(Value::as_str)
                .map(|d| crate::ir::truncate(d, 80)),
        }),
        _ => None,
    }
}

/// Coerce a `content` field (string | array of text/image blocks) to plain text.
fn coerce_content_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| i.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// camelCase → snake_case for `extra` keys (ASCII only; OpenClaw keys are all ASCII).
fn snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            out.push('_');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_ref(name: &str) -> SessionRef {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/openclaw")
            .join(name);
        SessionRef {
            id: "test-session".into(),
            harness: Harness::OpenClaw,
            path,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        }
    }

    fn parse_fixture(name: &str) -> Session {
        let r = fixture_ref(name);
        let text = fs::read_to_string(&r.path).unwrap_or_else(|e| panic!("reading {}: {e}", r.path.display()));
        parse_text(&text, &r)
    }

    #[test]
    fn non_utf8_line_skips_not_truncates() {
        // A stray binary line mid-transcript must cost exactly that line — `map_while(Result::ok)`
        // used to end the whole stream there, silently dropping every later message.
        let dir = std::env::temp_dir().join(format!("cv-openclaw-bin-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s1.jsonl");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(
            br#"{"type":"session","version":3,"id":"s1","timestamp":"2026-01-01T00:00:00Z","cwd":"/w"}"#,
        );
        bytes.push(b'\n');
        bytes.extend_from_slice(
            br#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":"first"}}"#,
        );
        bytes.push(b'\n');
        bytes.extend_from_slice(&[0xFF, 0xFE, 0x80, 0x81]); // undecodable garbage line
        bytes.push(b'\n');
        bytes.extend_from_slice(
            br#"{"type":"message","id":"a2","parentId":"a1","timestamp":"2026-01-01T00:00:02Z","message":{"role":"user","content":"second"}}"#,
        );
        bytes.push(b'\n');
        fs::write(&path, &bytes).unwrap();

        let r = SessionRef {
            id: "s1".into(),
            harness: Harness::OpenClaw,
            path,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let s = OpenClaw { roots: vec![] }.parse(&r).unwrap();
        let texts: Vec<_> = s.messages.iter().filter_map(|m| m.text()).collect();
        assert_eq!(
            texts,
            vec!["first", "second"],
            "messages after the binary line must survive"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parses_v3_full_content_union() {
        let s = parse_fixture("v3-full.jsonl");
        // header cwd + created_at pulled from the session line
        assert_eq!(s.cwd, Some(PathBuf::from("/work/proj")));
        assert!(s.created_at.is_some());
        // real model wins over the synthetic delivery-mirror trailer
        assert_eq!(s.model.as_deref(), Some("claude-opus-4"));

        // user, assistant, toolResult, + a synthetic delivery-mirror assistant trailer (kept,
        // but its model is not promoted to the session model)
        assert_eq!(s.messages.len(), 4);
        assert_eq!(s.messages[3].model.as_deref(), Some("delivery-mirror"));
        let user = &s.messages[0];
        assert_eq!(user.role, Role::User);
        assert_eq!(user.text().as_deref(), Some("hello"));
        // version stashed on the first message
        assert_eq!(
            user.extra.get("openclaw_session_version").and_then(Value::as_i64),
            Some(3)
        );

        let asst = &s.messages[1];
        assert_eq!(asst.role, Role::Assistant);
        // thinking (with signature + redacted), text, toolCall
        let kinds: Vec<&str> = asst
            .content
            .iter()
            .map(|b| match b {
                Block::Thinking { .. } => "thinking",
                Block::Text { .. } => "text",
                Block::ToolUse { .. } => "toolcall",
                Block::ToolResult { .. } => "toolresult",
                Block::Image { .. } => "image",
                Block::File { .. } => "file",
            })
            .collect();
        assert_eq!(kinds, ["thinking", "text", "toolcall"]);
        if let Block::Thinking {
            signature, encrypted, ..
        } = &asst.content[0]
        {
            assert_eq!(signature.as_deref(), Some("sig-think"));
            assert_eq!(encrypted.as_deref(), Some("redacted"));
        } else {
            panic!("expected thinking block");
        }
        if let Block::ToolUse { id, name, .. } = &asst.content[2] {
            assert_eq!(id, "call_1");
            assert_eq!(name, "read_file");
        } else {
            panic!("expected toolcall block");
        }
        // usage + raw usage captured
        let u = asst.usage.as_ref().expect("usage");
        assert_eq!(u.input_tokens, Some(100));
        assert_eq!(u.output_tokens, Some(20));
        assert_eq!(u.cache_read_tokens, Some(5));
        assert!(asst.extra.contains_key("usage_raw"));
        assert_eq!(asst.extra.get("provider").and_then(Value::as_str), Some("anthropic"));
        assert_eq!(asst.extra.get("stop_reason").and_then(Value::as_str), Some("toolUse"));

        let tr = &s.messages[2];
        assert_eq!(tr.role, Role::Tool);
        match &tr.content[0] {
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                tool_name,
                ..
            } => {
                assert_eq!(tool_use_id, "call_1");
                assert_eq!(content, "file contents");
                assert!(!is_error);
                assert_eq!(tool_name.as_deref(), Some("read_file"));
            }
            _ => panic!("expected tool result"),
        }
        assert_eq!(tr.extra.get("tool_name").and_then(Value::as_str), Some("read_file"));
        assert!(tr.extra.contains_key("tool_details"));
    }

    #[test]
    fn parses_legacy_v1_linear() {
        let s = parse_fixture("v1-linear.jsonl");
        assert_eq!(s.messages.len(), 2);
        // v1 entries have no parentId
        assert!(s.messages[0].parent_id.is_none());
        assert_eq!(
            s.messages[0]
                .extra
                .get("openclaw_session_version")
                .and_then(Value::as_i64),
            Some(1)
        );
        assert_eq!(s.messages[0].text().as_deref(), Some("legacy first"));
    }

    #[test]
    fn parses_custom_message_types() {
        let s = parse_fixture("custom-messages.jsonl");
        // bashExecution, branchSummary, compactionSummary, custom — all System turns
        assert_eq!(s.messages.len(), 4);

        let bash = &s.messages[0];
        assert_eq!(bash.role, Role::System);
        assert_eq!(
            bash.extra.get("openclaw_message_role").and_then(Value::as_str),
            Some("bashExecution")
        );
        assert!(bash.text().unwrap().contains("$ ls -la"));
        assert_eq!(bash.extra.get("exit_code").and_then(Value::as_i64), Some(0));

        let branch = &s.messages[1];
        assert_eq!(
            branch.extra.get("openclaw_message_role").and_then(Value::as_str),
            Some("branchSummary")
        );
        assert_eq!(
            branch.extra.get("branch_from_id").and_then(Value::as_str),
            Some("abc123")
        );

        let compact = &s.messages[2];
        assert_eq!(
            compact.extra.get("openclaw_message_role").and_then(Value::as_str),
            Some("compactionSummary")
        );
        assert_eq!(compact.extra.get("tokens_before").and_then(Value::as_i64), Some(5000));

        let custom = &s.messages[3];
        assert_eq!(
            custom.extra.get("openclaw_message_role").and_then(Value::as_str),
            Some("custom")
        );
        assert_eq!(
            custom.extra.get("custom_type").and_then(Value::as_str),
            Some("openclaw.cache-ttl")
        );
    }

    #[test]
    fn parses_topic_and_acp_session() {
        // topic file: id derives from the `<sid>-topic-<id>` filename when no header id given
        assert_eq!(session_id_from_filename(Path::new("sess-1-topic-foo.jsonl")), "sess-1");

        // a real topic transcript parses like any other thread
        let topic = parse_fixture("sess-1-topic-foo.jsonl");
        assert_eq!(topic.messages.len(), 1);
        assert_eq!(topic.messages[0].text().as_deref(), Some("topic thread message"));

        // ACP-bridged session: only user prompt + acp-runtime text reply persisted.
        let s = parse_fixture("acp-session.jsonl");
        assert_eq!(s.messages.len(), 2);
        // model is NOT reported as the synthetic acp-runtime
        assert_eq!(s.model, None);
        let reply = &s.messages[1];
        assert_eq!(reply.role, Role::Assistant);
        assert_eq!(reply.model.as_deref(), Some("acp-runtime"));
        assert_eq!(reply.extra.get("provider").and_then(Value::as_str), Some("openclaw"));
        assert_eq!(reply.text().as_deref(), Some("done bridging"));
        // bridged turns have no captured tool calls / thinking — confirm the gap
        assert!(reply.content.iter().all(|b| matches!(b, Block::Text { .. })));
    }
}
