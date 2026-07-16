//! Read-side projections over the [`TaskReadModel`]: filtered lists, per-assignee inboxes, and
//! the worktree-debt view. Pure functions over the model — no I/O.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::model::{RevisionState, TaskState};
use super::reduce::{EffectiveState, TaskProjection, TaskReadModel};

/// Filter for [`list`]. Empty filter = all tasks.
#[derive(Clone, Debug, Default)]
pub struct TaskFilter {
    /// Match against the *effective* state string (`open`, `claimed`, `ready`, `landed`, ...).
    pub state: Option<String>,
    pub assignee: Option<String>,
    pub repo: Option<PathBuf>,
    /// When false (default), terminal tasks (done/abandoned/superseded, landed included) are
    /// hidden unless `state` explicitly asks for them.
    pub include_terminal: bool,
}

/// List tasks matching `filter`, oldest first (task ids are time-sortable uuid v7).
pub fn list<'m>(model: &'m TaskReadModel, filter: &TaskFilter) -> Vec<&'m TaskProjection> {
    model
        .tasks
        .values()
        .filter(|t| {
            if let Some(state) = &filter.state {
                if t.effective_state().as_str() != state {
                    return false;
                }
            } else if !filter.include_terminal && t.state.is_terminal() {
                return false;
            }
            if let Some(assignee) = &filter.assignee {
                if t.assignee.as_deref() != Some(assignee.as_str()) {
                    return false;
                }
            }
            if let Some(repo) = &filter.repo {
                if t.repo.as_deref() != Some(repo.as_path()) {
                    return false;
                }
            }
            true
        })
        .collect()
}

/// Why a task appears in someone's inbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxReason {
    /// Open task assigned to you, not yet claimed.
    AssignedOpen,
    /// You claimed it; it is yours to finish.
    ClaimedByYou,
    /// A revision awaits your review verdict.
    AwaitingYourReview,
    /// Your reviewed revision is ready/merged-local but not observed landed — land it.
    YourUnlandedWork,
}

#[derive(Clone, Debug, Serialize)]
pub struct InboxEntry<'m> {
    pub task: &'m TaskProjection,
    pub reason: InboxReason,
}

/// The per-agent "what needs me" view.
pub fn inbox<'m>(model: &'m TaskReadModel, endpoint: &str) -> Vec<InboxEntry<'m>> {
    let mut entries = Vec::new();
    for task in model.tasks.values() {
        if task.state.is_terminal() {
            continue;
        }
        let reason = if let Some(rev) = task.current_revision() {
            match rev.state {
                RevisionState::AwaitingReview
                    if rev.active_reviewer.as_deref() == Some(endpoint) =>
                {
                    Some(InboxReason::AwaitingYourReview)
                }
                RevisionState::Ready | RevisionState::MergedLocal
                    if task.assignee.as_deref() == Some(endpoint)
                        || task.opened_by == endpoint =>
                {
                    Some(InboxReason::YourUnlandedWork)
                }
                _ => base_inbox_reason(task, endpoint),
            }
        } else {
            base_inbox_reason(task, endpoint)
        };
        if let Some(reason) = reason {
            entries.push(InboxEntry { task, reason });
        }
    }
    entries
}

fn base_inbox_reason(task: &TaskProjection, endpoint: &str) -> Option<InboxReason> {
    match task.state {
        TaskState::Open if task.assignee.as_deref() == Some(endpoint) => {
            Some(InboxReason::AssignedOpen)
        }
        TaskState::Claimed if task.assignee.as_deref() == Some(endpoint) => {
            Some(InboxReason::ClaimedByYou)
        }
        _ => None,
    }
}

/// One row of the worktree-debt view: reviewed work not observed on its upstream.
#[derive(Clone, Debug, Serialize)]
pub struct DebtEntry<'m> {
    pub task: &'m TaskProjection,
    pub revision_n: u32,
    pub branch: String,
    pub upstream: String,
    pub state: RevisionState,
    /// When the revision reached its current unlanded-but-reviewed state (pass time), for aging.
    pub since: DateTime<Utc>,
    pub issues: Vec<String>,
}

