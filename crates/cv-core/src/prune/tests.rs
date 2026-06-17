use super::*;

/// A tiny but realistic Claude JSONL: an assistant tool_use, a user tool_result with a big payload
/// (both the content block and the toolUseResult mirror), then a recent turn that must be spared.
fn fixture(dir: &Path, big: &str) -> PathBuf {
    let sid = "11111111-1111-4111-8111-111111111111";
    let lines = [
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u0","timestamp":"2026-06-16T00:00:00Z",
            "message":{"role":"user","content":"do the thing"}}),
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a1","parentUuid":"u0","timestamp":"2026-06-16T00:00:01Z",
            "message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/big.log"}}]}}),
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u1","parentUuid":"a1","timestamp":"2026-06-16T00:00:02Z",
            "toolUseResult":{"stdout":big},
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":big}]}}),
        // a recent assistant turn (within keep-last) — must be left verbatim
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a2","parentUuid":"u1","timestamp":"2026-06-16T00:00:03Z",
            "message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"Read","input":{"file_path":"/recent.log"}}]}}),
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u2","parentUuid":"a2","timestamp":"2026-06-16T00:00:04Z",
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_2","content":big}]}}),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    let body: String = lines.iter().map(|l| l.to_string() + "\n").collect();
    std::fs::write(&path, body).unwrap();
    path
}

fn tmpdir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("cv-prune-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn prunes_old_payloads_into_new_session_keeping_recent() {
    let dir = tmpdir();
    let big = "X".repeat(5000); // > default min_size 2048
    let src = fixture(&dir, &big);

    let opts = PruneOptions { min_size: 2048, keep_last: 2, drop: false, new_id: None, copy_resources: false, dry_run: false };
    let r = prune_session(&src, &opts).unwrap();

    // new session, distinct id + file
    assert_ne!(r.new_id, r.source_id);
    assert!(r.new_path.exists());
    assert!(r.new_size < r.original_size);
    assert!(r.pruned_count >= 1);
    assert!(r.est_context_tokens_saved > 0);

    let out = std::fs::read_to_string(&r.new_path).unwrap();
    // every line stamped with the NEW id, none with the old
    assert!(out.contains(&r.new_id));
    assert!(!out.contains(&r.source_id));
    // the OLD tool_result (toolu_1) is a marker; the big payload is gone from the line
    assert!(out.contains("[PRUNED id=toolu_1"));
    // the RECENT tool_result (toolu_2, within keep_last=2) is spared — its big payload survives
    // somewhere in the output (its tool_result line is left verbatim).
    assert!(out.contains(&big), "recent payload must be kept verbatim");
    let recent_result = out.lines().find(|l| l.contains("tool_result") && l.contains("toolu_2")).unwrap();
    assert!(recent_result.contains(&big), "recent tool_result kept");
    // old tool_result line no longer carries the raw payload
    let old_result = out.lines().find(|l| l.contains("tool_result") && l.contains("toolu_1")).unwrap();
    assert!(!old_result.contains(&big));

    // sidecar holds the original, retrievable byte-faithfully
    let sc = r.sidecar_path.expect("sidecar written");
    assert!(sc.exists());
    let restored = retrieve(&sc, "toolu_1").unwrap();
    assert_eq!(restored.as_str().unwrap(), big);
    // the toolUseResult mirror was also stashed
    assert!(retrieve(&sc, "toolu_1#tur").is_ok());

    // resulting session still parses as valid JSONL
    for line in out.lines() {
        serde_json::from_str::<serde_json::Value>(line).expect("valid json line");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn drop_mode_writes_no_sidecar() {
    let dir = tmpdir();
    let big = "Y".repeat(5000);
    let src = fixture(&dir, &big);
    // keep_last 0 → every turn is "old", so the drop removes ALL big payloads.
    let opts = PruneOptions { min_size: 2048, keep_last: 0, drop: true, new_id: Some("aaaa".into()), copy_resources: false, dry_run: false };
    let r = prune_session(&src, &opts).unwrap();
    assert_eq!(r.new_id, "aaaa");
    assert!(r.sidecar_path.is_none());
    assert!(!dir.join("aaaa.flat.jsonl").exists());
    let out = std::fs::read_to_string(&r.new_path).unwrap();
    assert!(out.contains("[PRUNED id=toolu_1"));
    assert!(!out.contains(&big)); // dropped entirely (nothing retained, no sidecar)
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn dry_run_writes_nothing() {
    let dir = tmpdir();
    let big = "Z".repeat(5000);
    let src = fixture(&dir, &big);
    let opts = PruneOptions { dry_run: true, keep_last: 2, ..Default::default() };
    let r = prune_session(&src, &opts).unwrap();
    assert!(r.pruned_count >= 1);
    assert!(!r.new_path.exists(), "dry run must not write the new session");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn keep_last_large_spares_everything() {
    let dir = tmpdir();
    let big = "Q".repeat(5000);
    let src = fixture(&dir, &big);
    let opts = PruneOptions { keep_last: 100, ..Default::default() };
    let r = prune_session(&src, &opts).unwrap();
    assert_eq!(r.pruned_count, 0, "nothing is old enough to snip");
    std::fs::remove_dir_all(&dir).ok();
}
