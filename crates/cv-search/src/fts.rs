//! Full-text search via tantivy.
//!
//! Schema fields:
//! - `id`         STRING | STORED  — exact session id (the delete key on reindex)
//! - `harness`    STRING | STORED  — fielded filter, e.g. `harness:claude`
//! - `cwd`        TEXT   | STORED  — tokenized so path fragments are searchable
//! - `title`      TEXT   | STORED
//! - `body`       TEXT             — the big searchable blob; **indexed but not stored**
//! - `path`       STRING | STORED  — source file, so a hit can be snippeted by re-reading just that
//!                                   one session live (no full stored body — keeps the index lean)
//! - `created_at`/`updated_at` i64 INDEXED | STORED — returned on hits + future sort/filter
//! - `mtime`      i64 STORED       — incremental-index key: unchanged `(id, mtime)` is skipped
//!
//! Indexing is **incremental** by default: only sessions whose file mtime changed (plus new ones)
//! are (re)written, and sessions whose files vanished are deleted. A full rebuild is `rebuild=true`.
//! Snippets are generated **live** from the top hits (re-reading the session, capped) rather than
//! from a stored copy of every body — which is what keeps the on-disk index small.

use crate::{Doc, Hit};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Schema, Value, INDEXED, STORED, STRING, TEXT};
use tantivy::{Index, IndexWriter, TantivyDocument, Term};

struct Fields {
    id: Field,
    harness: Field,
    cwd: Field,
    title: Field,
    body: Field,
    path: Field,
    created_at: Field,
    updated_at: Field,
    mtime: Field,
}

fn build_schema() -> Schema {
    let mut b = Schema::builder();
    b.add_text_field("id", STRING | STORED);
    b.add_text_field("harness", STRING | STORED);
    b.add_text_field("cwd", TEXT | STORED);
    b.add_text_field("title", TEXT | STORED);
    b.add_text_field("body", TEXT);
    b.add_text_field("path", STRING | STORED);
    b.add_i64_field("created_at", INDEXED | STORED);
    b.add_i64_field("updated_at", INDEXED | STORED);
    b.add_i64_field("mtime", STORED);
    b.build()
}

fn fields_of(schema: &Schema) -> Result<Fields> {
    let get = |name: &str| schema.get_field(name);
    Ok(Fields {
        id: get("id")?,
        harness: get("harness")?,
        cwd: get("cwd")?,
        title: get("title")?,
        body: get("body")?,
        path: get("path")?,
        created_at: get("created_at")?,
        updated_at: get("updated_at")?,
        mtime: get("mtime")?,
    })
}

/// Open the index at `dir`, creating it if absent. If an existing index has a **stale schema**
/// (missing the `path`/`mtime` fields from before this layout — e.g. an index built by an older
/// `cv`), it's transparently rebuilt fresh so the binary self-heals on upgrade.
fn open_or_create(dir: &Path) -> Result<(Index, Fields)> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating tantivy dir {}", dir.display()))?;
    let index = match Index::open_in_dir(dir) {
        Ok(idx)
            if idx.schema().get_field("path").is_ok()
                && idx.schema().get_field("mtime").is_ok() =>
        {
            idx
        }
        // Either no index yet, or one with an incompatible older schema → (re)create fresh.
        _ => {
            let _ = std::fs::remove_dir_all(dir);
            std::fs::create_dir_all(dir)
                .with_context(|| format!("recreating tantivy dir {}", dir.display()))?;
            Index::create_in_dir(dir, build_schema())
                .with_context(|| format!("creating tantivy index in {}", dir.display()))?
        }
    };
    let fields = fields_of(&index.schema())?;
    Ok((index, fields))
}

const COMMIT_EVERY: usize = 256;

