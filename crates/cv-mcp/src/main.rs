//! # cv-mcp — claurdvoyant as an MCP server
//!
//! A stdio [Model Context Protocol](https://modelcontextprotocol.io) server that lets a *running*
//! coding agent (Claude Code, Codex, Gemini, …) search and read the sessions of **other** agents —
//! within the same project, back through time, and across harnesses. "Let agents read other agents'
//! minds." It is a thin front-end over the [`cv_core`] library.
//!
//! ## Registering with a host
//!
//! Build it (`cargo build -p cv-mcp --release`) and point your agent at the binary. For Claude Code:
//!
//! ```text
//! claude mcp add claurdvoyant -- /absolute/path/to/cv-mcp
//! ```
//!
//! It speaks line-delimited JSON-RPC 2.0 over stdin/stdout. **stdout is the protocol channel**, so
//! all diagnostics go to stderr.
//!
//! ## Tools exposed
//!
//! - `list_sessions(harness?, cwd_contains?, limit=40)` — recent sessions, newest first.
//! - `search_sessions(query, harness?, cwd_contains?, limit=20)` — full-text substring search over
//!   parsed session content, with a snippet around the first match.
//! - `read_session(id, harness?, format="markdown"|"json")` — the full transcript.
//! - `project_sessions(cwd, limit=20)` — sessions whose recorded cwd contains the given path; the
//!   headline "what happened in THIS project before / what are sibling agents doing here" tool.

use anyhow::Context as _;
use cv_core::watch::{Filter, Watcher};
use cv_core::{Block, Harness, Session, SessionRef};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use std::io::Write as _;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PROTOCOL_VERSION: &str = "2025-06-18";

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    eprintln!("cv-mcp: claurdvoyant MCP server starting (stdio JSON-RPC)");

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break; // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                // Parse error: id unknown, reply with null id per JSON-RPC.
                let resp = error_response(Value::Null, -32700, &format!("Parse error: {e}"));
                write_message(&mut stdout, &resp).await?;
                continue;
            }
        };

        // A notification (no `id`) gets no response; just acknowledge by ignoring.
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");

        // Handle in a blocking-friendly way; the cv_core calls are synchronous + filesystem-heavy,
        // so run them on a blocking thread to keep the runtime responsive.
        let response = handle(method, req.get("params").cloned(), id.clone()).await;

        if let Some(resp) = response {
            write_message(&mut stdout, &resp).await?;
        }
    }

    Ok(())
}

async fn handle(method: &str, params: Option<Value>, id: Option<Value>) -> Option<Value> {
    // Notifications carry no id; we never respond to them.
    let is_notification = id.is_none();
    let id = id.unwrap_or(Value::Null);

    match method {
        "initialize" => Some(ok_response(id, initialize_result())),
        "ping" => Some(ok_response(id, json!({}))),
        "notifications/initialized" | "notifications/cancelled" => None,
        "tools/list" => Some(ok_response(id, json!({ "tools": tool_list() }))),
        "tools/call" => {
            let params = params.unwrap_or(Value::Null);
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));

            // Run the (synchronous, fs-bound) tool body off the async reactor.
            let result =
                tokio::task::spawn_blocking(move || call_tool(&name, &args)).await;

            match result {
                Ok(Ok(text)) => Some(ok_response(id, tool_text_result(&text, false))),
                Ok(Err(e)) => {
                    // Tool execution errors are reported via the result's isError flag, not a
                    // protocol-level error, per the MCP spec.
                    Some(ok_response(id, tool_text_result(&format!("error: {e:#}"), true)))
                }
                Err(join_err) => Some(error_response(
                    id,
                    -32603,
                    &format!("internal task error: {join_err}"),
                )),
            }
        }
        _ => {
            if is_notification {
                None
            } else {
                Some(error_response(id, -32601, &format!("Method not found: {method}")))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// MCP boilerplate
// ---------------------------------------------------------------------------

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "claurdvoyant",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": "claurdvoyant lets you read OTHER agents' sessions across harnesses and time. \
Use project_sessions(cwd) to see what happened (or is happening) in the current project, \
search_sessions(query) to find a past conversation by content, and read_session(id) to read a full transcript."
    })
}

