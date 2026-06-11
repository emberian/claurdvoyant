//! Integration tests for the cvd daemon binary: `sync` idempotency against a fixture corpus and
//! the `serve` HTTP endpoints (status codes, JSON shapes, query parsing) — all hermetic via a
//! temp `$HOME` + `$CLAURDVOYANT_HOME` passed only to child processes.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use std::fs;

struct World {
    base: PathBuf,
    home: PathBuf,
    cv_home: PathBuf,
}

impl World {
    fn new(tag: &str) -> World {
        let base = std::env::temp_dir().join(format!(
            "cvd-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = base.join("home");
        let cv_home = base.join("cvhome");
        fs::create_dir_all(home.join(".claude/projects/-work-proj")).unwrap();
        fs::create_dir_all(&cv_home).unwrap();
        let w = World { base, home, cv_home };
        w.write_fixtures();
        w
    }

    fn session_path(&self, name: &str) -> PathBuf {
        self.home
            .join(".claude/projects/-work-proj")
            .join(format!("{name}.jsonl"))
    }

    fn write_fixtures(&self) {
        let alpha = [
            json!({"type": "ai-title", "aiTitle": "alpha adventures"}),
            json!({"type": "user", "uuid": "u1", "timestamp": "2026-01-01T10:00:00Z",
                   "cwd": "/work/proj",
                   "message": {"role": "user", "content": "please fix the zebrafish migration"}}),
            json!({"type": "assistant", "uuid": "a1", "timestamp": "2026-01-02T10:00:00Z",
                   "message": {"role": "assistant", "content": [
                       {"type": "text", "text": "done"}]}}),
        ];
        let beta = [
            json!({"type": "user", "uuid": "b1", "timestamp": "2026-03-01T09:00:00Z",
                   "cwd": "/work/proj",
                   "message": {"role": "user", "content": "quokka census"}}),
            json!({"type": "assistant", "uuid": "b2", "timestamp": "2026-03-01T09:05:00Z",
                   "message": {"role": "assistant", "content": [
                       {"type": "text", "text": "seventeen"}]}}),
        ];
        for (name, lines) in [("alphasess", &alpha[..]), ("betasess", &beta[..])] {
            let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
            fs::write(self.session_path(name), body).unwrap();
        }
    }

    fn cvd_cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_cvd"));
        c.current_dir(&self.base)
            .env("HOME", &self.home)
            .env("CLAURDVOYANT_HOME", &self.cv_home)
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"));
        c
    }

    /// A third, tool-heavy session (separate from `write_fixtures` so the sync tests' "2
    /// discovered" counts hold): 6 messages with an Edit, a failing Bash, a clean Bash, and a
    /// closing text — the raw material for the messages/events/touched endpoint tests.
    fn write_gamma(&self) {
        let lines = [
            json!({"type": "user", "uuid": "g1", "timestamp": "2026-04-01T08:00:00Z",
                   "cwd": "/work/proj",
                   "message": {"role": "user", "content": "please refactor the pelican module"}}),
            json!({"type": "assistant", "uuid": "g2", "timestamp": "2026-04-01T08:01:00Z",
                   "message": {"role": "assistant", "content": [
                       {"type": "tool_use", "id": "t1", "name": "Edit",
                        "input": {"file_path": "src/pelican.rs", "old_string": "a", "new_string": "b"}}]}}),
            json!({"type": "user", "uuid": "g3", "timestamp": "2026-04-01T08:02:00Z",
                   "message": {"role": "user", "content": [
                       {"type": "tool_result", "tool_use_id": "t1",
                        "content": "error[E0308]: mismatched types", "is_error": true}]}}),
            json!({"type": "assistant", "uuid": "g4", "timestamp": "2026-04-01T08:03:00Z",
                   "message": {"role": "assistant", "content": [
                       {"type": "tool_use", "id": "t2", "name": "Bash",
                        "input": {"command": "cargo test -p pelican"}}]}}),
            json!({"type": "user", "uuid": "g5", "timestamp": "2026-04-01T08:04:00Z",
                   "message": {"role": "user", "content": [
                       {"type": "tool_result", "tool_use_id": "t2",
                        "content": "all tests pass", "is_error": false}]}}),
            json!({"type": "assistant", "uuid": "g6", "timestamp": "2026-04-01T08:05:00Z",
                   "message": {"role": "assistant", "content": [
                       {"type": "text", "text": "done refactoring"}]}}),
        ];
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        fs::write(self.session_path("gammasess"), body).unwrap();
    }

    /// Run a cvd subcommand to completion; returns (stdout, stderr), asserting success.
    fn cvd(&self, args: &[&str]) -> (String, String) {
        let out = self.cvd_cmd().args(args).output().expect("cvd should run");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            out.status.success(),
            "cvd {args:?} failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        (stdout, stderr)
    }
}

impl Drop for World {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.base).ok();
    }
}

