//! One shape, one identity: the shared serializable row shapes every task read surface (CLI,
//! MCP tools, cvd HTTP routes) renders from. Field names and declaration order here ARE the wire
//! contract — the workspace builds serde_json with `preserve_order`, so struct field order is the
//! emitted key order, and all three surfaces must stay byte-identical to what they shipped.
//!
//! Surface asymmetries that predate this module are carried as optional fields rather than
//! re-unified (that would break existing consumers): the HTTP task row carries timestamps the MCP
//! row never did ([`TaskRow::full`] vs [`TaskRow::brief`]), and the HTTP debt row carries a
//! literal `"suspect": false` marker the MCP row never did.

#[cfg(not(target_family = "wasm"))]
use std::path::Path;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::model::RevisionState;
#[cfg(not(target_family = "wasm"))]
use super::project::{AwaitingReviewEntry, DebtEntry};
use super::project::{self, InboxEntry, InboxReason};
use super::reduce::{TaskProjection, TaskReadModel};

/// One task-list row (`cv task list`, MCP `task_list`, `GET /api/tasks`).
#[derive(Clone, Debug, Serialize)]
pub struct TaskRow {
    pub id: String,
    pub title: String,
    pub effective_state: String,
    pub assignee: Option<String>,
    pub repo: Option<PathBuf>,
    pub channel: String,
    /// Timestamps ride the HTTP row only ([`TaskRow::full`]); the MCP row shipped without them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opened_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ts: Option<DateTime<Utc>>,
}

impl TaskRow {
    /// The lean row (MCP `task_list`): no timestamps on the wire.
    pub fn brief(t: &TaskProjection) -> TaskRow {
        TaskRow {
            id: t.task_id.clone(),
            title: t.title.clone(),
            effective_state: project::effective_display(t),
            assignee: t.assignee.clone(),
            repo: t.repo.clone(),
            channel: t.channel.clone(),
            opened_at: None,
            last_ts: None,
        }
    }

    /// The timestamped row (`GET /api/tasks`, and the CLI's text rendering ages off `last_ts`).
    pub fn full(t: &TaskProjection) -> TaskRow {
        TaskRow {
            opened_at: Some(t.opened_at),
            last_ts: Some(t.last_ts),
            ..TaskRow::brief(t)
        }
    }
}

/// One inbox row (`cv task inbox`, MCP `task_inbox`, `GET /api/tasks/inbox/{who}`).
#[derive(Clone, Debug, Serialize)]
pub struct InboxRow {
    pub id: String,
    pub title: String,
    pub reason: InboxReason,
    pub effective_state: String,
    /// The aging anchor the CLI renders (`⏰`, age column); never on the JSON wire — neither MCP
    /// nor HTTP ever carried it.
    #[serde(skip)]
    pub since: DateTime<Utc>,
}

impl InboxRow {
    /// The whole inbox view as rows, stalest first (the projection's order).
    pub fn compute(model: &TaskReadModel, who: &str) -> Vec<InboxRow> {
        project::inbox(model, who).iter().map(InboxRow::from_entry).collect()
    }

    pub fn from_entry(e: &InboxEntry<'_>) -> InboxRow {
        InboxRow {
            id: e.task.task_id.clone(),
            title: e.task.title.clone(),
            reason: e.reason,
            effective_state: project::effective_display(e.task),
            since: e.since,
        }
    }
}

/// One debt row: reviewed-but-unlanded work (`cv task debt`, MCP `task_debt`,
/// `GET /api/tasks/debt`).
#[derive(Clone, Debug, Serialize)]
pub struct DebtRow {
    pub id: String,
    pub title: String,
    pub repo: Option<PathBuf>,
    pub revision: u32,
    pub branch: String,
    pub upstream: String,
    pub state: RevisionState,
    pub since: DateTime<Utc>,
    pub issues: Vec<String>,
    /// The HTTP row ships a literal `"suspect": false` marker (its suspect rows say `true`);
    /// the MCP row never carried the key. [`DebtReport::compute`] leaves it off; the HTTP
    /// surface flips it on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suspect: Option<bool>,
}

