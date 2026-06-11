//! `cvd serve` — a tiny local HTTP server exposing the fleet's state as JSON.
//!
//! A static browser dashboard (served elsewhere) polls these endpoints live, turning `cvd` into the
//! fleet's live hub. We deliberately use a lightweight *synchronous* server ([`tiny_http`]) with a
//! small worker-thread pool rather than an async stack — the workload is a handful of pollers, and
//! simplicity wins.
//!
//! All responses are JSON with permissive CORS (`Access-Control-Allow-Origin: *`) so a dashboard on
//! any origin can consume them; `OPTIONS` preflight is answered with a 204.

use anyhow::Result;
use cv_core::{Flow, Harness, Message, MessageSink, ParseOptions, Session};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server};

/// Run the HTTP server until the process is killed. Never returns under normal operation.
pub fn run(host: &str, port: u16) -> Result<()> {
    let server = Server::http((host, port))
        .map_err(|e| anyhow::anyhow!("failed to bind {host}:{port}: {e}"))?;
    eprintln!("cvd serve on http://{host}:{port}");

    let server = Arc::new(server);
    let mut workers = Vec::new();
    // A handful of workers is plenty for dashboard polling; they share the listener.
    for _ in 0..4 {
        let server = Arc::clone(&server);
        workers.push(std::thread::spawn(move || {
            for request in server.incoming_requests() {
                handle(request);
            }
        }));
    }
    for w in workers {
        // If a worker panics we keep serving on the others, but join here so `run` blocks forever.
        let _ = w.join();
    }
    Ok(())
}

/// Handle one request, never panicking: any error becomes a JSON error response.
fn handle(request: Request) {
    let method = request.method().clone();
    let raw_url = request.url().to_string();

    // CORS preflight: answer every OPTIONS with a permissive 204.
    if method == Method::Options {
        let _ = request.respond(no_content());
        return;
    }

    // Split off the query string, then normalise the path (strip trailing slash, decode segments).
    let (path, query) = match raw_url.split_once('?') {
        Some((p, q)) => (p, q),
        None => (raw_url.as_str(), ""),
    };
    let trimmed = path.trim_end_matches('/');
    let segments: Vec<String> = trimmed
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| urlencoding::decode(s).map(|c| c.into_owned()).unwrap_or_else(|_| s.to_string()))
        .collect();

    // Only GET (besides the OPTIONS handled above) is supported.
    if method != Method::Get {
        let _ = request.respond(json_response(405, &json!({"error": "method not allowed"})));
        return;
    }

    let (status, body) = route(&segments, query);
    let _ = request.respond(json_response(status, &body));
}

/// Route a decoded path + raw query string to a `(status, json)` pair. Never panics.
fn route(segments: &[String], query: &str) -> (u16, Value) {
    let parts: Vec<&str> = segments.iter().map(|s| s.as_str()).collect();
    match parts.as_slice() {
        ["api", "health"] => ok(json!({
            "ok": true,
            "harnesses": Harness::ALL.iter().map(|h| h.as_str()).collect::<Vec<_>>(),
        })),

        ["api", "sessions"] => sessions(query),

        ["api", "session", harness, id] => session(harness, id),

        ["api", "session", harness, id, "messages"] => messages(harness, id, query),

        ["api", "session", harness, id, "events"] => session_events(harness, id, query),

        ["api", "touched"] => touched(query),

        ["api", "session", harness, id, "subagents"] => subagents(harness, id),

        ["api", "session", harness, parent, "subagent", agent] => subagent(harness, parent, agent),

        ["api", "board", channel] => board(channel, query),

        ["api", "claims", channel] => claims(channel),

        ["api", "who", channel] => who(channel, query),

        ["api", "channels"] => match cv_core::board::channels() {
            Ok(c) => ok(json!(c)),
            Err(e) => err(500, &e.to_string()),
        },

        _ => err(404, "not found"),
    }
}

