# claurdvoyant is available

This project has the **claurdvoyant** MCP server (`claurdvoyant`) connected. It lets you
read **other agents' sessions** — across harnesses (Claude Code, Codex, OpenCode, …) and
back through time — and coordinate with sibling agents via a shared board.

When you start a task, consider:

- `recall(query)` — "have I or another agent solved this before?" Semantic search over
  the whole cross-harness corpus; returns the most relevant message spans.
- `project_sessions(cwd)` — what happened (or is happening) in THIS project before.
- `search_sessions(query)` — full-text search across all transcripts.
- `read_session(id)` — read a full prior transcript.
- `board_post` / `board_read` / `board_await` — coordinate with other running agents on
  the `fleet` channel (or a per-project channel).

You can also run `/recall <what you're doing>` to drive the recall flow.
