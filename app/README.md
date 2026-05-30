# claurdvoyant — desktop app (Tauri v2)

A single clickable cross-platform desktop application that wraps claurdvoyant:
the existing **static web UI** (session viewer, timeline, compare, stats, OpenSession,
splice/loom composer, fleet dashboard) plus **native Rust backend power** (distill,
redact, generative loom) and an auto-launched local fleet API.

## Layout

```
app/
├── README.md            ← this file
└── src-tauri/           ← the Tauri v2 Rust app (its OWN cargo workspace)
    ├── Cargo.toml       ← declares `[workspace]` so it's EXCLUDED from the repo-root workspace
    ├── build.rs         ← tauri-build codegen
    ├── tauri.conf.json  ← v2 config; identifier `town.lunar.claurdvoyant`
    ├── capabilities/
    │   └── default.json ← grants core + shell:allow-open to the main window
    ├── icons/           ← app icon set (crystal-ball theme)
    └── src/
        ├── main.rs      ← thin binary entry point
        └── lib.rs       ← Tauri runtime: native commands + cvd-serve lifecycle
```

### Why its own workspace
`src-tauri/Cargo.toml` has a top-level `[workspace]` table. That makes it a **standalone
cargo workspace**, so it is *excluded* from the main claurdvoyant workspace at the repo
root. Building or CI-testing the main crates never compiles the app (which pulls in the
heavy WebKit/Tauri stack), and vice-versa. The app still depends on the real crates by
relative path: `cv-core`, `cv-llm`, `cv-search` at `../../crates/...`.

### Frontend
No build step. `frontendDist` in `tauri.conf.json` points at the existing static web app
(`../../web`). The desktop window simply loads `web/index.html`. We do **not** modify `web/`.

## Native commands

Invoke from JS with `window.__TAURI__.core.invoke("<name>", { ... })`. Keeping these in
Rust means LLM API keys live in the desktop *process* env, never in JS:

| Command         | Args                                          | Returns                         | Backed by              |
|-----------------|-----------------------------------------------|---------------------------------|------------------------|
| `distill`       | `{ sessionJson: string }`                     | Markdown digest (`string`)      | `cv_llm::distill`      |
| `redact`        | `{ sessionJson: string }`                     | redacted Session as JSON string | `cv_core::redact`      |
| `generate`      | `{ sessionJson: string, model?: string }`     | next assistant turn (`string`)  | `cv_llm::generate`     |
| `provider_info` | `{}`                                          | `{ provider, available }`       | `cv_llm::available_provider` |

`distill`/`generate` need a provider in the environment — see [LLM providers](#llm-providers).
`redact` is pure and offline (no key needed). All take/return the **same Session IR JSON**
the web UI already speaks (`serde_json` round-trip of `cv_core::Session`).

## cvd serve wiring

On startup the app **spawns `cvd serve --port 7777`** so the bundled fleet dashboard (which
fetches `http://localhost:7777`) works unchanged. The child handle is stored in Tauri managed
state and **killed on app exit** (`RunEvent::Exit`).

**Approach chosen: `std::process::Command`** (not a bundled sidecar) — simplest thing that
compiles and runs with no extra bundling step. Binary resolution order:

1. the repo's build output: `../../target/release/cvd`, then `../../target/debug/cvd`
2. `cvd` on `PATH`

So in dev you only need:

```sh
cargo build -p cvd            # from the repo root → target/debug/cvd
# or: cargo build -p cvd --release
```

If `cvd` can't be found, the app still runs; only the live fleet dashboard is unavailable
(a clear note is logged to stderr).

> **Optional: make it a real sidecar for a self-contained bundle.** Build `cvd`
> (`cargo build -p cvd --release`), copy it to `app/src-tauri/binaries/cvd-<target-triple>`
> (e.g. `cvd-aarch64-apple-darwin`), add `"externalBin": ["binaries/cvd"]` under `bundle` in
> `tauri.conf.json`, add the `shell:allow-execute`/sidecar permission to `capabilities/default.json`,
> and spawn it via `tauri_plugin_shell`'s sidecar API instead of `std::process::Command`. The
> `std::process` path above is intentionally the default because it needs no bundle prep.

## LLM providers

`distill` and `generate` pick a provider from the desktop process environment:

- `OPENROUTER_API_KEY` — preferred when set
- `ANTHROPIC_API_KEY`
- `LMSTUDIO_API_BASE` — **free, offline, no key.** Set to `local` (→ `http://localhost:1234/v1`)
  or a full base URL of any OpenAI-compatible local server (LM Studio / Ollama / vLLM).

Example, free local looming:

```sh
LMSTUDIO_API_BASE=local cargo tauri dev
```

## Build & run

From `app/src-tauri/`:

```sh
# Compile-check the Rust (fast; no bundling, no system bundler deps):
cargo build

# Run the app in dev (opens the window, loads web/, launches cvd serve):
cargo tauri dev

# Produce a distributable bundle (.app / .dmg on macOS, etc.):
cargo tauri build
```

Requirements:
- **Rust** (stable or nightly) + `tauri-cli` v2 (`cargo install tauri-cli --version '^2'`).
- A **WebKit-based webview**: macOS uses the system WebKit (already present); Linux needs
  `libwebkit2gtk-4.1-dev` + `libgtk-3-dev` etc.; Windows uses WebView2.
- For the live fleet dashboard: a built `cvd` (see [cvd serve wiring](#cvd-serve-wiring)).

## Verified in this environment

- `cargo build` in `app/src-tauri` **compiles cleanly** (Tauri v2 + system WebKit on macOS;
  cv-core / cv-llm / cv-search all build via relative paths).
- `cargo tauri info` validates `tauri.conf.json`, the shell plugin, and `frontendDist: ../../web`.
- The app is **excluded from the main repo workspace** (`cargo metadata` at the repo root does
  not list `claurdvoyant-app`).

A full `cargo tauri build` bundle (.app/.dmg) was not produced here; run it on your machine
with `tauri-cli` installed (it is — `tauri-cli 2.9.6`).
