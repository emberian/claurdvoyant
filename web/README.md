# claurdvoyant — web viewer

A static, **client-side** web app for browsing, comparing, and **splicing** agent
sessions. Drop one or more `.zip`s of a harness directory (a `.claude/projects/…`
tree, `.codex/sessions/…`, `.grok/sessions/…`, an OpenCode `storage/` tree, etc.)
**or** OpenSession `.json` files — they all merge into one pool you can explore,
chart, diff, and weave into new transcripts. Entirely in your browser. Nothing is
uploaded.

It also runs inside the **Tauri desktop shell** (the `app/` wrapper) and degrades
gracefully to a plain browser — see **Desktop (Tauri) integration** below.

## Keyboard shortcuts

Press **`?`** (or the header `?` button) for an in-app overlay. The essentials:

| Key | Action |
| --- | --- |
| `1`–`7` | Switch view (Sessions … OpenSession) |
| `/` | Focus the session search |
| `j` / `k` · `↓` / `↑` | Move the selection in the session list |
| `Enter` | Open the focused session |
| `t` | Cycle theme (dark → light → auto) |
| `Esc` | Close help · back to the session list (narrow screens) |
| `?` | Toggle the help overlay |

Shortcuts never fire while you're typing in an input, and respect focus.

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
  drag), remove (✕), or **fork from here** (⑂). A live preview renders the
  composition as you build it. **Download** it as an OpenSession `.json` or
  Markdown — pure client-side. Now it also *looms*: see **Generation & the branch
  tree** below.
- **📡 Fleet** (`<cv-fleet>`) — a live dashboard over a running `cvd serve` HTTP
  API. See **Fleet dashboard** below.
- **🧬 OpenSession** (`<cv-opensession>`) — the OpenSession standard, featured
  in-app. Also available as a standalone page at **`openSession.html`**.

## ⚡ Loom: generation & the branch tree

The loom no longer only *rearranges* messages — with an OpenRouter API key (or the
desktop runtime) it **generates** continuations, so you can fork a transcript and
grow alternate futures with a model.

The **branch tree** at the top of the loom is a real fork-structure
visualization, not a flat bar: roots and their forked descendants are indented by
depth with `├`/`└` guide rails, each child shows its fork point (`⑂N` = messages
shared with its parent), the active branch is highlighted, and rows are
keyboard-navigable (`↑`/`↓` to move, `Enter`/`Space` to switch). Deleting a branch
re-parents its children so the tree stays connected.

- **⚙ generation** (top-right of the loom) opens a settings panel: paste an
  **OpenRouter API key** and pick a **model** (free-text, with presets like
  `anthropic/claude-3.5-sonnet`, `openai/gpt-4o-mini`). The key is stored in
  `localStorage` on your device only and is sent **only** to
  `https://openrouter.ai/api/v1/chat/completions` over HTTPS (Bearer auth) —
  nothing else ever sees it.
- **⚡ generate continuation** converts the current composition to OpenRouter
  chat format (IR roles → `user`/`assistant`/`system`; `tool` → user context;
  text/thinking/tool blocks flattened to text) and **appends** the model's reply
  as a new IR `assistant` message in the lane. The reply **streams** in
  (`fetch` + `ReadableStream` over SSE) when the provider supports it, with a
  **stop** button; otherwise it awaits the full response. Inside the desktop
  shell this routes through the native `generate` command instead (no key — the
  bar shows **🖥 native**).
- **Branches.** The branch tree holds multiple named variants, each with its own
  lane. **＋ branch** duplicates the active branch into a child; **⑂** on any lane
  message forks a *child branch* from that prefix. Switch branches, generate
  divergent continuations, and compare them side-by-side in the **🔍 Compare**
  view (export each branch to OpenSession `.json` and drop them back in).
- Errors (bad key `401/403`, no credits `402`, rate limit `429`, …) surface
  inline and never block the UI; the key always stays client-side.

Implementation: `components/cv-loom.js` plus the dependency-free helper
`openrouter.js` (credential storage, IR→chat conversion, streaming request).

## 📡 Fleet dashboard

`<cv-fleet>` connects to a running **`cvd serve`** HTTP API and live-displays the
fleet by **polling every ~2s** (CORS is enabled server-side, so cross-origin
`fetch` from this static page works). Enter a **base URL** (default
`http://localhost:7777`) and a **channel** (default `fleet`, i.e. the `#fleet`
activity channel the daemon mirrors into).

It renders, all auto-refreshing with a **pause** toggle:

- an **active-agents** strip — present agents from `/api/who/<channel>`
  (recent heartbeats), harness-colored;
- a **claims table** — the distributed locks from `/api/claims/<channel>`
  (`key → owner → expires`), with soon/expired highlighting;
- a **#channel board feed** — messages from `/api/board/<channel>`
  (`{id,channel,from,ts,kind,body,tags,session_ref}`), newest first, colored by
  `kind` (`status`/`event`/`request`/`reply`/`presence`/`claim`);
- a **recent-sessions** activity feed — `/api/sessions?limit=N` (IR `Session[]`),
  harness-badged.

