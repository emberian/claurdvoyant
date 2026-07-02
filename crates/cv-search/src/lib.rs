//! # cv-search
//!
//! Pure-Rust search over clustervision sessions, in two flavours:
//!
//! 1. **Full-text** via [`tantivy`] (a Lucene-like inverted index) — `index_all` builds a
//!    persistent index, `text_search` queries it and returns highlighted snippets. This is a
//!    strict upgrade over a SQLite FTS5 index: real tokenization, BM25 scoring, fielded queries
//!    (`harness:claude foo`), phrase/boolean operators, and fast incremental commits.
//!
//! 2. **Semantic** via [`model2vec-rs`] (distilled *static* embeddings — tiny, fast, no ONNX
//!    runtime). `embed_all` cuts every session into overlapping ~3 KiB windows and embeds each
//!    (incrementally — unchanged sessions keep their vectors); `semantic_search` embeds the query
//!    and does a brute-force cosine ranking over all window vectors, deduped to the best window
//!    per session. Even at ~100k vectors an in-memory scan is plenty — no ANN crate needed. Gated
//!    behind the `semantic` cargo feature (default-on, since model2vec is lightweight); a minimal
//!    FTS-only build is `cargo build -p cv-search --no-default-features`.
//!
//! Both indexes live under `$CLUSTERVISION_HOME` (or `~/.clustervision`): the tantivy index in
//! `…/tantivy/` and the embedding store in `…/embeddings.bin`.

use anyhow::Result;
use std::path::PathBuf;

pub mod fts;
#[cfg(feature = "semantic")]
pub mod semantic;

/// A single search result, shared by full-text and semantic search.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Hit {
    pub id: String,
    pub harness: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    /// Relevance score (BM25 for FTS, cosine similarity for semantic).
    pub score: f32,
    /// A representative excerpt of the matching text (generated live from the hit session for FTS).
    pub snippet: String,
    /// Unix timestamps carried straight off the FTS index / embedding store, so callers can date
    /// rows without re-discovering the corpus. `None` when the session itself had no parseable
    /// dates, or for semantic hits from an embedding store written before dates were recorded
    /// (re-run `embed_all` to backfill).
    #[serde(default)]
    pub created_at: Option<i64>,
    #[serde(default)]
    pub updated_at: Option<i64>,
    /// Sub-agent provenance: when this hit is a folded-in sub-agent transcript (indexed with
    /// `cv index --subagents`), the top-level session that spawned it, the agent's own id, and the
    /// workflow run it belonged to (if any). All `None` for an ordinary top-level session, and for
    /// any hit from an index built before subagent folding (the fields default-absent on the
    /// stored doc and on deserialization of older serialized hits).
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub workflow: Option<String>,
}

/// Root directory for cv-search's on-disk state: `$CLUSTERVISION_HOME` or `~/.clustervision`.
///
/// Back-compat: the project was once "claurdvoyant", so we still honor `$CLAURDVOYANT_HOME` and an
/// existing `~/.claurdvoyant` (kept only if the new dir hasn't been created) — so a previously-built
/// index/embedding store isn't orphaned by the rename.
pub fn home_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("CLUSTERVISION_HOME").or_else(|| std::env::var_os("CLAURDVOYANT_HOME")) {
        return PathBuf::from(v);
    }
    let Some(home) = dirs::home_dir() else {
        return PathBuf::from(".");
    };
    let new = home.join(".clustervision");
    let legacy = home.join(".claurdvoyant");
    if !new.exists() && legacy.exists() {
        legacy
    } else {
        new
    }
}

/// Default location of the tantivy full-text index.
pub fn default_tantivy_dir() -> PathBuf {
    home_dir().join("tantivy")
}

/// Default location of the semantic embedding store (binary; see `semantic` module docs).
pub fn default_embeddings_path() -> PathBuf {
    home_dir().join("embeddings.bin")
}

// ---- Convenience wrappers over the default-located indexes ---------------------------------

/// Discover, parse, and update the full-text index at its default location. Incremental by default
/// (only changed/new sessions are rewritten; vanished ones are reaped); `rebuild = true` clears and
/// rebuilds from scratch. Returns the number of sessions discovered.
pub fn index_all(dir: impl Into<Option<PathBuf>>, rebuild: bool, subagents: bool) -> Result<usize> {
    let dir = dir.into().unwrap_or_else(default_tantivy_dir);
    fts::index_all(&dir, rebuild, subagents)
}

