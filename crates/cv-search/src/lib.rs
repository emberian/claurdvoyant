//! # cv-search
//!
//! Pure-Rust search over claurdvoyant sessions, in two flavours:
//!
//! 1. **Full-text** via [`tantivy`] (a Lucene-like inverted index) — `index_all` builds a
//!    persistent index, `text_search` queries it and returns highlighted snippets. This is a
//!    strict upgrade over a SQLite FTS5 index: real tokenization, BM25 scoring, fielded queries
//!    (`harness:claude foo`), phrase/boolean operators, and fast incremental commits.
//!
//! 2. **Semantic** via [`model2vec-rs`] (distilled *static* embeddings — tiny, fast, no ONNX
//!    runtime). `embed_all` embeds every session and persists the vectors; `semantic_search`
//!    embeds the query and does a brute-force cosine ranking. For ~1k sessions an in-memory
//!    `Vec<(id, Vec<f32>)>` scan is plenty — no ANN crate needed. Gated behind the `semantic`
//!    cargo feature (default-on, since model2vec is lightweight); a minimal FTS-only build is
//!    `cargo build -p cv-search --no-default-features`.
//!
//! Both indexes live under `$CLAURDVOYANT_HOME` (or `~/.claurdvoyant`): the tantivy index in
//! `…/tantivy/` and the embedding store in `…/embeddings.json`.

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
    /// A representative, highlighted (for FTS) excerpt of the matching text.
    pub snippet: String,
}

/// Root directory for cv-search's on-disk state: `$CLAURDVOYANT_HOME` or `~/.claurdvoyant`.
pub fn home_dir() -> PathBuf {
    std::env::var_os("CLAURDVOYANT_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".claurdvoyant")))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Default location of the tantivy full-text index.
pub fn default_tantivy_dir() -> PathBuf {
    home_dir().join("tantivy")
}

/// Default location of the semantic embedding store.
pub fn default_embeddings_path() -> PathBuf {
    home_dir().join("embeddings.json")
}

// ---- Convenience wrappers over the default-located indexes ---------------------------------

/// Discover, parse, and (re)build the full-text index at its default location.
/// Returns the number of sessions indexed.
pub fn index_all(dir: impl Into<Option<PathBuf>>) -> Result<usize> {
    let dir = dir.into().unwrap_or_else(default_tantivy_dir);
    fts::index_all(&dir)
}

/// Run a full-text query against the index at its default location.
pub fn text_search(
    dir: impl Into<Option<PathBuf>>,
    query: &str,
    limit: usize,
) -> Result<Vec<Hit>> {
    let dir = dir.into().unwrap_or_else(default_tantivy_dir);
    fts::text_search(&dir, query, limit)
}

/// Discover, parse, and embed every session, persisting vectors to the default store.
#[cfg(feature = "semantic")]
pub fn embed_all(path: impl Into<Option<PathBuf>>) -> Result<usize> {
    let path = path.into().unwrap_or_else(default_embeddings_path);
    semantic::embed_all(&path)
}

/// Embed the query and rank stored session vectors by cosine similarity (default store).
#[cfg(feature = "semantic")]
pub fn semantic_search(
    path: impl Into<Option<PathBuf>>,
    query: &str,
    k: usize,
) -> Result<Vec<Hit>> {
    let path = path.into().unwrap_or_else(default_embeddings_path);
    semantic::semantic_search(&path, query, k)
}

/// Stream the `(id, harness, cwd, title, body)` corpus by discovering + parsing every session,
/// invoking `f` once per session and **dropping the parsed `Session` + `Doc` immediately after**.
///
/// Peak memory is O(one session), NOT O(corpus). The previous `build_corpus() -> Vec<Doc>` held
/// every session's searchable body in a single `Vec` (~35 GB on a real machine) and the semantic
/// path then `.clone()`d them all again — which ballooned `cv` to ~65 GB RSS and got it OOM-killed
/// (taking down the terminal-sibling process). Both indexers now consume this streaming driver, so
/// they still see exactly the same searchable text but never hold the whole corpus at once.
/// Shared by both indexers so they see exactly the same searchable text.
pub(crate) fn stream_corpus(mut f: impl FnMut(Doc) -> Result<()>) -> Result<usize> {
    let mut n = 0usize;
    for r in cv_core::discover_all() {
        let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
            continue;
        };
        let session = match adapter.parse(&r) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cv-search: parse failed for {} ({}): {e:#}", r.id, r.harness);
                continue;
            }
        };
        let doc = Doc {
            id: r.id.clone(),
            harness: r.harness.as_str().to_string(),
            cwd: r.cwd.as_ref().map(|p| p.display().to_string()),
            title: Some(session.label()),
            created_at: r.created_at.map(|t| t.timestamp()),
            updated_at: r.updated_at.map(|t| t.timestamp()),
            body: session.searchable_text(),
        };
        drop(session); // free the parsed transcript before handing the doc downstream
        f(doc)?;
        n += 1;
    }
    Ok(n)
}

/// One indexable document distilled from a parsed [`cv_core::Session`].
#[derive(Debug, Clone)]
pub(crate) struct Doc {
    pub id: String,
    pub harness: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub body: String,
}
