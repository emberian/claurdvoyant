# The desktop & web app

Everything `cv` does on the command line, claurdvoyant also does with pixels. 🔮 The app is one static web UI — a header, a drop zone, a row of view tabs, and the active view — that runs two ways, with no code differences between them:

- **The Tauri desktop app.** A thin native shell around that same UI. It reads **all** of your real local sessions natively (no zip, no upload, zero setup), bundles a `cvd serve` for the live Fleet dashboard, and adds native distill / redact / generate so your LLM keys never touch JavaScript. This is the one to install if it's your machine.
- **The browser build.** Zero-install, hosted at the GitHub Pages site (<https://claurdvoyant.lunar.town/>). You **drop a harness `.zip`** (or an OpenSession `.json`) onto the page and explore. Nothing is uploaded — parsing happens in your browser via WASM. Great for a session a teammate sent you, or for poking around without installing anything.

Both open on a small **bundled sample** so the app is never empty, then quietly try to load your real sessions (see [Loading your sessions](#loading-your-sessions) below).

## The views

The tab bar across the top switches between eight views. They all read from one shared **pool** of sessions, so whatever you've loaded — your local corpus, a dropped zip, a File → Open import — is visible everywhere at once.

### 🗂 Sessions — the reading room

The classic two-pane view: a searchable, filterable list on the left, the full transcript on the right. This is where you actually *read* a session.

The **list** carries free-text search (matching titles, working directories, models, and **all message content**), per-harness filter chips, and sort by recency / oldest / title / message count. The count line ("142 of 1,203 sessions") tells you how hard your filters are biting.

The **transcript** renders a session as role-labeled turns: prose as safe Markdown, collapsible thinking blocks (including honest "[encrypted reasoning blob]" placeholders for harnesses like Codex and Grok), `tool_use` as labeled JSON with a copy button, `tool_result` with error styling, and images/files as labeled cards. The header shows model, cwd, branch/commit, token totals, and id, with one-click export to **OpenSession `.json`** or **Markdown**. Huge sessions (20k+ messages exist) open instantly: the first screenful renders synchronously and the rest streams in.

### ◈ Projects — the repo lens

Sessions belong to repositories (their `cwd`), and over months many agents across many harnesses work in the same repo — but a flat list hides that entirely. Projects groups your whole corpus by working directory. Each project is a card with a **per-harness breakdown bar** (a stacked bar tinted by harness, biggest slice first), a time span ("Mar '26 → 2h ago"), and an **expandable** list of its sessions (lazily rendered on first open). The question it answers: *"What has happened in this repo, across all my agents, ever?"* Click any session to jump to it in the Sessions view.

### 📈 Timeline — the scrying glass

A bird's-eye view of every session across all harnesses and all of time. Up top is a **GitHub-style activity heatmap** — a constellation of your working days, one cell per day, shaded by how many sessions you ran. Click a lit cell to jump that day into the feed below (it flashes). Above the heatmap sit a few at-a-glance numbers (active days, busiest day, days spanned), and below it a **day-grouped feed** — every session as a node, newest first, with a harness legend and totals. Click a node to open it. (The screenshot on the [Introduction](introduction.md) page is this view.)

### 🔍 Compare — side-by-side, aligned

Pick **two** sessions and view them side by side with message-level alignment. The app walks both message lists in parallel: while they match (same role + same flattened content) the rows are marked **shared** (a calm amethyst tint, ✓ in the gutter); from the first divergence onward everything is flagged **diverged** (warm tint, ≠ or •). The summary tells you the shape at a glance — *"37 shared messages, then diverges"*. This is exactly what you want for [Loom](#-loom--the-splice-composer) branches that share a common prefix and then split. Use the **⇄** button to swap A and B. (Picks aren't auto-aligned — aligning two huge arbitrary sessions on every visit was useless and slow, so *you* choose the pair.)

### 📊 Stats — the dashboard

A small, tasteful dashboard over the loaded pool: total sessions / messages / projects / harnesses, input & output token sums, sessions-per-harness bars, messages-by-role and block-kind breakdowns, top working directories, and an activity histogram over the date range. Charts are hand-rolled CSS bars and a tiny inline SVG — no chart library. It's **honest about lazy loading**: role, block, and token breakdowns reflect only the sessions whose transcripts are actually loaded, and it says so ("…breakdowns reflect the 12 opened sessions — open more to enrich them").

### ✨ Loom — the splice composer

The headline. A three-pane workspace for **weaving new transcripts** out of old ones:

1. **Source** — pick any loaded session; its transcript renders with a **＋ loom** button on every message.
2. **Composition** — the messages you've collected, in order. Reorder with ▲ ▼ or drag-and-drop, remove with ✕, retitle, and **⑂ fork from here** to spawn a new branch keeping everything up to that point.
3. **Live preview** — the composed transcript, rendered exactly as it will export.

Beyond rearranging, the Loom can actually *loom*: **⚡ generate continuation** sends the current composition to an LLM and appends the model's reply as a new assistant turn (streamed in live). Combined with fork, that grows a transcript into **alternate futures** — a branch tree you can navigate and then drop into [Compare](#-compare--side-by-side-aligned). Export the active branch as OpenSession `.json` or `.md`, entirely client-side.

Where the generation runs depends on how you're running the app:

- **Desktop:** generation routes through the native `generate` command — **no API key in the browser**, and it works with a free local `LMSTUDIO_API_BASE` server if you have one. The gen bar shows `🖥 native`.
- **Browser:** generation calls **OpenRouter** directly. Open **⚙ generation** to paste an API key (stored only in this device's `localStorage`, sent only to `openrouter.ai`) and choose a model.

### 📡 Fleet — the live dashboard

A live window onto a running [`cvd serve`](daemon.md). It polls the daemon's HTTP API (default `http://localhost:7777`) every ~2 seconds and shows your **active agents** (presence heartbeats), the live **coordination board** for a channel, the **claims** table (distributed locks, with live expiry countdowns), and a **recent-sessions** feed. Pause/resume and reconnect controls live in the toolbar; pick a channel from the dropdown.

In the **desktop app** this Just Works — it bundles and launches `cvd serve` on startup, so the Fleet tab is live the moment you open the app. In a **browser** it works too, as long as you have `cvd serve` running locally. If the API is unreachable, Fleet shows a friendly "start `cvd serve`" hint and lets you load a **static board `.json`** export instead. See [the daemon chapter](daemon.md) for what `cvd` exposes and the board chapter for what agents are saying to each other.

### 🧬 OpenSession — the standard

An in-app presentation of the OpenSession format — the unified representation every view reads. It's the schema, the eight design principles ("the working directory is metadata, never identity"), and the block types, all on one page. For the full treatment see the [OpenSession standard](opensession.md) chapter.

## Loading your sessions

There are three ways sessions get into the pool, and they all merge into one searchable corpus.

### 1. Your real local sessions (automatic)

On startup the app loads the machine's real on-disk sessions — **everything**, every harness, with zero configuration:

- **Desktop:** via the native `local_sessions` command. No HTTP, no CORS, no mixed-content surprises.
- **Browser:** by trying a local `cvd` over HTTP (`/api/sessions`). If `cvd` isn't running — the normal case for the static web deploy — it falls back to the bundled sample.

### 2. Lazy hydration (metadata stubs → full transcripts on demand)

Loading thousands of sessions stays fast because the pool is filled with lightweight **stubs**: id, harness, cwd, title, dates, message count — but **no messages**. The list, search, Projects, Timeline, and Stats all work on stubs alone. The full transcript is fetched **only when you actually open a session** (or pick it in Compare or Loom), then cached so it never re-fetches. That's why opening the app over thousands of sessions is instant, and why Stats is careful to say which numbers only count opened sessions.

### 3. Drop a zip, or File → Open

Drag one or more **`.zip`** harness exports or OpenSession **`.json`** files onto the drop zone — multiple at once, both formats, all merging into the pool. In the **browser** the zip is unpacked and parsed by the WASM module; in the **desktop app** zips are ingested natively in Rust (no WASM needed), and there's a native **File → Open zip…** menu item (`Cmd/Ctrl+O`) that runs a real file dialog and surfaces errors in a native message box. (For the gory details of how exports become OpenSession, see [Cross-harness conversion](conversion.md).)

The drop zone shows a **source chip** per loaded source ("local (cvd) **1203**", "myexport.zip **8**") so you always know what's in your pool. Dropping your first real data clears the sample automatically.

## Sub-agent trees

When a session spawned sub-agents — Claude Code's `Task` sub-agents — the transcript shows a collapsible **"N sub-agents spawned"** panel just below the header. Each child is **lazy-loaded**: expanding a row fetches that sub-agent's full transcript and renders it inline (recursively, as a nested transcript). Once loaded, the row is **relabeled with the sub-agent's task prompt** (its first user message) instead of a bare id, so you can see what each one was asked to do. Sub-agents live relative to their parent rather than in the main pool, so they don't clutter the session list.

## The searchable session picker

Anywhere you pick a session from many — Compare's two sides, Loom's source — you get a **command-palette-style picker** instead of a hopeless 2,000-option dropdown. Click the trigger (which shows the current pick as a harness badge + title) to open a filterable popover. Type to narrow: it does an **AND over your terms** across title, project, and harness ("claude payments" narrows to both), shows the most recent matches with badge · title · project · when · message count, and footers a "…and 1,140 more — keep typing to narrow" when there's overflow. Full keyboard nav: ↑/↓ to move, Enter to pick, Esc to close.

## Keyboard shortcuts

Press **?** anytime for the in-app cheat sheet.

| Key | Action |
| --- | --- |
| `1` – `8` | Switch view (Sessions, Projects, Timeline, Compare, Stats, Loom, Fleet, OpenSession) |
| `/` | Focus the session search (switches to Sessions first) |
| `j` / `k` · `↓` / `↑` | Move the selection in the session list |
| `Enter` | Open the focused session |
| `t` | Cycle theme (dark → light → auto) |
| `Esc` | Close help · back to the session list |
| `?` | Toggle the keyboard-shortcut help |

(Shortcuts never fire while you're typing in a search box or input.)

## What the desktop app adds

The browser build is fully capable, but the Tauri desktop shell adds the things a sandboxed web page can't do:

- **Native access to all your local sessions** — no zip, no `cvd` HTTP round-trip, no upload — via the `local_sessions` / `local_session` / `local_subagents` / `local_subagent` commands.
- **A bundled `cvd serve`** launched on startup (and toggleable from **View → Toggle cvd serve**), so the Fleet dashboard is live out of the box. It's reaped when you quit.
- **Native distill / redact / generate** — these call straight into `cv_llm` / `cv_core`, so LLM API keys live in the desktop process's environment (or a local LM Studio server), never shipped into JavaScript. Redaction is pure and offline; no key needed.
- **Native zip ingest** — File → Open zip… and drag-and-drop both work without the WASM module.
- A **native menu** (File / Edit / View / Help — Edit so `Cmd+C`/`Cmd+V` work in the transcript, which is the whole point of a viewer), DevTools, and **window state persistence** across launches.

Whichever way you run it, the promise on the footer holds: **all parsing happens locally; nothing is uploaded.**
