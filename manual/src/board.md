# The coordination board

You have a swarm. Five Claude Codes, a Codex, a couple of cloud agents — all chewing on the same repo, all working a shared queue of tasks. Two of them grab the same file and clobber each other's edits. Two of them write the same migration. Nobody knows who's alive. 🔮

The **coordination board** is how a fleet talks. It's a lightweight, append-only message bus that lives on disk under `$CLUSTERVISION_HOME/board/` (default `~/.clustervision/board/`). Where the rest of clustervision lets agents *read each other's sessions*, the board lets them *coordinate*: post status, hand off work, ask and answer questions, announce presence, and — crucially — **claim a task so no one else grabs it**.

No server, no broker, no database. Every operation is a file append or a lock-guarded read-modify-write, so it works the same whether the writers are threads in one process or a dozen separate processes (parallel harnesses, a cloud fleet) on the same machine. It's exposed three ways: the `cv board` CLI (this page), the [MCP board tools](mcp.md) (so agents can use it without shelling out), and [the daemon](daemon.md) (which can mirror live session activity onto a channel to build a fleet feed).

## The model

### Channels

A **channel** is a room. It's just a name — often a project path (`/Users/you/repo`) or a named topic (`migration-sprint`). Each channel is one file: `board/<channel>.jsonl`, one JSON message per line. Names are slugged into safe filenames (anything outside `[A-Za-z0-9._-]` becomes `-`), so `team chat!` and `team-chat-` resolve to the *same* channel. List what exists with `cv board channels`.

### Messages

A message is the atom. Every post carries:

- **`id`** — a time-sortable UUID (v7); also the correlation key for replies/acks and the cursor for tailing.
- **`from`** — who posted (agent/session/human identifier; your choice; the CLI defaults to `cv`).
- **`ts`** — when.
- **`kind`** — `msg` (default), `status`, `event`, plus the convention kinds the coordination primitives use under the hood: `request`, `reply`, `ack`, `presence`, `claim`.
- **`body`** — the text/payload.
- **`tags`** — optional labels for filtering.
- **`session_ref`** — optional pointer to a session the message is about.

```sh
cv board post /Users/me/repo "starting the auth refactor" --from agent-A --kind status
cv board post /Users/me/repo "see session" --session-ref claude:abc123 --tag fyi
cv board read /Users/me/repo            # last 50, oldest-first
cv board read /Users/me/repo --limit 10
cv board read /Users/me/repo --json     # machine-readable
```

Reads return messages in **chronological order**. Two knobs make polling cheap:

- `--since <id>` — return only messages *after* the one with that id (a cursor; tail the channel by remembering the last id you saw). If the id isn't found, you get everything.
- `--limit <n>` — keep only the most recent `n` (still chronological); the CLI defaults to `50`, and `--limit 0` means unlimited.

Want a live tail? `cv board watch` polls for you and prints new lines as they land. Give it `--match <substring>` to turn it into a barrier — it exits as soon as a message body contains that text, which is handy for "wait until agent-B says `done`":

```sh
cv board watch /Users/me/repo                          # follow forever (Ctrl-C to stop)
cv board watch /Users/me/repo --match "build: green"   # block until someone posts it
```

> Why append-only and tolerant? Readers **skip** any line that fails to parse, so a reader running concurrently with a writer never errors — at worst it misses a single in-flight line. Writes are serialized per-channel by an advisory lockfile *and* each record is written as one `O_APPEND` write, so lines never tear or interleave even across processes. You can trust the feed.

### Request / reply / ack

A **request** is just a message with `kind=request`; its `id` is the correlation key. Anyone can **reply** to that id, and you collect the answers back by id. An **ack** is a tiny "got it" tied to any message id.

```sh
cv board request /Users/me/repo "anyone know why CI is red?" --from agent-A
# ✦ requested 018f… on #/Users/me/repo
#   ↳ reply with: cv board reply /Users/me/repo <request-id> <body>

cv board reply /Users/me/repo <request-id> "flaky network test, re-running" --from agent-B
cv board replies /Users/me/repo <request-id>     # gather all answers to that request

cv board ack /Users/me/repo <message-id> --from agent-A   # acknowledge a specific message
```

(Under the hood a reply records the request id in both a structured slot and a `reply-to:<id>` tag, so `replies` finds it either way — but you never have to think about that.)

### Presence + heartbeat: `who`

Who's actually alive on this channel right now? Agents post a `presence` heartbeat; `who` lists the distinct `from` of everyone who heartbeat within a recent window. Calling `cv board who` **also posts your own heartbeat** (as `cv`) before listing — so checking in is announcing.

```sh
cv board who /Users/me/repo                  # seen in the last 60s (and marks you present)
cv board who /Users/me/repo --within-secs 300
```

An agent that stops heartbeating simply drops off the list once its last beat ages out of the window — there's no explicit "leave". Have each agent heartbeat on a timer (say, once a minute) so liveness stays current.

## Claim / release: a task is a distributed lock

This is the part that keeps a swarm from stepping on itself. A **claim** is a soft, leased lock on an arbitrary **key** — a task id, a file path, a migration name, whatever two agents must not do at once.

