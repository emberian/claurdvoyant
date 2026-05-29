//! Grok CLI adapter — `~/.grok/sessions/<percent-encoded-cwd>/<sessionId>/`.
//!
//! Each session is a *directory* with `summary.json` (metadata) and `chat_history.jsonl` (transcript).
//! Tool calls live in `events.jsonl`/`updates.jsonl` (not yet ingested); `chat_history` carries the
//! system/user/assistant conversation plus assistant reasoning.

use super::Adapter;
use crate::ir::*;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub struct Grok {
    root: Option<PathBuf>,
}

impl Grok {
    pub fn new() -> Self {
        let root = dirs::home_dir().map(|h| h.join(".grok").join("sessions"));
        Grok {
            root: root.filter(|p| p.exists()),
        }
    }
}

impl Default for Grok {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for Grok {
    fn harness(&self) -> Harness {
        Harness::Grok
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
            if entry.file_name() != "summary.json" {
                continue;
            }
            match scan(entry.path()) {
                Ok(Some(r)) => out.push(r),
                Ok(None) => {}
                Err(e) => eprintln!("cv: skipping {}: {e:#}", entry.path().display()),
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        // r.path is the session directory.
        let dir = &r.path;
        let summary: Value = fs::read_to_string(dir.join("summary.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);

        let mut s = Session {
            id: r.id.clone(),
            harness: Harness::Grok,
            cwd: summary
                .pointer("/info/cwd")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .or_else(|| r.cwd.clone()),
            title: r.title.clone(),
            created_at: summary
                .get("created_at")
                .and_then(Value::as_str)
                .and_then(parse_ts),
            updated_at: summary
                .get("updated_at")
                .and_then(Value::as_str)
                .and_then(parse_ts),
            model: summary
                .get("current_model_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            git: grok_git(&summary),
            messages: Vec::new(),
            source_path: Some(dir.clone()),
        };

        let chat = fs::read_to_string(dir.join("chat_history.jsonl"))
            .with_context(|| format!("reading chat_history in {}", dir.display()))?;
        for line in chat.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if let Some(m) = chat_message(&v) {
                s.messages.push(m);
            }
        }
        Ok(s)
    }

    fn can_emit(&self) -> bool {
        false
    }
}

fn grok_git(summary: &Value) -> Option<GitInfo> {
    let branch = summary.get("head_branch").and_then(Value::as_str);
    let commit = summary.get("head_commit").and_then(Value::as_str);
    let remote = summary
        .get("git_remotes")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str);
    if branch.is_none() && commit.is_none() && remote.is_none() {
        return None;
    }
    Some(GitInfo {
        branch: branch.map(str::to_string),
        commit: commit.map(str::to_string),
        remote: remote.map(str::to_string),
    })
}

fn chat_message(v: &Value) -> Option<Message> {
    let ty = v.get("type").and_then(Value::as_str)?;
    let role = match ty {
        "system" => Role::System,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    let mut m = Message::new(role);

    // assistant lines may carry reasoning
    if let Some(reasoning) = v.get("reasoning") {
        let text = reasoning
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let encrypted = reasoning
            .get("encrypted")
            .and_then(Value::as_str)
            .map(str::to_string);
        if !text.is_empty() || encrypted.is_some() {
            m.content.push(Block::Thinking {
                text,
                signature: None,
                encrypted,
            });
        }
    }
    if let Some(model) = v.get("model_id").and_then(Value::as_str) {
        m.model = Some(model.to_string());
    }

    let text = coerce_content(v.get("content"));
    if !text.is_empty() {
        m.content.push(Block::Text { text });
    }
    if m.content.is_empty() {
        return None;
    }
    Some(m)
}

/// Grok `content` is a string (system/assistant) or `[{type:text,text}]` (user).
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

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// `summary_path` is `.../<sid>/summary.json`; the session dir is its parent.
fn scan(summary_path: &Path) -> Result<Option<SessionRef>> {
    let dir = summary_path.parent().map(Path::to_path_buf);
    let Some(dir) = dir else { return Ok(None) };
    let text = fs::read_to_string(summary_path)?;
    let summary: Value = serde_json::from_str(&text)?;

    let id = summary
        .pointer("/info/id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            dir.file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
        })
        .unwrap_or_default();

    let cwd = summary
        .pointer("/info/cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from);

    // title: explicit summary text, else first user message from chat_history
    let mut title = summary
        .get("session_summary")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| crate::ir::truncate(s, 80));
    if title.is_none() {
        if let Ok(chat) = fs::read_to_string(dir.join("chat_history.jsonl")) {
            for line in chat.lines() {
                let Ok(v) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if v.get("type").and_then(Value::as_str) == Some("user") {
                    let t = coerce_content(v.get("content"));
                    if !t.trim().is_empty() {
                        title = Some(crate::ir::truncate(&t, 80));
                        break;
                    }
                }
            }
        }
    }

    let message_count = summary
        .get("num_chat_messages")
        .and_then(Value::as_u64)
        .or_else(|| summary.get("num_messages").and_then(Value::as_u64))
        .unwrap_or(0) as usize;

    Ok(Some(SessionRef {
        id,
        harness: Harness::Grok,
        path: dir,
        cwd,
        title,
        created_at: summary.get("created_at").and_then(Value::as_str).and_then(parse_ts),
        updated_at: summary.get("updated_at").and_then(Value::as_str).and_then(parse_ts),
        message_count,
    }))
}
