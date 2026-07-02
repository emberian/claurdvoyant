//! Protocol-level integration tests for the cv-mcp stdio JSON-RPC server: spawn the real binary,
//! run the MCP handshake, list tools, and call them against a temp-home fixture corpus.
//!
//! Hermetic: a fake `$HOME` carries the claude fixtures and `$CLUSTERVISION_HOME` the index/board
//! state; both are passed only to the child process's environment.

use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// A running cv-mcp server over stdio, with a line reader on a side thread so a hung server
/// fails the test with a timeout instead of wedging the run.
struct Server {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    base: PathBuf,
}

impl Server {
    fn spawn(tag: &str) -> Server {
        let base = std::env::temp_dir().join(format!(
            "cv-mcp-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = base.join("home");
        let cv_home = base.join("cvhome");
        let proj = home.join(".claude/projects/-work-proj");
        fs::create_dir_all(&proj).unwrap();
        fs::create_dir_all(&cv_home).unwrap();

        // Fixture corpus: two tiny claude sessions.
        let alpha = [
            json!({"type": "ai-title", "aiTitle": "alpha adventures"}),
            json!({"type": "user", "uuid": "u1", "timestamp": "2026-01-01T10:00:00Z",
                   "cwd": "/work/proj",
                   "message": {"role": "user", "content": "please fix the zebrafish migration"}}),
            json!({"type": "assistant", "uuid": "a1", "timestamp": "2026-01-02T10:00:00Z",
                   "message": {"role": "assistant", "content": [
                       {"type": "text", "text": "done — the migration is fixed"}]}}),
        ];
        let beta = [
            json!({"type": "user", "uuid": "b1", "timestamp": "2026-03-01T09:00:00Z",
                   "cwd": "/work/proj",
                   "message": {"role": "user", "content": "how many quokkas in the census"}}),
            json!({"type": "assistant", "uuid": "b2", "timestamp": "2026-03-01T09:05:00Z",
                   "message": {"role": "assistant", "content": [
                       {"type": "text", "text": "seventeen quokkas"}]}}),
        ];
        for (name, lines) in [("alphasess", &alpha[..]), ("betasess", &beta[..])] {
            let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
            fs::write(proj.join(format!("{name}.jsonl")), body).unwrap();
        }

        let mut child = Command::new(env!("CARGO_BIN_EXE_cv-mcp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env("HOME", &home)
            .env("CLUSTERVISION_HOME", &cv_home)
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .spawn()
            .expect("cv-mcp should spawn");

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Server {
            child,
            stdin,
            lines: rx,
            base,
        }
    }

    /// Send one raw line to the server.
    fn send_raw(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("write to cv-mcp stdin");
        self.stdin.flush().unwrap();
    }

    fn send(&mut self, v: &Value) {
        self.send_raw(&v.to_string());
    }

    /// Next response line as JSON (10s timeout → test failure, not a hang).
    fn recv(&mut self) -> Value {
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(10))
            .expect("timed out waiting for a cv-mcp response");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("non-JSON response {line:?}: {e}"))
    }

    /// Round-trip a request and assert the JSON-RPC envelope (version + echoed id).
    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let resp = self.recv();
        assert_eq!(resp["jsonrpc"], "2.0", "envelope: {resp}");
        assert_eq!(resp["id"], json!(id), "id must echo: {resp}");
        resp
    }

    /// Call a tool and return `(text, is_error)` from the MCP tool result.
    fn call_tool(&mut self, id: u64, name: &str, args: Value) -> (String, bool) {
        let resp = self.request(id, "tools/call", json!({"name": name, "arguments": args}));
        let result = &resp["result"];
        assert!(result.is_object(), "tools/call must return a result: {resp}");
        let content = result["content"]
            .as_array()
            .unwrap_or_else(|| panic!("tool result must carry content: {resp}"));
        assert_eq!(content[0]["type"], "text", "{resp}");
        let text = content[0]["text"].as_str().unwrap_or_default().to_string();
        let is_error = result["isError"].as_bool().unwrap_or(false);
        (text, is_error)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        fs::remove_dir_all(&self.base).ok();
    }
}

#[test]
fn handshake_and_tool_schemas() {
    let mut s = Server::spawn("handshake");

    // initialize: protocol version, server info, capabilities.
    let resp = s.request(1, "initialize", json!({"protocolVersion": "2025-06-18"}));
    let r = &resp["result"];
    assert!(r["protocolVersion"].is_string(), "{resp}");
    assert_eq!(r["serverInfo"]["name"], "clustervision", "{resp}");
    assert!(r["capabilities"]["tools"].is_object(), "{resp}");

    // The initialized notification must produce no response — the next reply must be for id 2.
    s.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let resp = s.request(2, "ping", json!({}));
    assert!(resp["result"].is_object(), "{resp}");

    // tools/list: every tool is named, described, and carries a valid object schema whose
    // `required` names actually exist in `properties`.
    let resp = s.request(3, "tools/list", json!({}));
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    assert!(tools.len() >= 5, "expected the full toolset, got {}", tools.len());
    let mut names = std::collections::HashSet::new();
    for t in tools {
        let name = t["name"].as_str().unwrap_or_else(|| panic!("unnamed tool: {t}"));
        assert!(names.insert(name.to_string()), "duplicate tool name {name}");
        assert!(
            !t["description"].as_str().unwrap_or("").is_empty(),
            "{name} needs a description"
        );
        let schema = &t["inputSchema"];
        assert_eq!(schema["type"], "object", "{name}: inputSchema must be type:object");
        let props = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{name}: properties must be an object"));
        for (pname, p) in props {
            assert!(p["type"].is_string(), "{name}.{pname}: every property needs a type");
            assert!(
                !p["description"].as_str().unwrap_or("").is_empty(),
                "{name}.{pname}: every property needs a description"
            );
        }
        if let Some(req) = schema["required"].as_array() {
            for r in req {
                let r = r.as_str().expect("required entries are strings");
                assert!(props.contains_key(r), "{name}: required {r:?} missing from properties");
            }
        }
    }
    for expected in [
        "list_sessions",
        "search_sessions",
        "read_session",
        "project_sessions",
        "recall",
        "observe_stream",
    ] {
        assert!(names.contains(expected), "missing core tool {expected}");
    }
}

#[test]
fn tool_calls_against_fixture_corpus() {
    let mut s = Server::spawn("tools");
    s.request(1, "initialize", json!({}));

    // list_sessions: both fixtures, newest (alpha, updated 2026-01-02… wait beta is 03-01) first.
    let (text, is_err) = s.call_tool(2, "list_sessions", json!({}));
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).expect("list_sessions returns JSON");
    let arr = v.as_array().expect("array");
    assert_eq!(arr.len(), 2, "{text}");
    assert_eq!(arr[0]["id"], "betasess", "newest-first: {text}");
    assert_eq!(arr[1]["id"], "alphasess", "{text}");
    assert_eq!(arr[1]["title"], "alpha adventures", "{text}");
    assert_eq!(arr[0]["harness"], "claude", "{text}");
    assert!(arr[0]["message_count"].as_u64().unwrap() >= 2, "{text}");

