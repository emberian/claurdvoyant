//! A SQLite FTS5 index over every discovered session, so `cv search` is instant instead of
//! parsing every transcript on each query. (sqlite feature only.)
//!
//! OWNED BY the index work. Contract below is stable; `cv-core::lib` gates this behind `sqlite`.

use crate::ir::{Harness, SessionRef};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension};
use std::path::PathBuf;

/// Bump when the schema changes incompatibly; `open_or_create` will drop+rebuild on mismatch.
const SCHEMA_VERSION: i64 = 1;

/// Default on-disk location for the index: `$CLAURDVOYANT_HOME/index.sqlite` or `~/.claurdvoyant/`.
pub fn default_index_path() -> PathBuf {
    let home = std::env::var_os("CLAURDVOYANT_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".claurdvoyant")))
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("index.sqlite")
}

/// One search hit from the index.
#[derive(Debug, Clone)]
pub struct IndexHit {
    pub id: String,
    pub harness: Harness,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
    pub snippet: String,
}

/// The persistent search index.
pub struct Index {
    #[allow(dead_code)]
    path: PathBuf,
    conn: Connection,
}

impl Index {
    /// Open (creating + migrating the schema if needed) the index at `path`.
    pub fn open_or_create(path: PathBuf) -> Result<Index> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating index dir {}", parent.display()))?;
            }
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("opening index db {}", path.display()))?;
        // Reasonable durability/concurrency defaults; a locked or corrupt db surfaces as Err below.
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("setting journal_mode")?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context("setting synchronous")?;
        let idx = Index { path, conn };
        idx.migrate()?;
        Ok(idx)
    }

    /// Ensure the schema exists and matches `SCHEMA_VERSION`; rebuild it from scratch on mismatch.
    fn migrate(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
            )
            .context("creating meta table")?;
        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("reading schema_version")?
            .and_then(|s| s.parse().ok());

        if existing != Some(SCHEMA_VERSION) {
            // Drop any prior (incompatible or absent) schema and recreate.
            self.conn
                .execute_batch(
                    "DROP TABLE IF EXISTS sessions_fts;
                     DROP TABLE IF EXISTS sessions;",
                )
                .context("dropping old schema")?;
            self.create_schema()?;
            self.conn
                .execute(
                    "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    [SCHEMA_VERSION.to_string()],
                )
                .context("recording schema_version")?;
        }
        Ok(())
    }

    fn create_schema(&self) -> Result<()> {
        // `sessions` holds metadata + the row key. The FTS5 table is an *external content* table
        // pointed at `sessions` so we don't duplicate the (potentially large) text twice; we keep
        // the title/content columns in `sessions` for `content=` to read from.
        self.conn
            .execute_batch(
                "CREATE TABLE sessions (
                    rowid         INTEGER PRIMARY KEY,
                    harness       TEXT NOT NULL,
                    sid           TEXT NOT NULL,
                    cwd           TEXT,
                    title         TEXT,
                    content       TEXT NOT NULL,
                    created_at    TEXT,
                    updated_at    TEXT,
                    message_count INTEGER NOT NULL DEFAULT 0,
                    source_path   TEXT,
                    content_hash  INTEGER NOT NULL,
                    UNIQUE(harness, sid)
                );
                CREATE VIRTUAL TABLE sessions_fts USING fts5(
                    title,
                    content,
                    content='sessions',
                    content_rowid='rowid',
                    tokenize = 'unicode61 remove_diacritics 2'
                );
                -- Keep the FTS index in sync with the content table via triggers.
                CREATE TRIGGER sessions_ai AFTER INSERT ON sessions BEGIN
                    INSERT INTO sessions_fts(rowid, title, content)
                    VALUES (new.rowid, new.title, new.content);
                END;
                CREATE TRIGGER sessions_ad AFTER DELETE ON sessions BEGIN
                    INSERT INTO sessions_fts(sessions_fts, rowid, title, content)
                    VALUES ('delete', old.rowid, old.title, old.content);
                END;
                CREATE TRIGGER sessions_au AFTER UPDATE ON sessions BEGIN
                    INSERT INTO sessions_fts(sessions_fts, rowid, title, content)
                    VALUES ('delete', old.rowid, old.title, old.content);
                    INSERT INTO sessions_fts(rowid, title, content)
                    VALUES (new.rowid, new.title, new.content);
                END;",
            )
            .context("creating sessions schema")?;
        Ok(())
    }

    /// Re-scan all harnesses and (re)index their sessions. Returns the number indexed.
    pub fn rebuild(&self) -> Result<usize> {
        let refs = crate::discover_all();
        let tx = self.conn.unchecked_transaction()?;
        let mut count = 0usize;
        for r in &refs {
            let adapter = match crate::harness::for_harness(r.harness) {
                Some(a) => a,
                None => continue,
            };
            let session = match adapter.parse(r) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("cv index: skip {} {}: {e:#}", r.harness, r.id);
                    continue;
                }
            };
            let content = session.searchable_text();
            let title = session.label();
            upsert_row(&tx, r, &title, &content)?;
            count += 1;
        }
        tx.commit().context("committing rebuild")?;
        Ok(count)
    }

    /// Incrementally index/refresh a single discovered session. Idempotent: replaces the existing
    /// row keyed by (harness, id). Skips the write if content is unchanged.
    pub fn upsert(&self, r: &SessionRef) -> Result<()> {
        let adapter = crate::harness::for_harness(r.harness)
            .with_context(|| format!("no adapter for harness {}", r.harness))?;
        let session = adapter
            .parse(r)
            .with_context(|| format!("parsing {} {}", r.harness, r.id))?;
        let content = session.searchable_text();
        let title = session.label();
        let tx = self.conn.unchecked_transaction()?;
        upsert_row(&tx, r, &title, &content)?;
        tx.commit().context("committing upsert")?;
        Ok(())
    }

    /// Full-text search the index, newest-first within relevance.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<IndexHit>> {
        let match_expr = match sanitize_query(query) {
            Some(m) => m,
            None => return Ok(Vec::new()),
        };
        let mut stmt = self
            .conn
            .prepare(
                "SELECT s.sid, s.harness, s.cwd, s.title, s.updated_at,
                        snippet(sessions_fts, 1, '[', ']', ' … ', 12) AS snip
                 FROM sessions_fts
                 JOIN sessions s ON s.rowid = sessions_fts.rowid
                 WHERE sessions_fts MATCH ?1
                 ORDER BY bm25(sessions_fts), s.updated_at DESC
                 LIMIT ?2",
            )
            .context("preparing search statement")?;
        let limit = if limit == 0 { i64::MAX } else { limit as i64 };
        let rows = stmt
            .query_map(rusqlite::params![match_expr, limit], |row| {
                let harness_s: String = row.get(1)?;
                let updated_s: Option<String> = row.get(4)?;
                Ok(IndexHit {
                    id: row.get(0)?,
                    harness: Harness::parse(&harness_s).unwrap_or(Harness::Claude),
                    cwd: row.get(2)?,
                    title: row.get(3)?,
                    updated_at: updated_s
                        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                        .map(|d| d.with_timezone(&Utc)),
                    snippet: row.get(5)?,
                })
            })
            .context("running search query")?;
        let mut hits = Vec::new();
        for r in rows {
            hits.push(r.context("reading search row")?);
        }
        Ok(hits)
    }
}

