//! cvd — the clustervision daemon.
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

/// Version with the embedded build commit (set by build.rs; "unknown" outside git) — a
/// long-running daemon must be checkable against source ("is the running verifier the new one").
const BUILD_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("CV_BUILD_SHA"), ")");

#[derive(Parser)]
#[command(
    name = "cvd",
    about = "clustervision daemon — archive agent sessions into a central store",
    version = BUILD_VERSION
)]
struct Cli {
    /// Archive home (default: $CLUSTERVISION_HOME or ~/.clustervision).
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
        /// Also run the task git-verifier every N seconds (0 disables). On by default: the
        /// daemon is the engine that keeps landing state and the debt view honest without
        /// anyone asking, and it writes the heartbeat the debt view's trust is judged by.
        #[arg(long = "verify-interval", default_value_t = 300)]
        verify_interval: u64,
    },
    /// Serve fleet state as JSON over HTTP for a live browser dashboard.
    Serve {
        /// Port to listen on.
        #[arg(long, default_value_t = 7777)]
        port: u16,
        /// Host/interface to bind.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Also host the browser dashboard from this directory (the repo's `web/`), so a single
        /// `cvd serve --web ./web` is a complete hub: UI at `/`, JSON API at `/api/*`.
        #[arg(long, value_name = "DIR")]
        web: Option<PathBuf>,
        /// Require `Authorization: Bearer <token>` on every /api/* request ($CVD_TOKEN also works).
        #[arg(long, value_name = "TOKEN")]
        token: Option<String>,
        /// Allow a non-loopback --host without a token. The transcript corpus can contain
        /// secrets; anyone who can reach the port can read all of it.
        #[arg(long)]
        insecure_expose: bool,
    },
    /// List what's in the archive.
    Ls,
    /// Print the archive location.
    Path,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    eprintln!("cvd {BUILD_VERSION}");
    let archive = Archive::resolve(cli.home.clone())?;

    match cli.command {
        Command::Sync => cmd_sync(&archive),
        Command::Watch { interval, harness, cwd, verify_interval } => {
            cmd_watch(&archive, interval, harness, cwd, verify_interval)
        }
        Command::Serve {
            port,
            host,
            web,
            token,
            insecure_expose,
        } => cmd_serve(&host, port, web, token, insecure_expose),
        Command::Ls => cmd_ls(&archive),
        Command::Path => {
            println!("{}", archive.home().display());
            Ok(())
        }
    }
}

/// Gatekeep `serve`'s exposure before handing off to [`serve::run`]: a non-loopback bind serves
/// the whole transcript corpus (secrets included) to anyone who can reach the port, so it demands
/// a bearer token or an explicit `--insecure-expose` — and warns loudly either way.
fn cmd_serve(host: &str, port: u16, web: Option<PathBuf>, token: Option<String>, insecure_expose: bool) -> Result<()> {
    let token = token
        .or_else(|| std::env::var("CVD_TOKEN").ok())
        .filter(|t| !t.is_empty());

    if !serve::is_loopback_host(host) {
        if token.is_none() && !insecure_expose {
            anyhow::bail!(
                "refusing to bind non-loopback host {host:?} without auth: the API serves your full \
                 transcript corpus (which can contain secrets) to anyone who can reach the port.\n\
                 Set a token (--token or $CVD_TOKEN), or pass --insecure-expose to serve it open."
            );
        }
        eprintln!("cvd serve: WARNING: binding non-loopback host {host:?} — the API is reachable from the network.");
        if token.is_none() {
            eprintln!("cvd serve: WARNING: --insecure-expose without a token: anyone who can reach {host}:{port} can read every archived transcript.");
        }
    }

    serve::run(host, port, web, token)
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
    println!("sync complete: {archived} archived, {skipped} skipped (unchanged) of {total} discovered");
    println!("archive: {}", archive.home().display());
    Ok(())
}

fn cmd_watch(
    archive: &Archive,
    interval: u64,
    harness: Option<String>,
    cwd: Option<String>,
    verify_interval: u64,
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

    if verify_interval > 0 {
        eprintln!("cvd: task verifier every {verify_interval}s");
    }

    // We don't use Watcher::run because it returns `!` (never returns) and we want to keep
    // ownership of `archive` for the closure; a manual loop reads identically and stays robust.
    let mut last_verify = std::time::Instant::now();
    loop {
        for ev in watcher.poll() {
            archive_event(archive, &ev.reference, ev.kind, ev.new_messages.len());
        }
        if verify_interval > 0 && last_verify.elapsed().as_secs() >= verify_interval {
            last_verify = std::time::Instant::now();
            let store = cv_core::task::TaskStore::default_store();
            let opts = cv_core::task::verify::VerifyOptions {
                interval_secs: Some(verify_interval),
                ..Default::default()
            };
            match cv_core::task::verify::run_verify(&store, None, &opts) {
                Ok((appended, warnings)) => {
                    for w in warnings {
                        eprintln!("cvd: verify: {w}");
                    }
                    for ev in appended {
                        eprintln!("cvd: observed {} on task {}", ev.kind.tag(), &ev.task_id[..8]);
                    }
                }
                Err(e) => eprintln!("cvd: verify pass failed: {e:#}"),
            }
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
            eprintln!("[{verb}] {} {}  +{delta} msgs  {}", r.harness, short(&r.id), ref_cwd(r));
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
