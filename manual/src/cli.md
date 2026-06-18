# The CLI: `cv`

`cv` is the front door to claurdvoyant. One small binary that can find, read, search, convert, port, splice, and stream every AI coding session on your machine — across every harness it knows how to parse.

Everything `cv` does, it does over your *real* local session storage (`~/.claude`, `~/.codex`, `~/.config/opencode`, and friends). There's no database to set up and nothing to import; discovery happens by scanning the harnesses you already have installed. (Search gets an optional index — more on that below.)

```sh
cv --help          # the full subcommand list
cv <cmd> --help    # flags for any one subcommand
```

## The map

| Group | Command | What it does |
| --- | --- | --- |
| **browsing** | [`ls`](#cv-ls) | list discovered sessions, newest first |
| | [`timeline`](#cv-timeline) | one chronological feed across all harnesses |
| | [`stats`](#cv-stats) | fleet analytics over everything you've run |
| **searching** | [`search`](#cv-search) | full-text (or `--semantic`) search of session content |
| | [`recall`](#cv-recall) | the most relevant past *spans* for a query |
| | [`index`](#cv-index) | build/refresh the search index (events ride along) |
| **provenance** | [`events`](#cv-events) | what a session *did*: file edits, commands, errors |
| | [`touched`](#cv-touched) | every session that ever touched a file |
| | [`blame`](#cv-blame) | which session's reasoning wrote this code |
| **anatomy** | [`workflow`](#cv-workflow) | a `Workflow` run's phase tree → agents → outcomes (+ script) |
| | [`tools`](#cv-tools) | cross-agent tool analytics over the whole sub-agent forest |
| | [`compaction`](#cv-compaction) | every context-compaction seam + the summary it left |
| **selecting** | [`query`](#cv-query) | the `-q` query-calculus reference (fields, operators, schema) |
| **config** | [`config`](#cv-config) | view config; register account-export sources to discover |
| **sharing** | [`share`](share.md) | one redacted, self-contained HTML artifact |
| | [`pack`](pack.md) | compile past-session context for a new task |
| | [`dataset`](#cv-dataset) | export the corpus as a fine-tuning dataset (JSONL) |
| **viewing** | [`show`](#cv-show) | print one session as a transcript (or `--json`) |
| | [`export`](#cv-export) | render a session to `md` / `json` / `html` |
| | [`tree`](#cv-tree) | message threading (DAG or numbered list) |
| | [`diff`](#cv-diff) | compare two sessions message-by-message |
| | [`redact`](#cv-redact) | scrub secrets/PII, then export (safe to share) |
| | [`prune`](#cv-prune) | custom lossless compaction → a new, smaller resumable session |
| **converting / porting** | [`convert`](#cv-convert) | rewrite a session into another harness's format |
| | [`port`](#cv-port) | rehome a session to a new directory (carries context files) |
| | [`resume`](#cv-resume) | print (or run) the native resume incantation |
| **composing** | [`splice`](#cv-splice) | stitch a new session from spans of existing ones |
| | [`loom`](#cv-loom) | graft one branch onto another (optionally generate more) |
| | [`distill`](#cv-distill) | boil a session down to durable memory via an LLM |
| **live** | [`scry`](#cv-scry) | `tail -f` for agent activity, machine-wide |
| **fleet** | [`board`](#cv-board) | the agent coordination board (post/read/claim/…) |

## Two patterns you'll use everywhere

**Session-id prefixes.** Almost every command takes a session id, and almost everywhere a *prefix* is enough. The ids are long UUIDs; you'll usually paste the short 8-character form `cv ls` prints:

```sh
cv show da9174f4          # resolves the full id by prefix
```

**`--harness <name>` to disambiguate.** If a prefix is ambiguous (or you just want to be explicit), narrow it with `--harness`. Known names: `claude`, `codex`, `grok`, `opencode`, `gemini`, `hermes`, `openclaw`, `kimi`, `qwen`, and more.

```sh
cv show da91 --harness codex
```

A couple of commands ([`diff`](#cv-diff) most notably) also accept an inline `harness:id` prefix on each id, so you can mix harnesses in a single invocation.

**`-q`, the query calculus.** [`ls`](#cv-ls), [`timeline`](#cv-timeline), [`stats`](#cv-stats), and [`dataset`](#cv-dataset) take a `-q`/`--query` selector — one small boolean language for picking sessions: `harness:claude (model:fable OR model:opus) msgs>=50 -title:test`. Run [`cv query`](#cv-query) for the full field/operator reference, or `cv query --json` for a machine schema. See [Selecting](#cv-query) below.

---

## Browsing

### `cv ls`

List discovered sessions, newest-updated first.

```sh
cv ls                         # 40 most-recent across all harnesses
cv ls --harness claude        # only Claude Code sessions
cv ls --cwd flux              # only sessions whose cwd contains "flux"
cv ls --limit 100
cv ls --sort-by messages      # updated (default) | created | messages
```

```text
2245 session(s)

claude    da9174f4  2026-05-28    87 msg  the flux inference refactor
codex     019e75e0  2026-05-27   142 msg  porting the parser to nom
grok      7c0b1a3e  2026-05-26    19 msg  ~/scratch/throwaway
…
… 2205 more (use --limit)
```

When a session has no title, `cv` falls back to a dimmed cwd so the row still tells you *where* it happened.

- `--harness <name>` — restrict to one harness.
- `--cwd <substr>` — substring match on the working directory.
- `--limit <n>` — rows to show (default `40`).

### `cv timeline`

The same corpus, but read as a *feed*: oldest → newest, grouped by day. Good for "what was I doing last Tuesday, across all my tools."

```sh
cv timeline
cv timeline --harness codex --cwd claurdvoyant --limit 80
```

```text
── 2026-05-27 ──
  09:14  codex     019e75e0  ~/pug/claurdvoyant       porting the parser to nom
  16:40  claude    4f2a0c11  ~/pug/claurdvoyant       wiring up the MCP server
── 2026-05-28 ──
  11:02  claude    da9174f4  ~/pug/claurdvoyant       the flux inference refactor

2245 session(s)
```

It shows the most-recent `--limit` rows (default `60`) but still prints them oldest-first, like a chat log. Same `--harness` / `--cwd` filters as `ls`.

### `cv stats`

Fleet analytics: totals, a per-harness breakdown, your busiest directories, and the date range your corpus spans.

```sh
cv stats
```

```text
✦ claurdvoyant fleet stats

2245 session(s) · 318940 message(s)

by harness:
  claude         1204
  codex           611
  opencode        298
  …

top cwds:
    412  ~/pug/claurdvoyant
    188  ~/work/api
  …

date range:
  earliest created: 2025-09-02 10:11
  latest updated:   2026-05-29 18:44
```

No flags — it's a snapshot of everything discovery can see.

---

## Searching

See [Search](search.md) for the full story on indexing, BM25, and semantic embeddings; here's the CLI surface.

### `cv search`

Full-text search across the *content* of every session.

```sh
cv search "flux inference refactor"
cv search "nom parser" --harness codex --limit 10
cv search "formalizing proofs" --semantic
```

```text
claude    da9174f4  2026-05-28  the flux inference refactor
          … so the new flux inference path replaces the old type-walker …
```

By default `cv` uses the tantivy full-text index if you've built one (real tokenization + BM25, instant). With no index it does a live scan and gently nudges you to run [`cv index`](#cv-index). When an index exists it's authoritative — no matches means no matches, not "scan harder."

- `--semantic` — rank by *meaning* using stored embeddings instead of keywords. Requires `cv index --semantic` first; it downloads a small embedding model on first use.
- `--harness <name>` — restrict to one harness.
- `--limit <n>` — results (default `20`).

### `cv recall`

Like `search`, but built for retrieval: it returns the top-`k` most relevant *spans* and prints a compact excerpt window around the best match in each — the CLI cousin of the recall an agent does over MCP.

```sh
cv recall "how did we decide to lay out the IR" -k 8
cv recall "auth token bug" --harness claude
```

```text
claude    da9174f4   0.842  the flux inference refactor  ·  ~/pug/claurdvoyant
      user: why did we move inference out of the type-walker?
      assistant: because the walker couldn't see flux constraints …
```

It prefers semantic ranking and quietly falls back to keyword mode if embeddings aren't built (it'll tell you to run `cv index --semantic`).

- `-k <n>` — how many spans to return (default `5`).
- `--harness <name>` — restrict to one harness.

### `cv index`

Build (or refresh) the search index. Run it once, re-run it whenever you want fresh sessions to be searchable.

```sh
cv index                # full-text (tantivy) index (top-level sessions)
cv index --semantic     # also build embeddings for `cv search --semantic` / `cv recall`
cv index --subagents    # also fold the sub-agent / workflow forest into the index + events
```

```text
✦ building full-text index…
indexed 2245 session(s) → ~/.cache/claurdvoyant/tantivy
✦ embedding sessions (downloads a small model on first use)…
embedded 2245 session(s) → ~/.cache/claurdvoyant/embeddings.bin
```

- `--semantic` — also compute embeddings (downloads a ~30 MB model the first time).
- `--subagents` — also index the **sub-agent / workflow forest** (the transcripts under each Claude session's `subagents/`), tagged with their parent session, workflow run, and agent id. Without it, `cv search`/`cv touched` see only top-level sessions; with it, a hit can point you *inside* a workflow agent (e.g. "agent `census:foo` of run `wf_…` in session `3b829648`"). It can add hundreds of MB to the index, so it's opt-in. A search `Hit` and `cv touched` rows carry the provenance.

`cv index` also ingests the **event catalog** in the same streaming pass — every
tool call is classified (`file_edit`, `file_read`, `command`, `error`, `tool`)
and stored in `catalog.db`, powering the provenance commands below. One read per
changed session feeds both stores.

---

## Provenance

Search answers *what was said*; the event catalog answers *what was done*.

### `cv events`

List the classified events of one session — every file it edited or read, every
command it ran, every tool error it hit.

```sh
cv events da9174f4               # everything, in message order
cv events da9174f4 --kind error  # file_edit | file_read | command | tool | error
```

A stale or uncataloged session is ingested on the spot, so this works even
before a full `cv index`.

### `cv touched`

Every session that ever touched a file — the question you actually have when
you're staring at code wondering where it came from.

```sh
cv touched crates/cv-core/src/ir.rs       # suffix-matched, so relative paths work
cv touched src/ir.rs --edits-only         # only sessions that *wrote* it
```

```text
claude    1d1465fa  2026-05-31  13 edit(s), 1 read(s)  Plan architecture refactoring…
```

### `cv blame`

Correlate a file's git history with the event catalog: which agent session's
reasoning produced each commit — and jump straight into the conversation at the
moment of the edit.

```sh
cv blame crates/cv-core/src/ir.rs         # commits ↦ matching sessions
cv blame src/ir.rs -L 42                  # who wrote *this line*
cv blame src/ir.rs --show                 # open the best match at the edit
```

Each matched commit prints the session, the message index of the nearest edit,
and a copy-pasteable `cv show <id> --range` centered on it. Time-correlation is
honest about its limits: rebases and squashes shift commit times away from the
edits that produced them, so matches are ranked, not asserted.

---

## Pruning

### `cv prune`

*Custom, lossless compaction* of a Claude Code session into a **new, resumable** session. The standard answer to a full context window is compaction — the model rewrites your whole history into a shorter summary, which is lossy by construction. `cv prune` does the opposite: it changes *nothing* about what was said, and instead lifts the **bulky old tool payloads** (large file reads, command logs, base64 screenshots) out of the conversation into a sidecar, leaving a small `[PRUNED id=…]` marker in their place. Prompts and the chronological flow are preserved verbatim; the most recent turns are kept untouched so the model stays sharp on the task at hand.

The output is a brand-new session (a fresh id stamped across every line) — resume it with `claude --resume <new-id>`. Nothing is ever lost: each snipped original stays in `<new-id>.flat.jsonl` and comes back with `--retrieve`.

```sh
cv prune 3b829648                                  # default: snip >2KB payloads, keep the last 25 turns
cv prune 3b829648 --keep-last 40 --min-size 4096   # spare more recent context; only snip bigger payloads
cv prune 3b829648 --dry-run                        # report the savings without writing
cv prune 3b829648 --to my-tidy-session             # choose the new id
cv prune 3b829648 --drop                           # hard-drop payloads (no sidecar, irreversible)

cv prune <new-id> --retrieve toolu_abc123          # fetch a stashed original back out
```

- `--min-size <bytes>` — only snip a tool payload larger than this (default 2048).
- `--keep-last <N>` — keep the last N conversational turns' payloads verbatim (default 25).
- `--to <id>` — the new session id (default: a fresh UUID).
- `--drop` — discard payloads entirely instead of stashing them (smallest output, irreversible; the source session is never touched regardless).
- `--copy-resources` — also copy the session's `subagents/`/`workflows/` dir under the new id (off by default; can be hundreds of MB for big sessions). `claude --resume` doesn't need it — only cv's forest features (`cv workflow`/`cv tools`) on the pruned session do.
- `--dry-run` — compute and report without writing.
- `--retrieve <tool_use_id>` — instead of pruning, print the stashed original for that id from `<id>.flat.jsonl`.

Claude Code only for now (it operates on the raw JSONL to stay byte-faithful). The source session is never modified — prune only ever *writes a new one*. Also available as the `prune_session` / `prune_retrieve` MCP tools.

---

## Anatomy of a run

A deep agent session isn't a flat transcript — it's a *forest* of sub-agents, punctuated by context-compaction seams and shaped by tool use. These three commands read that structure. They're richest on Claude Code sessions (which record sub-agent transcripts, `Workflow` runs, and compaction boundaries); other harnesses return what they can.

### `cv workflow`

A [`Workflow`](https://docs.claude.com/en/docs/claude-code)-tool run, first-class: its phase tree, the agents under each phase (with state, tokens, tool-calls, duration, and a result preview), run totals, and the driving script. Without a `<run_id>`, it lists the session's runs.

```sh
cv workflow 3b829648                  # list every workflow run in the session
cv workflow 3b829648 wf_ab915970      # render one run (run-id prefix is enough)
cv workflow 3b829648 wf_ab915970 --script   # print the driving JS instead
cv workflow 3b829648 wf_ab915970 --json     # the structured Workflow IR
```

Runs are read from the session's `workflows/wf_*.json` state files; the per-agent transcripts live a tier deeper under `subagents/workflows/<run>/` (see [`cv tools`](#cv-tools) and [`cv show --subagents`](#cv-show)).

### `cv tools`

Cross-agent tool analytics across the orchestrator **and** its whole sub-agent forest: per-agent histograms, which-agent-used-what, aggregate usage, and a wall-clock tool-call timeline.

```sh
cv tools 3b829648                 # aggregate histogram (orchestrator + forest)
cv tools 3b829648 --across        # one row per agent
cv tools 3b829648 --agent af19c2f1   # one agent's histogram
cv tools 3b829648 --tool Bash     # which agents used Bash, ranked
cv tools 3b829648 --workflow wf_ab915970   # restrict to one run's agents
cv tools 3b829648 --timeline      # chronological tool-call feed, tagged by agent
cv tools 3b829648 --json          # structured output for any of the above
```

### `cv compaction`

Every context-compaction boundary in a session: each `/compact` (or auto-compaction), its trigger, the pre-compaction context size, and the summary that seeded the next window. `--summaries` prints each full summary.

```sh
cv compaction 77230e3d            # list boundaries (trigger · pre-size · summary)
cv compaction 77230e3d --summaries   # print each summary in full
cv compaction 77230e3d --json     # boundaries + each one's pre-compaction span
```

To *read* the span a compaction discarded, jump straight to it with [`cv show --pre-compaction`](#cv-show) — it sets the message window to the messages before the boundary for you.

---

## Config

### `cv config`

View the user config (`$XDG_CONFIG_HOME/claurdvoyant/config.toml`, falling back to `~/.config/…`) and manage the **export-source index**. Account data exports (the ChatGPT / Claude.ai "Export data" archives) have no fixed home, so you register where they live and the [`chatgpt-export`](harnesses.md)/[`claude-export`](harnesses.md) harnesses discover them from there.

```sh
cv config                              # print the config path + registered export sources
cv config --add-export ~/Downloads     # register a dir to scan (or a specific conversations.json)
cv config --add-export ~/exports/chatgpt-2026/conversations.json
cv config --rm-export ~/Downloads      # unregister
```

The file is plain TOML (`exports = ["…", "…"]`) — edit it by hand if you prefer. `$CV_EXPORTS` (a `:`-separated list) is honored as an ad-hoc union on top, for one-off runs without touching the config. With nothing registered, export discovery is a no-op, so it never slows the default `cv ls` (these archives are large).

## Selecting

### `cv query`

Not a session command — the **reference for the `-q` query calculus** that [`ls`](#cv-ls), [`timeline`](#cv-timeline), [`stats`](#cv-stats), and [`dataset`](#cv-dataset) accept. Run it bare for the human reference (every field, operator, and example); `--json` emits a machine-readable schema an agent can crawl.

```sh
cv query           # the full language reference
cv query --json    # fields, types, operators, costs — as JSON
```

The language is a boolean conjunction of terms — implicit `AND`, plus `OR`/`|`, `NOT`/`-`, and `( )` grouping:

```sh
cv ls -q "harness:claude model:fable"                 # fable sessions from Claude
cv ls -q "(model:fable OR model:opus) -title:test"    # either model, excluding tests
cv ls -q "msgs>=50 after:2026-01-01 tool:Bash"        # big recent sessions that ran Bash
cv ls -q 'title~"fly\.?io|aws"'                       # ~ is case-insensitive regex
cv stats -q "touched:src/ir.rs has:errors"            # analytics over a slice
cv ls -q "harness:claude agent:Explore subtool:Bash"  # a forest query (see below)
```

Fields, by what they cost to answer:

- **catalog** (free, no parse): `harness`, `cwd`, `title`, `id`, `msgs`, `created`, `updated` (`before:`/`after:` are sugar).
- **parse** (reads the transcript): `model` (any turn's model), `git` (branch/remote), `thread` (a message-tree path — see below).
- **index** (needs `cv index`): `text` — full-text over content.
- **events**: `tool` (a tool *this session* ran), `touched` (a file it read/edited), `has` (flags: `subagents`, `errors`, `tools`, `images`, `compacted`, `workflows`).
- **forest** (walks the [sub-agent forest / workflows / compaction seams](#cv-workflow) — the priciest): `subtool` (a tool a *sub-agent* ran), `agent` (spawned a sub-agent of this type), `agents` (forest size), `workflow` (ran a matching `Workflow`), `workflows` (run count), `compactions` (boundary count).

Operators: `:` (contains), `=` (exact), `~` (case-insensitive regex, local string fields only), and `> >= < <=` for numbers/dates; values can be comma-OR-lists (`a,b`) or ranges (`lo..hi`). The evaluator prunes with the catalog-cheap terms **first**, so a forest/parse/index term only ever runs on what survives — always pair one with a `harness:`/`cwd:`/`msgs` term to stay fast.

```sh
cv ls -q "harness:claude agents>=5 subtool:Bash compactions>=1"   # deep, tool-heavy, compacted runs
cv ls -q "workflow:census OR workflows>=3"                        # ran a census workflow, or 3+ runs
```

**`thread:` — a CSS-y message-tree path.** Match a parent→child chain through the session's message DAG (the loom/threading dimension), using the CSS child combinator `>`. Each step matches one message; a step is a role (`user`/`assistant`/`tool`/`system`), `tool:NAME` (a turn with that tool-use), `text:WORD` (or a bare word — content contains). Quote the whole value so the spaces and `>` belong to the path, not the outer query:

```sh
cv ls -q 'harness:claude thread:"text:refactor > assistant"'   # a "refactor" turn whose reply is an assistant turn
cv ls -q 'thread:"user > assistant > tool:Bash"'               # a user turn that led to a Bash tool-use
```

Each `>` is a *direct* child (a reply), resolved via `parent_id`, so it follows the real threading — including loom branches and sidechains. (Harnesses that don't record message ids can't thread, so `thread:` simply won't match there.)

---

## Exporting datasets

### `cv dataset`

Export the corpus as a fine-tuning dataset — JSONL, one session per line. `chatml` (default) emits `{"messages":[…]}`; `sharegpt` emits `{"conversations":[…]}`. Both import directly into Unsloth Studio / TRL / HuggingFace `datasets` with no adapter. Streamed one session at a time, so memory stays flat even over a multi-GB corpus.

```sh
cv dataset --out corpus.jsonl                      # whole corpus, chatml
cv dataset --format sharegpt --harness claude      # one harness, ShareGPT shape
cv dataset -q "model:fable" --subagents --out fable.jsonl   # a queried slice + its forest
cv dataset -q "harness:claude" --redact-only private_key    # strip PEM keys, keep the rest
```

- `-q`/`--query <calculus>` — only sessions matching the [query calculus](#cv-query).
- `--subagents` — also emit each (Claude) session's sub-agent transcripts. A parent on one model often spawns sub-agents on another, and most model usage lives in the forest, so a `model:` query usually wants this.
- `--min-messages <n>` — drop sessions with fewer than `n` messages (default 2).
- `--redact` — scrub every secret/PII class before emitting. `--redact-only <classes>` scopes it to a comma list (`private_key`, `api_key`, `jwt`, `email`, `blob`, `assignment`) — e.g. strip PEM blocks while keeping identities intact.
- `--limit <n>` — stop after `n` emitted records. `--out <file>` — write to a file instead of stdout.

---

## Viewing

### `cv show`

Print a single session as a readable transcript.

```sh
cv show da9174f4
cv show da91 --harness codex
cv show da9174f4 --json      # the raw unified IR
```

The header line gives you harness, full id, cwd (home-relative), and model; then each message is printed role-by-role with tool calls and results inlined.

- `--harness <name>` — disambiguate the prefix.
- `--json` — emit the unified IR as pretty JSON instead of a transcript. This is the same shape every adapter parses into — handy for piping into `jq` or feeding another tool.
- `--range <start>-<end>` — render only that 0-based, end-exclusive message window (`<start>-`, `-<end>`, and negative `-N` from the end all work). Messages outside the window are never resolved, so a windowed view of a huge session reads only the bytes it shows.
- `--pre-compaction [N]` — read the span *before* a compaction boundary (the context a continued agent lost); defaults to the first boundary, `--pre-compaction 2` for the Nth. It sets `--range` for you. Pair with [`cv compaction`](#cv-compaction) to see where the seams are.
- `--subagents` — instead of the transcript, list the sub-agent forest this session spawned (each child's type, journaled outcome, and return value). `--agent <agent-id>` renders one specific sub-agent's transcript, resolved through this parent.

### `cv export`

Render a session to a file format on stdout. Redirect it wherever you like.

```sh
cv export da9174f4 > session.md             # markdown (default)
cv export da9174f4 --format json > s.json
cv export da9174f4 --format html > s.html   # one self-contained file
```

- `--format <md|json|html>` — output format (default `md`). `html` is a single self-contained page you can open or send to someone.
- `--harness <name>` — disambiguate the prefix.

(`show` and `export` are siblings: `show` is for your terminal, `export` is for a file or a pipe.)

### `cv tree`

Show how a session's messages thread together. If any message carries a `parent_id`, you get an indented DAG; otherwise a clean numbered list. Tool turns and sub-agent spawns are flagged.

```sh
cv tree da9174f4
```

```text
# the flux inference refactor
claude · da9174f4-… · 87 msg

• user  walk the type checker and find where flux is ignored
• assistant [🔧 tool, ↳ sub-agent (Task)]  dispatching a search…
  • tool [↩ result]  found 3 call sites …
  • assistant  here's the plan …
```

- `--harness <name>` — disambiguate the prefix.

### `cv diff`

Compare two sessions message-by-message: a shared prefix marked `=`, then the divergence — `<` for messages only in A, `>` for messages only in B. Great for inspecting two [`loom`](#cv-loom) branches that started from the same root.

```sh
cv diff da9174f4 4f2a0c11
cv diff claude:da91 codex:019e            # per-side harness prefixes
```

```text
A claude   da9174f4  87 msg
B claude   4f2a0c11  91 msg

= user      walk the type checker and find where flux is ignored
= assistant here's the plan …
< assistant let's start with the walker
> assistant let's start with the constraint solver

42 shared, 45 only-in-A, 49 only-in-B
```

Each side may carry its own `harness:id` prefix, so you can diff sessions that live in *different* harnesses. A side without a recognized prefix falls back to the shared `--harness` (or an unconstrained lookup).

### `cv redact`

Scrub secrets and PII — API keys, private keys, JWTs, emails, opaque blobs, `KEY=value` assignments — then export the cleaned session. The thing to reach for before you paste a transcript into an issue.

```sh
cv redact da9174f4 > safe.md
cv redact da9174f4 --format json --stats
```

```text
✦ redacted 7 item(s): 2 api_key, 1 private_key, 0 jwt, 3 email, 1 blob, 0 assignment
```

- `--format <md|json>` — output format (default `md`).
- `--stats` — print per-class redaction counts to stderr.
- `--harness <name>` — disambiguate the prefix.

---

## Converting & porting

This is claurdvoyant's headline trick. See [Cross-harness conversion](conversion.md) for which harnesses can emit and the gory IR details.

### `cv convert`

Rewrite a session into *another* harness's native format, so you can pick it up in a different tool.

```sh
cv convert da9174f4 --to codex
```

```text
✦ wrote ~/.codex/sessions/2026/05/28/rollout-….jsonl (019e75e0-…)
  ↳ codex resume 019e75e0-…
```

By default it writes into the target harness's *real* storage root — so the session shows up in that tool immediately. Don't want to touch real storage yet? Point `--out` at a scratch directory for a safe dry run.

- `--to <harness>` — **required** target harness.
- `--from <harness>` — source hint (otherwise auto-detected from the id).
- `--out <dir>` — write under this directory instead of the target's storage root.
- `--cwd <dir>` — rehome the converted session to this working directory.

If the target harness can't be emitted to yet, `cv` tells you so plainly (the source still parsed fine) and lists the harnesses that *are* supported.

### `cv port`

Break a session out of its directory jail. `port` rehomes a session to a new working directory — and, because most harnesses only resume from the exact directory a session ran in, this is what lets you move a project and keep the thread.

```sh
cv port da9174f4 --to-dir ~/new/home
cv port da9174f4 --to codex --to-dir ~/new/home   # rehome *and* switch harness
```

Unless you pass `--no-context`, it also copies the project's context files — `CLAUDE.md`, `CLAUDE.local.md`, `AGENTS.md`, `GEMINI.md`, `MEMORY.md`, `.cursorrules`, `.windsurfrules` — into the new home, so the ported session lands with its memory and instructions intact. It never overwrites an existing file at the target.

- `--to <harness>` — target harness (defaults to the source harness — a pure rehome).
- `--from <harness>` — source hint.
- `--to-dir <dir>` — the new working directory.
- `--out <dir>` — write under this directory instead of the target's storage root.
- `--no-context` — don't copy context files.

(`convert` is "same place, different tool"; `port` is "different place, optionally different tool, bring the memory.")

### `cv resume`

Print the exact incantation to resume a session in its native harness — or, with `--launch`, just run it for you (cd-ing to the session's cwd first).

```sh
cv resume da9174f4
```

```text
cd ~/pug/claurdvoyant
claude --resume da9174f4-…
```

```sh
cv resume da9174f4 --launch    # spawns the harness for you
```

`cv` knows the resume command for the CLI harnesses it supports (`claude --resume`, `codex resume`, `opencode --session`, …). For desktop/IDE-only harnesses there's no documented CLI resume, so it prints a friendly note instead.

- `--launch` — actually spawn the harness instead of printing.
- `--harness <name>` — disambiguate the prefix.

---

## Composing

The loom. These commands build *new* sessions out of old ones — and optionally let an LLM keep writing from where the stitch ends. The generative steps and model selection lean on the [app / LLM](app.md) layer; here are the CLI handles.

### `cv splice`

Stitch a new session together from spans of existing ones. Each spec is `<id>:<start>-<end>`, `<id>:<start>-` (through the end), or just `<id>` (the whole session). Indices are message positions.

```sh
# first 20 messages of one session, then messages 50+ of another
cv splice da9174f4:0-20 4f2a0c11:50- --export md

# materialize it into a real Codex session
cv splice da9174f4:0-20 4f2a0c11:50- --to codex

# stitch, then let an LLM continue the thread
cv splice da9174f4:0-20 4f2a0c11:50- --generate
```

```text
✦ composed 7b3e0a91 (claude) · 71 msg
  ↳ claude da9174f4[0..20]
  ↳ claude 4f2a0c11[50..91]

(use --export md|json to print it, or --to <harness> [--out <dir>] to emit it)
```

Without `--to` or `--out`, splice just composes in memory and prints a summary (add `--export md|json` to dump the result). With `--to`/`--out` it materializes the session for a harness, exactly like `convert`.

- `<specs>…` — one or more span specs (**required**).
- `--to <harness>` — target harness (defaults to the first spec's harness).
- `--out <dir>` — write under this directory instead of the target's storage root.
- `--export <md|json>` — print the composed session instead of emitting it.
- `--cwd <dir>` — rehome the composed session.
- `--generate` — append an LLM-generated continuation (needs a provider — see below).
- `--gen-model <model>` — model for `--generate` (provider-specific default otherwise).

### `cv loom`

A focused two-session graft: take `base[..N]`, then graft `graft[M..]` onto it, producing one new branched session. Where `splice` is general stitching, `loom` is the classic "rewind to message N, then continue along a different path."

```sh
cv loom da9174f4 --at 20 --graft 4f2a0c11 --from 35 --export md
cv loom da9174f4 --at 20 --graft 4f2a0c11 --from 35 --to codex
cv loom da9174f4 --at 20 --graft 4f2a0c11 --from 35 --generate   # 🔮 grow a new branch
```

- `<base>` — base session id (positional).
- `--at <n>` — keep `base[..n]`.
- `--graft <id>` — the session to graft from.
- `--from <m>` — start grafting at `graft[m]`.
- `--to <harness>` — target harness (defaults to the base's harness).
- `--out <dir>` — write under this directory instead of the target's storage root.
- `--export <md|json>` — print instead of emitting.
- `--cwd <dir>` — rehome the grafted session.
- `--generate` / `--gen-model <model>` — same generative tail as `splice`.

> **`--generate` needs an LLM provider.** Set one of `OPENROUTER_API_KEY` (preferred), `ANTHROPIC_API_KEY`, or `LMSTUDIO_API_BASE=local` (a free local LM Studio server at `localhost:1234`). It appends a single generated assistant turn to the composed branch.

### `cv distill`

Boil a session down to *durable memory* — the decisions, the gotchas, where things live — via an LLM. Perfect for building up a `MEMORY.md` you actually want to keep.

```sh
cv distill da9174f4                              # digest to stdout
cv distill da9174f4 --project                    # frame it as whole-project memory
cv distill da9174f4 --out MEMORY.md --append     # grow a dated MEMORY.md
```

```text
✦ distilling via openrouter…
```

- `--model <id>` — model id (defaults to the provider's cheap/fast model).
- `--project` — frame the distillation as whole-project memory (may span more than one session).
- `--out <file>` — write the digest to a file instead of stdout.
- `--append` — with `--out`, append under a dated header (overwrite is the default).
- `--harness <name>` — disambiguate the prefix.

Same provider requirement as `--generate` above; if no provider is configured it prints exactly which env var to set and exits.

---

## Live

### `cv scry`

`tail -f` for agent activity, across every harness at once. Run it and watch new sessions appear and existing ones grow, in real time. 🔮

```sh
cv scry
cv scry --harness claude --cwd claurdvoyant
cv scry --existing            # also emit sessions already present at startup
cv scry --interval 1.0
```

```text
✦ scrying for agent activity… (Ctrl-C to stop)
✷ new  claude   da9174f4  ~/pug/claurdvoyant  (3 msg)
      user walk the type checker and find where flux is ignored
   +  claude   da9174f4  ~/pug/claurdvoyant  (1 msg)
      assistant found it — the walker drops flux constraints here …
```

This is the CLI face of [the daemon](daemon.md), which mirrors the same activity into a live feed for your whole fleet (and powers the MCP `await_omen` block-until-match primitive).

- `--harness <name>` — only follow one harness.
- `--cwd <substr>` — only follow sessions whose cwd contains this substring.
- `--interval <secs>` — poll interval (default `2.0`).
- `--existing` — also emit sessions that already exist at startup (default: only new activity).

---

## Fleet

### `cv board`

The agent coordination board: a tiny message bus your agents (and you) use to post status, ask questions, hand off work, and claim tasks so two agents don't stomp the same file. It's the same board the MCP server and [the daemon](daemon.md) expose; see [the board](board.md) for the bigger picture. Everything is organized into named channels.

```sh
cv board post build "tests are green on the nom branch" --tag ci --kind status
cv board read build
cv board channels
cv board watch build --match "deploy done"
```

The full set of actions:

| Action | What it does |
| --- | --- |
| `post <channel> <body>` | post a message. `--from`, `--kind`, `--tag` (repeatable), `--session-ref` |
| `read <channel>` | read messages. `--since <id>`, `--limit` (default `50`), `--json` |
| `channels` | list every channel |
| `watch <channel>` | follow live; `--match <substr>` exits when a body matches. `--since`, `--interval` |
| `request <channel> <body>` | post a question others can `reply` to; prints the request id. `--from` |
| `reply <channel> <in-reply-to> <body>` | answer a request by its id. `--from` |
| `replies <channel> <request-id>` | collect every reply to a request |
| `claim <channel> <key>` | try to claim a task (a soft lease); exits non-zero on contention. `--from`, `--ttl-secs` (default `300`) |
| `release <channel> <key>` | release a claim you hold. `--from` |
| `claims <channel>` | list the active (un-expired) claims |
| `who <channel>` | list agents seen recently (and post your own heartbeat). `--within-secs` (default `60`) |
| `ack <channel> <message-id>` | acknowledge a message with a tiny ack note. `--from` |

A request/reply round-trip looks like this:

```sh
$ cv board request build "should I bump the MSRV to 1.82?"
✦ requested 3f9c0a21 on #build
  ↳ reply with: cv board reply build 3f9c0a21-… <body>

$ cv board reply build 3f9c0a21 "yes — CI already runs 1.82" --from codex-agent
✦ replied a7b1… to 3f9c0a21 on #build

$ cv board replies build 3f9c0a21
14:22:31  codex-agent  (reply) yes — CI already runs 1.82
```

And the claim/lease dance, for when two agents might race on the same work:

```sh
$ cv board claim build refactor-parser --from agent-a --ttl-secs 600
GRANTED  refactor-parser → agent-a (expires 2026-05-29 14:32:18)

$ cv board claim build refactor-parser --from agent-b
CONTENDED  refactor-parser is held by agent-a        # exits non-zero

$ cv board release build refactor-parser --from agent-a
✦ released refactor-parser on #build
```

`claim` exits non-zero when the key is already held, so it composes cleanly in scripts: `cv board claim … && do_the_work`.
