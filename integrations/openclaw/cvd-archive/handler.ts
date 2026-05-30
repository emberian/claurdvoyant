/**
 * claurdvoyant archive hook for OpenClaw.
 *
 * OpenClaw internal hooks are a directory with HOOK.md (metadata, declaring `events`) and
 * handler.ts (default export `(event) => Promise<void>`). The handler receives an event
 * with `type`, `action`, `sessionKey`, `timestamp`, `messages`, and event-specific
 * `context`. Lifecycle events (`session:*`, `gateway:*`) have no reply channel, so we
 * don't push to `event.messages`. Confirmed in docs/automation/hooks.md.
 *
 * On the subscribed lifecycle events we archive every harness's sessions (`cvd sync`) and
 * post a status note to the claurdvoyant `fleet` board (`cv board post`).
 */

import { execFile } from "node:child_process"
import { promisify } from "node:util"

const run = promisify(execFile)

const CV = process.env.CV_BIN ?? "cv"
const CVD = process.env.CVD_BIN ?? "cvd"

// Map the events we subscribe to (see HOOK.md `events`) to a short label for the board.
const ARCHIVE_TRIGGERS: Record<string, string> = {
  "session:compact:after": "compacted",
  command: "new-session", // command:new arrives as type "command", action "new"
  "gateway:shutdown": "shutdown",
}

const handler = async (event: any): Promise<void> => {
  const key = event?.type === "command" ? "command" : event?.type
  const label = ARCHIVE_TRIGGERS[key]
  if (!label) return
  if (event?.type === "command" && event?.action !== "new") return

  const cwd = event?.context?.workspaceDir ?? process.cwd()

  try {
    // Refresh the central archive (idempotent, fast).
    await run(CVD, ["sync"])
  } catch (err) {
    console.error(`[cvd-archive] cvd sync failed: ${String(err)}`)
  }

  try {
    await run(CV, [
      "board",
      "post",
      "fleet",
      `openclaw ${label} in ${cwd}`,
      "--from",
      "openclaw",
      "--kind",
      "status",
    ])
  } catch (err) {
    console.error(`[cvd-archive] board post failed: ${String(err)}`)
  }
}

export default handler