    // limit + harness filtering.
    let (text, _) = s.call_tool(3, "list_sessions", json!({"limit": 1}));
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1, "{text}");
    let (text, is_err) = s.call_tool(4, "list_sessions", json!({"harness": "warpdrive"}));
    assert!(is_err, "unknown harness must be a tool error: {text}");
    assert!(text.contains("unknown harness"), "{text}");

    // search_sessions: finds the planted text with a snippet.
    let (text, is_err) = s.call_tool(5, "search_sessions", json!({"query": "zebrafish"}));
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    let hits = v.as_array().unwrap();
    assert_eq!(hits.len(), 1, "{text}");
    assert_eq!(hits[0]["id"], "alphasess", "{text}");
    assert!(hits[0]["snippet"].as_str().unwrap().contains("zebrafish"), "{text}");
    // Missing required arg: tool error, not a crash.
    let (text, is_err) = s.call_tool(6, "search_sessions", json!({}));
    assert!(is_err, "{text}");
    assert!(text.contains("query"), "{text}");

    // read_session: markdown by default (prefix match allowed), JSON on request.
    let (text, is_err) = s.call_tool(7, "read_session", json!({"id": "alpha"}));
    assert!(!is_err, "{text}");
    assert!(text.contains("zebrafish migration"), "{text}");
    let (text, _) = s.call_tool(8, "read_session", json!({"id": "betasess", "format": "json"}));
    let v: Value = serde_json::from_str(&text).expect("json format parses");
    assert_eq!(v["id"], "betasess", "{text}");
    assert_eq!(v["messages"].as_array().unwrap().len(), 2, "{text}");
    let (text, is_err) = s.call_tool(9, "read_session", json!({"id": "nonexistent"}));
    assert!(is_err, "{text}");
    assert!(text.contains("no session found"), "{text}");
    let (text, is_err) = s.call_tool(10, "read_session", json!({"id": "alpha", "format": "yaml"}));
    assert!(is_err, "{text}");
    assert!(text.contains("unknown format"), "{text}");

    // project_sessions: exact cwd, ancestor, trailing component — but not a path-prefix fragment.
    for (q, want) in [("/work/proj", 2), ("/work", 2), ("proj", 2), ("/wo", 0)] {
        let (text, is_err) = s.call_tool(11, "project_sessions", json!({"cwd": q}));
        assert!(!is_err, "{text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v.as_array().unwrap().len(), want, "cwd={q}: {text}");
    }

    // board round-trip: post → read.
    let (text, is_err) = s.call_tool(12, "board_post", json!({"channel": "testchan", "body": "hello fleet"}));
    assert!(!is_err, "{text}");
    let posted: Value = serde_json::from_str(&text).unwrap();
    assert!(posted["id"].is_string(), "{text}");
    let (text, _) = s.call_tool(13, "board_read", json!({"channel": "testchan"}));
    let msgs: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(msgs.as_array().unwrap().len(), 1, "{text}");
    assert_eq!(msgs[0]["body"], "hello fleet", "{text}");

    // board_claim: granted, then contended for another owner, then released.
    let (text, _) = s.call_tool(
        14,
        "board_claim",
        json!({"channel": "testchan", "key": "taskA", "from": "me"}),
    );
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["granted"], true, "{text}");
    let (text, _) = s.call_tool(
        15,
        "board_claim",
        json!({"channel": "testchan", "key": "taskA", "from": "other"}),
    );
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["granted"], false, "{text}");
    let (text, _) = s.call_tool(
        16,
        "board_release",
        json!({"channel": "testchan", "key": "taskA", "from": "me"}),
    );
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["released"], true, "{text}");
    // Releasing a key you don't hold reports false (idempotent), never an error.
    let (text, is_err) = s.call_tool(
        17,
        "board_release",
        json!({"channel": "testchan", "key": "taskA", "from": "me"}),
    );
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["released"], false, "{text}");

    // recall with no index built: an honest tool error naming both failed paths.
    let (text, is_err) = s.call_tool(18, "recall", json!({"query": "zebrafish"}));
    assert!(is_err, "recall without an index should error: {text}");
    assert!(text.contains("recall failed"), "{text}");
}

