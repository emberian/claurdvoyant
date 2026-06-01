//! Goose adapter (Block) — modern SQLite store + legacy `.jsonl` files. **sqlite feature only.**
//!
//! Goose keeps all of a user's sessions under one *data dir*:
//!   * **Linux:**   `~/.local/share/goose/sessions/`            (XDG; `$XDG_DATA_HOME/goose`)
//!   * **macOS:**   `~/Library/Application Support/Block.block.goose/sessions/`
//!   * **Windows:** `%APPDATA%\Block\Block\goose\data\sessions\`
//!   * `$GOOSE_PATH_ROOT/data/sessions/` overrides all of the above (test hook), and we also honour
//!     `$XDG_DATA_HOME` directly.
//! The dir contains a modern `sessions.db` (SQLite, since the v1.10-era rewrite) and, for older
//! installs, legacy per-session `<name>.jsonl` files. New Goose imports the legacy files into the DB
//! on first run, but we read whatever is on disk: we parse the DB *and* any `.jsonl` not shadowed by
//! a same-id DB row.
//!
//! ## Modern schema (from `goose/src/session/session_manager.rs`)
//! ```sql
//! CREATE TABLE sessions (
//!   id TEXT PRIMARY KEY, name TEXT, description TEXT, working_dir TEXT NOT NULL,
//!   created_at TIMESTAMP, updated_at TIMESTAMP, total_tokens INTEGER, input_tokens INTEGER,
//!   output_tokens INTEGER, provider_name TEXT, model_config_json TEXT, session_type TEXT, … );
//! CREATE TABLE messages (
//!   id INTEGER PK, message_id TEXT, session_id TEXT, role TEXT, content_json TEXT NOT NULL,
//!   created_timestamp INTEGER NOT NULL, timestamp TIMESTAMP, tokens INTEGER, metadata_json TEXT );
//! ```
//! `created_at`/`updated_at` are stored as SQLite `TIMESTAMP` text (`YYYY-MM-DD HH:MM:SS`, UTC).
//! `messages.created_timestamp` is unix **seconds**. Schema columns have accreted over versions, so
//! we probe `PRAGMA table_info` and only SELECT columns that exist (older DBs lack `name`,
//! `provider_name`, `model_config_json`, `message_id`, `tokens`, …).
//!
//! ## content_json (one row's content = a JSON array of `MessageContent`, tagged `type`, camelCase)
//! ```jsonc
//! [{"type":"text","text":"…"},
//!  {"type":"image","data":"…","mimeType":"…"},
//!  {"type":"thinking","thinking":"…","signature":"…"},
//!  {"type":"redactedThinking","data":"…"},
//!  {"type":"toolRequest","id":"…",
//!     "toolCall":{"status":"success","value":{"name":"…","arguments":{…}}}},
//!  {"type":"toolResponse","id":"…",
//!     "toolResult":{"status":"success","value":{"content":[{"type":"text","text":"…"}],"isError":false}}}]
//! ```
//! Goose uses MCP-style tools: a `toolRequest` is the assistant's call (rmcp `CallToolRequestParams`
//! = `{name, arguments}`); a `toolResponse` is the result (rmcp `CallToolResult` =
//! `{content:[Content…], isError}`). On error the inner wrapper is `{"status":"error","error":"…"}`.
//! Tool requests/responses ride *inside* an assistant/user message's content array (Goose only has
//! user/assistant roles), so a tool result lives on a `user` message — we re-classify a message that
//! is purely tool responses to [`Role::Tool`] for IR fidelity.
//!
//! ## Legacy `.jsonl`
//! First line is a metadata header (`{description, working_dir, created_at, updated_at, …}`); every
//! subsequent line is one message `{id, role, created, content:[…]}` using the same `MessageContent`
//! shape as `content_json`.
//!
//! ## Fidelity caveats
//! * Goose records only user/assistant roles in the DB; there is no system message and tool turns are
//!   folded into user/assistant content (we recover [`Role::Tool`] heuristically). The MCP tool name
//!   lives on the *request*, not the *response*, so a `toolResponse` only carries `tool_name` when we
//!   can pair it to a prior request by id (we do this within a session).
//! * `model` is reconstructed from `provider_name` + `model_config_json.model_name` when present.

use super::Adapter;
use crate::ir::*;
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct Goose {
    /// The `sessions/` dir, if it exists on disk.
    dir: Option<PathBuf>,
}

impl Goose {
    pub fn new() -> Self {
        Goose { dir: sessions_dir() }
    }

