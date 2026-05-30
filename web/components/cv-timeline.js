// <cv-timeline> — all sessions across all harnesses on one chronological axis.
//
// API: timeline.sessions = Session[]   (setter)
//      emits "open" CustomEvent { detail: { session } } when a node is clicked.
//
// A unified "what happened when" feed: each session is a dot on a vertical
// time axis, colored by harness, grouped by day, newest first.
import "./cv-harness-badge.js";
import { esc, fmtTime, sortTime, sessionLabel, shortPath, HARNESS_LABELS } from "./util.js";

class CvTimeline extends HTMLElement {
  constructor() {
    super();
    this._sessions = [];
  }

  set sessions(arr) { this._sessions = Array.isArray(arr) ? arr : []; this.render(); }
  get sessions() { return this._sessions; }

  connectedCallback() { this.render(); }

  render() {
    const dated = this._sessions
      .map((s) => ({ s, t: sortTime(s) }))
      .sort((a, b) => b.t - a.t);

    if (!dated.length) {
      this.innerHTML = `<div class="view-empty muted"><p>No sessions loaded.</p></div>`;
      return;
    }

    const withTime = dated.filter((d) => d.t > 0);
    const undated = dated.filter((d) => d.t === 0);

    // Group by local day.
    const groups = new Map();
    for (const d of withTime) {
      const day = new Date(d.t).toLocaleDateString(undefined, { weekday: "short", year: "numeric", month: "short", day: "numeric" });
      if (!groups.has(day)) groups.set(day, []);
      groups.get(day).push(d);
    }

    const span = withTime.length
      ? `${fmtTime(new Date(withTime[withTime.length - 1].t).toISOString())} → ${fmtTime(new Date(withTime[0].t).toISOString())}`
      : "";

    let html = `
      <div class="view-head">
        <h2>Timeline</h2>
        <span class="muted">${this._sessions.length} session${this._sessions.length === 1 ? "" : "s"}${span ? ` · ${esc(span)}` : ""}</span>
      </div>
      <div class="timeline">`;

    for (const [day, items] of groups) {
      html += `<div class="tl-day"><div class="tl-day-label">${esc(day)}</div><div class="tl-items">`;
      for (const { s, t } of items) html += this._nodeHtml(s, t);
      html += `</div></div>`;
    }

    if (undated.length) {
      html += `<div class="tl-day"><div class="tl-day-label muted">undated</div><div class="tl-items">`;
      for (const { s } of undated) html += this._nodeHtml(s, 0);
      html += `</div></div>`;
    }

    html += `</div>`;
    this.innerHTML = html;

    this.querySelectorAll(".tl-node").forEach((node) => {
      const fire = () => {
        const s = this._sessions.find((x) => (x.id || "") === node.dataset.id);
        if (s) this.dispatchEvent(new CustomEvent("open", { detail: { session: s }, bubbles: true }));
      };
      node.addEventListener("click", fire);
      node.addEventListener("keydown", (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); fire(); } });
    });
  }

  _nodeHtml(s, t) {
    const h = (s.harness || "").toLowerCase();
    const time = t ? new Date(t).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" }) : "";
    const count = s.messages?.length || 0;
    return `
      <div class="tl-node" data-id="${esc(s.id || "")}" data-harness="${esc(h)}" tabindex="0" role="button" title="Open ${esc(sessionLabel(s))}">
        <span class="tl-dot" style="background: var(--h-${esc(h)}, var(--fg-muted))"></span>
        <span class="tl-time muted">${esc(time)}</span>
        <cv-harness-badge harness="${esc(h)}"></cv-harness-badge>
        <span class="tl-title">${esc(sessionLabel(s))}</span>
        <span class="tl-meta muted">${count} msg${count === 1 ? "" : "s"}${s.cwd ? ` · ${esc(shortPath(s.cwd, 2))}` : ""}</span>
      </div>`;
  }
}

customElements.define("cv-timeline", CvTimeline);