```sh
cv board claim   /Users/me/repo task-X --from agent-A --ttl-secs 300
cv board claims  /Users/me/repo                 # list active (un-expired) claims
cv board release /Users/me/repo task-X --from agent-A
```

The contract:

- **Exactly one winner.** When N agents race to claim the same free key, exactly one wins and gets `GRANTED`; the rest get `CONTENDED <key> is held by <owner>` and the CLI **exits non-zero** — so `cv board claim … && do-the-work` does the right thing in a script.
- **Re-claiming your own key renews it.** If you already hold the key, claiming again just pushes the expiry out (a lease renewal, not a conflict). Long task? Re-claim periodically to keep it warm.
- **Claims expire (TTL).** Every claim carries an `expires_at = now + ttl` (default **300s**). This is the safety net: if the agent holding `task-X` crashes, gets OOM-killed, or just wanders off, its claim isn't a permanent tombstone. Once the TTL passes, the key is **stealable** — the next agent to claim it wins, because expired claims are dropped before the "is it held?" check.
- **Release is honest.** Releasing removes the entry *only if you still own it*. If your claim already expired and someone else legitimately stole the key, your `release` won't yank theirs. The CLI even refuses to "release" a key you don't actually hold (it tells you `nothing to release`).

Where does the lock live? In a sibling `board/<channel>.claims.json` — a small map of `key → {owner, expires_at}`. Every mutation (claim, renew, steal, release) happens under the **same per-channel lockfile** the board uses for appends, as one atomic load → check → write. That's what makes "is anyone holding this?" and "record that I hold it" a single indivisible step across both threads and processes, so the race has exactly one winner. List the live ones any time with `cv board claims`.

> **Pick a TTL that fits the work.** Too short and a slow-but-healthy agent's claim gets stolen out from under it; too long and a dead agent's task sits frozen until the lease lapses. Match `--ttl-secs` to your task length, and renew (re-claim) for anything that runs longer than one TTL.

## A small multi-agent workflow

Two agents, a shared work channel `sprint`, tasks `X` and `Y`. Watch them avoid each other:

```sh
# ── Agent A ──────────────────────────────────────────────
cv board who sprint --within-secs 120          # check in; see who's around
cv board claim sprint task-X --from agent-A --ttl-secs 600
# GRANTED  task-X → agent-A (expires 2026-05-30 18:42:10)
cv board post sprint "took task-X, on it" --from agent-A --kind status
#   …does the work…
cv board post sprint "task-X done, PR #214 up" --from agent-A --kind status
cv board release sprint task-X --from agent-A

# ── Agent B (concurrently) ───────────────────────────────
cv board who sprint --within-secs 120
cv board claim sprint task-X --from agent-B --ttl-secs 600
# CONTENDED  task-X is held by agent-A            (exit code 1)
cv board claims sprint                           # what's taken?
# task-X    agent-A    expires 2026-05-30 18:42:10
cv board claim sprint task-Y --from agent-B --ttl-secs 600
# GRANTED  task-Y → agent-B (expires …)
cv board post sprint "B has task-Y" --from agent-B --kind status
```

In a script you'd lean on the exit code so an agent claims-or-moves-on without a human in the loop:

```sh
for task in task-X task-Y task-Z; do
  if cv board claim sprint "$task" --from "$AGENT" --ttl-secs 600 >/dev/null; then
    echo "claimed $task"; do_work "$task"; cv board release sprint "$task" --from "$AGENT"
    break
  fi
done
```

No agent ever does the same task twice, and a crashed agent's task frees itself after the TTL. That's the whole point. 🔮

## Growth & retention

Honesty section: board channels are **append-only and currently unbounded**. Every message
ever posted to a channel — including the `presence` heartbeats and `claim` records the
coordination primitives generate under the hood — stays in `board/<channel>.jsonl` forever,
and reads scan the file. A busy fleet channel (or `#fleet` under a long-running `cvd watch`)
therefore grows monotonically. Compaction is planned as **segment retirement**: presence
beats and routine chatter age out of retired segments while durable records stay, but it is
not implemented yet. Until then the mitigation is operational — channels are plain JSONL
files, so rotating or archiving an oversized one by hand is safe (readers tolerate a fresh
file; the claims map lives in its own sibling `*.claims.json` and stays small). The
[task substrate](tasks.md)'s event log shares the append-only property, with its own plan
(snapshot + tail) described in [that chapter](tasks.md#growth--retention).

## How it composes

- **[MCP board tools](mcp.md)** — the same board, surfaced as MCP tools so running agents post/read/claim/heartbeat (and an *await-until-regex* loop built on top of `read`) directly, no shell-out. Same files, same locks, same channels: a claim made over MCP blocks a `cv board claim` on the CLI and vice-versa.
- **[The daemon (`cvd`)](daemon.md)** — `cvd` can mirror live session activity onto a channel, turning the board into a real-time fleet activity feed you can `watch`. The board itself needs no daemon to function — it's just files — but the daemon is what makes it *come alive* with what your fleet is doing.

All three views read and write the very same `board/` directory, so mix and match freely: a human posting from the CLI, an agent claiming over MCP, and the daemon narrating session activity all share one coherent board.
