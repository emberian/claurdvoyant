//! `cv pack` — compile a context bundle for a new task from the session corpus.

use anyhow::{bail, Result};
use std::path::PathBuf;

pub(crate) fn cmd_pack(
    _task: &str,
    _format: &str,
    _to: Option<String>,
    _limit: usize,
    _out: Option<PathBuf>,
) -> Result<()> {
    bail!("cv pack is landing in 0.9.12 — not wired up yet")
}
