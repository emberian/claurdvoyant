# clustervision × mentor-observe — a senior agent watching a junior's stream

> The **non-blocking** coordination recipe: a senior/orchestrator agent **polls a junior
> agent's live activity on its own cadence** — draining the junior's newly-appended messages
> whenever it chooses, instead of *blocking* on a single expected signal.

This is not a harness adapter (the junior runs in whatever harness it likes — Claude Code,
Codex, …, already covered by the per-harness recipes). It's a **usage pattern** built from two
pieces clustervision already ships:

1. **`cvd watch`** — the daemon archives the junior's sessions into `~/.clustervision` live, so
   the senior can see them from another process (or another machine on a shared corpus).
2. **`observe_stream`** (MCP) — the senior drains the junior's incremental message tail and
   returns immediately, with a cursor to resume from next time.

## Why not just `await_omen`?

`await_omen` is the **blocking** primitive: it parks the senior until *one* regex matches, then
returns that single hit. That's perfect for "wake me when the build says PASSED/FAILED". But a
mentor often wants the opposite shape — **stay free, glance at what the junior has been doing,
decide what to do next, repeat** — without committing to a single expected string or holding a
blocking call open. `observe_stream` is that sibling:

| | `await_omen` | `observe_stream` |
|---|---|---|
| Blocks? | Yes — until match or timeout | **No** — returns immediately |
| Returns | the first message matching a regex | **all** newly-appended messages since a cursor |
| Cursor? | n/a (one-shot) | **yes** — resume the tail across calls |
| Use it to… | wait on a known signal | poll a junior's activity on your own cadence |

Both reuse the exact same live substrate (`cv_core::watch` — `Watcher` / `Filter` /
`EventKind`) and the same cross-harness parser, so they see identical message streams.

## Setup

1. **Build the binaries** (repo root):

   ```sh
   cargo build --release   # → target/release/{cv, cv-mcp, cvd}
   ```

2. **Keep the junior's sessions flowing into the corpus.** On the box where the junior runs
   (or anywhere that can read its session files), start the watcher scoped to the junior's
   project so its turns are archived live:

   ```sh
   cvd watch --cwd /path/to/junior-project
   ```

   (Or just `cvd watch` to archive every harness's sessions. If the junior is wired up via its
   own per-harness recipe, its SessionEnd/Stop hooks already feed `cvd` — `cvd watch` adds the
   live, between-turns archival a mentor wants.)

3. **Register the MCP server for the senior orchestrator** so it can call `observe_stream`:

   ```sh
   # Claude Code senior:
   claude mcp add clustervision -- /path/to/target/release/cv-mcp
   ```

   (Any MCP host works — see the per-harness recipes for Codex/Gemini/OpenCode/… registration.)

## The polling loop (what the senior does)

The senior calls `observe_stream` scoped to the junior's project. The **first** call records a
baseline and returns *no* backlog; every subsequent call, passing the returned `cursor`, drains
only what the junior appended since:

```jsonc
// 1) baseline — start following from "now"
observe_stream({ "cwd_contains": "/path/to/junior-project" })
//    → { "baseline": true, "count": 0, "messages": [], "cursor": "<C0>", "more_pending": false }

// 2) …let the junior work, then glance again with the cursor:
observe_stream({ "cwd_contains": "/path/to/junior-project", "since_cursor": "<C0>" })
//    → { "baseline": false, "messages": [ {role, session_id, harness, cwd, timestamp, text}, … ],
//        "count": N, "more_pending": false, "cursor": "<C1>" }

// 3) repeat with <C1>, <C2>, … on whatever cadence the senior likes.
```

Arguments:

- `cwd_contains` / `harness` — scope to the junior (same filter semantics as `await_omen`).
- `since_cursor` — the opaque cursor from the previous call. **Omit it on the first call** to
  baseline from the current tail (so you don't dump the junior's whole history).
- `max_messages` (default 50) — bound the batch. If more is waiting, `more_pending: true` is set
  and the returned cursor lets you drain the rest on the next call.
- `char_cap` (default 16000) — bound total characters of returned text; the last message is
  truncated to fit.

It is **read-only**: `observe_stream` parses and tails sessions — it never writes the board. To
hand work back to the junior, the senior uses the `board_*` tools (e.g. `board_post`,
`board_request`).

## Worked pattern: mentor a junior across a task

```text
senior: observe_stream(cwd_contains="/proj")                    # baseline → cursor C0
junior: …writes code, runs tests…
senior: observe_stream(cwd_contains="/proj", since_cursor=C0)   # sees the test output → C1
senior: board_post(channel="proj", body="tests look flaky — pin the seed")   # steer
junior: …applies the steer…
senior: observe_stream(cwd_contains="/proj", since_cursor=C1)   # confirms the fix landed → C2
```

The senior never blocks: between glances it can reason, watch *other* juniors, or wait on a
hard signal with `await_omen`. `observe_stream` is the cheap, repeatable "what's my junior up
to?" probe that makes a one-mentor-many-juniors loop practical.

## Honesty

- `observe_stream` is an **MCP tool** (in `cv-mcp`), the non-blocking sibling of `await_omen`.
  There is no `cv observe` CLI subcommand — the live tail is exposed over MCP (and `cv scry` /
  `cvd` provide the CLI/daemon live-follow).
- The cursor is opaque and self-describing; treat it as a token to pass straight back. A cursor
  from an older build degrades gracefully to a fresh baseline.
- No `cv distill` / `cv recall` CLI subcommands exist in this build (see the other recipes).
