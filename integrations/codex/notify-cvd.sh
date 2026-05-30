#!/usr/bin/env bash
# claurdvoyant notify hook for Codex.
#
# Codex's `notify` config spawns this program after each completed turn, appending a
# JSON payload (e.g. {"type":"agent-turn-complete", ...}) as the final argument.
# We ignore the payload and just refresh the claurdvoyant archive + post to the board.
#
# Wire it up in ~/.codex/config.toml:
#   notify = ["/path/to/integrations/codex/notify-cvd.sh"]
#
# Edit CV / CVD below to absolute paths if the binaries aren't on PATH.
set -euo pipefail

CV="${CV_BIN:-cv}"
CVD="${CVD_BIN:-cvd}"

# Refresh the central archive (idempotent).
"$CVD" sync >/dev/null 2>&1 || true

# Best-effort fleet board ping.
"$CV" board post fleet "codex turn complete in $PWD" --from codex --kind event >/dev/null 2>&1 || true

exit 0
