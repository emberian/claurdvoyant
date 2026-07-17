//! Replay-cost bench: the O(n²) cliff of the task substrate's append-only log, made visible.
//!
//! The store's CAS append replays the *whole* log under the lock before every write
//! (validate-against-exact-state), so a single append costs O(n) in log size and a fleet's
//! lifetime of n appends costs O(n²) total — the RAMCloud lesson. This bench measures, at
//! several log sizes, (a) one full replay and (b) one CAS append. It deliberately does NOT
//! fix the cliff (that is the snapshot+tail compaction plan in `task/store.rs`); it prices it.
//!
//! Emits a complete markdown document on stdout:
//!
//! ```text
//! cargo run --release -p cv-sim --bin cv-sim-bench > crates/cv-sim/BENCH.md
//! ```

use std::fs::{self, File};
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use cv_core::task::{new_event, TaskEvent, TaskEventKind, TaskStore};
use cv_sim::{FleetScenario, Pathology};

const SIZES: [usize; 4] = [1_000, 5_000, 20_000, 50_000];
const REPLAY_RUNS: usize = 3;
const APPEND_RUNS: usize = 5;

/// The store's log-format header (`task/store.rs` writes this exact line on a fresh log; the
/// reader skips it). Duplicated here because the bench writes log files directly — appending
/// 50k events through the CAS would itself be the O(n²) it is trying to measure.
const HEADER_LINE: &str = r#"{"format":"cv-task-log","v":1}"#;

fn main() -> Result<()> {
    let max = *SIZES.iter().max().expect("SIZES is non-empty");
    // ~7 events per task; overshoot so truncation always has enough.
    let scenario = FleetScenario {
        endpoints: 12,
        reviewers: 4,
        tasks: max / 4,
        seed: 20260716,
        pathology: Pathology::default(),
    };
    let events = scenario
        .generate()
        .map_err(|e| anyhow::anyhow!("scenario generation failed: {e}"))?;
    anyhow::ensure!(
        events.len() >= max,
        "scenario produced {} events, need {max}",
        events.len()
    );

    let root = std::env::temp_dir().join(format!("cv-sim-bench-{}", std::process::id()));
    fs::create_dir_all(&root)?;

    println!("# cv-sim replay-cost bench");
    println!();
    println!("The task log's CAS append replays the whole log before every write, so append cost");
    println!("grows linearly with history and lifetime append cost grows quadratically. Measured,");
    println!("not fixed (the fix is the snapshot+tail compaction plan in `task/store.rs`).");
    println!();
    println!("- machine: {}", machine());
    println!("- profile: {}", profile());
    println!("- date: {}", chrono::Utc::now().format("%Y-%m-%d"));
    println!("- method: synthetic fleet (`FleetScenario`, seed 20260716), log written directly at");
    println!("  each size; replay is best of {REPLAY_RUNS} full `TaskStore::replay` calls; append is the mean");
    println!("  of {APPEND_RUNS} `append_agent_event` calls (each one replay + validate + one write).");
    println!();
    println!("| events | log size | full replay | single CAS append |");
    println!("|-------:|---------:|------------:|------------------:|");

    for &size in &SIZES {
        let dir = root.join(format!("n{size}"));
        fs::create_dir_all(&dir)?;
        let log_bytes = write_log(&dir, &events[..size])?;
        let store = TaskStore::at(&dir);

        let mut replay_ms = f64::MAX;
        for _ in 0..REPLAY_RUNS {
            let t = Instant::now();
            let outcome = store.replay().context("replay failed")?;
            let dt = t.elapsed().as_secs_f64() * 1e3;
            anyhow::ensure!(
                outcome.warnings.is_empty() && outcome.quarantined == 0,
                "bench log must replay clean: {:?}",
                outcome.warnings
            );
            anyhow::ensure!(
                outcome.events.len() == size,
                "replay saw {} events",
                outcome.events.len()
            );
            replay_ms = replay_ms.min(dt);
        }

        let mut append_ms_total = 0.0;
        for i in 0..APPEND_RUNS {
            let ev = new_event(
                None,
                "agent:bench",
                TaskEventKind::Opened {
                    title: format!("bench probe {i}"),
                    body: String::new(),
                    repo: None,
                    issue: None,
                    channel: "tasks".to_string(),
                    assignee: None,
                },
            );
            let t = Instant::now();
            store.append_agent_event(ev).context("CAS append failed")?;
            append_ms_total += t.elapsed().as_secs_f64() * 1e3;
        }
        let append_ms = append_ms_total / APPEND_RUNS as f64;

        println!(
            "| {size} | {} | {replay_ms:.1} ms | {append_ms:.1} ms |",
            fmt_bytes(log_bytes)
        );
    }

    println!();
    println!(
        "Append ≈ replay at every size: the log walk dominates, the write is constant. \
         Doubling history doubles every future append."
    );

    let _ = fs::remove_dir_all(&root);
    Ok(())
}

/// Write `events` as a store-shaped log (header + one JSON line per event). Returns file size.
fn write_log(dir: &Path, events: &[TaskEvent]) -> Result<u64> {
    let path: PathBuf = dir.join("events.jsonl");
    let mut w = BufWriter::new(File::create(&path)?);
    writeln!(w, "{HEADER_LINE}")?;
    for ev in events {
        writeln!(w, "{}", serde_json::to_string(ev)?)?;
    }
    w.flush()?;
    Ok(fs::metadata(&path)?.len())
}

fn machine() -> String {
    let cpu = std::process::Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| std::env::consts::ARCH.to_string());
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
    format!(
        "{cpu}, {cores} cores, {}/{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

fn profile() -> &'static str {
    if cfg!(debug_assertions) {
        "DEBUG (numbers are not representative — rerun with --release)"
    } else {
        "release"
    }
}

fn fmt_bytes(b: u64) -> String {
    if b >= 1_048_576 {
        format!("{:.1} MiB", b as f64 / 1_048_576.0)
    } else {
        format!("{:.0} KiB", b as f64 / 1_024.0)
    }
}
