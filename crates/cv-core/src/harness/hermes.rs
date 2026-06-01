//! Hermes (Nous Research) adapter — `~/.hermes/state.db` (SQLite, schema v14).
//!
//! See `docs/FORMATS.md`. One DB holds all sessions: a `sessions` table (metadata) and a `messages`
//! table (OpenAI-shaped rows: role, content, tool_calls JSON, reasoning, …). cwd is NOT persisted
//! (runtime-only). Multimodal content is stored with a `\x00json:` sentinel prefix.
//!
//! Historical coverage: Hermes does NOT version-gate column additions — it reconciles live tables
//! against `SCHEMA_SQL` and `ALTER TABLE ADD COLUMN`s anything missing (the Beets/sqlite-utils
//! pattern). So a database that was last opened by an older Hermes can be missing newer columns
//! entirely (`token_count`, `reasoning_content`, `reasoning_details`, `codex_reasoning_items`,
//! `codex_message_items`, `platform_message_id`, `observed`, `finish_reason`, …). We therefore probe
//! `PRAGMA table_info` and only SELECT columns that actually exist, so the parser degrades
//! gracefully instead of failing the whole session on a missing column.
//!
//! Reasoning is recorded across several provider-specific columns and we map each:
//!   * `reasoning`         — short summary text (DeepSeek/Qwen, OpenRouter summary) → Thinking.text
//!   * `reasoning_content` — provider-native scratchpad (Moonshot/Novita) → Thinking.text (appended)
//!   * `reasoning_details` — `[{type:"reasoning.summary"|"reasoning.encrypted_content"|"thinking", …}]`
//!                            (OpenRouter unified) → Thinking.text (summaries) + Thinking.encrypted
//!   * `codex_reasoning_items` — `[{type:"reasoning", id, encrypted_content}]` (Codex Responses API)
//!                                → Thinking.encrypted + stashed verbatim in `extra`
//!   * `codex_message_items`   — `[{type:"message", phase, content:[{type:"output_text",text}]}]`
//!                                → stashed verbatim in `extra` (Codex final/commentary phases)
//!
//! Compression chains: a long conversation is split across multiple session rows linked by
//! `parent_session_id`, where the parent has `end_reason='compression'`. `parse` walks the lineage
//! root→tip and merges all messages (mirroring Hermes's own
//! `get_messages_as_conversation(include_ancestors=True)`), deduplicating the replayed first user
//! message at each compression boundary, so a resumed/compressed conversation reads as one transcript.

use super::Adapter;
use crate::ir::*;
use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;

const MULTIMODAL_SENTINEL: &str = "\u{0}json:";

/// Optional `messages` columns that older schemas may lack. We probe for each before SELECTing.
const OPTIONAL_MSG_COLS: &[&str] = &[
    "token_count",
    "finish_reason",
    "reasoning",
    "reasoning_content",
    "reasoning_details",
    "codex_reasoning_items",
    "codex_message_items",
    "platform_message_id",
    "observed",
];

pub struct Hermes {
    /// Every Hermes `state.db` we can read: the top-level `<home>/state.db` PLUS one per
    /// `<home>/profiles/*/state.db`. Hermes stores most sessions under the *active* profile (e.g.
    /// `profiles/hermes-x/state.db`), so a single top-level DB only sees a small slice. Each DB is
    /// opened independently per call (read-only) — `SessionRef.path` carries which DB a ref came
    /// from, so cross-profile session-id collisions never alias.
    dbs: Vec<PathBuf>,
}

