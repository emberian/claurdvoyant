// Shared lazy hydration: the session pool is metadata-only stubs (from cvd `/api/sessions` or the
// native `local_sessions` command) until a transcript is actually needed. Any view (transcript,
// compare, loom) calls `hydrateSession(stub)` to pull the full message tree on demand — native
// command in the desktop, HTTP to a local cvd in the browser. Already-full sessions pass through.
import { invoke, canInvokeNative } from "../tauri.js";
import { normalizeSession } from "./util.js";

const CVD_BASE = "http://localhost:7777";

/** Whether a session still needs its messages loaded. */
export function isStub(s) {
  return !!(s && s._stub && !(s.messages && s.messages.length));
}

/** Resolve a stub to a full session (with messages). Full sessions are returned unchanged. */
export async function hydrateSession(stub) {
  if (!isStub(stub)) return stub;
  let raw;
  if (canInvokeNative()) {
    raw = JSON.parse(await invoke("local_session", { harness: stub.harness, id: stub.id }));
  } else {
    const url = `${CVD_BASE}/api/session/${encodeURIComponent(stub.harness)}/${encodeURIComponent(stub.id)}`;
    const resp = await fetch(url, { headers: { Accept: "application/json" } });
    if (!resp.ok) throw new Error(`cvd ${resp.status}`);
    raw = await resp.json();
  }
  const full = normalizeSession(raw);
  full._stub = false;
  return full;
}
