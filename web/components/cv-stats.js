// <cv-stats> — a small, tasteful stats dashboard over the loaded session pool.
//
// API: stats.sessions = Session[]   (setter)
//
// Totals, per-harness counts, message counts, top cwds, date range, token
// sums. Charts are hand-rolled CSS bars + a tiny inline SVG activity sparkline.
// No chart library.
import { esc, fmtTime, sortTime, shortPath, sumTokens, HARNESS_LABELS } from "./util.js";

class CvStats extends HTMLElement {
  constructor() {
    super();
    this._sessions = [];
  }

  set sessions(arr) { this._sessions = Array.isArray(arr) ? arr : []; this.render(); }
  get sessions() { return this._sessions; }

  connectedCallback() { this.render(); }

  _compute() {
    const sessions = this._sessions;
    let messages = 0, tokIn = 0, tokOut = 0, tokCache = 0;
    const perHarness = new Map();
    const perRole = new Map();
    const cwds = new Map();
    const times = [];
    const blockKinds = new Map();

    for (const s of sessions) {
      const h = (s.harness || "?").toLowerCase();
      perHarness.set(h, (perHarness.get(h) || 0) + 1);
      if (s.cwd) cwds.set(s.cwd, (cwds.get(s.cwd) || 0) + 1);
      const t = sortTime(s);
      if (t > 0) times.push(t);
      const tk = sumTokens(s);
      tokIn += tk.input; tokOut += tk.output; tokCache += tk.cacheRead;
      for (const m of s.messages || []) {
        messages++;
        const r = (m.role || "?").toLowerCase();
        perRole.set(r, (perRole.get(r) || 0) + 1);
        for (const b of m.content || []) {
          const k = b?.kind || "?";
          blockKinds.set(k, (blockKinds.get(k) || 0) + 1);
        }
      }
    }

    times.sort((a, b) => a - b);
    return {
      sessions: sessions.length, messages, tokIn, tokOut, tokCache,
      perHarness, perRole, blockKinds,
      cwds: [...cwds.entries()].sort((a, b) => b[1] - a[1]).slice(0, 6),
      range: times.length ? [times[0], times[times.length - 1]] : null,
      times,
    };
  }

  render() {
    if (!this._sessions.length) {
      this.innerHTML = `<div class="view-empty muted"><p>No sessions loaded.</p></div>`;
      return;
    }
    const c = this._compute();

    const cards = [
      ["Sessions", c.sessions],
      ["Messages", c.messages.toLocaleString()],
      ["Harnesses", c.perHarness.size],
      ["Input tokens", c.tokIn ? c.tokIn.toLocaleString() : "—"],
      ["Output tokens", c.tokOut ? c.tokOut.toLocaleString() : "—"],
      ["Cached tokens", c.tokCache ? c.tokCache.toLocaleString() : "—"],
    ].map(([k, v]) => `<div class="stat-card"><div class="stat-num">${esc(String(v))}</div><div class="stat-label muted">${esc(k)}</div></div>`).join("");

    this.innerHTML = `
      <div class="view-head"><h2>Stats</h2>
        <span class="muted">${c.range ? `${esc(fmtTime(new Date(c.range[0]).toISOString()))} → ${esc(fmtTime(new Date(c.range[1]).toISOString()))}` : "no timestamps"}</span>
      </div>
      <div class="stat-grid">${cards}</div>
      <div class="stat-cols">
        <section class="stat-block">
          <h3>Sessions per harness</h3>
          ${this._barsHtml(c.perHarness, (k) => HARNESS_LABELS[k] || k, (k) => `var(--h-${k}, var(--accent))`)}
        </section>
        <section class="stat-block">
          <h3>Messages by role</h3>
          ${this._barsHtml(c.perRole, (k) => k, () => "var(--accent)")}
        </section>
        <section class="stat-block">
          <h3>Block kinds</h3>
          ${this._barsHtml(c.blockKinds, (k) => k, () => "var(--h-codex)")}
        </section>
        <section class="stat-block">
          <h3>Top working directories</h3>
          ${c.cwds.length ? `<ul class="cwd-list">${c.cwds.map(([p, n]) => `<li><span class="cwd-path" title="${esc(p)}">${esc(shortPath(p, 3))}</span><span class="cwd-count muted">${n}</span></li>`).join("")}</ul>` : '<p class="muted">none recorded</p>'}
        </section>
      </div>
      <section class="stat-block">
        <h3>Activity over time</h3>
        ${this._sparkHtml(c.times)}
      </section>
    `;
  }

  _barsHtml(map, labelFn, colorFn) {
    const entries = [...map.entries()].sort((a, b) => b[1] - a[1]);
    if (!entries.length) return '<p class="muted">none</p>';
    const max = Math.max(...entries.map((e) => e[1]));
    return `<div class="bars">${entries.map(([k, v]) => `
      <div class="bar-row">
        <span class="bar-label">${esc(labelFn(k))}</span>
        <span class="bar-track"><span class="bar-fill" style="width:${(v / max * 100).toFixed(1)}%; background:${colorFn(k)}"></span></span>
        <span class="bar-val muted">${v}</span>
      </div>`).join("")}</div>`;
  }

  // A tiny inline SVG: histogram of session activity over the loaded date range.
  _sparkHtml(times) {
    if (times.length < 2) return '<p class="muted">not enough dated sessions to chart</p>';
    const min = times[0], max = times[times.length - 1];
    const span = Math.max(1, max - min);
    const BUCKETS = 24;
    const buckets = new Array(BUCKETS).fill(0);
    for (const t of times) {
      const i = Math.min(BUCKETS - 1, Math.floor((t - min) / span * BUCKETS));
      buckets[i]++;
    }
    const peak = Math.max(...buckets, 1);
    const W = 600, H = 80, bw = W / BUCKETS;
    const rects = buckets.map((v, i) => {
      const h = v / peak * (H - 8);
      return `<rect x="${(i * bw + 1).toFixed(1)}" y="${(H - h).toFixed(1)}" width="${(bw - 2).toFixed(1)}" height="${h.toFixed(1)}" rx="2" fill="var(--accent)" opacity="${v ? 0.85 : 0.15}"><title>${v} session${v === 1 ? "" : "s"}</title></rect>`;
    }).join("");
    return `<svg class="spark" viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" role="img" aria-label="Activity histogram">${rects}</svg>`;
  }
}

customElements.define("cv-stats", CvStats);
