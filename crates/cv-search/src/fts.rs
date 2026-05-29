//! Full-text search via tantivy.
//!
//! Schema fields:
//! - `id`        STRING | STORED  — exact session id (used as the delete key on reindex)
//! - `harness`   STRING | STORED  — fielded filter, e.g. `harness:claude`
//! - `cwd`       TEXT   | STORED  — tokenized so path fragments are searchable
//! - `title`     TEXT   | STORED
//! - `body`      TEXT            — the big searchable blob; not stored (we snippet from a re-read
//!                                  copy stored separately to keep the index lean)
//! - `body_store` STORED        — a stored copy of the body for snippet generation
//! - `created_at`/`updated_at` i64 fast fields for future sorting/filtering.

use crate::{Doc, Hit};
use anyhow::{Context, Result};
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Schema, Value, INDEXED, STORED, STRING, TEXT};
use tantivy::snippet::SnippetGenerator;
use tantivy::{Index, IndexWriter, TantivyDocument, Term};

struct Fields {
    id: Field,
    harness: Field,
    cwd: Field,
    title: Field,
    body: Field,
    body_store: Field,
    created_at: Field,
    updated_at: Field,
}

fn schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let id = b.add_text_field("id", STRING | STORED);
    let harness = b.add_text_field("harness", STRING | STORED);
    let cwd = b.add_text_field("cwd", TEXT | STORED);
    let title = b.add_text_field("title", TEXT | STORED);
    let body = b.add_text_field("body", TEXT);
    let body_store = b.add_text_field("body_store", STORED);
    let created_at = b.add_i64_field("created_at", INDEXED | STORED);
    let updated_at = b.add_i64_field("updated_at", INDEXED | STORED);
    let schema = b.build();
    (
        schema,
        Fields {
            id,
            harness,
            cwd,
            title,
            body,
            body_store,
            created_at,
            updated_at,
        },
    )
}

fn open_or_create(dir: &Path) -> Result<(Index, Fields)> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating tantivy dir {}", dir.display()))?;
    let (schema, fields) = schema();
    // `open_in_dir` reuses an existing index (and its on-disk schema); otherwise create fresh.
    let index = match Index::open_in_dir(dir) {
        Ok(idx) => idx,
        Err(_) => Index::create_in_dir(dir, schema)
            .with_context(|| format!("creating tantivy index in {}", dir.display()))?,
    };
    Ok((index, fields))
}

/// Discover + parse every session, then (re)build the full-text index at `dir`.
pub fn index_all(dir: &Path) -> Result<usize> {
    let docs = crate::build_corpus();
    index_docs(dir, &docs)
}

/// Index an explicit set of docs (full rebuild: clears prior contents first).
pub(crate) fn index_docs(dir: &Path, docs: &[Doc]) -> Result<usize> {
    let (index, f) = open_or_create(dir)?;
    let mut writer: IndexWriter = index
        .writer(50_000_000)
        .context("creating tantivy index writer")?;
    // Full rebuild semantics: drop everything, then re-add. Simple and correct for a CLI reindex.
    writer.delete_all_documents().context("clearing index")?;
    for d in docs {
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
        doc.add_text(f.body_store, &d.body);
        if let Some(t) = d.created_at {
            doc.add_i64(f.created_at, t);
        }
        if let Some(t) = d.updated_at {
            doc.add_i64(f.updated_at, t);
        }
        writer.add_document(doc).context("adding document")?;
    }
    writer.commit().context("committing index")?;
    Ok(docs.len())
}

/// Run a full-text query, returning up to `limit` highlighted hits ranked by BM25.
///
/// The query supports tantivy's syntax: bare terms search title+body+cwd, and fielded terms
/// like `harness:claude`, phrases `"foo bar"`, and booleans `a AND b` work too.
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

    // Default fields for bare terms; `harness:` etc. still resolve explicitly.
    let mut parser = QueryParser::for_index(&index, vec![f.title, f.body, f.cwd]);
    parser.set_conjunction_by_default(); // all terms must match — better precision than OR
    let parsed = parser
        .parse_query(query)
        .with_context(|| format!("parsing query {query:?}"))?;

    let top = searcher
        .search(&parsed, &TopDocs::with_limit(limit).order_by_score())
        .context("running search")?;

    // Snippet over the stored body; falls back to a head of the body if no fragment is found.
    let snippet_gen = SnippetGenerator::create(&searcher, &*parsed, f.body_store).ok();

    let mut hits = Vec::with_capacity(top.len());
    for (score, addr) in top {
        let doc: TantivyDocument = searcher.doc(addr).context("loading stored doc")?;
        let get_str = |field: Field| -> Option<String> {
            doc.get_first(field)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        };
        let body = get_str(f.body_store).unwrap_or_default();
        let snippet = snippet_gen
            .as_ref()
            .map(|g| g.snippet_from_doc(&doc).to_html())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| head(&body, 200));

        hits.push(Hit {
            id: get_str(f.id).unwrap_or_default(),
            harness: get_str(f.harness).unwrap_or_default(),
            cwd: get_str(f.cwd),
            title: get_str(f.title),
            score,
            snippet,
        });
    }
    Ok(hits)
}

/// Remove a single session from the index by id (handy for incremental updates later).
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

    fn doc(id: &str, harness: &str, title: &str, body: &str) -> Doc {
        Doc {
            id: id.into(),
            harness: harness.into(),
            cwd: Some("/home/u/proj".into()),
            title: Some(title.into()),
            created_at: Some(1),
            updated_at: Some(2),
            body: body.into(),
        }
    }

    #[test]
    fn index_and_search_finds_docs() {
        let dir = tmpdir();
        let docs = vec![
            doc("a1", "claude", "Rust tantivy index", "We built a full text search engine with tantivy and BM25 scoring."),
            doc("b2", "codex", "Python pandas", "Loading dataframes and grouping by columns in pandas."),
        ];
        let n = index_docs(&dir, &docs).unwrap();
        assert_eq!(n, 2);

        // Term that appears only in the first doc.
        let hits = text_search(&dir, "tantivy", 10).unwrap();
        assert_eq!(hits.len(), 1, "expected exactly one match for 'tantivy'");
        assert_eq!(hits[0].id, "a1");
        assert!(hits[0].snippet.contains("tantivy"), "snippet should mention the term: {}", hits[0].snippet);

        // Term in the second doc.
        let hits = text_search(&dir, "pandas", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "b2");

        // Fielded query restricts by harness.
        let hits = text_search(&dir, "harness:codex pandas", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].harness, "codex");

        // Conjunction-by-default: a term present in neither yields nothing.
        let hits = text_search(&dir, "kubernetes", 10).unwrap();
        assert!(hits.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reindex_replaces_contents() {
        let dir = tmpdir();
        index_docs(&dir, &[doc("x", "claude", "first", "alpha beta gamma")]).unwrap();
        // Rebuild with different content; old doc must be gone.
        index_docs(&dir, &[doc("y", "claude", "second", "delta epsilon")]).unwrap();
        assert!(text_search(&dir, "alpha", 10).unwrap().is_empty());
        assert_eq!(text_search(&dir, "delta", 10).unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