#[cfg(not(target_family = "wasm"))]
impl DebtRow {
    fn from_entry(repo: &Option<PathBuf>, e: &DebtEntry<'_>) -> DebtRow {
        DebtRow {
            id: e.task.task_id.clone(),
            title: e.task.title.clone(),
            repo: repo.clone(),
            revision: e.revision_n,
            branch: e.branch.clone(),
            upstream: e.upstream.clone(),
            state: e.state,
            since: e.since,
            issues: e.issues.clone(),
            suspect: None,
        }
    }
}

/// One aged awaiting-review row riding the debt view.
#[derive(Clone, Debug, Serialize)]
pub struct AwaitingRow {
    pub id: String,
    pub title: String,
    pub repo: Option<PathBuf>,
    pub revision: u32,
    pub branch: String,
    pub reviewer: Option<String>,
    pub since: DateTime<Utc>,
}

#[cfg(not(target_family = "wasm"))]
impl AwaitingRow {
    fn from_entry(e: &AwaitingReviewEntry<'_>) -> AwaitingRow {
        AwaitingRow {
            id: e.task.task_id.clone(),
            title: e.task.title.clone(),
            repo: e.task.repo.clone(),
            revision: e.revision_n,
            branch: e.branch.clone(),
            reviewer: e.reviewer.clone(),
            since: e.since,
        }
    }
}

/// The suspect-land row shape the HTTP debt view ships (`task_id` renamed to `id`, plus the
/// literal `"suspect": true` marker); MCP serializes the raw
/// [`super::verify::SuspectLanded`] records instead — both read from [`DebtReport::suspects`].
#[cfg(not(target_family = "wasm"))]
#[derive(Clone, Debug, Serialize)]
pub struct SuspectRow {
    pub id: String,
    pub title: String,
    pub revision: u32,
    pub upstream: String,
    pub detail: String,
    pub suspect: bool,
}

#[cfg(not(target_family = "wasm"))]
impl SuspectRow {
    pub fn from_suspect(s: &super::verify::SuspectLanded) -> SuspectRow {
        SuspectRow {
            id: s.task_id.clone(),
            title: s.title.clone(),
            revision: s.revision,
            upstream: s.upstream.clone(),
            detail: s.detail.clone(),
            suspect: true,
        }
    }
}

/// The whole debt view, packaged once for all three surfaces: unlanded reviewed rows (grouped by
/// repo upstream, flattened repo-ascending / oldest-first here), aged awaiting-review rows, the
/// heartbeat's suspect lands, and the verifier-liveness fields — the debt view is only as honest
/// as the verifier is alive.
#[cfg(not(target_family = "wasm"))]
#[derive(Clone, Debug)]
pub struct DebtReport {
    pub debt: Vec<DebtRow>,
    pub awaiting_review: Vec<AwaitingRow>,
    pub suspects: Vec<super::verify::SuspectLanded>,
    pub verified_as_of: Option<DateTime<Utc>>,
    pub verify_warning: Option<String>,
}

