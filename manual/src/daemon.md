# The daemon: `cvd`

`cvd` is the clustervision daemon — the piece that turns a pile of per-machine, per-harness session
files into a single durable **archive** and a live **fleet feed**. Where [`cv`](cli.md) is your
interactive map of *this* machine, `cvd` is the long-running thing that watches every harness's
on-disk storage, snapshots sessions into a central store, and (optionally) serves that state as JSON
so [the app](app.md)'s fleet dashboard can render it. 🔮

It has three jobs worth knowing:

- **`cvd sync`** — one-shot: snapshot every discovered session into the archive, then exit.
- **`cvd watch`** — long-running: follow live activity and archive sessions as they change, emitting a fleet activity feed.
- **`cvd serve`** — a tiny local HTTP API that exposes fleet state as JSON.

Plus two small utilities: `cvd ls` (list what's in the archive) and `cvd path` (print the archive location).

## The archive

Everything `cvd` persists lands under one **archive home**, resolved in this order:

1. the `--home <DIR>` flag (global — works on every subcommand),
2. the `$CLUSTERVISION_HOME` environment variable,
3. otherwise `~/.clustervision`.

Inside it:

```
~/.clustervision/
├── catalog.jsonl                 # cheap metadata, one row per (harness, id)
└── archive/
    └── <harness>/<id>.json       # the full parsed session, harness-agnostic IR
```

`catalog.jsonl` is an append-only JSONL index — lightweight rows for listings and search — while the
full transcript for each session lives in `archive/<harness>/<id>.json` as clustervision's unified
representation. Storing is **idempotent**: if a session's serialized bytes are byte-identical to
what's already on disk, `cvd` skips the write entirely. The catalog dedupes by `(harness, id)` with
the newest `archived_at` winning, and self-compacts once it's carrying too many superseded lines.

Point several machines at the same archive home (a synced folder, a network share) and you get one
centralized, harness-agnostic store of every agent session you've ever run.

```sh
cvd path     # print the archive location
cvd ls       # list archived sessions, newest activity first
```

## `cvd sync` — snapshot everything once

```sh
cvd sync
```

`sync` discovers every session across every supported harness, parses each into the unified
representation, and stores it. It's a one-shot: it archives, prints a summary, and exits.

```text
sync complete: 42 archived, 318 skipped (unchanged) of 360 discovered
archive: /Users/you/.clustervision
```

`skipped` means "already archived and unchanged" (the idempotent fast path), so re-running `sync`
is cheap. Parse/store failures are logged to stderr per-session and don't abort the run. This is the
command to put on a cron / launchd timer if you just want periodic backups without a resident
process.

## `cvd watch` — follow live + archive as it happens

```sh
cvd watch                          # follow everything
cvd watch --interval 5             # poll every 5s (default 3)
cvd watch --harness claude         # only one harness
cvd watch --cwd clustervision       # only sessions whose cwd contains this substring
cvd watch --verify-interval 0      # disable the periodic task verifier (default 300s)
```

`watch` is the resident counterpart to `sync`. On its first poll it archives whatever already
exists (so it's self-sufficient — no need to `sync` first), then it follows live activity and
re-archives each session as it changes. It runs until you kill it.

Every time it archives a changed session it logs the delta to stderr:

```text
cvd: watching (interval 3s) -> /Users/you/.clustervision
[new] claude   a1b2c3d4  +12 msgs  ~/code/clustervision
[archived] codex    9f8e7d6c  +3 msgs  ~/code/other-project
```

### The fleet feed 🔮

Beyond stderr logging, `watch` mirrors each archive event onto [the board](board.md)'s **`#fleet`**
channel — a live, centralized activity feed. Run `cvd watch` on several machines pointed at the same
board and `#fleet` becomes a cross-machine stream of "who's doing what right now." You can tail it
with `cv scry` (see [scry / cv](cli.md)) or read it over HTTP via `GET /api/board/fleet` below.

Each event is posted with the harness as a tag and the session id attached, so the feed stays
filterable.

### The periodic task verifier

`watch` is also the engine that keeps the [task substrate](tasks.md)'s landing state honest
without anyone asking. Every `--verify-interval` seconds (default **300**; `0` disables) it
runs the same git verifier as `cv task verify --all` over the whole task log: it observes
whether each reviewed `ready`/`merged_local` revision's content is on its upstream (appending
`landed` / `merged_local` / findings), re-observes revisions recorded `landed` (flagging
SUSPECT rows — forged or rolled-back lands), and writes the heartbeat
(`<home>/tasks/last_verify.json`) that the debt view's `verified_as_of` and staleness warning
are judged by. On startup it announces itself:

```text
cvd: task verifier every 300s
```

and logs each observation as it happens (`cvd: observed landed on task 019f6e4b`). Without a
running `cvd watch` (or manual `cv task verify` passes), the debt view says so loudly —
*"landing state has NEVER been verified"* / *"verifier heartbeat is STALE"*.

## `cvd serve` — the local HTTP API

```sh
cvd serve                      # http://127.0.0.1:7777
cvd serve --port 8080          # pick a port
cvd serve --token s3cret       # require `Authorization: Bearer s3cret` on /api/* ($CVD_TOKEN works too)
cvd serve --host 0.0.0.0 --token s3cret   # expose beyond loopback (demands a token or --insecure-expose)
```

`serve` is a tiny synchronous HTTP server (a small worker pool, no async stack) that exposes fleet
state as JSON. This is exactly what [the app](app.md)'s live **fleet dashboard** polls. It binds
`127.0.0.1:7777` by default and runs until killed.

`--port 0` binds an **ephemeral port**, and the startup banner always carries the *real* bound
port — so a script or test harness can parse the address instead of racing to guess a free one:

```text
cvd serve on http://127.0.0.1:54573 — API at /api/* (no --web; JSON only)
```

Two things worth noting up front:

- **It reads *live* sessions, not the archive.** `serve`'s session endpoints call into the same
  discovery/parse engine `cv` uses, so they reflect the current on-disk state of each harness in
  real time. (The archive is what `sync`/`watch` build for durability and search; `serve` is the
  live window.) The board endpoints read [the board](board.md) directly.
- **Every response is JSON, and the API is locked to local callers.** Your transcripts can contain
  secrets, so `serve` treats them that way:
  - **CORS is an allow-list, never `*`.** Only local origins (`http://localhost:*`,
    `http://127.0.0.1:*`, `[::1]`, your configured `--host`) and the desktop app's Tauri origins get
    an `Access-Control-Allow-Origin` echo; a random website you happen to have open gets nothing,
    so its scripts can't read the corpus. `OPTIONS` preflight is answered with a `204`.
  - **The `Host` header must name this machine** (`127.0.0.1`, `localhost`, `[::1]`, or your
    `--host`) — anything else is a `403`. This defeats DNS rebinding, where a hostile page points
    its own domain at `127.0.0.1` to sidestep CORS entirely.
  - **Optional bearer token.** Set `--token <t>` (or `$CVD_TOKEN`) and every `/api/*` request must
    carry `Authorization: Bearer <t>` or get a `401`.
  - **Non-loopback binds demand an opt-in.** `--host 0.0.0.0` (or any non-loopback address) is
    refused unless you set a token or pass `--insecure-expose`, and warns loudly either way —
    anyone who can reach the port can read every transcript.

  Only `GET` is supported; anything else is a `405`.

### Routes

All routes are `GET` and live under `/api`. Unknown paths return `404`; a bad `harness` name returns
`400`.

#### `GET /api/health`

Liveness plus the set of harnesses this build knows about.

```http
GET /api/health
```

```json
{
  "ok": true,
  "harnesses": ["claude", "codex", "gemini", "..."]
}
```

#### `GET /api/sessions`

Session metadata, filtered and **newest-activity-first** (sorted by `updated_at`, falling back to
`created_at`). Query parameters, all optional:

| param     | meaning                                              |
|-----------|------------------------------------------------------|
| `harness` | only this harness (an unknown name → `400`)          |
| `cwd`     | only sessions whose cwd *contains* this substring    |
| `limit`   | keep at most this many (applied after sorting)       |

```sh
curl 'http://127.0.0.1:7777/api/sessions?harness=claude&limit=2'
```

```json
[
  {
    "id": "a1b2c3d4-5e6f-7890-abcd-ef0123456789",
    "harness": "claude",
    "cwd": "/Users/you/code/clustervision",
    "title": "wire up the fleet dashboard",
    "updated_at": "2026-05-30T18:42:10Z",
    "created_at": "2026-05-30T17:05:01Z",
    "message_count": 87
  },
  {
    "id": "9f8e7d6c-1234-5678-9abc-def012345678",
    "harness": "claude",
    "cwd": "/Users/you/code/other-project",
    "title": "debug the retry backoff",
    "updated_at": "2026-05-30T16:20:44Z",
    "created_at": "2026-05-30T15:58:12Z",
    "message_count": 31
  }
]
```

#### `GET /api/session/{harness}/{id}`

The full parsed session — the complete unified-representation transcript as JSON — or `404` if no
such session is found.

```sh
curl 'http://127.0.0.1:7777/api/session/claude/a1b2c3d4-5e6f-7890-abcd-ef0123456789'
```

#### `GET /api/session/{harness}/{id}/subagents`

Lightweight refs (same shape as `/api/sessions` rows) for the sub-agents this session spawned — for
example Claude Code `Task` sub-agents — newest first. Returns an empty array if there are none.

```sh
curl 'http://127.0.0.1:7777/api/session/claude/a1b2c3d4.../subagents'
```

#### `GET /api/session/{harness}/{parent}/subagent/{agent}`

The full parsed transcript of **one** sub-agent. Sub-agents don't live in the main session pool, so
they're loaded relative to their `{parent}`. `404` if either the parent or the named sub-agent isn't
found.

#### Board endpoints

These read [the board](board.md) directly (see that chapter for the data model):

- **`GET /api/board/{channel}`** — board messages. Optional `?since=<id>` and `?limit=<n>` (where
  `limit=0`, the default, means unlimited). Try `/api/board/fleet` for the live fleet feed `cvd
  watch` writes.
- **`GET /api/board/{channel}/unanswered`** — requests with **zero** replies, oldest first, each
  as `{ "message": {...}, "age_secs": n }`. Optional `?within_secs=<n>` (default `86400` = 24h).
- **`GET /api/claims/{channel}`** — active leases on the channel, each as `{ "key", "owner", "expires_at" }`.
- **`GET /api/who/{channel}`** — agents recently present on the channel. Optional `?within_secs=<n>`
  (default `60`).
- **`GET /api/channels`** — the list of known board channels.

```sh
curl 'http://127.0.0.1:7777/api/claims/fleet'
```

```json
[
  { "key": "deploy", "owner": "claude:a1b2c3d4", "expires_at": "2026-05-30T18:50:00Z" }
]
```

#### Task endpoints

These serve the [task substrate](tasks.md) — the same shared row shapes as `cv task --json`
and the MCP `task_*` tools. Every reply carries a `warnings` array: the task log's replay
warnings ride every response, so a corrupted coordination log is visible from any consumer.

- **`GET /api/tasks`** — task rows, oldest first. Optional `?state=`, `?assignee=`, `?repo=`,
  and `?all=1` (include terminal tasks). An unknown `state` is a `400` naming the whole
  vocabulary, never a silently-empty list.

  ```json
  {
    "tasks": [
      {
        "id": "019f6e4b-74c5-74f0-a493-1f8131ca38d5",
        "title": "switch task ids to uuid v7",
        "effective_state": "rev:landed",
        "assignee": "agent:mira",
        "repo": "/Users/you/code/demo",
        "channel": "tasks",
        "opened_at": "2026-07-17T04:17:46.694052Z",
        "last_ts": "2026-07-17T04:18:05.206768Z"
      }
    ],
    "warnings": []
  }
  ```

- **`GET /api/task/{id}`** — one task's full projection (id prefix ok; `404` otherwise), as
  `{ "task": {...}, "effective_state": "rev:landed", "warnings": [] }`. The `task` object is
  the complete read model: revisions with review/land evidence, notes, recorded issues.
- **`GET /api/task/{id}/events`** — the raw durable history, `{ "events": [...], "warnings": [] }`,
  each event carrying `id`, `task_id`, `ts`, `by`, and its tagged `event` payload
  (`opened`, `claimed`, `revision_proposed`, `review_passed`, `landed`, …).
- **`GET /api/tasks/debt`** — the honest-ledger envelope (optional `?repo=` filter):

  ```json
  {
    "debt": [
      {
        "id": "019f6e4c-5cc6-72a1-b358-a178d650e226",
        "title": "add retry backoff",
        "repo": "/Users/you/code/demo",
        "revision": 1,
        "branch": "task/retry-backoff",
        "upstream": "main",
        "state": "ready",
        "since": "2026-07-17T04:18:46.268959Z",
        "issues": [],
        "suspect": false
      }
    ],
    "awaiting_review": [],
    "suspects": [],
    "verified_as_of": "2026-07-17T04:18:46.612684Z",
    "verify_warning": null,
    "warnings": []
  }
  ```

  `debt` rows are reviewed-but-unlanded revisions (each with a literal `"suspect": false` —
  this surface's dashboard keys off the marker); `awaiting_review` rows are proposed revisions
  whose reviewer hasn't spoken (with `reviewer` and `since`); `suspects` are revisions recorded
  `landed` whose content the verifier no longer observes on upstream (`"suspect": true` rows);
  `verified_as_of` / `verify_warning` carry the verifier heartbeat — `verify_warning` names a
  never-run or stale verifier, and is `null` when the heartbeat is fresh.
- **`GET /api/tasks/inbox/{who}`** — what needs this endpoint, stalest first:
  `{ "inbox": [ { "id", "title", "reason", "effective_state" } ], "warnings": [] }`, where
  `reason` is one of `assigned_open`, `claimed_by_you`, `awaiting_your_review`,
  `your_unlanded_work`.

## Putting it together

A common setup on a workstation that's part of a fleet:

```sh
cvd watch &                    # archive + feed #fleet, forever
cvd serve --port 7777          # let the app's dashboard poll live state
```

…with the archive home on a synced folder so every machine contributes to one corpus. From there:

- browse and visualize it all in [the app](app.md),
- tail the live feed with `cv scry` (see [the CLI](cli.md)),
- and coordinate running agents through [the board](board.md).
