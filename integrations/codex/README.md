# clustervision × Codex CLI

Codex is one of the more deeply extensible harnesses — it exposes **MCP servers**,
a **Claude-Code-compatible lifecycle hooks system**, *and* a `notify` callback.

## 1. MCP server — [`config.toml`](./config.toml)

Codex registers stdio MCP servers under `[mcp_servers.<name>]` with
`command` / `args` / optional `env` / `enabled`. Confirmed against the live
`~/.codex/config.toml` on this machine (existing entries like `[mcp_servers.playwright]`,
`[mcp_servers.anna-mcp]`). Merge the `[mcp_servers.clustervision]` block into your
`~/.codex/config.toml`, pointing `command` at your built `cv-mcp`. A running Codex agent
then gets `recall`, `search_sessions`, `project_sessions`, `read_session`, and the
`board_*` coordination tools.

## 2. Lifecycle hooks — [`hooks.json`](./hooks.json)

Codex ships a hooks engine that is a clone of Claude Code's: events
(`SessionStart`, `Stop`, `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, …) live in a
`hooks.json` at `~/.codex/hooks.json` (also configurable as `[[hooks.<Event>]]` in
`config.toml`). Each event holds `{ "hooks": [ { "type": "command", "command": "<shell>" } ] }`,
and the command receives a JSON payload **on stdin** including `cwd`, `session_id`,
`hook_event_name` (Stop adds `last_assistant_message`, `turn_id`, `stop_hook_active`).

Confirmed in the local `codex` checkout:
- `codex-rs/core/tests/suite/hooks.rs` — the `hooks.json` shape, e.g.
  `"SessionStart": [{ "hooks": [{ "type": "command", "command": ... }] }]`, and
  `[[hooks.PreToolUse]]` TOML form.
- `codex-rs/hooks/schema/generated/session-start.command.input.schema.json` and
  `stop.command.input.schema.json` — the exact stdin field names.
- `codex-rs/core/src/hook_runtime.rs` — the runtime that dispatches `SessionStart`,
  `Stop`, etc.

Our [`hooks.json`](./hooks.json):
- **SessionStart** → `cv ls --cwd <cwd>` + `cv board read fleet` to surface prior
  project sessions and recent fleet activity.
- **Stop** → `cvd sync` (archive) + `cv board post fleet "codex finished in <cwd>"`.

Replace `/path/to/...` with absolute binary paths. The hook reads `cwd` from the stdin
payload via `jq`, falling back to `$PWD`.

## 3. `notify` callback — [`notify-cvd.sh`](./notify-cvd.sh)

Codex also supports a single `notify` program invoked after each completed turn with a
JSON payload appended as the last argv (documented in
`codex-rs/core/src/config/mod.rs` near the `notify` field; the live config on this
machine already uses `notify` for turn-end desktop notifications). If you'd rather not
use `hooks.json`, point `notify` at [`notify-cvd.sh`](./notify-cvd.sh) to run `cvd sync`
on every turn. Prefer the hooks approach if you can, since `notify` only allows one
program.

## Honesty

No `cv distill` / `cv recall` CLI subcommands exist in this build — the hooks use real
CLI commands (`cv ls`, `cv board *`, `cvd sync`) and the MCP server provides the
semantic `recall` tool.