#[cfg(not(target_family = "wasm"))]
impl DebtReport {
    /// Compute the report over a replayed model; `repo` filters both the debt and awaiting rows
    /// to one repo path. Pass the heartbeat read from the store's tasks dir (or `None`, which is
    /// itself loud: `verify_warning` names the never-verified state).
    pub fn compute(
        model: &TaskReadModel,
        heartbeat: Option<&super::verify::VerifyHeartbeat>,
        repo: Option<&Path>,
    ) -> DebtReport {
        let mut debt = Vec::new();
        for (group_repo, entries) in &project::debt(model) {
            if let Some(want) = repo {
                if group_repo.as_deref() != Some(want) {
                    continue;
                }
            }
            debt.extend(entries.iter().map(|e| DebtRow::from_entry(group_repo, e)));
        }
        let awaiting_review = project::awaiting_review(model)
            .iter()
            .filter(|e| match repo {
                Some(want) => e.task.repo.as_deref() == Some(want),
                None => true,
            })
            .map(AwaitingRow::from_entry)
            .collect();
        DebtReport {
            debt,
            awaiting_review,
            suspects: heartbeat.map(|h| h.suspect_landed.clone()).unwrap_or_default(),
            verified_as_of: heartbeat.map(|h| h.ts),
            verify_warning: super::verify::heartbeat_warning(heartbeat),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::model::{Revision, TaskEvent, TaskEventKind};
    use crate::task::reduce::TaskReducer;

    fn sha(c: char) -> String {
        std::iter::repeat_n(c, 40).collect()
    }

    fn model_with_ready_revision() -> (TaskReadModel, String) {
        let ts: DateTime<Utc> = "2026-07-16T12:00:00Z".parse().unwrap();
        let open = TaskEvent {
            id: "00000000-0000-7000-8000-000000000001".into(),
            task_id: "00000000-0000-7000-8000-000000000001".into(),
            ts,
            by: "h".into(),
            kind: TaskEventKind::Opened {
                title: "t".into(),
                body: String::new(),
                repo: Some(PathBuf::from("/tmp/r")),
                issue: None,
                channel: "tasks".into(),
                assignee: Some("agent:a".into()),
            },
        };
        let id = open.task_id.clone();
        let propose = TaskEvent {
            id: "00000000-0000-7000-8000-000000000002".into(),
            task_id: id.clone(),
            ts,
            by: "agent:a".into(),
            kind: TaskEventKind::RevisionProposed {
                revision: Revision {
                    n: 1,
                    branch: "task/x".into(),
                    worktree: None,
                    upstream: "origin/main".into(),
                    base: sha('0'),
                    review_sha: sha('1'),
                    patch_id: sha('b'),
                    reviewer: Some("agent:r".into()),
                    session_ref: None,
                },
            },
        };
        let pass = TaskEvent {
            id: "00000000-0000-7000-8000-000000000003".into(),
            task_id: id.clone(),
            ts,
            by: "agent:r".into(),
            kind: TaskEventKind::ReviewPassed {
                reviewer: "agent:r".into(),
                session_ref: None,
                independence: None,
                receipts: None,
            },
        };
        (TaskReducer::reduce(&[open, propose, pass]).unwrap(), id)
    }

    /// The wire contract: key names AND order are frozen per surface (preserve_order means
    /// declaration order is emitted order).
    #[test]
    fn row_serialization_is_the_shipped_wire_shape() {
        let (model, id) = model_with_ready_revision();
        let t = &model.tasks[&id];

        let brief = serde_json::to_value(TaskRow::brief(t)).unwrap();
        assert_eq!(
            serde_json::to_string(&brief).unwrap(),
            format!(
                r#"{{"id":"{id}","title":"t","effective_state":"rev:ready","assignee":"agent:a","repo":"/tmp/r","channel":"tasks"}}"#
            ),
        );
        let full = serde_json::to_value(TaskRow::full(t)).unwrap();
        let keys: Vec<&str> = full.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["id", "title", "effective_state", "assignee", "repo", "channel", "opened_at", "last_ts"],
        );

        let report = DebtReport::compute(&model, None, None);
        assert_eq!(report.debt.len(), 1);
        assert!(report.verify_warning.as_deref().unwrap_or("").contains("NEVER"));
        let row = serde_json::to_value(&report.debt[0]).unwrap();
        let keys: Vec<&str> = row.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["id", "title", "repo", "revision", "branch", "upstream", "state", "since", "issues"],
            "no `suspect` key unless a surface flips it on",
        );

        // Repo filter: a different repo empties both row sets.
        let filtered = DebtReport::compute(&model, None, Some(Path::new("/elsewhere")));
        assert!(filtered.debt.is_empty() && filtered.awaiting_review.is_empty());
    }

    #[test]
    fn inbox_rows_carry_reason_and_hidden_since() {
        let (model, id) = model_with_ready_revision();
        let rows = InboxRow::compute(&model, "agent:a");
        assert_eq!(rows.len(), 1, "the assignee owes the reviewed-but-unlanded revision");
        assert_eq!(rows[0].id, id);
        let v = serde_json::to_value(&rows[0]).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["id", "title", "reason", "effective_state"], "since never on the wire");
        assert_eq!(v["reason"], "your_unlanded_work");
        assert_eq!(v["effective_state"], "rev:ready");
    }
}
