# MCP — agents reading each other's minds

Most of clustervision is about *you* reading sessions after the fact. `cv-mcp` is the
other direction: it hands the same powers to a **running coding agent** so it can read
*other* agents' sessions — within this project, back through time, and across harnesses
— and coordinate with siblings live. 🔮

It's a [Model Context Protocol](https://modelcontextprotocol.io) server: a thin stdio
front-end over the `cv_core` library. Point Claude Code (or Codex, Gemini, …) at it and
the agent gets a handful of tools.

## Motivating use cases

- **"What happened in this project before?"** — `project_sessions(cwd)` lists every
  session (any harness) whose recorded working directory is this project, newest first.
  The agent reads the relevant one with `read_session(id)`.
- **"Has anyone solved this before?"** — `recall(query)` semantically searches the whole
  cross-harness corpus and pulls back the most relevant *message spans*, so the agent can
  fold prior context into a running task. See [search/recall](search.md).
- **"What's my sibling agent doing right now?"** — `list_sessions` / `search_sessions`
  surface live sessions; `board_who` shows who's present on a channel.
- **"Block until another session prints X."** — `await_omen(regex=…)` watches live
  message streams and returns when one matches; `board_await` does the same for explicit
  [board](board.md) posts.

## Registering it

Build the binary, then register it with your host. For Claude Code:

```sh
cargo build -p cv-mcp --release
claude mcp add clustervision -- /absolute/path/to/cv-mcp
```

Any MCP host works — give it the absolute path to the `cv-mcp` binary as the command
(no arguments needed). Under the hood it speaks **line-delimited JSON-RPC 2.0 over
stdio**: one JSON message per line on stdin, one per line on stdout. **stdout is the
protocol channel**, so all diagnostics go to stderr. It implements `initialize`, `ping`,
`tools/list`, and `tools/call` against protocol version `2025-06-18`.

Every tool returns its result as a text content block. The read/search/board tools return
**pretty-printed JSON** as that text; `read_session` can also return rendered Markdown.

## Reading sessions

These four tools find and read transcripts. Arguments marked **required** must be present.

### `list_sessions`

Recent sessions across all harnesses, newest first.

| Arg | Type | Default | Meaning |
|-----|------|---------|---------|
| `harness` | string | — | Restrict to one harness: `claude`, `codex`, `grok`, `opencode`, `gemini`. |
| `cwd_contains` | string | — | Only sessions whose recorded cwd contains this substring. |
| `limit` | number | `40` | Max results. |

Returns an array of session refs (`id`, `harness`, `cwd`, `title`, `updated_at`,
`created_at`, `message_count`). *When to use:* a quick "what's been going on lately"
glance, or to find a session id to read.

### `search_sessions`

Full-text search across transcripts — case-insensitive substring over all message
content. Sessions are parsed on demand.

| Arg | Type | Default | Meaning |
|-----|------|---------|---------|
| `query` | string | **required** | Text to search for. |
| `harness` | string | — | Restrict to one harness. |
| `cwd_contains` | string | — | Only sessions whose cwd contains this substring. |
| `limit` | number | `20` | Max results. |

Returns matching session refs, each with a `snippet` (~280 chars) around the first hit.
*When to use:* find a past conversation by something that was literally said in it.

### `read_session`

The full transcript by id (a unique **prefix** of the id is accepted).

| Arg | Type | Default | Meaning |
|-----|------|---------|---------|
| `id` | string | **required** | Session id, or a unique prefix. |
| `harness` | string | — | Disambiguate if the id/prefix is ambiguous. |
| `format` | string | `markdown` | `markdown` (rendered) or `json` (full structured session). |

*When to use:* once `list`/`search`/`project_sessions`/`recall` hands back an id you want
to read in full.

### `project_sessions`

