//! Claude desktop app adapter (macOS, Electron wrapper around claude.ai).
//!
//! # What we found (reverse-engineered 2026-05-29 on macOS)
//!
//! Storage root: `~/Library/Application Support/Claude/`. It is a standard Electron/Chromium
//! profile (Local Storage, IndexedDB, Session Storage, Cookies, Cache, GPUCache, etc.).
//!
//! **Conversation transcripts are NOT stored locally.** The desktop app is a thin wrapper around
//! `claude.ai`; chat history lives server-side and is fetched over the network. An exhaustive scan
//! of every local store found:
//! - `Local Storage/leveldb/` — only auth tokens, account/org JSON, feature flags, UI flags
//!   (e.g. `sidebar-dot-seen`, `default-landing-applied`), and the Electron user-agent string.
//! - `IndexedDB/https_claude.ai_0.indexeddb.leveldb/` — a `keyval-store` holding tiny UI state such
//!   as `{"state":{"starredIds":[]},"version":0,...}`. No messages.
//! - `Session Storage/`, `shared_proto_db/`, `blob_storage/` — no transcript content.
//! - Searching all `*.json`/`*.ldb`/`*.log` for `"role":"assistant"` or assistant-style prose
//!   returned **zero** hits.
//!
//! The only local conversational artifact is `claude-code-sessions/<acct>/<workspace>/local_<uuid>.json`
//! — but those are **metadata stubs** for Claude *Code* CLI sessions the desktop app launched
//! (fields: `cliSessionId`, `cwd`, `title`, `model`, `createdAt`, `completedTurns`, ...). The actual
//! transcripts they point at live in `~/.claude/projects/` and are already handled by the
//! [`super::claude::Claude`] adapter via `cliSessionId`. Re-parsing them here would only duplicate
//! that adapter's output, so we deliberately do not.
//!
//! **Conclusion: not parseable for transcripts.** We still report `storage_root()` when the app dir
//! exists so installation is detectable, but `discover()` returns empty.

use super::Adapter;
use crate::ir::*;
use anyhow::Result;
use std::path::PathBuf;

pub struct ClaudeApp {
    root: Option<PathBuf>,
}

impl ClaudeApp {
    pub fn new() -> Self {
        ClaudeApp {
            root: Self::detect_root(),
        }
    }

    /// The Electron profile dir for the Claude desktop app, if it exists.
    fn detect_root() -> Option<PathBuf> {
        dirs::home_dir()
            .map(|h| {
                h.join("Library")
                    .join("Application Support")
                    .join("Claude")
            })
            .filter(|p| p.exists())
    }
}

impl Default for ClaudeApp {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for ClaudeApp {
    fn harness(&self) -> Harness {
        Harness::ClaudeApp
    }

    /// Returns the real app dir when installed (for installation detection), else `None`.
    fn storage_root(&self) -> Option<PathBuf> {
        self.root.clone()
    }

    /// No locally-stored transcripts exist (server-side only); always empty.
    fn discover(&self) -> Result<Vec<SessionRef>> {
        Ok(vec![])
    }

    fn parse(&self, _r: &SessionRef) -> Result<Session> {
        anyhow::bail!(
            "claude-app stores conversations server-side (claude.ai); no local transcripts to parse"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_is_claude_app() {
        assert_eq!(ClaudeApp::new().harness(), Harness::ClaudeApp);
    }

    #[test]
    fn discover_is_empty_server_side_only() {
        // There are no local transcripts regardless of whether the app is installed.
        let refs = ClaudeApp::new().discover().unwrap();
        assert!(refs.is_empty());
    }

    #[test]
    fn storage_root_matches_install_state() {
        // storage_root() should reflect actual on-disk presence of the app dir.
        let app = ClaudeApp::new();
        match app.storage_root() {
            Some(p) => {
                assert!(p.exists(), "reported root must exist: {}", p.display());
                assert!(p.ends_with("Claude"));
            }
            None => {
                // Not installed (or non-macOS): acceptable.
            }
        }
    }
}