impl Hermes {
    pub fn new() -> Self {
        // HERMES_HOME overrides ~/.hermes.
        let home = std::env::var_os("HERMES_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".hermes")));
        let mut dbs = Vec::new();
        if let Some(home) = home {
            // Top-level DB (legacy / no-profile installs).
            let top = home.join("state.db");
            if top.exists() {
                dbs.push(top);
            }
            // Every `profiles/<name>/state.db` — depth-1 read_dir, so pre-update
            // `state-snapshots/**/state.db` are naturally skipped (never recurse).
            if let Ok(rd) = std::fs::read_dir(home.join("profiles")) {
                for e in rd.flatten() {
                    let p = e.path().join("state.db");
                    if p.exists() {
                        dbs.push(p); // hermes-x, hermes-google, …
                    }
                }
            }
        }
        Hermes { dbs }
    }

    fn open_path(path: &PathBuf) -> Result<Connection> {
        // Read-only so we never touch the user's live DB / take a write lock.
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening {}", path.display()))?;
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
        self.dbs.first().cloned()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        // Iterate every DB (top-level + each profile), opening each read-only, and concatenate the
        // per-DB discovery. Each `SessionRef` carries its own `path`, so refs from different
        // profiles stay disambiguated even when their internal session ids collide. A single
        // unreadable DB (e.g. permission-denied) is skipped rather than failing the whole scan.
        let mut out = Vec::new();
        for db in &self.dbs {
            let conn = match Self::open_path(db) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if let Ok(refs) = discover_conn(&conn, db) {
                out.extend(refs);
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        // Route by the ref's own DB path (set at discovery time), NOT a single self.db — this is
        // what lets one Hermes adapter span many profile DBs without id collisions.
        let conn = Self::open_path(&r.path)?;
        parse_conn(&conn, r)
    }

    fn can_emit(&self) -> bool {
        false
    }
}

/// Which optional columns exist on the `messages` table of this DB (for historical schemas).
fn present_msg_cols(conn: &Connection) -> HashSet<String> {
    let mut present = HashSet::new();
    if let Ok(mut stmt) = conn.prepare("PRAGMA table_info(messages)") {
        if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(1)) {
            for name in rows.flatten() {
                present.insert(name);
            }
        }
    }
    present
}

/// True if `sessions` has a given column (older schemas may lack `title`, `parent_session_id`, …).
fn session_has_col(conn: &Connection, col: &str) -> bool {
    let mut stmt = match conn.prepare("PRAGMA table_info(sessions)") {
        Ok(s) => s,
        Err(_) => return false,
    };
    let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(1)) else {
        return false;
    };
    let found = rows.flatten().any(|n| n == col);
    found
}

fn discover_conn(conn: &Connection, path: &PathBuf) -> Result<Vec<SessionRef>> {
    // `title` predates several columns but has existed a long time; guard it anyway.
    let has_title = session_has_col(conn, "title");
    let title_expr = if has_title { "title" } else { "NULL" };
    let sql = format!(
        "SELECT id, {title_expr}, started_at, ended_at, message_count \
         FROM sessions ORDER BY started_at DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
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
    for r in rows.flatten() {
        out.push(r);
    }
    Ok(out)
}

fn parse_conn(conn: &Connection, r: &SessionRef) -> Result<Session> {
    // session-level metadata (guard optional columns for historical schemas).
    let has_model = session_has_col(conn, "model");
    let has_title = session_has_col(conn, "title");
    let has_source = session_has_col(conn, "source");
    let has_parent = session_has_col(conn, "parent_session_id");
    let has_end_reason = session_has_col(conn, "end_reason");

    let meta_sql = format!(
        "SELECT {model}, started_at, ended_at, {title}, {source}, {parent}, {end_reason} \
         FROM sessions WHERE id = ?1",
        model = if has_model { "model" } else { "NULL" },
        title = if has_title { "title" } else { "NULL" },
        source = if has_source { "source" } else { "NULL" },
        parent = if has_parent { "parent_session_id" } else { "NULL" },
        end_reason = if has_end_reason { "end_reason" } else { "NULL" },
    );
    let (model, started, ended, title, source, _parent, _end_reason): (
        Option<String>,
        Option<f64>,
        Option<f64>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(&meta_sql, [&r.id], |row| {
            Ok((
                row.get(0).ok().flatten(),
                row.get(1).ok().flatten(),
                row.get(2).ok().flatten(),
                row.get(3).ok().flatten(),
                row.get(4).ok().flatten(),
                row.get(5).ok().flatten(),
                row.get(6).ok().flatten(),
            ))
        })
        .unwrap_or((None, None, None, r.title.clone(), None, None, None));

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
        extra: serde_json::Map::new(),
    };

    // Walk the compression/branch lineage root→tip so a compressed-and-continued conversation
    // reads as a single transcript (mirrors get_messages_as_conversation(include_ancestors=True)).
    let lineage = if has_parent {
        session_lineage_root_to_tip(conn, &r.id)
    } else {
        vec![r.id.clone()]
    };
    // `source` (cli/tui/telegram/…) is intentionally not surfaced on the IR Session: it has no
    // free-form extra map. See report for the proposed Session.extra addition.
    let _ = &source;

    let cols = present_msg_cols(conn);
    let has = |c: &str| cols.contains(c);

    // Build the SELECT defensively: required cols always present; optional cols gated by probe.
    let mut select_cols: Vec<&str> = vec!["role", "content", "tool_call_id", "tool_calls", "tool_name", "timestamp"];
    for c in OPTIONAL_MSG_COLS {
        if has(c) {
            select_cols.push(c);
        }
    }
    let select_list = select_cols.join(", ");

    for sid in &lineage {
        let sql = format!(
            "SELECT {select_list} FROM messages WHERE session_id = ?1 ORDER BY timestamp ASC, id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        // Map column name -> index so we can read by name regardless of which optionals exist.
        let idx = |name: &str| select_cols.iter().position(|c| *c == name);
        let rows = stmt.query_map([sid], |row| {
            let get_str = |name: &str| -> Option<String> {
                idx(name).and_then(|i| row.get::<_, Option<String>>(i).ok().flatten())
            };
            let get_i64 = |name: &str| -> Option<i64> {
                idx(name).and_then(|i| row.get::<_, Option<i64>>(i).ok().flatten())
            };
            Ok(MsgRow {
                role: get_str("role").unwrap_or_default(),
                content: get_str("content"),
                tool_call_id: get_str("tool_call_id"),
                tool_calls: get_str("tool_calls"),
                tool_name: get_str("tool_name"),
                timestamp: idx("timestamp")
                    .and_then(|i| row.get::<_, Option<f64>>(i).ok().flatten()),
                token_count: get_i64("token_count"),
                finish_reason: get_str("finish_reason"),
                reasoning: get_str("reasoning"),
                reasoning_content: get_str("reasoning_content"),
                reasoning_details: get_str("reasoning_details"),
                codex_reasoning_items: get_str("codex_reasoning_items"),
                codex_message_items: get_str("codex_message_items"),
                platform_message_id: get_str("platform_message_id"),
                observed: get_i64("observed"),
            })
        })?;

        for row in rows.flatten() {
            if let Some(m) = row.into_message() {
                // Dedup the replayed first user message at compression boundaries.
                if is_duplicate_replayed_user_message(&s.messages, &m) {
                    continue;
                }
                s.messages.push(m);
            }
        }
    }
    Ok(s)
}

/// Walk `parent_session_id` from `session_id` up to the root, return root→tip order.
/// Mirrors `SessionDB._session_lineage_root_to_tip`. Bounded + cycle-guarded.
fn session_lineage_root_to_tip(conn: &Connection, session_id: &str) -> Vec<String> {
    if session_id.is_empty() {
        return vec![session_id.to_string()];
    }
    let mut chain: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut current = session_id.to_string();
    for _ in 0..100 {
        if current.is_empty() || seen.contains(&current) {
            break;
        }
        seen.insert(current.clone());
        chain.push(current.clone());
        let parent: Option<String> = conn
            .query_row(
                "SELECT parent_session_id FROM sessions WHERE id = ?1",
                [&current],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten();
        match parent {
            Some(p) => current = p,
            None => break,
        }
    }
    chain.reverse();
    if chain.is_empty() {
        vec![session_id.to_string()]
    } else {
        chain
    }
}

/// Mirrors `SessionDB._is_duplicate_replayed_user_message`: a compression continuation re-injects
/// the boundary's last user message as its first message; suppress that duplicate when merging.
fn is_duplicate_replayed_user_message(existing: &[Message], msg: &Message) -> bool {
    if msg.role != Role::User {
        return false;
    }
    let Some(text) = single_text(msg) else {
        return false;
    };
    if text.is_empty() {
        return false;
    }
    for prev in existing.iter().rev() {
        match prev.role {
            Role::User => {
                if single_text(prev).as_deref() == Some(text.as_str()) {
                    return true;
                }
            }
            Role::Assistant => {
                // Any substantive assistant turn (text or tool call) ends the look-back window.
                let has_content = prev
                    .content
                    .iter()
                    .any(|b| matches!(b, Block::Text { .. } | Block::ToolUse { .. }));
                if has_content {
                    return false;
                }
            }
            _ => {}
        }
    }
    false
}

/// Plain text of a message iff it is exactly one Text block (the shape Hermes replays as a string).
fn single_text(m: &Message) -> Option<String> {
    if m.content.len() == 1 {
        if let Block::Text { text } = &m.content[0] {
            return Some(text.to_string());
        }
    }
    None
}

struct MsgRow {
    role: String,
    content: Option<String>,
    tool_call_id: Option<String>,
    tool_calls: Option<String>,
    tool_name: Option<String>,
    timestamp: Option<f64>,
    token_count: Option<i64>,
    finish_reason: Option<String>,
    reasoning: Option<String>,
    reasoning_content: Option<String>,
    reasoning_details: Option<String>,
    codex_reasoning_items: Option<String>,
    codex_message_items: Option<String>,
    platform_message_id: Option<String>,
    observed: Option<i64>,
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

        // Per-message token_count → Usage. Hermes records a single combined count per message; we
        // route it to output_tokens for assistant turns and input_tokens otherwise (best-effort).
        if let Some(tc) = self.token_count {
            if tc > 0 {
                let mut u = Usage::default();
                if role == Role::Assistant {
                    u.output_tokens = Some(tc as u64);
                } else {
                    u.input_tokens = Some(tc as u64);
                }
                m.usage = Some(u);
            }
        }

        if let Some(fr) = self.finish_reason.filter(|s| !s.is_empty()) {
            m.extra.insert("finish_reason".into(), Value::String(fr));
        }
        if let Some(pmid) = self.platform_message_id.filter(|s| !s.is_empty()) {
            m.extra.insert("platform_message_id".into(), Value::String(pmid));
        }
        if matches!(self.observed, Some(o) if o != 0) {
            m.extra.insert("observed".into(), Value::Bool(true));
        }

        // Reasoning: combine summary text from `reasoning`, `reasoning_content`, and the text
        // entries of `reasoning_details`; collect any encrypted blob (reasoning_details or codex).
        let mut reasoning_text = String::new();
        let mut push_reasoning = |s: &str| {
            let s = s.trim();
            if s.is_empty() {
                return;
            }
            if !reasoning_text.is_empty() {
                reasoning_text.push_str("\n\n");
            }
            reasoning_text.push_str(s);
        };
        if let Some(r) = self.reasoning.as_deref() {
            push_reasoning(r);
        }
        if let Some(r) = self.reasoning_content.as_deref() {
            // Avoid duplicating when identical to `reasoning` (Hermes' own dedup behaviour).
            if Some(r) != self.reasoning.as_deref() {
                push_reasoning(r);
            }
        }
        let mut encrypted: Option<String> = None;
        if let Some(raw) = self.reasoning_details.as_deref() {
            let (texts, enc) = parse_reasoning_details(raw);
            for t in texts {
                push_reasoning(&t);
            }
            encrypted = encrypted.or(enc);
            // Preserve the full structured blob for lossless round-tripping.
            if let Ok(v) = serde_json::from_str::<Value>(raw) {
                m.extra.insert("reasoning_details".into(), v);
            }
        }
        if let Some(raw) = self.codex_reasoning_items.as_deref() {
            encrypted = encrypted.or_else(|| extract_codex_encrypted(raw));
            if let Ok(v) = serde_json::from_str::<Value>(raw) {
                m.extra.insert("codex_reasoning_items".into(), v);
            }
        }
        if let Some(raw) = self.codex_message_items.as_deref() {
            if let Ok(v) = serde_json::from_str::<Value>(raw) {
                m.extra.insert("codex_message_items".into(), v);
            }
        }
        if !reasoning_text.is_empty() || encrypted.is_some() {
            m.content.push(Block::Thinking {
                text: reasoning_text.into(),
                signature: None,
                encrypted,
                redacted: false,
            });
        }

        // tool result rows carry the output in `content`
        if role == Role::Tool {
            m.content.push(Block::ToolResult {
                tool_use_id: self.tool_call_id.unwrap_or_default(),
                content: self.content.unwrap_or_default().into(),
                is_error: false,
                tool_name: self.tool_name.clone(),
                status: None,
                details: None,
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

        (!m.content.is_empty() || !m.extra.is_empty()).then_some(m)
    }
}

fn decode_content(raw: &str) -> Vec<Block> {
    if let Some(rest) = raw.strip_prefix(MULTIMODAL_SENTINEL) {
        match serde_json::from_str::<Value>(rest) {
            Ok(Value::Array(items)) => return items.iter().filter_map(part_to_block).collect(),
            // Dict-shaped content (provider wrappers, e.g. {"parts":[…]}). Best-effort: pull any
            // string fields out; otherwise stash nothing and fall through to the empty case.
            Ok(Value::Object(map)) => {
                if let Some(blocks) = object_content_to_blocks(&map) {
                    return blocks;
                }
            }
            _ => {}
        }
    }
    if raw.is_empty() {
        vec![]
    } else {
        vec![Block::Text {
            text: raw.to_string().into(),
        }]
    }
}

/// Best-effort extraction from a dict-shaped (non-array) decoded content payload.
fn object_content_to_blocks(map: &serde_json::Map<String, Value>) -> Option<Vec<Block>> {
    // Common shape: {"parts": [ {"text": "…"} | {"type":…} ]}
    if let Some(Value::Array(parts)) = map.get("parts") {
        let blocks: Vec<Block> = parts
            .iter()
            .filter_map(|p| {
                part_to_block(p).or_else(|| {
                    p.get("text")
                        .and_then(Value::as_str)
                        .map(|t| Block::Text { text: t.to_string().into() })
                })
            })
            .collect();
        if !blocks.is_empty() {
            return Some(blocks);
        }
    }
    None
}

fn part_to_block(part: &Value) -> Option<Block> {
    match part.get("type").and_then(Value::as_str)? {
        // OpenAI chat (`text`) and Responses (`input_text`/`output_text`) text parts.
        "text" | "input_text" | "output_text" => Some(Block::Text {
            text: part.get("text").and_then(Value::as_str)?.to_string().into(),
        }),
        // `image_url` parts hold either `{image_url:{url}}` or, for `input_image`, `{image_url:"…"}`.
        "image_url" | "input_image" => {
            let url = part
                .pointer("/image_url/url")
                .and_then(Value::as_str)
                .or_else(|| part.get("image_url").and_then(Value::as_str));
            Some(Block::Image {
                media_type: None,
                data_ref: url.map(|u| crate::ir::truncate(u, 120)),
            })
        }
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

/// `reasoning_details` is an OpenRouter-unified array. Each item may carry summary/thinking text
/// (`reasoning.summary`, `thinking`, `redacted_thinking`) or an encrypted blob
/// (`reasoning.encrypted_content`). Returns (collected text summaries, first encrypted blob).
fn parse_reasoning_details(raw: &str) -> (Vec<String>, Option<String>) {
    let mut texts = Vec::new();
    let mut encrypted = None;
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(raw) else {
        return (texts, encrypted);
    };
    for it in &items {
        if encrypted.is_none() {
            if let Some(enc) = it.get("encrypted_content").and_then(Value::as_str) {
                encrypted = Some(enc.to_string());
            }
        }
        // Summary/thinking text lives under one of several keys (provider-dependent).
        let text = it
            .get("summary")
            .or_else(|| it.get("thinking"))
            .or_else(|| it.get("content"))
            .or_else(|| it.get("text"))
            .and_then(Value::as_str);
        if let Some(t) = text {
            if !t.is_empty() {
                texts.push(t.to_string());
            }
        }
    }
    (texts, encrypted)
}

/// `codex_reasoning_items` is `[{type:"reasoning", id, encrypted_content}]`.
fn extract_codex_encrypted(raw: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// Schema matching hermes_state.py SCHEMA_VERSION 14.
    const SCHEMA_V14: &str = "
        CREATE TABLE sessions (
            id TEXT PRIMARY KEY,
            source TEXT NOT NULL,
            user_id TEXT,
            model TEXT,
            model_config TEXT,
            system_prompt TEXT,
            parent_session_id TEXT,
            started_at REAL NOT NULL,
            ended_at REAL,
            end_reason TEXT,
            message_count INTEGER DEFAULT 0,
            tool_call_count INTEGER DEFAULT 0,
            input_tokens INTEGER DEFAULT 0,
            output_tokens INTEGER DEFAULT 0,
            cache_read_tokens INTEGER DEFAULT 0,
            cache_write_tokens INTEGER DEFAULT 0,
            reasoning_tokens INTEGER DEFAULT 0,
            title TEXT
        );
        CREATE TABLE messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT,
            tool_call_id TEXT,
            tool_calls TEXT,
            tool_name TEXT,
            timestamp REAL NOT NULL,
            token_count INTEGER,
            finish_reason TEXT,
            reasoning TEXT,
            reasoning_content TEXT,
            reasoning_details TEXT,
            codex_reasoning_items TEXT,
            codex_message_items TEXT,
            platform_message_id TEXT,
            observed INTEGER DEFAULT 0
        );
    ";

    /// An older (pre-v14-ish) schema: only the columns Hermes had before the reasoning/codex/token
    /// additions. Exercises the column-probing degradation path.
    const SCHEMA_OLD: &str = "
        CREATE TABLE sessions (
            id TEXT PRIMARY KEY,
            source TEXT NOT NULL,
            model TEXT,
            started_at REAL NOT NULL,
            ended_at REAL,
            message_count INTEGER DEFAULT 0,
            title TEXT
        );
        CREATE TABLE messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT,
            tool_call_id TEXT,
            tool_calls TEXT,
            tool_name TEXT,
            timestamp REAL NOT NULL
        );
    ";

    fn mk_conn(schema: &str) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(schema).unwrap();
        conn
    }

    fn sref(id: &str) -> SessionRef {
        SessionRef {
            id: id.into(),
            harness: Harness::Hermes,
            path: PathBuf::from(":memory:"),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        }
    }

    fn insert_session(conn: &Connection, id: &str, parent: Option<&str>, end_reason: Option<&str>, started: f64, ended: Option<f64>) {
        conn.execute(
            "INSERT INTO sessions (id, source, model, parent_session_id, started_at, ended_at, end_reason, title) \
             VALUES (?1, 'cli', 'nous/hermes-4', ?2, ?3, ?4, ?5, 'T')",
            rusqlite::params![id, parent, started, ended, end_reason],
        ).unwrap();
    }

    #[test]
    fn parses_basic_text_conversation() {
        let conn = mk_conn(SCHEMA_V14);
        insert_session(&conn, "s1", None, None, 1000.0, None);
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('s1','user','Hello',1001.0)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp, token_count, finish_reason) \
             VALUES ('s1','assistant','Hi there!',1002.0, 42, 'stop')",
            [],
        ).unwrap();

        let s = parse_conn(&conn, &sref("s1")).unwrap();
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[0].text().as_deref(), Some("Hello"));
        assert_eq!(s.messages[1].role, Role::Assistant);
        assert_eq!(s.messages[1].text().as_deref(), Some("Hi there!"));
        // token_count → Usage.output_tokens for assistant.
        assert_eq!(s.messages[1].usage.as_ref().unwrap().output_tokens, Some(42));
        assert_eq!(s.messages[1].extra.get("finish_reason").unwrap(), "stop");
        assert_eq!(s.model.as_deref(), Some("nous/hermes-4"));
    }

    #[test]
    fn decodes_multimodal_sentinel() {
        let conn = mk_conn(SCHEMA_V14);
        insert_session(&conn, "s1", None, None, 1000.0, None);
        let content = "\u{0}json:[{\"type\":\"text\",\"text\":\"see this\"},{\"type\":\"image_url\",\"image_url\":{\"url\":\"data:image/png;base64,AAAA\"}}]";
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('s1','user',?1,1001.0)",
            [content],
        ).unwrap();
        let s = parse_conn(&conn, &sref("s1")).unwrap();
        assert_eq!(s.messages.len(), 1);
        let blocks = &s.messages[0].content;
        assert!(matches!(&blocks[0], Block::Text { text } if text == "see this"));
        assert!(matches!(&blocks[1], Block::Image { data_ref: Some(u), .. } if u.starts_with("data:image/png")));
    }

    #[test]
    fn decodes_input_image_string_form() {
        let conn = mk_conn(SCHEMA_V14);
        insert_session(&conn, "s1", None, None, 1000.0, None);
        // input_image with image_url as a bare string (Responses API form).
        let content = "\u{0}json:[{\"type\":\"input_text\",\"text\":\"hi\"},{\"type\":\"input_image\",\"image_url\":\"data:image/jpeg;base64,ZZZ\"}]";
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('s1','user',?1,1001.0)",
            [content],
        ).unwrap();
        let s = parse_conn(&conn, &sref("s1")).unwrap();
        let blocks = &s.messages[0].content;
        assert!(matches!(&blocks[0], Block::Text { text } if text == "hi"));
        assert!(matches!(&blocks[1], Block::Image { data_ref: Some(u), .. } if u.starts_with("data:image/jpeg")));
    }

