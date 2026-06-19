# clustervision × Kimi CLI

Kimi Code CLI is extensible via **MCP** (`kimi mcp ...`). It does not expose a documented
lifecycle-hooks system, so the integration is MCP-only: a running Kimi agent can read
other agents' sessions and use the coordination board. For archiving, run `cvd watch`
out-of-band (see the top-level README) — that captures Kimi sessions too.

## Mechanism (confirmed in `/Users/ember/pug/kimi-cli`)

- MCP servers are managed with `kimi mcp add/list/remove/...` and persisted to
  `~/.kimi/mcp.json` under a `mcpServers` map. Confirmed in
  `docs/en/customization/mcp.md` (the `kimi mcp add --transport stdio ...` form) and
  `docs/en/configuration/data-locations.md` (`mcp.json` -> `mcpServers`).
- No lifecycle/SessionStart/SessionEnd hook mechanism is documented for Kimi (grepping
  `docs/` finds MCP and slash commands, but no hooks). So Kimi feeds clustervision
  passively: `cvd sync` / `cvd watch` discovers and archives Kimi's on-disk sessions.

## Install

Recommended — let Kimi write `~/.kimi/mcp.json` for you:

```sh
cargo build --release   # repo root → target/release/cv-mcp
kimi mcp add --transport stdio clustervision -- /path/to/target/release/cv-mcp
kimi mcp list           # verify it connected
```

Or merge [`mcp.json`](./mcp.json) into `~/.kimi/mcp.json` by hand.

Inside Kimi, `/mcp` lists connected servers and loaded tools. clustervision exposes
`recall`, `search_sessions`, `project_sessions`, `read_session`, `list_sessions`,
`await_omen`, and the `board_*` family.
