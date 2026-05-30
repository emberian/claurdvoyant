# claurdvoyant — web viewer

A static, **client-side** web app for browsing, comparing, and **splicing** agent
sessions. Drop one or more `.zip`s of a harness directory (a `.claude/projects/…`
tree, `.codex/sessions/…`, `.grok/sessions/…`, an OpenCode `storage/` tree, etc.)
**or** OpenSession `.json` files — they all merge into one pool you can explore,
chart, diff, and weave into new transcripts. Entirely in your browser. Nothing is
uploaded.

## How it works

- `index.html` loads `main.js` as an ES module.
- `main.js` dynamically imports the WASM module at `./pkg/cv_web.js` (built by the
  `cv-web` crate via `wasm-pack`). If present, dropped `.zip` bytes are handed to
  `ingest_zip(bytes: Uint8Array)`, which returns a JSON string — an array of
  `Session` objects.
- Dropped `.json` files are parsed **directly in JS** (no wasm needed): an
  OpenSession document, an array of sessions, or a `{ sessions: [...] }` wrapper.
- If the WASM module **isn't** present (the static-only fallback deploy), the app
  runs in **demo mode** using the bundled `sample.js` dataset. `.zip` ingest is
  disabled in that mode, but OpenSession `.json` drops still work.

Everything dropped is **normalized** to one internal shape (`components/util.js`),
so the internal IR (snake_case: `tool_use`, `created_at`, `data_ref`, …) and the
OpenSession interchange shape (camelCase: `toolUse`, `createdAt`, `dataRef`,
`parentId`, …) both load and render identically.

## Views

A tab bar (`<cv-app>`) switches between views, all reading from the merged pool:

- **🗂 Sessions** — the classic two-pane list + transcript.
- **📈 Timeline** (`<cv-timeline>`) — every session across every harness on one
  chronological axis, grouped by day, colored by harness. Click to open.
- **🔍 Compare** (`<cv-compare>`) — pick two sessions side-by-side; messages are
  aligned and divergence is highlighted (shared prefix dims, the split is marked).
  Great for inspecting loom branches.
- **📊 Stats** (`<cv-stats>`) — totals, per-harness / per-role / per-block-kind
  bars, top working directories, token sums, date range, and an activity
  histogram. Hand-rolled CSS bars + inline SVG — no chart library.
- **✨ Loom** (`<cv-loom>`) — the headline. A three-pane composer: pick a source
  session, click **＋ loom** on any message to collect it, then reorder (▲ ▼ or
  drag), remove (✕), or **fork from here** (⑂, drop everything after). A live
  preview renders the composition as you build it. **Download** it as an
  OpenSession `.json` or Markdown — pure client-side.
- **🧬 OpenSession** (`<cv-opensession>`) — the OpenSession standard, featured
  in-app. Also available as a standalone page at **`openSession.html`**.

## Components

All are plain native custom elements (no framework, no build step), in `components/`:

- **`<cv-app>`** — shell: header, theme toggle, **multi-file** dropzone (with
  per-source counts), view tabs, and the active view. Merges all sources into one
  de-duplicated pool.
- **`<cv-session-list>`** — sortable, filterable list. Free-text search across
  titles, cwd, model, and **all message content**; harness filter chips; sort by
  recency / oldest / title / message count.
- **`<cv-transcript>`** — renders one `Session`: role-labeled turns; text (minimal
  inline/fenced code); **collapsible thinking** (with encrypted/redacted/signature
  handling); `tool_use` (auto-collapsed when large); `tool_result` (error styling,
  `status`, `tool_name`, collapsible `details`); **`file`** blocks; images; and a
  graceful fallback for unknown block kinds. Shows per-message **token usage** and
  a session-total. Has per-session **Markdown / OpenSession-JSON export** buttons,
  and an optional `pickMode` (used by the loom).
- **`<cv-timeline>`**, **`<cv-compare>`**, **`<cv-stats>`**, **`<cv-loom>`**,
  **`<cv-opensession>`** — the views above.
- **`<cv-harness-badge>`** — a small per-harness colored pill.

`components/util.js` holds shared helpers: HTML escaping, time formatting, search
indexing, **normalization** (`normalizeSessions`, accepts both shapes),
**export** (`toOpenSession`, `toMarkdown`, `downloadFile`), and token summing.
`styles.css` is a hand-written "lite" stylesheet with light/dark themes (the theme
toggle cycles dark → light → auto and persists to `localStorage`).

## Session schema

The internal IR (serde of `cv-core`). Each `Session`:

```
{ id, harness, cwd?, title?, created_at?, updated_at?, model?,
  git?{branch?,commit?,remote?}, extra?,
  messages: [ { id?, parent_id?, role, timestamp?, model?, usage?, content: [Block], extra? } ],
  source_path? }
```

`harness` is one of `claude | codex | grok | opencode | gemini | hermes | openclaw |
opensession`. `role` is `system | user | assistant | tool`. Each `Block` is tagged
by `kind`:

- `{ kind: "text", text }`
- `{ kind: "thinking", text, signature?, encrypted?, redacted? }`
- `{ kind: "tool_use", id, name, input }`
- `{ kind: "tool_result", tool_use_id, content, is_error, tool_name?, status?, details? }`
- `{ kind: "file", mime?, path?, source? }`
- `{ kind: "image", media_type?, data_ref? }`

Dropped OpenSession `.json` uses the camelCase equivalents (`toolUse`,
`toolResult`, `createdAt`, `parentId`, `dataRef`, `inputTokens`, …); the loader
normalizes them automatically. Unknown block kinds are rendered gracefully.

## Export & the loom

- From any transcript header: **⬇ .md** and **⬇ .json** (OpenSession).
- From the loom: the composed lane downloads as
  `{ openSession: "0.1", harness: "openSession", id, title, messages: [...] }`.

All exports are client-side `Blob` downloads — nothing leaves the page.

## Preview locally

No build step is needed for the JS. From the repo root:

```sh
python3 -m http.server --directory web 8080
# then open http://localhost:8080
```

It starts in demo mode (sample dataset, which includes an OpenSession-format
session to exercise the normalizer) unless you've built the WASM module into
`./pkg/`. To build the WASM:

```sh
wasm-pack build crates/cv-web --target web --out-dir ../../web/pkg --no-default-features
```

(run from the repo root). Then reload and drop one or more `.zip` / `.json` files.

> A plain `file://` open won't work — ES module imports require an HTTP origin.

## Deployment

`.github/workflows/pages.yml` builds the WASM module (best-effort), then publishes
the whole `web/` directory to GitHub Pages on every push to `main`. If the WASM
build fails or the crate isn't present, the static site still deploys and runs in
demo mode (with `.json` drops still functional).