fn tool_list() -> Value {
    json!([
        {
            "name": "list_sessions",
            "description": "List recent agent sessions across all harnesses, newest first. Optionally filter by harness or by a substring of the working directory.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "harness": { "type": "string", "description": "Restrict to one harness: claude, codex, grok, opencode, gemini." },
                    "cwd_contains": { "type": "string", "description": "Only sessions whose recorded cwd contains this substring." },
                    "limit": { "type": "number", "description": "Max results (default 40)." }
                }
            }
        },
        {
            "name": "search_sessions",
            "description": "Full-text search across agent session transcripts (case-insensitive substring over all message content). Returns matching sessions with a snippet around the first match. Parses sessions on demand.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Text to search for." },
                    "harness": { "type": "string", "description": "Restrict to one harness." },
                    "cwd_contains": { "type": "string", "description": "Only sessions whose recorded cwd contains this substring." },
                    "limit": { "type": "number", "description": "Max results (default 20)." }
                },
                "required": ["query"]
            }
        },
        {
            "name": "read_session",
            "description": "Read a full agent session transcript by id (prefix match allowed). Returns markdown by default, or pretty JSON.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Session id (or unique prefix)." },
                    "harness": { "type": "string", "description": "Disambiguate by harness if the id is ambiguous." },
                    "format": { "type": "string", "enum": ["markdown", "json"], "description": "Output format (default markdown)." }
                },
                "required": ["id"]
            }
        },
        {
            "name": "project_sessions",
            "description": "List sessions whose recorded working directory equals or contains the given path, newest first. The headline 'what happened in THIS project before / what are sibling agents doing here' tool.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "cwd": { "type": "string", "description": "Project path to match against sessions' recorded cwd." },
                    "limit": { "type": "number", "description": "Max results (default 20)." }
                },
                "required": ["cwd"]
            }
        },
        {
            "name": "await_omen",
            "description": "Block until another agent's session emits a message matching a regex, then return it. Watches live for new/appended messages (text, thinking, or tool results) across harnesses. Use to wait on a sibling agent — e.g. await_omen(regex='BUILD (PASSED|FAILED)', cwd_contains='/myproj'). Returns when matched or when timeout_secs elapses.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "regex": { "type": "string", "description": "Rust regex to match against newly-emitted message text." },
                    "harness": { "type": "string", "description": "Only watch this harness." },
                    "cwd_contains": { "type": "string", "description": "Only watch sessions whose cwd contains this substring." },
                    "timeout_secs": { "type": "number", "description": "Give up after this many seconds (default 120)." },
                    "interval_secs": { "type": "number", "description": "Poll interval (default 2)." }
                },
                "required": ["regex"]
            }
        },
        {
            "name": "board_post",
            "description": "Post a message to a coordination-board channel so OTHER agents can see it. Use to broadcast status, leave a note, or hand off work (e.g. channel='myproj', body='done: migrated auth, tests green').",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel": { "type": "string", "description": "Channel/room name (often a project path or topic)." },
                    "body": { "type": "string", "description": "The message text." },
                    "from": { "type": "string", "description": "Who's posting (agent/session name). Default 'agent'." },
                    "kind": { "type": "string", "description": "msg | status | event (default msg)." },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional tags." },
                    "session_ref": { "type": "string", "description": "Optional session id this message is about." }
                },
                "required": ["channel", "body"]
            }
        },
        {
            "name": "board_read",
            "description": "Read recent messages from a coordination-board channel — see what other agents have posted. Pass `since` (a message id cursor) to get only newer messages.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel": { "type": "string", "description": "Channel/room name." },
                    "since": { "type": "string", "description": "Return only messages after this message id." },
                    "limit": { "type": "number", "description": "Max messages (default 50, 0 = all)." }
                },
                "required": ["channel"]
            }
        },
        {
            "name": "board_await",
            "description": "Block until a NEW message on a board channel matches a regex (or timeout). The way to wait on a sibling agent: board_await(channel='myproj', regex='BUILD (PASSED|FAILED)').",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel": { "type": "string", "description": "Channel/room name." },
                    "regex": { "type": "string", "description": "Rust regex matched against message bodies." },
                    "since": { "type": "string", "description": "Start cursor (default: current tail — only new posts)." },
                    "timeout_secs": { "type": "number", "description": "Give up after this many seconds (default 120)." },
                    "interval_secs": { "type": "number", "description": "Poll interval (default 2)." }
                },
                "required": ["channel", "regex"]
            }
        }
    ])
}

