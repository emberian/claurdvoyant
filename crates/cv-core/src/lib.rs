//! # cv-core
//!
//! A unified parser and intermediate representation (IR) for AI coding-agent session transcripts
//! across harnesses (Claude Code, Codex, Grok, OpenCode, Gemini).
//!
//! Every harness has an [`Adapter`](harness::Adapter) that *discovers* sessions on disk and *parses*
//! them into the common [`Session`] IR. Cross-harness porting is then `parse(A) -> IR -> emit(B)`.

pub mod board;
pub mod catalog;
pub mod dataset;
pub mod discover_cache;
pub mod emit;
pub mod events;
pub mod harmony;
pub mod harness;
pub mod html;
pub mod ingest;
pub mod ir;
pub mod lazy;
pub mod loom;
pub mod redact;
pub mod render;
pub mod stream;
pub mod watch;

pub use emit::{EmitOptions, emit};
pub use harness::{Adapter, EmitResult};
pub use ir::*;
pub use lazy::{Resolver, Span, Text, INLINE_MAX};
pub use stream::{collect, CollectSink, Flow, MessageSink, ParseOptions, TeeSink};

use anyhow::Result;

/// Discover every session from every registered harness on this machine. Adapters are queried in
/// parallel (each is independent and `Send + Sync`), so the cost is the slowest single adapter rather
/// than the sum. Disable the `parallel` feature (e.g. on wasm32) for an identical sequential pass.
pub fn discover_all() -> Vec<SessionRef> {
    let adapters: Vec<Box<dyn Adapter>> = harness::all()
        .into_iter()
        .filter(|a| a.storage_root().is_some())
        .collect();

    fn run(a: &dyn Adapter) -> Vec<SessionRef> {
        match a.discover() {
            Ok(refs) => refs,
            Err(e) => {
                eprintln!("cv: discover failed for {}: {e:#}", a.harness());
                Vec::new()
            }
        }
    }

    #[cfg(feature = "parallel")]
    let out: Vec<SessionRef> = {
        use rayon::prelude::*;
        adapters.par_iter().flat_map_iter(|a| run(a.as_ref())).collect()
    };
    #[cfg(not(feature = "parallel"))]
    let out: Vec<SessionRef> = adapters.iter().flat_map(|a| run(a.as_ref())).collect();

    // Persist any freshly-scanned metadata so the next discovery reuses it, and refresh the
    // id→session catalog so `find` can resolve an id without re-scanning the whole fleet.
    discover_cache::persist();
    catalog::sync(&out);
    out
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
pub fn subagents_of(r: &SessionRef) -> Vec<SessionRef> {
    match r.harness {
        Harness::Claude => harness::claude::subagent_refs(&r.path),
        _ => Vec::new(),
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

    // Slow path (cold/stale catalog): a full discovery — which re-warms the catalog — then match.
    let refs = discover_all();
    let mut prefix_hits: Vec<SessionRef> = Vec::new();
    for r in refs {
        if let Some(h) = harness {
            if r.harness != h {
                continue;
            }
        }
        if r.id == id {
            let a = harness::for_harness(r.harness);
            return Ok(a.map(|a| (r, a)));
        }
        if r.id.starts_with(id) {
            prefix_hits.push(r);
        }
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
    let mut ids: Vec<String> = hits
        .map(|r| format!("{}:{}", r.harness.as_str(), r.id))
        .collect();
    ids.sort();
    anyhow::anyhow!(
        "ambiguous session id {id:?} matches {} sessions: {} — pass a longer id or --harness",
        ids.len(),
        ids.join(", ")
    )
}
