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

const enc = encodeURIComponent;

/** The sub-agents a session spawned (Claude Code Task sub-agents), as metadata-only stubs. Empty on
 *  any error or for harnesses without sub-agents. */
export async function getSubagents(session) {
  if (!session || !session.harness || !session.id) return [];
  try {
    let raw;
    if (canInvokeNative()) {
      raw = JSON.parse(await invoke("local_subagents", { harness: session.harness, id: session.id }));
    } else {
      const resp = await fetch(`${CVD_BASE}/api/session/${enc(session.harness)}/${enc(session.id)}/subagents`, {
        headers: { Accept: "application/json" },
      });
      if (!resp.ok) return [];
      raw = await resp.json();
    }
    if (!Array.isArray(raw)) return [];
    return raw.map((s) => {
      const n = normalizeSession(s);
      n._stub = true;
      n._parentId = session.id;
      n._parentHarness = session.harness;
      return n;
    });
  } catch {
    return [];
  }
}

/** Full transcript of one sub-agent, loaded relative to its parent. */
export async function getSubagent(parentHarness, parentId, agentId) {
  let raw;
  if (canInvokeNative()) {
    raw = JSON.parse(await invoke("local_subagent", { harness: parentHarness, parent: parentId, agent: agentId }));
  } else {
    const resp = await fetch(`${CVD_BASE}/api/session/${enc(parentHarness)}/${enc(parentId)}/subagent/${enc(agentId)}`, {
      headers: { Accept: "application/json" },
    });
    if (!resp.ok) throw new Error(`cvd ${resp.status}`);
    raw = await resp.json();
  }
  const full = normalizeSession(raw);
  full._stub = false;
  return full;
}
