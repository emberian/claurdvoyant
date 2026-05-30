# Troubleshooting & FAQ

### The app shows the sample data, not my real sessions

The desktop app reads your local sessions via a bundled `cvd serve` and a native bridge. If you see
the bundled sample, the app couldn't reach them — make sure you're running the **desktop app** (not
the plain browser build, which is zip-drop only), and that nothing else is occupying its port.

### "Showing 0 sessions" / nothing found

claurdvoyant only sees harnesses installed in their standard locations (see
[Harnesses](harnesses.md)). Run `cv ls` in a terminal — if that's empty too, no supported harness
data was found under `$HOME`.

### First `cv ls` is slow, then fast

Discovery parallelizes across harnesses and caches metadata keyed by `(mtime, size)`, so the first
run does the real work (a few seconds across thousands of sessions) and later runs are ~instant. A
huge transcript (hundreds of MB) is sampled head+tail during discovery, never fully read. See
[Architecture](architecture.md).

### `cv convert` warns about lost content

Conversion re-parses its own output and reports anything that didn't survive. Some targets are
*faithfully* lossy — e.g. LM Studio has no tool structures on disk, so tool calls become readable
text. That's a format limitation, not a bug. See [Cross-harness conversion](conversion.md).

### Sub-agents

Claude Code's Task tool spawns sub-agents whose transcripts are normally invisible. claurdvoyant
finds them and nests them under the parent session in the app's transcript view (lazy-loaded,
labeled by task prompt). There can be thousands, so they're never dumped into the main list. See
[The app](app.md).

### The MCP server isn't responding

`cv-mcp` speaks JSON-RPC over **stdio**, and **stdout is the protocol channel** — all diagnostics go
to stderr. Register it with `claude mcp add claurdvoyant -- /abs/path/to/cv-mcp`. See [MCP](mcp.md).

### Still stuck?

Open an issue: <https://github.com/emberian/claurdvoyant/issues>. Weird old transcripts especially
welcome. 🔮
