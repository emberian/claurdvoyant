//! Wire types for the task substrate: the durable event grammar and derived state enums.
//!
//! The JSON shapes here are a **wire contract**: events are appended to
//! `$CLUSTERVISION_HOME/tasks/events.jsonl` and replayed forever, so tags and field names must
//! stay stable. Additions are fine; renames are not. The round-trip and pinned-JSON tests at the
//! bottom of this file are the tripwire.
//!
//! Lineage: the revision (land-facet) grammar is ported from mission-control's
//! `ramspace-core/src/land_request.rs` pure reducer — the one module of that system worth keeping.
//! Deliberate deviations from it are documented in [`crate::task::reduce`].

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ir::Harness;

/// The `by` identity the cv-side git verifier stamps on verifier-only events. Not authority —
/// anyone can type it — but replay warns when a verifier-only event carries any *other* author,
/// and the periodic pass re-observes Landed revisions, so a forged land is loud and self-defeating.
pub const VERIFIER_BY: &str = "cv-verify";

/// A single durable task event. One per line of `tasks/events.jsonl`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskEvent {
    /// Unique, time-sortable event id (uuid v7). Idempotency key: re-appending a byte-identical
    /// event is a no-op; a *different* event reusing an id is a loud error.
    pub id: String,
    /// The task this event belongs to. For `Opened`, `task_id == id`.
    pub task_id: String,
    /// When the event was appended.
    pub ts: DateTime<Utc>,
    /// Who appended it (board `from` convention: agent/session/human identifier).
    pub by: String,
    /// What happened.
    #[serde(flatten)]
    pub kind: TaskEventKind,
}

/// The event grammar. Base lifecycle events apply to every task; the land facet
/// (`RevisionProposed` onward) applies only to code tasks that propose reviewed revisions.
///
/// Verifier-only kinds (`SourceUnavailable`, `MergeFailed`, `MergedLocal`, `ReconcileFailed`,
/// `Landed`) may only be emitted by the cv-side git verifier — law 1: **observed, not attested**.
/// The store's agent-facing append path rejects them; see [`TaskEventKind::is_verifier_only`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TaskEventKind {
    // ── base lifecycle ──────────────────────────────────────────────────────
    Opened {
        title: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        issue: Option<String>,
        /// Board channel task notifications post to.
        channel: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        assignee: Option<String>,
    },
    Claimed {
        assignee: String,
    },
    Released {},
    /// Progress note; never changes state.
    Noted {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_ref: Option<String>,
    },
    /// Non-code completion, with optional pointer to observable evidence.
    Done {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed: Option<String>,
    },
    Abandoned {
        reason: String,
    },
    Superseded {
        by_task: String,
    },

    // ── land facet (revision-scoped) ────────────────────────────────────────
    /// Attach a reviewed code revision. Proposing again supersedes the prior revision — that is
    /// the only cure for a refute (a REFUTE on a revision is terminal for that revision).
    RevisionProposed {
        revision: Revision,
    },
    ReviewRerouted {
        from: String,
        to: String,
    },
    ReviewPassed {
        reviewer: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_ref: Option<String>,
        /// Advisory reviewer-independence observation (recorded, never a gate).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        independence: Option<IndependenceCheck>,
    },
    ReviewRefuted {
        reviewer: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_ref: Option<String>,
    },

    // ── verifier-only (law 1: observed, not attested) ───────────────────────
    SourceUnavailable {
        detail: String,
    },
    MergeFailed {
        reason: MergeFailure,
    },
    MergedLocal {
        from_sha: String,
        to_sha: String,
    },
    ReconcileFailed {
        detail: String,
    },
    Landed {
        upstream_head: String,
        observed_patch_id: String,
    },
}

impl TaskEventKind {
    /// Kinds only the git verifier may emit. Agent-facing append paths (CLI verbs other than
    /// `verify`, every MCP tool) must reject these at the store seam.
    pub fn is_verifier_only(&self) -> bool {
        matches!(
            self,
            TaskEventKind::SourceUnavailable { .. }
                | TaskEventKind::MergeFailed { .. }
                | TaskEventKind::MergedLocal { .. }
                | TaskEventKind::ReconcileFailed { .. }
                | TaskEventKind::Landed { .. }
        )
    }