    fn db_path(&self) -> Option<PathBuf> {
        self.dir
            .as_ref()
            .map(|d| d.join("sessions.db"))
            .filter(|p| p.exists())
    }
}

/// Resolve Goose's `sessions/` directory across platforms / env overrides. Returns it only if it
/// exists (so discovery degrades to empty rather than erroring on machines without Goose).
fn sessions_dir() -> Option<PathBuf> {
    // 1. Explicit test/CI override used by Goose itself: `$GOOSE_PATH_ROOT/data`.
    if let Some(root) = std::env::var_os("GOOSE_PATH_ROOT") {
        let p = PathBuf::from(root).join("data").join("sessions");
        if p.exists() {
            return Some(p);
        }
    }
    let home = dirs::home_dir();
    // 2. XDG (honour an explicit $XDG_DATA_HOME even on macOS for portability).
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        let p = PathBuf::from(xdg).join("goose").join("sessions");
        if p.exists() {
            return Some(p);
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(h) = &home {
        // macOS Apple-strategy (etcetera bundle_id = "Block.block.goose").
        candidates.push(
            h.join("Library/Application Support/Block.block.goose/sessions"),
        );
        // Linux XDG default.
        candidates.push(h.join(".local/share/goose/sessions"));
    }
    // Windows: %APPDATA%\Block\Block\goose\data\sessions (etcetera Windows strategy).
    if let Some(appdata) = std::env::var_os("APPDATA") {
        candidates.push(
            PathBuf::from(appdata)
                .join("Block")
                .join("Block")
                .join("goose")
                .join("data")
                .join("sessions"),
        );
    }
    candidates.into_iter().find(|p| p.exists())
}

fn open_ro(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", path.display()))
}

impl Default for Goose {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for Goose {
    fn harness(&self) -> Harness {
        Harness::Goose
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.dir.clone()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(dir) = &self.dir else {
            return Ok(vec![]);
        };
        let mut out = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        // Modern SQLite store.
        if let Some(db) = self.db_path() {
            if let Ok(conn) = open_ro(&db) {
                if let Ok(refs) = discover_db(&conn, &db) {
                    for r in refs {
                        seen_ids.insert(r.id.clone());
                        out.push(r);
                    }
                }
            }
        }

        // Legacy `.jsonl` files not already represented by a DB row of the same id.
        for (name, path) in list_legacy(dir) {
            if seen_ids.contains(&name) {
                continue;
            }
            if let Some(r) = discover_legacy(&name, &path) {
                out.push(r);
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        crate::stream::collect(self, r)
    }

    fn stream(
        &self,
        r: &SessionRef,
        _opts: &ParseOptions,
        sink: &mut dyn MessageSink,
    ) -> Result<Session> {
        // A legacy session's `path` is the `.jsonl`; a DB session's `path` is `sessions.db`.
        if r.path.extension().is_some_and(|e| e == "jsonl") {
            return stream_legacy(&r.id, &r.path, sink);
        }
        let conn = open_ro(&r.path)?;
        stream_db(&conn, r, sink)
    }
}

// ---------------------------------------------------------------------------
// Modern SQLite store
// ---------------------------------------------------------------------------

/// Columns present on a table (for schema-drift tolerance).
fn columns(conn: &Connection, table: &str) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    if let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) {
        if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(1)) {
            for n in rows.flatten() {
                set.insert(n);
            }
        }
    }
    set
}

fn discover_db(conn: &Connection, db: &Path) -> Result<Vec<SessionRef>> {
    let cols = columns(conn, "sessions");
    let has = |c: &str| cols.contains(c);
    // Prefer `description` (the human title), fall back to `name`.
    let title_expr = match (has("description"), has("name")) {
        (true, _) => "description",
        (false, true) => "name",
        _ => "NULL",
    };
    let working_dir = if has("working_dir") { "working_dir" } else { "NULL" };
    let created = if has("created_at") { "created_at" } else { "NULL" };
    let updated = if has("updated_at") { "updated_at" } else { "NULL" };
    // Order by updated_at when present; the column always existed in the modern schema.
    let order = if has("updated_at") {
        "ORDER BY updated_at DESC"
    } else {
        ""
    };
    let sql = format!(
        "SELECT id, {title_expr}, {working_dir}, {created}, {updated} FROM sessions {order}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let title: Option<String> = row.get(1).ok().flatten();
        let wd: Option<String> = row.get(2).ok().flatten();
        let created: Option<String> = row.get(3).ok().flatten();
        let updated: Option<String> = row.get(4).ok().flatten();
        Ok((id, title, wd, created, updated))
    })?;
    let mut out = Vec::new();
    for row in rows.flatten() {
        let (id, title, wd, created, updated) = row;
        // message_count: cheap COUNT (a `message_count` column is not in the modern schema).
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
                [&id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        out.push(SessionRef {
            id,
            harness: Harness::Goose,
            path: db.to_path_buf(),
            cwd: wd.filter(|s| !s.is_empty()).map(PathBuf::from),
            title: title.filter(|s| !s.trim().is_empty()).map(|t| crate::ir::truncate(&t, 80)),
            created_at: created.as_deref().and_then(parse_ts_text),
            updated_at: updated.as_deref().and_then(parse_ts_text),
            message_count: count.max(0) as usize,
        });
    }
    Ok(out)
}

fn stream_db(conn: &Connection, r: &SessionRef, sink: &mut dyn MessageSink) -> Result<Session> {
    let cols = columns(conn, "sessions");
    let has = |c: &str| cols.contains(c);
    let title_expr = match (has("description"), has("name")) {
        (true, _) => "description",
        (false, true) => "name",
        _ => "NULL",
    };
    let sel = format!(
        "SELECT {title}, {wd}, {created}, {updated}, {provider}, {model_cfg} \
         FROM sessions WHERE id = ?1",
        title = title_expr,
        wd = if has("working_dir") { "working_dir" } else { "NULL" },
        created = if has("created_at") { "created_at" } else { "NULL" },
        updated = if has("updated_at") { "updated_at" } else { "NULL" },
        provider = if has("provider_name") { "provider_name" } else { "NULL" },
        model_cfg = if has("model_config_json") { "model_config_json" } else { "NULL" },
    );
    let (title, wd, created, updated, provider, model_cfg): (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(&sel, [&r.id], |row| {
            Ok((
                row.get(0).ok().flatten(),
                row.get(1).ok().flatten(),
                row.get(2).ok().flatten(),
                row.get(3).ok().flatten(),
                row.get(4).ok().flatten(),
                row.get(5).ok().flatten(),
            ))
        })
        .unwrap_or((None, None, None, None, None, None));

    let model = reconstruct_model(provider.as_deref(), model_cfg.as_deref());

    let s = Session {
        id: r.id.clone(),
        harness: Harness::Goose,
        cwd: wd
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| r.cwd.clone()),
        title: title
            .filter(|t| !t.trim().is_empty())
            .or_else(|| r.title.clone()),
        created_at: created.as_deref().and_then(parse_ts_text).or(r.created_at),
        updated_at: updated.as_deref().and_then(parse_ts_text).or(r.updated_at),
        model,
        git: None,
        messages: Vec::new(),
        source_path: Some(r.path.clone()),
        extra: serde_json::Map::new(),
    };
    // All session metadata is known up front, so hand it to the sink before the body.
    sink.meta(&s);

    let mcols = columns(conn, "messages");
    let has_msg = |c: &str| mcols.contains(c);
    let msg_id = if has_msg("message_id") { "message_id" } else { "NULL" };
    let tokens = if has_msg("tokens") { "tokens" } else { "NULL" };
    let sql = format!(
        "SELECT role, content_json, created_timestamp, {msg_id}, {tokens} \
         FROM messages WHERE session_id = ?1 ORDER BY created_timestamp, id"
    );
    let mut stmt = conn.prepare(&sql)?;
    // Stream rows lazily: build one `DbMsg` -> one `Message`, emit it, drop it before the next row,
    // so a large Goose session never fully materializes (peak = one message's content).
    let mut rows = stmt.query_map([&r.id], |row| {
        Ok(DbMsg {
            role: row.get::<_, String>(0).unwrap_or_default(),
            content_json: row.get::<_, Option<String>>(1).ok().flatten().unwrap_or_default(),
            created: row.get::<_, Option<i64>>(2).ok().flatten(),
            id: row.get::<_, Option<String>>(3).ok().flatten(),
            tokens: row.get::<_, Option<i64>>(4).ok().flatten(),
        })
    })?;

    // Track tool-request names so we can label later tool responses (forward-only state).
    let mut tool_names: HashMap<String, String> = HashMap::new();
    while let Some(row) = rows.next() {
        let Ok(row) = row else { continue };
        if let Some(m) = row.into_message(&mut tool_names) {
            if sink.message(m) == Flow::Stop {
                break;
            }
        }
    }
    Ok(s)
}

struct DbMsg {
    role: String,
    content_json: String,
    created: Option<i64>,
    id: Option<String>,
    tokens: Option<i64>,
}

impl DbMsg {
    fn into_message(self, tool_names: &mut HashMap<String, String>) -> Option<Message> {
        let content: Value = serde_json::from_str(&self.content_json).unwrap_or(Value::Null);
        let blocks = content_to_blocks(&content, tool_names);
        build_message(
            &self.role,
            self.id,
            self.created.and_then(secs_to_dt),
            self.tokens,
            blocks,
        )
    }
}

/// Reconstruct a `provider/model` string from the session's provider + model_config_json.
fn reconstruct_model(provider: Option<&str>, model_cfg: Option<&str>) -> Option<String> {
    let model_name = model_cfg
        .and_then(|j| serde_json::from_str::<Value>(j).ok())
        .and_then(|v| v.get("model_name").and_then(Value::as_str).map(str::to_string));
    match (provider.filter(|p| !p.is_empty()), model_name) {
        (Some(p), Some(m)) => Some(format!("{p}/{m}")),
        (None, Some(m)) => Some(m),
        (Some(p), None) => Some(p.to_string()),
        (None, None) => None,
    }
}

// ---------------------------------------------------------------------------
// Legacy `.jsonl`
// ---------------------------------------------------------------------------

fn list_legacy(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "jsonl") {
            if let Some(stem) = path.file_stem() {
                out.push((stem.to_string_lossy().into_owned(), path));
            }
        }
    }
    out
}

