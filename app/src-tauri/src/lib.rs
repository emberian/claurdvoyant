//! claurdvoyant desktop app — the Tauri v2 runtime.
//!
//! This crate is a thin native shell around the *existing* static web UI
//! (`/Users/ember/pug/claurdvoyant/web/`, wired via `frontendDist` in `tauri.conf.json`).
//! It adds two things the browser-only app can't do:
//!
//!   1. **Launches `cvd serve --port 7777` on startup** so the bundled fleet dashboard
//!      (which fetches `http://localhost:7777`) works unchanged. The child is killed on exit.
//!   2. **Native `#[tauri::command]`s** — `distill`, `redact`, `generate` — that call into
//!      `cv_llm` / `cv_core` directly, so LLM API keys live in the desktop process's
//!      environment (or a local `LMSTUDIO_API_BASE`) instead of being shipped into JS.
//!
//! ### cvd-serve wiring approach
//! We spawn `cvd` via `std::process::Command` (rather than a bundled Tauri *sidecar*) — it's
//! the simplest thing that compiles and runs everywhere without an extra bundling step. We
//! search PATH first, then the workspace `target/{release,debug}/cvd`, so a plain
//! `cargo build -p cvd` in the repo is enough to make it work in dev. The handle is stored in
//! Tauri's managed state and killed on `RunEvent::Exit`.
//!
//! To ship a self-contained bundle later, build `cvd` (`cargo build -p cvd --release`) and
//! either drop the binary on PATH or register it as a Tauri sidecar (see `app/README.md`).

use std::process::{Child, Command};
use std::sync::Mutex;

use serde::Serialize;
use tauri::{Manager, RunEvent};

use cv_core::Session;

/// The port the bundled web fleet-dashboard expects `cvd serve` on.
const CVD_PORT: u16 = 7777;

/// Handle to the spawned `cvd serve` child, kept in Tauri managed state so we can kill it on exit.
#[derive(Default)]
struct CvdServer(Mutex<Option<Child>>);

// ---------------------------------------------------------------------------
// Native commands — invoked from JS via `window.__TAURI__.core.invoke(name, args)`.
// ---------------------------------------------------------------------------

/// Parse a `Session` from a JSON string (the IR the web UI already speaks), mapping serde errors
/// to a readable string so the JS side gets a useful message.
fn parse_session(session_json: &str) -> Result<Session, String> {
    serde_json::from_str::<Session>(session_json)
        .map_err(|e| format!("invalid session JSON: {e}"))
}

/// Distill a session into durable Markdown memory via `cv_llm::distill`.
///
/// Requires an LLM provider in the environment: `OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`, or
/// `LMSTUDIO_API_BASE` (free/offline local server). Returns the Markdown digest, or an error string.
#[tauri::command]
async fn distill(session_json: String) -> Result<String, String> {
    // `cv_llm` uses blocking reqwest; keep the async executor free by offloading to a blocking task.
    tauri::async_runtime::spawn_blocking(move || {
        let session = parse_session(&session_json)?;
        cv_llm::distill(&session, &cv_llm::DistillOptions::default()).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| format!("distill task panicked: {e}"))?
}

/// Scrub secrets/PII from a session via `cv_core::redact`, returning the redacted session as JSON.
///
/// Pure + offline — no API key needed.
#[tauri::command]
fn redact(session_json: String) -> Result<String, String> {
    let session = parse_session(&session_json)?;
    let redacted = cv_core::redact::redact(&session);
    serde_json::to_string(&redacted).map_err(|e| format!("serializing redacted session: {e}"))
}

/// Generate the next assistant turn for a session — the generative half of looming — via
/// `cv_llm::generate`. Honors an optional `model` override; works with `LMSTUDIO_API_BASE` for
/// free local generation.
#[tauri::command]
async fn generate(session_json: String, model: Option<String>) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let session = parse_session(&session_json)?;
        let opts = cv_llm::GenerateOptions {
            model,
            ..Default::default()
        };
        cv_llm::generate(&session, &opts).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| format!("generate task panicked: {e}"))?
}

/// Report whether an LLM provider is configured (so the UI can enable/disable distill/loom).
#[derive(Serialize)]
struct ProviderInfo {
    /// `"openrouter" | "anthropic" | "lmstudio"`, or `null` if no provider is configured.
    provider: Option<String>,
    /// Convenience flag for the UI.
    available: bool,
}

/// Tell the frontend which LLM provider (if any) is wired up via the desktop process's env.
#[tauri::command]
fn provider_info() -> ProviderInfo {
    let provider = cv_llm::available_provider().map(|s| s.to_string());
    ProviderInfo {
        available: provider.is_some(),
        provider,
    }
}

// ---------------------------------------------------------------------------
// cvd serve lifecycle
// ---------------------------------------------------------------------------

/// Locate the `cvd` binary: PATH first (just `cvd`), then the repo's build outputs relative to
/// this crate's manifest dir, so a dev `cargo build -p cvd` works without installing anything.
fn cvd_binary() -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR is `.../claurdvoyant/app/src-tauri`; the workspace target dir is
    // `.../claurdvoyant/target`.
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    if let Some(root) = manifest.parent().and_then(|p| p.parent()) {
        for profile in ["release", "debug"] {
            let cand = root.join("target").join(profile).join("cvd");
            if cand.exists() {
                return cand;
            }
        }
    }
    // Fall back to PATH lookup.
    std::path::PathBuf::from("cvd")
}

/// Spawn `cvd serve --port 7777`. Errors are logged but non-fatal: the rest of the UI (everything
/// except the live fleet dashboard) still works without it.
fn spawn_cvd_serve() -> Option<Child> {
    let bin = cvd_binary();
    match Command::new(&bin)
        .arg("serve")
        .arg("--port")
        .arg(CVD_PORT.to_string())
        .spawn()
    {
        Ok(child) => {
            eprintln!(
                "claurdvoyant: launched `{} serve --port {CVD_PORT}` (pid {})",
                bin.display(),
                child.id()
            );
            Some(child)
        }
        Err(e) => {
            eprintln!(
                "claurdvoyant: could not launch cvd serve ({}): {e}. The fleet dashboard at \
                 http://localhost:{CVD_PORT} will be unavailable. Build it with \
                 `cargo build -p cvd --release` or put `cvd` on PATH.",
                bin.display()
            );
            None
        }
    }
}

/// Build and run the Tauri application. Called by `main.rs`.
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(CvdServer::default())
        .invoke_handler(tauri::generate_handler![
            distill,
            redact,
            generate,
            provider_info
        ])
        .setup(|app| {
            // Launch the fleet-state API so the bundled dashboard works unchanged.
            if let Some(child) = spawn_cvd_serve() {
                let state = app.state::<CvdServer>();
                *state.0.lock().unwrap() = Some(child);
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building claurdvoyant")
        .run(|app_handle, event| {
            // On exit, reap the cvd child so we don't leave an orphaned server on :7777.
            if let RunEvent::Exit = event {
                let state = app_handle.state::<CvdServer>();
                let child = state.0.lock().unwrap().take();
                if let Some(mut child) = child {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        });
}
