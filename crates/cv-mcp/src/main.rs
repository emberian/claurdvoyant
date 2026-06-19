//! # cv-mcp — clustervision as an MCP server
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
//! claude mcp add clustervision -- /absolute/path/to/cv-mcp
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
//! - `project_sessions(cwd, harness?, limit=20)` — sessions whose recorded cwd contains the given
//!   path; the headline "what happened in THIS project before / what are sibling agents doing
//!   here" tool.
//! - `recall(query, k=5, harness?, cwd_contains?)` — "have I/another agent solved this before?"
//!   Semantic search over the whole corpus (via `cv-search`) that returns the most relevant
//!   message *spans*, not just metadata. Needs `cv index --semantic`; degrades to keyword search
//!   otherwise.

use anyhow::Context as _;
use cv_core::watch::{Filter, Watcher};
use cv_core::{Block, Harness, Session, SessionRef};
use serde_json::{json, Value};
use std::io::Write as _;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PROTOCOL_VERSION: &str = "2025-06-18";

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    eprintln!("cv-mcp: clustervision MCP server starting (stdio JSON-RPC)");

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();
    let mut buf: Vec<u8> = Vec::new();

    loop {
        buf.clear();
        // Read raw bytes, not into a String: a single non-UTF-8 byte on stdin would make `read_line`
        // error out, and `?` would tear down the whole server. Lossy-decode instead so one bad frame
        // is tolerated rather than fatal.
        let n = reader.read_until(b'\n', &mut buf).await?;
        if n == 0 {
            break; // EOF
        }
        let line = String::from_utf8_lossy(&buf);
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
    // A request without an `id` is a JSON-RPC *notification*: the method still runs (a
    // notification tools/call may have side effects worth performing), but per spec the server
    // MUST NOT reply — not even with `id: null`.
    let is_notification = id.is_none();
    let id = id.unwrap_or(Value::Null);

    let response = match method {
        "initialize" => Some(ok_response(id, initialize_result())),
        "ping" => Some(ok_response(id, json!({}))),
        "notifications/initialized" | "notifications/cancelled" => None,
        "tools/list" => Some(ok_response(id, json!({ "tools": tool_list() }))),
        "tools/call" => {
            let params = params.unwrap_or(Value::Null);
            let name = params.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

            // Run the (synchronous, fs-bound) tool body off the async reactor.
            let result = tokio::task::spawn_blocking(move || call_tool(&name, &args)).await;

            match result {
                Ok(Ok(text)) => Some(ok_response(id, tool_text_result(&text, false))),
                Ok(Err(e)) => {
                    // Tool execution errors are reported via the result's isError flag, not a
                    // protocol-level error, per the MCP spec.
                    Some(ok_response(id, tool_text_result(&format!("error: {e:#}"), true)))
                }
                Err(join_err) => Some(error_response(id, -32603, &format!("internal task error: {join_err}"))),
            }
        }
        _ => Some(error_response(id, -32601, &format!("Method not found: {method}"))),
    };
    if is_notification {
        None
    } else {
        response
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
            "name": "clustervision",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": "clustervision lets you read OTHER agents' sessions across harnesses and time. \
    Use project_sessions(cwd) to see what happened (or is happening) in the current project, \
    search_sessions(query) to find a past conversation by content, and read_session(id) to read a full transcript. \
    Use recall(query) — the 'find where this was solved before' tool — to semantically search the whole \
    cross-harness corpus and pull back the most relevant message spans mid-task."
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
            "description": "List sessions whose recorded working directory equals or contains the given path, newest first. Optionally filter by harness. The headline 'what happened in THIS project before / what are sibling agents doing here' tool.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "cwd": { "type": "string", "description": "Project path to match against sessions' recorded cwd." },
                    "harness": { "type": "string", "description": "Restrict to one harness: claude, codex, grok, opencode, gemini." },
                    "limit": { "type": "number", "description": "Max results (default 20)." }
                },
                "required": ["cwd"]
            }
        },
        {
            "name": "recall",
            "description": "\"Have I (or another agent) solved/seen this before?\" Semantically searches the WHOLE cross-harness session corpus and returns the most relevant message SPANS (a short excerpt of the messages around the best match), not just session metadata — so you can pull past context into a running task. Tries meaning-based search first and falls back to keyword search. For best results run `cv index --semantic` to build the embedding store; without it, recall degrades to keyword (full-text) search.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What you're trying to do / the problem to find prior work on." },
                    "k": { "type": "number", "description": "Max sessions to return (default 5)." },
                    "harness": { "type": "string", "description": "Restrict results to one harness: claude, codex, grok, opencode, gemini." },
                    "cwd_contains": { "type": "string", "description": "Only sessions whose recorded cwd contains this substring." }
                },
                "required": ["query"]
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
        },
        {
            "name": "board_request",
            "description": "Post a REQUEST (a question/ask) on a board channel that other agents can answer with board_reply. Returns the posted message including its `id` — keep that id to collect answers via board_replies(channel, request_id) or to board_await replies.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel": { "type": "string", "description": "Channel/room name." },
                    "body": { "type": "string", "description": "The question / ask." },
                    "from": { "type": "string", "description": "Who's asking. Default 'agent'." }
                },
                "required": ["channel", "body"]
            }
        },
        {
            "name": "board_reply",
            "description": "Reply to a prior board_request, correlated by its message id. The reply records the request id so board_replies(channel, request_id) collects it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel": { "type": "string", "description": "Channel/room name." },
                    "in_reply_to": { "type": "string", "description": "The id of the request message being answered." },
                    "body": { "type": "string", "description": "The answer text." },
                    "from": { "type": "string", "description": "Who's replying. Default 'agent'." }
                },
                "required": ["channel", "in_reply_to", "body"]
            }
        },
        {
            "name": "board_replies",
            "description": "Collect all replies to a request id on a channel (the answers to a board_request), in chronological order.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel": { "type": "string", "description": "Channel/room name." },
                    "request_id": { "type": "string", "description": "The id of the original request message." }
                },
                "required": ["channel", "request_id"]
            }
        },
        {
            "name": "board_claim",
            "description": "Try to CLAIM a task `key` on a channel — a soft distributed lock so two agents don't grab the same task. Returns {granted: bool, lease?}. If granted is false another agent holds it; branch on that to pick a different task. Renews your own claim if you already hold it. Release with board_release when done.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel": { "type": "string", "description": "Channel/room name." },
                    "key": { "type": "string", "description": "The task key to claim (e.g. a file path or task id)." },
                    "from": { "type": "string", "description": "Who's claiming. Default 'agent'." },
                    "ttl_secs": { "type": "number", "description": "How long the claim is held before it can be stolen (default 300)." }
                },
                "required": ["channel", "key"]
            }
        },
        {
            "name": "board_release",
            "description": "Release a task `key` you claimed on a channel, freeing it for other agents. Idempotent and only releases a claim you own.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel": { "type": "string", "description": "Channel/room name." },
                    "key": { "type": "string", "description": "The claimed task key to release." },
                    "from": { "type": "string", "description": "Who's releasing (must match the claimant). Default 'agent'." }
                },
                "required": ["channel", "key"]
            }
        },
        {
            "name": "board_claims",
            "description": "List the currently-active (un-expired) task claims on a channel as {key, owner, expires_at}. See who's working on what.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel": { "type": "string", "description": "Channel/room name." }
                },
                "required": ["channel"]
            }
        },
        {
            "name": "board_who",
            "description": "List agents that have recently sent a board_heartbeat on a channel (active presence) within the last `within_secs` seconds (default 120).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel": { "type": "string", "description": "Channel/room name." },
                    "within_secs": { "type": "number", "description": "Presence window in seconds (default 120)." }
                },
                "required": ["channel"]
            }
        },
        {
            "name": "board_heartbeat",
            "description": "Announce your presence on a channel by posting a heartbeat. Other agents see you via board_who. Call periodically to stay 'active'.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel": { "type": "string", "description": "Channel/room name." },
                    "from": { "type": "string", "description": "Who's present (agent/session name)." }
                },
                "required": ["channel", "from"]
            }
        },
        {
            "name": "board_ack",
            "description": "Acknowledge a board message by id with a tiny ack note (correlated to the target message), so the sender can confirm it was seen.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channel": { "type": "string", "description": "Channel/room name." },
                    "message_id": { "type": "string", "description": "The id of the message being acknowledged." },
                    "from": { "type": "string", "description": "Who's acking. Default 'agent'." }
                },
                "required": ["channel", "message_id"]
            }
        },
        {
            "name": "prune_session",
            "description": "Custom, lossless compaction of a Claude Code session into a NEW resumable session: snip bulky OLD tool payloads (large file reads, command logs, screenshots) into a sidecar, leaving a small marker — prompts and chronological flow are preserved verbatim, and the most recent turns are kept untouched. Resume the new id with `claude --resume`. Originals stay retrievable via prune_retrieve. Claude harness only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Source session id (or unique prefix)." },
                    "min_size": { "type": "number", "description": "Snip a tool payload only if larger than this many bytes (default 2048)." },
                    "keep_last": { "type": "number", "description": "Keep the last N conversational turns' payloads verbatim (default 25)." },
                    "to": { "type": "string", "description": "New session id (default: a fresh UUID)." },
                    "drop": { "type": "boolean", "description": "Hard-drop payloads (no sidecar, irreversible) instead of stashing them." },
                    "thinking": { "type": "boolean", "description": "Also flatten the oldest assistant reasoning (thinking blocks); recent thinking (within keep_last) stays. Big lever for shrinking loaded context." },
                    "copy_resources": { "type": "boolean", "description": "Also copy the session's subagents/workflows dir under the new id (off by default; can be large)." },
                    "revive": { "type": "boolean", "description": "Correct the recorded context size so a maxed-out session will resume. Claude Code's resume gate reads the last turn's stored usage (input+cache) as the current size before re-sending anything, so a session at the wall refuses to resume even after pruning. This rewrites that stale number to the honest post-prune figure." },
                    "dry_run": { "type": "boolean", "description": "Report what would happen without writing." }
                },
                "required": ["id"]
            }
        },
        {
            "name": "prune_retrieve",
            "description": "Fetch a stashed original payload back out of a pruned session's sidecar by its tool_use_id (the id printed inside a [PRUNED id=…] marker). Returns the verbatim content.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "The pruned session id the marker names." },
                    "tool_use_id": { "type": "string", "description": "The id from the [PRUNED id=…] marker (e.g. toolu_… or …#tur)." }
                },
                "required": ["session", "tool_use_id"]
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

