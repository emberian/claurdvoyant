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

/** Window size for paged transcript loads. */
export const PAGE = 200;

/** Fetch the message window [start, end) of a session without loading the whole transcript
 *  (cvd ≥0.9.12's `/messages` endpoint / the desktop's `local_messages` command). Returns
 *  `{ messages, start, end, has_more, total_known, total, message_count, session }`, or `null`
 *  when the windowed path isn't available (older cvd, no cvd, older app) — callers fall back to
 *  a full `hydrateSession`. */
export async function getMessages(stub, start, end) {
  if (!stub || !stub.harness || !stub.id) return null;
  try {
    let raw;
    if (canInvokeNative()) {
      raw = JSON.parse(await invoke("local_messages", { harness: stub.harness, id: stub.id, start, end }));
    } else {
      const url = `${CVD_BASE}/api/session/${enc(stub.harness)}/${enc(stub.id)}/messages?start=${start}&end=${end}`;
      const resp = await fetch(url, { headers: { Accept: "application/json" } });
      if (!resp.ok) return null; // 404 = older cvd (or unknown session): take the full path
      raw = await resp.json();
    }
    return raw && Array.isArray(raw.messages) ? raw : null;
  } catch {
    return null;
  }
}

/** The session's extracted tool events (file edits/reads, commands, errors), in transcript
 *  order, optionally filtered by kind. Empty on any error or an older cvd — the events panel
 *  simply doesn't appear. */
export async function getEvents(session, kind) {
  if (!session || !session.harness || !session.id) return [];
  try {
    let raw;
    if (canInvokeNative()) {
      raw = JSON.parse(await invoke("local_events", { harness: session.harness, id: session.id, kind: kind ?? null }));
    } else {
      const q = kind ? `?kind=${enc(kind)}` : "";
      const resp = await fetch(`${CVD_BASE}/api/session/${enc(session.harness)}/${enc(session.id)}/events${q}`, {
        headers: { Accept: "application/json" },
      });
      if (!resp.ok) return [];
      raw = await resp.json();
    }
    return Array.isArray(raw) ? raw : [];
  } catch {
    return [];
  }
}

/** Which sessions touched a file (by exact or suffix path match). Returns an array of
 *  `{ harness, session_id, title, edits, reads, last_ts }` rows, or `null` when the lookup
 *  isn't available (older cvd / no cvd) so the UI can say so instead of showing "no results". */
export async function getTouched(path, editsOnly = false) {
  if (!path) return [];
  try {
    if (canInvokeNative()) {
      const raw = JSON.parse(await invoke("local_touched", { path, editsOnly: !!editsOnly }));
      return Array.isArray(raw) ? raw : null;
    }
    const resp = await fetch(`${CVD_BASE}/api/touched?path=${enc(path)}${editsOnly ? "&edits_only=true" : ""}`, {
      headers: { Accept: "application/json" },
    });
    if (!resp.ok) return null;
    const raw = await resp.json();
    return Array.isArray(raw) ? raw : null;
  } catch {
    return null;
  }
}

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
