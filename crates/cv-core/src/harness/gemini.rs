//! Gemini / Antigravity adapter — best-effort.
//!
//! The Antigravity IDE stores conversations as opaque protobuf (`~/.gemini/antigravity/conversations/
//! *.pb`) with no on-disk schema, so we can't read those yet. We *can* read the readable fallback at
//! `~/.gemini/tmp/<hash>/logs.json` — an array of `{sessionId, messageId, type, message, timestamp}`.
//! One logs.json may contain more than one `sessionId`, so each becomes its own IR session.

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
            if entry.file_name() != "logs.json" {
                continue;
            }
            let Ok(entries) = read_logs(entry.path()) else {
                continue;
            };
            // group by sessionId
            let mut by_session: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
            for e in &entries {
                let sid = e
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                by_session.entry(sid).or_default().push(e);
            }
            for (sid, items) in by_session {
                if sid.is_empty() {
                    continue;
                }
                let title = items
                    .iter()
                    .find(|e| e.get("type").and_then(Value::as_str) == Some("user"))
                    .and_then(|e| e.get("message").and_then(Value::as_str))
                    .map(|t| crate::ir::truncate(t, 80));
                let times: Vec<DateTime<Utc>> = items
                    .iter()
                    .filter_map(|e| e.get("timestamp").and_then(Value::as_str).and_then(parse_ts))
                    .collect();
                out.push(SessionRef {
                    id: sid,
                    harness: Harness::Gemini,
                    path: entry.path().to_path_buf(),
                    cwd: None,
                    title,
                    created_at: times.iter().min().copied(),
                    updated_at: times.iter().max().copied(),
                    message_count: items.len(),
                });
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        let entries = read_logs(&r.path)
            .with_context(|| format!("reading {}", r.path.display()))?;
        let mut items: Vec<&Value> = entries
            .iter()
            .filter(|e| e.get("sessionId").and_then(Value::as_str) == Some(r.id.as_str()))
            .collect();
        items.sort_by_key(|e| e.get("messageId").and_then(Value::as_i64).unwrap_or(0));

        let mut s = Session {
            id: r.id.clone(),
            harness: Harness::Gemini,
            cwd: None,
            title: r.title.clone(),
            created_at: r.created_at,
            updated_at: r.updated_at,
            model: None,
            git: None,
            messages: Vec::new(),
            source_path: Some(r.path.clone()),
        };
        for e in items {
            let role = match e.get("type").and_then(Value::as_str) {
                Some("assistant") | Some("model") => Role::Assistant,
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
        Ok(s)
    }

    fn can_emit(&self) -> bool {
        false
    }
}

/// Parse a Gemini `logs.json` from its text contents into one [`Session`] per distinct `sessionId`.
///
/// Pure (no filesystem). A single `logs.json` may interleave several sessions, so the entries are
/// grouped by `sessionId` (mirroring `discover`'s grouping). `source_path` records provenance.
pub fn parse_all_str(text: &str, source_path: Option<PathBuf>) -> Vec<Session> {
    let entries: Vec<Value> = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    // group by sessionId
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
        };
        for e in items {
            let role = match e.get("type").and_then(Value::as_str) {
                Some("assistant") | Some("model") => Role::Assistant,
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

fn read_logs(path: &Path) -> Result<Vec<Value>> {
    let text = fs::read_to_string(path)?;
    let v: Value = serde_json::from_str(&text)?;
    Ok(v.as_array().cloned().unwrap_or_default())
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}
