//! OpenClaw adapter — `~/.openclaw/agents/<agentId>/sessions/`.
//!
//! See `docs/FORMATS.md`. A `sessions.json` index maps session keys → metadata; each session is a
//! `<sessionId>.jsonl` (or `<sessionId>-topic-<topicId>.jsonl`) transcript: a `session` header line
//! then `message` lines `{id, parentId, timestamp, message:{role,content,...}}`. Roles are
//! user / assistant / toolResult; content blocks are text / thinking / toolCall / image.

use super::Adapter;
use crate::ir::*;
use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
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
                if path.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str())
                    != Some("sessions")
                {
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
        let text = fs::read_to_string(&r.path)
            .with_context(|| format!("reading {}", r.path.display()))?;
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
        };

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            match v.get("type").and_then(Value::as_str) {
                Some("session") => {
                    if s.cwd.is_none() {
                        s.cwd = v.get("cwd").and_then(Value::as_str).map(PathBuf::from);
                    }
                    if s.created_at.is_none() {
                        s.created_at =
                            v.get("timestamp").and_then(Value::as_str).and_then(parse_ts);
                    }
                }
                Some("message") => {
                    if let Some(m) = parse_entry(&v) {
                        if s.model.is_none() {
                            s.model = m.model.clone();
                        }
                        s.messages.push(m);
                    }
                }
                _ => {}
            }
        }
        Ok(s)
    }

    fn can_emit(&self) -> bool {
        false
    }
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

    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
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
                if let Some(ts) = v.get("timestamp").and_then(Value::as_str).and_then(parse_ts) {
                    updated_at = Some(ts);
                }
                if first_user.is_none()
                    && v.pointer("/message/role").and_then(Value::as_str) == Some("user")
                {
                    let t = coerce_content_text(v.pointer("/message/content"));
                    if !t.trim().is_empty() {
                        first_user = Some(crate::ir::truncate(&t, 80));
                    }
                }
            }
            _ => {}
        }
    }

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
    let entry_updated = entry
        .and_then(|e| e.get("updatedAt"))
        .and_then(Value::as_i64)
        .and_then(ms_to_dt);

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

fn parse_entry(v: &Value) -> Option<Message> {
    let msg = v.get("message")?;
    let role_str = msg.get("role").and_then(Value::as_str)?;
    let role = match role_str {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "toolResult" => Role::Tool,
        "system" => Role::System,
        _ => return None,
    };
    let mut m = Message::new(role);
    m.id = v.get("id").and_then(Value::as_str).map(str::to_string);
    m.parent_id = v.get("parentId").and_then(Value::as_str).map(str::to_string);
    m.timestamp = v
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_ts)
        .or_else(|| msg.get("timestamp").and_then(Value::as_i64).and_then(ms_to_dt));
    m.model = msg.get("model").and_then(Value::as_str).map(str::to_string);

    match role {
        Role::Tool => {
            m.content.push(Block::ToolResult {
                tool_use_id: msg
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                content: coerce_content_text(msg.get("content")),
                is_error: msg.get("isError").and_then(Value::as_bool).unwrap_or(false),
            });
        }
        _ => match msg.get("content") {
            Some(Value::String(s)) => m.content.push(Block::Text { text: s.clone() }),
            Some(Value::Array(items)) => {
                for item in items {
                    if let Some(b) = content_block(item) {
                        m.content.push(b);
                    }
                }
            }
            _ => {}
        },
    }

    (!m.content.is_empty()).then_some(m)
}

fn content_block(item: &Value) -> Option<Block> {
    match item.get("type").and_then(Value::as_str)? {
        "text" => Some(Block::Text {
            text: item.get("text").and_then(Value::as_str)?.to_string(),
        }),
        "thinking" => Some(Block::Thinking {
            text: item
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            signature: None,
            encrypted: None,
        }),
        "toolCall" => Some(Block::ToolUse {
            id: item.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
            name: item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
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

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn ms_to_dt(ms: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms).single()
}
