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
| **viewing** | [`show`](#cv-show) | print one session as a transcript (or `--json`) |
| | [`export`](#cv-export) | render a session to `md` / `json` / `html` |
| | [`tree`](#cv-tree) | message threading (DAG or numbered list) |
| | [`diff`](#cv-diff) | compare two sessions message-by-message |
| | [`redact`](#cv-redact) | scrub secrets/PII, then export (safe to share) |
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

By default `cv` uses the tantivy full-text index if you've built one (real tokenization + BM25, instant). If there's no tantivy index it falls back to an older SQLite FTS index, and if *that's* missing too it does a live scan and gently nudges you to run [`cv index`](#cv-index). When an index exists it's authoritative — no matches means no matches, not "scan harder."

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
cv index                # full-text (tantivy) index
cv index --semantic     # also build embeddings for `cv search --semantic` / `cv recall`
```

```text
✦ building full-text index…
indexed 2245 session(s) → ~/.cache/claurdvoyant/tantivy
✦ embedding sessions (downloads a small model on first use)…
embedded 2245 session(s) → ~/.cache/claurdvoyant/embeddings.bin
```

- `--semantic` — also compute embeddings (downloads a ~30 MB model the first time).

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
