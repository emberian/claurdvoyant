# Cross-harness conversion

Every agent harness stores its sessions a little differently — Claude Code threads JSONL
by `parentUuid`, Codex writes dated `rollout-*.jsonl` files, OpenCode shards messages and
parts across directories, Cline buries the working directory inside the first user message.
clustervision's trick is that it never converts harness-A-format *directly* into
harness-B-format. Instead, **every harness parses _into_ one unified IR**, and conversion
is just:

```text
   ┌────────────┐     parse(A)      ┌──────────┐     emit(B)      ┌────────────┐
   │ harness A  │ ────────────────▶ │    IR    │ ───────────────▶ │ harness B  │
   │ (on disk)  │                   │ Session  │                  │ (on disk)  │
   └────────────┘                   └──────────┘                  └────────────┘
                                         ▲
                                  one shared shape:
                          Session → Message → Block{…}
```

Because the IR sits in the middle, you don't need an N×N matrix of converters — you need
N parsers and a handful of emitters. **17 harnesses can be parsed** _into_ the IR
([harnesses](harnesses.md)); of those, **13 can also be conversion _targets_** that the IR
can be emitted back _out_ to. Any emit-capable target can receive a session that originated
in *any* of the 17, so the practical conversion space is "17 sources × 13 targets". 🔮

The IR itself is defined in `crates/cv-core/src/ir.rs`; the emit side lives in
`crates/cv-core/src/emit.rs`. For the broader picture of how parsers and emitters fit
together, see [architecture](architecture.md).

## The 13 emit targets

These are the harnesses `emit()` can currently write — the authoritative list is
`supported_targets()` in `emit.rs`:

| target | id | notes |
|---|---|---|
| Claude Code | `claude` | threaded JSONL under an encoded project dir |
| Codex | `codex` | dated `rollout-*.jsonl` |
| Grok | `grok` | `summary.json` + `chat_history.jsonl` |
| OpenCode | `opencode` | sharded session / message / part files |
| OpenClaw | `openclaw` | parent-linked `agents/<id>/sessions/*.jsonl` |
| Gemini CLI | `gemini` | legacy `ConversationRecord` JSON |
| Hermes | `hermes` | SQLite `state.db` *(requires the `sqlite` build feature)* |
| Kimi CLI | `kimi` | `~/.kimi` transcript |
| LM Studio | `lmstudio` | chat-app conversation JSON |
| Cline | `cline` | per-task `api_conversation_history.json` |
| Roo Code | `roo` | Cline-format per-task dir |
| Continue | `continue` | `~/.continue/sessions` JSON |
| Qwen Code | `qwen` | reuses Gemini's `ConversationRecord` emitter |

> The other four parseable harnesses — **Cursor**, **Goose**, the **Claude desktop app**,
> and the **ChatGPT desktop app** — are parse-only. You can read, search, and convert
> *away* from them, but they aren't conversion targets yet (their on-disk stores are
> harder to write back safely). Asking to emit to one gives a clear "not supported yet"
> error rather than corrupting anything.

## `cv convert` — change harness format

`cv convert` parses a session, then emits it into another harness's native format:

```sh
# Convert a Codex session into Claude Code's format.
cv convert <id> --to claude

# Source harness is auto-detected from the id; pass --from to disambiguate.
cv convert <id> --from codex --to opencode

# Dry run: write under a scratch dir instead of the target's real storage root.
cv convert <id> --to gemini --out /tmp/try-gemini

# Rehome to a new working directory while converting.
cv convert <id> --to claude --cwd ~/work/other-project
```

By default `convert` writes into the target harness's real storage root (so the converted
session shows up when you launch that harness), and prints a resume hint:

```text
✦ wrote /Users/you/.claude/projects/-Users-you-proj/9f3c….jsonl (9f3c…)
  ↳ claude --resume 9f3c…  (run from /Users/you/proj)
```

If the target harness doesn't appear to be installed, `cv` asks you to pass `--out <dir>`
rather than guessing where its store lives.

## `cv port` — rehome a session

`cv port` is conversion's sibling for *moving* a session. It defaults to the **same**
harness (a pure rehome) but takes `--to` to combine rehoming with a format change:

```sh
# Rehome a session to a new working directory (same harness).
cv port <id> --to-dir ~/work/new-checkout

# Rehome *and* convert to another harness in one step.
cv port <id> --to claude --to-dir ~/work/new-checkout
```

Rehoming rewrites the working directory baked into the target format (Claude's encoded
project-dir name, Grok's percent-encoded path, Cline/Roo's `<environment_details>` cwd
line, …) so the ported session resolves to the new location.

