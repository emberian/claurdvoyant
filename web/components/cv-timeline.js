// <cv-timeline> — the scrying glass: a bird's-eye view of every session across all harnesses and
// all of time. A GitHub-style activity heatmap up top (a constellation of your working days), a
// harness legend with totals, a few at-a-glance numbers, and a day-grouped feed below.
//
// API: timeline.sessions = Session[]   (setter)
//      emits "open" CustomEvent { detail: { session } } when a node is clicked.
import { esc, sortTime, sessionLabel, shortPath, msgCount, harnessBadge, HARNESS_LABELS } from "./util.js";

const DAY_MS = 86400000;
// Heatmap look-back cap: one mis-dated session (epoch 0, a bad clock) would otherwise
// stretch the grid to thousands of week columns. Older sessions still show in the feed.
const HEATMAP_MAX_YEARS = 6;

function dayKey(d) {
  // Local YYYY-MM-DD.
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function startOfDay(d) {
  const x = new Date(d);
  x.setHours(0, 0, 0, 0);
  return x;
}

function startOfWeek(d) {
  const x = startOfDay(d);
  x.setDate(x.getDate() - x.getDay()); // back to Sunday
  return x;
}

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

class CvTimeline extends HTMLElement {
  constructor() {
    super();
    this._sessions = [];
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

    // --- Aggregate per day + per harness.
    const byDay = new Map(); // key -> { count, harnesses:Set }
    const byHarness = new Map(); // harness -> count
    const dated = [];
    let undated = 0;
    for (const s of this._sessions) {
      const h = (s.harness || "").toLowerCase();
      byHarness.set(h, (byHarness.get(h) || 0) + 1);
      const t = sortTime(s);
      if (!t) {
        undated++;
        continue;
      }
      dated.push({ s, t });
      const key = dayKey(new Date(t));
      let e = byDay.get(key);
      if (!e) {
        e = { count: 0, harnesses: new Set() };
        byDay.set(key, e);
      }
      e.count++;
      e.harnesses.add(h);
    }
    dated.sort((a, b) => b.t - a.t);

    const heatmap = dated.length ? this._heatmapHtml(byDay) : "";
    const legend = this._legendHtml(byHarness);
    const stats = this._statsHtml(byDay, dated, undated);
    const feed = this._feedHtml(dated, undated);

    this.innerHTML = `
      <div class="view-head">
        <h2>🔮 Timeline</h2>
        <span class="muted">${this._sessions.length} session${this._sessions.length === 1 ? "" : "s"} across ${byHarness.size} harness${byHarness.size === 1 ? "" : "es"}</span>
      </div>
      ${stats}
      ${heatmap}
      ${legend}
      <div class="tl-feed">${feed}</div>`;

    this._wire();
    // Open scrolled to the most recent activity (right edge of the heatmap).
    const scroller = this.querySelector(".hm-scroll");
    if (scroller) scroller.scrollLeft = scroller.scrollWidth;
  }

  // ---- activity heatmap ----------------------------------------------------
  _heatmapHtml(byDay) {
    let max = 1;
    for (const e of byDay.values()) max = Math.max(max, e.count);

    // Build week columns from the Sunday on/before the earliest day through today,
    // clamped to at most HEATMAP_MAX_YEARS back so a mis-dated outlier can't blow
    // up the grid (days before the clamp simply don't get heatmap cells).
    let earliest = Infinity;
    for (const key of byDay.keys()) {
      const t = new Date(key + "T00:00:00").getTime();
      if (t < earliest) earliest = t;
    }
    const today = startOfDay(new Date());
    const floor = new Date(today);
    floor.setFullYear(floor.getFullYear() - HEATMAP_MAX_YEARS);
    const start = startOfWeek(new Date(Math.max(earliest, floor.getTime())));

    const columns = []; // each: { month, year, cells: [{key,count,level,date}|null]×7 }
    let cur = new Date(start);
    while (cur <= today) {
      const cells = [];
      let colMonth = cur.getMonth();
      let colYear = cur.getFullYear();
      for (let i = 0; i < 7; i++) {
        if (cur > today) {
          cells.push(null);
        } else {
          const key = dayKey(cur);
          const e = byDay.get(key);
          const count = e ? e.count : 0;
          const level = count === 0 ? 0 : Math.min(4, Math.ceil((count / max) * 4));
          cells.push({ key, count, level, date: new Date(cur) });
          if (i === 0) {
            colMonth = cur.getMonth();
            colYear = cur.getFullYear();
          }
        }
        cur = new Date(cur.getTime() + DAY_MS);
      }
      columns.push({ month: colMonth, year: colYear, cells });
    }

    // Month label row: a label whenever the month changes between columns.
    let labels = "";
    let lastMonth = -1;
    columns.forEach((col, i) => {
      if (col.month !== lastMonth) {
        const showYear = col.month === 0 || lastMonth === -1;
        labels += `<span class="hm-month" style="grid-column:${i + 2}">${MONTHS[col.month]}${showYear ? ` ’${String(col.year).slice(2)}` : ""}</span>`;
        lastMonth = col.month;
      }
    });

    // Cells grid: row 1 is the month-label row, rows 2–8 are Sun…Sat; column 1 is weekday labels.
    let grid = `
      <div class="hm-weekday" style="grid-row:3">Mon</div>
      <div class="hm-weekday" style="grid-row:5">Wed</div>
      <div class="hm-weekday" style="grid-row:7">Fri</div>`;
    columns.forEach((col, ci) => {
      col.cells.forEach((c, ri) => {
        if (!c) {
          grid += `<div class="hm-cell hm-empty" style="grid-column:${ci + 2};grid-row:${ri + 2}"></div>`;
          return;
        }
        const label = c.date.toLocaleDateString(undefined, { weekday: "short", month: "short", day: "numeric", year: "numeric" });
        const title = c.count ? `${label} — ${c.count} session${c.count === 1 ? "" : "s"}` : label;
        grid += `<div class="hm-cell" data-level="${c.level}" data-day="${c.key}" data-count="${c.count}"
          style="grid-column:${ci + 2};grid-row:${ri + 2}" title="${esc(title)}" tabindex="${c.count ? 0 : -1}" role="${c.count ? "button" : "presentation"}"></div>`;
      });
    });

    return `
      <div class="hm-scroll">
        <div class="hm-grid" style="grid-template-columns: auto repeat(${columns.length}, var(--hm-cell)); grid-template-rows: auto repeat(7, var(--hm-cell));">
          <div class="hm-months" style="grid-column:1 / -1; grid-row:1; display:grid; grid-template-columns:subgrid;">${labels}</div>
          ${grid}
        </div>
      </div>`;
  }

  _legendHtml(byHarness) {
    const items = [...byHarness.entries()]
      .sort((a, b) => b[1] - a[1])
      .map(
        ([h, n]) => `
        <span class="tl-legend-item">
          <span class="tl-legend-dot" style="background: var(--h-${esc(h)}, var(--fg-muted))"></span>
          ${esc(HARNESS_LABELS[h] || h || "?")}
          <span class="muted">${n}</span>
        </span>`
      )
      .join("");
    return `<div class="tl-legend">${items}</div>`;
  }

  _statsHtml(byDay, dated, undated) {
    const activeDays = byDay.size;
    let busiest = null;
    for (const [key, e] of byDay) {
      if (!busiest || e.count > busiest.count) busiest = { key, count: e.count };
    }
    const span =
      dated.length > 1 ? Math.round((dated[0].t - dated[dated.length - 1].t) / DAY_MS) : 0;
    const stat = (label, value) =>
      `<div class="tl-stat"><div class="tl-stat-n">${value}</div><div class="tl-stat-l muted">${label}</div></div>`;
    const busiestLabel = busiest
      ? `${new Date(busiest.key + "T00:00:00").toLocaleDateString(undefined, { month: "short", day: "numeric" })} · ${busiest.count}`
      : "—";
    return `
      <div class="tl-stats">
        ${stat("active days", activeDays.toLocaleString())}
        ${stat("busiest day", busiestLabel)}
        ${stat("days spanned", span ? span.toLocaleString() : "—")}
        ${undated ? stat("undated", undated.toLocaleString()) : ""}
      </div>`;
  }

  // ---- day-grouped feed ----------------------------------------------------
  _feedHtml(dated, undated) {
    // Group by local day, newest first. `content-visibility:auto` on each day group lets the browser
    // skip layout/paint for off-screen groups — cheap virtualization for thousands of sessions.
    const groups = new Map();
    for (const d of dated) {
      const key = dayKey(new Date(d.t));
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key).push(d);
    }
    let html = "";
    for (const [key, items] of groups) {
      const label = new Date(key + "T00:00:00").toLocaleDateString(undefined, {
        weekday: "short",
        year: "numeric",
        month: "short",
        day: "numeric",
      });
      html += `<div class="tl-day" data-day="${esc(key)}">
        <div class="tl-day-label">${esc(label)} <span class="muted">· ${items.length}</span></div>
        <div class="tl-items">${items.map(({ s, t }) => this._nodeHtml(s, t)).join("")}</div>
      </div>`;
    }
    if (undated) {
      const u = this._sessions.filter((s) => !sortTime(s));
      html += `<div class="tl-day" data-day="undated">
        <div class="tl-day-label muted">undated <span>· ${undated}</span></div>
        <div class="tl-items">${u.map((s) => this._nodeHtml(s, 0)).join("")}</div>
      </div>`;
    }
    return html;
  }

  _nodeHtml(s, t) {
    const h = (s.harness || "").toLowerCase();
    const time = t ? new Date(t).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" }) : "";
    const count = msgCount(s);
    return `
      <div class="tl-node" style="--c: var(--h-${esc(h)}, var(--fg-muted))" data-id="${esc(s.id || "")}" tabindex="0" role="button" title="Open ${esc(sessionLabel(s))}">
        <span class="tl-dot"></span>
        <span class="tl-time muted">${esc(time)}</span>
        ${harnessBadge(h)}
        <span class="tl-title">${esc(sessionLabel(s))}</span>
        <span class="tl-meta muted">${count} msg${count === 1 ? "" : "s"}${s.cwd ? ` · ${esc(shortPath(s.cwd, 2))}` : ""}</span>
      </div>`;
  }

  // ---- wiring (event delegation: one listener each, not per-node) -----------
  _wire() {
    const feed = this.querySelector(".tl-feed");
    if (feed) {
      const open = (el) => {
        const id = el?.dataset.id;
        const s = id && this._sessions.find((x) => (x.id || "") === id);
        if (s) this.dispatchEvent(new CustomEvent("open", { detail: { session: s }, bubbles: true }));
      };
      feed.addEventListener("click", (e) => open(e.target.closest(".tl-node")));
      feed.addEventListener("keydown", (e) => {
        if (e.key === "Enter" || e.key === " ") {
          const node = e.target.closest(".tl-node");
          if (node) {
            e.preventDefault();
            open(node);
          }
        }
      });
    }

    // Click a heatmap cell → reveal that day in the feed.
    const grid = this.querySelector(".hm-grid");
    if (grid) {
      const jump = (cell) => {
        const day = cell?.dataset.day;
        if (!day || cell.dataset.count === "0") return;
        const group = this.querySelector(`.tl-day[data-day="${CSS.escape(day)}"]`);
        if (group) {
          group.scrollIntoView({ behavior: "smooth", block: "center" });
          group.classList.remove("tl-flash");
          void group.offsetWidth; // restart the animation
          group.classList.add("tl-flash");
        }
      };
      grid.addEventListener("click", (e) => jump(e.target.closest(".hm-cell")));
      grid.addEventListener("keydown", (e) => {
        if (e.key === "Enter" || e.key === " ") {
          const cell = e.target.closest(".hm-cell");
          if (cell) {
            e.preventDefault();
            jump(cell);
          }
        }
      });
    }
  }
}

customElements.define("cv-timeline", CvTimeline);