#[test]
fn protocol_error_handling() {
    let mut s = Server::spawn("errors");
    s.request(1, "initialize", json!({}));

    // Unknown method with an id → -32601.
    let resp = s.request(2, "wizardry/cast", json!({}));
    assert_eq!(resp["error"]["code"], -32601, "{resp}");

    // Unparseable line → -32700 with a null id (and the server survives).
    s.send_raw("this is not json {");
    let resp = s.recv();
    assert_eq!(resp["error"]["code"], -32700, "{resp}");
    assert!(resp["id"].is_null(), "{resp}");

    // Unknown tool → a tool-level error result, not a protocol error.
    let (text, is_err) = s.call_tool(3, "tools_that_do_not_exist", json!({}));
    assert!(is_err, "{text}");
    assert!(text.contains("unknown tool"), "{text}");

    // A *notification* (no id) must never get a response — even for a known method. The next
    // line on the wire after the notifications must be the reply to the request that follows.
    s.send(&json!({"jsonrpc": "2.0", "method": "ping"}));
    s.send(&json!({"jsonrpc": "2.0", "method": "tools/list"}));
    let resp = s.request(4, "ping", json!({}));
    assert_eq!(
        resp["id"],
        json!(4),
        "a notification (no id) must not produce a response; got an extra reply: {resp}"
    );

    // Still alive and well after all of that.
    let resp = s.request(5, "tools/list", json!({}));
    assert!(resp["result"]["tools"].is_array(), "{resp}");
}