/// `GET /api/sessions?harness=&cwd=&limit=` — session metadata, filtered + newest-first.
fn sessions(query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    let harness = params.get("harness");
    let cwd = params.get("cwd");
    let limit = params.get("limit").and_then(|l| l.parse::<usize>().ok());

    // Validate harness up front so a typo is a clear 400 rather than silently empty.
    let harness = match harness {
        Some(h) => match Harness::parse(&h) {
            Some(h) => Some(h),
            None => return err(400, &format!("unknown harness {h:?}")),
        },
        None => None,
    };

    // The fast catalog read (~ms when warm); transparently escalates to a full scan when cold.
    let mut refs = cv_core::sessions();
    if let Some(h) = harness {
        refs.retain(|r| r.harness == h);
    }
    if let Some(needle) = &cwd {
        refs.retain(|r| {
            r.cwd
                .as_ref()
                .map(|c| c.to_string_lossy().contains(needle.as_str()))
                .unwrap_or(false)
        });
    }
    // Newest activity first (updated_at, falling back to created_at).
    refs.sort_by(|a, b| {
        let ka = a.updated_at.or(a.created_at);
        let kb = b.updated_at.or(b.created_at);
        kb.cmp(&ka)
    });
    if let Some(n) = limit {
        refs.truncate(n);
    }

    let out: Vec<Value> = refs
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "harness": r.harness.as_str(),
                "cwd": r.cwd.as_ref().map(|c| c.to_string_lossy()),
                "title": r.title,
                "updated_at": r.updated_at,
                "created_at": r.created_at,
                "message_count": r.message_count,
            })
        })
        .collect();
    ok(json!(out))
}

/// `GET /api/session/{harness}/{id}` — the full parsed session, or 404.
fn session(harness: &str, id: &str) -> (u16, Value) {
    let h = match Harness::parse(harness) {
        Some(h) => h,
        None => return err(400, &format!("unknown harness {harness:?}")),
    };
    match cv_core::find(id, Some(h)) {
        Ok(Some((r, adapter))) => match adapter.parse(&r) {
            Ok(session) => match serde_json::to_value(&session) {
                Ok(v) => ok(v),
                Err(e) => err(500, &e.to_string()),
            },
            Err(e) => err(500, &format!("parse failed: {e:#}")),
        },
        Ok(None) => err(404, "session not found"),
        Err(e) => err(500, &e.to_string()),
    }
}

/// Collects the message window `[start, end)` as serialized IR, never holding more than the
/// window: out-of-window messages pass by as unmaterialized lazy handles, and the stream stops
/// at `end` (after noting whether a message exists there — the `has_more` probe).
struct WindowSink {
    resolver: cv_core::Resolver,
    /// Index of the next message to arrive (pre-set to `start` on the seek path, 0 on the full).
    idx: usize,
    start: usize,
    /// Exclusive window end; `None` streams to EOF.
    end: Option<usize>,
    msgs: Vec<Value>,
    /// A message at `end` exists (so the window isn't the tail of the session).
    more: bool,
    /// Session metadata the stream delivered (codex meta / the seek store's snapshot / the
    /// parse-bridge), when any did — claude's native stream emits none.
    meta: Option<Value>,
    fail: Option<String>,
}

impl MessageSink for WindowSink {
    fn meta(&mut self, s: &Session) {
        if self.meta.is_none() {
            self.meta = Some(json!({
                "title": s.title,
                "model": s.model,
                "cwd": s.cwd.as_ref().map(|c| c.to_string_lossy()),
            }));
        }
    }

