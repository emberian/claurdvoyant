//! `cv task` — the fleet task substrate: open/claim/propose/review/verify dispatch objects whose
//! landing state is *observed* from git, never taken on an agent's word.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use clap::Subcommand;
use cv_core::sanitize::sanitize_line;
use cv_core::task::{self, InboxRow, TaskEventKind, TaskRow, TaskStore};

use crate::util::{fmt_local, short_id};

/// Identity resolution (G4) for chat-grade verbs: explicit `--from`, else `CV_ENDPOINT`, else
/// the literal `"cv"` (a human opening a task from a shell owes no ceremony). Shared logic lives
/// in [`task::actor`]; only the CLI's default sink is chosen here.
fn from_or_cv(explicit: Option<String>) -> String {
    task::actor(explicit, "cv")
}

/// Identity resolution for identity-BEARING verbs (claim/release/propose/pass/refute):
/// [`task::require_actor`], with the error naming this surface's `--from` flag.
fn require_from(explicit: Option<String>) -> Result<String> {
    task::require_actor(explicit, "--from")
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
        /// Endpoint to compute the inbox for. Default: $CV_ENDPOINT, else "cv".
        who: Option<String>,
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
    /// Fleet batting averages: per-endpoint and per-reviewer outcome counts, computed from
    /// observed events only. Informational — never a gate.
    Stats {
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

/// Append an agent event through the shared core path, print a one-line confirmation.
fn append_and_report(task_id: Option<&str>, from: &str, kind: TaskEventKind) -> Result<()> {
    let store = TaskStore::default_store();
    let report = task::append_and_notify(&store, task_id, from, kind, Vec::new())?;
    for w in report.replay_warnings.iter().chain(&report.warnings) {
        eprintln!("⚠ {}", sanitize_line(w));
    }
    let state = report.effective_state.unwrap_or_else(|| "?".into());
    println!("✦ {} {} → {}", report.event.kind.tag(), short_id(&report.event.task_id), state);
    if matches!(report.event.kind, TaskEventKind::Opened { .. }) {
        println!("{}", report.event.task_id);
    }
    Ok(())
}

fn resolve<'m>(model: &'m cv_core::task::TaskReadModel, prefix: &str) -> Result<&'m str> {
    task::resolve_id(model, prefix).map_err(|e| anyhow::anyhow!(e))
}

/// One `cv task list` row: id, age since last event (G8), state, assignee, title — rendered from
/// the shared [`TaskRow`] shape (built via [`TaskRow::full`], whose `last_ts` anchors the age).
/// All free-text fields are terminal-sanitized (G5) — the log is forever, so every reader strips.
fn task_row(r: &TaskRow, now: DateTime<Utc>) -> String {
    let assignee = r.assignee.as_deref().unwrap_or("-");
    format!(
        "{}  {:>4}  {:16} {:20} {}",
        short_id(&r.id),
        task::age_short(r.last_ts.unwrap_or(now), now),
        r.effective_state,
        sanitize_line(assignee),
        sanitize_line(&r.title)
    )
}

