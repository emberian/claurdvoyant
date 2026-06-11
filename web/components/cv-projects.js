// <cv-projects> — the project lens. Sessions belong to repos (their cwd), and many agents across
// many harnesses work in the same repo over time — but that structure is invisible in a flat list.
// This groups the whole corpus by project: each a card with a per-harness breakdown bar, a time
// span, and an expandable list of its sessions. "What happened in this repo, across all my agents?"
//
// API: projects.sessions = Session[]   (setter)
//      emits "open" CustomEvent { detail: { session } } when a session is clicked.
import { esc, sessionLabel, sortTime, shortPath, msgCount, harnessBadge, HARNESS_LABELS } from "./util.js";
import { getTouched } from "./hydrate.js";

const DAY_MS = 86400000;

function relTime(t) {
  if (!t) return "";
  const d = Date.now() - t;
  if (d < 60_000) return "just now";
  if (d < 3_600_000) return `${Math.round(d / 60_000)}m ago`;
  if (d < DAY_MS) return `${Math.round(d / 3_600_000)}h ago`;
  if (d < 30 * DAY_MS) return `${Math.round(d / DAY_MS)}d ago`;
  if (d < 365 * DAY_MS) return `${Math.round(d / (30 * DAY_MS))}mo ago`;
  return `${Math.round(d / (365 * DAY_MS))}y ago`;
}

function projectName(cwd) {
  if (!cwd) return "(no project)";
  const parts = String(cwd).split("/").filter(Boolean);
  return parts[parts.length - 1] || cwd;
}

class CvProjects extends HTMLElement {
  constructor() {
    super();
    this._sessions = [];
    this._byKey = new Map(); // cwd -> sessions[] (for lazy expand)
    this._expanded = new Set();
  }

  set sessions(arr) {
    this._sessions = Array.isArray(arr) ? arr : [];
    this.render();
  }
  get sessions() {
    return this._sessions;
  }

  connectedCallback() {
    this.render();
  }

  render() {
    if (!this._sessions.length) {
      this.innerHTML = `<div class="view-empty muted"><p>No sessions loaded.</p></div>`;
      return;
    }

    // Group by cwd.
    const groups = new Map(); // key -> { sessions, byHarness:Map, last, first }
    for (const s of this._sessions) {
      const key = s.cwd || "";
      let g = groups.get(key);
      if (!g) {
        g = { sessions: [], byHarness: new Map(), last: 0, first: Infinity };
        groups.set(key, g);
      }
      g.sessions.push(s);
      const h = (s.harness || "").toLowerCase();
      g.byHarness.set(h, (g.byHarness.get(h) || 0) + 1);
      const t = sortTime(s);
      if (t) {
        if (t > g.last) g.last = t;
        if (t < g.first) g.first = t;
      }
    }

    // Most-recently-active projects first.
    const projects = [...groups.entries()].sort((a, b) => b[1].last - a[1].last);
    this._byKey = groups;

    const cards = projects.map(([key, g]) => this._cardHtml(key, g)).join("");

    this.innerHTML = `
      <div class="view-head">
        <h2>◈ Projects</h2>
        <span class="muted">${projects.length} project${projects.length === 1 ? "" : "s"} · ${this._sessions.length} session${this._sessions.length === 1 ? "" : "s"}</span>
      </div>
      <div class="touched-box">
        <input class="touched-input" type="search"
          placeholder="Which sessions touched a file? e.g. src/main.rs"
          aria-label="Touched-file lookup" value="${esc(this._touchedQuery || "")}" />
        <label class="touched-edits muted"><input type="checkbox" class="touched-edits-cb" ${this._touchedEdits ? "checked" : ""}/> edits only</label>
        <button type="button" class="mini-btn touched-go">touched</button>
        <div class="touched-results"></div>
      </div>
      <div class="proj-list">${cards}</div>`;

    this._wire();
    this._wireTouched();
    // Re-render any previously-expanded project bodies.
    for (const key of this._expanded) this._populate(key);
  }

