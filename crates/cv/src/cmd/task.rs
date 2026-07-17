//! `cv task` — the fleet task substrate: open/claim/propose/review/verify dispatch objects whose
//! landing state is *observed* from git, never taken on an agent's word.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use clap::Subcommand;
use cv_core::sanitize::sanitize_line;
use cv_core::task::{self, InboxEntry, TaskEventKind, TaskProjection, TaskStore};

use crate::util::{fmt_local, short_id};

/// Identity resolution (G4): explicit `--from` wins, else the spawner-set `CV_ENDPOINT`, else
/// the literal `"cv"` — good enough for verbs where the actor is bookkeeping, not semantics
/// (open/note/done/abandon/supersede: a human opening a task from a shell owes no ceremony).
fn from_or_cv(explicit: Option<String>) -> String {
    explicit
        .or_else(task::default_endpoint)
        .unwrap_or_else(|| "cv".into())
}

/// Identity resolution for identity-BEARING verbs (claim/release/propose/pass/refute), whose
/// endpoint string keys inbox and reviewer semantics: no resolvable identity is an error, never
/// a silent fall-through to a shared sink.
fn require_from(explicit: Option<String>) -> Result<String> {
    explicit.or_else(task::default_endpoint).ok_or_else(|| {
        anyhow::anyhow!(
            "set CV_ENDPOINT or pass --from; identity-bearing events must record who acted"
        )
    })
}

