//! # cv-core
//!
//! A unified parser and intermediate representation (IR) for AI coding-agent session transcripts
//! across harnesses (Claude Code, Codex, Grok, OpenCode, Gemini).
//!
//! Every harness has an [`Adapter`](harness::Adapter) that *discovers* sessions on disk and *parses*
//! them into the common [`Session`] IR. Cross-harness porting is then `parse(A) -> IR -> emit(B)`.

pub mod board;
pub mod catalog;
pub mod compaction;
pub mod config;
pub mod dataset;
pub mod discover_cache;
pub mod doctor;
pub mod emit;
pub mod events;
pub mod harmony;
pub mod harness;
pub mod html;
pub mod ingest;
pub mod ir;
pub mod lazy;
pub mod loom;
pub mod offsets;
pub mod prune;
pub mod query;
pub mod redact;
pub mod render;
pub mod stream;
pub mod tools;
pub mod watch;

pub use emit::{emit, EmitOptions};
pub use harness::{Adapter, EmitResult};
pub use ir::*;
pub use lazy::{Resolver, Span, Text, INLINE_MAX};
pub use query::SessionQuery;
pub use stream::{collect, CollectSink, Flow, MessageSink, ParseOptions, TeeSink};

use anyhow::Result;

/// Discover every session from every registered harness on this machine — the **full scan**, and
/// the slow path. Adapters are queried in parallel (each is independent and `Send + Sync`), so the
/// cost is the slowest single adapter rather than the sum. Disable the `parallel` feature (e.g. on
/// wasm32) for an identical sequential pass.
///
/// Side effects: flushes the per-file scan cache, replaces each successfully-scanned harness's
/// rows + freshness watches in the catalog, and stamps the full-sync time — which is what makes
/// [`sessions`] a trustworthy fast read afterwards. Most read-side callers want [`sessions`]
/// instead; call this when you need a guaranteed-fresh scan (e.g. `cv ls --fresh`, indexers).
pub fn discover_all() -> Vec<SessionRef> {
    let adapters = harness::all();

    // Per-adapter outcome: refs on success, `None` on error (the catalog keeps that harness's
    // previous rows rather than wiping them over a transient failure). Rootless adapters are a
    // successful empty scan — their catalog rows (root deleted since last sync) must clear.
    fn run(a: &dyn Adapter) -> (Harness, Option<std::path::PathBuf>, Option<Vec<SessionRef>>) {
        let h = a.harness();
        let Some(root) = a.storage_root() else {
            return (h, None, Some(Vec::new()));
        };
        match a.discover() {
            Ok(refs) => (h, Some(root), Some(refs)),
            Err(e) => {
                eprintln!("cv: discover failed for {h}: {e:#}");
                (h, Some(root), None)
            }
        }
    }

    #[cfg(feature = "parallel")]
    let results: Vec<_> = {
        use rayon::prelude::*;
        adapters.par_iter().map(|a| run(a.as_ref())).collect()
    };
    #[cfg(not(feature = "parallel"))]
    let results: Vec<_> = adapters.iter().map(|a| run(a.as_ref())).collect();

    // Persist freshly-scanned metadata (pruning vanished files) so the next discovery reuses it.
    discover_cache::persist(true);

    // Refresh the catalog per harness so `sessions`/`find` can answer without re-scanning the
    // fleet, recording the watch set the freshness probe stats.
    let mut out = Vec::new();
    for (h, root, refs) in results {
        let Some(refs) = refs else { continue };
        catalog::replace_harness(h, &refs, &catalog::watches_for(h, root.as_deref(), &refs));
        out.extend(refs);
    }
    catalog::stamp_full_sync();
    out
}

