# clustervision × Gemini CLI

Gemini CLI is cleanly extensible via **extensions** that bundle an MCP server, a context
file, custom commands, and lifecycle hooks. This directory is a ready-to-install
extension: [`cv-clustervision/`](./cv-clustervision/).

## Mechanism (where this is confirmed, in `/Users/ember/pug/gemini-cli`)

- **`gemini-extension.json`** manifest with `name`, `version`, `mcpServers`,
  `contextFileName`. Confirmed in `docs/extensions/reference.md` (`### gemini-extension.json`).
- **MCP servers** declared in the manifest's `mcpServers` map (`command` + `args`),
  loaded on startup just like `settings.json` MCP servers. Confirmed in the same doc.
- **Custom commands** as TOML files under `commands/` (`description` + `prompt`,
  `{{args}}` substitution). `commands/recall.toml` → `/recall`. Confirmed in
  `docs/cli/custom-commands.md` and `docs/extensions/reference.md`.
- **Hooks** bundled as `hooks/hooks.json` inside the extension. Gemini supports
  `SessionStart` / `SessionEnd` (and many more) keyed in a `hooks` object of
  `{ matcher, hooks: [ { name, type: "command", command, timeout } ] }`. Hooks talk pure
  JSON over stdin/stdout; `SessionStart` injects context via
  `{"hookSpecificOutput": {"additionalContext": "..."}}`. Confirmed in
  `docs/hooks/index.md` (events table + config schema) and `docs/hooks/reference.md`
  (`hookSpecificOutput.additionalContext`).
- **Context file** `GEMINI.md` auto-loaded from the extension dir. Confirmed in the
  reference (`contextFileName`).

## What the extension does

- **MCP** (`clustervision`): exposes `recall`, `search_sessions`, `project_sessions`,
  `read_session`, `list_sessions`, `await_omen`, `board_*` to a running Gemini agent.
- **SessionStart hook** (`hooks/session-start.sh`): emits `cv ls --cwd <cwd>` +
  `cv board read fleet` as `additionalContext`, so prior project sessions and fleet
  activity are injected into the new session.
- **SessionEnd hook** (`hooks/session-end.sh`): runs `cvd sync` to archive everything and
  posts `"gemini finished in <cwd>"` to the fleet board.
- **`/recall <task>`** command: drives the MCP `recall` flow.
- **`GEMINI.md`**: tells the model clustervision is available and how to use it.

## Install

```sh
# Build the binaries first (repo root):
cargo build --release

# Install the extension (copies it into ~/.gemini/extensions/):
gemini extensions install /Users/ember/pug/clustervision/integrations/gemini-cli/cv-clustervision
```

Then edit the installed `gemini-extension.json` so `mcpServers.clustervision.command`
points at your absolute `cv-mcp` path (use `${extensionPath}` only for files shipped
inside the extension; the binary lives in your build tree). Restart Gemini CLI for the
extension to take effect.

> The hook scripts reference
> `$GEMINI_PROJECT_DIR/.gemini/extensions/cv-clustervision/hooks/...`. If your install
> path differs, adjust the `command` paths in `hooks/hooks.json` accordingly, or make
> `cv`/`cvd` available on `PATH` and set `CV_BIN`/`CVD_BIN`.

## Honesty

No `cv distill` / `cv recall` CLI subcommands exist; the hooks use real CLI commands and
the MCP server provides semantic `recall`. `jq` is required by the hook scripts.