  // ---- "touched" lookup ----------------------------------------------------
  // Who touched a file, across the whole corpus — cvd's `/api/touched` (or the desktop's
  // `local_touched`). Degrades to an explanatory note when no capable cvd is reachable.
  _wireTouched() {
    const input = this.querySelector(".touched-input");
    const go = () => {
      this._touchedQuery = input.value.trim();
      this._touchedEdits = this.querySelector(".touched-edits-cb")?.checked || false;
      this._runTouched();
    };
    this.querySelector(".touched-go")?.addEventListener("click", go);
    input?.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); go(); } });
    this.querySelector(".touched-results")?.addEventListener("click", (e) => {
      const row = e.target.closest(".touched-row");
      if (!row) return;
      const s = this._sessions.find((x) =>
        (x.id || "") === row.dataset.id && (x.harness || "").toLowerCase() === row.dataset.harness);
      if (s) this.dispatchEvent(new CustomEvent("open", { detail: { session: s }, bubbles: true }));
    });
  }

  async _runTouched() {
    const out = this.querySelector(".touched-results");
    if (!out) return;
    const path = this._touchedQuery;
    if (!path) { out.innerHTML = ""; return; }
    out.innerHTML = `<div class="muted">Looking up ${esc(path)}…</div>`;
    const rows = await getTouched(path, this._touchedEdits);
    if (out !== this.querySelector(".touched-results")) return; // re-rendered meanwhile
    if (rows === null) {
      out.innerHTML = `<div class="muted">Touched lookup needs a local <code>cvd serve</code> (0.9.12+) — none was reachable.</div>`;
      return;
    }
    if (!rows.length) {
      out.innerHTML = `<div class="muted">No sessions ${this._touchedEdits ? "edited" : "touched"} ${esc(path)} (events are ingested by <code>cv index</code>).</div>`;
      return;
    }
    out.innerHTML = rows.map((t) => {
      const date = t.last_ts ? new Date(t.last_ts * 1000).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" }) : "";
      const counts = [
        t.edits ? `${t.edits} edit${t.edits === 1 ? "" : "s"}` : "",
        t.reads ? `${t.reads} read${t.reads === 1 ? "" : "s"}` : "",
      ].filter(Boolean).join(", ");
      const inPool = this._sessions.some((x) =>
        (x.id || "") === t.session_id && (x.harness || "").toLowerCase() === (t.harness || "").toLowerCase());
      return `
        <div class="touched-row${inPool ? " in-pool" : ""}" data-id="${esc(t.session_id)}" data-harness="${esc((t.harness || "").toLowerCase())}"
          ${inPool ? `tabindex="0" role="button" title="Open ${esc(t.title || t.session_id)}"` : ""}>
          ${harnessBadge(t.harness)}
          <span class="touched-title">${esc(t.title || t.session_id)}</span>
          <span class="touched-meta muted">${esc(counts)}${date ? ` · ${esc(date)}` : ""}</span>
        </div>`;
    }).join("");
  }

  _cardHtml(key, g) {
    const total = g.sessions.length;
    const name = projectName(key);
    const path = key ? esc(key) : "";
    // Stacked harness bar, biggest first.
    const harnesses = [...g.byHarness.entries()].sort((a, b) => b[1] - a[1]);
    const bar = harnesses
      .map(([h, n]) => `<span class="proj-seg" style="flex:${n}; background: var(--h-${esc(h)}, var(--fg-muted))" title="${esc(HARNESS_LABELS[h] || h || "?")}: ${n}"></span>`)
      .join("");
    const chips = harnesses
      .map(([h, n]) => `<span class="proj-chip"><span class="proj-chip-dot" style="background: var(--h-${esc(h)}, var(--fg-muted))"></span>${esc(HARNESS_LABELS[h] || h || "?")} <span class="muted">${n}</span></span>`)
      .join("");
    const span = g.first < Infinity && g.last > g.first
      ? `${new Date(g.first).toLocaleDateString(undefined, { month: "short", year: "2-digit" })} → ${relTime(g.last)}`
      : relTime(g.last);
    const open = this._expanded.has(key);
    return `
      <div class="proj" data-key="${esc(key)}">
        <button class="proj-head" aria-expanded="${open}">
          <span class="proj-caret">${open ? "▾" : "▸"}</span>
          <span class="proj-mark">◈</span>
          <span class="proj-name">${esc(name)}</span>
          <span class="proj-count">${total}</span>
          <span class="proj-when muted">${esc(span)}</span>
        </button>
        <div class="proj-bar" aria-hidden="true">${bar}</div>
        <div class="proj-chips">${chips}${path ? `<span class="proj-path muted" title="${path}">${esc(shortPath(key, 4))}</span>` : ""}</div>
        <div class="proj-sessions" ${open ? "" : "hidden"}></div>
      </div>`;
  }

  _populate(key) {
    const card = this.querySelector(`.proj[data-key="${CSS.escape(key)}"] .proj-sessions`);
    if (!card || card.dataset.filled) return;
    const sessions = (this._byKey.get(key)?.sessions || [])
      .slice()
      .sort((a, b) => sortTime(b) - sortTime(a));
    card.innerHTML = sessions.map((s) => this._rowHtml(s)).join("");
    card.dataset.filled = "1";
  }

  _rowHtml(s) {
    const h = (s.harness || "").toLowerCase();
    const t = sortTime(s);
    const when = t ? new Date(t).toLocaleDateString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" }) : "";
    const n = msgCount(s);
    return `
      <div class="proj-row" data-id="${esc(s.id || "")}" tabindex="0" role="button" title="Open ${esc(sessionLabel(s))}">
        ${harnessBadge(h)}
        <span class="proj-row-title">${esc(sessionLabel(s))}</span>
        <span class="proj-row-meta muted">${esc(when)} · ${n} msg${n === 1 ? "" : "s"}</span>
      </div>`;
  }

  _wire() {
    const list = this.querySelector(".proj-list");
    if (!list) return;
    list.addEventListener("click", (e) => {
      const row = e.target.closest(".proj-row");
      if (row) {
        const s = this._sessions.find((x) => (x.id || "") === row.dataset.id);
        if (s) this.dispatchEvent(new CustomEvent("open", { detail: { session: s }, bubbles: true }));
        return;
      }
      const head = e.target.closest(".proj-head");
      if (head) this._toggle(head.closest(".proj"));
    });
    list.addEventListener("keydown", (e) => {
      if (e.key !== "Enter" && e.key !== " ") return;
      const row = e.target.closest(".proj-row");
      if (row) {
        e.preventDefault();
        const s = this._sessions.find((x) => (x.id || "") === row.dataset.id);
        if (s) this.dispatchEvent(new CustomEvent("open", { detail: { session: s }, bubbles: true }));
      }
    });
  }

  _toggle(card) {
    if (!card) return;
    const key = card.dataset.key;
    const body = card.querySelector(".proj-sessions");
    const head = card.querySelector(".proj-head");
    const caret = card.querySelector(".proj-caret");
    const open = this._expanded.has(key);
    if (open) {
      this._expanded.delete(key);
      body.hidden = true;
      head.setAttribute("aria-expanded", "false");
      caret.textContent = "▸";
    } else {
      this._expanded.add(key);
      this._populate(key); // lazy-render rows on first open
      body.hidden = false;
      head.setAttribute("aria-expanded", "true");
      caret.textContent = "▾";
    }
  }
}

customElements.define("cv-projects", CvProjects);