Polled endpoints: `/api/health`, `/api/sessions?limit=`, `/api/channels`,
`/api/board/<channel>`, `/api/claims/<channel>`, `/api/who/<channel>`.

If the API is unreachable it shows a friendly **"start `cvd serve`"** hint and
lets you **load a static board export** (`.json`: an array of `BoardMessage`s, or
an object `{ board?, claims?, who?, sessions?, channels?, channel? }`) to explore
the layout offline.

## Components

All are plain native custom elements (no framework, no build step), in `components/`:

- **`<cv-app>`** — shell: header, theme toggle, **multi-file** dropzone (with
  per-source counts), view tabs, and the active view. Merges all sources into one
  de-duplicated pool.
- **`<cv-session-list>`** — sortable, filterable list. Free-text search across
  titles, cwd, model, and **all message content**; harness filter chips; sort by
  recency / oldest / title / message count.
- **`<cv-transcript>`** — renders one `Session`: role-labeled turns; **Markdown
  prose** (headings, lists, blockquotes, emphasis, links, inline + fenced code
  with a light highlighter — see `markdown.js`, XSS-safe by escaping before
  injecting); **collapsible thinking** (with encrypted/redacted/signature
  handling); `tool_use` (highlighted JSON, auto-collapsed when large);
  `tool_result` (error styling, `status`, `tool_name`, collapsible `details`);
  **`file`** blocks; images; and a graceful fallback for unknown block kinds.
  Shows per-message **token usage** and a session-total. **Very long transcripts
  are virtualized** — only a sliding window of messages is in the DOM, so a
  12k-message session renders in ~30 ms instead of freezing the tab. Has
  per-session **Markdown / OpenSession-JSON export** buttons, and an optional
  `pickMode` (used by the loom).
- **`<cv-timeline>`**, **`<cv-compare>`**, **`<cv-stats>`**, **`<cv-loom>`**,
  **`<cv-fleet>`**, **`<cv-opensession>`** — the views above.
- **`<cv-harness-badge>`** — a small per-harness colored pill.

`openrouter.js` is a dependency-free OpenRouter client used by the loom (key
storage in `localStorage`, IR→chat conversion, and a streaming
`chat/completions` request).

`components/util.js` holds shared helpers: HTML escaping, time formatting, search
indexing, **normalization** (`normalizeSessions`, accepts both shapes),
**export** (`toOpenSession`, `toMarkdown`, `downloadFile`), and token summing.
`markdown.js` is the dependency-free, XSS-safe Markdown renderer + tiny code
highlighter used by the transcript. `tauri.js` is the desktop-integration shim
(see below). `styles.css` is a hand-written "lite" stylesheet with light/dark
themes (the theme toggle cycles dark → light → auto and persists to
`localStorage`).

## Desktop (Tauri) integration

The same `web/` runs unchanged in a plain browser **and** inside the Tauri desktop
shell (the `app/` wrapper). All of `tauri.js` degrades to no-ops when
`window.__TAURI__` is absent, so the browser path is never affected.

When running under Tauri, claurdvoyant:

- **routes generation through native commands.** The loom's **⚡ generate** calls
  `window.__TAURI__.core.invoke('generate', { messages, model })` instead of the
  in-JS OpenRouter path — **no API key needed** (the desktop env / a local LM
  Studio does the work). The gen bar shows **🖥 native**. (The same `invoke`
  pathway is ready for `distill` / `redact`.) Streaming is supported if the native
  side emits `cv://generate-token` events; otherwise the full reply is awaited.
- **loads sessions from the native File → Open.** It listens for the
  `cv://open-sessions` Tauri event (payload = a JSON array of `Session`), via
  `window.__TAURI__.event.listen`, and merges the opened sessions into the pool —
  exactly like a drop. The `app/` agent emits that event from its native menu.

In a browser none of this exists: `isTauri()` is `false`, `canInvokeNative()` is
`false`, the loom falls back to OpenRouter, and the event listener is a no-op.

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

### Demo the loom generation

1. Open the **✨ Loom** tab and click **＋ loom** on a few source messages.
2. Click **⚙ generation**, paste an OpenRouter API key, pick a model.
3. Click **⚡ generate continuation** — the model's reply streams in as a new
   assistant turn. Use **⑂** on a message to fork a sibling **branch**, switch to
   it, and generate a divergent continuation. Compare branches in **🔍 Compare**.

### Demo the fleet dashboard

Run the daemon's HTTP API alongside the static site:

```sh
cvd serve --addr 127.0.0.1:7777   # CORS-enabled
```

Open the **📡 Fleet** tab (base URL `http://localhost:7777`, channel `fleet`).
The active-agents strip, claims table, board feed, and recent-sessions feed
refresh every ~2s; use **⏸ pause** to freeze. No server? The offline panel lets
you load a static board `.json` to explore the layout.

## Deployment

`.github/workflows/pages.yml` builds the WASM module (best-effort), then publishes
the whole `web/` directory to GitHub Pages on every push to `main`. If the WASM
build fails or the crate isn't present, the static site still deploys and runs in
demo mode (with `.json` drops still functional).
