//! Cross-process advisory file lock shared by the append-only stores (board channels, task log).
//!
//! RAII guard for an OS `flock`-style **exclusive lock** on a dedicated `.lock` file. Closing the
//! file (drop) releases the lock, and the kernel releases it on process death too — so a crashed
//! holder can never wedge the store, and there is no stale-lock steal path (a `remove_file` +
//! re-create dance could crown two winners and cascade-steal a *live* holder's lock). The lockfile
//! itself is never removed: unlinking it would let the next locker open a fresh inode and lock
//! *that*, silently breaking mutual exclusion.

use std::fs::{File, OpenOptions};
use std::path::PathBuf;

use anyhow::{Context, Result};
#[cfg(not(target_family = "wasm"))]
use fs4::fs_std::FileExt;

pub(crate) struct FileLock {
    _file: File,
}

impl FileLock {
    /// Acquire the lock, blocking until the current holder (if any) releases it.
    pub(crate) fn acquire(path: PathBuf) -> Result<FileLock> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("creating lockfile {}", path.display()))?;
        // wasm32 has no file locking (and no cross-process concurrency): the open alone suffices.
        #[cfg(not(target_family = "wasm"))]
        file.lock_exclusive()
            .with_context(|| format!("locking lockfile {}", path.display()))?;
        Ok(FileLock { _file: file })
    }
}
