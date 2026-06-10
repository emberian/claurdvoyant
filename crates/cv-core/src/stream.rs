//! Streaming parse: the message stream — not the whole [`Session`] — is the fundamental unit.
//!
//! [`Adapter::parse`](crate::harness::Adapter::parse) materializes an entire transcript into one
//! owned `Session { messages: Vec<Message> }`. That sets the memory floor for everything: a 1 GB
//! transcript becomes several GB of `Vec<Message>` + owned `String`s, even when the consumer only
//! makes a single forward pass (index, export, dataset, live-search).
//!
//! [`Adapter::stream`](crate::harness::Adapter::stream) instead hands each [`Message`] to a
//! [`MessageSink`] and drops it before reading the next, so peak memory is O(largest message). The
//! whole-`Session` `parse` is recovered for free as "stream into a [`CollectSink`]" (see
//! [`collect`]). Adapters override **one** of the two: a native `stream` (memory-savings) *or* the
//! existing `parse` (and inherit a `stream` that bridges through it). No mutual recursion, so
//! migration is incremental.

use crate::harness::Adapter;
use crate::ir::{Message, Session, SessionRef};
use anyhow::Result;

/// Knobs controlling how much of each message an adapter materializes. Everything off (the
/// [`Default`]) is the cheapest faithful pass; full-fidelity callers opt back in.
///
/// # Which constructor? (the decision rule)
///
/// - [`full()`](ParseOptions::full) — you will **re-emit or round-trip** the session (convert,
///   port, `--json`, splice/loom, redact-to-file). Everything materialized, inline. Most
///   expensive; wrong for anything that only *reads*.
/// - [`bulk()`](ParseOptions::bulk) — you read **all** the text once, forward-only, and never
///   touch `extra` (live search over small/whole-doc harnesses, dataset-style flattening of
///   sessions you'll fully consume). Inline content, no sidecars.
/// - [`lazy()`](ParseOptions::lazy) — you read **at most a bounded amount** of any one field, or
///   skip content entirely: windowed/`--range` reads, metadata/event extraction, snippet scans,
///   head-only embedding, chunk-wise indexing. Large fields arrive as [`Span`](crate::lazy::Span)s
///   you resolve (possibly capped via [`Resolver::resolve_prefix`](crate::lazy::Resolver::
///   resolve_prefix) or chunked via `for_each_chunk`) — a 700 MB record costs 16 bytes until you
///   choose otherwise. Adapters without a span path just hand you inline content, so `lazy()` is
///   never *less* correct than `bulk()`, only cheaper when spans exist.
///
/// Rule of thumb: emit ⇒ `full`, read-everything ⇒ `bulk`, read-some/bounded ⇒ `lazy`.
#[derive(Debug, Clone, Default)]
pub struct ParseOptions {
    /// Populate the harness-specific `extra` maps — most importantly Claude's `toolUseResult`
    /// sidecar (structured patches, file contents, command stdout), which is `clone`d per tool
    /// turn and can dwarf the visible transcript. Bulk text consumers (index/search/dataset) never
    /// read it; only full-fidelity paths (json export, cross-harness convert/port, round-trips) do.
    pub extra: bool,
    /// Emit large content as lazy [`Span`](crate::lazy::Span)s into the source instead of owned
    /// strings. Off by default: it only pays off for **partial-access** consumers (e.g. reading a
    /// window of messages, web pagination) that never touch the giant fields — a consumer that
    /// reads *all* content materializes it anyway and just adds mmap overhead. Adapters that can't
    /// span ignore it and produce inline content regardless.
    pub spans: bool,
    /// Stamp each message's source **byte offset** into
    /// `Message.extra[`[`offsets::OFFSET_KEY`](crate::offsets::OFFSET_KEY)`]` (the offset of the
    /// record line that produced it). Only the offset-recording ingest pass
    /// ([`offsets::OffsetSink`](crate::offsets::OffsetSink) riding `cv index`) turns this on —
    /// ordinary lazy/bulk/full consumers never see the stamp, so their output is unchanged.
    /// Adapters that don't track byte positions (everything but claude/codex JSONL) ignore it.
    /// Implies nothing by itself: the adapters only stamp on their span-tracking paths, so use
    /// [`lazy_offsets()`](ParseOptions::lazy_offsets) (spans + offsets) rather than setting it solo.
    pub offsets: bool,
}