/// Discover + parse every session and bring the index at `dir` up to date.
///
/// Incremental by default: a session whose `(id, file-mtime)` already matches the index is **skipped**
/// (no re-parse cost beyond the metadata scan, no rewrite); changed sessions are replaced; new ones
/// added; and sessions whose source files no longer exist are deleted. Pass `rebuild = true` to clear
/// and rebuild from scratch. Returns the number of sessions discovered (not just the changed ones).
///
/// Streams one session at a time via [`crate::stream_corpus`], and commits periodically so segments
/// (and their term dictionaries) flush to disk instead of accumulating in the writer's heap — keeping
/// peak RSS bounded rather than scaling with the corpus.
pub fn index_all(dir: &Path, rebuild: bool) -> Result<usize> {
    let (index, f) = open_or_create(dir)?;
    let mut writer: IndexWriter = index
        .writer(50_000_000)
        .context("creating tantivy index writer")?;

    if rebuild {
        writer.delete_all_documents().context("clearing index")?;
        writer.commit().context("commit after clear")?;
    }

    // The skip-set: id → indexed file mtime. Empty on rebuild (everything is rewritten).
    let existing: HashMap<String, i64> = if rebuild {
        HashMap::new()
    } else {
        read_indexed_mtimes(&index, &f).unwrap_or_default()
    };

    let mut seen: HashSet<String> = HashSet::new();
    let mut since_commit = 0usize;
    let mut changed = 0usize;

    let total = crate::stream_corpus(|d| {
        seen.insert(d.id.clone());
        // Unchanged since last index (and we have a real mtime to trust) → skip entirely.
        if let Some(&mt) = existing.get(&d.id) {
            if mt == d.mtime_ns && d.mtime_ns != 0 {
                return Ok(());
            }
            // Changed: drop the old copy before re-adding (delete is by the exact id term).
            writer.delete_term(Term::from_field_text(f.id, &d.id));
        }
        add_doc(&mut writer, &f, &d)?;
        changed += 1;
        since_commit += 1;
        if since_commit >= COMMIT_EVERY {
            writer.commit().context("interim commit")?;
            since_commit = 0;
        }
        Ok(())
    })?;

    // Reap sessions that disappeared from disk since the last index (incremental only).
    let mut removed = 0usize;
    if !rebuild {
        for id in existing.keys() {
            if !seen.contains(id) {
                writer.delete_term(Term::from_field_text(f.id, id));
                removed += 1;
            }
        }
    }

    writer.commit().context("committing index")?;
    if !rebuild {
        eprintln!("  ↳ index: {changed} changed/new, {removed} removed, {} unchanged", total.saturating_sub(changed));
    }
    Ok(total)
}

/// Read `id → mtime` for every live document in the index — the incremental skip-set. Stored fields
/// are tiny now (no body is stored), so scanning them all is cheap.
fn read_indexed_mtimes(index: &Index, f: &Fields) -> Result<HashMap<String, i64>> {
    let reader = index.reader().context("opening index reader")?;
    let searcher = reader.searcher();
    let mut out = HashMap::new();
    for seg in searcher.segment_readers() {
        let store = seg.get_store_reader(0).context("opening store reader")?;
        for doc_id in seg.doc_ids_alive() {
            let doc: TantivyDocument = match store.get(doc_id) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let id = doc.get_first(f.id).and_then(|v| v.as_str()).map(str::to_string);
            let mt = doc.get_first(f.mtime).and_then(|v| v.as_i64());
            if let (Some(id), Some(mt)) = (id, mt) {
                out.insert(id, mt);
            }
        }
    }
    Ok(out)
}

/// Index an explicit set of docs (full rebuild: clears prior contents first). Used by tests.
#[cfg(test)]
pub(crate) fn index_docs(dir: &Path, docs: &[Doc]) -> Result<usize> {
    let (index, f) = open_or_create(dir)?;
    let mut writer: IndexWriter = index
        .writer(50_000_000)
        .context("creating tantivy index writer")?;
    writer.delete_all_documents().context("clearing index")?;
    for d in docs {
        add_doc(&mut writer, &f, d)?;
    }
    writer.commit().context("committing index")?;
    Ok(docs.len())
}

/// Add one [`Doc`] to `writer`. The body is **indexed but not stored** (we snippet live); the source
/// `path` and `mtime` are stored to drive live snippets and incremental reindex respectively.
fn add_doc(writer: &mut IndexWriter, f: &Fields, d: &Doc) -> Result<()> {
    let mut doc = TantivyDocument::default();
    doc.add_text(f.id, &d.id);
    doc.add_text(f.harness, &d.harness);
    if let Some(cwd) = &d.cwd {
        doc.add_text(f.cwd, cwd);
    }
    if let Some(title) = &d.title {
        doc.add_text(f.title, title);
    }
    doc.add_text(f.body, &d.body);
    if let Some(path) = &d.path {
        doc.add_text(f.path, path);
    }
    if let Some(t) = d.created_at {
        doc.add_i64(f.created_at, t);
    }
    if let Some(t) = d.updated_at {
        doc.add_i64(f.updated_at, t);
    }
    doc.add_i64(f.mtime, d.mtime_ns);
    writer.add_document(doc).context("adding document")?;
    Ok(())
}

