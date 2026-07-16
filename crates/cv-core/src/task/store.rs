//! Durable task event log: a single append-only JSONL file with locked compare-and-swap appends.
//!
//! `$CLUSTERVISION_HOME/tasks/events.jsonl` + sibling `events.lock`, using the same crash-safe
//! flock + `O_APPEND` single-`write_all` recipe as the board (see [`crate::lockfile`]).
//!
//! # Why not a board channel
//!
//! The board's reader deliberately *skips* malformed lines — right for chat, fatal for a replayed
//! event log (a silently dropped `Refuted` would resurrect a dead revision). Here interior
//! garbage is **loud** (surfaced as warnings on every read) and only a *trailing* partial line —
//! the one torn shape the locked writer can produce by crashing mid-append — is tolerated.
//!
//! # The CAS append
//!
//! [`TaskStore::append`] holds the lock across replay-validate-append: acquire → replay the log
//! through the reducer → apply the candidate event (any [`ReduceError`] rejects it) → append →
//! release. This is what makes `claim` a race-free first-writer-wins with zero extra lock kinds,
//! and it guarantees the log never contains an event the reducer would refuse on replay.
//!
//! # Law 1 at the seam
//!
//! [`TaskStore::append_agent_event`] — the only path the CLI verbs and MCP tools use — rejects
//! verifier-only kinds (`MergedLocal`, `Landed`, ...). Only the git verifier calls
//! [`TaskStore::append_verifier_event`]. The reducer itself cannot enforce this (it must replay
//! verifier events); the store seam is where "observed, not attested" becomes mechanical.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;

use crate::lockfile::FileLock;

use super::model::{TaskEvent, TaskEventKind};
use super::reduce::{TaskReadModel, TaskReducer};

/// A parsed replay of the whole log: the read model plus any loud warnings about interior
/// garbage. Every read surface must show `warnings` to its caller — a corrupted coordination log
/// is a finding, not a detail.
#[derive(Debug)]
pub struct ReplayOutcome {
    pub model: TaskReadModel,
    pub events: Vec<TaskEvent>,
    pub warnings: Vec<String>,
}

/// Handle on a task log directory. Cheap to construct; every operation opens files fresh.
#[derive(Clone, Debug)]
pub struct TaskStore {
    dir: PathBuf,
}

impl TaskStore {
    /// The default store at [`super::tasks_dir`].
    pub fn default_store() -> TaskStore {
        TaskStore { dir: super::tasks_dir() }
    }

    /// A store rooted at `dir` (testing and embedding).
    pub fn at(dir: impl Into<PathBuf>) -> TaskStore {
        TaskStore { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn events_path(&self) -> PathBuf {
        self.dir.join("events.jsonl")
    }

    fn lock_path(&self) -> PathBuf {
        self.dir.join("events.lock")
    }

    /// Replay the log without holding the lock (readers never block writers; a concurrent append
    /// is either wholly visible or wholly absent thanks to single-`write_all` O_APPEND lines).
    pub fn replay(&self) -> Result<ReplayOutcome> {
        let (events, warnings) = self.read_events()?;
        let model = TaskReducer::reduce(&events).map_err(|e| {
            anyhow::anyhow!("task log {} fails replay: {e}", self.events_path().display())
        })?;
        Ok(ReplayOutcome { model, events, warnings })
    }

    /// Agent-facing append: rejects verifier-only kinds (law 1), then CAS-appends.
    pub fn append_agent_event(&self, event: TaskEvent) -> Result<TaskEvent> {
        if event.kind.is_verifier_only() {
            bail!(
                "event kind '{}' is verifier-only: landing state is observed by `cv task verify`, never asserted",
                event.kind.tag()
            );
        }
        self.append(event)
    }

    /// Verifier-only append: same CAS, no kind restriction. Callers other than the git verifier
    /// (and its tests) must not use this.
    pub fn append_verifier_event(&self, event: TaskEvent) -> Result<TaskEvent> {
        self.append(event)
    }

    /// The locked CAS: replay → apply candidate → append. Returns the appended event.
    fn append(&self, event: TaskEvent) -> Result<TaskEvent> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating tasks dir {}", self.dir.display()))?;
        let _lock = FileLock::acquire(self.lock_path())?;

        // Replay under the lock: the candidate is validated against the exact state it will
        // land after.
        let (events, _warnings) = self.read_events()?;
        let mut reducer = TaskReducer::new();
        for ev in &events {
            reducer.apply(ev).map_err(|e| {
                anyhow::anyhow!("task log {} fails replay: {e}", self.events_path().display())
            })?;
        }
        reducer
            .apply(&event)
            .map_err(|e| anyhow::anyhow!("event rejected: {e}"))?;

        let mut line = serde_json::to_string(&event).context("serializing task event")?;
        line.push('\n');
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.events_path())
            .with_context(|| format!("opening task log {}", self.events_path().display()))?;
        f.write_all(line.as_bytes())
            .with_context(|| format!("appending to task log {}", self.events_path().display()))?;
        f.flush()
            .with_context(|| format!("flushing task log {}", self.events_path().display()))?;
        Ok(event)
    }

