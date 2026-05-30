//! # cv-core
//!
//! A unified parser and intermediate representation (IR) for AI coding-agent session transcripts
//! across harnesses (Claude Code, Codex, Grok, OpenCode, Gemini).
//!
//! Every harness has an [`Adapter`](harness::Adapter) that *discovers* sessions on disk and *parses*
//! them into the common [`Session`] IR. Cross-harness porting is then `parse(A) -> IR -> emit(B)`.

pub mod board;
pub mod discover_cache;
pub mod emit;
pub mod harmony;
pub mod harness;
pub mod html;
#[cfg(feature = "sqlite")]
pub mod index;
pub mod ingest;
pub mod ir;
pub mod loom;
pub mod redact;
pub mod render;
pub mod watch;

pub use emit::{EmitOptions, emit};
pub use harness::{Adapter, EmitResult};
pub use ir::*;

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

    // Persist any freshly-scanned metadata so the next discovery reuses it.
    discover_cache::persist();
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
    let mut prefix_hits: Vec<(SessionRef, Box<dyn Adapter>)> = Vec::new();
    for adapter in harness::all() {
        if let Some(h) = harness {
            if adapter.harness() != h {
                continue;
            }
        }
        if adapter.storage_root().is_none() {
            continue;
        }
        for r in adapter.discover()? {
            if r.id == id {
                return Ok(Some((r, adapter)));
            }
            if r.id.starts_with(id) {
                if let Some(fresh) = harness::for_harness(r.harness) {
                    prefix_hits.push((r, fresh));
                }
            }
        }
    }
    match prefix_hits.len() {
        0 => Ok(None),
        1 => Ok(prefix_hits.pop()),
        _ => {
            let mut ids: Vec<String> = prefix_hits
                .iter()
                .map(|(r, _)| format!("{}:{}", r.harness.as_str(), r.id))
                .collect();
            ids.sort();
            anyhow::bail!(
                "ambiguous session id {id:?} matches {} sessions: {} — pass a longer id or --harness",
                ids.len(),
                ids.join(", ")
            )
        }
    }
}
