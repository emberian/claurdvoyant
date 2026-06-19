//! End-to-end seekable-session roundtrip (REARCH Phase 2): record per-message byte offsets into
//! an isolated catalog, then `offsets::stream_range` must deliver byte-identical windows to a
//! full stream — and fall back (`Ok(false)`, sink untouched) on anything stale or unsupported.
//!
//! Lives in its own integration-test process because `CLUSTERVISION_HOME` is process-global state
//! (the in-crate unit tests that touch it serialize among themselves; we don't share their lock).

#![cfg(all(feature = "sqlite", feature = "mmap"))]

use cv_core::offsets::{self, OffsetSink};
use cv_core::{CollectSink, Flow, Message, MessageSink, ParseOptions, Session, SessionRef};
use std::io::Write;
use std::path::PathBuf;

/// Collects messages *and* the meta calls, so tests can assert on the replayed metadata too.
#[derive(Default)]
struct Probe {
    metas: Vec<(Option<String>, Option<String>)>, // (title, model)
    messages: Vec<Message>,
}

impl MessageSink for Probe {
    fn meta(&mut self, s: &Session) {
        self.metas.push((s.title.clone(), s.model.clone()));
    }
    fn message(&mut self, m: Message) -> Flow {
        self.messages.push(m);
        Flow::Continue
    }
}