/// Unlanded reviewed work, grouped by repo (`None` key = tasks with no repo recorded), oldest
/// first within each group. This is the "unlanded work must be loudly visible" surface.
pub fn debt(model: &TaskReadModel) -> BTreeMap<Option<PathBuf>, Vec<DebtEntry<'_>>> {
    let mut groups: BTreeMap<Option<PathBuf>, Vec<DebtEntry<'_>>> = BTreeMap::new();
    for task in model.tasks.values() {
        if task.state.is_terminal() {
            continue;
        }
        let Some(rev) = task.current_revision() else { continue };
        if !matches!(rev.state, RevisionState::Ready | RevisionState::MergedLocal) {
            continue;
        }
        let since = rev.pass.as_ref().map(|p| p.ts).unwrap_or(task.last_ts);
        groups.entry(task.repo.clone()).or_default().push(DebtEntry {
            task,
            revision_n: rev.revision.n,
            branch: rev.revision.branch.clone(),
            upstream: rev.revision.upstream.clone(),
            state: rev.state,
            since,
            issues: rev.issues.iter().map(|i| i.describe()).collect(),
        });
    }
    for entries in groups.values_mut() {
        entries.sort_by_key(|e| e.since);
    }
    groups
}

/// Resolve a task-id prefix (short id) to the unique matching task id.
pub fn resolve_id<'m>(model: &'m TaskReadModel, prefix: &str) -> Result<&'m str, String> {
    let matches: Vec<&str> = model
        .tasks
        .keys()
        .filter(|id| id.starts_with(prefix))
        .map(|s| s.as_str())
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("no task matches '{prefix}'")),
        many => Err(format!(
            "'{prefix}' is ambiguous ({} matches: {} ...)",
            many.len(),
            &many[..many.len().min(3)].join(", ")
        )),
    }
}

