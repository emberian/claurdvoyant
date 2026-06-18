//! A persistent, content-addressed cache for the cheap metadata that `discover` produces.
//!
//! Discovery's expensive step is reading + JSON-parsing every transcript just to extract a handful
//! of metadata fields. But transcripts only change by being appended to, so a file whose
//! `(mtime, size)` is unchanged since last time has unchanged metadata. This cache keys on exactly
//! that: a hit returns the stored [`SessionRef`]s without touching the file, so steady-state
//! discovery costs one `stat` per file instead of a full read+parse.
//!
//! It's process-global and shared by `cv`, `cvd`, and the desktop app via the `scan_cache` table in
//! the shared catalog db (see [`crate::catalog`], which owns the DDL; the rows also feed the
//! freshness probe's append detection). Earlier releases kept a whole-loaded `discover.json`
//! instead — a second, redundant representation of the same facts that every command paid to parse;
//! [`load`] leaves a "safe to delete" note if one is still lying around. Reads are tolerant — a
//! missing or unreadable cache simply means everything is re-scanned (and the cache rebuilt).
//! Correctness never depends on the cache: an entry is only reused when `(mtime, size)` match
//! exactly, so any real change misses.

use crate::ir::SessionRef;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};

struct Entry {
    mtime_ns: u64,
    size: u64,
    refs: Vec<SessionRef>,
    /// Scanned this run and not yet written back to sqlite.
    dirty: bool,
}

struct Cache {
    entries: RwLock<HashMap<PathBuf, Entry>>,
    any_dirty: AtomicBool,
}

static CACHE: OnceLock<Cache> = OnceLock::new();

/// One-time nudge about the pre-0.9.12 JSON cache this module replaced.
fn note_legacy_json() {
    let Some(path) = dirs::cache_dir().map(|d| d.join("claurdvoyant").join("discover.json")) else {
        return;
    };
    if path.exists() {
        eprintln!(
            "cv: note: stale discover.json no longer used (the scan cache lives in catalog.db now); safe to delete {}",
            path.display()
        );
    }
}

/// Bulk-load the persisted scan cache. Only ever runs on the discovery (slow) path — probe-only
/// commands never touch this module. Unparseable rows are simply dropped (re-scanned, re-written).
fn load() -> Cache {
    note_legacy_json();
    #[allow(unused_mut)]
    let mut entries: HashMap<PathBuf, Entry> = HashMap::new();
    #[cfg(feature = "sqlite")]
    if let Some(conn) = crate::catalog::open_db() {
        if let Ok(mut stmt) = conn.prepare("SELECT path, mtime_ns, size, refs FROM scan_cache") {
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            });
            if let Ok(rows) = rows {
                for (path, mtime_ns, size, refs) in rows.flatten() {
                    let Ok(refs) = serde_json::from_str::<Vec<SessionRef>>(&refs) else {
                        continue;
                    };
                    entries.insert(
                        PathBuf::from(path),
                        Entry {
                            mtime_ns: mtime_ns as u64,
                            size: size as u64,
                            refs,
                            dirty: false,
                        },
                    );
                }
            }
        }
    }
    Cache {
        entries: RwLock::new(entries),
        any_dirty: AtomicBool::new(false),
    }
}

/// `(mtime_ns, size)` for `path`, or `None` if it can't be stat'd (then callers just scan).
fn stat_key(path: &Path) -> Option<(u64, u64)> {
    let m = std::fs::metadata(path).ok()?;
    let mtime = m
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos() as u64;
    Some((mtime, m.len()))
}

/// Return cached refs for `path` when its `(mtime, size)` is unchanged; otherwise run `scan`, cache
/// any non-empty result, and return it. One file may map to several sessions (e.g. Gemini logs).
///
/// The stat happens **before** the scan, so a write racing the scan leaves a too-old `(mtime, size)`
/// in the cache — which misses next time and re-scans. The safe direction.
pub(crate) fn cached_scan_many<F>(path: &Path, scan: F) -> Vec<SessionRef>
where
    F: FnOnce() -> Vec<SessionRef>,
{
    let Some((mtime_ns, size)) = stat_key(path) else {
        return scan();
    };
    let cache = CACHE.get_or_init(load);
    if let Some(e) = cache.entries.read().unwrap().get(path) {
        if e.mtime_ns == mtime_ns && e.size == size {
            return e.refs.clone();
        }
    }
    let refs = scan();
    // Only cache hits — empty results are cheap to recompute (a failed name/content check that never
    // read the file) and caching them would bloat the table with every non-transcript path.
    if !refs.is_empty() {
        cache.entries.write().unwrap().insert(
            path.to_path_buf(),
            Entry {
                mtime_ns,
                size,
                refs: refs.clone(),
                dirty: true,
            },
        );
        cache.any_dirty.store(true, Ordering::Relaxed);
    }
    refs
}

/// Single-result convenience over [`cached_scan_many`].
pub(crate) fn cached_scan<F>(path: &Path, scan: F) -> Option<SessionRef>
where
    F: FnOnce() -> Option<SessionRef>,
{
    cached_scan_many(path, || scan().into_iter().collect())
        .into_iter()
        .next()
}

/// Flush freshly-scanned entries to the catalog db in one transaction. With `prune`, also drop
/// rows whose files no longer exist so the table doesn't grow without bound — pass it after a
/// *full* [`crate::discover_all`] (the stat-per-row cost belongs on the slow path; per-harness
/// probe refreshes skip it). Best-effort: a failed flush just means a re-scan next run.
pub(crate) fn persist(prune: bool) {
    let Some(cache) = CACHE.get() else {
        return;
    };
    if !cache.any_dirty.swap(false, Ordering::Relaxed) && !prune {
        return;
    }
    #[cfg(feature = "sqlite")]
    {
        let Some(mut conn) = crate::catalog::open_db() else {
            return;
        };
        let Ok(tx) = conn.transaction() else { return };
        let mut g = cache.entries.write().unwrap();
        {
            let Ok(mut stmt) =
                tx.prepare("INSERT OR REPLACE INTO scan_cache(path,mtime_ns,size,refs) VALUES(?1,?2,?3,?4)")
            else {
                return;
            };
            for (path, e) in g.iter_mut().filter(|(_, e)| e.dirty) {
                let Ok(refs) = serde_json::to_string(&e.refs) else {
                    continue;
                };
                if stmt
                    .execute(rusqlite::params![
                        path.to_string_lossy(),
                        e.mtime_ns as i64,
                        e.size as i64,
                        refs
                    ])
                    .is_ok()
                {
                    e.dirty = false;
                }
            }
        }
        if prune {
            let vanished: Vec<String> = tx
                .prepare("SELECT path FROM scan_cache")
                .ok()
                .and_then(|mut stmt| {
                    stmt.query_map([], |r| r.get::<_, String>(0))
                        .ok()
                        .map(|rows| rows.flatten().filter(|p| !Path::new(p).exists()).collect())
                })
                .unwrap_or_default();
            if let Ok(mut del) = tx.prepare("DELETE FROM scan_cache WHERE path=?1") {
                for p in &vanished {
                    let _ = del.execute(rusqlite::params![p]);
                    g.remove(Path::new(p));
                }
            }
        }
        drop(g);
        let _ = tx.commit();
    }
}
