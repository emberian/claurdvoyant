// tauri.js — graceful Tauri desktop integration.
//
// When clustervision runs inside the Tauri shell (the `app/` desktop wrapper),
// `window.__TAURI__` is present. We then:
//   (a) route distill / redact / generate through native commands via
//       `window.__TAURI__.core.invoke(...)` — no API key needed; the desktop
//       env (or a local LM Studio) does the work; and
//   (b) listen for the `cv://open-sessions` event whose payload is a JSON array
//       of Session objects, emitted by the native File → Open menu.
//
// In a plain browser none of this exists, so every helper degrades to a no-op /
// `false`, and callers fall back to the in-JS OpenRouter path.

/** True when running inside the Tauri desktop shell. `__TAURI_INTERNALS__` is the low-level bridge
 *  that is ALWAYS present in the webview (even when `withGlobalTauri` is off and the high-level
 *  `window.__TAURI__` object isn't injected), so it's the reliable presence signal. */
export function isTauri() {
  return typeof window !== "undefined"
    && !!(window.__TAURI_INTERNALS__ || window.__TAURI__);
}

/** The native `invoke` fn, or null in a browser. Prefer the high-level API, but fall back to the
 *  always-present internals bridge so native commands work regardless of `withGlobalTauri`. */
function invokeFn() {
  return window.__TAURI__?.core?.invoke
    ?? window.__TAURI__?.invoke // older Tauri exposed it at the root
    ?? window.__TAURI_INTERNALS__?.invoke // low-level bridge, always injected
    ?? null;
}

/**
 * Call a native command. Rejects (rather than silently no-op'ing) when not in
 * Tauri so callers can detect the situation and fall back deliberately.
 * @param {"distill"|"redact"|"generate"|string} command
 * @param {object} [args]
 * @returns {Promise<any>}
 */
export async function invoke(command, args = {}) {
  const fn = invokeFn();
  if (!fn) throw new Error(`Not running in Tauri — native command "${command}" is unavailable.`);
  return fn(command, args);
}

/** True when a native command path is usable (Tauri present + invoke exposed). */
export function canInvokeNative() {
  return isTauri() && !!invokeFn();
}

/**
 * Subscribe to a Tauri event. Returns an unlisten function (a no-op in a
 * browser). Safe to call unconditionally.
 * @param {string} event
 * @param {(payload:any)=>void} handler
 * @returns {Promise<() => void>}
 */
export async function listen(event, handler) {
  const ev = window.__TAURI__?.event;
  if (!ev?.listen) return () => {};
  try {
    const unlisten = await ev.listen(event, (e) => handler(e?.payload));
    return typeof unlisten === "function" ? unlisten : () => {};
  } catch (err) {
    console.warn("[clustervision] Tauri event listen failed:", err);
    return () => {};
  }
}

/**
 * Generate a continuation through the native command. Mirrors the OpenRouter
 * `generate({ messages, model, onToken, signal })` shape closely enough that
 * the loom can swap them.
 *
 * The native side may stream by emitting `cv://generate-token` events, or it
 * may simply resolve with the full text. We support both: if a `streamEvent`
 * is provided we wire it up for the duration of the call.
 *
 * @param {object} opts
 * @param {object[]} opts.messages   IR messages.
 * @param {string}  [opts.model]
 * @param {(chunk:string, full:string)=>void} [opts.onToken]
 * @returns {Promise<string>}
 */
export async function nativeGenerate({ messages, model, onToken } = {}) {
  let unlisten = () => {};
  let full = "";
  if (typeof onToken === "function") {
    unlisten = await listen("cv://generate-token", (payload) => {
      const chunk = typeof payload === "string" ? payload : (payload?.chunk ?? "");
      if (!chunk) return;
      full += chunk;
      onToken(chunk, full);
    });
  }
  try {
    const result = await invoke("generate", { messages, model });
    const text = typeof result === "string" ? result : (result?.text ?? result?.content ?? "");
    // If nothing streamed, deliver the whole thing once.
    if (onToken && !full && text) onToken(text, text);
    return text || full;
  } finally {
    unlisten();
  }
}
