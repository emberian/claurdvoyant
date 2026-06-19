# clustervision × Hermes (Nous)

Hermes is extensible via a **Python plugin system with lifecycle hooks** and via **MCP**.
Both are wired here.

## Mechanism (confirmed in `/Users/ember/pug/hermes-agent`)

- **Plugins** are directories under `~/.hermes/plugins/<name>/` (user) or
  `./.hermes/plugins/<name>/` (project), each with a `plugin.yaml` manifest and an
  `__init__.py` exposing `register(ctx)`. Hooks are wired with
  `ctx.register_hook("<event>", fn)`. Available lifecycle hooks include
  `on_session_end` and `post_tool_call`. Confirmed in `hermes_cli/plugins.py`
  (load paths + manifest model) and the bundled `plugins/disk-cleanup/` plugin
  (`register(ctx)`, `ctx.register_hook("on_session_end", ...)`, the
  `_on_session_end(session_id, completed, interrupted, **_)` signature).
- **MCP servers** are configured under `mcp_servers:` in the Hermes config
  (`cli-config.yaml`); stdio servers use `command` + `args` (+ optional `env`).
  Confirmed in `cli-config.yaml.example` ("MCP (Model Context Protocol) Servers").

## Assets

### 1. Plugin — [`cvd-archive/`](./cvd-archive/)

`plugin.yaml` + `__init__.py`. On `on_session_end` it runs `cvd sync` (archive every
harness) and posts `"hermes <status> (<session_id>)"` to the `fleet` board. Install:

```sh
cargo build --release      # repo root → cv, cvd (put them on PATH, or set CV_BIN/CVD_BIN)
cp -r cvd-archive ~/.hermes/plugins/cvd-archive
# ensure plugins are enabled in your Hermes config, then:
hermes plugins list        # confirm cvd-archive is discovered/enabled
```

### 2. MCP server — [`cli-config.yaml`](./cli-config.yaml)

Merge the `mcp_servers.clustervision` block into your Hermes config so a running Hermes
agent can call `recall`, `search_sessions`, `project_sessions`, `read_session`, and the
`board_*` tools.

## Notes

- The plugin is fully best-effort (every subprocess call is guarded) so it can never
  crash the agent loop — matching the bundled plugins' conventions.
- No `cv distill` / `cv recall` CLI subcommands exist; the plugin archives + posts, and
  the MCP server provides semantic `recall`.