/// Every known session, served from the catalog when it's provably fresh — the **fast read path**
/// (~ms against a warm catalog, vs ~seconds for [`discover_all`]'s stat-the-fleet scan). This is
/// what `cv ls`/`timeline`/`stats` read; other consumers (cvd, mcp, tui, indexers) should adopt it
/// as they shed their need for a guaranteed full scan.
///
/// Freshness model, in order:
/// 1. **Cold or overaged catalog** (never fully synced, or the last full sync is older than
///    `CLUSTERVISION_MAX_STALE_SECS`, default 900): transparently runs [`discover_all`] — identical
///    results, identical cost to today.
/// 2. **Probe** ([`catalog::probe_stale`]): re-stat the recorded watch set (session-bearing dirs +
///    their ancestors + sqlite db files) and the top-50 most-recently-updated session files. Any
///    change scopes a re-discovery to just the affected harnesses, then the catalog is read.
///
/// The result matches what [`discover_all`] would return — same (harness, id, path) set, same
/// fields (timestamps at second precision: the catalog's storage granularity) — except for the
/// probe's documented blind spots (see [`catalog::probe_stale`]): chiefly, an in-place append to a
/// session outside the top-50 shows a stale `updated_at`/`message_count` until the staleness
/// backstop, and brand-new sessions are always seen (new files change a watched dir mtime). Set
/// `CLUSTERVISION_MAX_STALE_SECS=0` (or pass `cv ls --fresh`) to force the full scan.
pub fn sessions() -> Vec<SessionRef> {
    sessions_impl().0
}

/// [`sessions`] plus whether the answer came from a full [`discover_all`] (so `find` knows not to
/// escalate to a second full scan).
fn sessions_impl() -> (Vec<SessionRef>, bool) {
    let max_stale = std::env::var("CLUSTERVISION_MAX_STALE_SECS")
        .or_else(|_| std::env::var("CLAURDVOYANT_MAX_STALE_SECS")) // back-compat with the old name
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(900);
    let fresh_enough = catalog::last_full_sync().is_some_and(|t| (chrono::Utc::now().timestamp() - t) < max_stale);
    if fresh_enough {
        if let Some(stale) = catalog::probe_stale() {
            if !stale.is_empty() {
                for h in stale {
                    refresh_harness(h);
                }
                discover_cache::persist(false);
            }
            if let Some(rows) = catalog::all_sessions() {
                return (rows, false);
            }
        }
    }
    (discover_all(), true)
}

/// Re-discover one harness and replace its catalog rows + watches — the probe's scoped escalation.
/// On a discover error the previous rows are kept (matching [`discover_all`]'s tolerance).
fn refresh_harness(h: Harness) {
    let Some(a) = harness::for_harness(h) else { return };
    let Some(root) = a.storage_root() else {
        // Root gone (harness uninstalled / dir deleted): its sessions vanish from the catalog.
        catalog::replace_harness(h, &[], &[]);
        return;
    };
    match a.discover() {
        Ok(refs) => {
            catalog::replace_harness(h, &refs, &catalog::watches_for(h, Some(&root), &refs));
        }
        Err(e) => eprintln!("cv: discover failed for {h}: {e:#}"),
    }
}

/// Map `f` over `items`, keeping the `Some` results — in parallel when the `parallel` feature is on
/// (used by the file-heavy adapters to scan many transcripts at once), sequentially otherwise. The
/// output order is not guaranteed to match the input.
pub(crate) fn par_filter_map<T, R, F>(items: Vec<T>, f: F) -> Vec<R>
where
    T: Send,
    R: Send,
    F: Fn(T) -> Option<R> + Sync + Send,
{
    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;
        items.into_par_iter().filter_map(f).collect()
    }
    #[cfg(not(feature = "parallel"))]
    {
        items.into_iter().filter_map(f).collect()
    }
}

/// Like [`par_filter_map`] but each input may yield zero, one, or many results (e.g. a Gemini
/// `logs.json` holding several sessions). Output order is not guaranteed.
pub(crate) fn par_flat_map<T, R, F>(items: Vec<T>, f: F) -> Vec<R>
where
    T: Send,
    R: Send,
    F: Fn(T) -> Vec<R> + Sync + Send,
{
    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;
        items.into_par_iter().flat_map_iter(f).collect()
    }
    #[cfg(not(feature = "parallel"))]
    {
        items.into_iter().flat_map(f).collect()
    }
}

/// The sub-agent sessions a given session spawned (lazily; not part of the main pool — there can be
/// thousands). Currently Claude Code's Task sub-agents; other harnesses return empty for now.
///
/// This is the *flat* view (top-level `subagents/*.jsonl` only). For the full forest — including
/// `Workflow` sub-agents and the meta/journal sidecars that carry each agent's purpose and outcome —
/// use [`subagent_tree_of`].
pub fn subagents_of(r: &SessionRef) -> Vec<SessionRef> {
    match r.harness {
        Harness::Claude => harness::claude::subagent_refs(&r.path),
        _ => Vec::new(),
    }
}

