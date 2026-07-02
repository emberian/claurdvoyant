//! Live session following — the engine behind `cv scry`, the `cvd` daemon, and MCP `await_omen`.
//!
//! Each poll reads the fleet through the catalog fast path ([`crate::sessions`]), whose freshness
//! probe re-stats only the recorded watch set (session-bearing dirs, sqlite files, the most
//! recently updated transcripts) — milliseconds, not the stat-the-fleet scan `discover_all`
//! makes. A changed session is then diffed against what we saw last: transcripts that grow by
//! appended JSONL records (claude, codex rollouts) are **tailed** — only the bytes past the
//! remembered offset are read and parsed — while other layouts re-parse just the changed session
//! and diff by parsed message count. Priming records catalog metadata and file sizes only; no
//! session is parsed until it actually changes. Polling (rather than inotify) keeps it uniform
//! across all harness layouts (single file, dir-of-files, sqlite).

use crate::harness;
use crate::ir::{Harness, Message, Session, SessionRef};
use crate::stream::{CollectSink, ParseOptions};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
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
    /// Whether a session passes this filter (harness + cwd-substring). Public so non-`Watcher`
    /// consumers (e.g. the MCP `observe_stream` tool) can reuse the exact same filtering semantics
    /// rather than re-deriving them and drifting.
    pub fn matches(&self, r: &SessionRef) -> bool {
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
    /// Cheap change signal from discovery (no parse or stat needed to detect change).
    trigger: (usize, Option<i64>),
    baseline: Baseline,
}

/// How much of a session has already been reported.
enum Baseline {
    /// Reported through this byte offset — always a record-line start — of an append-only JSONL
    /// transcript. The tail path: an update reads and parses only the appended lines.
    Bytes(u64),
    /// Reported this many parsed IR messages. The fallback path: an update re-parses the (one)
    /// changed session and emits `messages[n..]`. A primed watcher seeds this from the discover
    /// `message_count` without parsing; for non-tailable harnesses whose parsed IR interleaves
    /// turns the discover count doesn't see (reasoning/tool records), the first update may
    /// mis-slice by a few messages — the price of not parsing the whole corpus at construction.
    Parsed(usize),
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

    /// Record current state without emitting anything — and without parsing: the baseline is the
    /// transcript's byte length (tail path; exact, whatever the discover count means) or the
    /// discover-time message count (fallback path). The first change to a session does the first
    /// read of that one session.
    fn prime(&mut self) {
        for r in self.discover() {
            self.seen.insert(
                Self::key(&r),
                State {
                    trigger: Self::trigger_of(&r),
                    baseline: Self::baseline_of(&r),
                },
            );
        }
    }

    /// The baseline for a session being recorded silently. Tailable transcripts use their current
    /// byte length (re-anchored to a record boundary by the first [`tail`]); anything unreadable
    /// or non-tailable falls back to the discover message count.
    fn baseline_of(r: &SessionRef) -> Baseline {
        if tailable(r) {
            if let Ok(md) = std::fs::metadata(&r.path) {
                return Baseline::Bytes(md.len());
            }
        }
        Baseline::Parsed(r.message_count)
    }

    /// The filtered fleet, via the catalog fast path: [`crate::sessions`]'s freshness probe makes
    /// this a few hundred stats when nothing changed, escalating to a re-discovery scoped to just
    /// the harnesses the probe implicates — plus the full-scan backstop every
    /// `CLUSTERVISION_MAX_STALE_SECS`, which covers the probe's documented blind spots.
    fn discover(&self) -> Vec<SessionRef> {
        crate::sessions()
            .into_iter()
            .filter(|r| self.filter.matches(r))
            .collect()
    }

    fn parse(r: &SessionRef) -> Option<Session> {
        harness::for_harness(r.harness)?.parse(r).ok()
    }

    /// A session seen for the first time: the whole conversation plus its baseline. Tailable
    /// transcripts are read only up to their last complete record line — the byte the baseline
    /// records — so nothing is lost or double-reported if the file grows mid-read.
    fn first_read(r: &SessionRef) -> Option<(Vec<Message>, Baseline)> {
        if tailable(r) {
            let mut f = std::fs::File::open(&r.path).ok()?;
            let size = f.metadata().ok()?.len();
            let cut = line_start_at_or_before(&mut f, size).ok()?;
            let messages = if cut > 0 {
                read_lines(r, &mut f, 0, cut)?
            } else {
                Vec::new()
            };
            return Some((messages, Baseline::Bytes(cut)));
        }
        let session = Self::parse(r)?;
        let len = session.messages.len();
        Some((session.messages, Baseline::Parsed(len)))
    }

    /// The new messages of a changed session, plus its refreshed baseline. `None` = couldn't read
    /// right now (state stays put; the next poll retries).
    fn delta(r: &SessionRef, baseline: &Baseline) -> Option<(Vec<Message>, Baseline)> {
        match *baseline {
            Baseline::Bytes(pos) => tail(r, pos).map(|(messages, end)| (messages, Baseline::Bytes(end))),
            Baseline::Parsed(prev_len) => {
                let session = Self::parse(r)?;
                let new = if session.messages.len() > prev_len {
                    session.messages[prev_len..].to_vec()
                } else {
                    Vec::new()
                };
                Some((new, Baseline::Parsed(session.messages.len())))
            }
        }
    }

