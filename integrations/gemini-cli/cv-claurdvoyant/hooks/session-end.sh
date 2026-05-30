#!/usr/bin/env bash
# claurdvoyant SessionEnd hook for Gemini CLI.
#
# Advisory event: archive every harness's sessions and ping the fleet board.
# SessionEnd output is advisory, but we still keep stdout JSON-clean per Gemini's rule.
# Edit CV / CVD below to absolute paths if the binaries aren't on PATH.
set -euo pipefail

CV="${CV_BIN:-cv}"
CVD="${CVD_BIN:-cvd}"

payload="$(cat || true)"
cwd="$(printf '%s' "$payload" | jq -r '.cwd // empty' 2>/dev/null || true)"
cwd="${cwd:-$PWD}"

"$CVD" sync >/dev/null 2>&1 || true
"$CV" board post fleet "gemini finished in ${cwd}" --from gemini --kind status >/dev/null 2>&1 || true

# Emit an empty JSON object (no decision needed for an advisory event).
printf '{}\n'
