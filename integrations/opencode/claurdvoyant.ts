/**
 * claurdvoyant plugin for OpenCode.
 *
 * On every `session.idle` event it archives all harness sessions (`cvd sync`) and posts
 * a status note to the claurdvoyant coordination board's `fleet` channel, so the session
 * becomes searchable/portable and sibling agents (and `cvd watch`) see the activity live.
 *
 * OpenCode plugin API (confirmed in packages/plugin/src/index.ts of the opencode source):
 *   - A plugin is `(input: PluginInput, options?) => Promise<Hooks>`.
 *   - `PluginInput` provides `$` (a Bun shell), `directory`, `worktree`, `project`, `client`.
 *   - `Hooks.event` is `(input: { event: Event }) => Promise<void>`, invoked for every server
 *     event. The `session.idle` event (packages/sdk/js/src/gen/types.gen.ts → EventSessionIdle)
 *     carries `{ type: "session.idle", properties: { sessionID } }` and fires when a session
 *     stops working — our "session finished" signal.
 *
 * Install: drop this file at `~/.config/opencode/plugin/claurdvoyant.ts` (global) or
 * `<project>/.opencode/plugin/claurdvoyant.ts` (per project). OpenCode auto-loads files under
 * a `plugin/` directory; no entry in opencode.json is required for local plugin files.
 * Replace the paths below (or set CV_BIN / CVD_BIN env vars) with your built binaries.
 */

import type { Plugin } from "@opencode-ai/plugin"

const CV = process.env.CV_BIN ?? "cv"
const CVD = process.env.CVD_BIN ?? "cvd"

export const ClaurdvoyantPlugin: Plugin = async ({ $, directory }) => {
  return {
    event: async ({ event }) => {
      if (event.type !== "session.idle") return

      // 1) Archive every harness session into ~/.claurdvoyant (idempotent, fast).
      //    `.nothrow()` so a missing binary or transient error never disrupts OpenCode.
      await $`${CVD} sync`.quiet().nothrow()

      // 2) Announce on the fleet board so other agents / `cvd watch` see it live.
      const body = `opencode idle in ${directory}`
      await $`${CV} board post fleet ${body} --from opencode --kind status`
        .quiet()
        .nothrow()
    },
  }
}

export default ClaurdvoyantPlugin
