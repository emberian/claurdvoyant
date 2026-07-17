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
pub mod project;
pub mod reduce;
pub mod store;
#[cfg(not(target_family = "wasm"))]
pub mod verify;

pub use model::{
    harness_family, IndependenceCheck, MergeFailure, Revision, RevisionState, TaskEvent,
    TaskEventKind, TaskState, VERIFIER_BY,
};
pub use reduce::{
    EffectiveState, Note, PassEvidence, ReduceError, RefuteEvidence, RerouteEvidence,
    RevisionProjection, TaskIssue, TaskProjection, TaskReadModel, TaskReducer,
};
pub use project::{
    age_short, awaiting_review, debt, effective_display, inbox, list, resolve_id,
    AwaitingReviewEntry, DebtEntry, InboxEntry, InboxReason, TaskFilter,
};
pub use store::{new_event, replay, ReplayOutcome, TaskStore};

/// Advisory reviewer-independence check (law 2): read harness families from cv's catalog for the
/// reviewer's session and the current revision's recorded author session. Never blocks anything —
/// the result is recorded on the pass event and surfaced as a warning by the front-ends.
pub fn independence_check(
    t: &reduce::TaskProjection,
    reviewer_session: Option<&str>,
) -> Option<IndependenceCheck> {
    let reviewer_session = reviewer_session?;
    let family_of = |session_id: &str| -> Option<String> {
        let (sref, _) = crate::find_cheap(session_id, None).ok()??;
        harness_family(sref.harness).map(String::from)
    };
    let reviewer_family = family_of(reviewer_session);
    let author_family = t
        .current_revision()
        .and_then(|r| r.revision.session_ref.as_deref())
        .and_then(family_of);
    let independent = match (&author_family, &reviewer_family) {
        (Some(a), Some(r)) => Some(a != r),
        _ => None,
    };
    Some(IndependenceCheck { author_family, reviewer_family, independent })
}

/// Human-readable advisory line for an independence result (shared CLI/MCP wording).
pub fn independence_warning(check: Option<&IndependenceCheck>) -> Option<String> {
    match check {
        None => Some(
            "no reviewer session given: reviewer independence not checked (recorded as unknown)"
                .to_string(),
        ),
        Some(c) => match c.independent {
            Some(true) => None,
            Some(false) => Some(format!(
                "SAME-FAMILY REVIEW: author and reviewer are both {} — this pass is not an independent verdict (recorded, not blocked)",
                c.reviewer_family.as_deref().unwrap_or("?")
            )),
            None => Some(format!(
                "independence undetermined (author {:?}, reviewer {:?}) — recorded, not blocked",
                c.author_family, c.reviewer_family
            )),
        },
    }
}

/// Best-effort board notification for an appended task event (never called while the store lock
/// is held — the append has already returned). Failures are returned for the caller to warn
/// about, never to fail the operation.
pub fn notify_board(event: &TaskEvent, channel: &str) -> anyhow::Result<()> {
    let short = &event.task_id[..event.task_id.len().min(8)];
    let body = format!("task {short}: {}", event.kind.tag());
    crate::board::post(
        channel,
        &event.by,
        &body,
        Some("task"),
        vec![
            format!("task:{}", event.task_id),
            format!("ev:{}", event.kind.tag()),
        ],
        Some(event.task_id.clone()),
    )?;
    Ok(())
}

/// The fleet identity convention (audit G4): the spawner sets `CV_ENDPOINT=agent:<pane>` in each
/// agent's environment, and cv front-ends derive their default `--from` here — the *bare*
/// command records the right actor. This is environment, not assertion: nothing verifies the
/// string (law 3, no authority machinery); it only stops a forgotten flag from silently
/// detaching an agent from its own inbox and misattributing the durable record forever.
pub fn default_endpoint() -> Option<String> {
    let v = std::env::var("CV_ENDPOINT").ok()?;
    let v = v.trim();
    (!v.is_empty()).then(|| v.to_string())
}

use std::path::PathBuf;

/// Root dir for the task log: `$CLUSTERVISION_HOME/tasks` (or `~/.clustervision/tasks`).
pub fn tasks_dir() -> PathBuf {
    std::env::var_os("CLUSTERVISION_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".clustervision")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("tasks")
}

#[cfg(test)]
mod tests {
    use super::default_endpoint;

    /// All env cases live in ONE test: `set_var` is process-global and lib tests run in
    /// parallel threads, so sequencing inside a single test is the serialization.
    #[test]
    fn default_endpoint_reads_cv_endpoint_trimmed_nonempty() {
        std::env::remove_var("CV_ENDPOINT");
        assert_eq!(default_endpoint(), None, "unset → no identity");

        std::env::set_var("CV_ENDPOINT", "agent:demo");
        assert_eq!(default_endpoint().as_deref(), Some("agent:demo"));

        std::env::set_var("CV_ENDPOINT", "  agent:padded \n");
        assert_eq!(default_endpoint().as_deref(), Some("agent:padded"), "trimmed");

        std::env::set_var("CV_ENDPOINT", "   ");
        assert_eq!(default_endpoint(), None, "whitespace-only → no identity");

        std::env::set_var("CV_ENDPOINT", "");
        assert_eq!(default_endpoint(), None, "empty → no identity");

        std::env::remove_var("CV_ENDPOINT");
    }
}
