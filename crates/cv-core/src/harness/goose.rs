//! Goose adapter (Block) — `~/.local/share/goose/sessions/sessions.db` (SQLite; legacy `.jsonl`).
//! (Stub; implementation in progress. sqlite feature only.)

use super::Adapter;
use crate::ir::*;
use anyhow::Result;
use std::path::PathBuf;

pub struct Goose;
impl Goose {
    pub fn new() -> Self {
        Goose
    }
}
impl Default for Goose {
    fn default() -> Self {
        Self::new()
    }
}
impl Adapter for Goose {
    fn harness(&self) -> Harness {
        Harness::Goose
    }
    fn storage_root(&self) -> Option<PathBuf> {
        None
    }
    fn discover(&self) -> Result<Vec<SessionRef>> {
        Ok(vec![])
    }
    fn parse(&self, _r: &SessionRef) -> Result<Session> {
        anyhow::bail!("goose adapter not implemented yet")
    }
}
