//! The on-disk archive: a per-harness directory of serialized IR sessions plus an idempotent
//! JSONL catalog. The archive is the durable, harness-agnostic store that `cvd` populates so a
//! user can centralize agent logs from many machines/harnesses for search and safekeeping.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use cv_core::{Harness, Session, SessionRef};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// One row of the catalog (`catalog.jsonl`). Cheap metadata for listings/search; the full
/// transcript lives in the per-session JSON under `archive/<harness>/<id>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub harness: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub message_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    pub archived_at: DateTime<Utc>,
    /// Path to the serialized session JSON, relative to the archive home.
    pub path: PathBuf,
}

impl CatalogEntry {
    fn key(&self) -> (String, String) {
        (self.harness.clone(), self.id.clone())
    }
}

/// Whether a `store` call actually wrote (the session was new/changed) or was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOutcome {
    Archived,
    Skipped,
}

/// Handle to the archive root (`$CLUSTERVISION_HOME` or `~/.clustervision`, overridable by `--home`).
pub struct Archive {
    home: PathBuf,
}

impl Archive {
    /// Resolve the archive home: explicit `--home`, else `$CLUSTERVISION_HOME`, else `~/.clustervision`.
    pub fn resolve(explicit: Option<PathBuf>) -> Result<Self> {
        let home = if let Some(p) = explicit {
            p
        } else if let Some(env) = std::env::var_os("CLUSTERVISION_HOME") {
            PathBuf::from(env)
        } else {
            let base = dirs::home_dir().context("could not determine home directory")?;
            base.join(".clustervision")
        };
        Ok(Archive { home })
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    fn catalog_path(&self) -> PathBuf {
        self.home.join("catalog.jsonl")
    }

    /// Relative (to home) path where a session's JSON lives.
    fn rel_session_path(harness: Harness, id: &str) -> PathBuf {
        PathBuf::from("archive")
            .join(harness.as_str())
            .join(format!("{id}.json"))
    }

    fn session_path(&self, harness: Harness, id: &str) -> PathBuf {
        self.home.join(Self::rel_session_path(harness, id))
    }

    /// Persist a session to `archive/<harness>/<id>.json` and update the catalog. Idempotent:
    /// if the serialized bytes are byte-identical to what's already on disk, returns `Skipped`
    /// without rewriting anything.
    pub fn store(&self, session: &Session) -> Result<StoreOutcome> {
        let json = serde_json::to_vec_pretty(session).with_context(|| format!("serializing session {}", session.id))?;

        let dest = self.session_path(session.harness, &session.id);
        if let Ok(existing) = fs::read(&dest) {
            if existing == json {
                return Ok(StoreOutcome::Skipped);
            }
        }

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::write(&dest, &json).with_context(|| format!("writing {}", dest.display()))?;

        let entry = CatalogEntry {
            harness: session.harness.as_str().to_string(),
            id: session.id.clone(),
            cwd: session.cwd.clone(),
            title: session.title.clone().or_else(|| Some(session.label())),
            message_count: session.messages.len(),
            created_at: session.created_at,
            updated_at: session.updated_at,
            archived_at: Utc::now(),
            path: Self::rel_session_path(session.harness, &session.id),
        };
        self.upsert_catalog(entry)?;
        Ok(StoreOutcome::Archived)
    }

    /// Read the catalog, deduped by (harness, id) with the newest `archived_at` winning.
    pub fn load_catalog(&self) -> Result<Vec<CatalogEntry>> {
        let path = self.catalog_path();
        let mut map: BTreeMap<(String, String), CatalogEntry> = BTreeMap::new();
        let raw = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<CatalogEntry>(line) {
                Ok(entry) => {
                    let key = entry.key();
                    match map.get(&key) {
                        Some(prev) if prev.archived_at >= entry.archived_at => {}
                        _ => {
                            map.insert(key, entry);
                        }
                    }
                }
                Err(e) => eprintln!("cvd: skipping corrupt catalog line: {e}"),
            }
        }
        Ok(map.into_values().collect())
    }

    /// Record an entry by **appending** one line — O(1) per archive instead of rewriting the whole
    /// catalog every time (which made N archives O(N²)). `load_catalog` already dedups by key with
    /// the newest `archived_at` winning, so superseded lines are harmless until we compact. When the
    /// raw line count grows well past the number of unique entries, rewrite once to reclaim space.
    fn upsert_catalog(&self, entry: CatalogEntry) -> Result<()> {
        let path = self.catalog_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let line = serde_json::to_string(&entry)?;
        {
            let mut f = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("opening {} for append", path.display()))?;
            writeln!(f, "{line}")?;
            f.flush()?;
        }
        self.maybe_compact_catalog()?;
        Ok(())
    }

    /// Rewrite the catalog to one line per unique entry, but only when the on-disk line count has
    /// drifted far enough past the unique count to be worth the full rewrite (amortizes to O(1)).
    fn maybe_compact_catalog(&self) -> Result<()> {
        let path = self.catalog_path();
        let raw = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let raw_lines = raw.lines().filter(|l| !l.trim().is_empty()).count();
        let entries = self.load_catalog()?;
        // Compact only once we're carrying ~2x dead weight (and past a small floor).
        if raw_lines < 64 || raw_lines < entries.len() * 2 {
            return Ok(());
        }
        let tmp = path.with_extension(format!("{}.jsonl.tmp", std::process::id()));
        {
            let mut f = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
            for e in &entries {
                writeln!(f, "{}", serde_json::to_string(e)?)?;
            }
            f.flush()?;
        }
        fs::rename(&tmp, &path).with_context(|| format!("finalizing {}", path.display()))?;
        Ok(())
    }
}

/// Best-effort cwd rendering for logs/listings, abbreviating `$HOME` to `~`.
pub fn pretty_cwd(cwd: &Option<PathBuf>) -> String {
    let Some(p) = cwd else {
        return "-".to_string();
    };
    if let Some(home) = dirs::home_dir() {
        if let Ok(rest) = p.strip_prefix(&home) {
            if rest.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rest.display());
        }
    }
    p.display().to_string()
}

/// Short label for a discovered ref (for watch logs).
pub fn ref_cwd(r: &SessionRef) -> String {
    pretty_cwd(&r.cwd)
}