/// Run a full-text query against the index at its default location.
pub fn text_search(dir: impl Into<Option<PathBuf>>, query: &str, limit: usize) -> Result<Vec<Hit>> {
    let dir = dir.into().unwrap_or_else(default_tantivy_dir);
    fts::text_search(&dir, query, limit)
}

/// Discover, window, and embed sessions into the default store — incrementally: only changed/new
/// sessions are re-embedded, vanished ones dropped. Equivalent to [`embed_all_with`] without a
/// rebuild (signature kept for existing callers).
#[cfg(feature = "semantic")]
pub fn embed_all(path: impl Into<Option<PathBuf>>) -> Result<usize> {
    embed_all_with(path, false)
}

/// [`embed_all`], with `rebuild = true` forcing a full re-embed of every session.
#[cfg(feature = "semantic")]
pub fn embed_all_with(path: impl Into<Option<PathBuf>>, rebuild: bool) -> Result<usize> {
    let path = path.into().unwrap_or_else(default_embeddings_path);
    semantic::embed_all(&path, rebuild)
}

/// Embed the query and rank stored window vectors by cosine similarity (default store), deduped
/// to the best window per session.
#[cfg(feature = "semantic")]
pub fn semantic_search(path: impl Into<Option<PathBuf>>, query: &str, k: usize) -> Result<Vec<Hit>> {
    let path = path.into().unwrap_or_else(default_embeddings_path);
    semantic::semantic_search(&path, query, k)
}

#[cfg(feature = "semantic")]
pub use semantic::SemanticHit;

/// [`semantic_search`], but each hit also carries **where** in the session the best-matching
/// window starts (message index) — for `cv search --semantic`'s location display.
#[cfg(feature = "semantic")]
pub fn semantic_search_located(path: impl Into<Option<PathBuf>>, query: &str, k: usize) -> Result<Vec<SemanticHit>> {
    let path = path.into().unwrap_or_else(default_embeddings_path);
    semantic::semantic_search_located(&path, query, k)
}

