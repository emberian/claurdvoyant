//! Roo Code adapter (a Cline fork) — same per-task layout, different globalStorage namespace.
//! (Stub; implementation in progress.)

use super::Adapter;
use crate::ir::*;
use anyhow::Result;
use std::path::PathBuf;

pub struct Roo;
impl Roo {
    pub fn new() -> Self {
        Roo
    }
}
impl Default for Roo {
    fn default() -> Self {
        Self::new()
    }
}
impl Adapter for Roo {
    fn harness(&self) -> Harness {
        Harness::Roo
    }
    fn storage_root(&self) -> Option<PathBuf> {
        None
    }
    fn discover(&self) -> Result<Vec<SessionRef>> {
        Ok(vec![])
    }
    fn parse(&self, _r: &SessionRef) -> Result<Session> {
        anyhow::bail!("roo adapter not implemented yet")
    }
}