/// Requests are dispatched concurrently: a parked long-poll (board_await on a channel nobody
/// posts to) must not head-of-line-block a fast request issued after it. Responses come back out
/// of order — the fast one first — and both complete well under the long-poll's timeout budget.
#[test]
fn slow_long_poll_does_not_block_other_requests() {
    let mut s = Server::spawn("concurrent");
    s.request(1, "initialize", json!({}));

    let start = std::time::Instant::now();
    s.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
        "name": "board_await",
        "arguments": {"channel": "quiet", "regex": "NEVER_MATCHES", "timeout_secs": 6, "interval_secs": 1}}}));
    s.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
        "name": "list_sessions", "arguments": {}}}));

    let first = s.recv();
    let elapsed = start.elapsed();
    assert_eq!(first["id"], json!(3), "the fast request must answer first: {first}");
    assert!(
        elapsed < Duration::from_secs(4),
        "list_sessions took {elapsed:?} — blocked behind the 6s long-poll?"
    );
    assert_eq!(first["result"]["isError"], false, "{first}");

    // The long-poll still completes on its own schedule (timed out, unmatched).
    let second = s.recv();
    assert_eq!(second["id"], json!(2), "{second}");
    let text = second["result"]["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("\"timed_out\": true"), "{second}");
}

/// A present-but-malformed numeric argument is the caller's protocol mistake: JSON-RPC `-32602
/// Invalid params`, not a silent fallback to the default (and not an isError tool result). An
/// integral float is tolerated — clients often serialize `1` as `1.0`.
#[test]
fn malformed_numeric_args_are_invalid_params() {
    let mut s = Server::spawn("badparams");
    s.request(1, "initialize", json!({}));

    for (id, bad) in [(2u64, json!("ten")), (3, json!(-3)), (4, json!(1.5))] {
        let resp = s.request(
            id,
            "tools/call",
            json!({"name": "list_sessions", "arguments": {"limit": bad}}),
        );
        assert_eq!(resp["error"]["code"], -32602, "limit={bad}: {resp}");
        assert!(
            resp["error"]["message"].as_str().unwrap_or_default().contains("limit"),
            "the error must name the offending argument: {resp}"
        );
        assert!(resp["result"].is_null(), "an error response carries no result: {resp}");
    }

    // Integral float: accepted, behaves like the integer.
    let (text, is_err) = s.call_tool(5, "list_sessions", json!({"limit": 1.0}));
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1, "{text}");

    // Other tools route through the same guard (spot-check a long-poll's timeout).
    let resp = s.request(
        6,
        "tools/call",
        json!({"name": "board_await", "arguments": {"channel": "c", "regex": ".", "timeout_secs": "soon"}}),
    );
    assert_eq!(resp["error"]["code"], -32602, "{resp}");

    // Still alive afterwards.
    let resp = s.request(7, "ping", json!({}));
    assert!(resp["result"].is_object(), "{resp}");
}


