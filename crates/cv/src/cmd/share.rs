//! `cv share` — redact a session and emit one self-contained HTML artifact.

use anyhow::{bail, Result};
use std::path::PathBuf;

pub(crate) fn cmd_share(
    _id: &str,
    _harness: Option<String>,
    _out: Option<PathBuf>,
    _no_redact: bool,
) -> Result<()> {
    bail!("cv share is landing in 0.9.12 — not wired up yet")
}