    /// The wire tag for this kind (the serde `event` field), for logs and board notifications.
    pub fn tag(&self) -> &'static str {
        match self {
            TaskEventKind::Opened { .. } => "opened",
            TaskEventKind::Claimed { .. } => "claimed",
            TaskEventKind::Released {} => "released",
            TaskEventKind::Noted { .. } => "noted",
            TaskEventKind::Done { .. } => "done",
            TaskEventKind::Abandoned { .. } => "abandoned",
            TaskEventKind::Superseded { .. } => "superseded",
            TaskEventKind::RevisionProposed { .. } => "revision_proposed",
            TaskEventKind::ReviewRerouted { .. } => "review_rerouted",
            TaskEventKind::ReviewPassed { .. } => "review_passed",
            TaskEventKind::ReviewRefuted { .. } => "review_refuted",
            TaskEventKind::SourceUnavailable { .. } => "source_unavailable",
            TaskEventKind::MergeFailed { .. } => "merge_failed",
            TaskEventKind::MergedLocal { .. } => "merged_local",
            TaskEventKind::ReconcileFailed { .. } => "reconcile_failed",
            TaskEventKind::Landed { .. } => "landed",
        }
    }
}

/// A reviewed code revision. Identity-bearing fields are `review_sha` and `patch_id` (the
/// **range** patch-id: cumulative diff from merge-base to the review sha); `branch`/`worktree`
/// are locators, never identity. Both sha and patch-id are computed by cv at propose time by
/// running git — never typed in by an agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Revision {
    /// 1-based revision number within the task.
    pub n: u32,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<PathBuf>,
    /// The ref the revision must reach to count as landed, e.g. `origin/main`.
    pub upstream: String,
    /// The merge-base of `upstream` and `review_sha` **at propose time**. Recorded so the range
    /// patch-id stays recomputable between two fixed commits forever — after a direct-FF land,
    /// a live `merge-base(upstream, sha)` would equal `sha` and the range would vanish.
    pub base: String,
    /// Full 40-hex review commit sha.
    pub review_sha: String,
    /// 40-hex range patch-id (`git diff <base> <review_sha> | git patch-id --stable`).
    pub patch_id: String,
    /// Active reviewer endpoint. Reroute is the only reassignment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<String>,
    /// The author's session (cv session id) that produced this revision, when known — the
    /// author-side input to the advisory independence check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_ref: Option<String>,
}

/// Why a verification pass could not observe the revision as merged/landed. These are
/// **verification findings** (observations about the world), not merge-attempt outcomes —
/// cv never runs a merge on anyone's behalf.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MergeFailure {
    NonFastForward {},
    MissingWorktree {},
    MissingBranch {},
    BranchTipChanged {},
    GitFailed { detail: String },
}

impl MergeFailure {
    pub fn describe(&self) -> String {
        match self {
            MergeFailure::NonFastForward {} => "upstream moved; not fast-forwardable".to_string(),
            MergeFailure::MissingWorktree {} => "worktree missing".to_string(),
            MergeFailure::MissingBranch {} => "branch missing".to_string(),
            MergeFailure::BranchTipChanged {} => "branch tip no longer the reviewed sha".to_string(),
            MergeFailure::GitFailed { detail } => format!("git failed: {detail}"),
        }
    }
}

/// Advisory reviewer-independence observation, read from transcripts at pass time (law 2).
/// `None` fields mean "could not determine" — recorded and warned about, never a gate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndependenceCheck {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub independent: Option<bool>,
}

/// Base lifecycle state of a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Open,
    Claimed,
    Done,
    Abandoned,
    Superseded,
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        matches!(self, TaskState::Done | TaskState::Abandoned | TaskState::Superseded)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Open => "open",
            TaskState::Claimed => "claimed",
            TaskState::Done => "done",
            TaskState::Abandoned => "abandoned",
            TaskState::Superseded => "superseded",
        }
    }
}