    /// Read raw events. Interior unparseable lines are collected as warnings; a trailing partial
    /// line (no terminating newline, fails to parse) is tolerated silently.
    fn read_events(&self) -> Result<(Vec<TaskEvent>, Vec<String>)> {
        let path = self.events_path();
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Vec::new(), Vec::new()))
            }
            Err(e) => {
                return Err(e).with_context(|| format!("opening task log {}", path.display()))
            }
        };

        let mut events: Vec<TaskEvent> = Vec::new();
        let mut bad: Vec<(usize, String)> = Vec::new(); // (1-based line no, snippet)
        let mut reader = BufReader::new(file);
        let mut raw = String::new();
        let mut line_no = 0usize;
        loop {
            raw.clear();
            let n = reader
                .read_line(&mut raw)
                .with_context(|| format!("reading task log {}", path.display()))?;
            if n == 0 {
                break;
            }
            line_no += 1;
            let terminated = raw.ends_with('\n');
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<TaskEvent>(line) {
                Ok(ev) => events.push(ev),
                Err(_) if !terminated => {
                    // Trailing torn line: the only shape a crashed locked writer leaves behind.
                    break;
                }
                Err(_) => {
                    let snippet: String = line.chars().take(80).collect();
                    bad.push((line_no, snippet));
                }
            }
        }

        let warnings = bad
            .into_iter()
            .map(|(n, snippet)| {
                format!(
                    "task log {}: line {} is not a valid event (log may be corrupted): {}",
                    path.display(),
                    n,
                    snippet
                )
            })
            .collect();
        Ok((events, warnings))
    }
}

/// Build a new event with a fresh uuid v7 id and `ts = now`. For `Opened`, pass
/// `task_id = None` — the event id becomes the task id (open identity invariant).
pub fn new_event(task_id: Option<&str>, by: &str, kind: TaskEventKind) -> TaskEvent {
    let id = uuid::Uuid::now_v7().to_string();
    TaskEvent {
        task_id: task_id.map(String::from).unwrap_or_else(|| id.clone()),
        id,
        ts: Utc::now(),
        by: by.to_string(),
        kind,
    }
}

/// Convenience: agent-facing append to the default store.
pub fn append_agent_event(task_id: Option<&str>, by: &str, kind: TaskEventKind) -> Result<TaskEvent> {
    TaskStore::default_store().append_agent_event(new_event(task_id, by, kind))
}

/// Convenience: verifier append to the default store.
pub fn append_verifier_event(task_id: &str, by: &str, kind: TaskEventKind) -> Result<TaskEvent> {
    TaskStore::default_store().append_verifier_event(new_event(Some(task_id), by, kind))
}