/// Effective-state helper for callers that need a display string with the layer visible.
pub fn effective_display(task: &TaskProjection) -> String {
    match task.effective_state() {
        EffectiveState::Base(s) => s.as_str().to_string(),
        EffectiveState::Revision(s) => format!("rev:{}", s.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::model::{Revision, TaskEvent, TaskEventKind};
    use crate::task::reduce::TaskReducer;

    fn sha(c: char) -> String {
        std::iter::repeat(c).take(40).collect()
    }

    struct Log {
        events: Vec<TaskEvent>,
        n: u64,
    }

    impl Log {
        fn new() -> Self {
            Log { events: Vec::new(), n: 0 }
        }
        fn next_id(&mut self) -> String {
            self.n += 1;
            format!("00000000-0000-7000-8000-{:012}", self.n)
        }
        fn push(&mut self, task_id: &str, by: &str, kind: TaskEventKind) {
            let id = self.next_id();
            self.events.push(TaskEvent {
                id,
                task_id: task_id.to_string(),
                ts: "2026-07-16T12:00:00Z".parse().unwrap(),
                by: by.to_string(),
                kind,
            });
        }
        fn open(&mut self, by: &str, repo: Option<&str>, assignee: Option<&str>) -> String {
            let id = self.next_id();
            self.events.push(TaskEvent {
                id: id.clone(),
                task_id: id.clone(),
                ts: "2026-07-16T12:00:00Z".parse().unwrap(),
                by: by.to_string(),
                kind: TaskEventKind::Opened {
                    title: "t".into(),
                    body: String::new(),
                    repo: repo.map(PathBuf::from),
                    issue: None,
                    channel: "tasks".into(),
                    assignee: assignee.map(String::from),
                },
            });
            id
        }
        fn model(&self) -> TaskReadModel {
            TaskReducer::reduce(&self.events).unwrap()
        }
    }

    fn propose_and_pass(log: &mut Log, task: &str) {
        log.push(
            task,
            "agent:author",
            TaskEventKind::RevisionProposed {
                revision: Revision {
                    n: 1,
                    branch: "task/x".into(),
                    worktree: None,
                    upstream: "origin/main".into(),
                    base: sha('0'),
                    review_sha: sha('1'),
                    patch_id: sha('b'),
                    reviewer: Some("agent:reviewer".into()),
                    session_ref: None,
                },
            },
        );
        log.push(
            task,
            "agent:reviewer",
            TaskEventKind::ReviewPassed {
                reviewer: "agent:reviewer".into(),
                session_ref: None,
                independence: None,
            },
        );
    }

    #[test]
    fn list_filters_by_effective_state_and_hides_terminal_by_default() {
        let mut log = Log::new();
        let open = log.open("h", None, None);
        let done = log.open("h", None, None);
        log.push(&done, "a", TaskEventKind::Claimed { assignee: "a".into() });
        log.push(&done, "a", TaskEventKind::Done { observed: None });
        let ready = log.open("h", Some("/tmp/repo"), None);
        log.push(&ready, "agent:author", TaskEventKind::Claimed { assignee: "agent:author".into() });
        propose_and_pass(&mut log, &ready);

        let model = log.model();
        let all = list(&model, &TaskFilter::default());
        assert_eq!(all.len(), 2, "done task hidden by default");
        assert!(all.iter().any(|t| t.task_id == open));

        let ready_only = list(
            &model,
            &TaskFilter { state: Some("ready".into()), ..Default::default() },
        );
        assert_eq!(ready_only.len(), 1);
        assert_eq!(ready_only[0].task_id, ready);

        let done_only = list(
            &model,
            &TaskFilter { state: Some("done".into()), ..Default::default() },
        );
        assert_eq!(done_only.len(), 1);
        assert_eq!(done_only[0].task_id, done);
    }

    #[test]
    fn inbox_covers_all_four_reasons() {
        let mut log = Log::new();
        let assigned = log.open("human", None, Some("agent:a"));
        let claimed = log.open("human", None, None);
        log.push(&claimed, "agent:a", TaskEventKind::Claimed { assignee: "agent:a".into() });
        let review = log.open("human", Some("/tmp/r"), None);
        log.push(&review, "agent:a", TaskEventKind::Claimed { assignee: "agent:a".into() });
        log.push(
            &review,
            "agent:a",
            TaskEventKind::RevisionProposed {
                revision: Revision {
                    n: 1,
                    branch: "task/r".into(),
                    worktree: None,
                    upstream: "origin/main".into(),
                    base: sha('7'),
                    review_sha: sha('2'),
                    patch_id: sha('c'),
                    reviewer: Some("agent:b".into()),
                    session_ref: None,
                },
            },
        );
        let unlanded = log.open("human", Some("/tmp/r"), None);
        log.push(&unlanded, "agent:a", TaskEventKind::Claimed { assignee: "agent:a".into() });
        propose_and_pass(&mut log, &unlanded);

        let model = log.model();

        let a: Vec<_> = inbox(&model, "agent:a");
        let reasons: Vec<(String, InboxReason)> = a
            .iter()
            .map(|e| (e.task.task_id.clone(), e.reason))
            .collect();
        assert!(reasons.contains(&(assigned.clone(), InboxReason::AssignedOpen)));
        assert!(reasons.contains(&(claimed.clone(), InboxReason::ClaimedByYou)));
        assert!(reasons.contains(&(unlanded.clone(), InboxReason::YourUnlandedWork)));
        // The review task is claimed by a, but its revision awaits b — for a it shows as claimed.
        assert!(reasons.contains(&(review.clone(), InboxReason::ClaimedByYou)));

        let b: Vec<_> = inbox(&model, "agent:b");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].reason, InboxReason::AwaitingYourReview);
        assert_eq!(b[0].task.task_id, review);
    }

    #[test]
    fn debt_lists_unlanded_reviewed_work_by_repo() {
        let mut log = Log::new();
        let landed = log.open("h", Some("/tmp/r1"), None);
        propose_and_pass(&mut log, &landed);
        log.push(
            &landed,
            "verifier",
            TaskEventKind::Landed { upstream_head: sha('f'), observed_patch_id: sha('b') },
        );
        let owed = log.open("h", Some("/tmp/r1"), None);
        propose_and_pass(&mut log, &owed);
        let owed2 = log.open("h", Some("/tmp/r2"), None);
        propose_and_pass(&mut log, &owed2);
        log.push(
            &owed2,
            "verifier",
            TaskEventKind::MergedLocal { from_sha: sha('e'), to_sha: sha('1') },
        );

        let model = log.model();
        let groups = debt(&model);
        assert_eq!(groups.len(), 2);
        let r1 = &groups[&Some(PathBuf::from("/tmp/r1"))];
        assert_eq!(r1.len(), 1, "landed task owes nothing");
        assert_eq!(r1[0].task.task_id, owed);
        assert_eq!(r1[0].state, RevisionState::Ready);
        let r2 = &groups[&Some(PathBuf::from("/tmp/r2"))];
        assert_eq!(r2[0].state, RevisionState::MergedLocal);
    }

    #[test]
    fn resolve_id_prefixes() {
        let mut log = Log::new();
        let a = log.open("h", None, None);
        let model = log.model();
        assert_eq!(resolve_id(&model, &a[..8]).unwrap(), a);
        assert!(resolve_id(&model, "ffffffff").is_err());
    }
}