async fn write_message(stdout: &mut tokio::io::Stdout, msg: &Value) -> anyhow::Result<()> {
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
        "recall" => recall(args),
        "await_omen" => await_omen(args),
        "board_post" => board_post(args),
        "board_read" => board_read(args),
        "board_await" => board_await(args),
        "board_request" => board_request(args),
        "board_reply" => board_reply(args),
        "board_replies" => board_replies(args),
        "board_claim" => board_claim(args),
        "board_release" => board_release(args),
        "board_claims" => board_claims(args),
        "board_who" => board_who(args),
        "board_heartbeat" => board_heartbeat(args),
        "board_ack" => board_ack(args),
        "prune_session" => prune_session(args),
        "prune_retrieve" => prune_retrieve(args),
        other => anyhow::bail!("unknown tool: {other}"),
    }
}

/// Compact a Claude session into a new resumable one (see `cv_core::prune`).
fn prune_session(args: &Value) -> anyhow::Result<String> {
    let id = arg_str(args, "id").ok_or_else(|| anyhow::anyhow!("missing required argument: id"))?;
    let harness = parse_harness(args)?;
    let (sref, _adapter) =
        cv_core::find(id, harness)?.ok_or_else(|| anyhow::anyhow!("no session found for id {id:?}"))?;
    if sref.harness != Harness::Claude {
        anyhow::bail!(
            "prune currently supports Claude Code sessions only (got {})",
            sref.harness
        );
    }
    let opts = cv_core::prune::PruneOptions {
        min_size: arg_usize(args, "min_size", 2048),
        keep_last: arg_usize(args, "keep_last", 25),
        drop: args.get("drop").and_then(Value::as_bool).unwrap_or(false),
        thinking: args.get("thinking").and_then(Value::as_bool).unwrap_or(false),
        new_id: arg_str(args, "to").map(String::from),
        copy_resources: args.get("copy_resources").and_then(Value::as_bool).unwrap_or(false),
        revive: args.get("revive").and_then(Value::as_bool).unwrap_or(false),
        dry_run: args.get("dry_run").and_then(Value::as_bool).unwrap_or(false),
    };
    let r = cv_core::prune::prune_session(&sref.path, &opts)?;
    Ok(serde_json::to_string_pretty(&json!({
        "source_id": r.source_id,
        "new_id": r.new_id,
        "new_path": r.new_path,
        "sidecar_path": r.sidecar_path,
        "copied_resources": r.copied_resources,
        "pruned_count": r.pruned_count,
        "image_blocks": r.image_blocks,
        "original_size": r.original_size,
        "new_size": r.new_size,
        "bytes_saved": r.bytes_saved(),
        "est_context_tokens_saved": r.est_context_tokens_saved,
        "usage_rewritten": r.usage_rewritten,
        "revive_tokens": r.revive_tokens,
        "revive_old_tokens": r.revive_old_tokens,
        "dry_run": r.dry_run,
        "resume": format!("claude --resume {}", r.new_id),
    }))?)
}