fn count_lines(p: &Path) -> usize {
    fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

// ───────────────────────────── sync ─────────────────────────────

#[test]
fn sync_is_idempotent() {
    let w = World::new("sync");

    // First sync archives both sessions.
    let (out, _) = w.cvd(&["sync"]);
    assert!(out.contains("2 archived, 0 skipped (unchanged) of 2 discovered"), "{out}");
    assert!(w.cv_home.join("archive/claude/alphasess.json").is_file());
    assert!(w.cv_home.join("archive/claude/betasess.json").is_file());
    assert_eq!(count_lines(&w.cv_home.join("catalog.jsonl")), 2);

    // Second sync: nothing changed → nothing rewritten, no duplicate catalog rows.
    let (out, _) = w.cvd(&["sync"]);
    assert!(out.contains("0 archived, 2 skipped (unchanged) of 2 discovered"), "{out}");
    assert_eq!(count_lines(&w.cv_home.join("catalog.jsonl")), 2, "no dupes on resync");

    // ls shows each session exactly once.
    let (out, _) = w.cvd(&["ls"]);
    assert!(out.contains("2 session(s)"), "{out}");
    assert_eq!(out.matches("alphases").count(), 1, "{out}");
    assert!(out.contains("alpha adventures"), "{out}");

    // Append a turn to one session: only that one re-archives; the catalog dedupes by key.
    let mut body = fs::read_to_string(w.session_path("alphasess")).unwrap();
    body.push_str(
        &json!({"type": "user", "uuid": "u9", "timestamp": "2026-06-02T10:00:00Z",
                "message": {"role": "user", "content": "one more thing"}})
        .to_string(),
    );
    body.push('\n');
    fs::write(w.session_path("alphasess"), body).unwrap();

    let (out, _) = w.cvd(&["sync"]);
    assert!(out.contains("1 archived, 1 skipped (unchanged) of 2 discovered"), "{out}");
    let (out, _) = w.cvd(&["ls"]);
    assert!(out.contains("2 session(s)"), "changed session must not duplicate:\n{out}");
    assert!(out.contains("3 msgs"), "updated message count visible:\n{out}");

    // The archived JSON is the parsed IR, scrubbed of nothing — spot-check its shape.
    let archived: Value = serde_json::from_str(
        &fs::read_to_string(w.cv_home.join("archive/claude/alphasess.json")).unwrap(),
    )
    .expect("archived session is valid JSON");
    assert_eq!(archived["id"], "alphasess");
    assert_eq!(archived["messages"].as_array().unwrap().len(), 3);
}

#[test]
fn ls_and_path_on_empty_archive() {
    let w = World::new("empty");
    let (out, _) = w.cvd(&["ls"]);
    assert!(out.contains("archive empty"), "{out}");
    let (out, _) = w.cvd(&["path"]);
    assert_eq!(out.trim(), w.cv_home.display().to_string(), "{out}");
}

// ───────────────────────────── serve ─────────────────────────────

/// A minimal HTTP/1.0-style GET over a raw socket (no client dep): returns (status, body).
fn http(port: u16, method: &str, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to cvd serve");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    let text = String::from_utf8_lossy(&raw);
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("bad status line in: {text}"));
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

fn get_json(port: u16, path: &str) -> (u16, Value) {
    let (status, body) = http(port, "GET", path);
    let v = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("GET {path}: non-JSON body {body:?}: {e}"));
    (status, v)
}

