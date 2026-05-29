//! Per-harness adapters: discover sessions on disk, parse them into the IR, and (optionally)
//! emit the IR back into a harness's native format for cross-harness porting.

use crate::ir::{Harness, Session, SessionRef};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub mod claude;
pub mod codex;
pub mod gemini;
pub mod grok;
#[cfg(feature = "sqlite")]
pub mod hermes;
pub mod opencode;
pub mod openclaw;

/// Result of emitting an IR session into a harness's native on-disk format.
#[derive(Debug, Clone)]
pub struct EmitResult {
    /// Primary file (or directory) written.
    pub path: PathBuf,
    /// The new native session id in the target harness.
    pub new_id: String,
    /// A human hint for how to resume it, if known (e.g. `claude --resume <id>`).
    pub resume_hint: Option<String>,
}

/// A harness adapter. Implementors live in submodules and are registered in [`all`].
pub trait Adapter {
    fn harness(&self) -> Harness;

    /// The on-disk root this adapter reads from (already resolved against $HOME), if it exists.
    fn storage_root(&self) -> Option<PathBuf>;

    /// Cheaply enumerate sessions without fully parsing them.
    fn discover(&self) -> Result<Vec<SessionRef>>;

    /// Fully parse one discovered session into the IR.
    fn parse(&self, r: &SessionRef) -> Result<Session>;

    /// Emit an IR session into this harness's native format under `out_dir`.
    /// Default: unsupported. Adapters override to enable being a *conversion target*.
    fn emit(&self, _session: &Session, _out_dir: &Path) -> Result<EmitResult> {
        anyhow::bail!("emitting to {} is not implemented yet", self.harness())
    }

    /// Whether [`emit`] is implemented (so the CLI can list valid `--to` targets).
    fn can_emit(&self) -> bool {
        false
    }
}

/// All registered adapters.
pub fn all() -> Vec<Box<dyn Adapter>> {
    let mut adapters: Vec<Box<dyn Adapter>> = vec![
        Box::new(claude::Claude::new()),
        Box::new(codex::Codex::new()),
        Box::new(grok::Grok::new()),
        Box::new(opencode::OpenCode::new()),
        Box::new(gemini::Gemini::new()),
        Box::new(openclaw::OpenClaw::new()),
    ];
    #[cfg(feature = "sqlite")]
    adapters.push(Box::new(hermes::Hermes::new()));
    adapters
}

/// The adapter for a specific harness, if registered.
pub fn for_harness(h: Harness) -> Option<Box<dyn Adapter>> {
    all().into_iter().find(|a| a.harness() == h)
}