/// Read just the header line of a legacy `.jsonl` for a cheap [`SessionRef`].
fn discover_legacy(name: &str, path: &Path) -> Option<SessionRef> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let header: Value = lines
        .next()
        .and_then(|l| serde_json::from_str(l).ok())
        .unwrap_or(Value::Null);
    let count = lines.filter(|l| !l.trim().is_empty()).count();
    let title = header
        .get("description")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| crate::ir::truncate(s, 80));
    let cwd = header
        .get("working_dir")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    Some(SessionRef {
        id: name.to_string(),
        harness: Harness::Goose,
        path: path.to_path_buf(),
        cwd,
        title,
        created_at: header.get("created_at").and_then(parse_ts_value),
        updated_at: header.get("updated_at").and_then(parse_ts_value),
        message_count: count,
    })
}

fn stream_legacy(name: &str, path: &Path, sink: &mut dyn MessageSink) -> Result<Session> {
    use std::io::{BufRead, BufReader};
    // Stream line-by-line: a legacy transcript can be large, and `read_to_string` would
    // resident-spike the whole file. `BufReader::lines()` keeps peak at O(largest line) and handing
    // each message to the sink keeps it at O(largest message).
    let file = std::fs::File::open(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let mut lines = BufReader::new(file).lines();

    // First line is the metadata header.
    let header: Value = lines
        .next()
        .and_then(|l| l.ok())
        .and_then(|l| serde_json::from_str(&l).ok())
        .unwrap_or(Value::Null);

    let s = Session {
        id: name.to_string(),
        harness: Harness::Goose,
        cwd: header
            .get("working_dir")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
        title: header
            .get("description")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string),
        created_at: header.get("created_at").and_then(parse_ts_value),
        updated_at: header.get("updated_at").and_then(parse_ts_value),
        model: None,
        git: None,
        messages: Vec::new(),
        source_path: Some(path.to_path_buf()),
        extra: serde_json::Map::new(),
    };
    sink.meta(&s);

    let mut tool_names: HashMap<String, String> = HashMap::new();
    for line in lines {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let role = v.get("role").and_then(Value::as_str).unwrap_or("user");
        let id = v.get("id").and_then(Value::as_str).map(str::to_string);
        let ts = v.get("created").and_then(Value::as_i64).and_then(secs_to_dt);
        let blocks = v
            .get("content")
            .map(|c| content_to_blocks(c, &mut tool_names))
            .unwrap_or_default();
        if let Some(m) = build_message(role, id, ts, None, blocks) {
            if sink.message(m) == Flow::Stop {
                break;
            }
        }
    }
    Ok(s)
}

