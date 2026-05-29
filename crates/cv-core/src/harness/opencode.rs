//! OpenCode adapter — `~/.local/share/opencode/storage/`.
//!
//! Layout: `session/**/ses_*.json` (metadata, `directory` = cwd), `message/<sid>/<msgid>.json`
//! (per-message metadata), `part/<sid>/<msgid>/<partid>.json` (the actual content parts).

use super::Adapter;
use crate::ir::*;
use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
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
            let message_count = self
                .message_dir(&id)
                .map(|d| {
                    fs::read_dir(&d)
                        .map(|rd| rd.filter_map(|e| e.ok()).count())
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            out.push(SessionRef {
                id,
                harness: Harness::OpenCode,
                path: path.to_path_buf(),
                cwd: v.get("directory").and_then(Value::as_str).map(PathBuf::from),
                title: v
                    .get("title")
                    .and_then(Value::as_str)
                    .map(|t| crate::ir::truncate(t, 80)),
                created_at: v.pointer("/time/created").and_then(ms_to_dt),
                updated_at: v.pointer("/time/updated").and_then(ms_to_dt),
                message_count,
            });
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        let text = fs::read_to_string(&r.path)
            .with_context(|| format!("reading {}", r.path.display()))?;
        let meta: Value = serde_json::from_str(&text).unwrap_or(Value::Null);

        let mut s = Session {
            id: r.id.clone(),
            harness: Harness::OpenCode,
            cwd: meta.get("directory").and_then(Value::as_str).map(PathBuf::from),
            title: meta
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string),
            created_at: meta.pointer("/time/created").and_then(ms_to_dt),
            updated_at: meta.pointer("/time/updated").and_then(ms_to_dt),
            model: None,
            git: None,
            messages: Vec::new(),
            source_path: Some(r.path.clone()),
        };

        // collect & order messages by created time
        let mut msgs: Vec<(i64, Value)> = Vec::new();
        if let Some(mdir) = self.message_dir(&r.id) {
            if let Ok(rd) = fs::read_dir(&mdir) {
                for e in rd.filter_map(|e| e.ok()) {
                    if let Ok(t) = fs::read_to_string(e.path()) {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            let when = v.pointer("/time/created").and_then(Value::as_i64).unwrap_or(0);
                            msgs.push((when, v));
                        }
                    }
                }
            }
        }
        msgs.sort_by_key(|(t, _)| *t);

        for (_, mv) in &msgs {
            let role = match mv.get("role").and_then(Value::as_str) {
                Some("assistant") => Role::Assistant,
                Some("system") => Role::System,
                _ => Role::User,
            };
            if s.model.is_none() {
                s.model = mv.get("modelID").and_then(Value::as_str).map(str::to_string);
            }
            let msg_id = mv.get("id").and_then(Value::as_str).unwrap_or("");
            let mut m = Message::new(role);
            m.id = (!msg_id.is_empty()).then(|| msg_id.to_string());
            m.parent_id = mv.get("parentID").and_then(Value::as_str).map(str::to_string);
            m.timestamp = mv.pointer("/time/created").and_then(ms_to_dt);
            m.model = mv.get("modelID").and_then(Value::as_str).map(str::to_string);
            m.usage = parse_tokens(mv.get("tokens"));

            let mut tool_results: Vec<Block> = Vec::new();
            for part in self.load_parts(msg_id) {
                match part.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text" => {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            if !t.is_empty() {
                                m.content.push(Block::Text { text: t.to_string() });
                            }
                        }
                    }
                    "reasoning" => {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            if !t.is_empty() {
                                m.content.push(Block::Thinking {
                                    text: t.to_string(),
                                    signature: None,
                                    encrypted: None,
                                });
                            }
                        }
                    }
                    "tool" => {
                        let call_id = part
                            .get("callID")
                            .or_else(|| part.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let name = part
                            .get("tool")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        m.content.push(Block::ToolUse {
                            id: call_id.clone(),
                            name,
                            input: part.pointer("/state/input").cloned().unwrap_or(Value::Null),
                        });
                        if let Some(out) = part.pointer("/state/output") {
                            tool_results.push(Block::ToolResult {
                                tool_use_id: call_id,
                                content: coerce_text(out),
                                is_error: part.pointer("/state/status").and_then(Value::as_str)
                                    == Some("error"),
                            });
                        }
                    }
                    "file" => m.content.push(Block::Image {
                        media_type: part.get("mime").and_then(Value::as_str).map(str::to_string),
                        data_ref: part.get("url").and_then(Value::as_str).map(str::to_string),
                    }),
                    _ => {} // step-start/step-finish/snapshot/patch/agent
                }
            }

            // Older sessions stored no parts — fall back to the inline AI summary so the turn
            // isn't blank.
            if m.content.is_empty() {
                let summary = mv
                    .pointer("/summary/body")
                    .or_else(|| mv.pointer("/summary/title"))
                    .and_then(Value::as_str);
                if let Some(text) = summary {
                    if !text.is_empty() {
                        m.content.push(Block::Text {
                            text: text.to_string(),
                        });
                    }
                }
            }

            if !m.content.is_empty() {
                s.messages.push(m);
            }
            if !tool_results.is_empty() {
                let mut tm = Message::new(Role::Tool);
                tm.content = tool_results;
                s.messages.push(tm);
            }
        }

        Ok(s)
    }

    fn can_emit(&self) -> bool {
        false
    }
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

/// OpenCode timestamps are epoch milliseconds.
fn ms_to_dt(v: &Value) -> Option<DateTime<Utc>> {
    let ms = v.as_i64()?;
    Utc.timestamp_millis_opt(ms).single()
}
