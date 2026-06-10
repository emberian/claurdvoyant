//! `cv events` / `cv touched` — the extracted-event catalog (`cv blame` lives in `blame.rs`).

use crate::util::{parse_harness, short_id};
use anyhow::{Context, Result};
use cv_core::ir::truncate;

// ---------- events / touched ----------

pub(crate) fn cmd_events(id: &str, harness: Option<String>, kind: Option<String>) -> Result<()> {
    use cv_core::events;
    let want = parse_harness(&harness)?;
    let (r, _adapter) =
        cv_core::find(id, want)?.with_context(|| format!("no session matching {id:?}"))?;

    // Ensure this one session's events are current (cheap: a single streamed pass); a session
    // already cataloged at this mtime is a no-op.
    if events::needs_ingest(&r, events::file_mtime_ns(&r.path)) {
        events::ingest_ref(&r)?;
    }

    let rows = events::events_for(r.harness.as_str(), &r.id, kind.as_deref());
    if rows.is_empty() {
        match &kind {
            Some(k) => println!("no {k:?} events in {} (try without --kind)", short_id(&r.id)),
            None => println!("no tool events in {} (a chat-only session?)", short_id(&r.id)),
        }
        return Ok(());
    }

    println!(
        "{} event(s) in {}:{}\n",
        rows.len(),
        r.harness.as_str(),
        short_id(&r.id)
    );
    for e in &rows {
        let time = e
            .ts
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map(|d| d.format("%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "-----------".into());
        println!(
            "{:>5}  {:11}  {:9}  {:14}  {}",
            e.msg_idx,
            time,
            e.kind,
            e.tool.as_deref().unwrap_or("-"),
            e.target.as_deref().map(|t| truncate(t, 90)).unwrap_or_default(),
        );
        if let Some(d) = &e.detail {
            println!("       ↳ {}", truncate(d, 100));
        }
    }
    Ok(())
}

pub(crate) fn cmd_touched(path: &str, edits_only: bool) -> Result<()> {
    let rows = cv_core::events::sessions_touching(path, edits_only);
    if rows.is_empty() {
        println!(
            "no sessions {} {path:?} — events are ingested by `cv index` (run it first?)",
            if edits_only { "edited" } else { "touched" }
        );
        return Ok(());
    }
    println!("{} session(s) touched {path:?}:\n", rows.len());
    for t in &rows {
        let date = t
            .last_ts
            .and_then(|s| chrono::DateTime::from_timestamp(s, 0))
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "----------".into());
        let counts = match (t.edits, t.reads) {
            (0, n) => format!("{n} read(s)"),
            (n, 0) => format!("{n} edit(s)"),
            (e, r) => format!("{e} edit(s), {r} read(s)"),
        };
        println!(
            "{:8}  {:8}  {:10}  {:22}  {}",
            t.harness,
            short_id(&t.session_id),
            date,
            counts,
            t.title.as_deref().map(|s| truncate(s, 56)).unwrap_or_default(),
        );
    }
    Ok(())
}