/// One `cv task inbox` row, stalest first upstream: a `⏰` leads anything waiting > 24h. A
/// projection of age, not an escalation — the marker is the whole mechanism.
fn inbox_row(r: &InboxRow, now: DateTime<Utc>) -> String {
    let stale = now.signed_duration_since(r.since) > chrono::Duration::hours(24);
    format!(
        "{} {}  {:>4}  {:24} {}",
        if stale { "⏰" } else { "  " },
        short_id(&r.id),
        task::age_short(r.since, now),
        format!("{:?}", r.reason),
        sanitize_line(&r.title)
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
            let tasks = task::list(&outcome.model, &filter).map_err(|e| anyhow::anyhow!(e))?;
            if json {
                // The full projections, unchanged wire shape (`show --json` sibling).
                println!("{}", serde_json::to_string_pretty(&tasks)?);
            } else {
                let now = Utc::now();
                for t in &tasks {
                    println!("{}", task_row(&TaskRow::full(t), now));
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
                    for (verdict, receipts) in [
                        ("pass", rev.pass.as_ref().and_then(|p| p.receipts.as_ref())),
                        ("refute", rev.refute.as_ref().and_then(|r| r.receipts.as_ref())),
                    ] {
                        if let Some(r) = receipts {
                            println!("         receipts ({verdict}): {}", sanitize_line(&receipts_line(r)));
                        }
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
            // Both advisory observations from one parse of the reviewer's session (law 2:
            // read, record, warn — never gate).
            let obs = task::review_observation(&outcome.model.tasks[&id], session.as_deref());
            if let Some(w) = task::independence_warning(obs.independence.as_ref()) {
                eprintln!("⚠ {w}");
            }
            if let Some(w) = task::receipts_warning(obs.receipts.as_ref()) {
                eprintln!("⚠ {w}");
            }
            append_and_report(
                Some(&id),
                &from,
                TaskEventKind::ReviewPassed {
                    reviewer: from.clone(),
                    session_ref: session,
                    independence: obs.independence,
                    receipts: obs.receipts,
                },
            )
        }
        TaskCmd::Refute { id, session, from } => {
            let from = require_from(from)?;
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let receipts = task::review_receipts(&outcome.model.tasks[&id], session.as_deref());
            if let Some(w) = task::receipts_warning(receipts.as_ref()) {
                eprintln!("⚠ {w}");
            }
            append_and_report(
                Some(&id),
                &from,
                TaskEventKind::ReviewRefuted { reviewer: from.clone(), session_ref: session, receipts },
            )
        }
        TaskCmd::Verify { id, all, fetch, skip_landed } => cmd_verify(id, all, fetch, skip_landed),
        TaskCmd::Inbox { who, json } => {
            // Bare `cv task inbox` means "my inbox": the spawner-set CV_ENDPOINT, else "cv".
            let who = from_or_cv(who);
            let outcome = replay_loud()?;
            if json {
                // The full entries, unchanged wire shape.
                let entries = task::inbox(&outcome.model, &who);
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                let rows = InboxRow::compute(&outcome.model, &who);
                let now = Utc::now();
                for r in &rows {
                    println!("{}", inbox_row(r, now));
                }
                if rows.is_empty() {
                    println!("(inbox empty for {})", sanitize_line(&who));
                }
            }
            Ok(())
        }
        TaskCmd::Debt { repo, json } => {
            let outcome = replay_loud()?;
            // The debt view is only as honest as the verifier is alive (G1) — the heartbeat
            // rides it, and suspect lands (G3) re-surface as visible debt. Shaping (repo filter,
            // aged awaiting-review rows (G8), warning text) is the shared DebtReport.
            let hb = cv_core::task::verify::read_heartbeat(&task::tasks_dir());
            if json {
                // The full entries (task projection embedded), unchanged wire shape.
                let json_rows: Vec<_> = task::debt(&outcome.model)
                    .iter()
                    .filter(|(group_repo, _)| match &repo {
                        Some(want) => group_repo.as_deref() == Some(want.as_path()),
                        None => true,
                    })
                    .flat_map(|(_, entries)| entries.clone())
                    .collect();
                let awaiting: Vec<_> = task::awaiting_review(&outcome.model)
                    .into_iter()
                    .filter(|e| match &repo {
                        Some(want) => e.task.repo.as_deref() == Some(want.as_path()),
                        None => true,
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "debt": json_rows,
                        "awaiting_review": awaiting,
                        "suspects": hb.as_ref().map(|h| h.suspect_landed.clone()).unwrap_or_default(),
                        "verified_as_of": hb.as_ref().map(|h| h.ts),
                        "verify_warning": cv_core::task::verify::heartbeat_warning(hb.as_ref()),
                    }))?
                );
                return Ok(());
            }
            let report =
                task::DebtReport::compute(&outcome.model, hb.as_ref(), repo.as_deref());
            // Rows arrive repo-ascending (no-repo first), oldest first within a repo: render a
            // group header at each repo transition.
            let mut current: Option<&Option<std::path::PathBuf>> = None;
            let now = Utc::now();
            for row in &report.debt {
                if current != Some(&row.repo) {
                    let name = row
                        .repo
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(no repo)".into());
                    println!("{name}:");
                    current = Some(&row.repo);
                }
                let age = now.signed_duration_since(row.since);
                println!(
                    "  {}  rev{} {} [{}] unlanded for {}h — {}",
                    short_id(&row.id),
                    row.revision,
                    sanitize_line(&row.branch),
                    row.state.as_str(),
                    age.num_hours(),
                    sanitize_line(&row.title)
                );
                for issue in &row.issues {
                    println!("      ⚠ {}", sanitize_line(issue));
                }
            }
            if report.debt.is_empty() && report.suspects.is_empty() {
                println!("✓ no unlanded reviewed work");
            }
            if !report.awaiting_review.is_empty() {
                println!("awaiting review:");
                for row in &report.awaiting_review {
                    println!(
                        "  {}  rev{} {} → {} waiting {} — {}",
                        short_id(&row.id),
                        row.revision,
                        sanitize_line(&row.branch),
                        sanitize_line(row.reviewer.as_deref().unwrap_or("(reviewer unbound)")),
                        task::age_short(row.since, now),
                        sanitize_line(&row.title)
                    );
                }
            }
            for s in &report.suspects {
                println!(
                    "⚠ SUSPECT {}  rev{} {} — {}",
                    short_id(&s.task_id),
                    s.revision,
                    sanitize_line(&s.detail),
                    sanitize_line(&s.title)
                );
            }
            if let Some(hb) = &hb {
                println!(
                    "verified as of {}{}",
                    fmt_local(hb.ts, "%Y-%m-%d %H:%M:%S"),
                    hb.interval_secs
                        .map(|i| format!(" (every {i}s)"))
                        .unwrap_or_default()
                );
            }
            if let Some(w) = &report.verify_warning {
                eprintln!("⚠ {}", sanitize_line(w));
            }
            Ok(())
        }
        TaskCmd::Stats { repo, json } => {
            let outcome = replay_loud()?;
            let hb = cv_core::task::verify::read_heartbeat(&task::tasks_dir());
            let stats = task::FleetStats::compute(&outcome.model, hb.as_ref(), repo.as_deref());
            if json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                render_stats(&stats);
            }
            Ok(())
        }
    }
}

