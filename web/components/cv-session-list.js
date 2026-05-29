// <cv-session-list> — sortable/filterable list of sessions.
//
// API:
//   list.sessions = Session[]          (setter; triggers re-render)
//   emits "select" CustomEvent with detail = { session } when a row is clicked
//
// Filters: free-text search (matches title/cwd/model/all message content),
// harness multi-select, sort by recency / title / message count.
import "./cv-harness-badge.js";
import {
  esc, fmtTime, sortTime, searchableText, sessionLabel, shortPath,
  HARNESS_LABELS,
} from "./util.js";

class CvSessionList extends HTMLElement {
  constructor() {
    super();
    this._sessions = [];
    this._query = "";
    this._harnessFilter = new Set(); // empty = all
    this._sort = "recent";
    this._selectedId = null;
    this._searchCache = new WeakMap();
  }

  set sessions(arr) {
    this._sessions = Array.isArray(arr) ? arr : [];
    this._searchCache = new WeakMap();
    // Drop any harness filters that no longer apply.
    const present = new Set(this._sessions.map((s) => (s.harness || "").toLowerCase()));
    for (const h of [...this._harnessFilter]) if (!present.has(h)) this._harnessFilter.delete(h);
    this.render();
  }
  get sessions() { return this._sessions; }

  set selectedId(id) {
    this._selectedId = id;
    this._updateSelection();
  }

  connectedCallback() { this.render(); }

  _search(session) {
    let v = this._searchCache.get(session);
    if (v === undefined) {
      v = searchableText(session);
      this._searchCache.set(session, v);
    }
    return v;
  }

  _filtered() {
    const q = this._query.trim().toLowerCase();
    let rows = this._sessions.filter((s) => {
      const h = (s.harness || "").toLowerCase();
      if (this._harnessFilter.size && !this._harnessFilter.has(h)) return false;
      if (q && !this._search(s).includes(q)) return false;
      return true;
    });
    const cmp = {
      recent: (a, b) => sortTime(b) - sortTime(a),
      oldest: (a, b) => sortTime(a) - sortTime(b),
      title: (a, b) => sessionLabel(a).localeCompare(sessionLabel(b)),
      messages: (a, b) => (b.messages?.length || 0) - (a.messages?.length || 0),
    }[this._sort] || ((a, b) => 0);
    return rows.slice().sort(cmp);
  }

  render() {
    const present = [...new Set(this._sessions.map((s) => (s.harness || "").toLowerCase()))]
      .filter(Boolean)
      .sort();

    const harnessChips = present.map((h) => {
      const on = this._harnessFilter.has(h);
      return `<button type="button" class="chip${on ? " on" : ""}" data-harness="${esc(h)}"
        aria-pressed="${on}">${esc(HARNESS_LABELS[h] || h)}</button>`;
    }).join("");

    const rows = this._filtered();
    const rowHtml = rows.map((s) => this._rowHtml(s)).join("");

    this.innerHTML = `
      <div class="list-controls">
        <input type="search" class="search" placeholder="Search messages, titles, paths…"
          aria-label="Search sessions" value="${esc(this._query)}" />
        <div class="control-row">
          <div class="chips" role="group" aria-label="Filter by harness">${harnessChips || '<span class="muted">no sessions</span>'}</div>
          <label class="sort">
            <span class="muted">Sort</span>
            <select aria-label="Sort sessions">
              <option value="recent">Most recent</option>
              <option value="oldest">Oldest</option>
              <option value="title">Title A–Z</option>
              <option value="messages">Most messages</option>
            </select>
          </label>
        </div>
      </div>
      <div class="list-count muted">${rows.length} of ${this._sessions.length} session${this._sessions.length === 1 ? "" : "s"}</div>
      <ul class="session-rows" role="listbox" aria-label="Sessions">
        ${rowHtml || '<li class="empty muted">No sessions match your filters.</li>'}
      </ul>
    `;

    this.querySelector("select").value = this._sort;

    // Wire events.
    const search = this.querySelector(".search");
    search.addEventListener("input", () => { this._query = search.value; this._rerenderRows(); });
    this.querySelector("select").addEventListener("change", (e) => {
      this._sort = e.target.value; this._rerenderRows();
    });
    this.querySelectorAll(".chip").forEach((btn) => {
      btn.addEventListener("click", () => {
        const h = btn.dataset.harness;
        if (this._harnessFilter.has(h)) this._harnessFilter.delete(h);
        else this._harnessFilter.add(h);
        this.render();
      });
    });
    this._wireRows();
  }

  _rerenderRows() {
    const rows = this._filtered();
    this.querySelector(".list-count").textContent =
      `${rows.length} of ${this._sessions.length} session${this._sessions.length === 1 ? "" : "s"}`;
    const ul = this.querySelector(".session-rows");
    ul.innerHTML = rows.map((s) => this._rowHtml(s)).join("") ||
      '<li class="empty muted">No sessions match your filters.</li>';
    this._wireRows();
  }

  _rowHtml(s) {
    const id = s.id || "";
    const sel = id && id === this._selectedId ? " selected" : "";
    const when = fmtTime(s.updated_at || s.created_at);
    const count = s.messages?.length || 0;
    const cwd = s.cwd ? `<span class="row-cwd" title="${esc(s.cwd)}">${esc(shortPath(s.cwd))}</span>` : "";
    return `
      <li class="session-row${sel}" role="option" aria-selected="${!!sel}" data-id="${esc(id)}" tabindex="0">
        <div class="row-top">
          <cv-harness-badge harness="${esc((s.harness || "").toLowerCase())}"></cv-harness-badge>
          <span class="row-title">${esc(sessionLabel(s))}</span>
        </div>
        <div class="row-meta muted">
          ${cwd}
          ${when ? `<span class="row-when">${esc(when)}</span>` : ""}
          <span class="row-count">${count} msg${count === 1 ? "" : "s"}</span>
        </div>
      </li>`;
  }

  _wireRows() {
    this.querySelectorAll(".session-row").forEach((li) => {
      const fire = () => this._selectById(li.dataset.id);
      li.addEventListener("click", fire);
      li.addEventListener("keydown", (e) => {
        if (e.key === "Enter" || e.key === " ") { e.preventDefault(); fire(); }
      });
    });
  }

  _selectById(id) {
    const session = this._sessions.find((s) => (s.id || "") === id);
    if (!session) return;
    this._selectedId = id;
    this._updateSelection();
    this.dispatchEvent(new CustomEvent("select", { detail: { session }, bubbles: true }));
  }

  _updateSelection() {
    this.querySelectorAll(".session-row").forEach((li) => {
      const on = li.dataset.id === this._selectedId;
      li.classList.toggle("selected", on);
      li.setAttribute("aria-selected", String(on));
    });
  }
}

customElements.define("cv-session-list", CvSessionList);