    fn message(&mut self, m: Message) -> Flow {
        let idx = self.idx;
        self.idx += 1;
        if idx < self.start {
            return Flow::Continue; // before the window: skip without touching content bytes
        }
        if self.end.is_some_and(|e| idx >= e) {
            self.more = true;
            return Flow::Stop; // at the window's end: note the existence, read no further
        }
        let mut m = m;
        m.materialize(&self.resolver); // resolve lazy spans for just this message
        match serde_json::to_value(&m) {
            Ok(v) => {
                self.msgs.push(v);
                Flow::Continue
            }
            Err(e) => {
                self.fail = Some(e.to_string());
                Flow::Stop
            }
        }
    }
}

/// `GET /api/session/{harness}/{id}/messages?start=&end=` — the window `[start, end)` of the
/// session's messages as IR JSON, without parsing the rest of a huge transcript when possible.
///
/// First tries the seekable-session store ([`cv_core::offsets::stream_range`]: jump straight to
/// message `start`'s recorded byte offset); with no/stale offsets it falls back to a full stream
/// that still **stops at `end`** (the `cv show --range` pattern), so the tail is never read.
/// The reply carries the window's actual indices, `total_known` (+ `total` when the stream
/// reached EOF so the count is exact), and `has_more`.
fn messages(harness: &str, id: &str, query: &str) -> (u16, Value) {
    let h = match Harness::parse(harness) {
        Some(h) => h,
        None => return err(400, &format!("unknown harness {harness:?}")),
    };
    let params = Query::parse(query);
    // Window bounds must parse cleanly — a typo'd index is a clear 400, not a silent full read.
    let idx_param = |key: &str| -> Result<Option<usize>, (u16, Value)> {
        match params.get(key) {
            None => Ok(None),
            Some(v) => v
                .parse::<usize>()
                .map(Some)
                .map_err(|_| err(400, &format!("{key} must be a non-negative integer, got {v:?}"))),
        }
    };
    let start = match idx_param("start") {
        Ok(v) => v.unwrap_or(0),
        Err(e) => return e,
    };
    let end = match idx_param("end") {
        Ok(v) => v,
        Err(e) => return e,
    };
    if end.is_some_and(|e| e < start) {
        return err(400, "end must be >= start");
    }

    let (r, adapter) = match cv_core::find(id, Some(h)) {
        Ok(Some(hit)) => hit,
        Ok(None) => return err(404, "session not found"),
        Err(e) => return err(500, &e.to_string()),
    };

    let opts = ParseOptions::lazy();
    let mut sink = WindowSink {
        resolver: cv_core::Resolver::new(Some(r.path.clone())),
        idx: start,
        start,
        end,
        msgs: Vec::new(),
        more: false,
        meta: None,
        fail: None,
    };
    // Ask the stream for one message past the window so `has_more` is known either way.
    let probe = end.map(|e| e.saturating_add(1));
    let seeked = if start > 0 {
        match cv_core::offsets::stream_range(&r, start, probe, &opts, &mut sink) {
            Ok(seeked) => seeked,
            Err(e) => return err(500, &format!("range read failed: {e:#}")),
        }
    } else {
        false
    };
    if !seeked {
        sink.idx = 0; // full stream counts from the top (skipping pre-window messages cheaply)
        if let Err(e) = adapter.stream(&r, &opts, &mut sink) {
            return err(500, &format!("stream failed: {e:#}"));
        }
    }
    if let Some(f) = sink.fail {
        return err(500, &f);
    }

    // The count is exact when the stream ran to EOF. The one blind spot: a seek-read that
    // delivered nothing can't distinguish "EOF exactly at start" from "start past EOF".
    let total_known = !sink.more && (!seeked || !sink.msgs.is_empty());
    ok(json!({
        "harness": r.harness.as_str(),
        "id": r.id,
        "start": start,
        "end": start + sink.msgs.len(),
        "has_more": sink.more,
        "total_known": total_known,
        "total": total_known.then_some(sink.idx),
        // Discovery-time count, as a hint while `total` is unknown.
        "message_count": r.message_count,
        "session": sink.meta,
        "messages": sink.msgs,
    }))
}

