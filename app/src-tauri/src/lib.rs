//! claurdvoyant desktop app — the Tauri v2 runtime.
//!
//! This crate is a thin native shell around the *existing* static web UI
//! (`/Users/ember/pug/claurdvoyant/web/`, wired via `frontendDist` in `tauri.conf.json`).
//! It adds the things the browser-only app can't do:
//!
//!   1. **Launches `cvd serve --port 7777` on startup** so the bundled fleet dashboard
//!      (which fetches `http://localhost:7777`) works unchanged. The child is killed on exit.
//!   2. **Native `#[tauri::command]`s** — `distill`, `redact`, `generate`, `ingest_zip` — that call
//!      into `cv_llm` / `cv_core` directly, so LLM API keys live in the desktop process's
//!      environment (or a local `LMSTUDIO_API_BASE`) instead of being shipped into JS, and zip
//!      ingestion works without the WASM module.
//!   3. A **native application menu** (File / View / Help) — including **File → Open zip…** which
//!      runs a native file dialog, ingests the zip in Rust, and emits the `cv://open-sessions`
//!      Tauri event the web UI can listen for.
//!   4. **Window state persistence** (size/position across launches) via
//!      `tauri-plugin-window-state`.
//!
//! ### cvd-serve wiring approach
//! We spawn `cvd` via `std::process::Command` (rather than a bundled Tauri *sidecar*) — it's
//! the simplest thing that compiles and runs everywhere without an extra bundling step. We
//! search the repo's build outputs `target/{release,debug}/cvd` first, then PATH, so a plain
//! `cargo build -p cvd` in the repo is enough to make it work in dev. The handle is stored in
//! Tauri's managed state and killed on `RunEvent::Exit`. A **View → cvd serve** menu toggle lets
//! you start/stop it at runtime.
//!
//! To ship a self-contained bundle later, build `cvd` (`cargo build -p cvd --release`) and
//! either drop the binary on PATH or register it as a Tauri sidecar (see `app/README.md`).

use std::io::Read;
use std::process::{Child, Command};
use std::sync::Mutex;

use serde::Serialize;
use tauri::menu::{
    AboutMetadataBuilder, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
};
use tauri::{AppHandle, Emitter, Manager, RunEvent, Runtime, WebviewWindow};
use tauri_plugin_dialog::DialogExt;

use cv_core::Session;

/// The port the bundled web fleet-dashboard expects `cvd serve` on.
const CVD_PORT: u16 = 7777;

/// The Tauri event the web UI listens for to load freshly-ingested sessions. Payload is a JSON
/// string holding a `Session[]` array (the same IR the WASM `ingest_zip` returns).
const OPEN_SESSIONS_EVENT: &str = "cv://open-sessions";

/// The public web demo, opened from Help → Open the web demo.
const WEB_DEMO_URL: &str = "https://claurdvoyant.lunar.town/";

/// Handle to the spawned `cvd serve` child, kept in Tauri managed state so we can kill it on exit
/// (and toggle it from the menu).
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

/// Read a `.zip` of exported harness logs from `path`, unzip it in-memory, run
/// `cv_core::ingest::ingest_files`, and return the parsed sessions as a JSON string (a
/// `Session[]` array — the exact shape the web UI's WASM `ingest_zip` produces).
///
/// Pure + offline: no API key, and no on-disk extraction (everything is held in memory). This is
/// the native equivalent of the browser's WASM ingest path, so file-open works without WASM.
#[tauri::command]
fn ingest_zip(path: String) -> Result<String, String> {
    let bytes = std::fs::read(&path).map_err(|e| format!("reading {path}: {e}"))?;
    ingest_zip_bytes(&bytes)
}

