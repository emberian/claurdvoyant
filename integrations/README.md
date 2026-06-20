# clustervision integrations — the cross-harness memory + coordination layer

> Wire your agent harnesses *into* clustervision so they feed it and use it automatically.

clustervision already parses, searches, ports, and streams sessions from every harness.
These integrations close the loop: instead of running `cv`/`cvd` by hand, each harness's
own extension points (**hooks**, **plugins**, **MCP**) drive clustervision **live** —
turning it into a hub every agent plugs into.

Three things happen, automatically, in whatever harness you're in:

1. **Feed** — when a session ends/idles, the harness runs `cvd sync` to archive every
   harness's sessions into `~/.clustervision`, so nothing rots in scattered folders.
2. **Surface** — when a session starts, the harness pulls prior context for the current
   project (`cv ls --cwd`, `cv board read fleet`) and injects it; deeper, semantic recall
   is one tool/command away.
3. **Coordinate** — agents read each other's minds via the `cv-mcp` MCP server
   (`recall`, `project_sessions`, `search_sessions`, `read_session`, `await_omen`) and
   hand off work on a shared board (`board_post` / `board_read` / `board_await` /
   `board_claim`). Lifecycle hooks post `"<harness> finished in <cwd>"` to the `fleet`
   channel, so the whole machine — or cloud fleet — becomes one live feed.

## Build first

```sh
cargo build --release   # repo root → target/release/{cv, cv-mcp, cvd}
```

Put `cv` / `cvd` on your `PATH` (or set `CV_BIN` / `CVD_BIN`), and use the absolute path
to `cv-mcp` when registering the MCP server. Every config below uses the placeholder
`/path/to/...`.

## Per-harness recipes

| Harness | Extension surface used | Feeds `cvd`? | MCP? | Where confirmed |
|---|---|:--:|:--:|---|
| **[Claude Code](./claude-code/)** | `settings.json` hooks (SessionStart / SessionEnd / Stop) + `.claude/commands/*.md` + `claude mcp add` | ✅ | ✅ | on-disk `~/.claude/{settings.json,commands}`; hook schema cross-checked vs. Codex's clone |
| **[Codex CLI](./codex/)** | `~/.codex/hooks.json` lifecycle hooks + `[mcp_servers.*]` + `notify` | ✅ | ✅ | `codex-rs/core/tests/suite/hooks.rs`, `codex-rs/hooks/schema/generated/*`, live `~/.codex/config.toml` |
| **[OpenCode](./opencode/)** | TS plugin (`event` → `session.idle`) + `mcp` config | ✅ | ✅ | `packages/plugin/src/index.ts`, `packages/sdk/.../types.gen.ts`, `packages/opencode/src/config/config.ts` |
| **[Gemini CLI](./gemini-cli/)** | Extension: `gemini-extension.json` (mcpServers) + `hooks/hooks.json` (SessionStart/End) + `commands/*.toml` + `GEMINI.md` | ✅ | ✅ | `docs/extensions/reference.md`, `docs/hooks/{index,reference}.md` |
| **[Hermes](./hermes/)** | Python plugin (`register(ctx)` → `on_session_end`) + `mcp_servers:` config | ✅ | ✅ | `hermes_cli/plugins.py`, `plugins/disk-cleanup/`, `cli-config.yaml.example` |
| **[OpenClaw](./openclaw/)** | Internal hook (`HOOK.md` + `handler.ts`, `session:compact:after`/`gateway:shutdown`) + `mcp.servers.*` | ✅ | ✅ | `docs/automation/hooks.md`, `docs/gateway/configuration-reference.md` |
| **[Kimi CLI](./kimi/)** | `kimi mcp add` / `~/.kimi/mcp.json` | passive¹ | ✅ | `docs/en/customization/mcp.md`, `docs/en/configuration/data-locations.md` |

¹ Kimi exposes MCP but has **no documented lifecycle-hooks system**, so it can't actively
call `cvd sync`. It feeds clustervision passively: run `cvd watch` and it archives Kimi's
on-disk sessions like any other harness.

## Coordination patterns (not harness-specific)

Some recipes aren't a *harness* adapter — they're a **way to use** clustervision's coordination
tools across whatever harnesses your agents run in:

| Pattern | What it does | Built from |
|---|---|---|
| **[mentor-observe](./mentor-observe/)** | A senior/orchestrator agent **polls a junior agent's live activity on its own cadence** (non-blocking), drains the junior's newly-appended messages since a cursor, and steers via the board. The mentor-loop sibling of the blocking `await_omen`. | `cvd watch` + the `observe_stream` MCP tool |

## What turned out *not* cleanly extensible

- **Kimi CLI** — MCP only; no hooks/plugin lifecycle to actively run `cvd`. Covered via
  MCP + passive `cvd watch`.
- The **Claude / ChatGPT desktop apps** keep transcripts server-side or encrypted at
  rest (see the repo `docs/FORMATS.md`); there's nothing to hook. Not addressed here.

## Honest note on commands used

These recipes use only commands that **exist in this build**:

- CLI: `cv ls --cwd`, `cv search` (`--semantic`), `cv board post|read|...`, `cv scry`,
  `cv index --semantic`, `cv stats`, `cv resume`, `cv show`, `cv timeline`.
- Daemon: `cvd sync`, `cvd watch`, `cvd ls`, `cvd path`.
- MCP (`cv-mcp`): `recall`, `search_sessions`, `project_sessions`, `read_session`,
  `list_sessions`, `await_omen`, `observe_stream` (the non-blocking tail of `await_omen`),
  `board_*`.

There is **no `cv distill` or `cv recall` CLI subcommand** in this build — distillation
exists only as the `cv-llm::distill` library, and `recall` exists only as an MCP tool.
SessionStart hooks therefore use cheap synchronous CLI commands for prior context, and
the semantic "have I solved this before?" lookup is driven through the MCP `recall` tool
(and the `/recall` slash command shipped for Claude Code and Gemini). If a `cv distill`
subcommand lands later, you can append a `MEMORY.md` write to any SessionEnd hook.