// ---------------------------------------------------------------------------
// Shared content decoding (modern content_json == legacy `content` array)
// ---------------------------------------------------------------------------

/// Turn a `Vec<MessageContent>` JSON value into IR blocks, recording tool-request names.
fn content_to_blocks(content: &Value, tool_names: &mut HashMap<String, String>) -> Vec<Block> {
    let Some(items) = content.as_array() else {
        // A bare string content (defensive; Goose stores arrays) → one Text block.
        if let Some(t) = content.as_str() {
            if !t.is_empty() {
                return vec![Block::Text { text: t.to_string().into() }];
            }
        }
        return vec![];
    };
    let mut out = Vec::new();
    for item in items {
        if let Some(b) = item_to_block(item, tool_names) {
            out.push(b);
        }
    }
    out
}

fn item_to_block(item: &Value, tool_names: &mut HashMap<String, String>) -> Option<Block> {
    match item.get("type").and_then(Value::as_str)? {
        "text" => {
            let text = item.get("text").and_then(Value::as_str)?.to_string();
            Some(Block::Text { text: text.into() })
        }
        "image" => Some(Block::Image {
            media_type: item.get("mimeType").and_then(Value::as_str).map(str::to_string),
            data_ref: item
                .get("data")
                .and_then(Value::as_str)
                .map(|d| crate::ir::truncate(d, 120)),
        }),
        "thinking" => Some(Block::Thinking {
            text: item.get("thinking").and_then(Value::as_str).unwrap_or("").to_string().into(),
            signature: item
                .get("signature")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            encrypted: None,
            redacted: false,
        }),
        "redactedThinking" => Some(Block::Thinking {
            text: String::new().into(),
            signature: None,
            encrypted: item.get("data").and_then(Value::as_str).map(str::to_string),
            redacted: true,
        }),
        "toolRequest" | "frontendToolRequest" => {
            let id = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let call = item.get("toolCall")?;
            // Error wrapper: {"status":"error","error":"…"} → surface as a degenerate ToolUse.
            let (name, input) = match call.get("status").and_then(Value::as_str) {
                Some("error") => (
                    String::new(),
                    serde_json::json!({
                        "error": call.get("error").and_then(Value::as_str).unwrap_or("")
                    }),
                ),
                _ => {
                    let value = call.get("value")?;
                    let name = value.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                    let input = value.get("arguments").cloned().unwrap_or(Value::Null);
                    (name, input)
                }
            };
            if !id.is_empty() && !name.is_empty() {
                tool_names.insert(id.clone(), name.clone());
            }
            Some(Block::ToolUse { id, name, input })
        }
        "toolResponse" => {
            let id = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let result = item.get("toolResult")?;
            let tool_name = tool_names.get(&id).cloned();
            let (content, is_error) = match result.get("status").and_then(Value::as_str) {
                Some("error") => (
                    result.get("error").and_then(Value::as_str).unwrap_or("").to_string(),
                    true,
                ),
                _ => {
                    let value = result.get("value");
                    let is_error = value
                        .and_then(|v| v.get("isError"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let text = value
                        .and_then(|v| v.get("content"))
                        .map(flatten_result_content)
                        .unwrap_or_default();
                    (text, is_error)
                }
            };
            Some(Block::ToolResult {
                tool_use_id: id,
                content: content.into(),
                is_error,
                tool_name,
                status: None,
                details: None,
            })
        }
        // toolConfirmationRequest / actionRequired / systemNotification: no first-class IR home;
        // skip (they're ephemeral UI/control content, not transcript-bearing).
        _ => None,
    }
}

/// Flatten an rmcp `CallToolResult.content` array (`[{type:"text",text}|{type:"image",…}]`) to text.
fn flatten_result_content(content: &Value) -> String {
    let Some(items) = content.as_array() else {
        return content.as_str().unwrap_or_default().to_string();
    };
    let mut parts = Vec::new();
    for it in items {
        match it.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = it.get("text").and_then(Value::as_str) {
                    parts.push(t.to_string());
                }
            }
            Some("image") => {
                let mime = it.get("mimeType").and_then(Value::as_str).unwrap_or("image");
                parts.push(format!("[image: {mime}]"));
            }
            _ => {}
        }
    }
    parts.join("\n")
}