/// Unzip `bytes` in-memory and ingest the contained files into `Session[]` JSON. Factored out so
/// both the `ingest_zip` command and the File → Open zip… menu handler share one path.
fn ingest_zip_bytes(bytes: &[u8]) -> Result<String, String> {
    let reader = std::io::Cursor::new(bytes);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| format!("not a readable zip: {e}"))?;

    let mut files: Vec<(String, Vec<u8>)> = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("reading zip entry {i}: {e}"))?;
        if !entry.is_file() {
            continue;
        }
        // Prefer the safe sanitized name; fall back to the raw name so unusual exports still ingest.
        let name = entry
            .enclosed_name()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.name().to_string());
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut buf)
            .map_err(|e| format!("decompressing {name}: {e}"))?;
        files.push((name, buf));
    }

    let sessions = cv_core::ingest::ingest_files(files);
    serde_json::to_string(&sessions).map_err(|e| format!("serializing sessions: {e}"))
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
// Native application menu
// ---------------------------------------------------------------------------

/// Build the native application menu (File / View / Help). Menu item IDs are matched in
/// [`handle_menu_event`].
fn build_menu<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    // --- File
    let open_zip = MenuItem::with_id(app, "file.open_zip", "Open zip…", true, Some("CmdOrCtrl+O"))?;
    let quit = PredefinedMenuItem::quit(app, Some("Quit claurdvoyant"))?;
    let file = Submenu::with_items(app, "File", true, &[&open_zip, &quit])?;

    // --- Edit: without these, macOS gives the webview no Cmd+C/Cmd+V/Cmd+A — a real problem for a
    // transcript viewer where copying text is the whole point. Predefined items wire to the OS's
    // standard editing actions for the focused field/selection.
    let edit = Submenu::with_items(
        app,
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(app, Some("Undo"))?,
            &PredefinedMenuItem::redo(app, Some("Redo"))?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, Some("Cut"))?,
            &PredefinedMenuItem::copy(app, Some("Copy"))?,
            &PredefinedMenuItem::paste(app, Some("Paste"))?,
            &PredefinedMenuItem::select_all(app, Some("Select All"))?,
        ],
    )?;

    // --- View
    let reload = MenuItem::with_id(app, "view.reload", "Reload", true, Some("CmdOrCtrl+R"))?;
    let devtools = MenuItem::with_id(
        app,
        "view.devtools",
        "Toggle DevTools",
        true,
        Some("CmdOrCtrl+Alt+I"),
    )?;
    let fullscreen = PredefinedMenuItem::fullscreen(app, Some("Toggle Fullscreen"))?;
    let cvd_toggle = MenuItem::with_id(app, "view.cvd_toggle", "Toggle cvd serve", true, None::<&str>)?;
    let view = Submenu::with_items(
        app,
        "View",
        true,
        &[
            &reload,
            &devtools,
            &fullscreen,
            &PredefinedMenuItem::separator(app)?,
            &cvd_toggle,
        ],
    )?;

    // --- Help
    let about_meta = AboutMetadataBuilder::new()
        .name(Some("claurdvoyant"))
        .version(Some(env!("CARGO_PKG_VERSION")))
        .comments(Some(
            "Cross-harness AI session viewer with native distill, redact, and generative loom.",
        ))
        .build();
    let about = PredefinedMenuItem::about(app, Some("About claurdvoyant"), Some(about_meta))?;
    let web_demo = MenuItem::with_id(app, "help.web_demo", "Open the web demo", true, None::<&str>)?;
    let help = Submenu::with_items(app, "Help", true, &[&about, &web_demo])?;

    Menu::with_items(app, &[&file, &edit, &view, &help])
}

/// React to a native menu click. Unknown IDs are ignored.
fn handle_menu_event<R: Runtime>(app: &AppHandle<R>, event: MenuEvent) {
    match event.id().as_ref() {
        "file.open_zip" => open_zip_via_dialog(app),
        "view.reload" => {
            if let Some(win) = main_window(app) {
                // Reloading the webview re-fetches frontendDist (web/index.html).
                let _ = win.eval("window.location.reload()");
            }
        }
        "view.devtools" => {
            if let Some(win) = main_window(app) {
                if win.is_devtools_open() {
                    win.close_devtools();
                } else {
                    win.open_devtools();
                }
            }
        }
        "view.cvd_toggle" => toggle_cvd_serve(app),
        "help.web_demo" => {
            // Open in the user's default browser via the shell plugin (matches the existing
            // `shell:allow-open` capability). `Shell::open` is deprecated in favor of
            // tauri-plugin-opener, but using it avoids pulling in another plugin/permission set.
            #[allow(deprecated)]
            {
                use tauri_plugin_shell::ShellExt;
                let _ = app.shell().open(WEB_DEMO_URL, None);
            }
        }
        _ => {}
    }
}

