//! A persistent SQLite catalog of discovered sessions: `id → (harness, path, metadata)`.
//!
//! [`crate::find`] used to resolve one session id by calling `discover()` on **every** adapter and
//! scanning the whole fleet — O(all sessions across all harnesses) for a single `cv show <id>`. The
//! catalog turns that into an indexed lookup: [`crate::discover_all`] upserts every ref here, and
//! `find` consults it first (O(1)/O(log n)), falling back to a full scan only on a miss (which then
//! re-warms the catalog).
//!
//! Best-effort throughout: any sqlite error is swallowed and the caller falls back to scanning, so
//! correctness never depends on the catalog (a stale row is filtered by `path.exists()` at lookup).
//! Behind the `sqlite` feature (default-on); a no-op `lookup`/`sync` keep `find` working without it.

use crate::ir::{Harness, SessionRef};
#[cfg(feature = "sqlite")]
use std::path::PathBuf;

#[cfg(feature = "sqlite")]
mod imp {
    use super::*;
    use rusqlite::{params, Connection};
    use std::time::Duration;

    fn db_path() -> Option<PathBuf> {
        let dir = std::env::var_os("CLAURDVOYANT_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::cache_dir().map(|d| d.join("claurdvoyant")))?;
        let _ = std::fs::create_dir_all(&dir);
        Some(dir.join("catalog.db"))
    }

    fn open() -> Option<Connection> {
        let conn = Connection::open(db_path()?).ok()?;
        // WAL + a busy timeout so concurrent cv/cvd processes don't error each other out.
        conn.busy_timeout(Duration::from_secs(5)).ok()?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS sessions(
                 harness       TEXT NOT NULL,
                 id            TEXT NOT NULL,
                 path          TEXT NOT NULL,
                 cwd           TEXT,
                 title         TEXT,
                 created_at    INTEGER,
                 updated_at    INTEGER,
                 message_count INTEGER,
                 PRIMARY KEY(harness, id, path)
             );
             CREATE INDEX IF NOT EXISTS idx_sessions_id ON sessions(id);",
        )
        .ok()?;
        Some(conn)
    }

    /// Upsert every ref in one transaction. Stale rows (sessions later deleted on disk) linger but
    /// are harmless — `lookup` filters by `path.exists()`. Cheap (~one INSERT OR REPLACE per ref).
    pub fn sync(refs: &[SessionRef]) {
        let Some(mut conn) = open() else { return };
        let Ok(tx) = conn.transaction() else { return };
        {
            let Ok(mut stmt) = tx.prepare(
                "INSERT OR REPLACE INTO sessions
                 (harness,id,path,cwd,title,created_at,updated_at,message_count)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            ) else {
                return;
            };
            for r in refs {
                let _ = stmt.execute(params![
                    r.harness.as_str(),
                    r.id,
                    r.path.to_string_lossy(),
                    r.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
                    r.title,
                    r.created_at.map(|t| t.timestamp()),
                    r.updated_at.map(|t| t.timestamp()),
                    r.message_count as i64,
                ]);
            }
        }
        let _ = tx.commit();
    }

    /// All catalog rows whose id equals `id` or starts with it, optionally constrained to one
    /// harness. Order: exact-id matches first. Empty on any error or a cold catalog.
    pub fn lookup(id: &str, harness: Option<Harness>) -> Vec<SessionRef> {
        let Some(conn) = open() else { return Vec::new() };
        let like = format!("{id}%");
        let Ok(mut stmt) = conn.prepare(
            "SELECT harness,id,path,cwd,title,created_at,updated_at,message_count
             FROM sessions WHERE id=?1 OR id LIKE ?2
             ORDER BY (id=?1) DESC",
        ) else {
            return Vec::new();
        };
        let rows = stmt.query_map(params![id, like], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, i64>(7)?,
            ))
        });
        let Ok(rows) = rows else { return Vec::new() };
        let mut out = Vec::new();
        for row in rows.flatten() {
            let Some(harn) = Harness::parse(&row.0) else { continue };
            if let Some(want) = harness {
                if harn != want {
                    continue;
                }
            }
            out.push(SessionRef {
                harness: harn,
                id: row.1,
                path: PathBuf::from(row.2),
                cwd: row.3.map(PathBuf::from),
                title: row.4,
                created_at: row.5.and_then(|t| chrono::DateTime::from_timestamp(t, 0)),
                updated_at: row.6.and_then(|t| chrono::DateTime::from_timestamp(t, 0)),
                message_count: row.7 as usize,
            });
        }
        out
    }
}

#[cfg(feature = "sqlite")]
pub use imp::{lookup, sync};

/// No-op fallbacks when sqlite is disabled — `find` simply always takes its scan path.
#[cfg(not(feature = "sqlite"))]
pub fn sync(_refs: &[SessionRef]) {}
#[cfg(not(feature = "sqlite"))]
pub fn lookup(_id: &str, _harness: Option<Harness>) -> Vec<SessionRef> {
    Vec::new()
}
