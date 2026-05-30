//! # cv-core
//!
//! A unified parser and intermediate representation (IR) for AI coding-agent session transcripts
//! across harnesses (Claude Code, Codex, Grok, OpenCode, Gemini).
//!
//! Every harness has an [`Adapter`](harness::Adapter) that *discovers* sessions on disk and *parses*
//! them into the common [`Session`] IR. Cross-harness porting is then `parse(A) -> IR -> emit(B)`.

pub mod board;
pub mod emit;
pub mod harness;
#[cfg(feature = "sqlite")]
pub mod index;
pub mod ingest;
pub mod ir;
pub mod loom;
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
pub fn find(id: &str, harness: Option<Harness>) -> Result<Option<(SessionRef, Box<dyn Adapter>)>> {
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
            if r.id == id || r.id.starts_with(id) {
                return Ok(Some((r, adapter)));
            }
        }
    }
    Ok(None)
}