/// Build a [`Message`], re-classifying a pure-tool-response `user` message to [`Role::Tool`].
fn build_message(
    role: &str,
    id: Option<String>,
    ts: Option<DateTime<Utc>>,
    tokens: Option<i64>,
    blocks: Vec<Block>,
) -> Option<Message> {
    if blocks.is_empty() {
        return None;
    }
    let mut ir_role = match role {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "system" => Role::System,
        "tool" => Role::Tool,
        _ => Role::User,
    };
    // Goose stores tool results on `user` messages; if a message is *only* tool results, surface it
    // as a Tool turn so conversions re-encode it correctly.
    if ir_role == Role::User
        && !blocks.is_empty()
        && blocks.iter().all(|b| matches!(b, Block::ToolResult { .. }))
    {
        ir_role = Role::Tool;
    }
    let mut m = Message::new(ir_role);
    m.id = id;
    m.timestamp = ts;
    if let Some(t) = tokens {
        if t > 0 {
            let mut u = Usage::default();
            if ir_role == Role::Assistant {
                u.output_tokens = Some(t as u64);
            } else {
                u.input_tokens = Some(t as u64);
            }
            m.usage = Some(u);
        }
    }
    m.content = blocks;
    Some(m)
}

// ---------------------------------------------------------------------------
// Timestamp helpers
// ---------------------------------------------------------------------------

