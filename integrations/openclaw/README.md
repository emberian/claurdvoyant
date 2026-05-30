# claurdvoyant × OpenClaw

OpenClaw is richly extensible: it has a **directory-based internal hooks system** and
**consumes external MCP servers**. Both surfaces are wired here.

## Mechanism (confirmed in `/Users/ember/pug/openclaw`)

- **Internal hooks**: a hook is a directory with `HOOK.md` (frontmatter declaring
  `metadata.openclaw.events`) and `handler.ts` (default export
  `(event) => Promise<void>`). Events include `command:new`, `command:reset`,
  `command:stop`, `session:compact:before/after`, `agent:bootstrap`,
  `gateway:startup/shutdown`, `message:*`. Managed with `openclaw hooks enable <name>`.
  Confirmed in `docs/automation/hooks.md` (event table, HOOK.md format, handler signature,
  event context fields).
- **External MCP servers** (so OpenClaw can *use* claurdvoyant): declared under
  `mcp.servers.<name>` (stdio: `command` + `args`; remote: `url` + `transport`).
  Confirmed in `docs/gateway/configuration-reference.md` ("## MCP", `mcp.servers`).

## Assets

### 1. Archive hook — [`cvd-archive/`](./cvd-archive/)

`HOOK.md` + `handler.ts`. Subscribes to `session:compact:after`, `command:new`, and
`gateway:shutdown`; on each it runs `cvd sync` (archive) and posts
`"openclaw <label> in <cwd>"` to the `fleet` board. Install:

```sh
cargo build --release       # repo root → cv, cvd
cp -r cvd-archive ~/.openclaw/hooks/cvd-archive
openclaw hooks enable cvd-archive
openclaw hooks check
```

Make `cv`/`cvd` available on `PATH` (or set `CV_BIN`/`CVD_BIN`).

### 2. MCP server — [`config.json5`](./config.json5)

Merge the `mcp.servers.claurdvoyant` block into your OpenClaw gateway config (or run
`openclaw mcp set claurdvoyant ...`) so a running OpenClaw agent can call `recall`,
`search_sessions`, `project_sessions`, `read_session`, and the `board_*` tools.

## Notes

- OpenClaw also has a **typed plugin hook** system (`api.on(...)`, `before_agent_finalize`)
  for runtime lifecycle control; the directory-based internal hooks used here are the
  right surface for "operator-managed side effects" like archiving (per OpenClaw's own
  guidance in `docs/automation/hooks.md`).
- `command:stop` is cancellation, not agent-finalization, so we archive on
  `session:compact:after` / `command:new` / `gateway:shutdown` instead.
- No `cv distill` / `cv recall` CLI subcommands exist; the hook archives + posts, and the
  MCP server provides semantic `recall`.
