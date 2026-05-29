//! Claude Code adapter — `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl`.
//!
//! See `docs/FORMATS.md`. Key points: one session per `.jsonl` file; each line is a typed record;
//! `cwd` is read from inside the transcript (the dir-name encoding is lossy), and the conversation is
//! threaded via `uuid`/`parentUuid`.

use super::Adapter;
use crate::ir::*;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use walkdir::WalkDir;

pub struct Claude {
    root: Option<PathBuf>,
}

impl Claude {
    pub fn new() -> Self {
        let root = dirs::home_dir().map(|h| h.join(".claude").join("projects"));
        Claude {
            root: root.filter(|p| p.exists()),
        }
    }
}

impl Default for Claude {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for Claude {
    fn harness(&self) -> Harness {
        Harness::Claude
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.root.clone()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(root) = &self.root else {
            return Ok(vec![]);
        };
        let mut out = Vec::new();
        // Session files sit at projects/<encoded>/<sid>.jsonl (depth 2). Subagent transcripts live
        // deeper (…/<sid>/subagents/…), which max_depth(2) naturally excludes.
        for entry in WalkDir::new(root)
            .min_depth(2)
            .max_depth(2)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            match scan(path) {
                Ok(r) => out.push(r),
                Err(e) => eprintln!("cv: skipping {}: {e:#}", path.display()),
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        let text = fs::read_to_string(&r.path)
            .with_context(|| format!("reading {}", r.path.display()))?;
        Ok(parse_str(&r.id, &text, Some(r.path.clone())))
    }

    fn can_emit(&self) -> bool {
        false // TODO: Claude as a conversion target
    }
}

/// Parse a Claude `.jsonl` transcript from its text contents into a [`Session`].
///
/// Pure (no filesystem); the on-disk [`Adapter::parse`] reads the file then delegates here. `id` is
/// the session id (usually the file stem); `source_path` is recorded for provenance when known.
pub fn parse_str(id: &str, text: &str, source_path: Option<PathBuf>) -> Session {
    let mut session = Session {
        id: id.to_string(),
        harness: Harness::Claude,
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        model: None,
        git: None,
        messages: Vec::new(),
        source_path,
    };

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // tolerate the occasional corrupt line
        };
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");

        // session-level metadata records
        match ty {
            "ai-title" => {
                if let Some(t) = v.get("aiTitle").and_then(Value::as_str) {
                    session.title = Some(t.to_string());
                }
                continue;
            }
            "mode" | "permission-mode" | "last-prompt" | "summary" | "attachment" => continue,
            _ => {}
        }

        if session.cwd.is_none() {
            if let Some(cwd) = v.get("cwd").and_then(Value::as_str) {
                session.cwd = Some(PathBuf::from(cwd));
            }
        }
        if session.git.is_none() {
            if let Some(branch) = v.get("gitBranch").and_then(Value::as_str) {
                session.git = Some(GitInfo {
                    branch: Some(branch.to_string()),
                    ..Default::default()
                });
            }
        }
        if let Some(ts) = v.get("timestamp").and_then(Value::as_str).and_then(parse_ts) {
            session.created_at.get_or_insert(ts);
            session.updated_at = Some(ts);
        }

        if let Some(msg) = parse_message(ty, &v) {
            if session.model.is_none() {
                session.model = msg.model.clone();
            }
            session.messages.push(msg);
        }
    }

    session
}

/// Cheap metadata-only scan for `discover`.
fn scan(path: &std::path::Path) -> Result<SessionRef> {
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);

    let mut cwd = None;
    let mut title = None;
    let mut created_at: Option<DateTime<Utc>> = None;
    let mut updated_at: Option<DateTime<Utc>> = None;
    let mut message_count = 0usize;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        if ty == "ai-title" {
            if let Some(t) = v.get("aiTitle").and_then(Value::as_str) {
                title = Some(t.to_string());
            }
        }
        if cwd.is_none() {
            if let Some(c) = v.get("cwd").and_then(Value::as_str) {
                cwd = Some(PathBuf::from(c));
            }
        }
        if let Some(ts) = v.get("timestamp").and_then(Value::as_str).and_then(parse_ts) {
            created_at.get_or_insert(ts);
            updated_at = Some(ts);
        }
        if matches!(ty, "user" | "assistant") {
            message_count += 1;
        }
    }

    Ok(SessionRef {
        id,
        harness: Harness::Claude,
        path: path.to_path_buf(),
        cwd,
        title,
        created_at,
        updated_at,
        message_count,
    })
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Turn one `user`/`assistant` record into an IR [`Message`]. Returns `None` for other line types.
fn parse_message(ty: &str, v: &Value) -> Option<Message> {
    let role = match ty {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };

    let msg = v.get("message")?;
    let id = v
        .get("uuid")
        .and_then(Value::as_str)
        .map(str::to_string);
    let parent_id = v
        .get("parentUuid")
        .and_then(Value::as_str)
        .map(str::to_string);
    let timestamp = v.get("timestamp").and_then(Value::as_str).and_then(parse_ts);
    let model = msg.get("model").and_then(Value::as_str).map(str::to_string);
    let usage = msg.get("usage").map(parse_usage);

    let mut blocks = Vec::new();
    match msg.get("content") {
        Some(Value::String(s)) => blocks.push(Block::Text { text: s.clone() }),
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(b) = parse_block(item) {
                    blocks.push(b);
                }
            }
        }
        _ => {}
    }

    // A `user` line carrying only tool results is really a Tool turn.
    let role = if role == Role::User
        && !blocks.is_empty()
        && blocks
            .iter()
            .all(|b| matches!(b, Block::ToolResult { .. }))
    {
        Role::Tool
    } else {
        role
    };

    Some(Message {
        id,
        parent_id,
        role,
        timestamp,
        model,
        content: blocks,
        usage,
        extra: serde_json::Map::new(),
    })
}

fn parse_block(item: &Value) -> Option<Block> {
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
            signature: item
                .get("signature")
                .and_then(Value::as_str)
                .map(str::to_string),
            encrypted: None,
        }),
        "tool_use" => Some(Block::ToolUse {
            id: item.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
            name: item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            input: item.get("input").cloned().unwrap_or(Value::Null),
        }),
        "tool_result" => Some(Block::ToolResult {
            tool_use_id: item
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            content: coerce_text(item.get("content")),
            is_error: item.get("is_error").and_then(Value::as_bool).unwrap_or(false),
        }),
        "image" => Some(Block::Image {
            media_type: item
                .get("source")
                .and_then(|s| s.get("media_type"))
                .and_then(Value::as_str)
                .map(str::to_string),
            data_ref: None,
        }),
        _ => None,
    }
}

/// Claude tool_result `content` is sometimes a string, sometimes `[{type:text,text}]`.
fn coerce_text(v: Option<&Value>) -> String {
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

fn parse_usage(v: &Value) -> Usage {
    let get = |k: &str| v.get(k).and_then(Value::as_u64);
    Usage {
        input_tokens: get("input_tokens"),
        output_tokens: get("output_tokens"),
        cache_read_tokens: get("cache_read_input_tokens"),
        cache_creation_tokens: get("cache_creation_input_tokens"),
    }
}
