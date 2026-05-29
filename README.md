<div align="center">

# 🔮 claurdvoyant

**Peer into, search, and port the sessions of _any_ coding agent — across projects, across time, across harnesses.**

*one IR to parse them all* ✨

</div>

---

Your agent sessions are some of the most valuable context you'll ever produce. And yet today they're:

- 🙈 **unfindable** — scattered across dozens of project dirs (be honest, you track the important ones by hand)
- ⛓️ **dir-jailed** — most harnesses only let you resume a session from the *exact* directory it ran in
- 🏝️ **siloed** — every harness invents its own on-disk format and never speaks to the others

claurdvoyant fixes all three with one stubborn idea: **parse every harness into a single intermediate representation (IR)**. Once everything is in one shape, the magic falls right out:

```
parse(any harness)  →  🔮 unified IR  →  search · convert · port · archive · view · stream
```

## 🪐 Supported harnesses (7!)

| Harness | Where it lives | Parse | Convert *to* |
|---|---|:--:|:--:|
| **Claude Code** | `~/.claude/projects/**/*.jsonl` | ✅ | ✅ |
| **Codex CLI** | `~/.codex/sessions/**/rollout-*.jsonl` (+ legacy) | ✅ | ✅ |
| **Grok CLI** | `~/.grok/sessions/<cwd>/<sid>/` | ✅ | ✅ |
| **OpenCode** | `~/.local/share/opencode/storage/` | ✅ | — |
| **Gemini / Antigravity** | `~/.gemini/tmp/**/logs.json` (+ opaque `.pb`) | ✅ | — |
| **Hermes** (Nous) | `~/.hermes/state.db` (SQLite) | ✅ | — |
| **OpenClaw** | `~/.openclaw/agents/*/sessions/*.jsonl` | ✅ | — |

📖 Every format is reverse-engineered in [`docs/FORMATS.md`](docs/FORMATS.md). Want to add yours? → [`ADDING_HARNESS.md`](ADDING_HARNESS.md) 💛

## 🛠️ `cv` — the CLI

```sh
cargo build --release          # → target/release/{cv, cv-mcp, cvd}

cv ls                          # every session, every harness, newest first
cv search "flux klein"         # full-text search  (instant after `cv index`)
cv index                       # build the search index
cv show da9174f4               # render a transcript (id or prefix)
cv export da9174f4 --format md > session.md

# 🚀 port-a-sesh — the headline trick
cv convert da9174f4 --to codex          # a Claude session → a resumable Codex rollout (!!)
cv port    da9174f4 --to-dir ~/elsewhere  # rehome a session to a new cwd (escape the dir-jail)

# 👁️ scry — follow live agent activity (tail -f, across harnesses)
cv scry --cwd ~/myproject
```

Conversion **round-trips through the target's own parser** (verified by tests), and prints a `resume` hint so you can pick the session back up in the other tool. (ﾉ◕ヮ◕)ﾉ*:･ﾟ✧

## 🧠 `cv-mcp` — let agents read each other's minds

A stdio [MCP](https://modelcontextprotocol.io) server so a *running* agent can introspect what other agents have done — this project, the past, any harness:

```sh
claude mcp add claurdvoyant -- /path/to/target/release/cv-mcp
```

Tools: `list_sessions` · `search_sessions` · `read_session` · `project_sessions` ("what happened in *this* project before / what are my sibling agents up to?") · **`await_omen`** — block until a session emits a message matching a regex (wait for a sibling agent to print `BUILD PASSED` 👀).

## 📡 `cvd` — the archival daemon

Watches every harness and archives sessions into one central store (`~/.claurdvoyant`). Point it at your laptop, your servers, your whole cloud fleet — centralize everything.

```sh
cvd sync     # snapshot everything now
cvd watch    # follow live and archive as sessions change
cvd ls       # what's in the vault
```

## 🌐 The web viewer (WASM, no server)

`crates/cv-web` compiles the parser to WebAssembly; the app in `web/` lets *anyone* **drop a zip of a harness directory into their browser** and search/view it — 100% client-side, nothing uploaded. Ships to GitHub Pages.

```sh
wasm-pack build --target web --out-dir ../../web/pkg crates/cv-web -- --no-default-features
python3 -m http.server --directory web 8080   # → http://localhost:8080
```

## 🧬 The OpenSession standard

Having stared into the abyss of seven different transcript formats, we wrote down what they *should* have agreed on: **[OpenSession](docs/OPENSESSION.md)** — a small, honest, harness-neutral interchange format for agent sessions. claurdvoyant's IR is its reference implementation. 🤝

## 🏗️ Architecture

- **`cv-core`** — the IR (`Session → Message → Block{Text|Thinking|ToolUse|ToolResult|Image}`), one `Adapter` per harness (`discover` + `parse`), the `emit` engine (IR → native), the `watch` live engine, the FTS `index`, and `ingest` (in-memory/wasm parsing).
- **`cv`** CLI · **`cv-mcp`** MCP server · **`cvd`** daemon · **`cv-web`** WASM.

Cross-platform releases (macOS/Linux/Windows × arm64/x64/x86) via [`dist`](https://opensource.axo.dev/cargo-dist/) — tag `vX.Y.Z` and CI ships it. 📦

## 🧪 Status & honesty corner

Built in one (very fun) Friday-night session, largely by a swarm of agents working disjoint files. ✨ It's young:

- Conversion **emits** to Claude/Codex/Grok today; the other four are parse-only (PRs welcome!).
- The search index trades disk for speed (it stores full content — compression is a TODO).
- Gemini `.pb` is opaque; Grok/OpenCode tool-calls in sidecar logs aren't ingested yet.
- Historical format variants are an explicit goal — see [`ADDING_HARNESS.md`](ADDING_HARNESS.md).

PRs, especially **your own harness logs**, are deeply welcome — we can't test variants we've never seen. 💜

<div align="center"><sub>made with 🔮 and far too much enthusiasm</sub></div>
