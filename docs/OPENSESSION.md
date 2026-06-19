# 🧬 OpenSession — an interchange format for agent sessions

*Status: draft 0.2 · a clustervision proposal · feedback very welcome*

Every coding-agent harness records its conversations, and **every single one invented its own format**:
Claude threads by UUID, Codex emits an event stream (twice — once for the UI, once for the model),
Grok splits a session across four files in a percent-encoded directory, OpenCode stores one JSON file
per message *and* per content part, Gemini/Antigravity uses opaque protobuf, Hermes uses SQLite, OpenClaw
uses yet another JSONL dialect. We've now parsed all of them (see [`FORMATS.md`](FORMATS.md)) and the
striking thing is **how similar they actually are underneath**. They all encode the same handful of ideas.

OpenSession writes those ideas down. It is the format the harnesses would have agreed on if they'd talked
first. clustervision's in-memory IR is the reference implementation, and `cv export --format json` emits it.

## Design principles (the lessons, earned the hard way)

1. **A session is an ordered list of messages, plus provenance.** That's the whole spine. Don't overthink it.
2. **The working directory is just metadata, never identity.** The original sin of every harness is coupling
   a session to its cwd (in the *filename*, no less) so you can only resume it from there. OpenSession records
   `cwd` as a plain field you can change freely. Portability is the default.
3. **Content is a list of typed blocks**, because a single assistant turn legitimately contains reasoning,
   prose, and several tool calls. A flat string can't represent that without lying.
4. **Four roles, normalized:** `system`, `user`, `assistant`, `tool`. Several harnesses smuggle tool results
   into a `user` turn — OpenSession promotes them to `tool` so conversions don't misattribute them.
5. **Reasoning is first-class but may be opaque.** Codex and Grok ship *encrypted* reasoning blobs. Model
   them honestly: a `thinking` block carries plaintext when available and an opaque `encrypted` payload when
   not. Never pretend opaque data is readable.
6. **Tool calls and their results are linked by id**, not by adjacency — interleaving and parallel calls are real.
7. **Lossy-but-honest beats lossless-but-brittle.** Keep a per-message `extra` bag for harness-specific fields
   so round-trips preserve what they can, but the core stays small enough that *every* harness can fill it.
8. **Be tolerant on the way in.** Real transcripts have corrupt lines, missing fields, and multiple historical
   format versions. A parser that rejects a session because one line is malformed is worse than useless.

## The schema (v0.2)

```jsonc
{
  "openSession": "0.2",            // format version
  "harness": "claude",             // origin: claude|codex|grok|opencode|gemini|hermes|openclaw|...
  "id": "da9174f4-…",              // session id (native if possible)
  "cwd": "/Users/ember/pug/x",     // METADATA, not identity — freely rewritable
  "title": "…",                    // human/AI label (optional)
  "model": "claude-opus-4-8",      // primary model (optional)
  "createdAt": "2026-05-29T21:…Z", // ISO-8601 (optional)
  "updatedAt": "2026-05-29T22:…Z",
  "git": { "branch": "main", "commit": "…", "remote": "…" },   // optional
  "extra": { },                    // session-level harness-specific passthrough (optional)
  "messages": [
    {
      "id": "uuid",                // optional
      "parentId": "uuid|null",     // optional threading (DAG); omit for linear
      "role": "assistant",         // system|user|assistant|tool
      "timestamp": "…",            // optional
      "model": "…",                // optional, per-message
      "usage": { "inputTokens": 0, "outputTokens": 0,
                 "cacheReadTokens": 0, "cacheCreationTokens": 0 },   // optional
      "content": [                 // ordered, typed blocks
        { "kind": "thinking", "text": "…", "signature": "…", "encrypted": "…", "redacted": false },
        { "kind": "text", "text": "…" },
        { "kind": "toolUse", "id": "call_1", "name": "Bash", "input": { "command": "ls" } },
        { "kind": "toolResult", "toolUseId": "call_1", "content": "…", "isError": false,
          "toolName": "Bash", "status": "completed", "details": { } },
        { "kind": "file", "mime": "application/pdf", "path": "spec.pdf", "source": "file:///…" },
        { "kind": "image", "mediaType": "image/png", "dataRef": "…" }
      ],
      "extra": { }                 // harness-specific passthrough (optional)
    }
  ]
}
```

### Block kinds

| `kind` | meaning | key fields |
|---|---|---|
| `text` | plain prose | `text` |
| `thinking` | reasoning / chain-of-thought | `text`, `signature?`, `encrypted?` (opaque blob), `redacted?` (provider-redacted flag) |
| `toolUse` | a tool/function invocation | `id`, `name`, `input` (arbitrary JSON) |
| `toolResult` | the result of one | `toolUseId`, `content`, `isError`, `toolName?`, `status?`, `details?` (structured) |
| `file` | a file/dir/resource attachment (never inlined bytes) | `mime?`, `path?`, `source?` (uri/ref) |
| `image` | an image reference (never inlined bytes) | `mediaType?`, `dataRef?` |

New block kinds are additive; consumers MUST ignore kinds they don't recognize (forward-compatibility).
v0.2 added the `file` kind, the `toolResult` `toolName`/`status`/`details` fields, the `thinking` `redacted`
flag, and a session-level `extra` bag — all additive over v0.1.

## What's deliberately *not* in v0.1

- **System prompts / tool schemas** — huge, harness-specific, and rarely portable. Out of scope for now
  (a harness may stash them in `extra`).
- **Project context files** (`CLAUDE.md`, `MEMORY.md`, `AGENTS.md`) — these live *next to* a session, not
  inside it. clustervision's `cv port` carries them alongside; OpenSession may grow an optional `attachments`
  array later.
- **Billing/cost** — recorded by some harnesses; belongs in `extra` until there's demand.

## Versioning & historical variants

`openSession` is the format version of *this document*. The messy reality is that each *source* harness also
drifts over time (Codex alone has ≥3 on-disk shapes). A faithful importer must recognize those by **content,
not by a version field** — see [`ADDING_HARNESS.md`](../ADDING_HARNESS.md). OpenSession's job is to be the
stable target all those variants converge onto.

> Galaxy-brained by pug. If you ship a harness, please consider emitting OpenSession too — then everyone's
> sessions are portable by construction. 🤝
