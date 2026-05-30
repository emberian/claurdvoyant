// Small shared helpers used by the web components. No dependencies.

/** Escape text for safe insertion into HTML. */
export function esc(s) {
  return String(s ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

/** Pretty-print a JSON value, falling back to String() on cycles. */
export function pretty(value) {
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

/** Format an ISO timestamp into a compact local string, or "" if absent/invalid. */
export function fmtTime(iso) {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  return d.toLocaleString(undefined, {
    year: "numeric", month: "short", day: "numeric",
    hour: "2-digit", minute: "2-digit",
  });
}

/** Short relative-ish label for the most recent activity timestamp. */
export function sortTime(session) {
  const t = session.updated_at || session.created_at || null;
  const d = t ? new Date(t).getTime() : 0;
  return Number.isNaN(d) ? 0 : d;
}

/** All searchable text for a session (title + cwd + every block's text). */
export function searchableText(session) {
  const parts = [];
  if (session.title) parts.push(session.title);
  if (session.cwd) parts.push(session.cwd);
  if (session.model) parts.push(session.model);
  for (const m of session.messages || []) {
    for (const b of m.content || []) {
      switch (b.kind) {
        case "text":
        case "thinking":
          if (b.text) parts.push(b.text);
          break;
        case "tool_use":
          if (b.name) parts.push(b.name);
          // Only index string args, not a full JSON.stringify of the input — serializing every
          // tool input across every message was a per-keystroke hot spot for big sessions.
          if (typeof b.input === "string") parts.push(b.input);
          break;
        case "tool_result":
          if (b.content) parts.push(b.content);
          if (b.tool_name) parts.push(b.tool_name);
          break;
        case "file":
          if (b.path) parts.push(b.path);
          break;
      }
    }
  }
  return parts.join("\n").toLowerCase();
}

/** First non-empty user text — used as a preview/fallback title. */
export function firstUserText(session) {
  for (const m of session.messages || []) {
    if (m.role !== "user") continue;
    for (const b of m.content || []) {
      if (b.kind === "text" && b.text && b.text.trim()) return b.text.trim();
    }
  }
  return null;
}

/** Human label for a session listing. */
export function sessionLabel(session) {
  const raw = session.title || firstUserText(session) || "(untitled)";
  return truncate(raw.replace(/\s+/g, " ").trim(), 80);
}

export function truncate(s, max) {
  s = String(s ?? "");
  if (s.length <= max) return s;
  return s.slice(0, Math.max(0, max - 1)) + "…";
}

/** Message count that works for hydrated sessions and metadata-only stubs (cvd `/api/sessions`). */
export function msgCount(session) {
  return session.messages?.length || session.message_count || 0;
}

/** Harness pill as a plain HTML string — same markup/styling as <cv-harness-badge> but with NO
 *  custom-element upgrade, so lists of thousands of rows render fast. */
export function harnessBadge(harness) {
  const h = (harness || "").toLowerCase();
  const label = HARNESS_LABELS[h] || h || "?";
  return `<span class="cv-badge" data-harness="${esc(h)}" title="harness: ${esc(label)}">${esc(label)}</span>`;
}

/** A nicer display label for a filesystem path: keep the tail. */
export function shortPath(p, segs = 3) {
  if (!p) return "";
  const parts = String(p).split("/").filter(Boolean);
  if (parts.length <= segs) return p;
  return "…/" + parts.slice(-segs).join("/");
}

export const ROLE_LABELS = {
  system: "System",
  user: "User",
  assistant: "Assistant",
  tool: "Tool",
};

export const HARNESS_LABELS = {
  claude: "Claude",
  codex: "Codex",
  grok: "Grok",
  opencode: "OpenCode",
  gemini: "Gemini",
  hermes: "Hermes",
  openclaw: "OpenClaw",
  opensession: "OpenSession",
};

// ---------------------------------------------------------------------------
// Normalization — accept both the internal IR (snake_case: tool_use,
// tool_result, created_at, data_ref, …) AND the OpenSession interchange shape
// (camelCase: toolUse, toolResult, createdAt, dataRef, parentId, …). The wasm
// ingest emits the former; a dropped OpenSession .json emits the latter. We
// converge everything onto the internal IR shape the components already speak.
// ---------------------------------------------------------------------------

function pick(obj, ...keys) {
  for (const k of keys) if (obj?.[k] != null) return obj[k];
  return undefined;
}

/** Normalize one usage object (camel or snake) to snake_case-ish keys. */
function normUsage(u) {
  if (!u || typeof u !== "object") return undefined;
  const out = {};
  const map = {
    input_tokens: ["input_tokens", "inputTokens"],
    output_tokens: ["output_tokens", "outputTokens"],
    cache_read_tokens: ["cache_read_tokens", "cacheReadTokens"],
    cache_creation_tokens: ["cache_creation_tokens", "cacheCreationTokens"],
  };
  const consumed = new Set();
  for (const [dest, srcs] of Object.entries(map)) {
    const v = pick(u, ...srcs);
    if (v != null) out[dest] = v;
    for (const s of srcs) consumed.add(s);
  }
  // keep any other numeric fields verbatim (don't re-add the camelCase aliases
  // we already folded into snake_case above)
  for (const [k, v] of Object.entries(u)) {
    if (!consumed.has(k) && !(k in out) && typeof v === "number") out[k] = v;
  }
  return Object.keys(out).length ? out : undefined;
}

/** Normalize one content block. Maps OpenSession kinds to internal kinds. */
function normBlock(b) {
  if (!b || typeof b !== "object") return { kind: "text", text: String(b ?? "") };
  let kind = b.kind;
  // OpenSession camelCase kinds -> internal snake_case kinds.
  if (kind === "toolUse") kind = "tool_use";
  else if (kind === "toolResult") kind = "tool_result";

  switch (kind) {
    case "text":
      return { kind: "text", text: b.text ?? "" };
    case "thinking":
      return {
        kind: "thinking",
        text: b.text ?? "",
        signature: b.signature,
        encrypted: b.encrypted,
        redacted: b.redacted,
      };
    case "tool_use":
      return {
        kind: "tool_use",
        id: pick(b, "id", "toolUseId", "tool_use_id"),
        name: b.name,
        input: b.input,
      };
    case "tool_result":
      return {
        kind: "tool_result",
        tool_use_id: pick(b, "tool_use_id", "toolUseId"),
        content: b.content,
        is_error: pick(b, "is_error", "isError") ?? false,
        tool_name: pick(b, "tool_name", "toolName"),
        status: b.status,
        details: b.details,
      };
    case "file":
      return {
        kind: "file",
        mime: pick(b, "mime", "mediaType", "media_type"),
        path: b.path,
        source: b.source,
      };
    case "image":
      return {
        kind: "image",
        media_type: pick(b, "media_type", "mediaType"),
        data_ref: pick(b, "data_ref", "dataRef"),
      };
    default:
      return { ...b, kind: kind ?? "unknown" };
  }
}

/** Normalize one message. */
function normMessage(m) {
  if (!m || typeof m !== "object") return { role: "user", content: [] };
  const content = Array.isArray(m.content) ? m.content.map(normBlock)
    : m.content != null ? [normBlock({ kind: "text", text: String(m.content) })]
    : [];
  return {
    id: m.id,
    parent_id: pick(m, "parent_id", "parentId"),
    role: (m.role || "user").toLowerCase(),
    timestamp: m.timestamp,
    model: m.model,
    usage: normUsage(m.usage),
    content,
    extra: m.extra,
  };
}

/**
 * Normalize a single session object from either shape into the internal IR.
 * Idempotent: re-normalizing an already-normalized session is a no-op-ish.
 */
export function normalizeSession(s) {
  if (!s || typeof s !== "object") return null;
  const harness = (s.harness || "openSession").toLowerCase();
  return {
    id: s.id ?? randomId(),
    harness,
    cwd: s.cwd,
    title: s.title,
    created_at: pick(s, "created_at", "createdAt"),
    updated_at: pick(s, "updated_at", "updatedAt"),
    model: s.model,
    git: s.git,
    extra: s.extra,
    source_path: pick(s, "source_path", "sourcePath"),
    // Metadata-only "stub" sessions (e.g. from cvd's /api/sessions) carry a count but no messages
    // yet; keep it so the list can show the real length before the transcript is hydrated.
    message_count: pick(s, "message_count", "messageCount"),
    messages: Array.isArray(s.messages) ? s.messages.map(normMessage) : [],
  };
}

/**
 * Accept anything that might be a list of sessions: an array of sessions, a
 * single session object, or an OpenSession document. Returns Session[].
 */
export function normalizeSessions(data) {
  if (Array.isArray(data)) return data.map(normalizeSession).filter(Boolean);
  if (data && typeof data === "object") {
    // A single OpenSession doc, or a wrapper { sessions: [...] }.
    if (Array.isArray(data.sessions)) return data.sessions.map(normalizeSession).filter(Boolean);
    if (data.openSession || data.messages || data.harness) {
      const one = normalizeSession(data);
      return one ? [one] : [];
    }
  }
  return [];
}

/** A loose uuid-ish id (no crypto dependency required, but use it when present). */
export function randomId() {
  if (globalThis.crypto?.randomUUID) return globalThis.crypto.randomUUID();
  const hex = (n) => Array.from({ length: n }, () => Math.floor(Math.random() * 16).toString(16)).join("");
  return `${hex(8)}-${hex(4)}-4${hex(3)}-${hex(4)}-${hex(12)}`;
}

// ---------------------------------------------------------------------------
// Export — OpenSession JSON + Markdown, fully client-side.
// ---------------------------------------------------------------------------

/** Convert an internal session (or composed list of messages) to an OpenSession doc. */
export function toOpenSession(session) {
  const blockOut = (b) => {
    switch (b.kind) {
      case "text": return { kind: "text", text: b.text ?? "" };
      case "thinking": return clean({ kind: "thinking", text: b.text ?? "", signature: b.signature, encrypted: b.encrypted, redacted: b.redacted });
      case "tool_use": return clean({ kind: "toolUse", id: b.id, name: b.name, input: b.input });
      case "tool_result": return clean({ kind: "toolResult", toolUseId: b.tool_use_id, content: b.content, isError: !!b.is_error, toolName: b.tool_name, status: b.status, details: b.details });
      case "file": return clean({ kind: "file", mime: b.mime, path: b.path, source: b.source });
      case "image": return clean({ kind: "image", mediaType: b.media_type, dataRef: b.data_ref });
      default: return { ...b };
    }
  };
  const usageOut = (u) => u && clean({
    inputTokens: u.input_tokens, outputTokens: u.output_tokens,
    cacheReadTokens: u.cache_read_tokens, cacheCreationTokens: u.cache_creation_tokens,
  });
  const msgOut = (m) => clean({
    id: m.id, parentId: m.parent_id, role: m.role, timestamp: m.timestamp,
    model: m.model, usage: usageOut(m.usage),
    content: (m.content || []).map(blockOut), extra: m.extra,
  });
  return clean({
    openSession: "0.1",
    harness: session.harness || "openSession",
    id: session.id || randomId(),
    cwd: session.cwd, title: session.title, model: session.model,
    createdAt: session.created_at, updatedAt: session.updated_at,
    git: session.git,
    messages: (session.messages || []).map(msgOut),
    extra: session.extra,
  });
}

/** Drop undefined/null/empty-object fields for tidy JSON. */
function clean(obj) {
  const out = {};
  for (const [k, v] of Object.entries(obj)) {
    if (v == null) continue;
    if (typeof v === "object" && !Array.isArray(v) && Object.keys(v).length === 0) continue;
    out[k] = v;
  }
  return out;
}

/** Render a session as readable Markdown. */
export function toMarkdown(session) {
  const lines = [];
  lines.push(`# ${session.title || sessionLabel(session)}`);
  lines.push("");
  const meta = [];
  if (session.harness) meta.push(`**harness:** ${session.harness}`);
  if (session.model) meta.push(`**model:** ${session.model}`);
  if (session.cwd) meta.push(`**cwd:** \`${session.cwd}\``);
  if (session.git?.branch) meta.push(`**branch:** ${session.git.branch}`);
  if (session.created_at) meta.push(`**created:** ${session.created_at}`);
  if (session.updated_at) meta.push(`**updated:** ${session.updated_at}`);
  if (session.id) meta.push(`**id:** \`${session.id}\``);
  if (meta.length) { lines.push(meta.join("  \n")); lines.push(""); }

  for (const m of session.messages || []) {
    const role = (m.role || "?").toUpperCase();
    const when = m.timestamp ? ` · ${fmtTime(m.timestamp)}` : "";
    lines.push(`## ${role}${when}`);
    lines.push("");
    for (const b of m.content || []) {
      switch (b.kind) {
        case "text":
          lines.push(b.text || ""); lines.push(""); break;
        case "thinking":
          lines.push("> 💭 *thinking*");
          lines.push((b.text || (b.encrypted ? "[encrypted reasoning]" : b.redacted ? "[redacted]" : "")).split("\n").map((l) => "> " + l).join("\n"));
          lines.push(""); break;
        case "tool_use":
          lines.push(`**🔧 tool_use → \`${b.name || "?"}\`**`);
          lines.push("```json"); lines.push(pretty(b.input)); lines.push("```"); lines.push(""); break;
        case "tool_result":
          lines.push(`**↳ tool_result${b.is_error ? " (error)" : ""}${b.status ? " · " + b.status : ""}**`);
          lines.push("```"); lines.push(String(b.content ?? "")); lines.push("```"); lines.push(""); break;
        case "file":
          lines.push(`**📄 file:** \`${b.path || b.source || ""}\`${b.mime ? ` (${b.mime})` : ""}`); lines.push(""); break;
        case "image":
          lines.push(`**🖼 image:** ${b.media_type || ""} ${b.data_ref || ""}`.trim()); lines.push(""); break;
        default:
          lines.push(`*[${b.kind} block]*`); lines.push(""); break;
      }
    }
  }
  return lines.join("\n");
}

/** Trigger a client-side download of a string as a file. */
export function downloadFile(filename, text, mime = "application/octet-stream") {
  const blob = new Blob([text], { type: mime });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 2000);
}

/** A filesystem-safe slug from a label. */
export function slug(s, max = 48) {
  return String(s ?? "session")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, max) || "session";
}

/** Total token usage across a session's messages. */
export function sumTokens(session) {
  let input = 0, output = 0, cacheRead = 0;
  for (const m of session.messages || []) {
    const u = m.usage;
    if (!u) continue;
    input += u.input_tokens || 0;
    output += u.output_tokens || 0;
    cacheRead += u.cache_read_tokens || 0;
  }
  return { input, output, cacheRead, total: input + output };
}