    /// Poll once and return any events since the previous poll.
    pub fn poll(&mut self) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        let refs = self.discover();
        // Prune entries for sessions that vanished from discovery (deleted/pruned transcripts),
        // so a long-lived watcher (cvd) doesn't grow `seen` without bound. The size check skips
        // building the key set on the common nothing-vanished poll.
        if self.seen.len() > refs.len() {
            let live: std::collections::HashSet<String> = refs.iter().map(Self::key).collect();
            self.seen.retain(|k, _| live.contains(k));
        }
        for r in refs {
            let key = Self::key(&r);
            let trigger = Self::trigger_of(&r);
            match self.seen.get(&key) {
                None => {
                    let Some((new_messages, baseline)) = Self::first_read(&r) else {
                        continue;
                    };
                    events.push(SessionEvent {
                        kind: EventKind::New,
                        reference: r,
                        new_messages,
                    });
                    self.seen.insert(key, State { trigger, baseline });
                }
                Some(state) if state.trigger != trigger => {
                    let Some((new_messages, baseline)) = Self::delta(&r, &state.baseline) else {
                        continue;
                    };
                    if !new_messages.is_empty() {
                        events.push(SessionEvent {
                            kind: EventKind::Updated,
                            reference: r,
                            new_messages,
                        });
                    }
                    self.seen.insert(key, State { trigger, baseline });
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

/// Whether `r`'s transcript can be tailed: an append-only JSONL file whose records parse
/// independently of the skipped prefix — the same per-record contract the seek-replay paths in
/// [`crate::offsets`] rely on. Claude transcripts always; codex modern `.jsonl` rollouts (the
/// 2025 legacy single-JSON layout is whole-file). Everything else diffs by re-parse.
fn tailable(r: &SessionRef) -> bool {
    match r.harness {
        Harness::Claude => true,
        Harness::Codex => r.path.extension().and_then(|e| e.to_str()) == Some("jsonl"),
        _ => false,
    }
}

/// Parse only what was appended past `pos`: `(new messages, refreshed byte baseline)`.
///
/// The window is clamped to whole record lines on both ends: `pos` is re-anchored back to a line
/// start (a primed baseline is a raw stat size, which can land inside a record caught mid-write —
/// that record was never reported, so backing up loses nothing), and the far end stops at the
/// last complete line, so a record whose newline hasn't landed yet is picked up whole by the poll
/// that sees it finished. A file that *shrank* (rewritten/truncated) re-anchors quietly instead
/// of replaying content we can't prove is new — the parsed-length diff went silent on a shrink
/// too. `None` = I/O failure; the caller leaves state untouched and retries next poll.
fn tail(r: &SessionRef, pos: u64) -> Option<(Vec<Message>, u64)> {
    let mut f = std::fs::File::open(&r.path).ok()?;
    let size = f.metadata().ok()?.len();
    if size < pos {
        return Some((Vec::new(), line_start_at_or_before(&mut f, size).ok()?));
    }
    let start = line_start_at_or_before(&mut f, pos).ok()?;
    let cut = line_start_at_or_before(&mut f, size).ok()?;
    if cut <= start {
        return Some((Vec::new(), start));
    }
    let messages = read_lines(r, &mut f, start, cut)?;
    Some((messages, cut))
}

/// Stream the record lines in `[start, cut)` into messages, with the same [`ParseOptions`] as
/// [`Adapter::parse`](crate::harness::Adapter::parse) so tailed messages are shaped identically
/// to fully-parsed ones.
fn read_lines(r: &SessionRef, f: &mut std::fs::File, start: u64, cut: u64) -> Option<Vec<Message>> {
    let opts = ParseOptions::full();
    let mut sink = CollectSink::default();
    match r.harness {
        Harness::Claude => {
            f.seek(SeekFrom::Start(start)).ok()?;
            let reader = std::io::BufReader::new(f.by_ref().take(cut - start));
            harness::claude::stream_reader_from(&r.id, reader, Some(r.path.clone()), start, &opts, &mut sink);
        }
        Harness::Codex => {
            // `has_events` is a head property of the rollout; one cheap bounded read re-detects
            // it. The model at `start` is unknown (we never parsed the head), so a `turn_context`
            // in the window sets the model silently instead of emitting a `[model changed]` note —
            // a mid-watch model switch loses its note, nothing else.
            let head = std::fs::File::open(&r.path).ok()?;
            let has_events = harness::codex::detect_has_events(std::io::BufReader::new(head));
            f.seek(SeekFrom::Start(start)).ok()?;
            let reader = std::io::BufReader::new(f.by_ref().take(cut - start));
            harness::codex::stream_jsonl(&r.id, reader, Some(r.path.clone()), has_events, &opts, &mut sink);
        }
        _ => return None,
    }
    Some(sink.messages)
}

/// The largest line start `<= limit`: 0, or one past the last `\n` strictly before `limit`.
/// Scans backward in blocks; outside a mid-append instant a JSONL file ends with a newline, so in
/// practice this is a single small read that finds `\n` at `limit - 1`.
fn line_start_at_or_before(f: &mut std::fs::File, limit: u64) -> std::io::Result<u64> {
    const BLOCK: u64 = 8192;
    let mut buf = [0u8; BLOCK as usize];
    let mut hi = limit;
    while hi > 0 {
        let lo = hi.saturating_sub(BLOCK);
        f.seek(SeekFrom::Start(lo))?;
        let n = (hi - lo) as usize;
        f.read_exact(&mut buf[..n])?;
        if let Some(i) = buf[..n].iter().rposition(|&b| b == b'\n') {
            return Ok(lo + i as u64 + 1);
        }
        hi = lo;
    }
    Ok(0)
}