pub use harness::claude::SubagentInfo;

/// The full sub-agent **forest** a session spawned: directly-spawned (`Agent`/`Task`) sub-agents
/// *and* `Workflow` sub-agents (which live a tier deeper), each enriched with its `*.meta.json`
/// sidecar (agent type / task description / spawning tool_use) and, for workflow agents, the
/// orchestrator's journaled result (`status` + `summary`). Newest first. Non-Claude harnesses
/// return empty for now.
pub fn subagent_tree_of(r: &SessionRef) -> Vec<SubagentInfo> {
    match r.harness {
        Harness::Claude => harness::claude::subagent_tree(&r.path),
        _ => Vec::new(),
    }
}

pub use harness::claude_workflow::{Workflow, WorkflowAgent, WorkflowLaunch, WorkflowPhase};

/// Every `Workflow`-tool run a session launched, as first-class [`Workflow`] objects (phase tree →
/// agents → outcomes + the driving script), newest first. Read from the session's
/// `workflows/wf_*.json` state files. Non-Claude harnesses return empty.
pub fn workflows_of(r: &SessionRef) -> Vec<Workflow> {
    match r.harness {
        Harness::Claude => harness::claude_workflow::workflows(&r.path),
        _ => Vec::new(),
    }
}

/// Find workflow runs by **name** (exact, else prefix) across the whole catalog — so a workflow
/// can be addressed without knowing which session launched it (session titles are auto-generated
/// and rarely mention the workflow's name). Returns `(session, run)` pairs, newest-session first.
/// Cheap: the sidecar `workflows/` dir is only read for sessions that have one.
pub fn find_workflows_by_name(name: &str) -> Vec<(SessionRef, Workflow)> {
    let mut refs = sessions();
    refs.retain(|r| r.harness == Harness::Claude);
    refs.sort_by_key(|r| std::cmp::Reverse(r.updated_at.or(r.created_at)));
    let want = name.to_string();
    let per_session: Vec<Vec<(SessionRef, Workflow)>> = par_filter_map(refs, move |r| {
        let hits: Vec<(SessionRef, Workflow)> = harness::claude_workflow::workflows(&r.path)
            .into_iter()
            .filter(|w| w.name.as_deref().is_some_and(|n| n.starts_with(want.as_str())))
            .map(|w| (r.clone(), w))
            .collect();
        (!hits.is_empty()).then_some(hits)
    });
    let all: Vec<(SessionRef, Workflow)> = per_session.into_iter().flatten().collect();
    let (exact, prefix): (Vec<_>, Vec<_>) = all.into_iter().partition(|(_, w)| w.name.as_deref() == Some(name));
    if exact.is_empty() {
        prefix
    } else {
        exact
    }
}

/// Fleet-wide ghost hunt by name-prefix: [`workflow_ghosts_of`] across every Claude session that
/// recorded at least one workflow (the bounded set where the transcript-vs-state cross-check is
/// meaningful), in parallel. For finding a swarm you remember by name that left no run record.
pub fn find_ghost_launches_by_name(name: &str) -> Vec<(SessionRef, WorkflowLaunch)> {
    let mut refs = sessions();
    refs.retain(|r| r.harness == Harness::Claude);
    let name = name.to_string();
    let mut out: Vec<(SessionRef, WorkflowLaunch)> = par_filter_map(refs, move |r| {
        if harness::claude_workflow::workflows(&r.path).is_empty() {
            return None;
        }
        let hits: Vec<(SessionRef, WorkflowLaunch)> = harness::claude_workflow::ghost_launches(&r.path)
            .into_iter()
            .filter(|g| g.name.as_deref().is_some_and(|n| n.starts_with(name.as_str())))
            .map(|g| (r.clone(), g))
            .collect();
        (!hits.is_empty()).then_some(hits)
    })
    .into_iter()
    .flatten()
    .collect();
    out.sort_by_key(|(_, g)| std::cmp::Reverse(g.ts));
    out
}

/// Transcript `Workflow` launches with **no recorded run** — the crash-forensics view: a power
/// loss / hard kill can leave a launch in the transcript whose `workflows/wf_*.json` state file
/// was never persisted (its sub-agent debris, if any, sits under `subagents/workflows/`).
/// Non-Claude harnesses return empty.
pub fn workflow_ghosts_of(r: &SessionRef) -> Vec<WorkflowLaunch> {
    match r.harness {
        Harness::Claude => harness::claude_workflow::ghost_launches(&r.path),
        _ => Vec::new(),
    }
}