impl ParseOptions {
    /// Full fidelity: every harness-specific field materialized, inline content. Used by [`collect`]
    /// / `parse` and by conversion/round-trip paths that must lose nothing.
    pub fn full() -> Self {
        ParseOptions { extra: true, ..Default::default() }
    }

    /// The lean bulk-text pass: faithful message text, no fat `extra` sidecars, inline content
    /// (these consumers read everything, so spans wouldn't help). Used by index/search/dataset.
    pub fn bulk() -> Self {
        ParseOptions::default()
    }

    /// Partial-access pass: lazy content [`Span`](crate::lazy::Span)s so unread giant fields are
    /// never materialized. For windowed reads / pagination (Phase 2).
    pub fn lazy() -> Self {
        ParseOptions { spans: true, ..Default::default() }
    }

    /// [`lazy()`](ParseOptions::lazy) plus per-message byte-offset stamping — the **recording**
    /// pass `cv index` makes so [`offsets`](crate::offsets) can persist seek points. Everything
    /// else should use plain `lazy()`.
    pub fn lazy_offsets() -> Self {
        ParseOptions { spans: true, offsets: true, ..Default::default() }
    }
}

/// Whether a [`MessageSink`] wants the stream to keep going. Returning [`Flow::Stop`] lets a
/// consumer that has seen enough (e.g. a head-only embedder, or a `--range` window) end the parse
/// without reading the rest of the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

/// Receives messages as an adapter streams them. [`message`](MessageSink::message) is called once
/// per message, in transcript order; the message is dropped after the call returns unless the sink
/// keeps it.
///
/// [`meta`](MessageSink::meta) is an optional early signal carrying the session-level fields
/// (model/cwd/title/git) with an empty `messages` vec. Adapters that know the metadata before the
/// body call it *before* the first [`message`](MessageSink::message); the bridge `stream` (over a
/// full `parse`) always does. Sinks that must render a header ahead of the body (e.g. `cv show`)
/// use it; most sinks ignore it and just read the returned `Session` afterward.
pub trait MessageSink {
    fn message(&mut self, m: Message) -> Flow;

    /// Session metadata, possibly emitted before the first message. Default: ignore.
    fn meta(&mut self, _session: &Session) {}
}

/// Any `FnMut(Message) -> Flow` is a sink — the ergonomic form for ad-hoc consumers.
impl<F: FnMut(Message) -> Flow> MessageSink for F {
    fn message(&mut self, m: Message) -> Flow {
        self(m)
    }
}

/// Fans one adapter pass into two sinks, so a single read of a transcript feeds multiple consumers
/// — e.g. `cv index` driving the FTS [`ChunkSink`] and the event-catalog
/// [`EventSink`](crate::events::EventSink) off the same `stream()`. Each message is cloned once
/// while both sinks are live; under [`ParseOptions::lazy`] large content is a
/// [`Span`](crate::lazy::Span) handle, so the clone stays O(small).
///
/// Flow semantics: the stream continues until **both** sinks have said [`Flow::Stop`], and a sink
/// that stopped is never called again. (Stopping on the first `Stop` would silently truncate the
/// other consumer's view — e.g. a head-only sink ending event extraction mid-session.)
pub struct TeeSink<'a> {
    a: &'a mut dyn MessageSink,
    b: &'a mut dyn MessageSink,
    a_stopped: bool,
    b_stopped: bool,
}

impl<'a> TeeSink<'a> {
    pub fn new(a: &'a mut dyn MessageSink, b: &'a mut dyn MessageSink) -> Self {
        TeeSink { a, b, a_stopped: false, b_stopped: false }
    }
}

