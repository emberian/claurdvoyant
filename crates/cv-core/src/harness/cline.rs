//! Cline adapter (VS Code extension) — per-task `api_conversation_history.json` (Anthropic-format).
//! (Stub; implementation in progress.)

use super::Adapter;
use crate::ir::*;
use anyhow::Result;
use std::path::PathBuf;

pub struct Cline;
impl Cline {
    pub fn new() -> Self {
        Cline
    }
}
impl Default for Cline {
    fn default() -> Self {
        Self::new()
    }
}
impl Adapter for Cline {
    fn harness(&self) -> Harness {
        Harness::Cline
    }
    fn storage_root(&self) -> Option<PathBuf> {
        None
    }
    fn discover(&self) -> Result<Vec<SessionRef>> {
        Ok(vec![])
    }
    fn parse(&self, _r: &SessionRef) -> Result<Session> {
        anyhow::bail!("cline adapter not implemented yet")
    }
}