#[derive(Subcommand)]
pub(crate) enum TaskCmd {
    /// Open a new task. Prints its id.
    Open {
        title: String,
        #[arg(long, default_value = "")]
        body: String,
        /// Repository this task's code work happens in (enables propose/verify/debt).
        #[arg(long)]
        repo: Option<PathBuf>,
        /// External issue/work handle, free-form.
        #[arg(long)]
        issue: Option<String>,
        /// Board channel task notifications post to.
        #[arg(long, default_value = "tasks")]
        channel: String,
        #[arg(long)]
        assignee: Option<String>,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// List tasks (non-terminal by default).
    List {
        /// Filter by effective state (open|claimed|awaiting_review|ready|merged_local|landed|done|...).
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Include terminal tasks.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show one task (id may be a unique prefix).
    Show {
        id: String,
        #[arg(long)]
        json: bool,
        /// Also print the raw event history.
        #[arg(long)]
        events: bool,
    },
    /// Claim an open task (first writer wins).
    Claim {
        id: String,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Release a claim back to open.
    Release {
        id: String,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Record a progress note.
    Note {
        id: String,
        text: String,
        #[arg(long = "session-ref")]
        session_ref: Option<String>,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Complete a non-code task (refused while a code revision is live — land or kill it first).
    Done {
        id: String,
        /// Pointer to observable evidence (URL, path, session id).
        #[arg(long)]
        observed: Option<String>,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Kill a task.
    Abandon {
        id: String,
        #[arg(long, default_value = "no reason given")]
        reason: String,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Mark a task superseded by another.
    Supersede {
        id: String,
        #[arg(long = "by")]
        by_task: String,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Propose a reviewed revision: cv resolves the branch tip and computes the range patch-id
    /// from git itself (identity is observed, never typed).
    Propose {
        id: String,
        #[arg(long)]
        branch: String,
        /// Verify this sha is the branch tip (refused otherwise). Default: use the tip.
        #[arg(long)]
        sha: Option<String>,
        #[arg(long, default_value = "origin/main")]
        upstream: String,
        #[arg(long)]
        worktree: Option<PathBuf>,
        /// Reviewer endpoint bound to this revision (else the first verdict binds).
        #[arg(long)]
        reviewer: Option<String>,
        /// Your cv session id (author side of the independence check).
        #[arg(long = "session-ref")]
        session_ref: Option<String>,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Reroute the active review to another reviewer.
    Reroute {
        id: String,
        #[arg(long)]
        to: String,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Record a review PASS (advisory independence check runs if --session is given).
    Pass {
        id: String,
        /// The reviewer's cv session id — used to read the reviewer's harness family.
        #[arg(long)]
        session: Option<String>,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Record a review REFUTE (terminal for the revision; propose a new revision to continue).
    Refute {
        id: String,
        #[arg(long)]
        session: Option<String>,
        /// Acting endpoint recorded in `by`. Default: $CV_ENDPOINT.
        #[arg(long)]
        from: Option<String>,
    },
    /// Run the git verifier over Ready/MergedLocal revisions and record what it observes.
    Verify {
        /// Task id (prefix ok). Omit with --all to verify everything verifiable.
        id: Option<String>,
        #[arg(long)]
        all: bool,
        /// git fetch the upstream's remote first.
        #[arg(long)]
        fetch: bool,
        /// Skip re-observation of Landed revisions (opt-out for huge histories; previously
        /// recorded suspects are preserved, not cleared).
        #[arg(long = "skip-landed")]
        skip_landed: bool,
    },
    /// What needs `<who>`: assigned/claimed tasks, reviews awaiting them, their unlanded work.
    Inbox {
        #[arg(default_value = "cv")]
        who: String,
        #[arg(long)]
        json: bool,
    },
    /// Reviewed-but-unlanded work, grouped by repo, oldest first. The honest debt view.
    Debt {
        #[arg(long)]
        repo: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

/// Replay the default store, print any log warnings loudly, return the outcome.
fn replay_loud() -> Result<cv_core::task::ReplayOutcome> {
    let outcome = task::replay()?;
    for w in &outcome.warnings {
        eprintln!("⚠ {}", sanitize_line(w));
    }
    Ok(outcome)
}

/// Append an agent event, notify the board, print a one-line confirmation.
fn append_and_report(task_id: Option<&str>, from: &str, kind: TaskEventKind) -> Result<()> {
    let store = TaskStore::default_store();
    let event = store.append_agent_event(task::new_event(task_id, from, kind))?;
    let outcome = replay_loud()?;
    let proj = outcome.model.tasks.get(&event.task_id);
    let channel = proj.map(|t| t.channel.clone()).unwrap_or_else(|| "tasks".into());
    if let Err(e) = task::notify_board(&event, &channel) {
        eprintln!("⚠ board notification failed (task state is durable): {e}");
    }
    let state = proj.map(task::effective_display).unwrap_or_else(|| "?".into());
    println!("✦ {} {} → {}", event.kind.tag(), short_id(&event.task_id), state);
    if matches!(event.kind, TaskEventKind::Opened { .. }) {
        println!("{}", event.task_id);
    }
    Ok(())
}

fn resolve<'m>(model: &'m cv_core::task::TaskReadModel, prefix: &str) -> Result<&'m str> {
    task::resolve_id(model, prefix).map_err(|e| anyhow::anyhow!(e))
}

/// One `cv task list` row: id, age since last event (G8), state, assignee, title. All free-text
/// fields are terminal-sanitized (G5) — the log is forever, so every reader strips.
fn task_row(t: &TaskProjection, now: DateTime<Utc>) -> String {
    let assignee = t.assignee.as_deref().unwrap_or("-");
    format!(
        "{}  {:>4}  {:16} {:20} {}",
        short_id(&t.task_id),
        task::age_short(t.last_ts, now),
        task::effective_display(t),
        sanitize_line(assignee),
        sanitize_line(&t.title)
    )
}

/// One `cv task inbox` row, stalest first upstream: a `⏰` leads anything waiting > 24h. A
/// projection of age, not an escalation — the marker is the whole mechanism.
fn inbox_row(e: &InboxEntry<'_>, now: DateTime<Utc>) -> String {
    let stale = now.signed_duration_since(e.since) > chrono::Duration::hours(24);
    format!(
        "{} {}  {:>4}  {:24} {}",
        if stale { "⏰" } else { "  " },
        short_id(&e.task.task_id),
        task::age_short(e.since, now),
        format!("{:?}", e.reason),
        sanitize_line(&e.task.title)
    )
}

pub(crate) fn cmd_task(action: TaskCmd) -> Result<()> {
    match action {
        TaskCmd::Open {
            title,
            body,
            repo,
            issue,
            channel,
            assignee,
            from,
        } => {
            let repo = match repo {
                Some(r) => Some(
                    r.canonicalize()
                        .with_context(|| format!("repo {} not found", r.display()))?,
                ),
                None => None,
            };
            append_and_report(
                None,
                &from_or_cv(from),
                TaskEventKind::Opened { title, body, repo, issue, channel, assignee },
            )
        }
        TaskCmd::List { state, assignee, repo, all, json } => {
            let outcome = replay_loud()?;
            let filter = task::TaskFilter {
                state,
                assignee,
                repo,
                include_terminal: all,
            };
            let tasks = task::list(&outcome.model, &filter);
            if json {
                println!("{}", serde_json::to_string_pretty(&tasks)?);
            } else {
                let now = Utc::now();
                for t in &tasks {
                    println!("{}", task_row(t, now));
                }
                if tasks.is_empty() {
                    println!("(no matching tasks)");
                }
            }
            Ok(())
        }
        TaskCmd::Show { id, json, events } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let t = &outcome.model.tasks[&id];
            if json {
                println!("{}", serde_json::to_string_pretty(t)?);
            } else {
                // Every free-text field below came from the durable log — sanitize at render
                // (G5): titles, notes, endpoints, and branch names are all untrusted.
                println!("task {}  [{}]", t.task_id, task::effective_display(t));
                println!("  title:    {}", sanitize_line(&t.title));
                if !t.body.is_empty() {
                    println!("  body:     {}", sanitize_line(&t.body));
                }
                if let Some(repo) = &t.repo {
                    println!("  repo:     {}", repo.display());
                }
                if let Some(issue) = &t.issue {
                    println!("  issue:    {}", sanitize_line(issue));
                }
                println!("  channel:  #{}", sanitize_line(&t.channel));
                println!("  assignee: {}", sanitize_line(t.assignee.as_deref().unwrap_or("-")));
                println!(
                    "  opened:   {} by {}",
                    fmt_local(t.opened_at, "%Y-%m-%d %H:%M"),
                    sanitize_line(&t.opened_by)
                );
                for rev in &t.revisions {
                    println!(
                        "  rev {}: {} [{}] {} → {}",
                        rev.revision.n,
                        sanitize_line(&rev.revision.branch),
                        rev.state.as_str(),
                        &rev.revision.review_sha[..12],
                        sanitize_line(&rev.revision.upstream),
                    );
                    if let Some(reviewer) = &rev.active_reviewer {
                        println!("         reviewer: {}", sanitize_line(reviewer));
                    }
                    if let Some((head, pid)) = &rev.landed {
                        println!("         landed: upstream {} (patch-id {})", &head[..12], &pid[..12]);
                    }
                    for issue in &rev.issues {
                        println!("         ⚠ {}", sanitize_line(&issue.describe()));
                    }
                }
                for note in &t.notes {
                    println!(
                        "  note ({}, {}): {}",
                        sanitize_line(&note.by),
                        fmt_local(note.ts, "%Y-%m-%d %H:%M"),
                        sanitize_line(&note.text)
                    );
                }
            }
            if events {
                let history: Vec<_> = outcome
                    .events
                    .iter()
                    .filter(|e| e.task_id == id)
                    .collect();
                println!("{}", serde_json::to_string_pretty(&history)?);
            }
            Ok(())
        }
        TaskCmd::Claim { id, from } => {
            let from = require_from(from)?;
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            append_and_report(Some(&id), &from, TaskEventKind::Claimed { assignee: from.clone() })
        }
        TaskCmd::Release { id, from } => {
            let from = require_from(from)?;
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            append_and_report(Some(&id), &from, TaskEventKind::Released {})
        }
        TaskCmd::Note { id, text, session_ref, from } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            append_and_report(Some(&id), &from_or_cv(from), TaskEventKind::Noted { text, session_ref })
        }
        TaskCmd::Done { id, observed, from } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            append_and_report(Some(&id), &from_or_cv(from), TaskEventKind::Done { observed })
        }
        TaskCmd::Abandon { id, reason, from } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            append_and_report(Some(&id), &from_or_cv(from), TaskEventKind::Abandoned { reason })
        }
        TaskCmd::Supersede { id, by_task, from } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let by_task = resolve(&outcome.model, &by_task)?.to_string();
            append_and_report(Some(&id), &from_or_cv(from), TaskEventKind::Superseded { by_task })
        }
        TaskCmd::Propose {
            id,
            branch,
            sha,
            upstream,
            worktree,
            reviewer,
            session_ref,
            from,
        } => {
            let from = require_from(from)?;
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let t = &outcome.model.tasks[&id];
            let repo = t
                .repo
                .clone()
                .context("task has no --repo recorded; open it with one to propose revisions")?;
            let n = t.revisions.len() as u32 + 1;
            let revision = cv_core::task::verify::observe_revision(
                &repo,
                &branch,
                &upstream,
                sha.as_deref(),
                n,
                worktree,
                reviewer,
                session_ref,
            )?;
            println!(
                "observed: {} tip {} range-patch-id {}",
                sanitize_line(&branch),
                &revision.review_sha[..12],
                &revision.patch_id[..12]
            );
            // Advisory collision scan (never a block): another live task already carrying this
            // branch/worktree in the same repo is usually two agents about to trample each other.
            for w in task::propose_collision_warnings(&outcome.model, &id, &repo, &revision) {
                eprintln!("⚠ WARNING: {}", sanitize_line(&w));
            }
            append_and_report(Some(&id), &from, TaskEventKind::RevisionProposed { revision })
        }
        TaskCmd::Reroute { id, to, from } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let current = outcome.model.tasks[&id]
                .current_revision()
                .and_then(|r| r.active_reviewer.clone())
                .context("no active reviewer to reroute from")?;
            append_and_report(
                Some(&id),
                &from_or_cv(from),
                TaskEventKind::ReviewRerouted { from: current, to },
            )
        }
        TaskCmd::Pass { id, session, from } => {
            let from = require_from(from)?;
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let independence =
                task::independence_check(&outcome.model.tasks[&id], session.as_deref());
            if let Some(w) = task::independence_warning(independence.as_ref()) {
                eprintln!("⚠ {w}");
            }
            append_and_report(
                Some(&id),
                &from,
                TaskEventKind::ReviewPassed { reviewer: from.clone(), session_ref: session, independence },
            )
        }
        TaskCmd::Refute { id, session, from } => {
            let from = require_from(from)?;
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            append_and_report(
                Some(&id),
                &from,
                TaskEventKind::ReviewRefuted { reviewer: from.clone(), session_ref: session },
            )
        }
        TaskCmd::Verify { id, all, fetch, skip_landed } => cmd_verify(id, all, fetch, skip_landed),
        TaskCmd::Inbox { who, json } => {
            let outcome = replay_loud()?;
            let entries = task::inbox(&outcome.model, &who);
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                let now = Utc::now();
                for e in &entries {
                    println!("{}", inbox_row(e, now));
                }
                if entries.is_empty() {
                    println!("(inbox empty for {})", sanitize_line(&who));
                }
            }
            Ok(())
        }
        TaskCmd::Debt { repo, json } => {
            let outcome = replay_loud()?;
            let groups = task::debt(&outcome.model);
            // Aged awaiting-review revisions ride the debt view (G8): a dead reviewer is honest
            // state that nobody sees unless it ages on the owner's primary surface.
            let awaiting: Vec<_> = task::awaiting_review(&outcome.model)
                .into_iter()
                .filter(|e| match &repo {
                    Some(want) => e.task.repo.as_deref() == Some(want.as_path()),
                    None => true,
                })
                .collect();
            // The debt view is only as honest as the verifier is alive (G1) — render the
            // heartbeat with it, and re-surface suspect lands (G3) as visible debt.
            let hb = cv_core::task::verify::read_heartbeat(&task::tasks_dir());
            let verify_warning = cv_core::task::verify::heartbeat_warning(hb.as_ref());
            let suspects = hb
                .as_ref()
                .map(|h| h.suspect_landed.clone())
                .unwrap_or_default();
            let mut shown = 0usize;
            let mut json_rows = Vec::new();
            for (group_repo, entries) in &groups {
                if let Some(want) = &repo {
                    if group_repo.as_deref() != Some(want.as_path()) {
                        continue;
                    }
                }
                if json {
                    json_rows.extend(entries.iter());
                    continue;
                }
                let name = group_repo
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(no repo)".into());
                println!("{name}:");
                for e in entries {
                    let age = chrono::Utc::now().signed_duration_since(e.since);
                    println!(
                        "  {}  rev{} {} [{}] unlanded for {}h — {}",
                        short_id(&e.task.task_id),
                        e.revision_n,
                        sanitize_line(&e.branch),
                        e.state.as_str(),
                        age.num_hours(),
                        sanitize_line(&e.task.title)
                    );
                    for issue in &e.issues {
                        println!("      ⚠ {}", sanitize_line(issue));
                    }
                    shown += 1;
                }
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "debt": json_rows,
                        "awaiting_review": awaiting,
                        "suspects": suspects,
                        "verified_as_of": hb.as_ref().map(|h| h.ts),
                        "verify_warning": verify_warning,
                    }))?
                );
                return Ok(());
            }
            if shown == 0 && suspects.is_empty() {
                println!("✓ no unlanded reviewed work");
            }
            if !awaiting.is_empty() {
                let now = Utc::now();
                println!("awaiting review:");
                for e in &awaiting {
                    println!(
                        "  {}  rev{} {} → {} waiting {} — {}",
                        short_id(&e.task.task_id),
                        e.revision_n,
                        sanitize_line(&e.branch),
                        sanitize_line(e.reviewer.as_deref().unwrap_or("(reviewer unbound)")),
                        task::age_short(e.since, now),
                        sanitize_line(&e.task.title)
                    );
                }
            }
            for s in &suspects {
                println!(
                    "⚠ SUSPECT {}  rev{} {} — {}",
                    short_id(&s.task_id),
                    s.revision,
                    sanitize_line(&s.detail),
                    sanitize_line(&s.title)
                );
            }
            match &hb {
                Some(hb) => println!(
                    "verified as of {}{}",
                    fmt_local(hb.ts, "%Y-%m-%d %H:%M:%S"),
                    hb.interval_secs
                        .map(|i| format!(" (every {i}s)"))
                        .unwrap_or_default()
                ),
                None => {}
            }
            if let Some(w) = verify_warning {
                eprintln!("⚠ {}", sanitize_line(&w));
            }
            Ok(())
        }
    }
}

