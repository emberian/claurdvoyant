//! Codex CLI adapter — `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (+ archived, + 2025 legacy JSON).
//!
//! Format drift handled: first line is either a `session_meta` record or a bare `{id,timestamp}`
//! header; natural-language text appears both as `event_msg` and as `response_item message` — when
//! `event_msg`s are present we take NL text from them and skip the `response_item` duplicates.

use super::Adapter;
use crate::ir::*;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
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
        let mut out = Vec::new();
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
                match scan(path) {
                    Ok(r) => out.push(r),
                    Err(e) => eprintln!("cv: skipping {}: {e:#}", path.display()),
                }
            }
        }
        Ok(out)
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
    let mut s = Session {
        id: id.to_string(),
        harness: Harness::Codex,
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        model: None,
        git: None,
        messages: Vec::new(),
        source_path,
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
                    if let Some(p) = v.get("payload") {
                        if s.cwd.is_none() {
                            s.cwd = p.get("cwd").and_then(Value::as_str).map(PathBuf::from);
                        }
                        if s.model.is_none() {
                            s.model = p.get("model").and_then(Value::as_str).map(str::to_string);
                        }
                    }
                }
                Some("event_msg") if has_events => {
                    if let Some(m) = event_message(v.get("payload"), top_ts(v)) {
                        s.messages.push(m);
                    }
                }
                Some("response_item") => {
                    handle_item(v.get("payload"), has_events, top_ts(v), &mut s.messages);
                }
                _ => {} // compacted, token_count, etc.
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

fn event_message(payload: Option<&Value>, ts: Option<DateTime<Utc>>) -> Option<Message> {
    let p = payload?;
    let (role, text) = match p.get("type").and_then(Value::as_str)? {
        "user_message" => (Role::User, p.get("message")?.as_str()?.to_string()),
        "agent_message" => (Role::Assistant, p.get("message")?.as_str()?.to_string()),
        _ => return None,
    };
    let mut m = Message::new(role);
    m.timestamp = ts;
    m.content.push(Block::Text { text });
    Some(m)
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
    let push = |out: &mut Vec<Message>, role: Role, block: Block| {
        let mut m = Message::new(role);
        m.timestamp = ts;
        m.content.push(block);
        out.push(m);
    };

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
            let text = coerce_content(p.get("content"));
            if !text.is_empty() {
                push(out, ir_role, Block::Text { text });
            }
        }
        "reasoning" => {
            let text = p
                .get("summary")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            let encrypted = p
                .get("encrypted_content")
                .and_then(Value::as_str)
                .map(str::to_string);
            if !text.is_empty() || encrypted.is_some() {
                push(
                    out,
                    Role::Assistant,
                    Block::Thinking {
                        text,
                        signature: None,
                        encrypted,
                    },
                );
            }
        }
        "function_call" | "custom_tool_call" => {
            let call_id = p
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let name = p
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let input = if let Some(args) = p.get("arguments").and_then(Value::as_str) {
                serde_json::from_str(args).unwrap_or(Value::String(args.to_string()))
            } else {
                p.get("input").cloned().unwrap_or(Value::Null)
            };
            push(
                out,
                Role::Assistant,
                Block::ToolUse {
                    id: call_id,
                    name,
                    input,
                },
            );
        }
        "function_call_output" | "custom_tool_call_output" => {
            let call_id = p
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            push(
                out,
                Role::Tool,
                Block::ToolResult {
                    tool_use_id: call_id,
                    content: coerce_output(p.get("output")),
                    is_error: false,
                },
            );
        }
        _ => {}
    }
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

/// `output` is a string, or an object like `{output, metadata}`.
fn coerce_output(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(o @ Value::Object(_)) => o
            .get("output")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| o.to_string()),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn top_ts(v: &Value) -> Option<DateTime<Utc>> {
    v.get("timestamp").and_then(Value::as_str).and_then(parse_ts)
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Cheap metadata scan for `discover`.
fn scan(path: &Path) -> Result<SessionRef> {
    let text = fs::read_to_string(path)?;
    let mut id = String::new();
    let mut cwd = None;
    let mut title: Option<String> = None;
    let mut created_at: Option<DateTime<Utc>> = None;
    let mut updated_at: Option<DateTime<Utc>> = None;
    let mut message_count = 0usize;

    let mut consider_title = |t: String| {
        let trimmed = t.trim();
        // Skip Codex's injected preambles — they aren't the user's first words.
        let is_injected = trimmed.starts_with("<environment_context")
            || trimmed.starts_with("<user_instructions")
            || trimmed.starts_with("# Codex CLI");
        if title.is_none() && !trimmed.is_empty() && !is_injected {
            title = Some(crate::ir::truncate(trimmed, 80));
        }
    };

    if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if let Some(ts) = top_ts(&v) {
                created_at.get_or_insert(ts);
                updated_at = Some(ts);
            }
            match v.get("type").and_then(Value::as_str) {
                None => {
                    if id.is_empty() {
                        if let Some(i) = v.get("id").and_then(Value::as_str) {
                            id = i.to_string();
                        }
                    }
                }
                Some("session_meta") => {
                    let p = v.get("payload");
                    if id.is_empty() {
                        if let Some(i) = p.and_then(|p| p.get("id")).and_then(Value::as_str) {
                            id = i.to_string();
                        }
                    }
                    if cwd.is_none() {
                        cwd = p
                            .and_then(|p| p.get("cwd"))
                            .and_then(Value::as_str)
                            .map(PathBuf::from);
                    }
                }
                Some("event_msg") => {
                    if v.pointer("/payload/type").and_then(Value::as_str) == Some("user_message") {
                        message_count += 1;
                        if let Some(t) = v.pointer("/payload/message").and_then(Value::as_str) {
                            consider_title(t.to_string());
                        }
                    } else if v.pointer("/payload/type").and_then(Value::as_str)
                        == Some("agent_message")
                    {
                        message_count += 1;
                    }
                }
                Some("response_item") => {
                    if v.pointer("/payload/type").and_then(Value::as_str) == Some("message") {
                        message_count += 1;
                        if v.pointer("/payload/role").and_then(Value::as_str) == Some("user") {
                            consider_title(coerce_content(v.pointer("/payload/content")));
                        }
                    }
                }
                _ => {}
            }
        }
    } else if let Ok(root) = serde_json::from_str::<Value>(&text) {
        id = root
            .pointer("/session/id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        created_at = root
            .pointer("/session/timestamp")
            .and_then(Value::as_str)
            .and_then(parse_ts);
        updated_at = created_at;
        if let Some(items) = root.get("items").and_then(Value::as_array) {
            for it in items {
                if it.get("type").and_then(Value::as_str) == Some("message") {
                    message_count += 1;
                    if it.get("role").and_then(Value::as_str) == Some("user") {
                        consider_title(coerce_content(it.get("content")));
                    }
                }
            }
        }
    }

    if id.is_empty() {
        id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
    }

    Ok(SessionRef {
        id,
        harness: Harness::Codex,
        path: path.to_path_buf(),
        cwd,
        title,
        created_at,
        updated_at,
        message_count,
    })
}
