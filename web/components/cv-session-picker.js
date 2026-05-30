// <cv-session-picker> — a searchable, scannable replacement for a native session <select>.
//
// Picking one of thousands of sessions from a flat dropdown is hopeless: no search, no harness, no
// project, no time. This is a command-palette-style picker — a trigger that shows the current choice
// (harness badge + title), opening a filterable popover where each row carries badge · title ·
// project · when · message count, with full keyboard nav.
//
// API:
//   picker.sessions = Session[]
//   picker.value = id                 // current selection (id)
//   picker.placeholder = "Pick a session…"
//   emits "pick" CustomEvent { detail: { id, session } }
import { esc, sessionLabel, shortPath, fmtTime, msgCount, harnessBadge, HARNESS_LABELS } from "./util.js";

// Only one picker may be open at a time (two side-by-side pickers in Compare were overlapping).
let OPEN_PICKER = null;

// Render at most this many rows at once — it's a search box, so showing the most-recent N (and a
// labeled "…N more, keep typing" footer) keeps opening instant even with thousands of sessions.
const RENDER_CAP = 60;

class CvSessionPicker extends HTMLElement {
  constructor() {
    super();
    this._sessions = [];
    this._value = null;
    this._placeholder = "Pick a session…";
    this._open = false;
    this._query = "";
    this._active = 0;
    this._filtered = [];
    this._onDocDown = (e) => {
      if (!this.contains(e.target)) this.close();
    };
  }

  set sessions(arr) {
    this._sessions = Array.isArray(arr) ? arr : [];
    this._renderTrigger();
  }
  get sessions() {
    return this._sessions;
  }

  set value(id) {
    this._value = id ?? null;
    this._renderTrigger();
  }
  get value() {
    return this._value;
  }

  set placeholder(p) {
    this._placeholder = p || "Pick a session…";
    this._renderTrigger();
  }

  connectedCallback() {
    if (!this._built) {
      this.innerHTML = `
        <button type="button" class="sp-trigger" aria-haspopup="listbox" aria-expanded="false"></button>
        <div class="sp-pop" hidden>
          <input type="search" class="sp-search" placeholder="Search title, project, harness…" aria-label="Search sessions" />
          <div class="sp-list" role="listbox"></div>
        </div>`;
      this._trigger = this.querySelector(".sp-trigger");
      this._pop = this.querySelector(".sp-pop");
      this._search = this.querySelector(".sp-search");
      this._list = this.querySelector(".sp-list");

      this._trigger.addEventListener("click", () => (this._open ? this.close() : this.openPopover()));
      this._search.addEventListener("input", () => {
        this._query = this._search.value;
        this._active = 0;
        this._renderList();
      });
      this._search.addEventListener("keydown", (e) => this._onKey(e));
      this._list.addEventListener("click", (e) => {
        const row = e.target.closest(".sp-entry");
        if (row) this._choose(row.dataset.id);
      });
      this._list.addEventListener("mousemove", (e) => {
        const row = e.target.closest(".sp-entry");
        if (row && row.dataset.idx != null) {
          const i = +row.dataset.idx;
          if (i !== this._active) {
            this._active = i;
            this._paintActive();
          }
        }
      });
      this._built = true;
    }
    this._renderTrigger();
  }

  disconnectedCallback() {
    document.removeEventListener("pointerdown", this._onDocDown, true);
    if (OPEN_PICKER === this) OPEN_PICKER = null;
  }

  _current() {
    return this._sessions.find((s) => (s.id || "") === this._value) || null;
  }

  _renderTrigger() {
    if (!this._trigger) return;
    const s = this._current();
    if (s) {
      const h = (s.harness || "").toLowerCase();
      this._trigger.innerHTML = `
        ${harnessBadge(h)}
        <span class="sp-trigger-title">${esc(sessionLabel(s))}</span>
        <span class="sp-caret" aria-hidden="true">▾</span>`;
      this._trigger.classList.remove("sp-empty");
    } else {
      this._trigger.innerHTML = `<span class="sp-trigger-title muted">${esc(this._placeholder)}</span><span class="sp-caret" aria-hidden="true">▾</span>`;
      this._trigger.classList.add("sp-empty");
    }
  }