fn ok_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_text_result(text: &str, is_error: bool) -> Value {
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": is_error
    })
}

async fn write_message(
    stdout: &mut tokio::io::Stdout,
    msg: &Value,
) -> anyhow::Result<()> {
    let mut buf = serde_json::to_vec(msg)?;
    buf.push(b'\n');
    stdout.write_all(&buf).await?;
    stdout.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tool implementations (synchronous; called via spawn_blocking)
// ---------------------------------------------------------------------------

fn call_tool(name: &str, args: &Value) -> anyhow::Result<String> {
    match name {
        "list_sessions" => list_sessions(args),
        "search_sessions" => search_sessions(args),
        "read_session" => read_session(args),
        "project_sessions" => project_sessions(args),
        "await_omen" => await_omen(args),
        "board_post" => board_post(args),
        "board_read" => board_read(args),
        "board_await" => board_await(args),
        other => anyhow::bail!("unknown tool: {other}"),
    }
}

/// Post a message to a coordination-board channel (so other agents can see it).
fn board_post(args: &Value) -> anyhow::Result<String> {
    let channel = arg_str(args, "channel").context("`channel` is required")?;
    let from = arg_str(args, "from").unwrap_or("agent");
    let body = arg_str(args, "body").context("`body` is required")?;
    let kind = arg_str(args, "kind");
    let tags = args
        .get("tags")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let session_ref = arg_str(args, "session_ref").map(String::from);
    let msg = cv_core::board::post(channel, from, body, kind, tags, session_ref)?;
    Ok(serde_json::to_string_pretty(&msg)?)
}

/// Read recent messages from a board channel (optionally only those after a cursor id).
fn board_read(args: &Value) -> anyhow::Result<String> {
    let channel = arg_str(args, "channel").context("`channel` is required")?;
    let since = arg_str(args, "since");
    let limit = arg_usize(args, "limit", 50);
    let msgs = cv_core::board::read(channel, since, limit)?;
    Ok(serde_json::to_string_pretty(&msgs)?)
}

/// Block until a board message body matches a regex (or timeout). The polling loop runs on a
/// blocking worker thread (tools dispatch via spawn_blocking), so blocking here is fine.
fn board_await(args: &Value) -> anyhow::Result<String> {
    let channel = arg_str(args, "channel").context("`channel` is required")?;
    let pattern = arg_str(args, "regex").context("`regex` is required")?;
    let re = regex::Regex::new(pattern).with_context(|| format!("invalid regex: {pattern:?}"))?;
    let timeout = Duration::from_secs(arg_usize(args, "timeout_secs", 120) as u64);
    let interval = Duration::from_secs(arg_usize(args, "interval_secs", 2).max(1) as u64);
    // Start the cursor at the current tail so we only react to *new* posts (unless caller gives one).
    let mut cursor = arg_str(args, "since").map(String::from).or_else(|| {
        cv_core::board::read(channel, None, 0)
            .ok()
            .and_then(|m| m.last().map(|x| x.id.clone()))
    });
    let start = Instant::now();
    loop {
        let fresh = cv_core::board::read(channel, cursor.as_deref(), 0)?;
        for m in &fresh {
            cursor = Some(m.id.clone());
            if re.is_match(&m.body) {
                return Ok(serde_json::to_string_pretty(&json!({
                    "matched": true,
                    "message": m,
                    "cursor": m.id,
                }))?);
            }
        }
        if start.elapsed() >= timeout {
            return Ok(serde_json::to_string_pretty(&json!({
                "matched": false,
                "timed_out": true,
                "cursor": cursor,
            }))?);
        }
        std::thread::sleep(interval);
    }
}

/// Block until a newly-emitted message matches `regex` (or `timeout_secs` elapses). The polling loop
/// runs on a blocking worker thread (tools are dispatched via spawn_blocking), so blocking is fine.
fn await_omen(args: &Value) -> anyhow::Result<String> {
    let pattern = arg_str(args, "regex").context("`regex` is required")?;
    let re = regex::Regex::new(pattern).with_context(|| format!("invalid regex: {pattern:?}"))?;
    let filter = Filter {
        harness: parse_harness(args)?,
        cwd_contains: arg_str(args, "cwd_contains").map(str::to_string),
    };
    let timeout = Duration::from_secs(arg_usize(args, "timeout_secs", 120) as u64);
    let interval = Duration::from_secs(arg_usize(args, "interval_secs", 2).max(1) as u64);

    // Only react to activity from now on (not the existing backlog).
    let mut watcher = Watcher::new(filter, false);
    let start = Instant::now();
    loop {
        for ev in watcher.poll() {
            for m in &ev.new_messages {
                let text = message_text(m);
                if let Some(hit) = re.find(&text) {
                    let snippet_start = hit.start().saturating_sub(80);
                    let snippet_end = (hit.end() + 80).min(text.len());
                    return Ok(serde_json::to_string_pretty(&json!({
                        "matched": true,
                        "harness": ev.reference.harness.as_str(),
                        "id": ev.reference.id,
                        "cwd": ev.reference.cwd.as_ref().map(|p| p.to_string_lossy()),
                        "role": cv_core::render::role_label(m.role),
                        "matched_text": &text[char_floor(&text, snippet_start)..char_ceil(&text, snippet_end)],
                        "event": match ev.kind { cv_core::watch::EventKind::New => "new_session", _ => "appended" },
                    }))?);
                }
            }
        }
        if start.elapsed() >= timeout {
            return Ok(serde_json::to_string_pretty(&json!({
                "matched": false,
                "timed_out": true,
                "waited_secs": start.elapsed().as_secs(),
            }))?);
        }
        std::thread::sleep(interval);
    }
}

/// All matchable text in a message: text + thinking + tool-result content.
fn message_text(m: &cv_core::Message) -> String {
    let mut out = String::new();
    for b in &m.content {
        match b {
            Block::Text { text } | Block::Thinking { text, .. } => {
                out.push_str(text);
                out.push('\n');
            }
            Block::ToolResult { content, .. } => {
                out.push_str(content);
                out.push('\n');
            }
            Block::ToolUse { name, input, .. } => {
                out.push_str(name);
                out.push(' ');
                out.push_str(&input.to_string());
                out.push('\n');
            }
            Block::File { path, source, .. } => {
                if let Some(p) = path.as_deref().or(source.as_deref()) {
                    out.push_str(p);
                    out.push('\n');
                }
            }
            Block::Image { .. } => {}
        }
    }
    out
}

fn char_floor(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
fn char_ceil(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn arg_usize(args: &Value, key: &str, default: usize) -> usize {
    args.get(key)
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(default)
}

fn parse_harness(args: &Value) -> anyhow::Result<Option<Harness>> {
    match arg_str(args, "harness") {
        None => Ok(None),
        Some(s) => match Harness::parse(s) {
            Some(h) => Ok(Some(h)),
            None => anyhow::bail!("unknown harness: {s:?} (expected one of claude, codex, grok, opencode, gemini)"),
        },
    }
}

/// Sort newest-first by updated_at (then created_at).
fn sort_newest_first(refs: &mut [SessionRef]) {
    refs.sort_by(|a, b| {
        let ka = a.updated_at.or(a.created_at);
        let kb = b.updated_at.or(b.created_at);
        kb.cmp(&ka)
    });
}

fn cwd_contains(r: &SessionRef, needle: &str) -> bool {
    r.cwd
        .as_ref()
        .map(|p| p.to_string_lossy().contains(needle))
        .unwrap_or(false)
}

fn ref_to_json(r: &SessionRef) -> Value {
    json!({
        "id": r.id,
        "harness": r.harness.as_str(),
        "cwd": r.cwd.as_ref().map(|p| p.to_string_lossy().to_string()),
        "title": r.title,
        "updated_at": r.updated_at.map(|t| t.to_rfc3339()),
        "created_at": r.created_at.map(|t| t.to_rfc3339()),
        "message_count": r.message_count,
    })
}

fn list_sessions(args: &Value) -> anyhow::Result<String> {
    let harness = parse_harness(args)?;
    let cwd_needle = arg_str(args, "cwd_contains");
    let limit = arg_usize(args, "limit", 40);

    let mut refs: Vec<SessionRef> = cv_core::discover_all()
        .into_iter()
        .filter(|r| harness.is_none_or(|h| r.harness == h))
        .filter(|r| cwd_needle.is_none_or(|n| cwd_contains(r, n)))
        .collect();

    sort_newest_first(&mut refs);
    refs.truncate(limit);

    let out: Vec<Value> = refs.iter().map(ref_to_json).collect();
    Ok(serde_json::to_string_pretty(&json!(out))?)
}

fn project_sessions(args: &Value) -> anyhow::Result<String> {
    let cwd = arg_str(args, "cwd")
        .ok_or_else(|| anyhow::anyhow!("missing required argument: cwd"))?;
    let limit = arg_usize(args, "limit", 20);

    let mut refs: Vec<SessionRef> = cv_core::discover_all()
        .into_iter()
        .filter(|r| {
            r.cwd
                .as_ref()
                .map(|p| {
                    let s = p.to_string_lossy();
                    // equals or contains in either direction (project path under cwd, or cwd under project)
                    s.contains(cwd) || cwd.contains(s.as_ref())
                })
                .unwrap_or(false)
        })
        .collect();

    sort_newest_first(&mut refs);
    refs.truncate(limit);

    let out: Vec<Value> = refs.iter().map(ref_to_json).collect();
    Ok(serde_json::to_string_pretty(&json!(out))?)
}

fn search_sessions(args: &Value) -> anyhow::Result<String> {
    let query = arg_str(args, "query")
        .ok_or_else(|| anyhow::anyhow!("missing required argument: query"))?;
    let harness = parse_harness(args)?;
    let cwd_needle = arg_str(args, "cwd_contains");
    let limit = arg_usize(args, "limit", 20);
    let needle = query.to_lowercase();

    let mut refs: Vec<SessionRef> = cv_core::discover_all()
        .into_iter()
        .filter(|r| harness.is_none_or(|h| r.harness == h))
        .filter(|r| cwd_needle.is_none_or(|n| cwd_contains(r, n)))
        .collect();
    sort_newest_first(&mut refs);

    let mut hits: Vec<Value> = Vec::new();
    for r in &refs {
        if hits.len() >= limit {
            break;
        }
        // Locate and parse this session.
        let parsed = match cv_core::find(&r.id, Some(r.harness)) {
            Ok(Some((sref, adapter))) => match adapter.parse(&sref) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("cv-mcp: parse failed for {} ({}): {e:#}", r.id, r.harness);
                    continue;
                }
            },
            _ => continue,
        };

        let haystack = parsed.searchable_text();
        let lower = haystack.to_lowercase();
        if let Some(pos) = lower.find(&needle) {
            let snippet = make_snippet(&haystack, pos, needle.len());
            let mut entry = ref_to_json(r);
            if let Value::Object(ref mut m) = entry {
                m.insert("snippet".to_string(), json!(snippet));
            }
            hits.push(entry);
        }
    }

    Ok(serde_json::to_string_pretty(&json!(hits))?)
}

/// Build a ~280-char snippet centered on a byte match position, on char boundaries.
fn make_snippet(text: &str, match_byte: usize, match_len: usize) -> String {
    const RADIUS: usize = 120;
    let start = floor_char_boundary(text, match_byte.saturating_sub(RADIUS));
    let end = ceil_char_boundary(text, (match_byte + match_len + RADIUS).min(text.len()));
    let mut snippet = String::new();
    if start > 0 {
        snippet.push('…');
    }
    snippet.push_str(text[start..end].trim());
    if end < text.len() {
        snippet.push('…');
    }
    snippet.replace('\n', " ")
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn read_session(args: &Value) -> anyhow::Result<String> {
    let id = arg_str(args, "id")
        .ok_or_else(|| anyhow::anyhow!("missing required argument: id"))?;
    let harness = parse_harness(args)?;
    let format = arg_str(args, "format").unwrap_or("markdown");

    let (sref, adapter) = cv_core::find(id, harness)?
        .ok_or_else(|| anyhow::anyhow!("no session found for id {id:?}"))?;
    let session: Session = adapter.parse(&sref)?;

    match format {
        "json" => Ok(serde_json::to_string_pretty(&session)?),
        "markdown" | "" => Ok(cv_core::render::to_markdown(&session)),
        other => anyhow::bail!("unknown format {other:?} (expected 'markdown' or 'json')"),
    }
}

// Ensure stderr logging is line-buffered-ish on panic paths; not strictly required.
#[allow(dead_code)]
fn flush_stderr() {
    let _ = std::io::stderr().flush();
}