/// Convenience: replay the default store.
pub fn replay() -> Result<ReplayOutcome> {
    TaskStore::default_store().replay()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::model::TaskState;
    use std::io::Write as _;

    /// A throwaway task dir under the system temp dir, unique per test (board.rs idiom).
    fn tmp_tasks() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cv-task-test-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open_kind() -> TaskEventKind {
        TaskEventKind::Opened {
            title: "demo".into(),
            body: String::new(),
            repo: None,
            issue: None,
            channel: "tasks".into(),
            assignee: None,
        }
    }

    fn open_task(store: &TaskStore) -> String {
        store
            .append_agent_event(new_event(None, "agent:author", open_kind()))
            .unwrap()
            .task_id
    }

    #[test]
    fn append_then_replay_round_trips() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);
        store
            .append_agent_event(new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Claimed { assignee: "agent:a".into() },
            ))
            .unwrap();

        let outcome = store.replay().unwrap();
        assert!(outcome.warnings.is_empty());
        assert_eq!(outcome.events.len(), 2);
        assert_eq!(outcome.model.tasks[&task].state, TaskState::Claimed);
    }

    #[test]
    fn cas_rejects_events_the_reducer_refuses() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);

        // Done on an Open task with no live revision is fine…
        store
            .append_agent_event(new_event(Some(&task), "agent:a", TaskEventKind::Done { observed: None }))
            .unwrap();
        // …after which nothing else applies, and crucially the log did NOT grow.
        let before = store.replay().unwrap().events.len();
        let err = store
            .append_agent_event(new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Claimed { assignee: "agent:a".into() },
            ))
            .unwrap_err();
        assert!(err.to_string().contains("event rejected"), "{err}");
        assert_eq!(store.replay().unwrap().events.len(), before);
    }

    #[test]
    fn agent_append_rejects_verifier_only_kinds() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);
        let err = store
            .append_agent_event(new_event(
                Some(&task),
                "agent:sneaky",
                TaskEventKind::Landed {
                    upstream_head: "a".repeat(40),
                    observed_patch_id: "b".repeat(40),
                },
            ))
            .unwrap_err();
        assert!(err.to_string().contains("verifier-only"), "{err}");
        // Nothing was appended.
        assert_eq!(store.replay().unwrap().events.len(), 1);
    }

    #[test]
    fn concurrent_claims_have_exactly_one_winner() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);

        let n = 8;
        let wins: Vec<bool> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..n)
                .map(|i| {
                    let store = store.clone();
                    let task = task.clone();
                    s.spawn(move || {
                        store
                            .append_agent_event(new_event(
                                Some(&task),
                                &format!("agent:{i}"),
                                TaskEventKind::Claimed { assignee: format!("agent:{i}") },
                            ))
                            .is_ok()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        assert_eq!(wins.iter().filter(|w| **w).count(), 1, "exactly one claim must win");
        let outcome = store.replay().unwrap();
        assert_eq!(outcome.events.len(), 2); // open + the single winning claim
        assert_eq!(outcome.model.tasks[&task].state, TaskState::Claimed);
    }

    #[test]
    fn trailing_torn_line_is_tolerated_but_interior_garbage_is_loud() {
        let dir = tmp_tasks();
        let store = TaskStore::at(&dir);
        let task = open_task(&store);
        store
            .append_agent_event(new_event(
                Some(&task),
                "agent:a",
                TaskEventKind::Noted { text: "hi".into(), session_ref: None },
            ))
            .unwrap();

        // Torn trailing line (no newline): tolerated silently.
        let path = store.events_path();
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"id\":\"trunc").unwrap();
        drop(f);
        let outcome = store.replay().unwrap();
        assert_eq!(outcome.events.len(), 2);
        assert!(outcome.warnings.is_empty());

        // Interior garbage (newline-terminated junk followed by a valid event): loud.
        let mut f = OpenOptions::new().write(true).truncate(true).open(&path).unwrap();
        let good = serde_json::to_string(&new_event(None, "agent:a", open_kind())).unwrap();
        writeln!(f, "not json at all").unwrap();
        writeln!(f, "{good}").unwrap();
        drop(f);
        let outcome = store.replay().unwrap();
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.warnings.len(), 1);
        assert!(outcome.warnings[0].contains("line 1"), "{}", outcome.warnings[0]);
    }

    #[test]
    fn replay_equals_live_across_process_restarts() {
        // Simulated restart: two distinct TaskStore values over the same dir.
        let dir = tmp_tasks();
        let task = {
            let store = TaskStore::at(&dir);
            open_task(&store)
        };
        let store2 = TaskStore::at(&dir);
        store2
            .append_agent_event(new_event(
                Some(&task),
                "agent:b",
                TaskEventKind::Claimed { assignee: "agent:b".into() },
            ))
            .unwrap();
        let outcome = store2.replay().unwrap();
        assert_eq!(outcome.model.tasks[&task].state, TaskState::Claimed);
        assert_eq!(
            outcome.model,
            TaskReducer::reduce(&outcome.events).unwrap(),
            "replay parity"
        );
    }
}