fn secs_to_dt(s: i64) -> Option<DateTime<Utc>> {
    if s <= 0 {
        return None;
    }
    Utc.timestamp_opt(s, 0).single()
}

/// Parse a JSON timestamp value: an RFC3339 string, a SQLite `TIMESTAMP` string, or unix seconds.
fn parse_ts_value(v: &Value) -> Option<DateTime<Utc>> {
    if let Some(s) = v.as_str() {
        return parse_ts_text(s);
    }
    if let Some(n) = v.as_i64() {
        return secs_to_dt(n);
    }
    if let Some(f) = v.as_f64() {
        return secs_to_dt(f as i64);
    }
    None
}

/// Parse a textual timestamp: RFC3339 (`2024-01-01T12:00:00Z`) or SQLite (`2024-01-01 12:00:00`).
fn parse_ts_text(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    // SQLite `CURRENT_TIMESTAMP` form (space separator, UTC, no offset).
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(Utc.from_utc_datetime(&ndt));
        }
    }
    None
}

/// Whole-`Session` convenience over [`stream_db`] for tests: stream into a [`CollectSink`].
#[cfg(test)]
fn parse_db(conn: &Connection, r: &SessionRef) -> Result<Session> {
    let mut sink = crate::stream::CollectSink::default();
    let mut s = stream_db(conn, r, &mut sink)?;
    s.messages = sink.messages;
    Ok(s)
}