/// Get the main webview window (label `"main"`), or `None` if it's gone.
fn main_window<R: Runtime>(app: &AppHandle<R>) -> Option<WebviewWindow<R>> {
    app.get_webview_window("main")
}

/// File → Open zip…: native picker → in-memory ingest → emit `cv://open-sessions` with the
/// `Session[]` JSON payload. All failures are surfaced via a native message dialog rather than
/// silently dropped.
fn open_zip_via_dialog<R: Runtime>(app: &AppHandle<R>) {
    let app = app.clone();
    // Run the (blocking) native picker off the main thread so we never deadlock the event loop.
    std::thread::spawn(move || {
        let picked = app
            .dialog()
            .file()
            .add_filter("Harness export (.zip)", &["zip"])
            .set_title("Open a harness export zip")
            .blocking_pick_file();

        let Some(file_path) = picked else {
            return; // user cancelled
        };

        let result = file_path
            .into_path()
            .map_err(|e| format!("resolving picked path: {e}"))
            .and_then(|p| std::fs::read(&p).map_err(|e| format!("reading {}: {e}", p.display())))
            .and_then(|bytes| ingest_zip_bytes(&bytes));

        match result {
            Ok(sessions_json) => {
                if let Err(e) = app.emit(OPEN_SESSIONS_EVENT, sessions_json) {
                    eprintln!("claurdvoyant: failed to emit {OPEN_SESSIONS_EVENT}: {e}");
                }
            }
            Err(e) => {
                use tauri_plugin_dialog::{MessageDialogButtons, MessageDialogKind};
                app.dialog()
                    .message(e)
                    .kind(MessageDialogKind::Error)
                    .title("Couldn't open zip")
                    .buttons(MessageDialogButtons::Ok)
                    .blocking_show();
            }
        }
    });
}

// ---------------------------------------------------------------------------
// cvd serve lifecycle
// ---------------------------------------------------------------------------

/// Locate the `cvd` binary: the repo's build outputs relative to this crate's manifest dir first
/// (so a dev `cargo build -p cvd` works without installing anything), then a bare `cvd` on PATH.
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
/// except the live fleet dashboard) still works without it. Returns the child on success.
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
                 http://localhost:{CVD_PORT} will be unavailable (non-fatal). Build it with \
                 `cargo build -p cvd --release` or put `cvd` on PATH.",
                bin.display()
            );
            None
        }
    }
}

/// View → Toggle cvd serve: kill it if running, otherwise (re)spawn it.
fn toggle_cvd_serve<R: Runtime>(app: &AppHandle<R>) {
    let state = app.state::<CvdServer>();
    let mut guard = state.0.lock().unwrap();
    if let Some(mut child) = guard.take() {
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("claurdvoyant: stopped cvd serve via menu toggle.");
    } else {
        *guard = spawn_cvd_serve();
    }
}

