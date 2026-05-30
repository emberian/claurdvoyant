//! Continue adapter — `~/.continue/sessions/` (index `sessions.json` + per-session `<id>.json`).
//! (Stub; implementation in progress. Module is `continuedev` since `continue` is a keyword.)

use super::Adapter;
use crate::ir::*;
use anyhow::Result;
use std::path::PathBuf;

pub struct Continue;
impl Continue {
    pub fn new() -> Self {
        Continue
    }
}
impl Default for Continue {
    fn default() -> Self {
        Self::new()
    }
}
impl Adapter for Continue {
    fn harness(&self) -> Harness {
        Harness::Continue
    }
    fn storage_root(&self) -> Option<PathBuf> {
        None
    }
    fn discover(&self) -> Result<Vec<SessionRef>> {
        Ok(vec![])
    }
    fn parse(&self, _r: &SessionRef) -> Result<Session> {
        anyhow::bail!("continue adapter not implemented yet")
    }
}