/// Insert-or-replace a session row. Skips the (expensive) write when the content hash is unchanged.
fn upsert_row(conn: &Connection, r: &SessionRef, title: &str, content: &str) -> Result<()> {
    let harness = r.harness.as_str();
    let hash = content_hash(content) as i64;

    // Skip unchanged rows so a re-index is cheap.
    let prior: Option<i64> = conn
        .query_row(
            "SELECT content_hash FROM sessions WHERE harness = ?1 AND sid = ?2",
            rusqlite::params![harness, r.id],
            |row| row.get(0),
        )
        .optional()
        .context("checking prior hash")?;
    if prior == Some(hash) {
        return Ok(());
    }

    let cwd = r.cwd.as_ref().map(|p| p.to_string_lossy().into_owned());
    let source_path = r.path.to_string_lossy().into_owned();
    let created = r.created_at.map(|d| d.to_rfc3339());
    let updated = r.updated_at.map(|d| d.to_rfc3339());

    // Replace by (harness, sid). DELETE first so the FTS delete trigger fires with the old text
    // (external-content FTS needs the old content to remove the right index entries), then INSERT.
    conn.execute(
        "DELETE FROM sessions WHERE harness = ?1 AND sid = ?2",
        rusqlite::params![harness, r.id],
    )
    .context("deleting prior row")?;
    conn.execute(
        "INSERT INTO sessions
            (harness, sid, cwd, title, content, created_at, updated_at, message_count, source_path, content_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            harness,
            r.id,
            cwd,
            title,
            content,
            created,
            updated,
            r.message_count as i64,
            source_path,
            hash,
        ],
    )
    .context("inserting session row")?;
    Ok(())
}

