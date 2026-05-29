<div align="center">

# 🔮 claurdvoyant

### Your AI coding sessions are gold. Stop letting them rot in scattered folders.

**One tool to find, read, search, port, and stream every agent session you've ever run — across every harness.**

`claude` · `codex` · `grok` · `opencode` · `gemini` · `hermes` · `openclaw` · `cursor` · …

*parse anything · search by meaning · resume anywhere · let your agents read each other's minds*

</div>

---

## The problem (you've felt this)

You spent three hours teaching an agent your codebase. That context — the dead ends, the decisions, the hard-won understanding — is some of the most valuable output you produce. And then:

- 🙈 **You can't find it.** It's one of hundreds of `.jsonl` files in a folder named after a path. (Yes, you keep the important session IDs in a notes app. Everyone does.)
- ⛓️ **It's trapped.** Most harnesses only resume a session from the *exact directory* it ran in. Move the project, lose the thread.
- 🏝️ **It's stranded.** Started in Claude Code but want to continue in Codex? Tough. Every tool speaks its own dialect and none of them talk.

Restarting from scratch is the most expensive thing you do all day. **claurdvoyant makes sure you never have to.**

## What you can actually do with it

```sh
# 🔎 find that session from last week by what it was ABOUT, not where it lived
cv search "the flux inference refactor"        # full-text, instant
cv-search semantic "formalizing proofs"        # by meaning — no keyword overlap needed

# 🚀 take a Claude session and continue it in Codex. for real.
cv convert da9174f4 --to codex
#   ✦ wrote ~/.codex/sessions/2026/…/rollout-….jsonl
#   ↳ codex resume 019e75e0-…

# 🧳 break a session out of its directory jail (brings CLAUDE.md / MEMORY.md along)
cv port da9174f4 --to-dir ~/new/home

# 👁️ watch every agent on your machine work, live, in one feed
cv scry
```

…and a couple that didn't exist before:

- 🧠 **An MCP server** so a *running* agent can read **other** agents' sessions — "what happened in this project before?", "what's my sibling agent doing right now?" — and even **`await_omen`**: block until another session prints something matching a regex.
- 📣 **A coordination board** (`cv board` + MCP): agents post status and hand off work to each other. With the daemon mirroring activity, it's a live feed across your whole **cloud fleet**.
- 🌐 **A zero-install web viewer**: drag a zip of any harness folder into your browser and explore it. Nothing uploaded, all WASM.

## 🪐 Harnesses

| | Harness | Parse | Convert *to* |
|---|---|:--:|:--:|
| ✅ | **Claude Code** | ✅ | ✅ |
| ✅ | **Codex CLI** | ✅ | ✅ |
| ✅ | **Grok CLI** | ✅ | ✅ |
| ✅ | **OpenCode** | ✅ | ✅ |
| ✅ | **Gemini / Antigravity** | ✅ | ✅ |
| ✅ | **Hermes** (Nous) | ✅ | ✅ |
| ✅ | **OpenClaw** | ✅ | ✅ |
| ✅ | **Cursor** | ✅ | — |
| 🔒 | **Claude / ChatGPT desktop apps** | detected¹ | — |

<sub>¹ The Claude app keeps transcripts server-side; the ChatGPT app keeps them locally but encrypted at rest. We detect the install and document exactly why neither is readable — see [`docs/FORMATS.md`](docs/FORMATS.md).</sub>

**Conversion is N-way**: any ✅ source → any ✅ target, mediated by one unified IR. Every format reverse-engineered in [`docs/FORMATS.md`](docs/FORMATS.md). Bringing your own? → [`ADDING_HARNESS.md`](ADDING_HARNESS.md) 💛

## 🛠️ Install

```sh
cargo build --release          # → target/release/{cv, cv-mcp, cvd, cv-search}
```

(Prebuilt binaries for macOS/Linux/Windows × arm64/x64/x86 ship on each release via `dist`.)

## 🧠 Let agents read each other's minds (MCP)

```sh
claude mcp add claurdvoyant -- /path/to/target/release/cv-mcp
```

Tools: `list_sessions` · `search_sessions` · `read_session` · `project_sessions` · **`await_omen`** (block until a session matches a regex) · `board_*` (post/read/await on the coordination board).

## 📡 Archive your whole fleet (`cvd`)

```sh
cvd sync     # snapshot every session into ~/.claurdvoyant
cvd watch    # follow live + archive as sessions change → a fleet activity feed
```

## 🧬 The OpenSession standard

After staring into seven different transcript formats, we wrote down the one they *should* have agreed on: **[OpenSession](docs/OPENSESSION.md)** — a small, honest, harness-neutral interchange format (the key heresy: *cwd is metadata, not identity*). claurdvoyant's IR is its reference implementation. If you ship a harness, emit OpenSession and everyone's sessions become portable by construction. 🤝

## 🏗️ Under the hood

One IR (`Session → Message → Block{Text|Thinking|ToolUse|ToolResult|Image}`), one `Adapter` per harness (`discover` + `parse` + `emit`), and a handful of small crates on top: **`cv`** (CLI) · **`cv-mcp`** (MCP) · **`cvd`** (daemon) · **`cv-search`** (tantivy FTS + `model2vec` semantic) · **`cv-web`** (WASM).

```
parse(any harness) → 🔮 unified IR → search · convert · port · archive · view · stream · coordinate
```

## 🧪 Status

Built in a couple of (gleeful) sessions, much of it by a swarm of agents working disjoint files. ✨ Young and honest:

- Conversion **emits** to all 7 core harnesses; Cursor + desktop apps are parse-only and landing now.
- The search index trades disk for speed (full content; compression is on the list).
- Gemini's protobuf `.pb` is opaque; some sidecar tool-call streams aren't merged yet.
- Historical format variants are an explicit goal — see [`ADDING_HARNESS.md`](ADDING_HARNESS.md), and **please send your own harness logs** (we can only test what we can see).

PRs and weird old transcripts deeply welcome. 💜

<div align="center"><sub>made with 🔮 and an unreasonable amount of enthusiasm · MIT/Apache-2.0</sub></div>