/// `cv task verify` — the observation pass, shared with MCP/cvd via `verify::run_verify`.
fn cmd_verify(id: Option<String>, all: bool, fetch: bool, skip_landed: bool) -> Result<()> {
    if id.is_none() && !all {
        bail!("pass a task id or --all");
    }
    let store = TaskStore::default_store();
    let ids: Option<Vec<String>> = match &id {
        Some(prefix) => {
            let outcome = replay_loud()?;
            Some(vec![resolve(&outcome.model, prefix)?.to_string()])
        }
        None => None,
    };
    let opts = cv_core::task::verify::VerifyOptions { fetch, skip_landed, ..Default::default() };
    let (appended, warnings) = cv_core::task::verify::run_verify(&store, ids.as_deref(), &opts)?;
    for w in &warnings {
        eprintln!("⚠ {}", sanitize_line(w));
    }
    for ev in &appended {
        println!("✦ observed {} on {}", ev.kind.tag(), short_id(&ev.task_id));
    }
    if appended.is_empty() {
        println!("(nothing new observed)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cv_core::task::{InboxReason, TaskState};

    fn proj(title: &str, last_ts: &str) -> TaskProjection {
        TaskProjection {
            task_id: "00000000-0000-7000-8000-000000000001".into(),
            title: title.into(),
            body: String::new(),
            repo: None,
            issue: None,
            channel: "tasks".into(),
            state: TaskState::Open,
            assignee: Some("agent:a".into()),
            opened_by: "h".into(),
            opened_at: "2026-07-10T00:00:00Z".parse().unwrap(),
            last_event_id: "e".into(),
            last_ts: last_ts.parse().unwrap(),
            revisions: Vec::new(),
            notes: Vec::new(),
            superseded_by: None,
            abandoned_reason: None,
            done_observed: None,
        }
    }

    #[test]
    fn task_row_shows_age_and_strips_ansi() {
        let t = proj("evil\u{1b}]0;pwn\u{7}title\u{1b}[31m!", "2026-07-13T00:00:00Z");
        let now: DateTime<Utc> = "2026-07-16T00:00:00Z".parse().unwrap();
        let row = task_row(&t, now);
        assert!(row.contains("  3d  "), "age column from last_ts: {row}");
        assert!(row.contains("eviltitle!"), "payload stripped: {row}");
        assert!(!row.contains('\u{1b}'), "no ESC survives: {row}");
    }

    #[test]
    fn inbox_row_marks_older_than_24h_and_strips_ansi() {
        let t = proj("t\u{1b}[31mred", "2026-07-13T00:00:00Z");
        let now: DateTime<Utc> = "2026-07-16T00:00:00Z".parse().unwrap();
        let stale = InboxEntry {
            task: &t,
            reason: InboxReason::ClaimedByYou,
            since: "2026-07-13T00:00:00Z".parse().unwrap(),
        };
        let row = inbox_row(&stale, now);
        assert!(row.starts_with("⏰"), "24h+ rows lead with the marker: {row}");
        assert!(row.contains("tred") && !row.contains('\u{1b}'), "{row}");

        let fresh = InboxEntry {
            task: &t,
            reason: InboxReason::ClaimedByYou,
            since: "2026-07-15T12:00:00Z".parse().unwrap(),
        };
        let row = inbox_row(&fresh, now);
        assert!(!row.contains('⏰'), "young rows carry no marker: {row}");
        assert!(row.contains("12h"), "{row}");
    }

    #[test]
    fn explicit_from_beats_everything_and_missing_identity_names_the_cure() {
        assert_eq!(require_from(Some("agent:me".into())).unwrap(), "agent:me");
        assert_eq!(from_or_cv(Some("agent:me".into())), "agent:me");
        // The unset-env cases are exercised end-to-end in tests/cli.rs (spawned process with a
        // controlled environment — no process-global set_var races here).
    }
}
