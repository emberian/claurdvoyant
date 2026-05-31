//! Per-harness adapters: discover sessions on disk, parse them into the IR, and (optionally)
//! emit the IR back into a harness's native format for cross-harness porting.

use crate::ir::{Harness, Session, SessionRef};
use crate::stream::{Flow, MessageSink, ParseOptions};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub mod chatgpt_app;
pub mod claude;
pub mod claude_app;
pub mod cline;
pub mod codex;
pub mod continuedev;
#[cfg(feature = "sqlite")]
pub mod cursor;
#[cfg(feature = "sqlite")]
pub mod goose;
pub mod roo;
pub mod gemini;
pub mod grok;
#[cfg(feature = "sqlite")]
pub mod hermes;
pub mod kimi;
pub mod lmstudio;
pub mod opencode;
pub mod openclaw;
pub mod qwen;

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
///
/// `Send + Sync` so [`crate::discover_all`] can fan discovery across adapters in parallel. Every
/// adapter holds only path data (connections are opened per call), so this bound is always satisfied.
pub trait Adapter: Send + Sync {
    fn harness(&self) -> Harness;

    /// The on-disk root this adapter reads from (already resolved against $HOME), if it exists.
    fn storage_root(&self) -> Option<PathBuf>;

    /// Cheaply enumerate sessions without fully parsing them.
    fn discover(&self) -> Result<Vec<SessionRef>>;

    /// Fully parse one discovered session into the IR.
    ///
    /// Adapters with a native [`stream`](Adapter::stream) implement this as
    /// `crate::stream::collect(self, r)`; adapters that don't yet stream implement `parse`
    /// directly and inherit the default [`stream`](Adapter::stream) that bridges through it.
    fn parse(&self, r: &SessionRef) -> Result<Session>;

    /// Stream a session's messages into `sink` one at a time, dropping each before the next, so
    /// peak memory is O(largest message) rather than O(transcript). Returns the session's metadata
    /// (id/cwd/title/model/git/timestamps) with an **empty** `messages` vec — the messages went to
    /// the sink. `opts` controls how much of each message is materialized (see [`ParseOptions`]).
    ///
    /// The default bridges through [`parse`](Adapter::parse): correct for any adapter, but with no
    /// memory savings (it materializes the whole `Session` first). Override with a native streaming
    /// parse to get the savings — see [`claude`](crate::harness::claude) for the reference.
    fn stream(
        &self,
        r: &SessionRef,
        _opts: &ParseOptions,
        sink: &mut dyn MessageSink,
    ) -> Result<Session> {
        let mut session = self.parse(r)?;
        let messages = std::mem::take(&mut session.messages);
        // The full session is already parsed, so its metadata (model/cwd/title) is known up front —
        // hand it to the sink before the body so header-rendering sinks have it.
        sink.meta(&session);
        for m in messages {
            if sink.message(m) == Flow::Stop {
                break;
            }
        }
        Ok(session)
    }

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
        Box::new(claude_app::ClaudeApp::new()),
        Box::new(chatgpt_app::ChatGptApp::new()),
        Box::new(kimi::Kimi::new()),
        Box::new(qwen::Qwen::new()),
        Box::new(lmstudio::LmStudio::new()),
        Box::new(cline::Cline::new()),
        Box::new(roo::Roo::new()),
        Box::new(continuedev::Continue::new()),
    ];
    #[cfg(feature = "sqlite")]
    {
        adapters.push(Box::new(hermes::Hermes::new()));
        adapters.push(Box::new(cursor::Cursor::new()));
        adapters.push(Box::new(goose::Goose::new()));
    }
    adapters
}

/// The adapter for a specific harness, if registered.
pub fn for_harness(h: Harness) -> Option<Box<dyn Adapter>> {
    all().into_iter().find(|a| a.harness() == h)
}
