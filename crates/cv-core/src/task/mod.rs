//! 📋 The task substrate — durable, replayable dispatch objects for agent fleets.
//!
//! Where the [`crate::board`] lets agents *talk*, tasks let a fleet *commit to work* and let a
//! human see what is actually true about it. A task is a durable object with a replay-derived
//! lifecycle; code tasks additionally carry reviewed **revisions** whose landing is *observed*
//! by running git, never taken on an agent's word.
//!
//! # The four laws
//!
//! 1. **Observed, not attested.** No agent claim ever produces `MergedLocal` or `Landed`; only
//!    the cv-side git verifier emits those events, and agent-facing append paths reject them at
//!    the store seam.
//! 2. **Independence is read, not asserted.** Reviewer independence (cross-family review) is
//!    determined by reading the reviewer's transcript harness from cv's catalog — advisory-warn,
//!    never a gate.
//! 3. **No authority machinery.** No seats, grants, or debts. Landing authority is whoever can
//!    push to the repo; cv only tracks and verifies.
//! 4. **Small.** This substrate stays a few thousand lines. Complexity requests go to the
//!    design-notes graveyard, next to the 200k-line system this replaced.
//!
//! Storage is a single append-only event log (`$CLUSTERVISION_HOME/tasks/events.jsonl`) with the
//! same crash-safe flock recipe as the board.

pub mod model;
pub mod reduce;

pub use model::{
    harness_family, IndependenceCheck, MergeFailure, Revision, RevisionState, TaskEvent,
    TaskEventKind, TaskState,
};
pub use reduce::{
    EffectiveState, Note, PassEvidence, ReduceError, RefuteEvidence, RerouteEvidence,
    RevisionProjection, TaskIssue, TaskProjection, TaskReadModel, TaskReducer,
};

use std::path::PathBuf;

/// Root dir for the task log: `$CLUSTERVISION_HOME/tasks` (or `~/.clustervision/tasks`).
pub fn tasks_dir() -> PathBuf {
    std::env::var_os("CLUSTERVISION_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".clustervision")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("tasks")
}