/// `GET /api/session/{harness}/{id}/events?kind=` — the session's extracted tool events
/// (file edits/reads, commands, errors) from the event catalog, in transcript order. A stale
/// or missing catalog row is (re)ingested on the spot, exactly like `cv events`.
fn session_events(harness: &str, id: &str, query: &str) -> (u16, Value) {
    let h = match Harness::parse(harness) {
        Some(h) => h,
        None => return err(400, &format!("unknown harness {harness:?}")),
    };
    let kind = Query::parse(query).get("kind");
    match cv_core::find(id, Some(h)) {
        Ok(Some((r, _))) => {
            use cv_core::events;
            if events::needs_ingest(&r, events::file_mtime_ns(&r.path)) {
                if let Err(e) = events::ingest_ref(&r) {
                    return err(500, &format!("event ingest failed: {e:#}"));
                }
            }
            let out: Vec<Value> = events::events_for(r.harness.as_str(), &r.id, kind.as_deref())
                .iter()
                .map(|e| {
                    json!({
                        "msg_idx": e.msg_idx,
                        "ts": e.ts,
                        "kind": e.kind,
                        "tool": e.tool,
                        "target": e.target,
                        "detail": e.detail,
                    })
                })
                .collect();
            ok(json!(out))
        }
        Ok(None) => err(404, "session not found"),
        Err(e) => err(500, &e.to_string()),
    }
}

/// `GET /api/touched?path=&edits_only=` — sessions whose tool events touched a file (exact or
/// suffix path match), newest activity first. Empty on a cold catalog, like `cv touched`.
fn touched(query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    let Some(path) = params.get("path") else {
        return err(400, "missing path parameter");
    };
    let edits_only = params
        .get("edits_only")
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let out: Vec<Value> = cv_core::events::sessions_touching(&path, edits_only)
        .iter()
        .map(|t| {
            json!({
                "harness": t.harness,
                "session_id": t.session_id,
                "title": t.title,
                "edits": t.edits,
                "reads": t.reads,
                "last_ts": t.last_ts,
            })
        })
        .collect();
    ok(json!(out))
}

/// `GET /api/session/{harness}/{id}/subagents` — lightweight refs for the sub-agents this session
/// spawned (e.g. Claude Code Task sub-agents), newest first. Empty array if none.
fn subagents(harness: &str, id: &str) -> (u16, Value) {
    let h = match Harness::parse(harness) {
        Some(h) => h,
        None => return err(400, &format!("unknown harness {harness:?}")),
    };
    match cv_core::find(id, Some(h)) {
        Ok(Some((r, _))) => {
            let out: Vec<Value> = cv_core::subagents_of(&r)
                .iter()
                .map(|s| {
                    json!({
                        "id": s.id,
                        "harness": s.harness.as_str(),
                        "cwd": s.cwd.as_ref().map(|c| c.to_string_lossy()),
                        "title": s.title,
                        "updated_at": s.updated_at,
                        "created_at": s.created_at,
                        "message_count": s.message_count,
                    })
                })
                .collect();
            ok(json!(out))
        }
        Ok(None) => err(404, "session not found"),
        Err(e) => err(500, &e.to_string()),
    }
}

/// `GET /api/session/{harness}/{parent}/subagent/{agent}` — the full parsed transcript of one
/// sub-agent (sub-agents aren't in the main pool, so they're loaded relative to their parent).
fn subagent(harness: &str, parent: &str, agent: &str) -> (u16, Value) {
    let h = match Harness::parse(harness) {
        Some(h) => h,
        None => return err(400, &format!("unknown harness {harness:?}")),
    };
    match cv_core::find(parent, Some(h)) {
        Ok(Some((r, adapter))) => {
            match cv_core::subagents_of(&r).into_iter().find(|s| s.id == agent) {
                Some(sr) => match adapter.parse(&sr) {
                    Ok(session) => match serde_json::to_value(&session) {
                        Ok(v) => ok(v),
                        Err(e) => err(500, &e.to_string()),
                    },
                    Err(e) => err(500, &format!("parse failed: {e:#}")),
                },
                None => err(404, "subagent not found"),
            }
        }
        Ok(None) => err(404, "parent session not found"),
        Err(e) => err(500, &e.to_string()),
    }
}