  openPopover() {
    if (OPEN_PICKER && OPEN_PICKER !== this) OPEN_PICKER.close();
    OPEN_PICKER = this;
    this._open = true;
    this._pop.hidden = false;
    this._trigger.setAttribute("aria-expanded", "true");
    this._query = "";
    this._search.value = "";
    // Start the active row on the current selection, if any.
    this._renderList();
    const cur = this._filtered.findIndex((s) => (s.id || "") === this._value);
    this._active = cur >= 0 ? cur : 0;
    this._paintActive(true);
    document.addEventListener("pointerdown", this._onDocDown, true);
    requestAnimationFrame(() => this._search.focus());
  }

  close() {
    if (OPEN_PICKER === this) OPEN_PICKER = null;
    if (!this._open) return;
    this._open = false;
    this._pop.hidden = true;
    this._trigger.setAttribute("aria-expanded", "false");
    document.removeEventListener("pointerdown", this._onDocDown, true);
  }

  _match(s) {
    const q = this._query.trim().toLowerCase();
    if (!q) return true;
    const hay = `${s.title || ""} ${sessionLabel(s)} ${s.cwd || ""} ${s.harness || ""} ${HARNESS_LABELS[(s.harness || "").toLowerCase()] || ""}`.toLowerCase();
    // AND over whitespace-separated terms so "claude payments" narrows.
    return q.split(/\s+/).every((term) => hay.includes(term));
  }

  _renderList() {
    const all = this._sessions.filter((s) => this._match(s));
    if (!all.length) {
      this._filtered = [];
      this._list.innerHTML = `<div class="sp-empty-row muted">No sessions match “${esc(this._query)}”.</div>`;
      return;
    }
    // Only render a capped window; the rest is reachable by typing to narrow.
    this._filtered = all.slice(0, RENDER_CAP);
    const overflow = all.length - this._filtered.length;
    const more = overflow > 0
      ? `<div class="sp-more muted">…and ${overflow.toLocaleString()} more — keep typing to narrow</div>`
      : "";
    this._list.innerHTML = this._filtered
      .map((s, i) => {
        const h = (s.harness || "").toLowerCase();
        const when = fmtTime(s.updated_at || s.created_at);
        const n = msgCount(s);
        const proj = s.cwd ? esc(shortPath(s.cwd, 2)) : "";
        const cur = (s.id || "") === this._value ? " sp-current" : "";
        return `
          <div class="sp-entry${cur}" role="option" data-id="${esc(s.id || "")}" data-idx="${i}" aria-selected="${(s.id || "") === this._value}">
            ${harnessBadge(h)}
            <span class="sp-entry-main">
              <span class="sp-entry-title">${esc(sessionLabel(s))}</span>
              <span class="sp-entry-meta muted">${proj ? `${proj} · ` : ""}${esc(when)} · ${n} msg${n === 1 ? "" : "s"}</span>
            </span>
          </div>`;
      })
      .join("") + more;
    this._paintActive();
  }

  _paintActive(scroll = false) {
    const rows = this._list.querySelectorAll(".sp-entry");
    rows.forEach((r, i) => r.classList.toggle("sp-active", i === this._active));
    if (scroll && rows[this._active]) rows[this._active].scrollIntoView({ block: "nearest" });
  }

  _onKey(e) {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      this._active = Math.min(this._filtered.length - 1, this._active + 1);
      this._paintActive(true);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      this._active = Math.max(0, this._active - 1);
      this._paintActive(true);
    } else if (e.key === "Enter") {
      e.preventDefault();
      const s = this._filtered[this._active];
      if (s) this._choose(s.id || "");
    } else if (e.key === "Escape") {
      e.preventDefault();
      this.close();
      this._trigger.focus();
    }
  }

  _choose(id) {
    this._value = id;
    this._renderTrigger();
    this.close();
    const session = this._current();
    this.dispatchEvent(new CustomEvent("pick", { detail: { id, session }, bubbles: true }));
  }
}

customElements.define("cv-session-picker", CvSessionPicker);
