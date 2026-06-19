# clustervision × OpenCode

Two integration surfaces, both confirmed against the OpenCode source in
`/Users/ember/pug/opencode`:

## 1. Plugin (active feed) — [`clustervision.ts`](./clustervision.ts)

OpenCode has a first-class TypeScript plugin API:

- A plugin is `(input, options?) => Promise<Hooks>`. The `PluginInput` gives you `$`
  (a Bun shell), `directory`, `worktree`, `project`, and `client`.
  Confirmed in `packages/plugin/src/index.ts` (types `Plugin`, `PluginInput`, `Hooks`).
- `Hooks.event` — `({ event }) => Promise<void>` — is invoked for **every** server
  event. We listen for `event.type === "session.idle"`, which fires when a session
  stops working. Confirmed in `packages/sdk/js/src/gen/types.gen.ts` (`EventSessionIdle`,
  `type: "session.idle"`, `properties.sessionID`).

On `session.idle` the plugin runs `cvd sync` (archives every harness's sessions) and
posts `"opencode idle in <directory>"` to the clustervision `fleet` board channel via
`cv board post`. All shell calls use `.nothrow()` so they never disrupt OpenCode.

**Install.** OpenCode auto-discovers local plugin files under a `.opencode/plugin(s)/`
directory — no `opencode.json` entry needed. Confirmed in
`packages/opencode/src/config/config.ts` (`// Auto-discovered plugins under
.opencode/plugin(s)`). Place the file at:

- `~/.config/opencode/plugin/clustervision.ts` — global, or
- `<project>/.opencode/plugin/clustervision.ts` — per project.

Set `CV_BIN` / `CVD_BIN` env vars to absolute binary paths, or rely on `cv`/`cvd`
being on `PATH`. The plugin imports types from `@opencode-ai/plugin` (the published
plugin package; types only, erased at runtime).

## 2. MCP server (read other agents' minds) — [`opencode.json`](./opencode.json)

OpenCode supports stdio MCP servers via the top-level `mcp` config map: each entry is
`{ "type": "local", "command": [<argv>], "enabled": true }`. Confirmed in
`packages/sdk/js/src/gen/types.gen.ts` (`McpLocalConfig`) and the `mcp` schema in
`config.ts`. Merge the `mcp` block from [`opencode.json`](./opencode.json) into your
config to expose `recall`, `search_sessions`, `project_sessions`, `read_session`, and
the `board_*` tools to a running OpenCode agent.

## Notes

- `session.idle` fires whenever a session goes idle (not strictly "end"); `cvd sync` is
  idempotent so re-archiving on each idle is cheap and keeps the archive fresh. OpenCode
  has no distinct "session end" server event, so idle is the right hook.
- There is no `cv distill` CLI subcommand in this build, so the plugin archives + posts
  to the board rather than summarizing into MEMORY.md.
