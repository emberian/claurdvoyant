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
          if (b.input != null) parts.push(pretty(b.input));
          break;
        case "tool_result":
          if (b.content) parts.push(b.content);
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
};