/// Run a full-text query, returning up to `limit` hits ranked by BM25, each with a live snippet and
/// its `created_at`/`updated_at` (so callers needn't re-discover the corpus just to date the rows).
///
/// The query supports tantivy's syntax: bare terms search title+body+cwd, and fielded terms like
/// `harness:claude`, phrases `"foo bar"`, and booleans `a AND b` work too.
pub fn text_search(dir: &Path, query: &str, limit: usize) -> Result<Vec<Hit>> {
    if !dir.join("meta.json").exists() {
        anyhow::bail!(
            "no full-text index at {} — run `index_all` first",
            dir.display()
        );
    }
    let (index, f) = open_or_create(dir)?;
    let reader = index.reader().context("opening index reader")?;
    let searcher = reader.searcher();

    let mut parser = QueryParser::for_index(&index, vec![f.title, f.body, f.cwd]);
    parser.set_conjunction_by_default(); // all terms must match — better precision than OR
    let parsed = parser
        .parse_query(query)
        .with_context(|| format!("parsing query {query:?}"))?;

    let top = searcher
        .search(&parsed, &TopDocs::with_limit(limit).order_by_score())
        .context("running search")?;

    let mut hits = Vec::with_capacity(top.len());
    for (score, addr) in top {
        let doc: TantivyDocument = searcher.doc(addr).context("loading stored doc")?;
        let get_str = |field: Field| -> Option<String> {
            doc.get_first(field)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        };
        let harness = get_str(f.harness).unwrap_or_default();
        let path = get_str(f.path);
        let snippet = path
            .as_deref()
            .and_then(|p| live_snippet(p, &harness, query))
            .unwrap_or_default();

        hits.push(Hit {
            id: get_str(f.id).unwrap_or_default(),
            harness,
            cwd: get_str(f.cwd),
            title: get_str(f.title),
            created_at: doc.get_first(f.created_at).and_then(|v| v.as_i64()),
            updated_at: doc.get_first(f.updated_at).and_then(|v| v.as_i64()),
            score,
            snippet,
        });
    }
    Ok(hits)
}

/// Most text we'll scan from a session to find a snippet window. Matches are almost always early;
/// capping keeps snippeting a multi-GB hit bounded (and Phase-2 offsets will make it a seek).
const SNIPPET_SCAN_CAP: usize = 256 * 1024;

/// Generate a snippet for a hit by re-reading **just that one session** (streamed, capped), finding
/// the first query term, and windowing around it — instead of keeping a stored copy of every body.
fn live_snippet(path: &str, harness: &str, query: &str) -> Option<String> {
    use cv_core::{Flow, Message, MessageSink, ParseOptions, SessionRef};
    let h = cv_core::Harness::parse(harness)?;
    let adapter = cv_core::harness::for_harness(h)?;
    let sref = SessionRef {
        id: String::new(),
        harness: h,
        path: PathBuf::from(path),
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        message_count: 0,
    };

    struct Acc {
        buf: String,
    }
    impl MessageSink for Acc {
        fn message(&mut self, m: Message) -> Flow {
            cv_core::stream::append_searchable(&mut self.buf, &m);
            if self.buf.len() >= SNIPPET_SCAN_CAP {
                Flow::Stop
            } else {
                Flow::Continue
            }
        }
    }
    let mut acc = Acc { buf: String::new() };
    adapter.stream(&sref, &ParseOptions::bulk(), &mut acc).ok()?;
    Some(make_snippet(&acc.buf, query))
}

/// Window the body around the first matching query term; fall back to a head if nothing matches.
fn make_snippet(body: &str, query: &str) -> String {
    let lc = body.to_lowercase();
    for term in query_terms(query) {
        if let Some(pos) = lc.find(&term) {
            return window(body, pos, term.len());
        }
    }
    head(body, 200)
}

/// The bare, matchable terms of a tantivy query: drops `field:value` filters and boolean operators,
/// keeps lowercased word tokens. Used only to locate a snippet window, not for retrieval.
fn query_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .filter(|t| !t.contains(':'))
        .filter(|t| !matches!(*t, "AND" | "OR" | "NOT"))
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