/// Build and run the Tauri application. Called by `main.rs`.
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        // Persist each window's size/position/etc. across launches and restore on startup.
        .plugin(tauri_plugin_window_state::Builder::new().build())
        .manage(CvdServer::default())
        .invoke_handler(tauri::generate_handler![
            distill,
            redact,
            generate,
            ingest_zip,
            provider_info
        ])
        .on_menu_event(handle_menu_event)
        .setup(|app| {
            // Install the native application menu.
            let menu = build_menu(app.handle())?;
            app.set_menu(menu)?;

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

// ---------------------------------------------------------------------------
// Tests — cover the native-command *logic* (parse/ingest/redact/binary lookup). The GUI shell
// (menu, window, event loop) can't be driven headlessly, but these are the parts with real bug
// surface, and they run without a webview via plain `cargo test`.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Build an in-memory zip from `(name, bytes)` entries using `Stored` (no compression feature
    /// needed), returning the raw zip bytes.
    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in entries {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(bytes).unwrap();
        }
        zw.finish().unwrap().into_inner()
    }

    /// A minimal Claude `.jsonl` transcript: ingest sniffs it by `sessionId` + `type:user|assistant`.
    const CLAUDE_JSONL: &str = concat!(
        r#"{"type":"user","sessionId":"sess-abc","parentUuid":null,"message":{"role":"user","content":"hello"}}"#,
        "\n",
        r#"{"type":"assistant","sessionId":"sess-abc","parentUuid":null,"message":{"role":"assistant","content":[{"type":"text","text":"hi there"}]}}"#,
        "\n",
    );

    #[test]
    fn ingest_zip_bytes_parses_a_claude_export() {
        let zip = make_zip(&[("logs/session.jsonl", CLAUDE_JSONL.as_bytes())]);
        let json = ingest_zip_bytes(&zip).expect("ingest should succeed");
        let sessions: Vec<Session> = serde_json::from_str(&json).expect("valid Session[] JSON");
        assert_eq!(sessions.len(), 1, "one transcript → one session");
        assert_eq!(sessions[0].harness, cv_core::Harness::Claude);
        assert!(
            !sessions[0].messages.is_empty(),
            "the session should carry its messages"
        );
    }

    #[test]
    fn ingest_zip_bytes_rejects_non_zip() {
        let err = ingest_zip_bytes(b"not a zip at all").unwrap_err();
        assert!(err.contains("not a readable zip"), "got: {err}");
    }

    #[test]
    fn ingest_zip_bytes_skips_dirs_and_unknown_files() {
        // A directory entry and an unrecognized file should yield an empty (but valid) array, no err.
        let zip = make_zip(&[
            ("notes/", b""),
            ("random.txt", b"just some prose, not a transcript"),
        ]);
        let json = ingest_zip_bytes(&zip).expect("ingest should not error on unknown content");
        let sessions: Vec<Session> = serde_json::from_str(&json).unwrap();
        assert!(sessions.is_empty(), "nothing recognizable → []");
    }

    #[test]
    fn parse_session_roundtrips_and_reports_errors() {
        let zip = make_zip(&[("s.jsonl", CLAUDE_JSONL.as_bytes())]);
        let json = ingest_zip_bytes(&zip).unwrap();
        let one = &serde_json::from_str::<Vec<Session>>(&json).unwrap()[0];
        let one_json = serde_json::to_string(one).unwrap();
        let parsed = parse_session(&one_json).expect("a session we just serialized must parse");
        assert_eq!(parsed.id, one.id);

        let err = parse_session("{ not json").unwrap_err();
        assert!(err.contains("invalid session JSON"), "got: {err}");
    }

    #[test]
    fn redact_command_runs_offline_and_returns_valid_json() {
        let zip = make_zip(&[("s.jsonl", CLAUDE_JSONL.as_bytes())]);
        let session_json = {
            let one = &serde_json::from_str::<Vec<Session>>(&ingest_zip_bytes(&zip).unwrap()).unwrap()[0];
            serde_json::to_string(one).unwrap()
        };
        let out = redact(session_json).expect("redact is pure/offline and shouldn't fail");
        // Output must be a valid Session again (so the UI can render it).
        serde_json::from_str::<Session>(&out).expect("redacted output is a valid Session");
    }

    #[test]
    fn cvd_binary_resolves_to_a_cvd_path() {
        // Either a built binary under target/{release,debug}/cvd, or the bare PATH fallback — but it
        // must always end in the executable name so the spawn targets the right thing.
        let p = cvd_binary();
        assert_eq!(p.file_name().and_then(|s| s.to_str()), Some("cvd"));
    }
}