/// 64-bit FNV-1a over the content — cheap change-detection so `upsert` can skip unchanged sessions.
fn content_hash(content: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in content.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Turn arbitrary user input into a safe FTS5 MATCH expression.
///
/// FTS5 treats `"`, `*`, `(`, `:`, `-`, etc. as syntax and will error on malformed queries, so we
/// don't pass raw input through. We split on whitespace, keep only alphanumeric runs from each
/// token, wrap each surviving token as a double-quoted phrase (which neutralizes all special
/// characters), append `*` for prefix matching, and AND them together. Returns `None` if nothing
/// searchable remains.
fn sanitize_query(query: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    for raw in query.split_whitespace() {
        // Strip everything that isn't alphanumeric (keeps unicode letters/digits).
        let cleaned: String = raw.chars().filter(|c| c.is_alphanumeric()).collect();
        if cleaned.is_empty() {
            continue;
        }
        // Quote as a phrase to escape, then prefix-match the final token of each.
        terms.push(format!("\"{cleaned}\"*"));
    }
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" AND "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db_path() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cv-index-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p.push("index.sqlite");
        p
    }

    /// Insert a synthetic row directly (bypassing harness parsing) to test the FTS plumbing.
    fn insert_synthetic(
        idx: &Index,
        harness: Harness,
        id: &str,
        title: &str,
        content: &str,
    ) {
        let r = SessionRef {
            id: id.to_string(),
            harness,
            path: PathBuf::from(format!("/tmp/{id}.jsonl")),
            cwd: Some(PathBuf::from("/home/user/project")),
            title: Some(title.to_string()),
            created_at: None,
            updated_at: Some(Utc::now()),
            message_count: 3,
        };
        let tx = idx.conn.unchecked_transaction().unwrap();
        upsert_row(&tx, &r, title, content).unwrap();
        tx.commit().unwrap();
    }

    #[test]
    fn open_create_and_search() {
        let path = temp_db_path();
        let idx = Index::open_or_create(path.clone()).unwrap();

        insert_synthetic(
            &idx,
            Harness::Claude,
            "sess-alpha",
            "Refactor the parser",
            "We rewrote the recursive descent parser and fixed a tokenizer bug.",
        );
        insert_synthetic(
            &idx,
            Harness::Codex,
            "sess-beta",
            "Database migration",
            "Added an FTS5 sqlite index and a meta table for schema versioning.",
        );

        // Term that only appears in alpha.
        let hits = idx.search("tokenizer", 10).unwrap();
        assert_eq!(hits.len(), 1, "expected one tokenizer hit, got {hits:?}");
        assert_eq!(hits[0].id, "sess-alpha");
        assert!(!hits[0].snippet.is_empty());

        // Term that only appears in beta.
        let hits = idx.search("sqlite", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].harness, Harness::Codex);

        // Prefix match: "pars" should hit alpha via "parser".
        let hits = idx.search("pars", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "sess-alpha");

        // Punctuation-heavy input must not error: FTS5 syntax chars get stripped/escaped. Here the
        // surviving terms ("parser", "tokenizer") both occur in alpha, so it still matches.
        let hits = idx
            .search("  \"parser\"* AND (tokenizer): --  ", 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "sess-alpha");

        // A bare query with FTS operators must not error even if it matches nothing.
        assert!(idx.search("DROP TABLE sessions", 10).is_ok());

        // Pure-punctuation / empty input yields no results, no error.
        assert!(idx.search("", 10).unwrap().is_empty());
        assert!(idx.search("()*:-", 10).unwrap().is_empty());
    }

    #[test]
    fn upsert_is_idempotent_and_updates() {
        let path = temp_db_path();
        let idx = Index::open_or_create(path).unwrap();

        insert_synthetic(&idx, Harness::Claude, "s1", "First", "alpha bravo charlie");
        // Re-insert same id with different content -> replaces, doesn't duplicate.
        insert_synthetic(&idx, Harness::Claude, "s1", "First", "delta echo foxtrot");

        let n: i64 = idx
            .conn
            .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "upsert should replace, not duplicate");

        // Old content is gone from the index, new content is searchable.
        assert!(idx.search("alpha", 10).unwrap().is_empty());
        assert_eq!(idx.search("foxtrot", 10).unwrap().len(), 1);
    }
}
