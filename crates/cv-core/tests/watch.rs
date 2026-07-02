//! Integration tests for the live watcher ([`cv_core::watch`]): a session appearing is a `New`
//! event, an append is an `Updated` event carrying only the new messages, and vanished sessions
//! are pruned from watcher state. Covers both update paths: the byte tail (claude/codex JSONL —
//! an update must read only the appended bytes, never the transcript head) and the re-parse
//! fallback (grok, whose dir-of-files layout has no tail support).
//!
//! These tests mutate process-global env (`HOME`, `CLUSTERVISION_HOME`), so every test holds a
//! static mutex for its whole body (the `World` guard) — the same discipline as `freshness.rs`.

use cv_core::ir::{Block, Message};
use cv_core::watch::{EventKind, Filter, Watcher};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

static ENV: Mutex<()> = Mutex::new(());

struct World {
    base: PathBuf,
    home: PathBuf,
    _guard: MutexGuard<'static, ()>,
}

impl World {
    fn new(tag: &str) -> World {
        let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!(
            "cv-watch-{tag}-{}-{}",
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
        std::env::set_var("HOME", &home);
        std::env::set_var("CLUSTERVISION_HOME", &cv_home);
        std::env::set_var("XDG_CACHE_HOME", home.join(".cache"));
        std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        std::env::set_var("XDG_DATA_HOME", home.join(".local/share"));
        std::env::remove_var("CLUSTERVISION_MAX_STALE_SECS");
        std::env::remove_var("CURSOR_USER_DIR");
        World {
            base,
            home,
            _guard: guard,
        }
    }

    /// Write a claude-format session of user messages `0..n` under the standard project dir.
    fn write_claude(&self, name: &str, n: usize) -> PathBuf {
        let dir = self.home.join(".claude/projects/-work-proj");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.jsonl"));
        let body: String = (0..n).map(|i| user_line(name, i)).collect();
        fs::write(&path, body).unwrap();
        path
    }

    /// Append user messages `range` to an existing claude session file.
    fn append_claude(&self, path: &PathBuf, sid: &str, range: std::ops::Range<usize>) {
        let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
        for i in range {
            f.write_all(user_line(sid, i).as_bytes()).unwrap();
        }
    }
}

impl Drop for World {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.base).ok();
    }
}

fn user_line(sid: &str, i: usize) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "type": "user", "uuid": format!("{sid}-u{i}"), "sessionId": sid,
            "timestamp": format!("2026-01-0{}T10:0{}:00Z", (i % 9) + 1, i % 10),
            "cwd": "/work/proj",
            "message": {"role": "user", "content": format!("message {i} of {sid}")}
        })
    )
}

/// The inline text of each message (all `Text` blocks concatenated), for compact assertions.
fn texts(msgs: &[Message]) -> Vec<String> {
    msgs.iter()
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    Block::Text { text } => text.inline_str().map(str::to_string),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .collect()
}

/// The `tail -f` lifecycle on the tail path: primed quiet, append → `Updated` with only the new
/// messages, new file → `New` with the whole conversation, deleted session pruned from `seen`
/// (observable because a reborn file with the same id is `New` again, not a stale `Updated`).
#[test]
fn claude_new_append_and_prune() {
    let w = World::new("claude");
    let alpha = w.write_claude("alphasess", 2);
    let mut watcher = Watcher::new(Filter::default(), false);
    assert!(watcher.poll().is_empty(), "primed quiet corpus → no events");

    w.append_claude(&alpha, "alphasess", 2..4);
    let evs = watcher.poll();
    assert_eq!(evs.len(), 1, "one changed session → one event");
    assert_eq!(evs[0].kind, EventKind::Updated);
    assert_eq!(evs[0].reference.id, "alphasess");
    assert_eq!(
        texts(&evs[0].new_messages),
        ["message 2 of alphasess", "message 3 of alphasess"],
        "Updated must carry only the appended messages"
    );
    assert!(watcher.poll().is_empty(), "no further activity → quiet");

    w.write_claude("betasess", 1);
    let evs = watcher.poll();
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].kind, EventKind::New);
    assert_eq!(evs[0].reference.id, "betasess");
    assert_eq!(texts(&evs[0].new_messages), ["message 0 of betasess"]);

    fs::remove_file(&alpha).unwrap();
    assert!(watcher.poll().is_empty(), "a deletion is not an event");
    w.write_claude("alphasess", 1);
    let evs = watcher.poll();
    assert_eq!(evs.len(), 1);
    assert_eq!(
        evs[0].kind,
        EventKind::New,
        "vanished session must have been pruned, so its rebirth is New"
    );
    assert_eq!(texts(&evs[0].new_messages), ["message 0 of alphasess"]);
}

/// `emit_existing = true` skips priming: the first poll reports the current fleet as `New`.
#[test]
fn emit_existing_reports_current_fleet() {
    let w = World::new("existing");
    w.write_claude("alphasess", 2);
    let mut watcher = Watcher::new(Filter::default(), true);
    let evs = watcher.poll();
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].kind, EventKind::New);
    assert_eq!(evs[0].new_messages.len(), 2);
    assert!(watcher.poll().is_empty(), "second poll is quiet");
}