/// Fetch a stashed original out of a pruned session's sidecar by tool_use_id.
fn prune_retrieve(args: &Value) -> anyhow::Result<String> {
    let session = arg_str(args, "session").ok_or_else(|| anyhow::anyhow!("missing required argument: session"))?;
    let tool_use_id =
        arg_str(args, "tool_use_id").ok_or_else(|| anyhow::anyhow!("missing required argument: tool_use_id"))?;
    let (sref, _adapter) =
        cv_core::find(session, None)?.ok_or_else(|| anyhow::anyhow!("no session found for id {session:?}"))?;
    let dir = sref.path.parent().unwrap_or(std::path::Path::new("."));
    let stem = sref.path.file_stem().and_then(|s| s.to_str()).unwrap_or(session);
    let sidecar = dir.join(format!("{stem}.flat.jsonl"));
    let value = cv_core::prune::retrieve(&sidecar, tool_use_id)?;
    match value.as_str() {
        Some(s) => Ok(s.to_string()),
        None => Ok(serde_json::to_string_pretty(&value)?),
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

// ---------------------------------------------------------------------------
// Board coordination primitives (request/reply, claim/lease, presence, ack)
// ---------------------------------------------------------------------------

/// Post a request (a question others can `board_reply` to). Returns the message incl. its id.
fn board_request(args: &Value) -> anyhow::Result<String> {
    let channel = arg_str(args, "channel").context("`channel` is required")?;
    let from = arg_str(args, "from").unwrap_or("agent");
    let body = arg_str(args, "body").context("`body` is required")?;
    let msg = cv_core::board::request(channel, from, body)?;
    Ok(serde_json::to_string_pretty(&msg)?)
}

/// Reply to a prior request, correlated by its message id.
fn board_reply(args: &Value) -> anyhow::Result<String> {
    let channel = arg_str(args, "channel").context("`channel` is required")?;
    let from = arg_str(args, "from").unwrap_or("agent");
    let in_reply_to = arg_str(args, "in_reply_to").context("`in_reply_to` is required")?;
    let body = arg_str(args, "body").context("`body` is required")?;
    let msg = cv_core::board::reply(channel, from, in_reply_to, body)?;
    Ok(serde_json::to_string_pretty(&msg)?)
}

/// Collect all replies to a request id on a channel.
fn board_replies(args: &Value) -> anyhow::Result<String> {
    let channel = arg_str(args, "channel").context("`channel` is required")?;
    let request_id = arg_str(args, "request_id").context("`request_id` is required")?;
    let msgs = cv_core::board::replies(channel, request_id)?;
    Ok(serde_json::to_string_pretty(&msgs)?)
}

/// Try to claim a task `key` (soft distributed lock). Returns `{granted, lease?}`.
fn board_claim(args: &Value) -> anyhow::Result<String> {
    let channel = arg_str(args, "channel").context("`channel` is required")?;
    let from = arg_str(args, "from").unwrap_or("agent");
    let key = arg_str(args, "key").context("`key` is required")?;
    let ttl = Duration::from_secs(arg_usize(args, "ttl_secs", 300).max(1) as u64);
    match cv_core::board::claim(channel, from, key, ttl)? {
        Some(lease) => Ok(serde_json::to_string_pretty(&json!({
            "granted": true,
            "lease": lease_json(&lease),
        }))?),
        None => Ok(serde_json::to_string_pretty(&json!({
            "granted": false,
        }))?),
    }
}

/// Release a task `key` previously claimed by `from`. Idempotent; only releases a claim you own.
///
/// `board::release` takes a `Lease`, whose dir/channel fields are private — the only way to obtain
/// one is via `claim`. So we re-claim the key (which renews/returns our own lease, or returns None
/// if another agent holds it) and then release that lease. If someone else owns the key, claim
/// returns None and we leave their claim untouched (a no-op, which is the correct semantics).
fn board_release(args: &Value) -> anyhow::Result<String> {
    let channel = arg_str(args, "channel").context("`channel` is required")?;
    let from = arg_str(args, "from").unwrap_or("agent");
    let key = arg_str(args, "key").context("`key` is required")?;
    // Guard before re-claiming: claiming a *free* key just to release it would falsely report
    // `released: true` for a lock we never held. Only proceed if `from` already owns a live claim.
    let owned = cv_core::board::active_claims(channel)?
        .into_iter()
        .any(|(k, owner, _)| k == key && owner == from);
    if !owned {
        return Ok(serde_json::to_string_pretty(&json!({
            "released": false,
            "key": key,
            "reason": "you don't hold this key; nothing released",
        }))?);
    }
    // Re-acquire (renews our own lease) then release it.
    match cv_core::board::claim(channel, from, key, Duration::from_secs(1))? {
        Some(lease) => {
            cv_core::board::release(&lease)?;
            Ok(serde_json::to_string_pretty(&json!({
                "released": true,
                "key": key,
                "owner": from,
            }))?)
        }
        None => Ok(serde_json::to_string_pretty(&json!({
            "released": false,
            "key": key,
            "reason": "key is held by another agent; nothing released",
        }))?),
    }
}

/// List active (un-expired) claims on a channel as `{key, owner, expires_at}`.
fn board_claims(args: &Value) -> anyhow::Result<String> {
    let channel = arg_str(args, "channel").context("`channel` is required")?;
    let claims = cv_core::board::active_claims(channel)?;
    let out: Vec<Value> = claims
        .into_iter()
        .map(|(key, owner, expires_at)| {
            json!({
                "key": key,
                "owner": owner,
                "expires_at": expires_at.to_rfc3339(),
            })
        })
        .collect();
    Ok(serde_json::to_string_pretty(&json!(out))?)
}

/// List agents that heartbeat on a channel within the last `within_secs` (default 120).
fn board_who(args: &Value) -> anyhow::Result<String> {
    let channel = arg_str(args, "channel").context("`channel` is required")?;
    let within = Duration::from_secs(arg_usize(args, "within_secs", 120) as u64);
    let who = cv_core::board::who(channel, within)?;
    Ok(serde_json::to_string_pretty(&json!(who))?)
}

/// Announce presence on a channel by posting a heartbeat.
fn board_heartbeat(args: &Value) -> anyhow::Result<String> {
    let channel = arg_str(args, "channel").context("`channel` is required")?;
    let from = arg_str(args, "from").context("`from` is required")?;
    let msg = cv_core::board::heartbeat(channel, from)?;
    Ok(serde_json::to_string_pretty(&msg)?)
}

/// Acknowledge a message by id with a tiny ack note.
fn board_ack(args: &Value) -> anyhow::Result<String> {
    let channel = arg_str(args, "channel").context("`channel` is required")?;
    let from = arg_str(args, "from").unwrap_or("agent");
    let message_id = arg_str(args, "message_id").context("`message_id` is required")?;
    let msg = cv_core::board::ack(channel, from, message_id)?;
    Ok(serde_json::to_string_pretty(&msg)?)
}

/// Build a JSON view of a `Lease` from its public fields (the struct isn't directly Serialize).
fn lease_json(lease: &cv_core::board::Lease) -> Value {
    json!({
        "key": lease.key,
        "owner": lease.owner,
        "expires_at": lease.expires_at.to_rfc3339(),
    })
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

/// Whether two paths belong to the same project: equal, or one is an ancestor of the other —
/// compared on **path-component** boundaries so `/foo` no longer matches `/foobar`, and a bare
/// substring like `src` no longer drags in every unrelated repo. The query may be relative (e.g.
/// just a project name), in which case `b` matching is by trailing-component containment.
fn paths_related(a: &std::path::Path, b: &std::path::Path) -> bool {
    let ac: Vec<_> = a.components().collect();
    let bc: Vec<_> = b.components().collect();
    let (short, long) = if ac.len() <= bc.len() { (&ac, &bc) } else { (&bc, &ac) };
    // Ancestor relationship: `short` is a component-wise prefix of `long`.
    if long.starts_with(short.as_slice()) {
        return true;
    }
    // Relative query (e.g. "my-project"): match if it appears as a contiguous run of components.
    if short.iter().any(|c| matches!(c, std::path::Component::Normal(_))) && !short.is_empty() {
        long.windows(short.len()).any(|w| w == short.as_slice())
    } else {
        false
    }
}

fn project_sessions(args: &Value) -> anyhow::Result<String> {
    let cwd = arg_str(args, "cwd").ok_or_else(|| anyhow::anyhow!("missing required argument: cwd"))?;
    let harness = parse_harness(args)?;
    let limit = arg_usize(args, "limit", 20);

    let query = std::path::Path::new(cwd);
    let mut refs: Vec<SessionRef> = cv_core::discover_all()
        .into_iter()
        .filter(|r| harness.is_none_or(|h| r.harness == h))
        .filter(|r| r.cwd.as_deref().map(|p| paths_related(p, query)).unwrap_or(false))
        .collect();

    sort_newest_first(&mut refs);
    refs.truncate(limit);

    let out: Vec<Value> = refs.iter().map(ref_to_json).collect();
    Ok(serde_json::to_string_pretty(&json!(out))?)
}

fn search_sessions(args: &Value) -> anyhow::Result<String> {
    let query = arg_str(args, "query").ok_or_else(|| anyhow::anyhow!("missing required argument: query"))?;
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
    let id = arg_str(args, "id").ok_or_else(|| anyhow::anyhow!("missing required argument: id"))?;
    let harness = parse_harness(args)?;
    let format = arg_str(args, "format").unwrap_or("markdown");

    let (sref, adapter) =
        cv_core::find(id, harness)?.ok_or_else(|| anyhow::anyhow!("no session found for id {id:?}"))?;
    let session: Session = adapter.parse(&sref)?;

    match format {
        "json" => Ok(serde_json::to_string_pretty(&session)?),
        "markdown" | "" => Ok(cv_core::render::to_markdown(&session)),
        other => anyhow::bail!("unknown format {other:?} (expected 'markdown' or 'json')"),
    }
}

// ---------------------------------------------------------------------------
// recall — semantic "have I solved this before?" across the whole corpus
// ---------------------------------------------------------------------------

/// Semantic recall: find the most relevant past sessions for `query` and return, for each, a short
/// rendered SPAN of the messages around the best textual match — so a running agent can pull prior
/// context mid-task. Tries `cv_search::semantic_search` first; on any error (e.g. no embedding store
/// built) falls back to `cv_search::text_search`. Optionally filtered to one harness.
fn recall(args: &Value) -> anyhow::Result<String> {
    let query = arg_str(args, "query").ok_or_else(|| anyhow::anyhow!("missing required argument: query"))?;
    let k = arg_usize(args, "k", 5).max(1);
    let harness = parse_harness(args)?;
    let cwd_needle = arg_str(args, "cwd_contains");

    // We may filter by harness/cwd *after* searching, so over-fetch a bit to still fill `k`.
    let fetch = if harness.is_some() || cwd_needle.is_some() {
        (k * 4).max(20)
    } else {
        k
    };

    let (mut hits, mode) = match cv_search::semantic_search(None, query, fetch) {
        Ok(h) => (h, "semantic"),
        Err(e) => {
            eprintln!("cv-mcp: recall semantic search unavailable ({e:#}); falling back to text search");
            match cv_search::text_search(None, query, fetch) {
                Ok(h) => (h, "text"),
                Err(e2) => {
                    anyhow::bail!("recall failed: semantic ({e:#}) and text ({e2:#}) search both unavailable")
                }
            }
        }
    };

    if let Some(h) = harness {
        let want = h.as_str();
        hits.retain(|hit| hit.harness == want);
    }
    if let Some(needle) = cwd_needle {
        hits.retain(|hit| hit.cwd.as_deref().is_some_and(|c| c.contains(needle)));
    }
    hits.truncate(k);

    // Keep the whole response compact: cap total bytes of rendered spans across all hits.
    const TOTAL_SPAN_BUDGET: usize = 8_000;
    let per_hit_budget = (TOTAL_SPAN_BUDGET / k).max(400);

    let out: Vec<Value> = hits
        .iter()
        .map(|hit| {
            let span = extract_span(&hit.id, &hit.harness, query, per_hit_budget).unwrap_or_default();
            json!({
                "id": hit.id,
                "harness": hit.harness,
                "cwd": hit.cwd,
                "title": hit.title,
                "score": hit.score,
                "why": hit.snippet,
                "span": span,
            })
        })
        .collect();

    Ok(serde_json::to_string_pretty(&json!({
        "mode": mode,
        "query": query,
        "results": out,
    }))?)
}

/// Load a session and render a short excerpt of the messages around the best textual match of
/// `query`, capped at ~`budget` bytes. Returns an empty string if the session can't be loaded.
fn extract_span(id: &str, harness: &str, query: &str, budget: usize) -> Option<String> {
    let h = Harness::parse(harness);
    let (sref, adapter) = cv_core::find(id, h).ok().flatten()?;
    let session = adapter.parse(&sref).ok()?;
    if session.messages.is_empty() {
        return None;
    }

    let needle = query.to_lowercase();
    // Find the message whose combined text best matches the query (most query *words* present,
    // tie-broken by an exact substring hit). Falls back to the first message.
    let words: Vec<&str> = needle.split_whitespace().filter(|w| w.len() > 2).collect();
    let mut best_idx = 0usize;
    let mut best_score = -1i64;
    for (i, m) in session.messages.iter().enumerate() {
        let text = message_text(m).to_lowercase();
        if text.trim().is_empty() {
            continue;
        }
        let mut score = 0i64;
        if text.contains(&needle) {
            score += 100;
        }
        for w in &words {
            if text.contains(w) {
                score += 1;
            }
        }
        if score > best_score {
            best_score = score;
            best_idx = i;
        }
    }

    // Take a small window of messages around the best match (one before, the match, one after).
    let lo = best_idx.saturating_sub(1);
    let hi = (best_idx + 2).min(session.messages.len());

    let mut out = String::new();
    for m in &session.messages[lo..hi] {
        let role = cv_core::render::role_label(m.role);
        let body = message_text(m);
        let body = body.trim();
        if body.is_empty() {
            continue;
        }
        let remaining = budget.saturating_sub(out.len());
        if remaining < 40 {
            out.push('…');
            break;
        }
        out.push_str(role);
        out.push_str(": ");
        if body.len() > remaining {
            let end = floor_char_boundary(body, remaining);
            out.push_str(&body[..end]);
            out.push('…');
        } else {
            out.push_str(body);
        }
        out.push_str("\n\n");
    }

    let trimmed = out.trim_end();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

// Ensure stderr logging is line-buffered-ish on panic paths; not strictly required.
#[allow(dead_code)]
fn flush_stderr() {
    let _ = std::io::stderr().flush();
}
