//! `cv events` / `cv touched` — the extracted-event catalog (`cv blame` lives in `blame.rs`).

use crate::util::{parse_harness, short_id};
use anyhow::{Context, Result};
use cv_core::ir::truncate;

// ---------- events / touched ----------

pub(crate) fn cmd_events(id: &str, harness: Option<String>, kind: Option<String>, subagents: bool) -> Result<()> {
    use cv_core::events;
    let want = parse_harness(&harness)?;
    let (r, _adapter) = cv_core::find(id, want)?.with_context(|| format!("no session matching {id:?}"))?;

    // Ensure this one session's events are current (cheap: a single streamed pass); a session
    // already cataloged at this mtime is a no-op.
    if events::needs_ingest(&r, events::file_mtime_ns(&r.path)) {
        events::ingest_ref(&r)?;
    }

    let rows = events::events_for(r.harness.as_str(), &r.id, kind.as_deref());
    if rows.is_empty() && !subagents {
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
        print_event_row(
            e.msg_idx,
            e.ts,
            &e.kind,
            e.tool.as_deref(),
            e.target.as_deref(),
            e.detail.as_deref(),
        );
    }

    // `--subagents`: descend into the forest and attribute each agent's events to it. Sub-agents
    // aren't in the catalog (there can be thousands), so they're streamed on the spot through an
    // `EventSink` — large tool-result content stays on disk (lazy), only the small rows accumulate.
    if subagents {
        print_subagent_events(&r, kind.as_deref())?;
    }
    Ok(())
}

/// Stream every sub-agent of `r` through an [`EventSink`] and print its events grouped under the
/// agent (with the agent's type/task as a header). Lazy parse → result bodies never materialize.
fn print_subagent_events(r: &cv_core::SessionRef, kind: Option<&str>) -> Result<()> {
    use cv_core::events::EventSink;
    use cv_core::ParseOptions;
    let subs = cv_core::subagent_tree_of(r);
    if subs.is_empty() {
        return Ok(());
    }
    let adapter = cv_core::harness::for_harness(r.harness).with_context(|| format!("no adapter for {}", r.harness))?;

    let mut total = 0usize;
    for s in &subs {
        let mut sink = EventSink::new(s.session.cwd.clone());
        // Best-effort: a single unreadable sub-agent transcript shouldn't abort the whole forest.
        if adapter.stream(&s.session, &ParseOptions::lazy(), &mut sink).is_err() {
            continue;
        }
        let evs: Vec<_> = sink
            .into_events()
            .into_iter()
            .filter(|e| kind.is_none_or(|k| e.kind == k))
            .collect();
        if evs.is_empty() {
            continue;
        }
        total += evs.len();
        let wf = s.workflow.as_deref().map(|w| format!(" ⟐{w}")).unwrap_or_default();
        println!(
            "\n┌─ sub-agent {}  {}{}  ({} event(s)){}",
            short_id(s.agent_id()),
            s.agent_type.as_deref().unwrap_or("agent"),
            wf,
            evs.len(),
            s.description
                .as_deref()
                .map(|d| format!("  — {}", truncate(d, 60)))
                .unwrap_or_default(),
        );
        for e in &evs {
            print!("│ ");
            print_event_row(
                e.msg_idx as i64,
                e.ts,
                e.kind,
                e.tool.as_deref(),
                e.target.as_deref(),
                e.detail.as_deref(),
            );
        }
    }
    if total > 0 {
        println!("\n{total} event(s) across {} sub-agent(s)", subs.len());
    }
    Ok(())
}

/// Print one event row in the shared `cv events` layout (index · time · kind · tool · target, then
/// an optional indented detail line).
fn print_event_row(
    msg_idx: i64,
    ts: Option<i64>,
    kind: &str,
    tool: Option<&str>,
    target: Option<&str>,
    detail: Option<&str>,
) {
    let time = ts
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|d| d.format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-----------".into());
    println!(
        "{:>5}  {:11}  {:9}  {:14}  {}",
        msg_idx,
        time,
        kind,
        tool.unwrap_or("-"),
        target.map(|t| truncate(t, 90)).unwrap_or_default(),
    );
    if let Some(d) = detail {
        println!("       ↳ {}", truncate(d, 100));
    }
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
        // Folded-in sub-agent rows (`cv index --subagents`) carry attribution: surface which agent
        // of which workflow of which parent the touch came from.
        if t.parent_id.is_some() || t.agent_id.is_some() || t.workflow.is_some() {
            let wf = t.workflow.as_deref().map(|w| format!(" ⟐{w}")).unwrap_or_default();
            println!(
                "          ↳ sub-agent {} of {}{}",
                t.agent_id.as_deref().map(short_id).unwrap_or_else(|| "?".into()),
                t.parent_id.as_deref().map(short_id).unwrap_or_else(|| "?".into()),
                wf,
            );
        }
    }
    Ok(())
}
