//! Production-robustness integration tests for claurdvoyant.
//!
//! These use ONLY the public `cv_core` API so they survive in-flight additive IR changes:
//!   1. real-corpus smoke (`#[ignore]`d — needs ~2000 local sessions; parse errors OK, panics not),
//!   2. malformed-input fuzz-lite against the public pure parsers (always runs, no data),
//!   3. an emit→re-parse round-trip invariant across every `supported_targets()` (always runs).
//!
//! The bar for the parsers: never panic on hostile input. Returning empty/partial is fine.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use cv_core::ir::{Block, GitInfo, Harness, Message, Role, Session, SessionRef};

// Some emit targets resolve their storage root from env (HOME / HERMES_HOME). Mutating process
// env is not thread-safe, and these tests share the binary with one another, so serialize every
// env-touching section behind one lock (mirrors how emit.rs's own tests guard HOME/HERMES_HOME).
static ENV_LOCK: Mutex<()> = Mutex::new(());

// ------------------------------------------------------------------------------------------------
// helpers
// ------------------------------------------------------------------------------------------------

/// A fresh temp dir under the system temp, using a uuid subdir (no tempfile crate).
fn temp_dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("cv-robust-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A small, representative IR session: system + user + assistant(text+thinking+tool_use) + tool.
fn sample_session(harness: Harness) -> Session {
    let mut sys = Message::new(Role::System);
    sys.content.push(Block::Text {
        text: "you are a helpful assistant".into(),
    });

    let mut user = Message::new(Role::User);
    user.content.push(Block::Text {
        text: "list the files please".into(),
    });

    let mut asst = Message::new(Role::Assistant);
    asst.model = Some("test-model".into());
    asst.content.push(Block::Thinking {
        text: "I should run ls".into(),
        signature: None,
        encrypted: None,
        redacted: false,
    });
    asst.content.push(Block::Text {
        text: "Sure, listing now.".into(),
    });
    asst.content.push(Block::ToolUse {
        id: "call_1".into(),
        name: "run_shell".into(),
        input: serde_json::json!({ "cmd": "ls" }),
    });

    let mut tool = Message::new(Role::Tool);
    tool.content.push(Block::ToolResult {
        tool_use_id: "call_1".into(),
        content: "file_a.txt\nfile_b.txt".into(),
        is_error: false,
        tool_name: Some("run_shell".into()),
        status: None,
        details: None,
    });

    Session {
        id: "orig-id".into(),
        harness,
        cwd: Some(PathBuf::from("/Users/test/project")),
        title: Some("a test session".into()),
        created_at: Some(chrono::Utc::now()),
        updated_at: Some(chrono::Utc::now()),
        model: Some("test-model".into()),
        git: Some(GitInfo {
            branch: Some("main".into()),
            commit: None,
            remote: None,
        }),
        messages: vec![sys, user, asst, tool],
        source_path: None,
        extra: serde_json::Map::new(),
    }
}

fn user_texts(s: &Session) -> Vec<String> {
    s.messages
        .iter()
        .filter(|m| m.role == Role::User)
        .filter_map(|m| m.text())
        .collect()
}

fn assistant_texts(s: &Session) -> Vec<String> {
    s.messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .filter_map(|m| m.text())
        .collect()
}

fn tool_names(s: &Session) -> Vec<String> {
    let mut out = Vec::new();
    for m in &s.messages {
        for b in &m.content {
            if let Block::ToolUse { name, .. } = b {
                out.push(name.clone());
            }
        }
    }
    out
}

/// Adversarial / malformed inputs that any robust parser must tolerate without panicking.
fn hostile_inputs() -> Vec<(&'static str, String)> {
    let mut v: Vec<(&'static str, String)> = vec![
        ("empty", String::new()),
        ("whitespace", "   \n\t  \n".into()),
        ("truncated_json", r#"{"type":"user","message":{"role":"#.into()),
        ("half_a_line", r#"{"uuid":"abc","parentUuid":nul"#.into()),
        ("all_empty_objects", "{}\n{}\n{}\n{}\n".into()),
        ("wrong_shape", r#"{"hello":"world","nested":{"a":[1,2,3]}}"#.into()),
        (
            "null_fields",
            r#"{"type":"user","message":null,"uuid":null,"parentUuid":null,"cwd":null}"#.into(),
        ),
        (
            "missing_fields",
            r#"{"type":"assistant"}
{"type":"user"}
{"type":"tool_result"}"#
                .into(),
        ),
        ("bare_array", "[1, 2, 3, {}, null, \"x\"]".into()),
        ("bare_scalar_num", "12345".into()),
        ("bare_scalar_str", "\"just a string\"".into()),
        ("bare_null", "null".into()),
        ("not_json_at_all", "this is not json at all <<<>>> %%%".into()),
        ("only_newlines", "\n\n\n\n\n\n".into()),
        (
            "mixed_valid_invalid",
            "{}\nnot json\n{\"type\":\"user\"}\n{{{{\n".into(),
        ),
        (
            "control_chars",
            "{\"type\":\"user\",\"x\":\"\u{1}\u{2}\u{7}\u{1b}\"}".into(),
        ),
    ];

    // Deeply nested JSON (objects & arrays) — guard against recursion blowups.
    let mut nested = String::new();
    for _ in 0..2000 {
        nested.push_str("{\"a\":");
    }
    nested.push('1');
    for _ in 0..2000 {
        nested.push('}');
    }
    v.push(("deeply_nested_obj", nested));

    let mut nested_arr = String::new();
    for _ in 0..5000 {
        nested_arr.push('[');
    }
    for _ in 0..5000 {
        nested_arr.push(']');
    }
    v.push(("deeply_nested_arr", nested_arr));

    // Huge single line (1 MB) of valid-ish JSON content.
    let huge_line = format!(r#"{{"type":"user","message":"{}"}}"#, "x".repeat(1_000_000));
    v.push(("huge_single_line", huge_line));

    // 1 MB random-ASCII blob (deterministic LCG so failures reproduce).
    let mut rng: u64 = 0x9e3779b97f4a7c15;
    let mut blob = String::with_capacity(1_048_576);
    for _ in 0..1_048_576 {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        // printable ASCII 0x20..0x7e
        let c = 0x20u8 + ((rng >> 33) as u8 % (0x7eu8 - 0x20u8));
        blob.push(c as char);
    }
    v.push(("random_ascii_1mb", blob));

    // A JSONL where every line is a different broken thing.
    v.push((
        "broken_jsonl",
        "{\n}\n[\n]\n,\n:\n\"\ntrue false\nNaN\nInfinity\n".into(),
    ));

    v
}

// ------------------------------------------------------------------------------------------------
// 1. real-corpus smoke (gated)
// ------------------------------------------------------------------------------------------------

/// Parse EVERY real session discovered on this machine and assert zero panics. Parse *errors* are
/// allowed and counted per harness. Run with:
///   cargo test -p cv-core --test robustness -- --ignored --nocapture
#[test]
#[ignore = "needs the local ~2000-session corpus; run with --ignored"]
fn real_corpus_zero_panics() {
    let refs = cv_core::discover_all();
    let total = refs.len();

    let mut discovered: BTreeMap<String, usize> = BTreeMap::new();
    let mut parsed_ok: BTreeMap<String, usize> = BTreeMap::new();
    let mut parse_err: BTreeMap<String, usize> = BTreeMap::new();
    let mut msg_counts: BTreeMap<String, usize> = BTreeMap::new();

    for r in &refs {
        let h = r.harness.to_string();
        *discovered.entry(h.clone()).or_default() += 1;

        // Build the adapter once per ref via the public for_harness entry point.
        let adapter = match cv_core::harness::for_harness(r.harness) {
            Some(a) => a,
            None => {
                // No adapter registered (e.g. a feature-gated harness). Count as an error, not a panic.
                *parse_err.entry(h).or_default() += 1;
                continue;
            }
        };

        match adapter.parse(r) {
            Ok(s) => {
                *parsed_ok.entry(h.clone()).or_default() += 1;
                *msg_counts.entry(h).or_default() += s.messages.len();
            }
            Err(_) => {
                *parse_err.entry(h).or_default() += 1;
            }
        }
    }

    eprintln!("\n=== real-corpus smoke: {total} sessions discovered ===");
    eprintln!(
        "{:<14} {:>10} {:>10} {:>10} {:>12}",
        "harness", "discovered", "parsed", "errors", "messages"
    );
    let mut all_harnesses: Vec<String> = discovered.keys().cloned().collect();
    all_harnesses.sort();
    let (mut tot_ok, mut tot_err, mut tot_msg) = (0usize, 0usize, 0usize);
    for h in &all_harnesses {
        let d = *discovered.get(h).unwrap_or(&0);
        let ok = *parsed_ok.get(h).unwrap_or(&0);
        let err = *parse_err.get(h).unwrap_or(&0);
        let m = *msg_counts.get(h).unwrap_or(&0);
        tot_ok += ok;
        tot_err += err;
        tot_msg += m;
        eprintln!("{h:<14} {d:>10} {ok:>10} {err:>10} {m:>12}");
    }
    eprintln!(
        "{:<14} {:>10} {:>10} {:>10} {:>12}",
        "TOTAL", total, tot_ok, tot_err, tot_msg
    );

    // The invariant: we got here without any parse() panicking. Errors are tolerated.
    // (If a panic occurred, the test process would have aborted before this line.)
    assert_eq!(
        tot_ok + tot_err,
        total,
        "every discovered session was accounted for (parsed or errored, never panicked)"
    );
}

// ------------------------------------------------------------------------------------------------
// 2. malformed-input fuzz-lite (always runs)
// ------------------------------------------------------------------------------------------------

#[test]
fn claude_parse_str_never_panics() {
    for (label, input) in hostile_inputs() {
        // catch_unwind so a panic surfaces as a clean assertion failure naming the input.
        let res = std::panic::catch_unwind(|| {
            cv_core::harness::claude::parse_str("hostile-id", &input, None)
        });
        assert!(res.is_ok(), "claude::parse_str panicked on input `{label}`");
    }
}

#[test]
fn codex_parse_str_never_panics() {
    for (label, input) in hostile_inputs() {
        for is_jsonl in [true, false] {
            let inp = input.clone();
            let res = std::panic::catch_unwind(move || {
                cv_core::harness::codex::parse_str("hostile-id", &inp, is_jsonl, None)
            });
            assert!(
                res.is_ok(),
                "codex::parse_str panicked on input `{label}` (is_jsonl={is_jsonl})"
            );
        }
    }
}

#[test]
fn gemini_parse_all_str_never_panics() {
    for (label, input) in hostile_inputs() {
        let res = std::panic::catch_unwind(|| {
            cv_core::harness::gemini::parse_all_str(&input, None)
        });
        assert!(
            res.is_ok(),
            "gemini::parse_all_str panicked on input `{label}`"
        );
    }
}

#[test]
fn ingest_files_never_panics() {
    // Mix of named files (so the sniffer routes them) plus pure-garbage names/bytes.
    for (label, input) in hostile_inputs() {
        let files: Vec<(String, Vec<u8>)> = vec![
            (format!("{label}.jsonl"), input.clone().into_bytes()),
            (format!("{label}.json"), input.clone().into_bytes()),
            ("rollout-2025-01-01T00-00-00-abc.jsonl".into(), input.clone().into_bytes()),
            ("session.json".into(), input.clone().into_bytes()),
            (String::new(), input.clone().into_bytes()),
        ];
        let res = std::panic::catch_unwind(move || cv_core::ingest::ingest_files(files));
        assert!(res.is_ok(), "ingest_files panicked on input `{label}`");
    }

    // Non-UTF8 / arbitrary bytes for ingest_files specifically (it decodes lossily).
    let byte_blobs: Vec<(&str, Vec<u8>)> = vec![
        ("all_ff", vec![0xFFu8; 4096]),
        ("all_zero", vec![0x00u8; 4096]),
        ("invalid_utf8", vec![0xC3, 0x28, 0xA0, 0xA1, 0xE2, 0x28, 0xA1]),
        ("lone_surrogate_bytes", vec![0xED, 0xA0, 0x80]),
        ("random_bytes", (0u32..8192).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect()),
        ("bom_then_garbage", {
            let mut b = vec![0xEF, 0xBB, 0xBF];
            b.extend_from_slice(&[0x80, 0x81, 0x82, 0x00, 0xFF]);
            b
        }),
    ];
    for (label, bytes) in byte_blobs {
        let files = vec![
            (format!("{label}.jsonl"), bytes.clone()),
            (format!("{label}.json"), bytes.clone()),
            ("rollout-x.jsonl".into(), bytes.clone()),
        ];
        let res = std::panic::catch_unwind(move || cv_core::ingest::ingest_files(files));
        assert!(res.is_ok(), "ingest_files panicked on byte blob `{label}`");
    }
}

// ------------------------------------------------------------------------------------------------
// 3. round-trip invariant (always runs)
// ------------------------------------------------------------------------------------------------

/// Re-parse an emitted session, given the EmitResult path + target harness. Returns the parsed IR.
/// Handles the per-target quirks: env-rooted harnesses (OpenCode, Hermes) are discovered from a
/// redirected HOME/HERMES_HOME; the rest parse their EmitResult path directly.
fn emit_and_reparse(target: Harness) -> Option<Session> {
    let session = sample_session(target);
    let out = temp_dir();

    match target {
        // OpenCode resolves message/part dirs from $HOME; redirect HOME at a temp home, emit into
        // its storage, then discover+parse the emitted session.
        Harness::OpenCode => {
            let _guard = ENV_LOCK.lock().unwrap();
            let home = temp_dir();
            let storage = home.join(".local/share/opencode/storage");
            std::fs::create_dir_all(&storage).unwrap();
            let res = cv_core::emit::emit(&session, target, &storage, &Default::default())
                .expect("emit opencode");

            let prev = std::env::var_os("HOME");
            // SAFETY: guarded by ENV_LOCK; restored before releasing it.
            unsafe { std::env::set_var("HOME", &home) }
            let adapter = cv_core::harness::for_harness(target).unwrap();
            let parsed = adapter
                .discover()
                .ok()
                .and_then(|refs| refs.into_iter().find(|r| r.id == res.new_id))
                .map(|r| adapter.parse(&r).expect("parse opencode"));
            unsafe {
                match prev {
                    Some(h) => std::env::set_var("HOME", h),
                    None => std::env::remove_var("HOME"),
                }
            }
            parsed
        }
        // Hermes resolves its state.db from HERMES_HOME; emit into a dir, then discover from it.
        Harness::Hermes => {
            let _guard = ENV_LOCK.lock().unwrap();
            let res = cv_core::emit::emit(&session, target, &out, &Default::default())
                .expect("emit hermes");
            let prev = std::env::var_os("HERMES_HOME");
            // SAFETY: guarded by ENV_LOCK; restored before releasing it.
            unsafe { std::env::set_var("HERMES_HOME", &out) }
            let adapter = cv_core::harness::for_harness(target).unwrap();
            let parsed = adapter
                .discover()
                .ok()
                .and_then(|refs| refs.into_iter().find(|r| r.id == res.new_id))
                .map(|r| adapter.parse(&r).expect("parse hermes"));
            unsafe {
                match prev {
                    Some(p) => std::env::set_var("HERMES_HOME", p),
                    None => std::env::remove_var("HERMES_HOME"),
                }
            }
            parsed
        }
        // Everything else: the EmitResult path is directly parseable by the adapter.
        _ => {
            let res = cv_core::emit::emit(&session, target, &out, &Default::default())
                .unwrap_or_else(|e| panic!("emit {target} failed: {e:#}"));
            assert!(res.path.exists(), "emit {target} produced a missing path");
            let adapter = cv_core::harness::for_harness(target).unwrap();
            // For file-stem-keyed harnesses (claude/codex) the id is derived from the file; for the
            // rest we already know the new id. Either way res.new_id is the authoritative one.
            let r = SessionRef {
                id: res.new_id.clone(),
                harness: target,
                path: res.path.clone(),
                cwd: None,
                title: None,
                created_at: None,
                updated_at: None,
                message_count: 0,
            };
            Some(adapter.parse(&r).unwrap_or_else(|e| panic!("parse {target} failed: {e:#}")))
        }
    }
}

#[test]
fn round_trip_all_supported_targets() {
    let targets = cv_core::emit::supported_targets();
    assert!(!targets.is_empty(), "there should be at least one emit target");

    let mut checked = Vec::new();
    for &target in targets {
        let parsed = emit_and_reparse(target)
            .unwrap_or_else(|| panic!("{target}: emitted session was not discoverable"));

        // Two faithful-format quirks among the newer targets:
        //  - LM Studio has no tool structures on disk → tool turns flatten to text (lossy on tools);
        //  - Cline/Roo embed cwd + a `<task>` title inside the first user message's text (that's how
        //    those harnesses really store it), so the user text comes back wrapped, not bare;
        //  - Continue/LM Studio have no thinking part, so a thinking block flattens into the
        //    assistant's text.
        // For these we assert the core text is present rather than byte-exact.
        let wraps_text = matches!(
            target,
            Harness::LmStudio | Harness::Cline | Harness::Roo | Harness::Continue
        );
        let flattens_tools = matches!(target, Harness::LmStudio);

        if wraps_text {
            assert!(
                user_texts(&parsed).iter().any(|t| t.contains("list the files please")),
                "{target}: user text did not survive round-trip (got {:?})",
                user_texts(&parsed)
            );
            assert!(
                assistant_texts(&parsed).iter().any(|t| t.contains("Sure, listing now.")),
                "{target}: assistant text did not survive round-trip (got {:?})",
                assistant_texts(&parsed)
            );
        } else {
            // Core invariant: user/assistant text survives the round trip exactly.
            assert_eq!(
                user_texts(&parsed),
                vec!["list the files please"],
                "{target}: user text did not survive round-trip"
            );
            assert_eq!(
                assistant_texts(&parsed),
                vec!["Sure, listing now."],
                "{target}: assistant text did not survive round-trip"
            );
        }

        // Tool names survive for every target except the ones that flatten tools to text.
        if !flattens_tools {
            assert_eq!(
                tool_names(&parsed),
                vec!["run_shell"],
                "{target}: tool name did not survive round-trip"
            );
        }

        checked.push(target.to_string());
    }

    eprintln!(
        "round-trip OK for {} target(s): {}",
        checked.len(),
        checked.join(", ")
    );
}