impl MessageSink for TeeSink<'_> {
    fn meta(&mut self, s: &Session) {
        self.a.meta(s);
        self.b.meta(s);
    }

    fn message(&mut self, m: Message) -> Flow {
        match (self.a_stopped, self.b_stopped) {
            (false, false) => {
                let copy = m.clone();
                self.a_stopped = self.a.message(m) == Flow::Stop;
                self.b_stopped = self.b.message(copy) == Flow::Stop;
            }
            (false, true) => self.a_stopped = self.a.message(m) == Flow::Stop,
            (true, false) => self.b_stopped = self.b.message(m) == Flow::Stop,
            (true, true) => {}
        }
        if self.a_stopped && self.b_stopped {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }
}

/// A sink that retains every message — the bridge back to a whole [`Session`]. See [`collect`].
#[derive(Default)]
pub struct CollectSink {
    pub messages: Vec<Message>,
}

impl MessageSink for CollectSink {
    fn message(&mut self, m: Message) -> Flow {
        self.messages.push(m);
        Flow::Continue
    }
}

/// Drive `adapter.stream` into a [`CollectSink`] and reattach the messages — i.e. fully parse `r`
/// at full fidelity. This is the default body of [`Adapter::parse`](crate::harness::Adapter::parse)
/// for adapters that implement a native `stream`.
pub fn collect(adapter: &dyn Adapter, r: &SessionRef) -> Result<Session> {
    let mut session = collect_with(adapter, r, &ParseOptions::full())?;
    // Full-fidelity parse: resolve any lazy content spans so the returned `Session` owns all its
    // content inline (safe for `--json` serialization, cross-harness emit, `Deref` reads, etc.).
    session.materialize();
    Ok(session)
}

/// Like [`collect`] but with explicit [`ParseOptions`] and **without** materializing — the returned
/// session may carry lazy content [`Span`]s. Consumers that hold the whole session but read it once
/// (e.g. dataset export) use this and resolve per block, so a giant field is never owned wholesale.
/// Use [`collect`] (or call `.materialize()`) when you need owned inline content.
pub fn collect_with(adapter: &dyn Adapter, r: &SessionRef, opts: &ParseOptions) -> Result<Session> {
    let mut sink = CollectSink::default();
    let mut session = adapter.stream(r, opts, &mut sink)?;
    session.messages = sink.messages;
    Ok(session)
}

/// Append a message's searchable text to `out` — the same projection [`Session::searchable_text`]
/// makes, but per-message so the bulk indexer can build a body incrementally and drop each message.
/// Thin alias for [`Message::append_searchable`], the single canonical projection.
pub fn append_searchable(out: &mut String, m: &Message) {
    m.append_searchable(out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Block, Role};

    fn text_msg(t: &str) -> Message {
        let mut m = Message::new(Role::User);
        m.content = vec![Block::Text { text: t.into() }];
        m
    }

    /// A sink that records the texts it saw and stops after `stop_after` messages.
    struct Probe {
        seen: Vec<String>,
        stop_after: usize,
    }
    impl MessageSink for Probe {
        fn message(&mut self, m: Message) -> Flow {
            self.seen.push(m.text().unwrap_or_default());
            if self.seen.len() >= self.stop_after {
                Flow::Stop
            } else {
                Flow::Continue
            }
        }
    }

    #[test]
    fn tee_feeds_both_until_both_stop_and_never_calls_a_stopped_sink() {
        let mut a = Probe { seen: Vec::new(), stop_after: 1 };
        let mut b = Probe { seen: Vec::new(), stop_after: 3 };
        let mut tee = TeeSink::new(&mut a, &mut b);
        // a stops after the first message; the tee keeps going for b until *it* stops.
        assert_eq!(tee.message(text_msg("one")), Flow::Continue);
        assert_eq!(tee.message(text_msg("two")), Flow::Continue);
        assert_eq!(tee.message(text_msg("three")), Flow::Stop);
        assert_eq!(a.seen, vec!["one"]);
        assert_eq!(b.seen, vec!["one", "two", "three"]);
    }
}