/// One receipts observation as a human line: `saw change ✓, ran checks ✗, 14 turns`
/// (`?` = undetermined — observed as unknown, never guessed).
fn receipts_line(r: &cv_core::task::ReviewReceipts) -> String {
    let mark = |o: Option<bool>| match o {
        Some(true) => "✓",
        Some(false) => "✗",
        None => "?",
    };
    let turns = r.turns.map(|t| format!("{t} turns")).unwrap_or_else(|| "? turns".into());
    format!("saw change {}, ran checks {}, {}", mark(r.saw_change), mark(r.ran_checks), turns)
}

/// A median duration cell: `-` when there is no sample (never a fake `0s`).
fn median_cell(secs: Option<i64>) -> String {
    let now = Utc::now();
    match secs {
        None => "-".into(),
        Some(s) => task::age_short(now - chrono::Duration::seconds(s.max(0)), now),
    }
}

/// `cv task stats` human rendering. Small-n honesty: rates print as the counts they came from
/// (`2/3`), never a bare percentage.
fn render_stats(stats: &cv_core::task::FleetStats) {
    if stats.endpoints.is_empty() && stats.reviewers.is_empty() {
        println!("(no task history yet)");
    }
    if !stats.endpoints.is_empty() {
        println!("endpoints (as author/assignee):");
        println!(
            "  {:24} {:>7} {:>8} {:>7} {:>7} {:>5} {:>6}  {:>9} {:>9}",
            "endpoint", "claimed", "proposed", "landed", "refuted", "live", "aband", "land-rate", "med-land"
        );
        for e in &stats.endpoints {
            let terminal = e.landed + e.refuted + e.superseded;
            let rate = if terminal == 0 { "-".into() } else { format!("{}/{terminal}", e.landed) };
            println!(
                "  {:24} {:>7} {:>8} {:>7} {:>7} {:>5} {:>6}  {:>9} {:>9}",
                sanitize_line(&e.endpoint),
                e.claimed,
                e.proposed,
                e.landed,
                e.refuted,
                e.unlanded,
                e.abandoned_live,
                rate,
                median_cell(e.median_secs_to_land),
            );
        }
    }
    if !stats.reviewers.is_empty() {
        println!("reviewers:");
        println!(
            "  {:24} {:>8} {:>11} {:>9} {:>8} {:>10}  {:>11}",
            "reviewer", "verdicts", "pass/refute", "same-fam", "no-rcpt", "no-contact", "med-latency"
        );
        for r in &stats.reviewers {
            println!(
                "  {:24} {:>8} {:>11} {:>9} {:>8} {:>10}  {:>11}",
                sanitize_line(&r.reviewer),
                r.verdicts,
                format!("{}/{}", r.passes, r.refutes),
                r.same_family_passes,
                r.no_receipts_passes,
                r.no_contact_passes,
                median_cell(r.median_review_latency_secs),
            );
        }
    }
    for f in &stats.families {
        println!(
            "family {}: {} reviews ({} cross-family, {} same-family, {} undetermined)",
            sanitize_line(&f.family),
            f.reviews_given,
            f.cross_family,
            f.same_family,
            f.undetermined
        );
    }
    match &stats.verified_as_of {
        Some(ts) => println!("verified as of {}", fmt_local(*ts, "%Y-%m-%d %H:%M:%S")),
        None => println!("verified: NEVER"),
    }
    if let Some(w) = &stats.verify_warning {
        eprintln!("⚠ {}", sanitize_line(w));
    }
    println!("computed from observed events only; landed = git-verified");
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
    use cv_core::task::{InboxEntry, InboxReason, TaskProjection, TaskState};

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
        let row = task_row(&TaskRow::full(&t), now);
        assert!(row.contains("  3d  "), "age column from last_ts: {row}");
        assert!(row.contains("eviltitle!"), "payload stripped: {row}");
        assert!(!row.contains('\u{1b}'), "no ESC survives: {row}");
    }

    #[test]
    fn inbox_row_marks_older_than_24h_and_strips_ansi() {
        let t = proj("t\u{1b}[31mred", "2026-07-13T00:00:00Z");
        let now: DateTime<Utc> = "2026-07-16T00:00:00Z".parse().unwrap();
        let stale = InboxRow::from_entry(&InboxEntry {
            task: &t,
            reason: InboxReason::ClaimedByYou,
            since: "2026-07-13T00:00:00Z".parse().unwrap(),
        });
        let row = inbox_row(&stale, now);
        assert!(row.starts_with("⏰"), "24h+ rows lead with the marker: {row}");
        assert!(row.contains("tred") && !row.contains('\u{1b}'), "{row}");

        let fresh = InboxRow::from_entry(&InboxEntry {
            task: &t,
            reason: InboxReason::ClaimedByYou,
            since: "2026-07-15T12:00:00Z".parse().unwrap(),
        });
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
