//! Hermes (Nous Research) adapter — `~/.hermes/state.db` (SQLite, schema v14).
//!
//! See `docs/FORMATS.md`. One DB holds all sessions: a `sessions` table (metadata) and a `messages`
//! table (OpenAI-shaped rows: role, content, tool_calls JSON, reasoning, …). cwd is NOT persisted
//! (runtime-only). Multimodal content is stored with a `\x00json:` sentinel prefix.

use super::Adapter;
use crate::ir::*;
use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::path::PathBuf;

const MULTIMODAL_SENTINEL: &str = "\u{0}json:";

pub struct Hermes {
    db: Option<PathBuf>,
}

impl Hermes {
    pub fn new() -> Self {
        // HERMES_HOME overrides ~/.hermes.
        let home = std::env::var_os("HERMES_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".hermes")));
        let db = home.map(|h| h.join("state.db")).filter(|p| p.exists());
        Hermes { db }
    }

    fn open(&self) -> Result<Connection> {
        let db = self.db.as_ref().context("hermes state.db not found")?;
        // Read-only so we never touch the user's live DB / take a write lock.
        let conn = Connection::open_with_flags(
            db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening {}", db.display()))?;
        Ok(conn)
    }
}

impl Default for Hermes {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for Hermes {
    fn harness(&self) -> Harness {
        Harness::Hermes
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.db.clone()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        if self.db.is_none() {
            return Ok(vec![]);
        }
        let conn = self.open()?;
        let path = self.db.clone().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, title, started_at, ended_at, message_count FROM sessions ORDER BY started_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            let title: Option<String> = row.get(1).ok().flatten();
            let started: Option<f64> = row.get(2).ok().flatten();
            let ended: Option<f64> = row.get(3).ok().flatten();
            let count: Option<i64> = row.get(4).ok().flatten();
            Ok(SessionRef {
                id,
                harness: Harness::Hermes,
                path: path.clone(),
                cwd: None,
                title: title.map(|t| crate::ir::truncate(&t, 80)),
                created_at: started.and_then(secs_to_dt),
                updated_at: ended.and_then(secs_to_dt).or_else(|| started.and_then(secs_to_dt)),
                message_count: count.unwrap_or(0).max(0) as usize,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            if let Ok(r) = r {
                out.push(r);
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        let conn = self.open()?;

        // session-level metadata
        let (model, started, ended, title): (Option<String>, Option<f64>, Option<f64>, Option<String>) =
            conn.query_row(
                "SELECT model, started_at, ended_at, title FROM sessions WHERE id = ?1",
                [&r.id],
                |row| {
                    Ok((
                        row.get(0).ok().flatten(),
                        row.get(1).ok().flatten(),
                        row.get(2).ok().flatten(),
                        row.get(3).ok().flatten(),
                    ))
                },
            )
            .unwrap_or((None, None, None, r.title.clone()));

        let mut s = Session {
            id: r.id.clone(),
            harness: Harness::Hermes,
            cwd: None,
            title: title.or_else(|| r.title.clone()),
            created_at: started.and_then(secs_to_dt).or(r.created_at),
            updated_at: ended.and_then(secs_to_dt).or(r.updated_at),
            model,
            git: None,
            messages: Vec::new(),
            source_path: Some(r.path.clone()),
        };

        let mut stmt = conn.prepare(
            "SELECT role, content, tool_call_id, tool_calls, tool_name, timestamp, reasoning, reasoning_details \
             FROM messages WHERE session_id = ?1 ORDER BY timestamp ASC, id ASC",
        )?;
        let rows = stmt.query_map([&r.id], |row| {
            Ok(MsgRow {
                role: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                content: row.get::<_, Option<String>>(1)?,
                tool_call_id: row.get::<_, Option<String>>(2)?,
                tool_calls: row.get::<_, Option<String>>(3)?,
                tool_name: row.get::<_, Option<String>>(4)?,
                timestamp: row.get::<_, Option<f64>>(5)?,
                reasoning: row.get::<_, Option<String>>(6)?,
                reasoning_details: row.get::<_, Option<String>>(7)?,
            })
        })?;

        for row in rows.flatten() {
            if let Some(m) = row.into_message() {
                s.messages.push(m);
            }
        }
        Ok(s)
    }

    fn can_emit(&self) -> bool {
        false
    }
}

struct MsgRow {
    role: String,
    content: Option<String>,
    tool_call_id: Option<String>,
    tool_calls: Option<String>,
    tool_name: Option<String>,
    timestamp: Option<f64>,
    reasoning: Option<String>,
    reasoning_details: Option<String>,
}

impl MsgRow {
    fn into_message(self) -> Option<Message> {
        let role = match self.role.as_str() {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "tool" => Role::Tool,
            "system" => Role::System,
            _ => Role::User,
        };
        let mut m = Message::new(role);
        m.timestamp = self.timestamp.and_then(secs_to_dt);

        // reasoning (assistant)
        let reasoning_text = self.reasoning.unwrap_or_default();
        let encrypted = self
            .reasoning_details
            .as_deref()
            .and_then(extract_encrypted_reasoning);
        if !reasoning_text.is_empty() || encrypted.is_some() {
            m.content.push(Block::Thinking {
                text: reasoning_text,
                signature: None,
                encrypted,
            });
        }

        // tool result rows carry the output in `content`
        if role == Role::Tool {
            m.content.push(Block::ToolResult {
                tool_use_id: self.tool_call_id.unwrap_or_default(),
                content: self.content.unwrap_or_default(),
                is_error: false,
            });
            // attach tool name for context if present
            if let Some(name) = self.tool_name {
                m.extra.insert("tool_name".into(), Value::String(name));
            }
            return (!m.content.is_empty()).then_some(m);
        }

        // text / multimodal content
        if let Some(raw) = &self.content {
            for b in decode_content(raw) {
                m.content.push(b);
            }
        }

        // assistant tool calls
        if let Some(tc) = &self.tool_calls {
            for b in parse_tool_calls(tc) {
                m.content.push(b);
            }
        }

        (!m.content.is_empty()).then_some(m)
    }
}

fn decode_content(raw: &str) -> Vec<Block> {
    if let Some(rest) = raw.strip_prefix(MULTIMODAL_SENTINEL) {
        if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(rest) {
            return items.iter().filter_map(part_to_block).collect();
        }
    }
    if raw.is_empty() {
        vec![]
    } else {
        vec![Block::Text {
            text: raw.to_string(),
        }]
    }
}

fn part_to_block(part: &Value) -> Option<Block> {
    match part.get("type").and_then(Value::as_str)? {
        "text" => Some(Block::Text {
            text: part.get("text").and_then(Value::as_str)?.to_string(),
        }),
        "image_url" => Some(Block::Image {
            media_type: None,
            data_ref: part
                .pointer("/image_url/url")
                .and_then(Value::as_str)
                .map(|u| crate::ir::truncate(u, 120)),
        }),
        _ => None,
    }
}

/// Hermes `tool_calls` is an OpenAI-shaped array: `[{id,type,function:{name,arguments}}]`.
fn parse_tool_calls(raw: &str) -> Vec<Block> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(raw) else {
        return vec![];
    };
    items
        .iter()
        .map(|tc| {
            let id = tc.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let name = tc
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let input = tc
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .and_then(|a| serde_json::from_str::<Value>(a).ok())
                .or_else(|| tc.pointer("/function/arguments").cloned())
                .unwrap_or(Value::Null);
            Block::ToolUse { id, name, input }
        })
        .collect()
}

/// reasoning_details is `[{type:"reasoning.encrypted_content", encrypted_content:"…"}, …]`.
fn extract_encrypted_reasoning(raw: &str) -> Option<String> {
    let Value::Array(items) = serde_json::from_str::<Value>(raw).ok()? else {
        return None;
    };
    items.iter().find_map(|it| {
        it.get("encrypted_content")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

fn secs_to_dt(s: f64) -> Option<DateTime<Utc>> {
    if !s.is_finite() || s <= 0.0 {
        return None;
    }
    Utc.timestamp_millis_opt((s * 1000.0) as i64).single()
}
