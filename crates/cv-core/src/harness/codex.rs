//! Codex CLI adapter — `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (+ archived, + 2025 legacy JSON).
//!
//! Format drift handled: the first line is either a `session_meta` record, a bare `{id,timestamp}`
//! header, or (2025 legacy) a single `{session, items[]}` JSON document. Natural-language text
//! appears both as `event_msg` and as `response_item message`; when `event_msg`s are present we take
//! NL text from them and skip the `response_item` duplicates.
//!
//! Record types (ground truth: codex-rs `protocol::RolloutItem` + `EventMsg`):
//! - `session_meta` — id, cwd, originator, cli_version, source, model_provider, git.
//! - `turn_context`  — per-turn model/cwd/approval/sandbox; we track model/cwd drift.
//! - `response_item` — the model-visible items: `message`, `reasoning`, `function_call`,
//!   `function_call_output`, `custom_tool_call(_output)`, `local_shell_call`, `web_search_call`,
//!   `tool_search_call`/`tool_search_output`, `image_generation_call`, `compaction`.
//! - `event_msg`     — UI-side events: `user_message`/`agent_message` (NL text), `token_count`
//!   (usage + rate limits), `view_image_tool_call`, `web_search_*`, `context_compacted`, etc.
//! - `compacted`     — a top-level record marking an auto/manual history compaction boundary.
//!
//! Multimodal/structured bits that the IR can't model natively (rate limits, web-search queries,
//! shell command vectors, compaction boundaries) are stashed in `Message.extra` so conversions stay
//! as lossless as the IR allows. Images become `Block::Image` with a reference (never inlined bytes).

use super::Adapter;
use crate::ir::*;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct Codex {
    roots: Vec<PathBuf>,
}

impl Codex {
    pub fn new() -> Self {
        let home = dirs::home_dir();
        let roots = home
            .map(|h| {
                vec![
                    h.join(".codex").join("sessions"),
                    h.join(".codex").join("archived_sessions"),
                ]
            })
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.exists())
            .collect();
        Codex { roots }
    }
}

impl Default for Codex {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for Codex {
    fn harness(&self) -> Harness {
        Harness::Codex
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.roots.first().cloned()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        // Walk (cheap) to collect candidate paths, then scan (file read + parse) them in parallel.
        let mut paths = Vec::new();
        for root in &self.roots {
            for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
                let path = entry.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !name.starts_with("rollout-") {
                    continue;
                }
                if !name.ends_with(".jsonl") && !name.ends_with(".json") {
                    continue;
                }
                paths.push(path.to_path_buf());
            }
        }
        Ok(crate::par_filter_map(paths, |path| {
            crate::discover_cache::cached_scan(&path, || match scan(&path) {
                Ok(r) => Some(r),
                Err(e) => {
                    eprintln!("cv: skipping {}: {e:#}", path.display());
                    None
                }
            })
        }))
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        let text = fs::read_to_string(&r.path)
            .with_context(|| format!("reading {}", r.path.display()))?;
        let is_jsonl = r.path.extension().and_then(|e| e.to_str()) == Some("jsonl");
        Ok(parse_str(&r.id, &text, is_jsonl, Some(r.path.clone())))
    }

    fn can_emit(&self) -> bool {
        false // TODO: Codex as a conversion target (the headline MVP)
    }
}

/// Parse a Codex transcript from its text contents into a [`Session`].
///
/// Pure (no filesystem); handles both the modern `.jsonl` rollout (`is_jsonl = true`) and the 2025
/// legacy single-JSON layout. `id` is the fallback session id (used when the transcript carries no
/// `session_meta`/header id); `source_path` records provenance when known.
pub fn parse_str(id: &str, text: &str, is_jsonl: bool, source_path: Option<PathBuf>) -> Session {
    // Start the id empty so a transcript-provided id (session_meta / bare header / legacy
    // `session.id`) is authoritative; the `id` argument is only the fallback when none is present.
    let mut s = Session {
        id: String::new(),
        harness: Harness::Codex,
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        model: None,
        git: None,
        messages: Vec::new(),
        source_path,
        extra: serde_json::Map::new(),
    };

    if is_jsonl {
        let lines: Vec<Value> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let has_events = lines.iter().any(|v| {
            v.get("type").and_then(Value::as_str) == Some("event_msg")
                && matches!(
                    v.pointer("/payload/type").and_then(Value::as_str),
                    Some("user_message") | Some("agent_message")
                )
        });
        for v in &lines {
            if let Some(ts) = top_ts(v) {
                s.created_at.get_or_insert(ts);
                s.updated_at = Some(ts);
            }
            match v.get("type").and_then(Value::as_str) {
                None => {
                    // bare {id,timestamp} header
                    if s.id.is_empty() {
                        if let Some(id) = v.get("id").and_then(Value::as_str) {
                            s.id = id.to_string();
                        }
                    }
                }
                Some("session_meta") => apply_meta(&mut s, v.get("payload")),
                Some("turn_context") => {
                    apply_turn_context(&mut s, v.get("payload"), top_ts(v));
                }
                Some("event_msg") => {
                    handle_event(v.get("payload"), has_events, top_ts(v), &mut s.messages);
                }
                Some("response_item") => {
                    handle_item(v.get("payload"), has_events, top_ts(v), &mut s.messages);
                }
                Some("compacted") => {
                    handle_compacted(v.get("payload"), top_ts(v), &mut s.messages);
                }
                _ => {}
            }
        }
    } else if let Ok(root) = serde_json::from_str::<Value>(text) {
        apply_meta(&mut s, root.get("session"));
        let items = root
            .get("items")
            .and_then(Value::as_array)
            .or_else(|| root.as_array());
        if let Some(items) = items {
            for it in items {
                handle_item(Some(it), false, None, &mut s.messages);
            }
        }
    }

    if s.id.is_empty() {
        s.id = id.to_string();
    }
    s
}

