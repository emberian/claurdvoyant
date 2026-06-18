//! User config — a human-editable TOML file at `$XDG_CONFIG_HOME/claurdvoyant/config.toml` (falling
//! back to `~/.config/claurdvoyant/config.toml`).
//!
//! Today it holds the **export source index**: the dirs (or `conversations.json` files) where your
//! account data exports (ChatGPT / Claude.ai) live. Account exports have no fixed home like the CLI
//! harnesses, so rather than scan a hardcoded path, you register the ones you care about — as many as
//! you like — and the [`export`](crate::harness::export) adapters read them from here. (`$CV_EXPORTS`,
//! a `:`-separated list, is still honored as an ad-hoc union on top — handy for one-off runs.)

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The parsed config (missing/extra fields tolerated; absent file ⇒ defaults).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Dirs or `conversations.json` files holding account data exports to discover. Scanned ≤2 deep.
    #[serde(default)]
    pub exports: Vec<PathBuf>,
}

/// `$XDG_CONFIG_HOME/claurdvoyant` (or `~/.config/claurdvoyant`). `None` only if there's no home.
pub fn config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .map(|c| c.join("claurdvoyant"))
}

/// The config file path.
pub fn config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("config.toml"))
}

/// Load the config, tolerantly: a missing or malformed file yields [`Config::default`].
pub fn load() -> Config {
    let Some(p) = config_path() else {
        return Config::default();
    };
    match std::fs::read_to_string(&p) {
        Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
            eprintln!("cv: ignoring malformed {}: {e}", p.display());
            Config::default()
        }),
        Err(_) => Config::default(),
    }
}

/// Persist the config (creating the dir). Returns the path written.
pub fn save(cfg: &Config) -> Result<PathBuf> {
    let p = config_path().context("no home directory for the config file")?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let body = toml::to_string_pretty(cfg).context("serializing config")?;
    std::fs::write(&p, body).with_context(|| format!("writing {}", p.display()))?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_exports() {
        let cfg = Config {
            exports: vec![PathBuf::from("/a/exports"), PathBuf::from("/b/conversations.json")],
        };
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.exports, cfg.exports);
        // empty/missing tolerated
        let empty: Config = toml::from_str("").unwrap();
        assert!(empty.exports.is_empty());
    }
}
