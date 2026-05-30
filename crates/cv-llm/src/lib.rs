//! cv-llm — LLM-backed features over claurdvoyant sessions (distillation), via OpenRouter / Anthropic.
//! (Skeleton; the distill engine lands here.)

use anyhow::Result;

/// Knobs for distillation.
#[derive(Debug, Clone, Default)]
pub struct DistillOptions {
    /// Model id (OpenRouter provider-prefixed, e.g. `anthropic/claude-…`, or an Anthropic model).
    pub model: Option<String>,
    /// Distill a whole project (all sessions for a cwd) rather than a single session.
    pub project: bool,
}

/// Distill a session into a compact, durable digest of what was learned — the kind of thing you'd
/// keep in a `MEMORY.md`/`CLAUDE.md`: decisions, gotchas, where things live, open threads.
pub fn distill(_session: &cv_core::Session, _opts: &DistillOptions) -> Result<String> {
    anyhow::bail!("distill not implemented yet")
}