fn temp_home(tag: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("cv-offsets-seek-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    home
}

fn claude_ref(path: PathBuf) -> SessionRef {
    SessionRef {
        id: "seek-claude".into(),
        harness: cv_core::Harness::Claude,
        path,
        cwd: Some("/w".into()),
        title: Some("seek test".into()),
        created_at: None,
        updated_at: None,
        message_count: 0,
    }
}

fn write_claude(path: &PathBuf, extra_lines: usize) {
    let big = "long line of \"content\"\n".repeat(400);
    let mut f = std::fs::File::create(path).unwrap();
    writeln!(f, r#"{{"type":"ai-title","aiTitle":"seek test"}}"#).unwrap();
    writeln!(
        f,
        r#"{{"type":"user","uuid":"u0","cwd":"/w","message":{{"role":"user","content":"first question"}}}}"#
    )
    .unwrap();
    writeln!(
        f,
        "{}",
        serde_json::json!({"type":"assistant","uuid":"a1",
            "message":{"role":"assistant","model":"claude-test","content":[
                {"type":"text","text":"answer"},
                {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}})
    )
    .unwrap();
    writeln!(
        f,
        "{}",
        serde_json::json!({"type":"user","uuid":"u2","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"t1","content":big,"is_error":false}]}})
    )
    .unwrap();
    for i in 0..(6 + extra_lines) {
        writeln!(
            f,
            "{}",
            serde_json::json!({"type":"user","uuid":format!("u{}", 3 + i),
                "message":{"role":"user","content":format!("follow-up number {i}")}})
        )
        .unwrap();
    }
}

/// Record offsets for `r` exactly as the indexer's ride-along does.
fn record(r: &SessionRef) {
    let adapter = cv_core::harness::for_harness(r.harness).unwrap();
    let (mtime, size) = offsets::file_sig(&r.path);
    let mut sink = OffsetSink::new();
    adapter.stream(r, &ParseOptions::lazy_offsets(), &mut sink).unwrap();
    assert!(sink.seekable(), "fixture must produce a seekable session");
    offsets::record(r, &sink, mtime, size);
}

fn full_stream(r: &SessionRef, opts: &ParseOptions) -> Vec<Message> {
    let adapter = cv_core::harness::for_harness(r.harness).unwrap();
    let mut sink = CollectSink::default();
    adapter.stream(r, opts, &mut sink).unwrap();
    sink.messages
}

#[test]
fn record_then_seek_windows_match_full_stream_and_guard_staleness() {
    let home = temp_home("e2e");
    std::env::set_var("CLUSTERVISION_HOME", &home);

    // ---------- claude ----------
    let cpath = home.join("seek-claude.jsonl");
    write_claude(&cpath, 0);
    let r = claude_ref(cpath.clone());
    record(&r);

    let opts = ParseOptions::lazy();
    let full = full_stream(&r, &opts);
    assert_eq!(full.len(), 9);

    // Several windows, including open-ended, end-past-EOF, and start-past-EOF.
    for (start, end) in [
        (1usize, Some(3usize)),
        (3, Some(7)),
        (5, None),
        (8, Some(50)),
        (40, Some(44)),
    ] {
        let mut probe = Probe::default();
        let hit = offsets::stream_range(&r, start, end, &opts, &mut probe).unwrap();
        assert!(hit, "offsets recorded+fresh ⇒ seek path taken (start={start})");
        let lo = start.min(full.len());
        let hi = end.unwrap_or(full.len()).min(full.len()).max(lo);
        assert_eq!(
            serde_json::to_value(&full[lo..hi]).unwrap(),
            serde_json::to_value(&probe.messages).unwrap(),
            "window {start}..{end:?} must be byte-identical to the full stream's"
        );
        // The replayed metadata: claude emits none, so the synthetic one mirrors the SessionRef
        // and carries the model the skipped prefix provided (first model at idx 1).
        assert_eq!(probe.metas.len(), 1);
        let want_model = (start > 1).then(|| "claude-test".to_string());
        assert_eq!(probe.metas[0], (Some("seek test".into()), want_model));
    }

    // start == 0 is already optimal — explicit fallback, sink untouched.
    let mut probe = Probe::default();
    assert!(!offsets::stream_range(&r, 0, Some(2), &opts, &mut probe).unwrap());
    assert!(probe.messages.is_empty() && probe.metas.is_empty());

    // ---------- staleness: append a line → signature mismatch → fallback, never wrong ----------
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&cpath).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","uuid":"ux","message":{{"role":"user","content":"appended"}}}}"#
        )
        .unwrap();
    }
    let mut probe = Probe::default();
    assert!(
        !offsets::stream_range(&r, 2, Some(4), &opts, &mut probe).unwrap(),
        "a changed file must read as stale"
    );
    assert!(
        probe.messages.is_empty() && probe.metas.is_empty(),
        "fallback leaves the sink untouched"
    );
    // Re-recording the grown file heals the seek path.
    record(&r);
    let full2 = full_stream(&r, &opts);
    assert_eq!(full2.len(), 10);
    let mut probe = Probe::default();
    assert!(offsets::stream_range(&r, 9, None, &opts, &mut probe).unwrap());
    assert_eq!(
        serde_json::to_value(&full2[9..]).unwrap(),
        serde_json::to_value(&probe.messages).unwrap()
    );

    // ---------- codex ----------
    let xpath = home.join("rollout-seek.jsonl");
    {
        let big = "giant output\n".repeat(500);
        let mut f = std::fs::File::create(&xpath).unwrap();
        for l in [
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"seek-codex","cwd":"/work"}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"turn_context","payload":{"cwd":"/work","model":"gpt-test"}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"user_message","message":"hello"}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"agent_message","message":"working on it"}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:04Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":7,"output_tokens":3}}}}"#.to_string(),
            r#"{"timestamp":"2026-01-01T00:00:05Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}","call_id":"c1"}}"#.to_string(),
            serde_json::json!({"timestamp":"2026-01-01T00:00:06Z","type":"response_item",
                "payload":{"type":"function_call_output","call_id":"c1","output":big}}).to_string(),
            r#"{"timestamp":"2026-01-01T00:00:07Z","type":"event_msg","payload":{"type":"agent_message","message":"done"}}"#.to_string(),
        ] {
            writeln!(f, "{l}").unwrap();
        }
    }
    let rx = SessionRef {
        id: "seek-codex".into(),
        harness: cv_core::Harness::Codex,
        path: xpath.clone(),
        cwd: Some("/work".into()),
        title: Some("discovery title".into()),
        created_at: None,
        updated_at: None,
        message_count: 0,
    };
    record(&rx);
    let xfull = full_stream(&rx, &opts);
    assert_eq!(xfull.len(), 5); // user, assistant(+usage), tool-use, span result, trailing assistant

    for (start, end) in [(1usize, Some(2usize)), (2, None), (3, Some(5)), (4, Some(9))] {
        let mut probe = Probe::default();
        assert!(offsets::stream_range(&rx, start, end, &opts, &mut probe).unwrap());
        let hi = end.unwrap_or(xfull.len()).min(xfull.len());
        assert_eq!(
            serde_json::to_value(&xfull[start..hi]).unwrap(),
            serde_json::to_value(&probe.messages).unwrap(),
            "codex window {start}..{end:?}"
        );
        // codex's recorded meta snapshot: title=None (the parse never sets one — it must
        // override the discovery title exactly like the live stream does), model from
        // turn_context, delivered before the window (meta_idx 0 ≤ start).
        assert_eq!(probe.metas, vec![(None, Some("gpt-test".into()))]);
    }

    // A model *change* mid-session is a replay hazard: recorded as unseekable → fallback.
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&xpath).unwrap();
        writeln!(
            f,
            r#"{{"timestamp":"2026-01-01T00:00:08Z","type":"turn_context","payload":{{"cwd":"/work","model":"gpt-other"}}}}"#
        )
        .unwrap();
    }
    let adapter = cv_core::harness::for_harness(rx.harness).unwrap();
    let (mtime, size) = offsets::file_sig(&rx.path);
    let mut sink = OffsetSink::new();
    adapter.stream(&rx, &ParseOptions::lazy_offsets(), &mut sink).unwrap();
    assert!(!sink.seekable(), "a model change must mark the session unseekable");
    offsets::record(&rx, &sink, mtime, size);
    let mut probe = Probe::default();
    assert!(!offsets::stream_range(&rx, 2, Some(4), &opts, &mut probe).unwrap());
    assert!(probe.messages.is_empty());

    // ---------- unsupported harness: never seekable ----------
    let other = SessionRef {
        harness: cv_core::Harness::Grok,
        ..r.clone()
    };
    let mut probe = Probe::default();
    assert!(!offsets::stream_range(&other, 2, Some(4), &opts, &mut probe).unwrap());

    std::env::remove_var("CLUSTERVISION_HOME");
    std::fs::remove_dir_all(&home).ok();
}
