//! Roo Code adapter — a Cline fork with the **same** per-task file format.
//!
//! Roo stores tasks under a different VS Code extension namespace
//! (`rooveterinaryinc.roo-cline` and the newer `rooveterinaryinc.roo-code`) plus a plain `~/.roo/`.
//! The on-disk layout inside each `tasks/<taskId>/` is identical to Cline's
//! (`api_conversation_history.json` array of Anthropic messages + `ui_messages.json` +
//! `task_metadata.json`), so all real logic lives in [`super::cline`] as `pub` helpers and we just
//! point them at Roo's roots and tag the output `Harness::Roo`.

use super::cline::{discover_in, parse_task_dir, task_roots, ROO_EXT_IDS};
use super::Adapter;
use crate::ir::*;
use anyhow::Result;
use std::path::PathBuf;

pub struct Roo {
    roots: Vec<PathBuf>,
}

impl Roo {
    pub fn new() -> Self {
        Roo {
            roots: task_roots(ROO_EXT_IDS, ".roo"),
        }
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
        self.roots.first().cloned()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        Ok(discover_in(&self.roots, Harness::Roo))
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        parse_task_dir(&r.path, &r.id, Harness::Roo)
    }

    fn can_emit(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::super::cline::parse_history_str;
    use super::*;

    fn fixture(name: &str) -> String {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/roo/");
        std::fs::read_to_string(format!("{path}{name}"))
            .unwrap_or_else(|e| panic!("reading fixture {name}: {e}"))
    }

    #[test]
    fn roo_reuses_cline_parser_and_tags_harness() {
        let s = parse_history_str(
            "1717111111111",
            &fixture("api_conversation_history.json"),
            Harness::Roo,
            None,
        );
        assert_eq!(s.harness, Harness::Roo);
        assert_eq!(s.cwd.as_deref(), Some(std::path::Path::new("/home/dev/app")));
        assert_eq!(s.title.as_deref(), Some("Add a unit test"));
        // user, assistant, tool turn.
        assert_eq!(s.messages.len(), 3);
        assert_eq!(s.messages[2].role, Role::Tool);
    }
}
