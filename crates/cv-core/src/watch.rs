//! Live session following — the engine behind `cv scry`, the `cvd` daemon, and MCP `await_omen`.
//!
//! It polls discovery on an interval, diffs each session's message tail against what it saw last,
//! and emits [`SessionEvent`]s for new sessions and newly-appended messages. Polling (rather than
//! inotify) keeps it uniform across all harness layouts (single file, dir-of-files, sqlite).

use crate::harness;
use crate::ir::{Harness, Message, Session, SessionRef};
use std::collections::HashMap;
use std::time::Duration;

/// What changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// A session we'd never seen before appeared.
    New,
    /// An existing session gained messages.
    Updated,
}

/// A live event about a session.
#[derive(Debug, Clone)]
pub struct SessionEvent {
    pub kind: EventKind,
    pub reference: SessionRef,
    /// For `New`: the whole conversation. For `Updated`: only the newly-appended messages.
    pub new_messages: Vec<Message>,
}

/// Narrow what to follow.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub harness: Option<Harness>,
    pub cwd_contains: Option<String>,
}

impl Filter {
    fn matches(&self, r: &SessionRef) -> bool {
        if let Some(h) = self.harness {
            if r.harness != h {
                return false;
            }
        }
        if let Some(needle) = &self.cwd_contains {
            let hit = r
                .cwd
                .as_ref()
                .map(|p| p.to_string_lossy().contains(needle))
                .unwrap_or(false);
            if !hit {
                return false;
            }
        }
        true
    }
}

struct State {
    /// Cheap change signal from discovery (no full parse needed to detect change).
    trigger: (usize, Option<i64>),
    /// How many IR messages we've already reported.
    parsed_len: usize,
}

/// A stateful poller. Call [`poll`](Watcher::poll) repeatedly, or [`run`](Watcher::run) to loop.
pub struct Watcher {
    filter: Filter,
    seen: HashMap<String, State>,
}

impl Watcher {
    /// Create a watcher. If `emit_existing` is false, the current sessions are recorded silently so
    /// only activity *after* construction is reported (the usual `tail -f` feel).
    pub fn new(filter: Filter, emit_existing: bool) -> Self {
        let mut w = Watcher {
            filter,
            seen: HashMap::new(),
        };
        if !emit_existing {
            w.prime();
        }
        w
    }

    fn key(r: &SessionRef) -> String {
        format!("{}:{}", r.harness.as_str(), r.id)
    }

    fn trigger_of(r: &SessionRef) -> (usize, Option<i64>) {
        (r.message_count, r.updated_at.map(|t| t.timestamp_millis()))
    }

    /// Record current state without emitting anything.
    fn prime(&mut self) {
        for r in self.discover() {
            // The cheap discover `message_count` is NOT the parsed IR length for several harnesses
            // (codex/hermes/claude add reasoning/tool/system turns), so seeding `parsed_len` from it
            // makes the first `Updated` poll re-emit messages that predate the watcher. Parse here to
            // record the true baseline.
            let parsed_len = Self::parse(&r)
                .map(|s| s.messages.len())
                .unwrap_or(r.message_count);
            self.seen.insert(
                Self::key(&r),
                State {
                    trigger: Self::trigger_of(&r),
                    parsed_len,
                },
            );
        }
    }

    fn discover(&self) -> Vec<SessionRef> {
        crate::discover_all()
            .into_iter()
            .filter(|r| self.filter.matches(r))
            .collect()
    }

    fn parse(r: &SessionRef) -> Option<Session> {
        harness::for_harness(r.harness)?.parse(r).ok()
    }

    /// Poll once and return any events since the previous poll.
    pub fn poll(&mut self) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        for r in self.discover() {
            let key = Self::key(&r);
            let trigger = Self::trigger_of(&r);
            match self.seen.get(&key) {
                None => {
                    let Some(session) = Self::parse(&r) else {
                        continue;
                    };
                    let parsed_len = session.messages.len();
                    events.push(SessionEvent {
                        kind: EventKind::New,
                        reference: r,
                        new_messages: session.messages,
                    });
                    self.seen.insert(key, State { trigger, parsed_len });
                }
                Some(state) if state.trigger != trigger => {
                    let prev_len = state.parsed_len;
                    let Some(session) = Self::parse(&r) else {
                        continue;
                    };
                    let new_messages = if session.messages.len() > prev_len {
                        session.messages[prev_len..].to_vec()
                    } else {
                        Vec::new()
                    };
                    let parsed_len = session.messages.len();
                    if !new_messages.is_empty() {
                        events.push(SessionEvent {
                            kind: EventKind::Updated,
                            reference: r,
                            new_messages,
                        });
                    }
                    self.seen.insert(key, State { trigger, parsed_len });
                }
                Some(_) => {}
            }
        }
        events
    }

    /// Poll forever, invoking `on_event` for each event. Blocks the calling thread.
    pub fn run<F: FnMut(SessionEvent)>(&mut self, interval: Duration, mut on_event: F) -> ! {
        loop {
            for ev in self.poll() {
                on_event(ev);
            }
            std::thread::sleep(interval);
        }
    }
}
