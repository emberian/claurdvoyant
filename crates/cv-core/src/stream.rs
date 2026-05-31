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
#[derive(Debug, Clone, Default)]
pub struct ParseOptions {
    /// Populate the harness-specific `extra` maps — most importantly Claude's `toolUseResult`
    /// sidecar (structured patches, file contents, command stdout), which is `clone`d per tool
    /// turn and can dwarf the visible transcript. Bulk text consumers (index/search/dataset) never
    /// read it; only full-fidelity paths (json export, cross-harness convert/port, round-trips) do.
    pub extra: bool,
}

impl ParseOptions {
    /// Full fidelity: every harness-specific field materialized. Used by [`collect`] / `parse` and
    /// by conversion/round-trip paths that must lose nothing.
    pub fn full() -> Self {
        ParseOptions { extra: true }
    }

    /// The lean bulk-text pass: faithful message text, but no fat `extra` sidecars. Used by
    /// indexing, search, and dataset export.
    pub fn bulk() -> Self {
        ParseOptions::default()
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
    collect_with(adapter, r, &ParseOptions::full())
}

/// Like [`collect`] but with explicit [`ParseOptions`] — e.g. `ParseOptions::bulk()` to materialize
/// every message but skip the fat `extra` sidecars (for consumers like dataset export that need all
/// of one session's messages at once but never read `extra`).
pub fn collect_with(adapter: &dyn Adapter, r: &SessionRef, opts: &ParseOptions) -> Result<Session> {
    let mut sink = CollectSink::default();
    let mut session = adapter.stream(r, opts, &mut sink)?;
    session.messages = sink.messages;
    Ok(session)
}

/// Append a message's searchable text to `out` — the same projection [`Session::searchable_text`]
/// makes, but per-message so the bulk indexer can build a body incrementally and drop each message.
/// Kept in sync with `Session::searchable_text`.
pub fn append_searchable(out: &mut String, m: &Message) {
    use crate::ir::Block;
    for b in &m.content {
        match b {
            Block::Text { text } | Block::Thinking { text, .. } => {
                out.push_str(text);
                out.push('\n');
            }
            Block::ToolUse { name, input, .. } => {
                out.push_str(name);
                out.push(' ');
                out.push_str(&input.to_string());
                out.push('\n');
            }
            Block::ToolResult { content, .. } => {
                out.push_str(content);
                out.push('\n');
            }
            Block::File { path, source, .. } => {
                if let Some(p) = path.as_deref().or(source.as_deref()) {
                    out.push_str(p);
                    out.push('\n');
                }
            }
            Block::Image { .. } => {}
        }
    }
}
