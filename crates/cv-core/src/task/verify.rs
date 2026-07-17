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
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::model::{MergeFailure, Revision, RevisionState, TaskEventKind, VERIFIER_BY};
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

/// Options for [`run_verify`].
#[derive(Clone, Debug, Default)]
pub struct VerifyOptions {
    /// `git fetch` each revision's upstream remote before observing.
    pub fetch: bool,
    /// Skip re-observation of Landed revisions (opt-out for huge histories; suspects recorded by
    /// earlier passes are preserved, not cleared).
    pub skip_landed: bool,
    /// The periodic interval when driven by `cvd watch`, recorded in the heartbeat so readers can
    /// call the verifier stale (age > 2x interval). `None` for one-shot CLI/MCP passes.
    pub interval_secs: Option<u64>,
    /// Suppress board notifications (tests and embedded runs).
    pub quiet: bool,
}

/// A Landed revision whose reviewed content is *no longer observed* on its upstream — either a
/// forged Landed event or a genuinely rolled-back land. Not an event (the grammar has no un-land
/// transition and law 1 doesn't need one): a per-pass observation, persisted in the heartbeat so
/// the debt view can render it as visible debt again.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SuspectLanded {
    pub task_id: String,
    pub revision: u32,
    pub upstream: String,
    pub title: String,
    pub detail: String,
}

/// The verifier's liveness marker, written to `<tasks dir>/last_verify.json` after every pass.
/// Its absence means landing state has NEVER been verified; a stale one means the periodic
/// verifier is dead — both are loud on the debt surfaces (G1: silence is never evidence of
/// health).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerifyHeartbeat {
    pub ts: DateTime<Utc>,
    pub tasks_checked: usize,
    pub events_appended: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suspect_landed: Vec<SuspectLanded>,
}

const HEARTBEAT_FILE: &str = "last_verify.json";

