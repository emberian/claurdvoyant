# claurdvoyant — web viewer

A static, **client-side** web app for browsing agent sessions. Drop a `.zip` of a
harness directory (a `.claude/projects/…` tree, `.codex/sessions/…`, `.grok/sessions/…`,
an OpenCode `storage/` tree, etc.) and browse, search, and read the sessions inside —
entirely in your browser. Nothing is uploaded.

## How it works

- The page (`index.html`) loads `main.js` as an ES module.
- `main.js` dynamically imports the WASM module at `./pkg/cv_web.js` (built by the
  `cv-web` crate via `wasm-pack`). If it's present, dropped `.zip` bytes are handed
  straight to `ingest_zip(bytes: Uint8Array)`, which returns a JSON string — an array
  of `Session` objects.
- If the WASM module **isn't** present (e.g. the static-only fallback deploy), the app
  runs in **demo mode** using the bundled `sample.js` dataset, so the UI is always
  usable. `.zip` ingest is disabled in that mode.

The raw zip bytes are passed to the WASM module unmodified — the WASM side does the
unzipping and parsing. No JS zip library is bundled.

## Components

All are plain native custom elements (no framework, no build step), in `components/`:

- **`<cv-app>`** — shell: header, theme toggle, dropzone/upload, and the two-pane
  (list + transcript) layout. Wires WASM ingest / sample fallback. Auto-loads the
  sample dataset on first paint so the page is never empty.
- **`<cv-session-list>`** — sortable, filterable list. Free-text search across titles,
  cwd, model, and **all message content**; multi-select harness filter chips; sort by
  recency / oldest / title / message count. Emits a `select` event.
- **`<cv-transcript>`** — renders one `Session`: role-labeled turns, text (with minimal
  inline/fenced code handling), **collapsible thinking**, `tool_use` as a labeled JSON
  code block, `tool_result` (with error styling), and images as labeled placeholders.
  Shows per-message token usage and session metadata (model, cwd, git, timestamps).
- **`<cv-harness-badge>`** — a small per-harness colored pill.

`components/util.js` holds shared helpers (escaping, time formatting, search indexing,
labels). `styles.css` is a hand-written "lite" stylesheet with light/dark themes (the
theme toggle cycles dark → light → auto and persists to `localStorage`).

## Session schema

Matches the serde output of the `cv-core` IR. Each `Session`:

```
{ id, harness, cwd?, title?, created_at?, updated_at?, model?,
  git?{branch?,commit?,remote?},
  messages: [ { id?, parent_id?, role, timestamp?, model?, content: [Block], usage? } ],
  source_path? }
```

`harness` is one of `claude | codex | grok | opencode | gemini | hermes | openclaw`.
`role` is one of `system | user | assistant | tool`. Each `Block` is tagged by `kind`:

- `{ kind: "text", text }`
- `{ kind: "thinking", text, signature?, encrypted? }`
- `{ kind: "tool_use", id, name, input }`
- `{ kind: "tool_result", tool_use_id, content, is_error }`
- `{ kind: "image", media_type?, data_ref? }`

## Preview locally

No build step is needed for the JS. From this directory:

```sh
python3 -m http.server 8080
# then open http://localhost:8080
```

It will start in demo mode (sample dataset) unless you've built the WASM module into
`./pkg/`. To build the WASM locally:

```sh
wasm-pack build crates/cv-web --target web --out-dir ../../web/pkg --no-default-features
```

(run from the repo root). After that, reload the page and drop a `.zip`.

> A plain `file://` open won't work because ES module imports require an HTTP origin —
> use the local server above.

## Deployment

`.github/workflows/pages.yml` builds the WASM module (best-effort), then publishes the
whole `web/` directory to GitHub Pages on every push to `main`. If the WASM build fails
or the crate isn't present yet, the static site still deploys and runs in demo mode.
