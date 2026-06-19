---
description: Recall prior cross-harness work relevant to the current task (via clustervision)
argument-hint: <what you're trying to do>
---

Use the clustervision MCP server to find prior work — across every harness and every
past session — relevant to: **$ARGUMENTS**

Do this:

1. Call the `recall` tool with `query: "$ARGUMENTS"` (k=5). It semantically searches
   the whole cross-harness corpus and returns the most relevant message *spans*, not
   just metadata. Summarize what was already tried / decided / learned.
2. If `recall` returns nothing useful, fall back to `search_sessions` with the same
   query.
3. Also call `project_sessions` with `cwd` set to the current working directory to
   surface what happened (or is happening) in *this* project before.

Then give me a tight briefing: relevant prior context, dead ends already hit, and
decisions already made — so we don't redo work. Cite session ids you pulled from.