    #[test]
    fn decodes_dict_shaped_content() {
        let conn = mk_conn(SCHEMA_V14);
        insert_session(&conn, "s1", None, None, 1000.0, None);
        let content = "\u{0}json:{\"parts\":[{\"text\":\"hi\"}]}";
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp) VALUES ('s1','user',?1,1001.0)",
            [content],
        ).unwrap();
        let s = parse_conn(&conn, &sref("s1")).unwrap();
        assert_eq!(s.messages[0].text().as_deref(), Some("hi"));
    }

    #[test]
    fn parses_tool_call_and_result() {
        let conn = mk_conn(SCHEMA_V14);
        insert_session(&conn, "s1", None, None, 1000.0, None);
        let tc = "[{\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"web_search\",\"arguments\":\"{\\\"q\\\":\\\"x\\\"}\"}}]";
        conn.execute(
            "INSERT INTO messages (session_id, role, content, tool_calls, timestamp) VALUES ('s1','assistant','',?1,1001.0)",
            [tc],
        ).unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, tool_call_id, tool_name, timestamp) \
             VALUES ('s1','tool','{\"ok\":true}','c1','web_search',1002.0)",
            [],
        ).unwrap();
        let s = parse_conn(&conn, &sref("s1")).unwrap();
        assert_eq!(s.messages.len(), 2);
        let tu = &s.messages[0].content[0];
        assert!(matches!(tu, Block::ToolUse { name, id, .. } if name == "web_search" && id == "c1"));
        if let Block::ToolUse { input, .. } = tu {
            assert_eq!(input.get("q").and_then(Value::as_str), Some("x"));
        }
        let tr = &s.messages[1].content[0];
        assert!(matches!(tr, Block::ToolResult { tool_use_id, .. } if tool_use_id == "c1"));
        assert_eq!(s.messages[1].extra.get("tool_name").unwrap(), "web_search");
    }

    #[test]
    fn maps_all_reasoning_variants() {
        let conn = mk_conn(SCHEMA_V14);
        insert_session(&conn, "s1", None, None, 1000.0, None);
        let details = "[{\"type\":\"reasoning.summary\",\"summary\":\"summarised\"},{\"type\":\"reasoning.encrypted_content\",\"encrypted_content\":\"ENC123\"}]";
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp, reasoning, reasoning_content, reasoning_details) \
             VALUES ('s1','assistant','answer',1001.0,'short summary','native scratchpad',?1)",
            [details],
        ).unwrap();
        let s = parse_conn(&conn, &sref("s1")).unwrap();
        let think = s.messages[0].content.iter().find_map(|b| match b {
            Block::Thinking { text, encrypted, .. } => Some((text.clone(), encrypted.clone())),
            _ => None,
        }).unwrap();
        assert!(think.0.contains("short summary"));
        assert!(think.0.contains("native scratchpad"));
        assert!(think.0.contains("summarised"));
        assert_eq!(think.1.as_deref(), Some("ENC123"));
        // structured blob preserved in extra
        assert!(s.messages[0].extra.contains_key("reasoning_details"));
    }

    #[test]
    fn reasoning_content_not_duplicated_when_equal() {
        let conn = mk_conn(SCHEMA_V14);
        insert_session(&conn, "s1", None, None, 1000.0, None);
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp, reasoning, reasoning_content) \
             VALUES ('s1','assistant','a',1001.0,'same','same')",
            [],
        ).unwrap();
        let s = parse_conn(&conn, &sref("s1")).unwrap();
        let text = s.messages[0].content.iter().find_map(|b| match b {
            Block::Thinking { text, .. } => Some(text.clone()),
            _ => None,
        }).unwrap();
        assert_eq!(text, "same");
    }

    #[test]
    fn maps_codex_items() {
        let conn = mk_conn(SCHEMA_V14);
        insert_session(&conn, "s1", None, None, 1000.0, None);
        let cri = "[{\"type\":\"reasoning\",\"id\":\"rs_a\",\"encrypted_content\":\"BLOB\"}]";
        let cmi = "[{\"type\":\"message\",\"phase\":\"final_answer\",\"content\":[{\"type\":\"output_text\",\"text\":\"Done\"}]}]";
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp, codex_reasoning_items, codex_message_items) \
             VALUES ('s1','assistant','Done',1001.0,?1,?2)",
            [cri, cmi],
        ).unwrap();
        let s = parse_conn(&conn, &sref("s1")).unwrap();
        let enc = s.messages[0].content.iter().find_map(|b| match b {
            Block::Thinking { encrypted, .. } => encrypted.clone(),
            _ => None,
        });
        assert_eq!(enc.as_deref(), Some("BLOB"));
        assert!(s.messages[0].extra.contains_key("codex_reasoning_items"));
        assert!(s.messages[0].extra.contains_key("codex_message_items"));
    }

    #[test]
    fn platform_message_id_and_observed() {
        let conn = mk_conn(SCHEMA_V14);
        insert_session(&conn, "s1", None, None, 1000.0, None);
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp, platform_message_id, observed) \
             VALUES ('s1','user','hi',1001.0,'abc-123',1)",
            [],
        ).unwrap();
        let s = parse_conn(&conn, &sref("s1")).unwrap();
        assert_eq!(s.messages[0].extra.get("platform_message_id").unwrap(), "abc-123");
        assert_eq!(s.messages[0].extra.get("observed").unwrap(), &Value::Bool(true));
    }

    #[test]
    fn walks_compression_chain_and_dedups_replayed_user() {
        let conn = mk_conn(SCHEMA_V14);
        // root ended via compression at t=2000; child created after, replays the last user msg.
        insert_session(&conn, "root", None, Some("compression"), 1000.0, Some(2000.0));
        insert_session(&conn, "child", Some("root"), None, 2001.0, None);
        conn.execute("INSERT INTO messages (session_id, role, content, timestamp) VALUES ('root','user','first prompt',1001.0)", []).unwrap();
        conn.execute("INSERT INTO messages (session_id, role, content, timestamp) VALUES ('root','assistant','first answer',1002.0)", []).unwrap();
        conn.execute("INSERT INTO messages (session_id, role, content, timestamp) VALUES ('root','user','second prompt',1003.0)", []).unwrap();
        // child replays 'second prompt' then continues.
        conn.execute("INSERT INTO messages (session_id, role, content, timestamp) VALUES ('child','user','second prompt',2002.0)", []).unwrap();
        conn.execute("INSERT INTO messages (session_id, role, content, timestamp) VALUES ('child','assistant','second answer',2003.0)", []).unwrap();

        // Parsing the CHILD should walk up to the root and merge, deduping the replayed user msg.
        let s = parse_conn(&conn, &sref("child")).unwrap();
        let texts: Vec<String> = s.messages.iter().filter_map(|m| m.text()).collect();
        assert_eq!(texts, vec!["first prompt", "first answer", "second prompt", "second answer"]);
    }

    #[test]
    fn old_schema_missing_columns_still_parses() {
        let conn = mk_conn(SCHEMA_OLD);
        conn.execute(
            "INSERT INTO sessions (id, source, model, started_at, title) VALUES ('s1','cli','m',1000.0,'T')",
            [],
        ).unwrap();
        conn.execute("INSERT INTO messages (session_id, role, content, timestamp) VALUES ('s1','user','hello old',1001.0)", []).unwrap();
        conn.execute("INSERT INTO messages (session_id, role, content, tool_calls, timestamp) VALUES ('s1','assistant','',?1,1002.0)",
            ["[{\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]"]).unwrap();
        // Must not error despite missing reasoning/token/codex/parent columns.
        let s = parse_conn(&conn, &sref("s1")).unwrap();
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[0].text().as_deref(), Some("hello old"));
        assert!(matches!(&s.messages[1].content[0], Block::ToolUse { name, .. } if name == "f"));
        // discover also works on the old schema.
        let refs = discover_conn(&conn, &PathBuf::from(":memory:")).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].id, "s1");
    }

    /// Branch-don't-fork: the adapter must discover sessions in BOTH the top-level `state.db` AND
    /// every `profiles/<name>/state.db`. Regression guard for the bug where `Hermes::new()` bound a
    /// single `<home>/state.db` and left the active profile's sessions (2782 on the real install)
    /// invisible. Synthesizes a minimal `~/.hermes`-shaped tree under a temp `HERMES_HOME`.
    #[test]
    fn discovers_top_level_and_profile_dbs() {
        // Mutating process env (HERMES_HOME) is not thread-safe; serialize against the other
        // env-touching tests in this binary.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();

        let home = std::env::temp_dir().join(format!("cv-hermes-profiles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("profiles/hermes-x")).unwrap();
        // A pre-update snapshot dir at a DEEPER level must NOT be picked up (depth-1 read_dir only).
        std::fs::create_dir_all(home.join("profiles/state-snapshots/old")).unwrap();

        // helper: create a state.db with one named session.
        let mk_db = |path: &std::path::Path, sid: &str| {
            let conn = Connection::open(path).unwrap();
            conn.execute_batch(SCHEMA_V14).unwrap();
            conn.execute(
                "INSERT INTO sessions (id, source, model, started_at, message_count, title) \
                 VALUES (?1,'cli','nous/hermes-4',1000.0,1,?2)",
                rusqlite::params![sid, sid],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO messages (session_id, role, content, timestamp) VALUES (?1,'user','hi',1001.0)",
                [sid],
            )
            .unwrap();
        };
        mk_db(&home.join("state.db"), "top-session");
        mk_db(&home.join("profiles/hermes-x/state.db"), "profile-session");
        // A snapshot DB deeper than depth-1 — must stay hidden.
        mk_db(&home.join("profiles/state-snapshots/old/state.db"), "snapshot-session");

        // Point the adapter at our synthetic home and discover.
        std::env::set_var("HERMES_HOME", &home);
        let hermes = Hermes::new();
        // Two readable DBs: top-level + the one profile (snapshot is depth-2, skipped).
        assert_eq!(hermes.dbs.len(), 2, "should bind top-level + profile DB only");
        let refs = hermes.discover().unwrap();
        std::env::remove_var("HERMES_HOME");

        let ids: HashSet<&str> = refs.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains("top-session"), "top-level session must be discovered");
        assert!(ids.contains("profile-session"), "profile session must be discovered (the bug)");
        assert!(!ids.contains("snapshot-session"), "depth-2 snapshot DB must NOT be discovered");

        // parse() must route by the ref's own DB path, so the profile session parses from its DB.
        let pref = refs.iter().find(|r| r.id == "profile-session").unwrap();
        let s = hermes.parse(pref).unwrap();
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0].text().as_deref(), Some("hi"));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn discover_orders_and_counts() {
        let conn = mk_conn(SCHEMA_V14);
        conn.execute("INSERT INTO sessions (id, source, started_at, message_count, title) VALUES ('a','cli',1000.0,3,'A')", []).unwrap();
        conn.execute("INSERT INTO sessions (id, source, started_at, message_count, title) VALUES ('b','cli',2000.0,5,'B')", []).unwrap();
        let refs = discover_conn(&conn, &PathBuf::from(":memory:")).unwrap();
        assert_eq!(refs.len(), 2);
        // ordered by started_at DESC
        assert_eq!(refs[0].id, "b");
        assert_eq!(refs[0].message_count, 5);
        assert_eq!(refs[1].id, "a");
    }
}
