//! A persistent, content-addressed cache for the cheap metadata that `discover` produces.
//!
//! Discovery's expensive step is reading + JSON-parsing every transcript just to extract a handful
//! of metadata fields. But transcripts only change by being appended to, so a file whose
//! `(mtime, size)` is unchanged since last time has unchanged metadata. This cache keys on exactly
//! that: a hit returns the stored [`SessionRef`]s without touching the file, so steady-state
//! discovery costs one `stat` per file instead of a full read+parse.
//!
//! It's process-global and shared by `cv`, `cvd`, and the desktop app via a JSON file under the
//! user's cache dir. Writes are atomic (temp + rename) and reads are tolerant — a missing or corrupt
//! cache simply means everything is re-scanned (and the cache rebuilt). Correctness never depends on
//! the cache: an entry is only reused when `(mtime, size)` match exactly, so any real change misses.

use crate::ir::SessionRef;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct Entry {
    path: PathBuf,
    mtime_ns: u64,
    size: u64,
    refs: Vec<SessionRef>,
}

struct Cache {
    entries: RwLock<HashMap<PathBuf, Entry>>,
    dirty: AtomicBool,
}

static CACHE: OnceLock<Cache> = OnceLock::new();

fn cache_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("claurdvoyant").join("discover.json"))
}

fn load() -> Cache {
    let entries = cache_path()
        .and_then(|p| std::fs::read(&p).ok())
        .and_then(|b| serde_json::from_slice::<Vec<Entry>>(&b).ok())
        .map(|v| v.into_iter().map(|e| (e.path.clone(), e)).collect())
        .unwrap_or_default();
    Cache {
        entries: RwLock::new(entries),
        dirty: AtomicBool::new(false),
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
    // read the file) and caching them would bloat the file with every non-transcript path.
    if !refs.is_empty() {
        cache.entries.write().unwrap().insert(
            path.to_path_buf(),
            Entry {
                path: path.to_path_buf(),
                mtime_ns,
                size,
                refs: refs.clone(),
            },
        );
        cache.dirty.store(true, Ordering::Relaxed);
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

/// Flush the cache to disk if it changed this run, dropping entries whose files no longer exist so
/// it doesn't grow without bound. Cheap and a no-op when nothing was scanned. Call after a full
/// [`crate::discover_all`].
pub(crate) fn persist() {
    let Some(cache) = CACHE.get() else {
        return;
    };
    if !cache.dirty.swap(false, Ordering::Relaxed) {
        return;
    }
    let Some(path) = cache_path() else {
        return;
    };
    let snapshot: Vec<Entry> = {
        let mut g = cache.entries.write().unwrap();
        g.retain(|p, _| p.exists());
        g.values().cloned().collect()
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(body) = serde_json::to_vec(&snapshot) else {
        return;
    };
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&tmp, &body).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}