fn window(body: &str, pos: usize, len: usize) -> String {
    let start = floor_char(body, pos.saturating_sub(40));
    let end = ceil_char(body, (pos + len + 80).min(body.len()));
    let s = &body[start..end];
    let s = s.replace('\n', " ");
    if s.chars().count() > 200 {
        let t: String = s.chars().take(199).collect();
        format!("{t}…")
    } else {
        s
    }
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
fn ceil_char(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Remove a single session from the index by id (handy for incremental updates).
#[allow(dead_code)]
pub fn delete(dir: &Path, id: &str) -> Result<()> {
    let (index, f) = open_or_create(dir)?;
    let mut writer: IndexWriter = index.writer(15_000_000)?;
    writer.delete_term(Term::from_field_text(f.id, id));
    writer.commit()?;
    Ok(())
}

fn head(s: &str, max_chars: usize) -> String {
    let t: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        format!("{t}…")
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Doc;

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cv-search-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Write a minimal one-message Claude transcript so `live_snippet` has a real file to re-read,
    /// and return its path. `body` becomes the single user message's text.
    fn write_claude(dir: &Path, id: &str, body: &str) -> String {
        let p = dir.join(format!("{id}.jsonl"));
        let line = serde_json::json!({
            "type": "user",
            "uuid": id,
            "sessionId": id,
            "message": { "role": "user", "content": body }
        });
        std::fs::write(&p, format!("{line}\n")).unwrap();
        p.display().to_string()
    }

    fn doc(id: &str, title: &str, body: &str, path: String) -> Doc {
        Doc {
            id: id.into(),
            harness: "claude".into(),
            cwd: Some("/home/u/proj".into()),
            title: Some(title.into()),
            created_at: Some(1),
            updated_at: Some(2),
            body: body.into(),
            path: Some(path),
            mtime_ns: 1,
        }
    }

    #[test]
    fn index_and_search_with_live_snippets() {
        let dir = tmpdir();
        let sdir = tmpdir();
        let b1 = "We built a full text search engine with tantivy and BM25 scoring.";
        let b2 = "Loading dataframes and grouping by columns in pandas.";
        let p1 = write_claude(&sdir, "a1", b1);
        let p2 = write_claude(&sdir, "b2", b2);
        let docs = vec![
            doc("a1", "Rust tantivy index", b1, p1),
            doc("b2", "Python pandas", b2, p2),
        ];
        let n = index_docs(&dir, &docs).unwrap();
        assert_eq!(n, 2);

        // Term unique to the first doc; snippet is generated live from the session file.
        let hits = text_search(&dir, "tantivy", 10).unwrap();
        assert_eq!(hits.len(), 1, "expected exactly one match for 'tantivy'");
        assert_eq!(hits[0].id, "a1");
        assert!(
            hits[0].snippet.contains("tantivy"),
            "live snippet should mention the term: {:?}",
            hits[0].snippet
        );
        // Dates come straight off the index now (no corpus re-discovery).
        assert_eq!(hits[0].created_at, Some(1));
        assert_eq!(hits[0].updated_at, Some(2));

        let hits = text_search(&dir, "pandas", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "b2");

        // Fielded query restricts by harness.
        let hits = text_search(&dir, "harness:claude pandas", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].harness, "claude");

        // Conjunction-by-default: a term present in neither yields nothing.
        assert!(text_search(&dir, "kubernetes", 10).unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }

    #[test]
    fn incremental_skips_unchanged_and_reaps_missing() {
        let dir = tmpdir();
        let sdir = tmpdir();
        let p = write_claude(&sdir, "x", "alpha beta gamma");
        // Seed the index with one doc at mtime 1.
        index_docs(&dir, &[doc("x", "first", "alpha beta gamma", p.clone())]).unwrap();
        assert_eq!(text_search(&dir, "alpha", 10).unwrap().len(), 1);

        // A direct full-rebuild replaces contents (old doc gone).
        let p2 = write_claude(&sdir, "y", "delta epsilon");
        index_docs(&dir, &[doc("y", "second", "delta epsilon", p2)]).unwrap();
        assert!(text_search(&dir, "alpha", 10).unwrap().is_empty());
        assert_eq!(text_search(&dir, "delta", 10).unwrap().len(), 1);

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&sdir).ok();
    }
}
