---
name: cvd-archive
description: "Archive agent sessions into clustervision and post fleet-board status on lifecycle events"
metadata:
  {
    "openclaw":
      {
        "emoji": "🔮",
        "events": ["session:compact:after", "command:new", "gateway:shutdown"],
        "requires": { "anyBins": ["cv", "cvd"] },
      },
  }
---

# cvd-archive

A clustervision internal hook for OpenClaw. On the lifecycle events it subscribes to, it
runs `cvd sync` to archive every harness's sessions into `~/.clustervision`, and posts a
status note to the clustervision `fleet` coordination board so sibling agents (and
`cvd watch`) see the activity live.

## Events

- `session:compact:after` — after compaction completes (a natural "checkpoint" — archive
  the now-summarized session).
- `command:new` — when `/new` starts a fresh session (archive the one just finished).
- `gateway:shutdown` — flush an archive as the gateway goes down.

## Install

1. Build clustervision (`cargo build --release`) and make sure `cv` and `cvd` are on PATH
   (or set `CV_BIN` / `CVD_BIN`).
2. Copy this `cvd-archive/` directory into one of OpenClaw's hook directories (e.g.
   `~/.openclaw/hooks/cvd-archive/`), then enable it:

   ```sh
   openclaw hooks list
   openclaw hooks enable cvd-archive
   openclaw hooks check
   ```

OpenClaw only loads internal hooks once at least one hook is enabled/configured.