/// The claude update path must be a byte tail, not a re-parse: corrupt the transcript *head* in
/// place (same byte length, invalid JSON) and append one line — the watcher must still deliver
/// exactly the appended message. A head re-parse would see a shorter conversation and go silent;
/// reading the head at all is the cost this test guards against.
#[test]
fn claude_update_reads_only_the_appended_bytes() {
    let w = World::new("tailproof");
    let alpha = w.write_claude("alphasess", 3);
    let mut watcher = Watcher::new(Filter::default(), false);
    assert!(watcher.poll().is_empty());

    let line0_len = fs::read_to_string(&alpha).unwrap().lines().next().unwrap().len();
    {
        use std::os::unix::fs::FileExt;
        let f = fs::OpenOptions::new().write(true).open(&alpha).unwrap();
        f.write_all_at(&vec![b'x'; line0_len], 0).unwrap();
    }
    w.append_claude(&alpha, "alphasess", 3..4);

    let evs = watcher.poll();
    assert_eq!(evs.len(), 1, "{evs:?}");
    assert_eq!(evs[0].kind, EventKind::Updated);
    assert_eq!(texts(&evs[0].new_messages), ["message 3 of alphasess"]);
}

/// Codex JSONL rollouts take the tail path too: an appended `event_msg` arrives as an `Updated`
/// event with exactly that message.
#[test]
fn codex_jsonl_tail() {
    let w = World::new("codex");
    let dir = w.home.join(".codex/sessions");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rollout-2026-07-01T10-00-00-codexsess.jsonl");
    fs::write(
        &path,
        concat!(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"codexsess","cwd":"/work"}}"#, "\n",
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"turn_context","payload":{"cwd":"/work","model":"gpt-test"}}"#, "\n",
            r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"user_message","message":"hello"}}"#, "\n",
            r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"agent_message","message":"working on it"}}"#, "\n",
        ),
    )
    .unwrap();
    let mut watcher = Watcher::new(Filter::default(), false);
    assert!(watcher.poll().is_empty(), "primed quiet corpus → no events");

    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(
        f,
        r#"{{"timestamp":"2026-01-01T00:00:09Z","type":"event_msg","payload":{{"type":"agent_message","message":"done"}}}}"#
    )
    .unwrap();
    drop(f);

    let evs = watcher.poll();
    assert_eq!(evs.len(), 1, "{evs:?}");
    assert_eq!(evs[0].kind, EventKind::Updated);
    assert_eq!(evs[0].reference.id, "codexsess");
    assert_eq!(texts(&evs[0].new_messages), ["done"]);
}

fn write_grok_session(home: &Path, name: &str, lines: &[&str], updated_at: &str) -> PathBuf {
    let dir = home.join(".grok/sessions/-work-proj").join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("summary.json"),
        serde_json::json!({
            "info": {"id": name, "cwd": "/work/proj"},
            "num_chat_messages": lines.len(),
            "created_at": "2026-01-01T10:00:00Z",
            "updated_at": updated_at,
        })
        .to_string(),
    )
    .unwrap();
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(dir.join("chat_history.jsonl"), body).unwrap();
    dir
}

/// A harness without tail support (grok: session = a directory of files) falls back to
/// re-parsing the changed session and diffing by message count — with the baseline seeded from
/// the discover-time count at prime, so construction still parses nothing.
#[test]
fn fallback_harness_reparses_only_the_changed_session() {
    let w = World::new("grok");
    // Grok has no scan-cache/db-file probe coverage for in-place edits inside a session dir, so
    // force full discovery per poll — the subject here is the *diff* fallback, not the probe.
    std::env::set_var("CLUSTERVISION_MAX_STALE_SECS", "0");
    let user = r#"{"type":"user","content":[{"type":"text","text":"hi"}]}"#;
    let asst = r#"{"type":"assistant","content":"hello there","model_id":"grok-test"}"#;
    let dir = write_grok_session(&w.home, "grokked", &[user, asst], "2026-01-01T10:01:00Z");

    let mut watcher = Watcher::new(Filter::default(), false);
    assert!(watcher.poll().is_empty(), "primed quiet corpus → no events");

    let follow = r#"{"type":"user","content":[{"type":"text","text":"and another thing"}]}"#;
    write_grok_session(&w.home, "grokked", &[user, asst, follow], "2026-01-01T10:02:00Z");
    let evs = watcher.poll();
    assert_eq!(evs.len(), 1, "{evs:?}");
    assert_eq!(evs[0].kind, EventKind::Updated);
    assert_eq!(evs[0].reference.id, "grokked");
    assert_eq!(
        texts(&evs[0].new_messages),
        ["and another thing"],
        "fallback diff must emit only the new messages"
    );

    // A new session on the fallback path: New with the whole conversation, then a further append
    // diffs against the now-parsed baseline.
    write_grok_session(&w.home, "grokked2", &[user], "2026-01-01T10:03:00Z");
    let evs = watcher.poll();
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].kind, EventKind::New);
    assert_eq!(evs[0].reference.id, "grokked2");
    assert_eq!(texts(&evs[0].new_messages), ["hi"]);

    write_grok_session(&w.home, "grokked2", &[user, asst], "2026-01-01T10:04:00Z");
    let evs = watcher.poll();
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].kind, EventKind::Updated);
    assert_eq!(texts(&evs[0].new_messages), ["hello there"]);

    drop(dir);
    std::env::remove_var("CLUSTERVISION_MAX_STALE_SECS");
}

/// Not a correctness test: wall-clock of construction (prime) and ten polls against the **real**
/// corpus of whatever HOME this process inherits. Run it alone (never with the isolated-home
/// tests, which repoint HOME):
/// `cargo test -p clustervision-core --test watch -- --ignored --nocapture bench`
#[test]
#[ignore = "wall-clock bench against the real corpus; run alone with --nocapture"]
fn bench_prime_and_poll_real_corpus() {
    let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("CLUSTERVISION_MAX_STALE_SECS");
    let t0 = std::time::Instant::now();
    let mut w = Watcher::new(Filter::default(), false);
    println!("prime: {:?}", t0.elapsed());
    for i in 0..10 {
        let t = std::time::Instant::now();
        let n = w.poll().len();
        println!("poll {i}: {:?} ({n} events)", t.elapsed());
    }
}
