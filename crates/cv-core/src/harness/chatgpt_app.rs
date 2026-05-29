//! ChatGPT desktop app adapter (macOS, native AppKit/Swift — *not* Electron).
//!
//! # What we found (reverse-engineered 2026-05-29 on macOS)
//!
//! Storage root: `~/Library/Application Support/com.openai.chat/`. (The app is not sandboxed in a
//! `~/Library/Containers/com.openai.chat/` data container; only `com.openai.chat.Widgets` exists
//! there.) The store is organized as per-feature, per-account directories whose names embed the
//! account id, e.g.:
//! - `conversations-v3-<accountId>/<conversationId>.data`  ← one file per conversation
//! - `drafts-v2-<accountId>/`, `gizmos-<accountId>/`, `models-<accountId>/`,
//!   `system-hints-<accountId>/response.data`, `project-g-p-<id>/`, `pinned-items-...`, etc.
//!
//! So the app **does** keep conversation history locally (offline history), one `.data` file per
//! conversation, named by the conversation UUID.
//!
//! **However, every `.data` file is encrypted.** `file(1)` reports `data`; the bytes are
//! high-entropy ciphertext (a 9 KB conversation yields ~2 readable >=8-char strings, a 7 MB one
//! yields ~36 — i.e. coincidental byte runs, not text). There is no plaintext index, no JSON, and
//! no SQLite alongside them. The decryption key is not stored in a user-readable Keychain item
//! (`security find-generic-password -s com.openai.chat` → not found; no `openai`/`chatgpt` entries
//! in the dumpable keychain), consistent with the app holding the key in its own protected
//! keychain access group / data-protection class.
//!
//! **Conclusion: history is stored locally but encrypted at rest with an app-held key → not
//! parseable.** We report `storage_root()` when the app dir exists (installation is detectable and
//! the encrypted conversation count is observable), but `discover()` returns empty because we
//! cannot decrypt the contents.

use super::Adapter;
use crate::ir::*;
use anyhow::Result;
use std::path::PathBuf;

pub struct ChatGptApp {
    root: Option<PathBuf>,
}

impl ChatGptApp {
    pub fn new() -> Self {
        ChatGptApp {
            root: Self::detect_root(),
        }
    }

    /// The app's Application Support dir, if it exists.
    fn detect_root() -> Option<PathBuf> {
        dirs::home_dir()
            .map(|h| {
                h.join("Library")
                    .join("Application Support")
                    .join("com.openai.chat")
            })
            .filter(|p| p.exists())
    }
}

impl Default for ChatGptApp {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for ChatGptApp {
    fn harness(&self) -> Harness {
        Harness::ChatGptApp
    }

    /// Returns the real app dir when installed (for installation detection), else `None`.
    fn storage_root(&self) -> Option<PathBuf> {
        self.root.clone()
    }

    /// Local history exists but is encrypted at rest; we cannot enumerate readable sessions.
    fn discover(&self) -> Result<Vec<SessionRef>> {
        Ok(vec![])
    }

    fn parse(&self, _r: &SessionRef) -> Result<Session> {
        anyhow::bail!(
            "chatgpt-app stores conversations locally but encrypted at rest \
             (conversations-v3-<acct>/*.data); decryption key is app-held, not parseable"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_is_chatgpt_app() {
        assert_eq!(ChatGptApp::new().harness(), Harness::ChatGptApp);
    }

    #[test]
    fn discover_is_empty_encrypted_at_rest() {
        let refs = ChatGptApp::new().discover().unwrap();
        assert!(refs.is_empty());
    }

    #[test]
    fn storage_root_matches_install_state() {
        let app = ChatGptApp::new();
        match app.storage_root() {
            Some(p) => {
                assert!(p.exists(), "reported root must exist: {}", p.display());
                assert!(p.ends_with("com.openai.chat"));
            }
            None => {
                // Not installed (or non-macOS): acceptable.
            }
        }
    }
}
