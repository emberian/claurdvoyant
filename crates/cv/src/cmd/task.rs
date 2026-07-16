//! `cv task` — the fleet task substrate: open/claim/propose/review/verify dispatch objects whose
//! landing state is *observed* from git, never taken on an agent's word.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use cv_core::task::{self, TaskEventKind, TaskProjection, TaskStore};

use crate::util::{fmt_local, short_id};

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
        #[arg(long, default_value = "cv")]
        from: String,
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
        #[arg(long, default_value = "cv")]
        from: String,
    },
    /// Release a claim back to open.
    Release {
        id: String,
        #[arg(long, default_value = "cv")]
        from: String,
    },
    /// Record a progress note.
    Note {
        id: String,
        text: String,
        #[arg(long = "session-ref")]
        session_ref: Option<String>,
        #[arg(long, default_value = "cv")]
        from: String,
    },
    /// Complete a non-code task (refused while a code revision is live — land or kill it first).
    Done {
        id: String,
        /// Pointer to observable evidence (URL, path, session id).
        #[arg(long)]
        observed: Option<String>,
        #[arg(long, default_value = "cv")]
        from: String,
    },
    /// Kill a task.
    Abandon {
        id: String,
        #[arg(long, default_value = "no reason given")]
        reason: String,
        #[arg(long, default_value = "cv")]
        from: String,
    },
    /// Mark a task superseded by another.
    Supersede {
        id: String,
        #[arg(long = "by")]
        by_task: String,
        #[arg(long, default_value = "cv")]
        from: String,
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
        #[arg(long, default_value = "cv")]
        from: String,
    },
    /// Reroute the active review to another reviewer.
    Reroute {
        id: String,
        #[arg(long)]
        to: String,
        #[arg(long, default_value = "cv")]
        from: String,
    },
    /// Record a review PASS (advisory independence check runs if --session is given).
    Pass {
        id: String,
        /// The reviewer's cv session id — used to read the reviewer's harness family.
        #[arg(long)]
        session: Option<String>,
        #[arg(long, default_value = "cv")]
        from: String,
    },
    /// Record a review REFUTE (terminal for the revision; propose a new revision to continue).
    Refute {
        id: String,
        #[arg(long)]
        session: Option<String>,
        #[arg(long, default_value = "cv")]
        from: String,
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
        eprintln!("⚠ {w}");
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

fn print_task_line(t: &TaskProjection) {
    let assignee = t.assignee.as_deref().unwrap_or("-");
    println!(
        "{}  {:16} {:20} {}",
        short_id(&t.task_id),
        task::effective_display(t),
        assignee,
        t.title
    );
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
                &from,
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
                for t in &tasks {
                    print_task_line(t);
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
                println!("task {}  [{}]", t.task_id, task::effective_display(t));
                println!("  title:    {}", t.title);
                if !t.body.is_empty() {
                    println!("  body:     {}", t.body);
                }
                if let Some(repo) = &t.repo {
                    println!("  repo:     {}", repo.display());
                }
                if let Some(issue) = &t.issue {
                    println!("  issue:    {}", issue);
                }
                println!("  channel:  #{}", t.channel);
                println!("  assignee: {}", t.assignee.as_deref().unwrap_or("-"));
                println!(
                    "  opened:   {} by {}",
                    fmt_local(t.opened_at, "%Y-%m-%d %H:%M"),
                    t.opened_by
                );
                for rev in &t.revisions {
                    println!(
                        "  rev {}: {} [{}] {} → {}",
                        rev.revision.n,
                        rev.revision.branch,
                        rev.state.as_str(),
                        &rev.revision.review_sha[..12],
                        rev.revision.upstream,
                    );
                    if let Some(reviewer) = &rev.active_reviewer {
                        println!("         reviewer: {reviewer}");
                    }
                    if let Some((head, pid)) = &rev.landed {
                        println!("         landed: upstream {} (patch-id {})", &head[..12], &pid[..12]);
                    }
                    for issue in &rev.issues {
                        println!("         ⚠ {}", issue.describe());
                    }
                }
                for note in &t.notes {
                    println!(
                        "  note ({}, {}): {}",
                        note.by,
                        fmt_local(note.ts, "%Y-%m-%d %H:%M"),
                        note.text
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
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            append_and_report(Some(&id), &from, TaskEventKind::Claimed { assignee: from.clone() })
        }
        TaskCmd::Release { id, from } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            append_and_report(Some(&id), &from, TaskEventKind::Released {})
        }
        TaskCmd::Note { id, text, session_ref, from } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            append_and_report(Some(&id), &from, TaskEventKind::Noted { text, session_ref })
        }
        TaskCmd::Done { id, observed, from } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            append_and_report(Some(&id), &from, TaskEventKind::Done { observed })
        }
        TaskCmd::Abandon { id, reason, from } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            append_and_report(Some(&id), &from, TaskEventKind::Abandoned { reason })
        }
        TaskCmd::Supersede { id, by_task, from } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let by_task = resolve(&outcome.model, &by_task)?.to_string();
            append_and_report(Some(&id), &from, TaskEventKind::Superseded { by_task })
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
                branch,
                &revision.review_sha[..12],
                &revision.patch_id[..12]
            );
            append_and_report(Some(&id), &from, TaskEventKind::RevisionProposed { revision })
        }
        TaskCmd::Reroute { id, to, from } => {
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            let current = outcome.model.tasks[&id]
                .current_revision()
                .and_then(|r| r.active_reviewer.clone())
                .context("no active reviewer to reroute from")?;
            append_and_report(Some(&id), &from, TaskEventKind::ReviewRerouted { from: current, to })
        }
        TaskCmd::Pass { id, session, from } => {
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
            let outcome = replay_loud()?;
            let id = resolve(&outcome.model, &id)?.to_string();
            append_and_report(
                Some(&id),
                &from,
                TaskEventKind::ReviewRefuted { reviewer: from.clone(), session_ref: session },
            )
        }
        TaskCmd::Verify { id, all, fetch } => cmd_verify(id, all, fetch),
        TaskCmd::Inbox { who, json } => {
            let outcome = replay_loud()?;
            let entries = task::inbox(&outcome.model, &who);
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                for e in &entries {
                    println!(
                        "{}  {:24} {}",
                        short_id(&e.task.task_id),
                        format!("{:?}", e.reason),
                        e.task.title
                    );
                }
                if entries.is_empty() {
                    println!("(inbox empty for {who})");
                }
            }
            Ok(())
        }
        TaskCmd::Debt { repo, json } => {
            let outcome = replay_loud()?;
            let groups = task::debt(&outcome.model);
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
                        e.branch,
                        e.state.as_str(),
                        age.num_hours(),
                        e.task.title
                    );
                    for issue in &e.issues {
                        println!("      ⚠ {issue}");
                    }
                    shown += 1;
                }
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&json_rows)?);
            } else if shown == 0 {
                println!("✓ no unlanded reviewed work");
            }
            Ok(())
        }
    }
}

/// `cv task verify` — the observation pass, shared with MCP/cvd via `verify::run_verify`.
fn cmd_verify(id: Option<String>, all: bool, fetch: bool) -> Result<()> {
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
    let (appended, warnings) = cv_core::task::verify::run_verify(&store, ids.as_deref(), fetch)?;
    for w in &warnings {
        eprintln!("⚠ {w}");
    }
    for ev in &appended {
        println!("✦ observed {} on {}", ev.kind.tag(), short_id(&ev.task_id));
    }
    if appended.is_empty() {
        println!("(nothing new observed)");
    }
    Ok(())
}