/// `GET /api/board/{channel}?since=&limit=` — board messages.
fn board(channel: &str, query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    let since = params.get("since");
    let limit = params
        .get("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(0); // 0 == unlimited in cv_core::board::read.
    match cv_core::board::read(channel, since.as_deref(), limit) {
        Ok(msgs) => ok(json!(msgs)),
        Err(e) => err(500, &e.to_string()),
    }
}

/// `GET /api/claims/{channel}` — active leases as `{key, owner, expires_at}`.
fn claims(channel: &str) -> (u16, Value) {
    match cv_core::board::active_claims(channel) {
        Ok(claims) => {
            let out: Vec<Value> = claims
                .into_iter()
                .map(|(key, owner, expires_at)| {
                    json!({ "key": key, "owner": owner, "expires_at": expires_at })
                })
                .collect();
            ok(json!(out))
        }
        Err(e) => err(500, &e.to_string()),
    }
}

/// `GET /api/who/{channel}?within_secs=` — recently-present agents.
fn who(channel: &str, query: &str) -> (u16, Value) {
    let params = Query::parse(query);
    let within = params
        .get("within_secs")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(60);
    match cv_core::board::who(channel, Duration::from_secs(within)) {
        Ok(names) => ok(json!(names)),
        Err(e) => err(500, &e.to_string()),
    }
}

// --- small helpers ---------------------------------------------------------

/// A trivially-parsed query string. Avoids pulling a URL crate for our handful of flat params.
struct Query(Vec<(String, String)>);

impl Query {
    fn parse(q: &str) -> Self {
        let pairs = q
            .split('&')
            .filter(|s| !s.is_empty())
            .map(|pair| {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                let dec = |s: &str| {
                    urlencoding::decode(&s.replace('+', " "))
                        .map(|c| c.into_owned())
                        .unwrap_or_else(|_| s.to_string())
                };
                (dec(k), dec(v))
            })
            .collect();
        Query(pairs)
    }

    /// First non-empty value for `key`, if any.
    fn get(&self, key: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(k, v)| k == key && !v.is_empty())
            .map(|(_, v)| v.clone())
    }
}

fn ok(value: Value) -> (u16, Value) {
    (200, value)
}

fn err(status: u16, msg: &str) -> (u16, Value) {
    (status, json!({ "error": msg }))
}

/// Build a JSON response with permissive CORS headers.
fn json_response(status: u16, body: &Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let data = serde_json::to_vec(body).unwrap_or_else(|_| b"{\"error\":\"serialize failed\"}".to_vec());
    let mut resp = Response::from_data(data).with_status_code(status);
    for h in cors_headers() {
        resp.add_header(h);
    }
    resp.add_header(header("Content-Type", "application/json"));
    resp
}

/// A 204 No Content with CORS headers, for `OPTIONS` preflight.
fn no_content() -> Response<std::io::Empty> {
    let mut resp = Response::empty(204);
    for h in cors_headers() {
        resp.add_header(h);
    }
    resp
}

fn cors_headers() -> Vec<Header> {
    vec![
        header("Access-Control-Allow-Origin", "*"),
        header("Access-Control-Allow-Methods", "GET, OPTIONS"),
        header("Access-Control-Allow-Headers", "*"),
    ]
}

fn header(field: &str, value: &str) -> Header {
    // These are all static, valid headers; the parse cannot fail in practice.
    Header::from_bytes(field.as_bytes(), value.as_bytes())
        .unwrap_or_else(|_| Header::from_bytes(&b"X-Invalid"[..], &b"1"[..]).unwrap())
}