/// Whole-`Session` convenience over [`stream_legacy`] for tests.
#[cfg(test)]
fn parse_legacy(name: &str, path: &Path) -> Result<Session> {
    let mut sink = crate::stream::CollectSink::default();
    let mut s = stream_legacy(name, path, &mut sink)?;
    s.messages = sink.messages;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: &str = "
        CREATE TABLE sessions (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL DEFAULT '',
            description TEXT NOT NULL DEFAULT '',
            working_dir TEXT NOT NULL,
            created_at TIMESTAMP,
            updated_at TIMESTAMP,
            total_tokens INTEGER,
            provider_name TEXT,
            model_config_json TEXT,
            session_type TEXT NOT NULL DEFAULT 'user'
        );
        CREATE TABLE messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            message_id TEXT,
            session_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content_json TEXT NOT NULL,
            created_timestamp INTEGER NOT NULL,
            timestamp TIMESTAMP,
            tokens INTEGER,
            metadata_json TEXT
        );
    ";

    /// An older schema lacking `description`, `provider_name`, `model_config_json`, `message_id`,
    /// `tokens` — exercises the column-probing degradation path.
    const SCHEMA_OLD: &str = "
        CREATE TABLE sessions (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL DEFAULT '',
            working_dir TEXT NOT NULL,
            created_at TIMESTAMP,
            updated_at TIMESTAMP
        );
        CREATE TABLE messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content_json TEXT NOT NULL,
            created_timestamp INTEGER NOT NULL
        );
    ";

    fn mk(schema: &str) -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(schema).unwrap();
        c
    }

    fn sref(id: &str) -> SessionRef {
        SessionRef {
            id: id.into(),
            harness: Harness::Goose,
            path: PathBuf::from("sessions.db"),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        }
    }

    #[test]
    fn parses_modern_db_text_and_tools() {
        let c = mk(SCHEMA);
        c.execute(
            "INSERT INTO sessions (id, description, working_dir, created_at, updated_at, provider_name, model_config_json) \
             VALUES ('s1','Build a thing','/home/u/proj','2024-01-01 12:00:00','2024-01-01 12:05:00','anthropic','{\"model_name\":\"claude-sonnet-4\"}')",
            [],
        ).unwrap();
        // user text
        c.execute(
            "INSERT INTO messages (session_id, role, content_json, created_timestamp, tokens) \
             VALUES ('s1','user','[{\"type\":\"text\",\"text\":\"Hello\"}]',1704110400,3)",
            [],
        ).unwrap();
        // assistant: thinking + text + tool request
        let asst = r#"[{"type":"thinking","thinking":"let me think","signature":"sig"},{"type":"text","text":"Working on it"},{"type":"toolRequest","id":"call_1","toolCall":{"status":"success","value":{"name":"shell","arguments":{"command":"ls"}}}}]"#;
        c.execute(
            "INSERT INTO messages (session_id, role, content_json, created_timestamp, tokens) VALUES ('s1','assistant',?1,1704110401,42)",
            [asst],
        ).unwrap();
        // tool response carried on a user message
        let toolresp = r#"[{"type":"toolResponse","id":"call_1","toolResult":{"status":"success","value":{"content":[{"type":"text","text":"file1\nfile2"}],"isError":false}}}]"#;
        c.execute(
            "INSERT INTO messages (session_id, role, content_json, created_timestamp) VALUES ('s1','user',?1,1704110402)",
            [toolresp],
        ).unwrap();

        let s = parse_db(&c, &sref("s1")).unwrap();
        assert_eq!(s.title.as_deref(), Some("Build a thing"));
        assert_eq!(s.cwd.as_deref().map(|p| p.to_str().unwrap()), Some("/home/u/proj"));
        assert_eq!(s.model.as_deref(), Some("anthropic/claude-sonnet-4"));
        assert!(s.created_at.is_some());
        assert_eq!(s.messages.len(), 3);

        // user
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[0].text().as_deref(), Some("Hello"));
        assert_eq!(s.messages[0].usage.as_ref().unwrap().input_tokens, Some(3));

        // assistant: thinking, text, tooluse
        let a = &s.messages[1];
        assert_eq!(a.role, Role::Assistant);
        assert!(matches!(&a.content[0], Block::Thinking { text, signature, .. } if text == "let me think" && signature.as_deref() == Some("sig")));
        assert!(matches!(&a.content[1], Block::Text { text } if text == "Working on it"));
        assert!(matches!(&a.content[2], Block::ToolUse { name, id, input } if name == "shell" && id == "call_1" && input.get("command").and_then(Value::as_str) == Some("ls")));
        assert_eq!(a.usage.as_ref().unwrap().output_tokens, Some(42));

        // tool response reclassified to Role::Tool, paired name recovered
        let t = &s.messages[2];
        assert_eq!(t.role, Role::Tool);
        assert!(matches!(&t.content[0], Block::ToolResult { tool_use_id, content, tool_name, is_error, .. }
            if tool_use_id == "call_1" && content == "file1\nfile2" && tool_name.as_deref() == Some("shell") && !is_error));
    }

    #[test]
    fn parses_tool_error_response() {
        let c = mk(SCHEMA);
        c.execute("INSERT INTO sessions (id, working_dir) VALUES ('s1','/x')", []).unwrap();
        let resp = r#"[{"type":"toolResponse","id":"c9","toolResult":{"status":"error","error":"boom"}}]"#;
        c.execute(
            "INSERT INTO messages (session_id, role, content_json, created_timestamp) VALUES ('s1','user',?1,100)",
            [resp],
        ).unwrap();
        let s = parse_db(&c, &sref("s1")).unwrap();
        assert!(matches!(&s.messages[0].content[0], Block::ToolResult { is_error, content, .. } if *is_error && content == "boom"));
    }

    #[test]
    fn discover_db_orders_and_counts() {
        let c = mk(SCHEMA);
        c.execute("INSERT INTO sessions (id, description, working_dir, updated_at) VALUES ('a','A','/x','2024-01-01 00:00:00')", []).unwrap();
        c.execute("INSERT INTO sessions (id, description, working_dir, updated_at) VALUES ('b','B','/y','2024-02-01 00:00:00')", []).unwrap();
        c.execute("INSERT INTO messages (session_id, role, content_json, created_timestamp) VALUES ('a','user','[{\"type\":\"text\",\"text\":\"hi\"}]',1)", []).unwrap();
        c.execute("INSERT INTO messages (session_id, role, content_json, created_timestamp) VALUES ('a','assistant','[{\"type\":\"text\",\"text\":\"yo\"}]',2)", []).unwrap();
        let refs = discover_db(&c, Path::new("sessions.db")).unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].id, "b"); // updated_at DESC
        assert_eq!(refs[1].id, "a");
        assert_eq!(refs[1].message_count, 2);
        assert_eq!(refs[0].title.as_deref(), Some("B"));
    }

    #[test]
    fn old_schema_missing_columns_still_parses() {
        let c = mk(SCHEMA_OLD);
        c.execute("INSERT INTO sessions (id, name, working_dir) VALUES ('s1','My Session','/p')", []).unwrap();
        c.execute("INSERT INTO messages (session_id, role, content_json, created_timestamp) VALUES ('s1','user','[{\"type\":\"text\",\"text\":\"old\"}]',1704110400)", []).unwrap();
        let refs = discover_db(&c, Path::new("sessions.db")).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].title.as_deref(), Some("My Session")); // falls back to `name`
        let s = parse_db(&c, &sref("s1")).unwrap();
        assert_eq!(s.messages.len(), 1);
        assert_eq!(s.messages[0].text().as_deref(), Some("old"));
        assert_eq!(s.title.as_deref(), Some("My Session"));
        assert!(s.model.is_none());
    }

    #[test]
    fn parses_legacy_jsonl() {
        let dir = tempdir();
        let path = dir.join("20240101_120000.jsonl");
        let content = concat!(
            "{\"description\":\"legacy chat\",\"working_dir\":\"/home/u/old\",\"created_at\":\"2024-01-01T12:00:00Z\",\"updated_at\":\"2024-01-01T12:01:00Z\"}\n",
            "{\"id\":\"m1\",\"role\":\"user\",\"created\":1704110400,\"content\":[{\"type\":\"text\",\"text\":\"Hi\"}]}\n",
            "{\"id\":\"m2\",\"role\":\"assistant\",\"created\":1704110401,\"content\":[{\"type\":\"text\",\"text\":\"Hello!\"},{\"type\":\"toolRequest\",\"id\":\"t1\",\"toolCall\":{\"status\":\"success\",\"value\":{\"name\":\"developer__shell\",\"arguments\":{\"command\":\"pwd\"}}}}]}\n",
            "{\"id\":\"m3\",\"role\":\"user\",\"created\":1704110402,\"content\":[{\"type\":\"toolResponse\",\"id\":\"t1\",\"toolResult\":{\"status\":\"success\",\"value\":{\"content\":[{\"type\":\"text\",\"text\":\"/home/u/old\"}]}}}]}\n"
        );
        std::fs::write(&path, content).unwrap();

        // discover (header-only)
        let r = discover_legacy("20240101_120000", &path).unwrap();
        assert_eq!(r.title.as_deref(), Some("legacy chat"));
        assert_eq!(r.cwd.as_deref().map(|p| p.to_str().unwrap()), Some("/home/u/old"));
        assert_eq!(r.message_count, 3);
        assert!(r.created_at.is_some());

        // full parse
        let s = parse_legacy("20240101_120000", &path).unwrap();
        assert_eq!(s.title.as_deref(), Some("legacy chat"));
        assert_eq!(s.cwd.as_deref().map(|p| p.to_str().unwrap()), Some("/home/u/old"));
        assert_eq!(s.messages.len(), 3);
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[1].role, Role::Assistant);
        assert!(matches!(&s.messages[1].content[1], Block::ToolUse { name, .. } if name == "developer__shell"));
        // tool response reclassified + name paired
        assert_eq!(s.messages[2].role, Role::Tool);
        assert!(matches!(&s.messages[2].content[0], Block::ToolResult { tool_name, content, .. }
            if tool_name.as_deref() == Some("developer__shell") && content == "/home/u/old"));
    }

    #[test]
    fn legacy_without_metadata_header_is_tolerant() {
        // A header that's actually a message (no description) — we still read messages, no panic.
        let dir = tempdir();
        let path = dir.join("s.jsonl");
        std::fs::write(
            &path,
            "{\"role\":\"user\",\"created\":1,\"content\":[{\"type\":\"text\",\"text\":\"only msg\"}]}\n",
        )
        .unwrap();
        let s = parse_legacy("s", &path).unwrap();
        // first line consumed as header; since it has no `working_dir`/`description`, fields are None.
        assert!(s.title.is_none());
        assert_eq!(s.messages.len(), 0);
    }

    #[test]
    fn redacted_thinking_maps() {
        let c = mk(SCHEMA);
        c.execute("INSERT INTO sessions (id, working_dir) VALUES ('s1','/x')", []).unwrap();
        c.execute(
            "INSERT INTO messages (session_id, role, content_json, created_timestamp) VALUES ('s1','assistant','[{\"type\":\"redactedThinking\",\"data\":\"BLOB\"}]',1)",
            [],
        ).unwrap();
        let s = parse_db(&c, &sref("s1")).unwrap();
        assert!(matches!(&s.messages[0].content[0], Block::Thinking { redacted, encrypted, .. } if *redacted && encrypted.as_deref() == Some("BLOB")));
    }

    #[test]
    fn empty_dir_discovers_nothing() {
        let g = Goose { dir: None };
        assert!(g.discover().unwrap().is_empty());
        assert!(g.storage_root().is_none());
    }

    fn tempdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cv-goose-test-{}", std::process::id()));
        p.push(format!("{:?}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
