#!/usr/bin/env bash
# claurdvoyant SessionStart hook for Gemini CLI.
#
# Gemini's GOLDEN RULE: stdout must contain ONLY a single JSON object. All logging goes
# to stderr. We emit {"hookSpecificOutput": {"additionalContext": "<prior context>"}}
# so Gemini injects claurdvoyant's view of prior project sessions + fleet activity.
#
# The stdin payload includes `cwd`; we read it with jq and fall back to $PWD.
# Edit CV below to an absolute path if `cv` isn't on PATH.
set -euo pipefail

CV="${CV_BIN:-cv}"

payload="$(cat || true)"
cwd="$(printf '%s' "$payload" | jq -r '.cwd // empty' 2>/dev/null || true)"
cwd="${cwd:-$PWD}"

# Gather prior context (stderr-safe; errors swallowed).
sessions="$("$CV" ls --cwd "$cwd" --limit 8 2>/dev/null || true)"
board="$("$CV" board read fleet --limit 5 2>/dev/null || true)"

context="## claurdvoyant — prior context for ${cwd}

Recent sessions in this project (cv ls):
${sessions:-<none>}

Recent fleet board activity (cv board read fleet):
${board:-<none>}

Use the claurdvoyant MCP tools (recall, project_sessions, search_sessions) to pull
deeper prior context mid-task."

# Emit the required JSON. jq builds it so the string is correctly escaped.
jq -n --arg ctx "$context" \
  '{hookSpecificOutput: {additionalContext: $ctx}}'