/// Land-facet state of a revision. Ported from mission-control's `LandRequestState`:
/// `MergedLocal` is actionable, **not** terminal — a local merge is not a land until the
/// verifier observes the reviewed patch on the upstream ref.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionState {
    AwaitingReview,
    Ready,
    Refuted,
    Superseded,
    MergedLocal,
    Landed,
}

impl RevisionState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RevisionState::Refuted | RevisionState::Superseded | RevisionState::Landed
        )
    }

    pub fn is_actionable(self) -> bool {
        matches!(self, RevisionState::Ready | RevisionState::MergedLocal)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RevisionState::AwaitingReview => "awaiting_review",
            RevisionState::Ready => "ready",
            RevisionState::Refuted => "refuted",
            RevisionState::Superseded => "superseded",
            RevisionState::MergedLocal => "merged_local",
            RevisionState::Landed => "landed",
        }
    }
}

/// Model family behind a harness, for the advisory independence check. Multi-model harnesses
/// (IDE extensions, local runners) return `None` — undeterminable at harness granularity.
/// Model-granular resolution (per-message `model` field) is a planned refinement.
pub fn harness_family(harness: Harness) -> Option<&'static str> {
    match harness {
        Harness::Claude | Harness::ClaudeApp | Harness::ClaudeExport => Some("anthropic"),
        Harness::Codex | Harness::ChatGptApp | Harness::ChatGptExport => Some("openai"),
        Harness::Gemini => Some("google"),
        Harness::Grok => Some("xai"),
        Harness::Qwen => Some("alibaba"),
        Harness::Kimi => Some("moonshot"),
        Harness::Hermes => Some("nous"),
        // Model-agnostic multiplexers: could be running anything.
        Harness::OpenCode
        | Harness::OpenClaw
        | Harness::Cursor
        | Harness::LmStudio
        | Harness::Cline
        | Harness::Roo
        | Harness::Continue
        | Harness::Goose
        | Harness::Zed => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: TaskEventKind) -> TaskEvent {
        TaskEvent {
            id: "0198c0de-0000-7000-8000-000000000001".to_string(),
            task_id: "0198c0de-0000-7000-8000-000000000000".to_string(),
            ts: "2026-07-16T12:00:00Z".parse().unwrap(),
            by: "agent:test".to_string(),
            kind,
        }
    }

    fn revision() -> Revision {
        Revision {
            n: 1,
            branch: "task/demo".to_string(),
            worktree: None,
            upstream: "origin/main".to_string(),
            base: "0".repeat(40),
            review_sha: "a".repeat(40),
            patch_id: "b".repeat(40),
            reviewer: Some("agent:reviewer".to_string()),
            session_ref: None,
        }
    }

    #[test]
    fn every_kind_round_trips() {
        let kinds = vec![
            TaskEventKind::Opened {
                title: "t".into(),
                body: "b".into(),
                repo: Some(PathBuf::from("/tmp/x")),
                issue: Some("#1".into()),
                channel: "tasks".into(),
                assignee: None,
            },
            TaskEventKind::Claimed { assignee: "a".into() },
            TaskEventKind::Released {},
            TaskEventKind::Noted { text: "n".into(), session_ref: Some("s".into()) },
            TaskEventKind::Done { observed: None },
            TaskEventKind::Abandoned { reason: "r".into() },
            TaskEventKind::Superseded { by_task: "t2".into() },
            TaskEventKind::RevisionProposed { revision: revision() },
            TaskEventKind::ReviewRerouted { from: "a".into(), to: "b".into() },
            TaskEventKind::ReviewPassed {
                reviewer: "b".into(),
                session_ref: None,
                independence: Some(IndependenceCheck {
                    author_family: Some("anthropic".into()),
                    reviewer_family: Some("openai".into()),
                    independent: Some(true),
                }),
            },
            TaskEventKind::ReviewRefuted { reviewer: "b".into(), session_ref: None },
            TaskEventKind::SourceUnavailable { detail: "gone".into() },
            TaskEventKind::MergeFailed { reason: MergeFailure::GitFailed { detail: "boom".into() } },
            TaskEventKind::MergeFailed { reason: MergeFailure::NonFastForward {} },
            TaskEventKind::MergedLocal { from_sha: "c".repeat(40), to_sha: "a".repeat(40) },
            TaskEventKind::ReconcileFailed { detail: "d".into() },
            TaskEventKind::Landed { upstream_head: "d".repeat(40), observed_patch_id: "b".repeat(40) },
        ];
        for kind in kinds {
            let ev = event(kind);
            let json = serde_json::to_string(&ev).unwrap();
            let back: TaskEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(ev, back, "round-trip mismatch for {json}");
        }
    }

    /// The wire contract: tags and core field names, pinned. If this test fails you are
    /// breaking replay of existing logs — add fields instead of renaming.
    #[test]
    fn wire_tags_are_pinned() {
        let ev = event(TaskEventKind::Claimed { assignee: "agent:a".into() });
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(v["event"], "claimed");
        assert_eq!(v["task_id"], "0198c0de-0000-7000-8000-000000000000");
        assert_eq!(v["by"], "agent:test");
        assert_eq!(v["assignee"], "agent:a");

        let ev = event(TaskEventKind::MergeFailed { reason: MergeFailure::BranchTipChanged {} });
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(v["event"], "merge_failed");
        assert_eq!(v["reason"]["kind"], "branch_tip_changed");

        let ev = event(TaskEventKind::RevisionProposed { revision: revision() });
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(v["event"], "revision_proposed");
        assert_eq!(v["revision"]["review_sha"], "a".repeat(40));
        assert_eq!(v["revision"]["upstream"], "origin/main");

        // Tag helper stays in sync with serde.
        for (kind, tag) in [
            (TaskEventKind::Released {}, "released"),
            (TaskEventKind::Landed { upstream_head: "d".repeat(40), observed_patch_id: "b".repeat(40) }, "landed"),
        ] {
            let v: serde_json::Value =
                serde_json::from_str(&serde_json::to_string(&event(kind.clone())).unwrap()).unwrap();
            assert_eq!(v["event"], tag);
            assert_eq!(kind.tag(), tag);
        }
    }

    #[test]
    fn verifier_only_partition_is_exact() {
        let verifier_only = [
            TaskEventKind::SourceUnavailable { detail: String::new() },
            TaskEventKind::MergeFailed { reason: MergeFailure::MissingBranch {} },
            TaskEventKind::MergedLocal { from_sha: String::new(), to_sha: String::new() },
            TaskEventKind::ReconcileFailed { detail: String::new() },
            TaskEventKind::Landed { upstream_head: String::new(), observed_patch_id: String::new() },
        ];
        let agent_facing = [
            TaskEventKind::Opened {
                title: String::new(),
                body: String::new(),
                repo: None,
                issue: None,
                channel: String::new(),
                assignee: None,
            },
            TaskEventKind::Claimed { assignee: String::new() },
            TaskEventKind::Released {},
            TaskEventKind::Noted { text: String::new(), session_ref: None },
            TaskEventKind::Done { observed: None },
            TaskEventKind::Abandoned { reason: String::new() },
            TaskEventKind::Superseded { by_task: String::new() },
            TaskEventKind::RevisionProposed { revision: revision() },
            TaskEventKind::ReviewRerouted { from: String::new(), to: String::new() },
            TaskEventKind::ReviewPassed { reviewer: String::new(), session_ref: None, independence: None },
            TaskEventKind::ReviewRefuted { reviewer: String::new(), session_ref: None },
        ];
        for k in &verifier_only {
            assert!(k.is_verifier_only(), "{} should be verifier-only", k.tag());
        }
        for k in &agent_facing {
            assert!(!k.is_verifier_only(), "{} should be agent-facing", k.tag());
        }
    }

    #[test]
    fn harness_family_maps_majors_and_declines_multiplexers() {
        assert_eq!(harness_family(Harness::Claude), Some("anthropic"));
        assert_eq!(harness_family(Harness::Codex), Some("openai"));
        assert_eq!(harness_family(Harness::Gemini), Some("google"));
        assert_eq!(harness_family(Harness::Cursor), None);
        assert_eq!(harness_family(Harness::Goose), None);
    }
}