/// Stream-parse one session into a [`Doc`] for the **semantic** path: per-message searchable text
/// pieces (for the window cutter), plus label/dates off the ref. `None` (with a logged warning)
/// on a parse failure, so the caller simply leaves the session out of the store until a later run
/// parses it whole.
///
/// Peak memory is O([`semantic::SESSION_TEXT_BUDGET`]), NOT O(session) or O(corpus). History:
/// `build_corpus() -> Vec<Doc>` held every session's full searchable body (~35 GB) and the
/// semantic path `.clone()`d them all again — ~65 GB RSS, OOM-killed. Streaming one session at a
/// time fixed the corpus dimension; the per-session budget (sized to what [`semantic`]'s window
/// cap can embed anyway) fixes the per-session one.
#[cfg(feature = "semantic")]
pub(crate) fn doc_for_ref(r: &cv_core::SessionRef) -> Option<Doc> {
    use cv_core::{Block, Flow, Message, MessageSink, ParseOptions, Role};

    /// Raw bytes of a lazy span resolved while hunting the first user text — the label only ever
    /// reads its first 72 chars ([`cv_core::label_from`]), so 512 bytes is plenty.
    const FIRST_USER_HEAD: u64 = 512;

    /// Collects one `(message index, searchable text)` piece per message, dropping each message
    /// immediately. Streams under [`ParseOptions::lazy`]: large content arrives as spans and is
    /// resolved only up to the remaining session budget, so a giant field is never materialized.
    /// Stops the parse as soon as the budget is spent and a first-user label candidate was seen.
    struct PieceSink {
        pieces: Vec<(u32, String)>,
        idx: u32,
        total: usize,
        truncated: bool,
        first_user_text: Option<String>,
        resolver: cv_core::Resolver,
    }
    impl MessageSink for PieceSink {
        fn message(&mut self, m: Message) -> Flow {
            let idx = self.idx;
            self.idx += 1;
            if self.first_user_text.is_none() && m.role == Role::User {
                // Same projection as `Message::text()` (text blocks joined by '\n'), span-aware.
                let mut s = String::new();
                for b in &m.content {
                    if let Block::Text { text } = b {
                        if !s.is_empty() {
                            s.push('\n');
                        }
                        match text.as_span() {
                            Some(sp) => s.push_str(&self.resolver.resolve_prefix(sp, FIRST_USER_HEAD)),
                            None => {
                                if let Some(t) = text.inline_str() {
                                    s.push_str(t);
                                }
                            }
                        }
                    }
                }
                if !s.trim().is_empty() {
                    self.first_user_text = Some(s);
                }
            }
            let budget = semantic::SESSION_TEXT_BUDGET;
            if self.total < budget {
                let mut text = String::new();
                self.append_searchable_capped(&mut text, &m, budget - self.total);
                if !text.trim().is_empty() {
                    self.total += text.len();
                    self.pieces.push((idx, text));
                }
            } else {
                self.truncated = true; // content past the budget exists and goes unwindowed
            }
            if self.total >= budget && self.first_user_text.is_some() {
                Flow::Stop
            } else {
                Flow::Continue
            }
        }
    }
    impl PieceSink {
        /// Append one content text to `out`: inline as-is, a span resolved to at most `remaining`.
        fn push_text(&mut self, out: &mut String, text: &cv_core::Text, remaining: usize) {
            if let Some(sp) = text.as_span() {
                let left = remaining.saturating_sub(out.len()) as u64;
                if left > 0 {
                    out.push_str(&self.resolver.resolve_prefix(sp, left));
                }
            } else if let Some(s) = text.inline_str() {
                out.push_str(s);
            }
        }

        /// The `cv_core::stream::append_searchable` projection into `out`, span-aware and capped
        /// at `remaining` bytes (a big inline block is truncated on a char boundary).
        fn append_searchable_capped(&mut self, out: &mut String, m: &Message, remaining: usize) {
            use std::fmt::Write as _;
            for b in &m.content {
                if out.len() >= remaining {
                    break;
                }
                match b {
                    Block::Text { text } | Block::Thinking { text, .. } => {
                        self.push_text(out, text, remaining);
                        out.push('\n');
                    }
                    Block::ToolUse { name, input, .. } => {
                        out.push_str(name);
                        out.push(' ');
                        let _ = write!(out, "{input}");
                        out.push('\n');
                    }
                    Block::ToolResult { content, .. } => {
                        self.push_text(out, content, remaining);
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
            if out.len() > remaining {
                let mut cut = remaining;
                while cut > 0 && !out.is_char_boundary(cut) {
                    cut -= 1;
                }
                out.truncate(cut);
                self.truncated = true;
            }
        }
    }

    let adapter = cv_core::harness::for_harness(r.harness)?;
    let mut sink = PieceSink {
        pieces: Vec::new(),
        idx: 0,
        total: 0,
        truncated: false,
        first_user_text: None,
        resolver: cv_core::Resolver::new(Some(r.path.clone())),
    };
    let meta = match adapter.stream(r, &ParseOptions::lazy(), &mut sink) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("cv-search: parse failed for {} ({}): {e:#}", r.id, r.harness);
            return None;
        }
    };
    let title = cv_core::label_from(meta.title.as_deref(), sink.first_user_text.as_deref());
    Some(Doc {
        id: r.id.clone(),
        harness: r.harness.as_str().to_string(),
        cwd: r.cwd.as_ref().map(|p| p.display().to_string()),
        title: Some(title),
        path: r.path.display().to_string(),
        pieces: sink.pieces,
        truncated: sink.truncated,
        created_at: r.created_at.map(|t| t.timestamp()),
        updated_at: r.updated_at.map(|t| t.timestamp()),
    })
}

/// One session distilled for the **semantic** path ([`semantic::embed_all`]): per-message text
/// pieces ready for the window cutter, plus display metadata. The FTS path doesn't go through
/// `Doc`: it streams straight into `fts::ChunkSink`, which reads dates/path/mtime off the
/// [`cv_core::SessionRef`] instead (so they aren't duplicated here).
#[cfg(feature = "semantic")]
#[derive(Debug, Clone)]
pub(crate) struct Doc {
    pub id: String,
    pub harness: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    /// Source file, persisted so query-time snippets can re-read the matched window's messages.
    pub path: String,
    /// `(0-based message index, searchable text)` per non-empty message, in stream order —
    /// [`semantic`]'s window cutter maps window offsets back to these indices.
    pub pieces: Vec<(u32, String)>,
    /// The per-session text budget cut the parse short (the embedded windows cover a prefix).
    pub truncated: bool,
    /// Unix-seconds session dates off the discovery [`cv_core::SessionRef`], so semantic hits
    /// carry `created_at`/`updated_at` just like FTS hits do.
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
}