fn apply_meta(s: &mut Session, payload: Option<&Value>) {
    let Some(p) = payload else { return };
    if let Some(id) = p.get("id").and_then(Value::as_str) {
        if s.id.is_empty() {
            s.id = id.to_string();
        }
    }
    if s.cwd.is_none() {
        s.cwd = p.get("cwd").and_then(Value::as_str).map(PathBuf::from);
    }
    if let Some(ts) = p.get("timestamp").and_then(Value::as_str).and_then(parse_ts) {
        s.created_at.get_or_insert(ts);
    }
    if s.git.is_none() {
        if let Some(g) = p.get("git") {
            s.git = Some(GitInfo {
                branch: g.get("branch").and_then(Value::as_str).map(str::to_string),
                commit: g
                    .get("commit_hash")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                remote: g
                    .get("repository_url")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
    }
}

/// `turn_context` records persist per-turn config (model, cwd, sandbox/approval). We seed the
/// session-level `model`/`cwd` from the first one, and emit a lightweight system marker whenever the
/// effective model changes mid-session so that drift survives into the IR.
fn apply_turn_context(s: &mut Session, payload: Option<&Value>, ts: Option<DateTime<Utc>>) {
    let Some(p) = payload else { return };
    if s.cwd.is_none() {
        s.cwd = p.get("cwd").and_then(Value::as_str).map(PathBuf::from);
    }
    let model = p.get("model").and_then(Value::as_str);
    match (&s.model, model) {
        (None, Some(m)) => s.model = Some(m.to_string()),
        (Some(prev), Some(m)) if prev != m => {
            // Model switched mid-session (e.g. `/model`, escalation). Record the change as a system
            // note and update the session-level model to the latest effective value.
            let mut msg = Message::new(Role::System);
            msg.timestamp = ts;
            msg.content.push(Block::Text {
                text: format!("[model changed: {prev} → {m}]"),
            });
            msg.extra
                .insert("codex_event".into(), Value::String("turn_context".into()));
            msg.extra
                .insert("model".into(), Value::String(m.to_string()));
            if let Some(cwd) = p.get("cwd").and_then(Value::as_str) {
                msg.extra.insert("cwd".into(), Value::String(cwd.into()));
            }
            s.messages.push(msg);
            s.model = Some(m.to_string());
        }
        _ => {}
    }
}

/// Handle an `event_msg` record. These are the UI-side mirror of the model exchange. We emit
/// natural-language `user_message`/`agent_message` text (only when `has_events`, since otherwise the
/// `response_item message`s carry it), surface `view_image_tool_call` as an image attachment, and
/// attach `token_count` usage/rate-limit info to the most recent assistant message. Other events
/// (exec_command_*, web_search_*, task_started/complete, …) are intentionally not turned into
/// messages: their structured equivalents arrive as `response_item`s.
fn handle_event(
    payload: Option<&Value>,
    has_events: bool,
    ts: Option<DateTime<Utc>>,
    out: &mut Vec<Message>,
) {
    let Some(p) = payload else { return };
    match p.get("type").and_then(Value::as_str) {
        Some("user_message") if has_events => {
            if let Some(text) = p.get("message").and_then(Value::as_str) {
                let mut m = Message::new(Role::User);
                m.timestamp = ts;
                m.content.push(Block::Text { text: text.into() });
                if let Some(kind) = p.get("kind").and_then(Value::as_str) {
                    m.extra.insert("kind".into(), Value::String(kind.into()));
                }
                out.push(m);
            }
        }
        Some("agent_message") if has_events => {
            if let Some(text) = p.get("message").and_then(Value::as_str) {
                let mut m = Message::new(Role::Assistant);
                m.timestamp = ts;
                m.content.push(Block::Text { text: text.into() });
                out.push(m);
            }
        }
        Some("view_image_tool_call") => {
            // The agent attached a local image via the `view_image` tool.
            let path = p.get("path").and_then(Value::as_str);
            let mut m = Message::new(Role::Assistant);
            m.timestamp = ts;
            m.content.push(Block::Image {
                media_type: None,
                data_ref: path.map(str::to_string),
            });
            if let Some(id) = p.get("call_id").and_then(Value::as_str) {
                m.extra.insert("call_id".into(), Value::String(id.into()));
            }
            m.extra.insert(
                "codex_event".into(),
                Value::String("view_image_tool_call".into()),
            );
            out.push(m);
        }
        Some("token_count") => {
            // Usage totals + rate limits. Attach to the latest message that lacks usage so the
            // numbers ride along with the assistant turn they belong to; otherwise drop a bare
            // carrier message so the rate-limit snapshot isn't lost.
            apply_token_count(p, ts, out);
        }
        _ => {}
    }
}

/// Pull `last_token_usage` into IR [`Usage`] and stash rate-limit / context-window info in `extra`.
fn apply_token_count(p: &Value, ts: Option<DateTime<Utc>>, out: &mut Vec<Message>) {
    let info = p.get("info");
    let usage = info
        .and_then(|i| i.get("last_token_usage"))
        .and_then(parse_usage);
    let rate_limits = p.get("rate_limits").filter(|v| !v.is_null()).cloned();
    let ctx_window = info
        .and_then(|i| i.get("model_context_window"))
        .filter(|v| !v.is_null())
        .cloned();
    if usage.is_none() && rate_limits.is_none() && ctx_window.is_none() {
        return;
    }
    // Prefer attaching to the trailing assistant message of the just-finished turn.
    let target = out
        .iter_mut()
        .rev()
        .find(|m| m.role == Role::Assistant && m.usage.is_none());
    let m = match target {
        Some(m) => m,
        None => {
            let mut nm = Message::new(Role::Assistant);
            nm.timestamp = ts;
            nm.extra
                .insert("codex_event".into(), Value::String("token_count".into()));
            out.push(nm);
            out.last_mut().unwrap()
        }
    };
    if let Some(u) = usage {
        m.usage = Some(u);
    }
    if let Some(rl) = rate_limits {
        m.extra.insert("rate_limits".into(), rl);
    }
    if let Some(cw) = ctx_window {
        m.extra.insert("model_context_window".into(), cw);
    }
}

/// Codex token-usage block → IR [`Usage`]. `cached_input_tokens` maps to cache-read; Codex has no
/// separate cache-creation counter.
fn parse_usage(v: &Value) -> Option<Usage> {
    let u64f = |k: &str| v.get(k).and_then(Value::as_u64);
    let u = Usage {
        input_tokens: u64f("input_tokens"),
        output_tokens: u64f("output_tokens"),
        cache_read_tokens: u64f("cached_input_tokens"),
        cache_creation_tokens: None,
    };
    if u.input_tokens.is_none() && u.output_tokens.is_none() && u.cache_read_tokens.is_none() {
        return None;
    }
    Some(u)
}

/// A top-level `compacted` record marks an auto/manual history-compaction boundary. The `message`
/// (a human summary) is preserved as a system note; `replacement_history` is summarized in `extra`.
fn handle_compacted(payload: Option<&Value>, ts: Option<DateTime<Utc>>, out: &mut Vec<Message>) {
    let Some(p) = payload else { return };
    let summary = p.get("message").and_then(Value::as_str).unwrap_or("");
    let mut m = Message::new(Role::System);
    m.timestamp = ts;
    let text = if summary.trim().is_empty() {
        "[history compacted]".to_string()
    } else {
        format!("[history compacted]\n{summary}")
    };
    m.content.push(Block::Text { text });
    m.extra
        .insert("codex_event".into(), Value::String("compacted".into()));
    if let Some(rh) = p.get("replacement_history").and_then(Value::as_array) {
        m.extra.insert(
            "replacement_history_len".into(),
            Value::Number(rh.len().into()),
        );
    }
    out.push(m);
}

/// Handle a `response_item` payload or a legacy `items[]` entry.
fn handle_item(
    payload: Option<&Value>,
    has_events: bool,
    ts: Option<DateTime<Utc>>,
    out: &mut Vec<Message>,
) {
    let Some(p) = payload else { return };
    let ty = p.get("type").and_then(Value::as_str).unwrap_or("");
    let str_field = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("").to_string();

    match ty {
        "message" => {
            let role = p.get("role").and_then(Value::as_str).unwrap_or("user");
            // Natural-language user/assistant text is taken from event_msg when present.
            if has_events && (role == "user" || role == "assistant") {
                return;
            }
            let ir_role = match role {
                "assistant" => Role::Assistant,
                "developer" | "system" => Role::System,
                _ => Role::User,
            };
            // A message can mix text and images; emit one message carrying all blocks in order.
            let blocks = content_blocks(p.get("content"));
            if !blocks.is_empty() {
                let mut m = Message::new(ir_role);
                m.timestamp = ts;
                if let Some(phase) = p.get("phase").and_then(Value::as_str) {
                    m.extra.insert("phase".into(), Value::String(phase.into()));
                }
                m.content = blocks;
                out.push(m);
            }
        }
        "reasoning" => {
            // `summary` holds the user-visible reasoning summary; `content` (when present) holds the
            // raw chain-of-thought. Prefer raw content, fall back to summary. `encrypted_content`
            // carries the opaque replay blob.
            let summary = join_text(p.get("summary"));
            let raw = join_text(p.get("content"));
            let text = if !raw.is_empty() { raw } else { summary.clone() };
            let encrypted = p
                .get("encrypted_content")
                .and_then(Value::as_str)
                .map(str::to_string);
            if !text.is_empty() || encrypted.is_some() {
                let mut m = Message::new(Role::Assistant);
                m.timestamp = ts;
                // When we surfaced raw content but a distinct summary also exists, keep the summary.
                if !summary.is_empty() && text != summary {
                    m.extra.insert("reasoning_summary".into(), Value::String(summary));
                }
                m.content.push(Block::Thinking {
                    text,
                    signature: None,
                    encrypted,
                    redacted: false,
                });
                out.push(m);
            }
        }
        "function_call" | "custom_tool_call" | "local_shell_call" => {
            let call_id = p
                .get("call_id")
                .and_then(Value::as_str)
                // local_shell_call may carry only a legacy `id`.
                .or_else(|| p.get("id").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            let (name, input) = if ty == "local_shell_call" {
                // {status, action:{type:"exec", command:[...], ...}}
                let mut obj = Map::new();
                if let Some(action) = p.get("action") {
                    obj.insert("action".into(), action.clone());
                }
                if let Some(status) = p.get("status") {
                    obj.insert("status".into(), status.clone());
                }
                ("local_shell".to_string(), Value::Object(obj))
            } else {
                let name = str_field("name");
                // Tool `arguments`/`input` arrive as JSON-encoded *strings*. Only treat the decode as
                // structured when it yields an object/array; a bare scalar (e.g. the literal arg `"42"`
                // or `"true"`) must stay a string, or we'd silently retype the call's payload.
                let as_structured = |s: &str| -> Value {
                    match serde_json::from_str::<Value>(s) {
                        Ok(v @ Value::Object(_)) | Ok(v @ Value::Array(_)) => v,
                        _ => Value::String(s.to_string()),
                    }
                };
                let input = if let Some(args) = p.get("arguments").and_then(Value::as_str) {
                    as_structured(args)
                } else if let Some(input) = p.get("input").and_then(Value::as_str) {
                    // custom_tool_call `input` is a freeform string.
                    as_structured(input)
                } else {
                    p.get("input").cloned().unwrap_or(Value::Null)
                };
                (name, input)
            };
            let mut m = Message::new(Role::Assistant);
            m.timestamp = ts;
            if let Some(status) = p.get("status").and_then(Value::as_str) {
                m.extra.insert("status".into(), Value::String(status.into()));
            }
            m.content.push(Block::ToolUse {
                id: call_id,
                name,
                input,
            });
            out.push(m);
        }
        "function_call_output" | "custom_tool_call_output" => {
            let call_id = str_field("call_id");
            let (content, images) = coerce_output(p.get("output"));
            let is_error = output_is_error(p.get("output"));
            let mut m = Message::new(Role::Tool);
            m.timestamp = ts;
            m.content.push(Block::ToolResult {
                tool_use_id: call_id,
                content,
                is_error,
                tool_name: None,
                status: Some(if is_error { "error" } else { "completed" }.into()),
                details: None,
            });
            // Structured outputs can return image content items alongside text.
            m.content.extend(images);
            out.push(m);
        }
        "tool_search_call" => {
            let call_id = str_field("call_id");
            let mut input = Map::new();
            if let Some(args) = p.get("arguments") {
                input.insert("arguments".into(), args.clone());
            }
            if let Some(exec) = p.get("execution") {
                input.insert("execution".into(), exec.clone());
            }
            let mut m = Message::new(Role::Assistant);
            m.timestamp = ts;
            m.content.push(Block::ToolUse {
                id: call_id,
                name: "tool_search".into(),
                input: Value::Object(input),
            });
            out.push(m);
        }
        "tool_search_output" => {
            let call_id = str_field("call_id");
            let content = p
                .get("tools")
                .map(|t| t.to_string())
                .unwrap_or_else(|| "[]".into());
            let mut m = Message::new(Role::Tool);
            m.timestamp = ts;
            m.content.push(Block::ToolResult {
                tool_use_id: call_id,
                content,
                is_error: false,
                tool_name: Some("tool_search".into()),
                status: None,
                details: None,
            });
            out.push(m);
        }
        "web_search_call" => {
            // Model-side web search; surface the query/queries as a tool call.
            let call_id = p
                .get("call_id")
                .and_then(Value::as_str)
                .or_else(|| p.get("id").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            let input = p.get("action").cloned().unwrap_or(Value::Null);
            let mut m = Message::new(Role::Assistant);
            m.timestamp = ts;
            if let Some(status) = p.get("status").and_then(Value::as_str) {
                m.extra.insert("status".into(), Value::String(status.into()));
            }
            m.content.push(Block::ToolUse {
                id: call_id,
                name: "web_search".into(),
                input,
            });
            out.push(m);
        }
        "image_generation_call" => {
            let mut m = Message::new(Role::Assistant);
            m.timestamp = ts;
            if let Some(rp) = p.get("revised_prompt").and_then(Value::as_str) {
                m.extra
                    .insert("revised_prompt".into(), Value::String(rp.into()));
            }
            // `result` is base64 image data; record a reference, not the bytes.
            m.content.push(Block::Image {
                media_type: None,
                data_ref: p
                    .get("result")
                    .and_then(Value::as_str)
                    .map(|_| "base64:inline".to_string()),
            });
            out.push(m);
        }
        // Mid-history compaction recorded inline as a response_item (vs. the top-level `compacted`
        // record). Only an opaque encrypted blob survives; note the boundary.
        "compaction" | "compaction_summary" | "context_compaction" => {
            let mut m = Message::new(Role::System);
            m.timestamp = ts;
            m.content.push(Block::Text {
                text: "[history compacted]".into(),
            });
            m.extra
                .insert("codex_event".into(), Value::String(ty.into()));
            if p.get("encrypted_content").and_then(Value::as_str).is_some() {
                m.extra.insert("encrypted".into(), Value::Bool(true));
            }
            out.push(m);
        }
        _ => {}
    }
}

/// Build IR content blocks from a `ContentItem[]` (or a bare string), preserving text + images.
fn content_blocks(v: Option<&Value>) -> Vec<Block> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => vec![Block::Text { text: s.clone() }],
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for it in items {
                match it.get("type").and_then(Value::as_str) {
                    Some("input_image") | Some("output_image") => {
                        out.push(image_block(it));
                    }
                    // input_text / output_text / text / summary_text / reasoning_text
                    _ => {
                        if let Some(t) = it.get("text").and_then(Value::as_str) {
                            if !t.is_empty() {
                                out.push(Block::Text { text: t.into() });
                            }
                        }
                    }
                }
            }
            out
        }
        _ => Vec::new(),
    }
}

/// An `input_image`/`output_image` content item → [`Block::Image`]. We never inline the bytes: a
/// `data:` URL becomes the marker `base64:inline`; an `http(s)`/`file:` URL is kept as a reference.
fn image_block(it: &Value) -> Block {
    let url = it
        .get("image_url")
        .and_then(Value::as_str)
        .or_else(|| it.get("url").and_then(Value::as_str));
    let media_type = url.and_then(|u| {
        u.strip_prefix("data:")
            .and_then(|rest| rest.split(';').next())
            .filter(|m| m.contains('/'))
            .map(str::to_string)
    });
    let data_ref = url.map(|u| {
        if u.starts_with("data:") {
            "base64:inline".to_string()
        } else {
            u.to_string()
        }
    });
    Block::Image {
        media_type,
        data_ref,
    }
}

/// Join the `text` fields of a `[{... text}]` array (used for reasoning summary/content).
fn join_text(v: Option<&Value>) -> String {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// `content` is `[{type:input_text|output_text|text, text}]` or a plain string.
fn coerce_content(v: Option<&Value>) -> String {
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

/// Coerce a tool-call `output` into text plus any image blocks. On the wire `output` is one of:
/// a plain string; an object like `{output, metadata}` (legacy) or `{success, content}`; or an
/// array of structured content items (`input_text`/`input_image`/`encrypted_content`).
fn coerce_output(v: Option<&Value>) -> (String, Vec<Block>) {
    match v {
        Some(Value::String(s)) => (s.clone(), Vec::new()),
        Some(Value::Array(items)) => {
            let mut text = Vec::new();
            let mut images = Vec::new();
            for it in items {
                match it.get("type").and_then(Value::as_str) {
                    Some("input_image") | Some("output_image") => images.push(image_block(it)),
                    Some("encrypted_content") => {}
                    _ => {
                        if let Some(t) = it.get("text").and_then(Value::as_str) {
                            text.push(t.to_string());
                        }
                    }
                }
            }
            (text.join("\n"), images)
        }
        Some(o @ Value::Object(_)) => {
            // `{output: ...}` may itself nest a string or an array of content items.
            let inner = o.get("output");
            match inner {
                Some(Value::String(s)) => (s.clone(), Vec::new()),
                Some(arr @ Value::Array(_)) => coerce_output(Some(arr)),
                _ => {
                    // `{content: "...", success: bool}` or unknown — best-effort string.
                    let s = o
                        .get("content")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| o.to_string());
                    (s, Vec::new())
                }
            }
        }
        Some(other) => (other.to_string(), Vec::new()),
        None => (String::new(), Vec::new()),
    }
}

/// Detect a failed tool result from the `success`/`exit_code`/`metadata` markers Codex emits.
fn output_is_error(v: Option<&Value>) -> bool {
    let Some(Value::Object(o)) = v else {
        return false;
    };
    if o.get("success").and_then(Value::as_bool) == Some(false) {
        return true;
    }
    matches!(
        o.get("metadata")
            .and_then(|m| m.get("exit_code"))
            .and_then(Value::as_i64),
        Some(c) if c != 0
    )
}

fn top_ts(v: &Value) -> Option<DateTime<Utc>> {
    v.get("timestamp").and_then(Value::as_str).and_then(parse_ts)
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Accumulates the cheap metadata `discover` needs, one record at a time. Factored out of [`scan`]
/// so it can be fed either the whole file (small sessions) or just a head+tail sample (huge ones).
#[derive(Default)]
struct CodexScan {
    id: String,
    cwd: Option<PathBuf>,
    title: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    message_count: usize,
}

impl CodexScan {
    fn consider_title(&mut self, t: &str) {
        let trimmed = t.trim();
        // Skip Codex's injected preambles — they aren't the user's first words.
        let is_injected = trimmed.starts_with("<environment_context")
            || trimmed.starts_with("<user_instructions")
            || trimmed.starts_with("# Codex CLI");
        if self.title.is_none() && !trimmed.is_empty() && !is_injected {
            self.title = Some(crate::ir::truncate(trimmed, 80));
        }
    }

    /// Ingest one `.jsonl` record line.
    fn feed(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        if let Some(ts) = top_ts(&v) {
            self.created_at.get_or_insert(ts);
            self.updated_at = Some(ts);
        }
        match v.get("type").and_then(Value::as_str) {
            None => {
                if self.id.is_empty() {
                    if let Some(i) = v.get("id").and_then(Value::as_str) {
                        self.id = i.to_string();
                    }
                }
            }
            Some("session_meta") => {
                let p = v.get("payload");
                if self.id.is_empty() {
                    if let Some(i) = p.and_then(|p| p.get("id")).and_then(Value::as_str) {
                        self.id = i.to_string();
                    }
                }
                if self.cwd.is_none() {
                    self.cwd = p
                        .and_then(|p| p.get("cwd"))
                        .and_then(Value::as_str)
                        .map(PathBuf::from);
                }
            }
            Some("event_msg") => {
                let pt = v.pointer("/payload/type").and_then(Value::as_str);
                if pt == Some("user_message") {
                    self.message_count += 1;
                    if let Some(t) = v.pointer("/payload/message").and_then(Value::as_str) {
                        self.consider_title(t);
                    }
                } else if pt == Some("agent_message") {
                    self.message_count += 1;
                }
            }
            Some("response_item") => {
                if v.pointer("/payload/type").and_then(Value::as_str) == Some("message") {
                    self.message_count += 1;
                    if v.pointer("/payload/role").and_then(Value::as_str) == Some("user") {
                        self.consider_title(&coerce_content(v.pointer("/payload/content")));
                    }
                }
            }
            _ => {}
        }
    }

    /// Ingest a legacy single-object `.json` recording.
    fn feed_json_object(&mut self, root: &Value) {
        self.id = root
            .pointer("/session/id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        self.created_at = root
            .pointer("/session/timestamp")
            .and_then(Value::as_str)
            .and_then(parse_ts);
        self.updated_at = self.created_at;
        if let Some(items) = root.get("items").and_then(Value::as_array) {
            for it in items {
                if it.get("type").and_then(Value::as_str) == Some("message") {
                    self.message_count += 1;
                    if it.get("role").and_then(Value::as_str) == Some("user") {
                        self.consider_title(&coerce_content(it.get("content")));
                    }
                }
            }
        }
    }
}

/// Read at most `n` bytes from the start of `path`, trimmed back to the last complete line.
fn read_head(path: &Path, n: usize) -> Result<String> {
    use std::io::Read;
    let f = fs::File::open(path)?;
    let mut buf = Vec::with_capacity(n.min(1 << 20));
    f.take(n as u64).read_to_end(&mut buf)?;
    if let Some(pos) = buf.iter().rposition(|&b| b == b'\n') {
        buf.truncate(pos);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Read the last `n` bytes of `path` (the first line is likely partial — callers should skip it).
fn read_tail(path: &Path, n: usize) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = fs::File::open(path)?;
    let len = f.metadata()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(n as u64)))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Cheap metadata scan for `discover`. Files up to [`FULL_SCAN_CAP`] are parsed in full; larger ones
/// (Codex rollout logs can be hundreds of MB) are sampled head+tail so discovery never reads — and
/// JSON-parses — gigabytes just to list sessions. Exact content is parsed lazily on actual open.
fn scan(path: &Path) -> Result<SessionRef> {
    const FULL_SCAN_CAP: u64 = 8 << 20; // 8 MiB
    const SAMPLE: usize = 1 << 20; // 1 MiB head, 1 MiB tail

    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let is_jsonl = path.extension().and_then(|e| e.to_str()) == Some("jsonl");
    let mut s = CodexScan::default();

    if is_jsonl {
        if size <= FULL_SCAN_CAP {
            let text = fs::read_to_string(path)?;
            for line in text.lines() {
                s.feed(line);
            }
        } else {
            // Head: id / cwd / title / created_at all live near the top.
            let head = read_head(path, SAMPLE)?;
            let head_len = head.len().max(1) as u128;
            for line in head.lines() {
                s.feed(line);
            }
            let head_msgs = s.message_count;
            // Tail: the last record carries the real updated_at. Skip the (likely partial) first line.
            let tail = read_tail(path, SAMPLE)?;
            for line in tail.lines().skip(1) {
                s.feed(line);
            }
            // We never read the middle, so the count is a head-density estimate — kept roughly
            // monotonic with file growth. The true count comes from a full parse on open.
            let est = (head_msgs as u128 * size as u128 / head_len) as usize;
            s.message_count = est.max(s.message_count);
        }
    } else {
        // Legacy single-object `.json` recordings are small; read fully.
        let text = fs::read_to_string(path)?;
        if let Ok(root) = serde_json::from_str::<Value>(&text) {
            s.feed_json_object(&root);
        }
    }

    if s.id.is_empty() {
        s.id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
    }

    Ok(SessionRef {
        id: s.id,
        harness: Harness::Codex,
        path: path.to_path_buf(),
        cwd: s.cwd,
        title: s.title,
        created_at: s.created_at,
        updated_at: s.updated_at,
        message_count: s.message_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `.jsonl` transcript from individual record lines and parse it.
    fn parse_jsonl(lines: &[&str]) -> Session {
        let text = lines.join("\n");
        parse_str("fallback-id", &text, true, None)
    }

    fn first_block<'a>(s: &'a Session, pred: impl Fn(&&'a Block) -> bool) -> &'a Block {
        s.messages
            .iter()
            .flat_map(|m| &m.content)
            .find(pred)
            .expect("matching block")
    }

    #[test]
    fn session_meta_seeds_id_cwd_git() {
        let s = parse_jsonl(&[
            r#"{"timestamp":"2026-02-07T14:07:45Z","type":"session_meta","payload":{"id":"abc-123","timestamp":"2026-02-07T14:07:44Z","cwd":"/work","originator":"codex-cli","cli_version":"0.99.0","git":{"branch":"main","commit_hash":"deadbeef","repository_url":"https://x/y"}}}"#,
        ]);
        assert_eq!(s.id, "abc-123");
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/work")));
        let g = s.git.unwrap();
        assert_eq!(g.branch.as_deref(), Some("main"));
        assert_eq!(g.commit.as_deref(), Some("deadbeef"));
        assert_eq!(g.remote.as_deref(), Some("https://x/y"));
    }

    #[test]
    fn bare_header_provides_id() {
        // No session_meta: first line is a bare {id,timestamp} header.
        let s = parse_str(
            "",
            "{\"id\":\"hdr-1\",\"timestamp\":\"2025-09-01T00:00:00Z\"}\n{\"timestamp\":\"2025-09-01T00:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"hi\"}]}}",
            true,
            None,
        );
        assert_eq!(s.id, "hdr-1");
        assert_eq!(s.messages.len(), 1);
    }

    #[test]
    fn event_msg_dedups_response_item_text() {
        // When event_msgs are present, NL text comes from them and the response_item message dup is
        // skipped — so we expect exactly one user + one assistant text message.
        let s = parse_jsonl(&[
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"user_message","message":"hello"}}"#,
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}"#,
            r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"agent_message","message":"hi there"}}"#,
            r#"{"timestamp":"2026-01-01T00:00:03Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi there"}]}}"#,
        ]);
        let texts: Vec<_> = s.messages.iter().filter_map(|m| m.text()).collect();
        assert_eq!(texts, vec!["hello", "hi there"]);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[1].role, Role::Assistant);
    }

    #[test]
    fn reasoning_prefers_content_keeps_summary_and_encrypted() {
        let s = parse_jsonl(&[
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"short summary"}],"content":[{"type":"reasoning_text","text":"raw thoughts"}],"encrypted_content":"ENC"}}"#,
        ]);
        let m = &s.messages[0];
        match &m.content[0] {
            Block::Thinking { text, encrypted, .. } => {
                assert_eq!(text, "raw thoughts");
                assert_eq!(encrypted.as_deref(), Some("ENC"));
            }
            other => panic!("expected thinking, got {other:?}"),
        }
        assert_eq!(
            m.extra.get("reasoning_summary").and_then(Value::as_str),
            Some("short summary")
        );
    }

    #[test]
    fn function_call_and_output_roundtrip_with_error() {
        let s = parse_jsonl(&[
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"ls\"]}","call_id":"c1"}}"#,
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":{"output":"boom","metadata":{"exit_code":1}}}}"#,
        ]);
        match first_block(&s, |b| matches!(b, Block::ToolUse { .. })) {
            Block::ToolUse { id, name, input } => {
                assert_eq!(id, "c1");
                assert_eq!(name, "shell");
                assert_eq!(input["command"][0], "ls");
            }
            _ => unreachable!(),
        }
        match first_block(&s, |b| matches!(b, Block::ToolResult { .. })) {
            Block::ToolResult { tool_use_id, content, is_error, .. } => {
                assert_eq!(tool_use_id, "c1");
                assert_eq!(content, "boom");
                assert!(is_error);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn custom_tool_call_string_input() {
        let s = parse_jsonl(&[
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"response_item","payload":{"type":"custom_tool_call","call_id":"c2","name":"apply_patch","input":"*** Begin Patch","status":"completed"}}"#,
        ]);
        match &s.messages[0].content[0] {
            Block::ToolUse { name, input, .. } => {
                assert_eq!(name, "apply_patch");
                assert_eq!(input, &Value::String("*** Begin Patch".into()));
            }
            _ => unreachable!(),
        }
        assert_eq!(s.messages[0].extra.get("status").and_then(Value::as_str), Some("completed"));
    }

    #[test]
    fn local_shell_call_maps_to_tool_use() {
        let s = parse_jsonl(&[
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"response_item","payload":{"type":"local_shell_call","id":"ls1","status":"completed","action":{"type":"exec","command":["echo","hi"],"timeout_ms":1000}}}"#,
        ]);
        match &s.messages[0].content[0] {
            Block::ToolUse { id, name, input } => {
                assert_eq!(id, "ls1");
                assert_eq!(name, "local_shell");
                assert_eq!(input["action"]["command"][1], "hi");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn web_search_call_surfaces_query() {
        let s = parse_jsonl(&[
            r#"{"timestamp":"2026-04-28T00:00:00Z","type":"response_item","payload":{"type":"web_search_call","status":"completed","action":{"type":"search","query":"rust serde"}}}"#,
        ]);
        match &s.messages[0].content[0] {
            Block::ToolUse { name, input, .. } => {
                assert_eq!(name, "web_search");
                assert_eq!(input["query"], "rust serde");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn tool_search_call_and_output() {
        let s = parse_jsonl(&[
            r#"{"timestamp":"2026-04-28T00:00:00Z","type":"response_item","payload":{"type":"tool_search_call","call_id":"t1","status":"completed","execution":"client","arguments":{"query":"repl","limit":5}}}"#,
            r#"{"timestamp":"2026-04-28T00:00:01Z","type":"response_item","payload":{"type":"tool_search_output","call_id":"t1","status":"completed","execution":"client","tools":[]}}"#,
        ]);
        assert!(matches!(&s.messages[0].content[0], Block::ToolUse { name, .. } if name == "tool_search"));
        assert!(matches!(&s.messages[1].content[0], Block::ToolResult { tool_use_id, content, .. } if tool_use_id == "t1" && content == "[]"));
    }

    #[test]
    fn user_message_with_input_image() {
        let s = parse_jsonl(&[
            r#"{"timestamp":"2026-01-27T00:46:27Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"see this"},{"type":"input_image","image_url":"data:image/png;base64,iVBORabc"}]}}"#,
        ]);
        let m = &s.messages[0];
        assert!(matches!(&m.content[0], Block::Text { text } if text == "see this"));
        match &m.content[1] {
            Block::Image { media_type, data_ref } => {
                assert_eq!(media_type.as_deref(), Some("image/png"));
                assert_eq!(data_ref.as_deref(), Some("base64:inline"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn view_image_event_becomes_image_block() {
        let s = parse_jsonl(&[
            r#"{"timestamp":"2026-05-06T01:38:29Z","type":"event_msg","payload":{"type":"view_image_tool_call","call_id":"v1","path":"/tmp/x.png"}}"#,
        ]);
        match &s.messages[0].content[0] {
            Block::Image { data_ref, .. } => assert_eq!(data_ref.as_deref(), Some("/tmp/x.png")),
            _ => unreachable!(),
        }
    }

    #[test]
    fn token_count_attaches_usage_to_assistant() {
        let s = parse_jsonl(&[
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"event_msg","payload":{"type":"agent_message","message":"done"}}"#,
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":10,"output_tokens":20,"total_tokens":120},"model_context_window":258400},"rate_limits":{"primary":{"used_percent":7.0}}}}"#,
        ]);
        let m = s.messages.iter().find(|m| m.role == Role::Assistant).unwrap();
        let u = m.usage.as_ref().expect("usage attached");
        assert_eq!(u.input_tokens, Some(100));
        assert_eq!(u.output_tokens, Some(20));
        assert_eq!(u.cache_read_tokens, Some(10));
        assert!(m.extra.contains_key("rate_limits"));
        assert!(m.extra.contains_key("model_context_window"));
    }

    #[test]
    fn compacted_record_becomes_system_note() {
        let s = parse_jsonl(&[
            r#"{"timestamp":"2026-01-27T00:52:58Z","type":"compacted","payload":{"message":"summary text","replacement_history":[{"type":"message","role":"user","content":[]}]}}"#,
        ]);
        let m = &s.messages[0];
        assert_eq!(m.role, Role::System);
        assert!(m.text().unwrap().contains("history compacted"));
        assert!(m.text().unwrap().contains("summary text"));
        assert_eq!(m.extra.get("replacement_history_len").and_then(Value::as_u64), Some(1));
    }

    #[test]
    fn turn_context_records_model_change() {
        let s = parse_jsonl(&[
            r#"{"timestamp":"2026-02-07T14:07:47Z","type":"turn_context","payload":{"cwd":"/work","model":"gpt-5.3-codex","approval_policy":"never","sandbox_policy":{"type":"danger-full-access"}}}"#,
            r#"{"timestamp":"2026-02-07T14:10:00Z","type":"turn_context","payload":{"cwd":"/work","model":"gpt-5.3-codex-high","approval_policy":"never","sandbox_policy":{"type":"danger-full-access"}}}"#,
        ]);
        assert_eq!(s.model.as_deref(), Some("gpt-5.3-codex-high"));
        let note = s.messages.iter().find(|m| m.role == Role::System).unwrap();
        assert!(note.text().unwrap().contains("model changed"));
        assert_eq!(note.extra.get("model").and_then(Value::as_str), Some("gpt-5.3-codex-high"));
    }

    #[test]
    fn legacy_2025_json_layout() {
        let text = r#"{"session":{"timestamp":"2025-05-05T19:24:54Z","id":"legacy-1","instructions":""},"items":[{"role":"user","content":[{"type":"input_text","text":"do the thing"}],"type":"message"},{"type":"reasoning","summary":[{"type":"summary_text","text":"thinking"}]},{"type":"function_call","name":"shell","arguments":"{}","call_id":"x"},{"type":"function_call_output","call_id":"x","output":"ok"}]}"#;
        let s = parse_str("fallback", text, false, None);
        assert_eq!(s.id, "legacy-1");
        // user text, reasoning, tool use, tool result => 4 messages (no event_msg dedup in legacy).
        assert_eq!(s.messages.len(), 4);
        assert!(s.messages.iter().any(|m| matches!(m.content.first(), Some(Block::Thinking { .. }))));
        assert!(s.messages.iter().any(|m| matches!(m.content.first(), Some(Block::ToolResult { content, .. }) if content == "ok")));
    }

    /// Developer smoke test against the real on-disk corpus (run with `--ignored`). Parses every
    /// discovered Codex session and asserts no panics + that every tool result references a tool use.
    #[test]
    #[ignore = "requires local ~/.codex corpus"]
    fn real_corpus_parses_without_panic() {
        let cx = Codex::new();
        let refs = cx.discover().expect("discover");
        eprintln!("codex corpus: {} sessions", refs.len());
        let mut tool_uses = 0usize;
        let mut tool_results = 0usize;
        let mut images = 0usize;
        let mut thinking = 0usize;
        for r in &refs {
            let s = cx.parse(r).expect("parse");
            assert!(!s.id.is_empty());
            for m in &s.messages {
                for b in &m.content {
                    match b {
                        Block::ToolUse { .. } => tool_uses += 1,
                        Block::ToolResult { .. } => tool_results += 1,
                        Block::Image { .. } => images += 1,
                        Block::Thinking { .. } => thinking += 1,
                        _ => {}
                    }
                }
            }
        }
        eprintln!(
            "tool_uses={tool_uses} tool_results={tool_results} images={images} thinking={thinking}"
        );
        assert!(tool_uses > 0, "expected some tool calls in the corpus");
    }

    #[test]
    fn structured_output_with_image() {
        let s = parse_jsonl(&[
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":[{"type":"input_text","text":"here"},{"type":"input_image","image_url":"data:image/jpeg;base64,zzz"}]}}"#,
        ]);
        let m = &s.messages[0];
        assert!(matches!(&m.content[0], Block::ToolResult { content, .. } if content == "here"));
        assert!(matches!(&m.content[1], Block::Image { media_type, .. } if media_type.as_deref() == Some("image/jpeg")));
    }
}