/// A child process killed on drop, so a failing test never leaks a server.
struct Reaper(Child);
impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn `cvd serve` on a free port and wait for the listener. Returns the port and the reaper
/// keeping the child alive (drop it to kill the server).
fn spawn_serve(w: &World) -> (u16, Reaper) {
    // A free port: bind 0, note the assignment, release it for cvd (tiny race, fine for tests).
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();

    let child = w
        .cvd_cmd()
        .args(["serve", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("cvd serve should spawn");
    let reaper = Reaper(child);

    // Wait for the listener.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "cvd serve never came up on :{port}");
        std::thread::sleep(Duration::from_millis(50));
    }
    (port, reaper)
}

#[test]
fn serve_endpoints() {
    let w = World::new("serve");
    let (port, _reaper) = spawn_serve(&w);

    // health
    let (status, v) = get_json(port, "/api/health");
    assert_eq!(status, 200);
    assert_eq!(v["ok"], true, "{v}");
    assert!(v["harnesses"].as_array().unwrap().iter().any(|h| h == "claude"), "{v}");

    // sessions: both fixtures, newest first.
    let (status, v) = get_json(port, "/api/sessions");
    assert_eq!(status, 200);
    let arr = v.as_array().expect("array");
    assert_eq!(arr.len(), 2, "{v}");
    assert_eq!(arr[0]["id"], "betasess", "newest-first: {v}");

    // limit
    let (status, v) = get_json(port, "/api/sessions?limit=1");
    assert_eq!(status, 200);
    assert_eq!(v.as_array().unwrap().len(), 1, "{v}");

    // unparseable limit is ignored (no truncation), not a 500.
    let (status, v) = get_json(port, "/api/sessions?limit=banana");
    assert_eq!(status, 200);
    assert_eq!(v.as_array().unwrap().len(), 2, "{v}");

    // harness filter + a typo'd harness is a clear 400.
    let (status, v) = get_json(port, "/api/sessions?harness=claude");
    assert_eq!(status, 200);
    assert_eq!(v.as_array().unwrap().len(), 2, "{v}");
    let (status, v) = get_json(port, "/api/sessions?harness=warpdrive");
    assert_eq!(status, 400);
    assert!(v["error"].as_str().unwrap().contains("unknown harness"), "{v}");

    // cwd filter
    let (status, v) = get_json(port, "/api/sessions?cwd=%2Fwork%2Fproj");
    assert_eq!(status, 200);
    assert_eq!(v.as_array().unwrap().len(), 2, "{v}");
    let (_, v) = get_json(port, "/api/sessions?cwd=nowhere");
    assert_eq!(v.as_array().unwrap().len(), 0, "{v}");

    // one session: found / unknown id / unknown harness.
    let (status, v) = get_json(port, "/api/session/claude/alphasess");
    assert_eq!(status, 200);
    assert_eq!(v["id"], "alphasess", "{v}");
    assert_eq!(v["messages"].as_array().unwrap().len(), 2, "{v}");
    let (status, v) = get_json(port, "/api/session/claude/zzz-not-here");
    assert_eq!(status, 404);
    assert!(v["error"].is_string(), "{v}");
    let (status, _) = get_json(port, "/api/session/warpdrive/alphasess");
    assert_eq!(status, 400);

    // subagents of a session with none: an empty array, not an error.
    let (status, v) = get_json(port, "/api/session/claude/alphasess/subagents");
    assert_eq!(status, 200);
    assert_eq!(v.as_array().unwrap().len(), 0, "{v}");
    let (status, _) = get_json(port, "/api/session/claude/alphasess/subagent/ghost");
    assert_eq!(status, 404);

    // board endpoints on an empty channel: empty arrays.
    let (status, v) = get_json(port, "/api/board/fleet");
    assert_eq!(status, 200);
    assert_eq!(v.as_array().unwrap().len(), 0, "{v}");
    // KNOWN cv-core BUG (handed off — see crates/cv-core/tests/board_fresh_home.rs): with no
    // board/ dir at all, active_claims errors → 500. Pre-create the dir so this exercises cvd's
    // routing rather than that bug; once cv-core is fixed the pre-create becomes a no-op.
    fs::create_dir_all(w.cv_home.join("board")).unwrap();
    let (status, v) = get_json(port, "/api/claims/fleet");
    assert_eq!(status, 200);
    assert!(v.as_array().unwrap().is_empty(), "{v}");
    let (status, v) = get_json(port, "/api/who/fleet?within_secs=60");
    assert_eq!(status, 200);
    assert!(v.as_array().unwrap().is_empty(), "{v}");

    // unknown route → 404; non-GET → 405; OPTIONS preflight → 204 with CORS.
    let (status, v) = get_json(port, "/api/definitely/not/a/route");
    assert_eq!(status, 404);
    assert!(v["error"].is_string(), "{v}");
    let (status, _) = http(port, "POST", "/api/health");
    assert_eq!(status, 405);
    let (status, body) = http(port, "OPTIONS", "/api/health");
    assert_eq!(status, 204);
    assert!(body.is_empty(), "{body}");
    let (_, raw) = (status, {
        // Re-fetch to check the CORS header is present on data responses too.
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(stream, "GET /api/health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
        let mut s = String::new();
        stream.read_to_string(&mut s).unwrap();
        s
    });
    assert!(
        raw.to_ascii_lowercase().contains("access-control-allow-origin: *"),
        "CORS header missing:\n{raw}"
    );
}

/// The Wave-2 endpoints: windowed messages (full-stream fallback that stops at the window's
/// end), per-session events with on-the-spot ingest, and the touched lookup.
#[test]
fn serve_messages_events_touched() {
    let w = World::new("window");
    w.write_gamma();
    let (port, _reaper) = spawn_serve(&w);

    // A middle window [2, 4): exactly 2 messages, indices echoed back, more beyond it, and the
    // total unknown (the stream stopped at the window's end without reaching EOF).
    let (status, v) = get_json(port, "/api/session/claude/gammasess/messages?start=2&end=4");
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["start"], 2, "{v}");
    assert_eq!(v["end"], 4, "{v}");
    let msgs = v["messages"].as_array().expect("messages array");
    assert_eq!(msgs.len(), 2, "{v}");
    assert_eq!(v["has_more"], true, "{v}");
    assert_eq!(v["total_known"], false, "{v}");
    assert!(v["total"].is_null(), "{v}");
    // Message 3 (the second of the window) is the Bash tool_use turn.
    let blocks = msgs[1]["content"].as_array().expect("content blocks");
    assert!(
        blocks.iter().any(|b| b["kind"] == "tool_use" && b["name"] == "Bash"),
        "{v}"
    );

    // A tail window: the stream reaches EOF, so the total is exact and nothing more remains.
    let (status, v) = get_json(port, "/api/session/claude/gammasess/messages?start=4");
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["messages"].as_array().unwrap().len(), 2, "{v}");
    assert_eq!((v["start"].as_u64(), v["end"].as_u64()), (Some(4), Some(6)), "{v}");
    assert_eq!(v["has_more"], false, "{v}");
    assert_eq!(v["total_known"], true, "{v}");
    assert_eq!(v["total"], 6, "{v}");

    // No bounds at all: the whole session, total exact.
    let (status, v) = get_json(port, "/api/session/claude/gammasess/messages");
    assert_eq!(status, 200);
    assert_eq!(v["messages"].as_array().unwrap().len(), 6, "{v}");
    assert_eq!(v["total"], 6, "{v}");

    // A window past EOF is empty, not an error.
    let (status, v) = get_json(port, "/api/session/claude/gammasess/messages?start=50&end=60");
    assert_eq!(status, 200);
    assert_eq!(v["messages"].as_array().unwrap().len(), 0, "{v}");
    assert_eq!(v["has_more"], false, "{v}");

    // Bad windows / unknowns are clear errors.
    let (status, v) = get_json(port, "/api/session/claude/gammasess/messages?start=4&end=2");
    assert_eq!(status, 400);
    assert!(v["error"].as_str().unwrap().contains("end"), "{v}");
    let (status, _) = get_json(port, "/api/session/claude/gammasess/messages?start=banana");
    assert_eq!(status, 400);
    let (status, _) = get_json(port, "/api/session/claude/zzz-not-here/messages");
    assert_eq!(status, 404);
    let (status, _) = get_json(port, "/api/session/warpdrive/gammasess/messages");
    assert_eq!(status, 400);

    // Events: ingested on the spot, classified rows in transcript order.
    let (status, v) = get_json(port, "/api/session/claude/gammasess/events");
    assert_eq!(status, 200);
    let events = v.as_array().expect("events array");
    let kind_of = |k: &str| events.iter().filter(|e| e["kind"] == k).count();
    assert!(kind_of("file_edit") >= 1, "{v}");
    assert!(kind_of("command") >= 1, "{v}");
    assert!(kind_of("error") >= 1, "{v}");
    let edit = events.iter().find(|e| e["kind"] == "file_edit").unwrap();
    assert_eq!(edit["target"], "/work/proj/src/pelican.rs", "{v}");
    assert_eq!(edit["tool"], "Edit", "{v}");
    let errev = events.iter().find(|e| e["kind"] == "error").unwrap();
    assert!(errev["detail"].as_str().unwrap().contains("E0308"), "{v}");

    // Kind filter narrows; unknown session is a 404.
    let (status, v) = get_json(port, "/api/session/claude/gammasess/events?kind=command");
    assert_eq!(status, 200);
    assert!(v.as_array().unwrap().iter().all(|e| e["kind"] == "command"), "{v}");
    assert_eq!(v.as_array().unwrap().len(), 1, "{v}");
    let (status, _) = get_json(port, "/api/session/claude/zzz-not-here/events");
    assert_eq!(status, 404);

    // Touched: suffix path match finds the session; edits_only keeps it (it has an edit);
    // an untouched file is an empty array; a missing path param is a 400.
    let (status, v) = get_json(port, "/api/touched?path=src%2Fpelican.rs");
    assert_eq!(status, 200);
    let rows = v.as_array().expect("touched array");
    assert_eq!(rows.len(), 1, "{v}");
    assert_eq!(rows[0]["session_id"], "gammasess", "{v}");
    assert_eq!(rows[0]["edits"], 1, "{v}");
    let (status, v) = get_json(port, "/api/touched?path=src%2Fpelican.rs&edits_only=true");
    assert_eq!(status, 200);
    assert_eq!(v.as_array().unwrap().len(), 1, "{v}");
    let (status, v) = get_json(port, "/api/touched?path=src%2Fnobody.rs");
    assert_eq!(status, 200);
    assert!(v.as_array().unwrap().is_empty(), "{v}");
    let (status, v) = get_json(port, "/api/touched");
    assert_eq!(status, 400);
    assert!(v["error"].as_str().unwrap().contains("path"), "{v}");
}
