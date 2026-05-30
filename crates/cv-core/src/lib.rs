//! # cv-core
//!
//! A unified parser and intermediate representation (IR) for AI coding-agent session transcripts
//! across harnesses (Claude Code, Codex, Grok, OpenCode, Gemini).
//!
//! Every harness has an [`Adapter`](harness::Adapter) that *discovers* sessions on disk and *parses*
//! them into the common [`Session`] IR. Cross-harness porting is then `parse(A) -> IR -> emit(B)`.

pub mod board;
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

/// Discover every session from every registered harness on this machine.
pub fn discover_all() -> Vec<SessionRef> {
    let mut out = Vec::new();
    for adapter in harness::all() {
        if adapter.storage_root().is_none() {
            continue;
        }
        match adapter.discover() {
            Ok(mut refs) => out.append(&mut refs),
            Err(e) => eprintln!("cv: discover failed for {}: {e:#}", adapter.harness()),
        }
    }
    out
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
