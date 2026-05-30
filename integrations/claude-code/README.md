# claurdvoyant × Claude Code

Wire Claude Code into claurdvoyant so the harness **feeds** the archive on the way
out and **surfaces prior context** on the way in — and lets a running agent read
other agents' minds over MCP.

## Mechanism (where this is confirmed)

Claude Code's extension points used here:

- **Hooks** in `settings.json` under a top-level `hooks` object, keyed by event name
  (`SessionStart`, `SessionEnd`, `Stop`, …). Each entry is
  `{ "matcher": <regex?>, "hooks": [ { "type": "command", "command": "<shell>" } ] }`.
  The hook command receives a JSON object **on stdin** with keys including
  `session_id`, `transcript_path`, `cwd`, and `hook_event_name`. `SessionStart` hook
  stdout is injected into the session as context.
  - This is the same hook schema Codex implements (a Claude-Code-compatible clone);
    cross-checked against `codex-rs/hooks/schema/generated/session-start.command.input.schema.json`
    and `stop.command.input.schema.json` in the local `codex` checkout, whose input
    fields are `cwd, hook_event_name, model, permission_mode, session_id, source,
    transcript_path` (and Stop adds `last_assistant_message, stop_hook_active, turn_id`).
- **Project commands** as Markdown files in `.claude/commands/<name>.md` with optional
  YAML frontmatter (`description`, `argument-hint`). Confirmed against the on-disk
  `~/.claude/commands/*.md` (e.g. `crate.md`) on this machine.
- **MCP servers** registered with `claude mcp add <name> -- <binary>`. claurdvoyant
  ships a stdio MCP server (`cv-mcp`).

## Install

1. **Build the binaries** (from the repo root):

   ```sh
   cargo build --release   # → target/release/{cv, cv-mcp, cvd}
   ```

2. **Hooks** — merge the `hooks` block from [`settings.json`](./settings.json) into
   your `~/.claude/settings.json` (user-wide) or `<project>/.claude/settings.json`
   (per project). Replace every `/path/to/cv` / `/path/to/cvd` with the absolute path
   to your built binary. `$CLAUDE_PROJECT_DIR` is exported by Claude Code for hooks.

   What the hooks do:
   - **SessionStart** → runs `cv ls --cwd <project>` and `cv board read fleet`, printing
     prior sessions for this project + recent fleet activity into the new session's
     context. (Deeper recall is available on demand via the MCP `recall` tool / the
     `/recall` command below.)
   - **SessionEnd** → runs `cvd sync` to archive every session into `~/.claurdvoyant`.
   - **Stop** → posts `"claude finished in <cwd>"` to the `fleet` board channel so
     sibling agents (and `cvd watch`) see it live.

3. **MCP server** — let a running Claude read *other* agents' sessions:

   ```sh
   claude mcp add claurdvoyant -- /path/to/target/release/cv-mcp
   ```

   Tools exposed: `recall`, `search_sessions`, `project_sessions`, `read_session`,
   `list_sessions`, `await_omen`, and the `board_*` coordination family
   (see the repo `README.md`).

4. **Slash command** — copy [`.claude/commands/recall.md`](./.claude/commands/recall.md)
   into your project's `.claude/commands/` (or `~/.claude/commands/` for all projects).
   Then `/recall <what you're doing>` pulls relevant prior work via the MCP server.

## Notes / honesty

- There is **no `cv distill` or `cv recall` CLI subcommand** in this build. Distillation
  exists only as a library (`cv-llm::distill`) and `recall` exists only as an **MCP
  tool**. The SessionStart hook therefore uses the real CLI (`cv ls` / `cv board read`)
  for cheap synchronous context, and the `/recall` slash command drives the MCP
  `recall` tool for the semantic "have I solved this before?" lookup. If you later wire
  a `cv distill` subcommand, you can append a MEMORY.md write to the SessionEnd hook.
- The `Stop` hook uses `jq` to read `cwd` from the hook's stdin JSON; if `jq` is absent
  it falls back to `$PWD`. All hook commands are `|| true`-guarded so a missing binary
  never blocks Claude Code.