/// Read the heartbeat under `tasks_dir`, if a valid one exists (a torn/absent/foreign file is
/// simply "no heartbeat" — the warning path handles it).
pub fn read_heartbeat(tasks_dir: &Path) -> Option<VerifyHeartbeat> {
    let raw = std::fs::read_to_string(tasks_dir.join(HEARTBEAT_FILE)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_heartbeat(tasks_dir: &Path, hb: &VerifyHeartbeat) -> Result<()> {
    std::fs::create_dir_all(tasks_dir)
        .with_context(|| format!("creating tasks dir {}", tasks_dir.display()))?;
    let tmp = tasks_dir.join(format!("{HEARTBEAT_FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(hb).context("serializing heartbeat")?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, tasks_dir.join(HEARTBEAT_FILE)).context("publishing heartbeat")?;
    Ok(())
}

/// The loud line for a missing or stale heartbeat, if one is warranted. `None` heartbeat is the
/// loudest case; a present one is stale when older than 2x its own recorded interval (one-shot
/// heartbeats with no interval can't be judged stale and never warn).
pub fn heartbeat_warning(hb: Option<&VerifyHeartbeat>) -> Option<String> {
    let Some(hb) = hb else {
        return Some(
            "landing state has NEVER been verified — run `cv task verify --all` or enable \
             `cvd watch --verify-interval`"
                .to_string(),
        );
    };
    let interval = hb.interval_secs?;
    let age = Utc::now().signed_duration_since(hb.ts).num_seconds().max(0) as u64;
    (age > interval.saturating_mul(2)).then(|| {
        format!(
            "verifier heartbeat is STALE: last pass {} ({age}s ago, expected every {interval}s) \
             — the periodic verifier may be dead and the landing state out of date",
            hb.ts.to_rfc3339()
        )
    })
}

/// Re-observe a task whose current revision is recorded Landed (G3): landed-ness is monotone-true
/// for genuine lands, so `Ok(false)` — the reviewed content is NOT on upstream — contradicts the
/// record: a forged Landed event or a rolled-back land. A git error observes nothing (a pruned
/// replayed sha is indistinguishable from tampering) and is never treated as forged.
fn reobserve_landed(task: &TaskProjection) -> Option<SuspectLanded> {
    let rev = task.current_revision()?;
    if rev.state != RevisionState::Landed {
        return None;
    }
    let repo = task.repo.as_deref()?;
    if !repo.is_dir() {
        return None;
    }
    let r = &rev.revision;
    match content_is_on_upstream(repo, &r.review_sha, &r.upstream) {
        Ok(false) => Some(SuspectLanded {
            task_id: task.task_id.clone(),
            revision: r.n,
            upstream: r.upstream.clone(),
            title: task.title.clone(),
            detail: format!(
                "recorded Landed but content is not observed on {} — possible forged or \
                 rolled-back land",
                r.upstream
            ),
        }),
        Ok(true) | Err(_) => None,
    }
}

/// Run the verifier over `ids` (or every task when `ids` is `None`) against `store`: verify each
/// task, append what was observed, post board notifications, re-observe Landed revisions, and
/// write the heartbeat. Returns the appended events plus non-fatal warnings. The shared engine
/// behind `cv task verify`, the MCP `task_verify` tool, and cvd's periodic pass.
///
/// One bad task never aborts the pass: a rejected append (e.g. a tampered stored patch-id turning
/// the Landed fold into `PatchIdMismatch`) becomes a per-task warning and the loop continues.
pub fn run_verify(
    store: &super::store::TaskStore,
    ids: Option<&[String]>,
    opts: &VerifyOptions,
) -> Result<(Vec<super::model::TaskEvent>, Vec<String>)> {
    let outcome = store.replay()?;
    let mut warnings = outcome.warnings.clone();
    let all_ids: Vec<String> = match ids {
        Some(ids) => ids.to_vec(),
        None => outcome.model.tasks.keys().cloned().collect(),
    };
    let mut appended = Vec::new();
    let mut suspects: Vec<SuspectLanded> = Vec::new();
    let mut checked: Vec<String> = Vec::new();
    for tid in all_ids {
        let Some(task) = outcome.model.tasks.get(&tid) else {
            warnings.push(format!("unknown task {tid}"));
            continue;
        };
        checked.push(tid.clone());
        if !opts.skip_landed {
            if let Some(s) = reobserve_landed(task) {
                warnings.push(format!(
                    "task {} rev {} {}",
                    &s.task_id[..s.task_id.len().min(8)],
                    s.revision,
                    s.detail
                ));
                suspects.push(s);
            }
        }
        let kinds = match verify_task(task, opts.fetch) {
            Ok(kinds) => kinds,
            Err(e) => {
                warnings.push(format!("verify {tid}: {e}"));
                continue;
            }
        };
        for kind in kinds {
            let tag = kind.tag();
            match store.append_verifier_event(super::store::new_event(Some(&tid), VERIFIER_BY, kind))
            {
                Ok(ev) => {
                    if !opts.quiet {
                        if let Err(e) = super::notify_board(&ev, &task.channel) {
                            warnings.push(format!("board notification failed: {e}"));
                        }
                    }
                    appended.push(ev);
                }
                Err(e) => {
                    // A persistently-invalid append must not wedge the pass for every other task.
                    warnings.push(format!(
                        "task {tid}: observed '{tag}' but the append was rejected ({e}) — \
                         continuing with remaining tasks"
                    ));
                }
            }
        }
    }

    // Heartbeat (G1). Suspects observed by earlier passes stay recorded unless this pass actually
    // re-observed their task (a partial or --skip-landed pass must not launder a suspect away).
    if let Some(prev) = read_heartbeat(store.dir()) {
        for s in prev.suspect_landed {
            let reobserved_now = !opts.skip_landed && checked.contains(&s.task_id);
            if !reobserved_now && !suspects.iter().any(|n| n.task_id == s.task_id) {
                suspects.push(s);
            }
        }
    }
    suspects.sort_by(|a, b| a.task_id.cmp(&b.task_id));
    let hb = VerifyHeartbeat {
        ts: Utc::now(),
        tasks_checked: checked.len(),
        events_appended: appended.len(),
        interval_secs: opts.interval_secs,
        suspect_landed: suspects,
    };
    if let Err(e) = write_heartbeat(store.dir(), &hb) {
        warnings.push(format!("could not write verifier heartbeat: {e}"));
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

/// How far back [`finding`] looks for an identical issue. Last-1 let an alternating pair
/// (NonFastForward / GitFailed flapping between passes) grow the log forever; a shallow window
/// still re-records anything genuinely intermittent.
const DEDUP_WINDOW: usize = 5;

/// Anti-spam: drop a finding identical to any of the revision's last [`DEDUP_WINDOW`] issues, and
/// route `SourceUnavailable` from `MergedLocal` into `ReconcileFailed` (grammar legality).
fn finding(task: &TaskProjection, state: RevisionState, kind: TaskEventKind) -> Vec<TaskEventKind> {
    let kind = match kind {
        TaskEventKind::SourceUnavailable { detail } if state == RevisionState::MergedLocal => {
            TaskEventKind::ReconcileFailed { detail }
        }
        other => other,
    };
    let Some(rev) = task.current_revision() else { return vec![kind] };
    let same = |issue: &TaskIssue| match (&kind, issue) {
        (TaskEventKind::SourceUnavailable { detail }, TaskIssue::SourceUnavailable { detail: d, .. }) => detail == d,
        (TaskEventKind::MergeFailed { reason }, TaskIssue::MergeFailed { reason: r, .. }) => reason == r,
        (TaskEventKind::ReconcileFailed { detail }, TaskIssue::ReconcileFailed { detail: d, .. }) => detail == d,
        _ => false,
    };
    if rev.issues.iter().rev().take(DEDUP_WINDOW).any(same) {
        return Vec::new();
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
        // Hermetic git: blank out the machine's global/system config so commits never pick up
        // commit-signing (1Password prompts = machine-wide flake + per-commit latency) or other
        // user config. Production code only *reads* repos, so this lives on the fixture helper.
        let out = Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
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
                    receipts: None,
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

        // Alternation: an unrelated issue lands between two identical findings (the flapping
        // NonFastForward/GitFailed pair). Last-1 dedup would re-record; the last-5 window doesn't.
        let t = model.tasks.get_mut(&tid).unwrap();
        t.revisions.last_mut().unwrap().issues.push(TaskIssue::MergeFailed {
            reason: MergeFailure::NonFastForward {},
            event_id: "y".into(),
        });
        let alternated = verify_task(the_task(&model), false).unwrap();
        assert!(
            alternated.is_empty(),
            "a finding identical to a recent (non-last) issue must not spam the log: {alternated:?}"
        );

        // But once the identical issue has scrolled out of the window, it records again — the
        // dedup is a spam guard, not amnesia about a still-broken world.
        let t = model.tasks.get_mut(&tid).unwrap();
        for i in 0..5 {
            t.revisions.last_mut().unwrap().issues.push(TaskIssue::MergeFailed {
                reason: MergeFailure::GitFailed { detail: format!("distinct {i}") },
                event_id: format!("z{i}"),
            });
        }
        let reemitted = verify_task(the_task(&model), false).unwrap();
        assert!(
            matches!(reemitted.as_slice(), [TaskEventKind::SourceUnavailable { .. }]),
            "an issue older than the window records again: {reemitted:?}"
        );
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
            TaskEventKind::ReviewPassed { reviewer: "t".into(), session_ref: None, independence: None, receipts: None },
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

    // ── run_verify: resilience, heartbeat, forged-land re-observation ──

    use crate::task::store::{new_event, TaskStore};

    fn open_in(store: &TaskStore, repo: &Path, title: &str) -> String {
        store
            .append_agent_event(new_event(
                None,
                "agent:author",
                TaskEventKind::Opened {
                    title: title.into(),
                    body: String::new(),
                    repo: Some(repo.to_path_buf()),
                    issue: None,
                    channel: "tasks".into(),
                    assignee: None,
                },
            ))
            .unwrap()
            .task_id
    }

    fn propose_and_pass_in(store: &TaskStore, task: &str, revision: Revision) {
        store
            .append_agent_event(new_event(
                Some(task),
                "agent:author",
                TaskEventKind::RevisionProposed { revision },
            ))
            .unwrap();
        store
            .append_agent_event(new_event(
                Some(task),
                "agent:reviewer",
                TaskEventKind::ReviewPassed {
                    reviewer: "agent:reviewer".into(),
                    session_ref: None,
                    independence: None,
                    receipts: None,
                },
            ))
            .unwrap();
    }

    fn quiet() -> VerifyOptions {
        VerifyOptions { quiet: true, ..Default::default() }
    }

    #[test]
    fn run_verify_continues_past_poisoned_task_and_writes_heartbeat() {
        let repo = tmp_repo();
        let store = TaskStore::at(tmp_dir("store"));

        // Poisoned task: honest branch, but the RECORDED patch_id is tampered, so folding the
        // verifier's Landed observation hits PatchIdMismatch and the append is rejected.
        run(&repo, &["checkout", "-q", "-b", "task/poison"]);
        commit(&repo, "p.txt", "p", "feat p");
        let mut rev_p =
            observe_revision(&repo, "task/poison", "main", None, 1, None, None, None).unwrap();
        rev_p.patch_id = "f".repeat(40);
        let poisoned = open_in(&store, &repo, "poisoned");
        propose_and_pass_in(&store, &poisoned, rev_p);

        // Good task on its own branch.
        run(&repo, &["checkout", "-q", "main"]);
        run(&repo, &["checkout", "-q", "-b", "task/good"]);
        commit(&repo, "g.txt", "g", "feat g");
        let rev_g =
            observe_revision(&repo, "task/good", "main", None, 1, None, None, None).unwrap();
        let good = open_in(&store, &repo, "good");
        propose_and_pass_in(&store, &good, rev_g);

        // Land both on main.
        run(&repo, &["checkout", "-q", "main"]);
        run(&repo, &["merge", "-q", "--no-edit", "task/poison"]);
        run(&repo, &["merge", "-q", "--no-edit", "task/good"]);

        let (appended, warnings) = run_verify(&store, None, &quiet()).unwrap();
        assert_eq!(appended.len(), 1, "good task still verified: {appended:?}\n{warnings:?}");
        assert_eq!(appended[0].task_id, good);
        assert!(matches!(appended[0].kind, TaskEventKind::Landed { .. }));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("append was rejected") && w.contains(&poisoned)),
            "the poisoned task is a warning, not an abort: {warnings:?}"
        );

        let hb = read_heartbeat(store.dir()).expect("heartbeat written after the pass");
        assert_eq!(hb.tasks_checked, 2);
        assert_eq!(hb.events_appended, 1);
        assert!(hb.suspect_landed.is_empty());
        assert_eq!(hb.interval_secs, None);
    }

    #[test]
    fn forged_landed_warns_on_provenance_and_reobservation_flags_it() {
        let repo = tmp_repo();
        let store = TaskStore::at(tmp_dir("store"));
        run(&repo, &["checkout", "-q", "-b", "task/forge"]);
        commit(&repo, "f.txt", "f", "feat f");
        let rev = observe_revision(&repo, "task/forge", "main", None, 1, None, None, None).unwrap();
        run(&repo, &["checkout", "-q", "main"]); // NOT merged — the land is a lie

        let task = open_in(&store, &repo, "forged land");
        let patch_id = rev.patch_id.clone();
        propose_and_pass_in(&store, &task, rev);

        // The forgery: echo a self-consistent Landed line (patch_id copied from the revision's
        // own record) straight into events.jsonl, exactly as a seat with Bash could.
        let forged = new_event(
            Some(&task),
            "agent:sneaky",
            TaskEventKind::Landed {
                upstream_head: head(&repo),
                observed_patch_id: patch_id,
            },
        );
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(store.dir().join("events.jsonl"))
                .unwrap();
            writeln!(f, "{}", serde_json::to_string(&forged).unwrap()).unwrap();
        }

        // Replay folds it (self-consistent) but the provenance scan is loud.
        let outcome = store.replay().unwrap();
        assert_eq!(outcome.quarantined, 0);
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| w.contains("attested, not observed") && w.contains("agent:sneaky")),
            "{:?}",
            outcome.warnings
        );
        assert_eq!(
            outcome.model.tasks[&task].current_revision().unwrap().state,
            RevisionState::Landed
        );

        // The periodic pass re-observes Landed: the content is NOT on upstream → suspect.
        let (appended, warnings) = run_verify(&store, None, &quiet()).unwrap();
        assert!(appended.is_empty(), "{appended:?}");
        assert!(
            warnings.iter().any(|w| w.contains("possible forged or rolled-back land")),
            "{warnings:?}"
        );
        let hb = read_heartbeat(store.dir()).unwrap();
        assert_eq!(hb.suspect_landed.len(), 1);
        assert_eq!(hb.suspect_landed[0].task_id, task);

        // A --skip-landed pass must not launder the suspect away.
        run_verify(&store, None, &VerifyOptions { skip_landed: true, ..quiet() }).unwrap();
        assert_eq!(read_heartbeat(store.dir()).unwrap().suspect_landed.len(), 1);
    }

    #[test]
    fn heartbeat_warning_covers_absent_stale_fresh_and_oneshot() {
        assert!(heartbeat_warning(None).unwrap().contains("NEVER"), "absence is loudest");
        let fresh = VerifyHeartbeat {
            ts: chrono::Utc::now(),
            tasks_checked: 1,
            events_appended: 0,
            interval_secs: Some(300),
            suspect_landed: Vec::new(),
        };
        assert_eq!(heartbeat_warning(Some(&fresh)), None);
        let stale = VerifyHeartbeat {
            ts: chrono::Utc::now() - chrono::Duration::seconds(1000),
            ..fresh.clone()
        };
        assert!(heartbeat_warning(Some(&stale)).unwrap().contains("STALE"));
        // One-shot passes record no interval: age alone can't call them stale.
        let oneshot = VerifyHeartbeat {
            ts: chrono::Utc::now() - chrono::Duration::seconds(100_000),
            interval_secs: None,
            ..fresh
        };
        assert_eq!(heartbeat_warning(Some(&oneshot)), None);
    }
}
