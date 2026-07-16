//! Git observation for the task substrate — the only source of `MergedLocal`/`Landed` truth
//! (law 1: observed, not attested) and of revision identity at propose time.
//!
//! Everything here runs `git` against a real repository and interprets only unambiguous output;
//! anything unexpected is a *finding*, never a default. **A git error is never treated as
//! landed.** The landed predicate is lifted from mission-control's `git_correlate.rs` — the
//! direct-FF short-circuit and the refuse-to-interpret cherry parse carry regression scars worth
//! keeping (p3A: a direct-FF land makes `git cherry` print nothing, which a naive parse reads as
//! "cannot verify").
//!
//! Not compiled for wasm (no processes there); the module is cfg-gated in `task/mod.rs`.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use super::model::{MergeFailure, Revision, RevisionState, TaskEventKind};
use super::reduce::{TaskIssue, TaskProjection};

/// Run git in `repo` with `args`; return trimmed stdout on success, Err with stderr otherwise.
fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .with_context(|| format!("running git {} in {}", args.join(" "), repo.display()))?;
    if !out.status.success() {
        bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            repo.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Classify one line of `git cherry <upstream> <sha> <sha>~1` output. `-` means the commit's
/// content is ALREADY upstream by patch-id equivalence (landed under any sha — the replayed
/// case). `+` means it is not. Anything else we refuse to interpret.
fn cherry_line_says_upstream(line: &str) -> Option<bool> {
    match line.trim().chars().next() {
        Some('-') => Some(true),
        Some('+') => Some(false),
        _ => None,
    }
}

/// Ask git whether `sha`'s content is already on `upstream`, by ancestry or patch-id. Returns
/// Err when git cannot answer — the caller records a finding and never assumes "landed".
///
/// Lifted from mission-control `src/git_correlate.rs:26-71`. A sha that is an ancestor of
/// upstream (including sha == tip) is trivially landed and makes `git cherry` print nothing, so
/// ancestry is asked FIRST; cherry handles the replayed-sha case via patch-id equivalence.
pub fn content_is_on_upstream(repo: &Path, sha: &str, upstream: &str) -> Result<bool> {
    let ancestor = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge-base", "--is-ancestor", sha, upstream])
        .output()
        .with_context(|| format!("could not run git merge-base in {}", repo.display()))?;
    // exit 0 = ancestor (trivially on upstream). exit 1 = not an ancestor (fall through to the
    // patch-id cherry check). Any other exit (bad sha, no repo) also falls through, where `git
    // cherry` re-hits the same error and reports it — we never treat an error as "landed".
    if ancestor.status.success() {
        return Ok(true);
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cherry", upstream, sha, &format!("{sha}~1")])
        .output()
        .with_context(|| format!("could not run git cherry in {}", repo.display()))?;
    if !output.status.success() {
        bail!(
            "git cherry {upstream} {sha} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(first) = stdout.lines().next() else {
        bail!("git cherry produced no output for {sha} — cannot verify the land");
    };
    cherry_line_says_upstream(first)
        .with_context(|| format!("unparseable git cherry output: {first:?}"))
}

/// Range patch-id between two fixed commits: `git diff <base> <sha> | git patch-id --stable`.
/// The cumulative-content identity of the whole branch — a tampered or dropped earlier commit
/// changes it, which is exactly the hole tip-only patch-ids left open.
pub fn range_patch_id(repo: &Path, base: &str, sha: &str) -> Result<String> {
    let diff = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff", base, sha])
        .output()
        .with_context(|| format!("running git diff in {}", repo.display()))?;
    if !diff.status.success() {
        bail!(
            "git diff {base}..{sha} failed in {}: {}",
            repo.display(),
            String::from_utf8_lossy(&diff.stderr).trim()
        );
    }
    if diff.stdout.is_empty() {
        bail!("empty diff {base}..{sha} — nothing to identify");
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["patch-id", "--stable"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("spawning git patch-id")?;
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().context("patch-id stdin")?;
        stdin.write_all(&diff.stdout).context("writing diff to patch-id")?;
    }
    let out = child.wait_with_output().context("waiting for git patch-id")?;
    if !out.status.success() {
        bail!("git patch-id failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let pid = stdout
        .split_whitespace()
        .next()
        .filter(|s| s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()))
        .context("unparseable git patch-id output")?;
    Ok(pid.to_string())
}

/// Observe a revision at propose time: resolve the branch tip (or verify a caller-supplied sha
/// matches it), record the merge-base, and compute the range patch-id. The returned [`Revision`]
/// is built from what git says, never from what an agent typed.
#[allow(clippy::too_many_arguments)]
pub fn observe_revision(
    repo: &Path,
    branch: &str,
    upstream: &str,
    sha_override: Option<&str>,
    n: u32,
    worktree: Option<std::path::PathBuf>,
    reviewer: Option<String>,
    session_ref: Option<String>,
) -> Result<Revision> {
    git(repo, &["rev-parse", "--git-dir"])
        .with_context(|| format!("{} is not a git repository", repo.display()))?;
    let tip = git(repo, &["rev-parse", "--verify", &format!("{branch}^{{commit}}")])
        .with_context(|| format!("branch '{branch}' not found in {}", repo.display()))?;
    let review_sha = match sha_override {
        Some(sha) => {
            let full = git(repo, &["rev-parse", "--verify", &format!("{sha}^{{commit}}")])?;
            if !full.eq_ignore_ascii_case(&tip) {
                bail!(
                    "supplied sha {} is not the tip of '{branch}' ({}); propose reviews exact tips",
                    &full[..12.min(full.len())],
                    &tip[..12.min(tip.len())]
                );
            }
            full
        }
        None => tip,
    };
    git(repo, &["rev-parse", "--verify", &format!("{upstream}^{{commit}}")])
        .with_context(|| format!("upstream '{upstream}' not found in {}", repo.display()))?;
    let base = git(repo, &["merge-base", upstream, &review_sha])?;
    if base.eq_ignore_ascii_case(&review_sha) {
        bail!("'{branch}' has no commits over {upstream} — nothing to review");
    }
    let patch_id = range_patch_id(repo, &base, &review_sha)?;
    Ok(Revision {
        n,
        branch: branch.to_string(),
        worktree,
        upstream: upstream.to_string(),
        base,
        review_sha,
        patch_id,
        reviewer,
        session_ref,
    })
}

/// Verify one task's current revision against git. Pure decision over observations: returns the
/// events to append (possibly none). Never errors on "git said no" — those become findings; only
/// infrastructure failure (e.g. cannot spawn git at all) errors.
pub fn verify_task(task: &TaskProjection, fetch: bool) -> Result<Vec<TaskEventKind>> {
    let Some(rev) = task.current_revision() else { return Ok(Vec::new()) };
    if !matches!(rev.state, RevisionState::Ready | RevisionState::MergedLocal) {
        return Ok(Vec::new());
    }

    // Repo reachable at all?
    let Some(repo) = task.repo.as_deref() else { return Ok(Vec::new()) };
    if !repo.is_dir() || git(repo, &["rev-parse", "--git-dir"]).is_err() {
        return Ok(finding(
            task,
            rev.state,
            TaskEventKind::SourceUnavailable {
                detail: format!("{} missing or not a git repository", repo.display()),
            },
        ));
    }

    if fetch {
        if let Some((remote, _)) = rev.revision.upstream.split_once('/') {
            // Best-effort: an unreachable remote must not block verification of local refs.
            let _ = git(repo, &["fetch", "--quiet", remote]);
        }
    }

    let r = &rev.revision;

    // 1. The landed predicate (observed on upstream, by ancestry or patch-id equivalence).
    match content_is_on_upstream(repo, &r.review_sha, &r.upstream) {
        Ok(true) => {
            // Recompute the observed patch-id from the RECORDED base — two fixed commits, valid
            // regardless of upstream movement. Guards a tampered stored patch_id; the reducer
            // cross-checks it against the revision.
            let observed_patch_id = match range_patch_id(repo, &r.base, &r.review_sha) {
                Ok(pid) => pid,
                Err(e) => {
                    return Ok(finding(
                        task,
                        rev.state,
                        merge_or_reconcile(rev.state, format!("landed but patch-id recompute failed: {e}")),
                    ))
                }
            };
            let upstream_head = git(repo, &["rev-parse", &r.upstream])?;
            return Ok(vec![TaskEventKind::Landed { upstream_head, observed_patch_id }]);
        }
        Ok(false) => {}
        Err(e) => {
            return Ok(finding(
                task,
                rev.state,
                merge_or_reconcile(rev.state, e.to_string()),
            ));
        }
    }

    // Not on upstream. From MergedLocal there is nothing further to observe (push pending).
    if rev.state == RevisionState::MergedLocal {
        return Ok(Vec::new());
    }

    // 2. Local-integration check: merged into the local primary line but not pushed? Consult
    // explicit refs (never HEAD — a checkout sitting on the task branch would self-certify).
    for local in ["main", "master"] {
        if local == r.upstream {
            break; // local-only workflow: the upstream check above already answered
        }
        if git(repo, &["rev-parse", "--verify", &format!("{local}^{{commit}}")]).is_err() {
            continue;
        }
        if content_is_on_upstream(repo, &r.review_sha, local).unwrap_or(false) {
            return Ok(vec![TaskEventKind::MergedLocal {
                from_sha: r.base.clone(),
                to_sha: r.review_sha.clone(),
            }]);
        }
        break; // only consult the first primary branch that exists
    }

    // 3. Observational findings about why this is not landable as reviewed.
    if let Some(wt) = &r.worktree {
        if !wt.is_dir() {
            return Ok(finding(
                task,
                rev.state,
                TaskEventKind::MergeFailed { reason: MergeFailure::MissingWorktree {} },
            ));
        }
    }
    match git(repo, &["rev-parse", "--verify", &format!("{}^{{commit}}", r.branch)]) {
        Err(_) => {
            return Ok(finding(
                task,
                rev.state,
                TaskEventKind::MergeFailed { reason: MergeFailure::MissingBranch {} },
            ))
        }
        Ok(tip) if !tip.eq_ignore_ascii_case(&r.review_sha) => {
            return Ok(finding(
                task,
                rev.state,
                TaskEventKind::MergeFailed { reason: MergeFailure::BranchTipChanged {} },
            ))
        }
        Ok(_) => {}
    }
    // Upstream moved past the recorded base → a plain fast-forward is no longer possible.
    let upstream_head = git(repo, &["rev-parse", &r.upstream])?;
    if !upstream_head.eq_ignore_ascii_case(&r.base) {
        return Ok(finding(
            task,
            rev.state,
            TaskEventKind::MergeFailed { reason: MergeFailure::NonFastForward {} },
        ));
    }

    // Reviewed, intact, fast-forwardable, just not landed yet: nothing to record.
    Ok(Vec::new())
}

/// Run the verifier over `ids` (or every task when `ids` is `None`) against `store`: verify each
/// task, append what was observed, post board notifications. Returns the appended events plus
/// non-fatal warnings. The shared engine behind `cv task verify`, the MCP `task_verify` tool, and
/// cvd's periodic pass.
pub fn run_verify(
    store: &super::store::TaskStore,
    ids: Option<&[String]>,
    fetch: bool,
) -> Result<(Vec<super::model::TaskEvent>, Vec<String>)> {
    let outcome = store.replay()?;
    let mut warnings = outcome.warnings.clone();
    let all_ids: Vec<String> = match ids {
        Some(ids) => ids.to_vec(),
        None => outcome.model.tasks.keys().cloned().collect(),
    };
    let mut appended = Vec::new();
    for tid in all_ids {
        let Some(task) = outcome.model.tasks.get(&tid) else {
            warnings.push(format!("unknown task {tid}"));
            continue;
        };
        let kinds = match verify_task(task, fetch) {
            Ok(kinds) => kinds,
            Err(e) => {
                warnings.push(format!("verify {tid}: {e}"));
                continue;
            }
        };
        for kind in kinds {
            let ev = store.append_verifier_event(super::store::new_event(
                Some(&tid),
                "cv-verify",
                kind,
            ))?;
            if let Err(e) = super::notify_board(&ev, &task.channel) {
                warnings.push(format!("board notification failed: {e}"));
            }
            appended.push(ev);
        }
    }
    Ok((appended, warnings))
}

/// Route an anomaly to the state-legal issue kind: `MergeFailed`/`SourceUnavailable` only apply
/// from `Ready`; from `MergedLocal` the grammar's issue kind is `ReconcileFailed`.
fn merge_or_reconcile(state: RevisionState, detail: String) -> TaskEventKind {
    if state == RevisionState::MergedLocal {
        TaskEventKind::ReconcileFailed { detail }
    } else {
        TaskEventKind::MergeFailed { reason: MergeFailure::GitFailed { detail } }
    }
}

/// Anti-spam: drop a finding identical to the revision's most recent issue, and route
/// `SourceUnavailable` from `MergedLocal` into `ReconcileFailed` (grammar legality).
fn finding(task: &TaskProjection, state: RevisionState, kind: TaskEventKind) -> Vec<TaskEventKind> {
    let kind = match kind {
        TaskEventKind::SourceUnavailable { detail } if state == RevisionState::MergedLocal => {
            TaskEventKind::ReconcileFailed { detail }
        }
        other => other,
    };
    let Some(rev) = task.current_revision() else { return vec![kind] };
    if let Some(last) = rev.issues.last() {
        let same = match (&kind, last) {
            (TaskEventKind::SourceUnavailable { detail }, TaskIssue::SourceUnavailable { detail: d, .. }) => detail == d,
            (TaskEventKind::MergeFailed { reason }, TaskIssue::MergeFailed { reason: r, .. }) => reason == r,
            (TaskEventKind::ReconcileFailed { detail }, TaskIssue::ReconcileFailed { detail: d, .. }) => detail == d,
            _ => false,
        };
        if same {
            return Vec::new();
        }
    }
    vec![kind]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::model::{TaskEvent, TaskEventKind};
    use crate::task::reduce::{TaskReadModel, TaskReducer};
    use std::path::PathBuf;

    // ── scratch-repo plumbing (pattern from mission-control git_correlate tests) ──

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cv-task-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tmp_repo() -> PathBuf {
        let dir = tmp_dir("git");
        run(&dir, &["init", "-q", "-b", "main"]);
        run(&dir, &["config", "user.email", "t@example.com"]);
        run(&dir, &["config", "user.name", "t"]);
        commit(&dir, "base.txt", "base", "base commit");
        dir
    }

    fn run(repo: &Path, args: &[&str]) {
        let out = Command::new("git").arg("-C").arg(repo).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn commit(repo: &Path, file: &str, content: &str, msg: &str) {
        std::fs::write(repo.join(file), content).unwrap();
        run(repo, &["add", "."]);
        run(repo, &["commit", "-q", "-m", msg]);
    }

    fn head(repo: &Path) -> String {
        git(repo, &["rev-parse", "HEAD"]).unwrap()
    }

    // Build a task model whose single task has a proposed+passed revision in `repo`.
    fn ready_task(repo: &Path, rev: Revision) -> TaskReadModel {
        let mk = |n: u64, task_id: &str, kind: TaskEventKind| TaskEvent {
            id: if n == 1 {
                task_id.to_string()
            } else {
                format!("00000000-0000-7000-8000-{n:012}")
            },
            task_id: task_id.to_string(),
            ts: "2026-07-16T12:00:00Z".parse().unwrap(),
            by: "t".into(),
            kind,
        };
        let tid = "00000000-0000-7000-8000-000000000001";
        let events = vec![
            mk(
                1,
                tid,
                TaskEventKind::Opened {
                    title: "t".into(),
                    body: String::new(),
                    repo: Some(repo.to_path_buf()),
                    issue: None,
                    channel: "tasks".into(),
                    assignee: None,
                },
            ),
            mk(2, tid, TaskEventKind::RevisionProposed { revision: rev }),
            mk(
                3,
                tid,
                TaskEventKind::ReviewPassed {
                    reviewer: "t".into(),
                    session_ref: None,
                    independence: None,
                },
            ),
        ];
        TaskReducer::reduce(&events).unwrap()
    }

    fn the_task(model: &TaskReadModel) -> &TaskProjection {
        model.tasks.values().next().unwrap()
    }

    fn branch_revision(repo: &Path, branch: &str) -> Revision {
        observe_revision(repo, branch, "main", None, 1, None, None, None).unwrap()
    }

    // ── the eight scenarios ──

    #[test]
    fn direct_ff_land_is_observed_landed() {
        let repo = tmp_repo();
        run(&repo, &["checkout", "-q", "-b", "task/a"]);
        commit(&repo, "a.txt", "a", "feat a");
        let rev = branch_revision(&repo, "task/a");
        run(&repo, &["checkout", "-q", "main"]);
        run(&repo, &["merge", "-q", "--ff-only", "task/a"]);

        let model = ready_task(&repo, rev.clone());
        let events = verify_task(the_task(&model), false).unwrap();
        assert_eq!(events.len(), 1);
        let TaskEventKind::Landed { observed_patch_id, .. } = &events[0] else {
            panic!("expected Landed, got {events:?}");
        };
        assert_eq!(observed_patch_id, &rev.patch_id, "recompute from recorded base matches");
    }

    #[test]
    fn cherry_pick_replay_lands_via_patch_id() {
        let repo = tmp_repo();
        run(&repo, &["checkout", "-q", "-b", "task/b"]);
        commit(&repo, "b.txt", "b", "feat b");
        let rev = branch_revision(&repo, "task/b");
        // main moves independently, then the reviewed commit is REPLAYED (new sha).
        run(&repo, &["checkout", "-q", "main"]);
        commit(&repo, "other.txt", "other", "unrelated");
        run(&repo, &["cherry-pick", &rev.review_sha]);

        let model = ready_task(&repo, rev.clone());
        let events = verify_task(the_task(&model), false).unwrap();
        assert!(
            matches!(events.as_slice(), [TaskEventKind::Landed { .. }]),
            "replayed sha must land via cherry patch-id equivalence: {events:?}"
        );
    }

    #[test]
    fn unlanded_intact_branch_yields_no_events() {
        let repo = tmp_repo();
        run(&repo, &["checkout", "-q", "-b", "task/c"]);
        commit(&repo, "c.txt", "c", "feat c");
        let rev = branch_revision(&repo, "task/c");
        run(&repo, &["checkout", "-q", "main"]);

        let model = ready_task(&repo, rev);
        let events = verify_task(the_task(&model), false).unwrap();
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn deleted_branch_is_missing_branch() {
        let repo = tmp_repo();
        run(&repo, &["checkout", "-q", "-b", "task/d"]);
        commit(&repo, "d.txt", "d", "feat d");
        let rev = branch_revision(&repo, "task/d");
        run(&repo, &["checkout", "-q", "main"]);
        run(&repo, &["branch", "-q", "-D", "task/d"]);

        let model = ready_task(&repo, rev);
        let events = verify_task(the_task(&model), false).unwrap();
        assert!(
            matches!(
                events.as_slice(),
                [TaskEventKind::MergeFailed { reason: MergeFailure::MissingBranch {} }]
            ),
            "{events:?}"
        );
    }

    #[test]
    fn extra_commit_is_branch_tip_changed() {
        let repo = tmp_repo();
        run(&repo, &["checkout", "-q", "-b", "task/e"]);
        commit(&repo, "e.txt", "e", "feat e");
        let rev = branch_revision(&repo, "task/e");
        commit(&repo, "e2.txt", "e2", "sneaky extra commit after review");
        run(&repo, &["checkout", "-q", "main"]);

        let model = ready_task(&repo, rev);
        let events = verify_task(the_task(&model), false).unwrap();
        assert!(
            matches!(
                events.as_slice(),
                [TaskEventKind::MergeFailed { reason: MergeFailure::BranchTipChanged {} }]
            ),
            "{events:?}"
        );
    }

    #[test]
    fn upstream_movement_is_non_fast_forward() {
        let repo = tmp_repo();
        run(&repo, &["checkout", "-q", "-b", "task/f"]);
        commit(&repo, "f.txt", "f", "feat f");
        let rev = branch_revision(&repo, "task/f");
        run(&repo, &["checkout", "-q", "main"]);
        commit(&repo, "moved.txt", "m", "main moved");

        let model = ready_task(&repo, rev);
        let events = verify_task(the_task(&model), false).unwrap();
        assert!(
            matches!(
                events.as_slice(),
                [TaskEventKind::MergeFailed { reason: MergeFailure::NonFastForward {} }]
            ),
            "{events:?}"
        );
    }

    #[test]
    fn missing_repo_is_source_unavailable_and_repeat_is_deduped() {
        let repo = tmp_repo();
        run(&repo, &["checkout", "-q", "-b", "task/g"]);
        commit(&repo, "g.txt", "g", "feat g");
        let rev = branch_revision(&repo, "task/g");

        // Point the task at a directory that is not a repo.
        let gone = tmp_dir("gone");
        let mut model = ready_task(&repo, rev);
        let tid = model.tasks.keys().next().unwrap().clone();
        model.tasks.get_mut(&tid).unwrap().repo = Some(gone);

        let events = verify_task(the_task(&model), false).unwrap();
        assert!(
            matches!(events.as_slice(), [TaskEventKind::SourceUnavailable { .. }]),
            "{events:?}"
        );

        // Apply the finding, verify again: identical finding is suppressed (anti-spam).
        let t = model.tasks.get_mut(&tid).unwrap();
        t.revisions.last_mut().unwrap().issues.push(TaskIssue::SourceUnavailable {
            detail: match &events[0] {
                TaskEventKind::SourceUnavailable { detail } => detail.clone(),
                _ => unreachable!(),
            },
            event_id: "x".into(),
        });
        let again = verify_task(the_task(&model), false).unwrap();
        assert!(again.is_empty(), "identical finding must not spam the log: {again:?}");
    }

    #[test]
    fn landed_revision_is_not_reverified() {
        let repo = tmp_repo();
        run(&repo, &["checkout", "-q", "-b", "task/h"]);
        commit(&repo, "h.txt", "h", "feat h");
        let rev = branch_revision(&repo, "task/h");
        run(&repo, &["checkout", "-q", "main"]);
        run(&repo, &["merge", "-q", "--ff-only", "task/h"]);

        let mut model = ready_task(&repo, rev.clone());
        let tid = model.tasks.keys().next().unwrap().clone();
        // Simulate the landed fold.
        {
            let t = model.tasks.get_mut(&tid).unwrap();
            let r = t.revisions.last_mut().unwrap();
            r.state = RevisionState::Landed;
            r.landed = Some((head(&repo), rev.patch_id.clone()));
        }
        let events = verify_task(the_task(&model), false).unwrap();
        assert!(events.is_empty(), "terminal revisions are not re-verified: {events:?}");
    }

    #[test]
    fn tampered_stored_patch_id_is_caught_by_the_reducer() {
        let repo = tmp_repo();
        run(&repo, &["checkout", "-q", "-b", "task/i"]);
        commit(&repo, "i.txt", "i", "feat i");
        let mut rev = branch_revision(&repo, "task/i");
        rev.patch_id = "f".repeat(40); // tampered after review
        run(&repo, &["checkout", "-q", "main"]);
        run(&repo, &["merge", "-q", "--ff-only", "task/i"]);

        let model = ready_task(&repo, rev);
        let task = the_task(&model);
        let events = verify_task(task, false).unwrap();
        let [landed @ TaskEventKind::Landed { .. }] = events.as_slice() else {
            panic!("expected Landed observation, got {events:?}");
        };
        // The observation is honest, but folding it against the tampered revision fails loudly.
        let mut reducer = TaskReducer::new();
        let mut n = 0u64;
        let mut apply = |kind: TaskEventKind, task_id: &str| {
            n += 1;
            let id = if n == 1 {
                task_id.to_string()
            } else {
                format!("00000000-0000-7000-8000-{n:012}")
            };
            reducer.apply(&TaskEvent {
                id,
                task_id: task_id.to_string(),
                ts: "2026-07-16T12:00:00Z".parse().unwrap(),
                by: "t".into(),
                kind,
            })
        };
        let tid = task.task_id.clone();
        apply(
            TaskEventKind::Opened {
                title: "t".into(),
                body: String::new(),
                repo: task.repo.clone(),
                issue: None,
                channel: "tasks".into(),
                assignee: None,
            },
            &tid,
        )
        .unwrap();
        apply(
            TaskEventKind::RevisionProposed {
                revision: task.revisions[0].revision.clone(),
            },
            &tid,
        )
        .unwrap();
        apply(
            TaskEventKind::ReviewPassed { reviewer: "t".into(), session_ref: None, independence: None },
            &tid,
        )
        .unwrap();
        let err = apply(landed.clone(), &tid).unwrap_err();
        assert!(
            matches!(err, crate::task::reduce::ReduceError::PatchIdMismatch { .. }),
            "{err:?}"
        );
    }

    // ── propose-time observation ──

    #[test]
    fn observe_revision_reads_identity_from_git() {
        let repo = tmp_repo();
        run(&repo, &["checkout", "-q", "-b", "task/x"]);
        commit(&repo, "feat.txt", "feature", "feat commit");

        let rev = observe_revision(&repo, "task/x", "main", None, 1, None, None, None).unwrap();
        assert_eq!(rev.review_sha.len(), 40);
        assert_eq!(rev.patch_id.len(), 40);
        assert_eq!(rev.review_sha, head(&repo));
        assert_eq!(rev.base, git(&repo, &["rev-parse", "main"]).unwrap());

        // A sha override that isn't the tip is refused.
        let base = git(&repo, &["rev-parse", "main"]).unwrap();
        let err =
            observe_revision(&repo, "task/x", "main", Some(&base), 2, None, None, None).unwrap_err();
        assert!(err.to_string().contains("not the tip"), "{err}");

        // An empty branch (no commits over upstream) is refused.
        run(&repo, &["checkout", "-q", "-b", "task/empty", "main"]);
        let err =
            observe_revision(&repo, "task/empty", "main", None, 1, None, None, None).unwrap_err();
        assert!(err.to_string().contains("no commits over"), "{err}");
    }

    #[test]
    fn range_patch_id_covers_the_whole_branch() {
        let repo = tmp_repo();
        let base = head(&repo);
        run(&repo, &["checkout", "-q", "-b", "task/two"]);
        commit(&repo, "one.txt", "1", "first");
        let one_commit = range_patch_id(&repo, &base, "HEAD").unwrap();
        commit(&repo, "two.txt", "2", "second");
        let two_commits = range_patch_id(&repo, &base, "HEAD").unwrap();
        assert_ne!(
            one_commit, two_commits,
            "adding a commit must change the range patch-id (tip-only would not)"
        );
    }

    #[test]
    fn missing_branch_and_non_repo_are_errors() {
        let repo = tmp_repo();
        assert!(observe_revision(&repo, "nope", "main", None, 1, None, None, None).is_err());
        let not_repo = tmp_dir("notrepo");
        assert!(observe_revision(&not_repo, "main", "main", None, 1, None, None, None).is_err());
    }
}