/// One workflow run of a session by its `runId` (with or without the `wf_` prefix, or a unique
/// prefix). `None` if the session has no such run.
pub fn workflow_of(r: &SessionRef, run_id: &str) -> Option<Workflow> {
    match r.harness {
        Harness::Claude => harness::claude_workflow::workflow(&r.path, run_id),
        _ => None,
    }
}

/// Find a single session by id (optionally constrained to one harness), returning its ref + adapter.
///
/// An exact id match always wins and returns immediately. Otherwise the id is treated as a prefix:
/// a single prefix hit is returned, but *multiple* distinct prefix hits are an error rather than a
/// silent "first one wins" — callers should disambiguate (e.g. by passing a longer id or a harness).
pub fn find(id: &str, harness: Option<Harness>) -> Result<Option<(SessionRef, Box<dyn Adapter>)>> {
    // Fast path: the persisted catalog resolves the id without touching the fleet. We trust a row
    // only if its file still exists (a stale row — session deleted/moved — falls through to scan).
    let cataloged = catalog::lookup(id, harness);
    if !cataloged.is_empty() {
        if let Some(r) = cataloged.iter().find(|r| r.id == id && r.path.exists()) {
            if let Some(a) = harness::for_harness(r.harness) {
                return Ok(Some((r.clone(), a)));
            }
        }
        let live: Vec<&SessionRef> = cataloged.iter().filter(|r| r.path.exists()).collect();
        match live.len() {
            1 => {
                if let Some(a) = harness::for_harness(live[0].harness) {
                    return Ok(Some((live[0].clone(), a)));
                }
            }
            n if n > 1 => return Err(ambiguous(id, live.into_iter())),
            _ => {} // all stale → fall through to a fresh scan
        }
    }

    // Slow path (cold/stale catalog): freshen via the probe first — usually a few hundred stats
    // plus at most a scoped re-discovery, and it re-warms the catalog. Only when the id *still*
    // isn't there (a session in one of the probe's documented blind spots) escalate to the full
    // scan — unless the probe path already was one (cold catalog), in which case a miss is a miss.
    let (refs, was_full) = sessions_impl();
    match resolve_id(refs, id, harness)? {
        Some(hit) => Ok(Some(hit)),
        None if was_full => Ok(None),
        None => resolve_id(discover_all(), id, harness),
    }
}

/// Match `id` (exact first, then as a prefix) against `refs` — `find`'s matching contract.
/// Multiple distinct prefix hits are an error rather than a silent "first one wins".
fn resolve_id(
    refs: Vec<SessionRef>,
    id: &str,
    harness: Option<Harness>,
) -> Result<Option<(SessionRef, Box<dyn Adapter>)>> {
    let mut prefix_hits: Vec<SessionRef> = Vec::new();
    for r in refs {
        if let Some(h) = harness {
            if r.harness != h {
                continue;
            }
        }
        if !r.id.starts_with(id) {
            continue;
        }
        // Mirror the fast path: never return a session whose file has vanished (deleted between
        // the scan and now, or a stale probe-path catalog row). Stat only the id matches — cheap.
        if !r.path.exists() {
            continue;
        }
        if r.id == id {
            let a = harness::for_harness(r.harness);
            return Ok(a.map(|a| (r, a)));
        }
        prefix_hits.push(r);
    }
    match prefix_hits.len() {
        0 => Ok(None),
        1 => {
            let r = prefix_hits.pop().unwrap();
            Ok(harness::for_harness(r.harness).map(|a| (r, a)))
        }
        _ => Err(ambiguous(id, prefix_hits.iter())),
    }
}

/// The "ambiguous prefix" error shared by `find`'s fast and slow paths.
fn ambiguous<'a>(id: &str, hits: impl Iterator<Item = &'a SessionRef>) -> anyhow::Error {
    let mut ids: Vec<String> = hits.map(|r| format!("{}:{}", r.harness.as_str(), r.id)).collect();
    ids.sort();
    anyhow::anyhow!(
        "ambiguous session id {id:?} matches {} sessions: {} — pass a longer id or --harness",
        ids.len(),
        ids.join(", ")
    )
}