**It also brings the project's memory along.** Unless you pass `--no-context`, `port`
copies the source cwd's context files into the new directory so the ported session lands
with its instructions/memory intact. The carried set (`CONTEXT_FILES` in `main.rs`) is:

```text
CLAUDE.md   CLAUDE.local.md   AGENTS.md   GEMINI.md
MEMORY.md   .cursorrules      .windsurfrules
```

This copy is strictly best-effort: it **never overwrites** an existing file at the target
(it tells you it left it as-is) and never fails the port if a copy doesn't work.

## What survives, and what's lossy

The IR is deliberately a *superset* — fields a given harness lacks are simply `None`/empty,
and harness-specific extras ride along in `Message::extra`. So a clean round-trip is the
common case. But some target formats genuinely cannot represent some IR content, and
clustervision tells you when that happens instead of pretending otherwise.

### Verified emits

`emit_verified()` does the honest thing: after writing the output, it **re-parses that
output with the target's own adapter** and diffs it against the source IR. Any content that
didn't make it back is reported as a human-readable warning (`diff_lossy()`), e.g.
`dropped 3 tool calls` or `2 reasoning blocks preserved as encrypted/summary only`. This is
purely a read-back check — it never changes what `emit()` wrote.

It compares counts of the things most likely to be lost: tool calls, tool results, images,
reasoning (thinking) text, and standalone system turns. An empty warning list means a clean
round-trip (modulo fields the format simply can't hold, which aren't flagged because they're
inherent to the format).

### Known faithful-but-lossy cases

These are honest format limitations of the *target*, not bugs in the converter — the
emitter does the most faithful thing the destination format allows:

| target | what's lossy | why |
|---|---|---|
| **LM Studio** | tool calls & tool results flatten to text like `[tool call: …]` / `[tool result: …]` | LM Studio is a chat app with **no first-class tool structures** on disk, so we don't fabricate any |
| **LM Studio** | thinking is kept but as a `style.type=="thinking"` text block; no `cwd` is written | it's a chat app — `Session::cwd` is always `None`; the chosen cwd only appears in the resume hint |
| **Continue** | thinking flattens into a plain text part | Continue's `ChatMessage` has no thinking/reasoning part, so reasoning is preserved as searchable text |
| **Cline** / **Roo** | the cwd and task title are **embedded into the first user message text** (`<task>…</task>` + `<environment_details># Current Working Directory (/abs/path) Files`) | that's how Cline/Roo natively carry cwd/title — the parser reads them right back out of the transcript, plus a `task_metadata.json` cwd-hint sidecar |
| **Grok** | per-message content collapses to plain text (thinking rides in a separate `reasoning` field, tools in `tool_calls[]`) | Grok's `chat_history` carries text only |
| any target without a standalone system turn (e.g. Claude) | a `Role::System` turn may be dropped | the format has no place for a standalone system message; dropping it avoids polluting user text |
| reasoning that was only ever an encrypted blob | comes back as encrypted/summary only | the raw chain-of-thought text was never stored to begin with |

When any of these reduce the content on the way out, `emit_verified` surfaces it as a
warning — so "lossy" is always *visible*, never silent.

## The IR block kinds

A `Session` is metadata (`id`, `cwd`, `title`, `model`, `git`, timestamps, …) plus a list
of `Message`s. Each `Message` has a `Role` (`System` / `User` / `Assistant` / `Tool` —
tool results are kept as a distinct role so conversions can re-encode them correctly) and a
`content: Vec<Block>`. The block kinds (see `Block` in `ir.rs`):

| block | carries | notes |
|---|---|---|
| `Text` | `text` | plain assistant/user prose |
| `Thinking` | `text`, `signature?`, `encrypted?`, `redacted` | extended reasoning / chain-of-thought; `encrypted` holds an opaque provider blob |
| `ToolUse` | `id`, `name`, `input` (JSON) | a tool/function invocation by the assistant |
| `ToolResult` | `tool_use_id`, `content`, `is_error`, `tool_name?`, `status?`, `details?` | the result fed back for a call |
| `File` | `mime?`, `path?`, `source?` | a first-class file/dir/resource attachment |
| `Image` | `media_type?`, `data_ref?` | image bytes are **not** inlined into the IR — only a path/opaque reference is kept |

Because each harness parser produces these same blocks, and each emitter knows how to write
(or gracefully flatten) each one, conversion is just translation between two dialects of the
same underlying conversation. For the full shape of a parsed session, see
[OpenSession](opensession.md).