/// `observe_stream` is the non-blocking tail of `await_omen`: bounded, read-only, cursor-driven.
/// Covers the spec's required cases — an absent/empty corpus yields a bounded EMPTY result, the
/// baseline call emits no backlog, the cursor drains only newly-appended messages, max_messages
/// bounds the batch (more_pending), and NO board side-effect is produced.
#[test]
fn observe_stream_bounded_read_only_tail() {
    let mut s = Server::spawn("observe");
    s.request(1, "initialize", json!({}));

    // 1) Empty corpus (a filter that matches NO session) → a bounded, empty, baseline result.
    //    No crash, no error, an empty `messages`, and a usable cursor.
    let (text, is_err) = s.call_tool(2, "observe_stream", json!({"cwd_contains": "/no/such/dir"}));
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).expect("observe_stream returns JSON");
    assert_eq!(v["baseline"], true, "first call is a baseline: {text}");
    assert_eq!(v["count"], 0, "absent corpus → zero messages: {text}");
    assert_eq!(v["messages"].as_array().unwrap().len(), 0, "{text}");
    assert_eq!(v["more_pending"], false, "{text}");
    assert!(v["cursor"].as_str().unwrap().contains("\"v\":1"), "cursor is versioned: {text}");

    // 2) Baseline over the REAL fixture corpus emits no backlog (only future activity is tailed),
    //    and hands back a cursor recording the current tail.
    let (text, is_err) = s.call_tool(3, "observe_stream", json!({"cwd_contains": "/work/proj"}));
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["baseline"], true, "{text}");
    assert_eq!(v["count"], 0, "baseline emits no history: {text}");
    let cursor = v["cursor"].as_str().unwrap().to_string();

    // 3) Replaying that cursor with no new activity drains nothing (and is no longer a baseline).
    let (text, is_err) = s.call_tool(
        4,
        "observe_stream",
        json!({"cwd_contains": "/work/proj", "since_cursor": cursor}),
    );
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["baseline"], false, "a cursor call is not a baseline: {text}");
    assert_eq!(v["count"], 0, "no new activity → nothing drained: {text}");

    // 4) Append a NEW message to the alpha session on disk; the next cursor call drains exactly it.
    let proj = s.base.join("home/.claude/projects/-work-proj");
    let extra = json!({"type": "assistant", "uuid": "a2",
                       "timestamp": "2026-06-19T12:00:00Z",
                       "message": {"role": "assistant", "content": [
                           {"type": "text", "text": "ZEBRA_TAIL_MARKER appended later"}]}});
    let mut body = fs::read_to_string(proj.join("alphasess.jsonl")).unwrap();
    body.push_str(&format!("{extra}\n"));
    fs::write(proj.join("alphasess.jsonl"), body).unwrap();

    let (text, is_err) = s.call_tool(
        5,
        "observe_stream",
        json!({"cwd_contains": "/work/proj", "since_cursor": cursor}),
    );
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    let msgs = v["messages"].as_array().unwrap();
    assert!(
        msgs.iter().any(|m| m["text"].as_str().unwrap_or("").contains("ZEBRA_TAIL_MARKER")),
        "the appended message must be drained: {text}"
    );
    assert!(
        msgs.iter().all(|m| m["text"].as_str().unwrap_or("").contains("ZEBRA_TAIL_MARKER")
            || !m["text"].as_str().unwrap_or("").contains("zebrafish")),
        "previously-seen messages must NOT re-emit: {text}"
    );

    // 5) max_messages bounds the batch. Append several messages, baseline-reset, then drain with a
    //    cap of 1 and assert more_pending is flagged.
    let mut body = fs::read_to_string(proj.join("alphasess.jsonl")).unwrap();
    for i in 0..3 {
        let m = json!({"type": "assistant", "uuid": format!("burst{i}"),
                       "timestamp": "2026-06-19T13:00:00Z",
                       "message": {"role": "assistant", "content": [
                           {"type": "text", "text": format!("burst message {i}")}]}});
        body.push_str(&format!("{m}\n"));
    }
    fs::write(proj.join("alphasess.jsonl"), &body).unwrap();
    // Fresh baseline so we know exactly what's pending, then append AFTER baselining.
    let (text, _) = s.call_tool(6, "observe_stream", json!({"cwd_contains": "/work/proj"}));
    let base2 = serde_json::from_str::<Value>(&text).unwrap()["cursor"].as_str().unwrap().to_string();
    let mut body = fs::read_to_string(proj.join("alphasess.jsonl")).unwrap();
    for i in 0..3 {
        let m = json!({"type": "assistant", "uuid": format!("after{i}"),
                       "timestamp": "2026-06-19T14:00:00Z",
                       "message": {"role": "assistant", "content": [
                           {"type": "text", "text": format!("after message {i}")}]}});
        body.push_str(&format!("{m}\n"));
    }
    fs::write(proj.join("alphasess.jsonl"), body).unwrap();
    let (text, is_err) = s.call_tool(
        7,
        "observe_stream",
        json!({"cwd_contains": "/work/proj", "since_cursor": base2, "max_messages": 1}),
    );
    assert!(!is_err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["count"], 1, "max_messages=1 bounds the batch: {text}");
    assert_eq!(v["more_pending"], true, "the rest must be flagged pending: {text}");

    // 6) READ-ONLY: observe_stream must NEVER write the board. Reading the channel observe_stream
    //    was scoped to ("/work/proj"/"fleet") returns nothing it could have posted. Assert the
    //    board has no observe_stream-authored traffic on a fresh channel.
    let (text, _) = s.call_tool(8, "board_read", json!({"channel": "fleet"}));
    let msgs: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        msgs.as_array().unwrap().len(),
        0,
        "observe_stream must not write the board (fleet channel must be empty): {text}"
    );
}
