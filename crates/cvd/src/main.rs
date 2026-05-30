//! cvd — the claurdvoyant daemon.
//!
//! Watches every harness's on-disk storage and archives agent sessions into a central store, so a
//! user can centralize cloud-fleet / multi-machine agent logs for search and safekeeping. Built on
//! cv-core's live-watch engine.

mod archive;
mod serve;

use anyhow::Result;
use archive::{pretty_cwd, ref_cwd, Archive, StoreOutcome};
use clap::{Parser, Subcommand};
use cv_core::watch::{EventKind, Filter, Watcher};
use cv_core::{Harness, SessionRef};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "cvd",
    about = "claurdvoyant daemon — archive agent sessions into a central store",
    version
)]
struct Cli {
    /// Archive home (default: $CLAURDVOYANT_HOME or ~/.claurdvoyant).
    #[arg(long, global = true, value_name = "DIR")]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// One-shot: discover and archive all sessions, then exit.
    Sync,
    /// Continuously follow live activity and archive sessions as they change.
    Watch {
        /// Poll interval in seconds.
        #[arg(long, default_value_t = 3)]
        interval: u64,
        /// Only this harness (e.g. claude, codex, gemini).
        #[arg(long)]
        harness: Option<String>,
        /// Only sessions whose cwd contains this substring.
        #[arg(long)]
        cwd: Option<String>,
    },
    /// Serve fleet state as JSON over HTTP for a live browser dashboard.
    Serve {
        /// Port to listen on.
        #[arg(long, default_value_t = 7777)]
        port: u16,
        /// Host/interface to bind.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
    },
    /// List what's in the archive.
    Ls,
    /// Print the archive location.
    Path,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let archive = Archive::resolve(cli.home.clone())?;

    match cli.command {
        Command::Sync => cmd_sync(&archive),
        Command::Watch {
            interval,
            harness,
            cwd,
        } => cmd_watch(&archive, interval, harness, cwd),
        Command::Serve { port, host } => serve::run(&host, port),
        Command::Ls => cmd_ls(&archive),
        Command::Path => {
            println!("{}", archive.home().display());
            Ok(())
        }
    }
}

/// Parse a harness name into a [`Harness`], erroring with the valid set.
fn parse_harness(name: &str) -> Result<Harness> {
    Harness::parse(name).ok_or_else(|| {
        let valid: Vec<&str> = Harness::ALL.iter().map(|h| h.as_str()).collect();
        anyhow::anyhow!("unknown harness {name:?}; valid: {}", valid.join(", "))
    })
}

fn cmd_sync(archive: &Archive) -> Result<()> {
    let refs = cv_core::discover_all();
    let total = refs.len();
    let mut archived = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for r in refs {
        let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
            continue;
        };
        match adapter.parse(&r) {
            Ok(session) => match archive.store(&session) {
                Ok(StoreOutcome::Archived) => archived += 1,
                Ok(StoreOutcome::Skipped) => skipped += 1,
                Err(e) => {
                    failed += 1;
                    eprintln!("cvd: failed to archive {} {}: {e:#}", r.harness, r.id);
                }
            },
            Err(e) => {
                failed += 1;
                eprintln!("cvd: failed to parse {} {}: {e:#}", r.harness, r.id);
            }
        }
    }

    if failed > 0 {
        eprintln!("cvd: {failed} session(s) failed (logged above)");
    }
    println!(
        "sync complete: {archived} archived, {skipped} skipped (unchanged) of {total} discovered"
    );
    println!("archive: {}", archive.home().display());
    Ok(())
}

fn cmd_watch(
    archive: &Archive,
    interval: u64,
    harness: Option<String>,
    cwd: Option<String>,
) -> Result<()> {
    let filter = Filter {
        harness: match harness {
            Some(h) => Some(parse_harness(&h)?),
            None => None,
        },
        cwd_contains: cwd,
    };

    // emit_existing = true: archive whatever already exists on first poll, then follow live
    // activity. (`sync` is the dedicated one-shot, but watch should also be self-sufficient.)
    let mut watcher = Watcher::new(filter, true);
    let interval = Duration::from_secs(interval.max(1));

    eprintln!(
        "cvd: watching (interval {}s) -> {}",
        interval.as_secs(),
        archive.home().display()
    );

    // We don't use Watcher::run because it returns `!` (never returns) and we want to keep
    // ownership of `archive` for the closure; a manual loop reads identically and stays robust.
    loop {
        for ev in watcher.poll() {
            archive_event(archive, &ev.reference, ev.kind, ev.new_messages.len());
        }
        std::thread::sleep(interval);
    }
}

/// Re-parse and archive the session behind an event, logging the outcome to stderr.
fn archive_event(archive: &Archive, r: &SessionRef, kind: EventKind, delta: usize) {
    let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
        return;
    };
    let session = match adapter.parse(r) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[parse-fail] {} {}: {e:#}", r.harness, short(&r.id));
            return;
        }
    };
    match archive.store(&session) {
        Ok(StoreOutcome::Archived) => {
            let verb = match kind {
                EventKind::New => "new",
                EventKind::Updated => "archived",
            };
            eprintln!(
                "[{verb}] {} {}  +{delta} msgs  {}",
                r.harness,
                short(&r.id),
                ref_cwd(r)
            );
            // Mirror the event onto the coordination board's #fleet channel: a live, centralized
            // activity feed across every machine pointed at this archive.
            let _ = cv_core::board::post(
                "fleet",
                &format!("{}:{}", r.harness, short(&r.id)),
                &format!("{verb} +{delta} msg  {}", ref_cwd(r)),
                Some("event"),
                vec![r.harness.as_str().to_string()],
                Some(r.id.clone()),
            );
        }
        Ok(StoreOutcome::Skipped) => {}
        Err(e) => eprintln!("[store-fail] {} {}: {e:#}", r.harness, short(&r.id)),
    }
}

fn cmd_ls(archive: &Archive) -> Result<()> {
    let mut entries = archive.load_catalog()?;
    if entries.is_empty() {
        println!("(archive empty — run `cvd sync`)");
        println!("archive: {}", archive.home().display());
        return Ok(());
    }
    // Newest activity first.
    entries.sort_by(|a, b| {
        b.updated_at
            .unwrap_or(b.archived_at)
            .cmp(&a.updated_at.unwrap_or(a.archived_at))
    });

    for e in &entries {
        let title = e.title.clone().unwrap_or_else(|| "(untitled)".to_string());
        let archived = e.archived_at.format("%Y-%m-%d %H:%M");
        println!(
            "{:<9} {:<10} {:>4} msgs  {}  [{}]  {}",
            e.harness,
            short(&e.id),
            e.message_count,
            archived,
            pretty_cwd(&e.cwd),
            truncate(&title, 60),
        );
    }
    println!("\n{} session(s) in {}", entries.len(), archive.home().display());
    Ok(())
}

/// Short id for terminal display (first 8 chars).
fn short(id: &str) -> String {
    if id.len() <= 8 {
        id.to_string()
    } else {
        id.chars().take(8).collect()
    }
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max {
        s
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