Sessions whose recorded working directory **equals or contains** the given path, newest
first. Path matching is component-aware (so `/foo` won't match `/foobar`), and a bare
project name matches by trailing components.

| Arg | Type | Default | Meaning |
|-----|------|---------|---------|
| `cwd` | string | **required** | Project path (or name) to match against sessions' cwd. |
| `limit` | number | `20` | Max results. |

*When to use:* the headline tool — "what happened in THIS project before, and what are
sibling agents doing here?"

## Pruning — custom lossless compaction

### `prune_session`

Compact a Claude session into a **new, resumable** one by snipping bulky *old* tool payloads into a sidecar (see [`cv prune`](cli.md#cv-prune)). The source is never modified; resume the new id with `claude --resume`.

| Argument | Type | Default | Meaning |
| --- | --- | --- | --- |
| `id` | string | **required** | Source session id (or unique prefix). |
| `min_size` | number | `2048` | Only snip a tool payload larger than this many bytes. |
| `keep_last` | number | `25` | Keep the last N conversational turns' payloads verbatim. |
| `to` | string | — | New session id (default: a fresh UUID). |
| `drop` | boolean | `false` | Hard-drop payloads (no sidecar, irreversible). |
| `dry_run` | boolean | `false` | Report what would happen without writing. |

Returns JSON with the new id, paths, counts, byte/token savings, and the `claude --resume` line.

### `prune_retrieve`

Fetch a stashed original back out of a pruned session's sidecar.

| Argument | Type | Default | Meaning |
| --- | --- | --- | --- |
| `session` | string | **required** | The pruned session id the `[PRUNED id=…]` marker names. |
| `tool_use_id` | string | **required** | The id from the marker (e.g. `toolu_…` or `…#tur`). |

## Recall — semantic prior-work search

### `recall`

"Have I (or another agent) solved/seen this before?" Semantically searches the whole
cross-harness corpus and returns the most relevant message **spans** — a short excerpt
of the messages around each best match — not just metadata.

| Arg | Type | Default | Meaning |
|-----|------|---------|---------|
| `query` | string | **required** | What you're trying to do / the problem to find prior work on. |
| `k` | number | `5` | Max sessions to return. |
| `harness` | string | — | Restrict results to one harness. |

Returns `{ mode, query, results[] }`, where each result has `id`, `harness`, `cwd`,
`title`, `score`, `why` (why it matched), and `span` (the rendered excerpt). `mode` is
`"semantic"` when an embedding store is available and `"text"` when it fell back to
keyword search. For meaning-based results, build the embedding store first with
`cv index --semantic`; without it, recall **degrades gracefully** to full-text search.
See [search/recall](search.md) for the search machinery.

## Awaiting live activity

### `await_omen`

Block until any other agent's session emits a message matching a regex, then return it.
Watches **live** for new and appended messages — text, thinking, tool results, tool uses
— across harnesses, reacting only to activity from *now on* (not the backlog).

| Arg | Type | Default | Meaning |
|-----|------|---------|---------|
| `regex` | string | **required** | Rust regex matched against newly-emitted message text. |
| `harness` | string | — | Only watch this harness. |
| `cwd_contains` | string | — | Only watch sessions whose cwd contains this substring. |
| `timeout_secs` | number | `120` | Give up after this many seconds. |
| `interval_secs` | number | `2` | Poll interval (minimum 1). |

On a match returns `{ matched: true, harness, id, cwd, role, matched_text, event }`
(`event` is `new_session` or `appended`); on timeout returns
`{ matched: false, timed_out: true, waited_secs }`. *When to use:* wait on a sibling
agent's raw transcript — e.g. `await_omen(regex='BUILD (PASSED|FAILED)', cwd_contains='/myproj')`.
For explicit, structured coordination, prefer the board (below).

## The coordination board

The board is a lightweight pub/sub + locking layer agents use to talk to each other
directly. Full conceptual treatment is in [the board](board.md); here are the MCP tools.
Most take a `channel` (a room name, often a project path or topic) and an optional `from`
(who's posting, default `"agent"`). Results come back as pretty JSON.

### Messaging

| Tool | Required args | Optional args | Purpose / when to use |
|------|---------------|---------------|------------------------|
| `board_post` | `channel`, `body` | `from`, `kind` (`msg`\|`status`\|`event`, default `msg`), `tags[]`, `session_ref` | Broadcast status, leave a note, or hand off work so other agents see it. |
| `board_read` | `channel` | `since` (id cursor), `limit` (default `50`, `0`=all) | Read recent posts; pass `since` to get only newer ones. |
| `board_await` | `channel`, `regex` | `since` (default: current tail), `timeout_secs` (`120`), `interval_secs` (`2`) | Block until a **new** post matches the regex (or timeout). The clean way to wait on a sibling: `board_await(channel='myproj', regex='BUILD (PASSED|FAILED)')`. |
| `board_ack` | `channel`, `message_id` | `from` | Acknowledge a message by id with a tiny ack note, so the sender knows it was seen. |

`board_await` returns `{ matched, message?, cursor }` on a hit, or
`{ matched: false, timed_out: true, cursor }` on timeout.

### Request / reply

A small Q&A protocol layered on the board, correlated by message id.

| Tool | Required args | Optional args | Purpose / when to use |
|------|---------------|---------------|------------------------|
| `board_request` | `channel`, `body` | `from` | Post a question other agents can answer. Returns the message **including its `id`** — keep it to collect answers. |
| `board_reply` | `channel`, `in_reply_to`, `body` | `from` | Answer a prior request, correlated by its id. |
| `board_replies` | `channel`, `request_id` | — | Collect all replies to a request id, in chronological order. |

### Claims (soft distributed locks)

So two agents don't grab the same task.

| Tool | Required args | Optional args | Purpose / when to use |
|------|---------------|---------------|------------------------|
| `board_claim` | `channel`, `key` | `from`, `ttl_secs` (`300`) | Try to claim a task `key`. Returns `{granted: bool, lease?}`; if `granted` is false someone else holds it, so pick a different task. Renews your own claim if you already hold it. |
| `board_release` | `channel`, `key` | `from` | Release a key you claimed, freeing it for others. Idempotent; only releases a claim you own (returns `{released: false, reason}` otherwise). |
| `board_claims` | `channel` | — | List active (un-expired) claims as `{key, owner, expires_at}` — see who's working on what. |

A claimed `key` is typically a file path or task id. Always `board_release` when done
(or let the `ttl_secs` lease expire so it can be stolen).

### Presence

| Tool | Required args | Optional args | Purpose / when to use |
|------|---------------|---------------|------------------------|
| `board_heartbeat` | `channel`, `from` | — | Announce you're alive on a channel. Call periodically to stay "active". |
| `board_who` | `channel` | `within_secs` (`120`) | List agents that heartbeat within the window — active presence. |

## A typical flow

1. An agent starts work in `/myproj`. It calls `project_sessions(cwd='/myproj')` to see
   prior history, and `board_who(channel='myproj')` to see which siblings are live.
2. It `board_heartbeat`s, then `board_claim(channel='myproj', key='src/auth.rs')` so no
   one else touches that file.
3. Mid-task it hits a wall and `recall(query='refresh token rotation')`s the corpus for
   how it was solved before, reading the winner with `read_session(id)`.
4. It hands off: `board_post(channel='myproj', kind='status', body='auth done, tests green')`
   and `board_release(channel='myproj', key='src/auth.rs')`.
5. A sibling that ran `board_await(channel='myproj', regex='auth done')` unblocks and
   picks up the next task.
