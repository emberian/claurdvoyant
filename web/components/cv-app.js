// <cv-app> — top-level shell: header, multi-source dropzone/upload, a view
// switcher (tabs), and the active view. Sessions from every dropped source are
// merged into one pool; each view reads from that pool.
//
// Views:
//   sessions   — the classic two-pane list + transcript
//   timeline   — chronological cross-harness feed (<cv-timeline>)
//   compare    — side-by-side message diff (<cv-compare>)
//   stats      — dashboard (<cv-stats>)
//   loom       — splice/loom composer (<cv-loom>)
//   fleet      — live cvd serve dashboard (<cv-fleet>)
//   opensession— the OpenSession standard (<cv-opensession>)
import "./cv-session-list.js";
import "./cv-transcript.js";
import "./cv-timeline.js";
import "./cv-compare.js";
import "./cv-stats.js";
import "./cv-loom.js";
import "./cv-fleet.js";
import "./cv-opensession.js";
import { esc, normalizeSessions, randomId } from "./util.js";
import { isTauri, listen } from "../tauri.js";

const VIEWS = [
  ["sessions", "Sessions", "🗂"],
  ["timeline", "Timeline", "📈"],
  ["compare", "Compare", "🔍"],
  ["stats", "Stats", "📊"],
  ["loom", "Loom", "✨"],
  ["fleet", "Fleet", "📡"],
  ["opensession", "OpenSession", "🧬"],
];

class CvApp extends HTMLElement {
  constructor() {
    super();
    this._sessions = [];
    this._sources = [];        // [{ name, count }]
    this._wasmState = "loading";
    this._view = "sessions";
    this._isSample = true;     // true until the user loads their own data
  }

  connectedCallback() {
    this.render();
    this._init();
    this._wireKeyboard();
    this._wireTauri();
  }

  disconnectedCallback() {
    if (this._onKeydown) document.removeEventListener("keydown", this._onKeydown);
    this._unlistenTauri?.();
  }

  async _init() {
    const main = await import("../main.js");
    this._ingestZip = main.ingestZip;
    this._loadSample = main.loadSample;

    this._wasmState = await main.wasmReady();
    this._updateStatus();

    try {
      const sample = await main.loadSample();
      if (!this._sessions.length) {
        this._sessions = normalizeSessions(sample);
        this._sources = [{ name: "sample", count: this._sessions.length }];
        this._isSample = true;
        this._refreshViews();
        this._setStatus(isTauri()
          ? "Showing the bundled sample — use File → Open in the desktop app, or drop .zip / .json files, to load your own."
          : "Showing the bundled sample dataset — drop one or more .zip / .json files to load your own.");
      }
    } catch (e) {
      console.warn("[claurdvoyant] failed to load sample:", e);
    }
  }

  // ---- Tauri: native File→Open pushes sessions via the cv://open-sessions event
  async _wireTauri() {
    if (!isTauri()) return;
    this._unlistenTauri = await listen("cv://open-sessions", (payload) => {
      try {
        const sessions = normalizeSessions(payload);
        if (!sessions.length) { this._setStatus("File → Open: no sessions in that payload.", "warn"); return; }
        if (this._isSample) { this._sessions = []; this._sources = []; this._isSample = false; }
        this._sessions = this._mergePool(this._sessions, sessions);
        this._sources.push({ name: "File → Open", count: sessions.length });
        this._renderSources();
        this._refreshViews();
        if (this._list && this._sessions[0] && !this._list.selectedId) {
          this._transcript.session = this._sessions[0];
          this._list.selectedId = this._sessions[0].id;
        }
        this._setStatus(`Loaded ${sessions.length} session${sessions.length === 1 ? "" : "s"} from File → Open.`, "ok");
      } catch (err) {
        console.error("[claurdvoyant] cv://open-sessions failed:", err);
        this._setStatus("Could not load the opened sessions.", "error");
      }
    });
  }

  render() {
    this.innerHTML = `
      <header class="app-header">
        <div class="brand">
          <span class="logo" aria-hidden="true">🔮</span>
          <div class="brand-text">
            <h1>claurdvoyant</h1>
            <p class="tagline">Browse, splice &amp; loom agent sessions — entirely in your browser.</p>
          </div>
        </div>
        <div class="header-actions">
          <button type="button" class="icon-btn help-btn" title="Keyboard shortcuts (?)" aria-label="Keyboard shortcuts">?</button>
          <button type="button" class="icon-btn theme-toggle" title="Toggle theme — dark / light / auto (t)" aria-label="Toggle theme">◐</button>
          <a class="repo-link" href="https://github.com/" target="_blank" rel="noopener" title="Project repository">source</a>
        </div>
      </header>

      <div class="dropzone" tabindex="0" role="button" aria-label="Upload or drop .zip / .json session files">
        <input type="file" class="file-input" accept=".zip,.json,application/zip,application/json" multiple hidden />
        <div class="dz-inner">
          <span class="dz-glyph" aria-hidden="true">📦</span>
          <div class="dz-text">
            <strong>Drop <code>.zip</code> or <code>.json</code> files here</strong>
            <span class="muted">multiple at once — harness <code>.zip</code>s are parsed by WASM; an OpenSession <code>.json</code> loads directly. Everything merges into one pool.</span>
          </div>
          <div class="dz-sources" aria-live="polite"></div>
        </div>
        <div class="dz-status muted" aria-live="polite"></div>
      </div>

      <nav class="view-tabs" role="tablist" aria-label="Views">
        ${VIEWS.map(([id, label, glyph]) => `
          <button type="button" class="view-tab${id === this._view ? " on" : ""}" role="tab"
            aria-selected="${id === this._view}" data-view="${id}">
            <span class="vt-glyph" aria-hidden="true">${glyph}</span>${esc(label)}
          </button>`).join("")}
      </nav>

      <main class="view-host"></main>

      <footer class="app-footer muted">
        <span>All parsing happens locally; nothing is uploaded. ·
        <a class="repo-link" href="#" data-goto="opensession">About OpenSession</a></span>
      </footer>
    `;

    this._dropzone = this.querySelector(".dropzone");
    this._fileInput = this.querySelector(".file-input");
    this._status = this.querySelector(".dz-status");
    this._sourcesEl = this.querySelector(".dz-sources");
    this._host = this.querySelector(".view-host");

    this.querySelectorAll(".view-tab").forEach((tab) =>
      tab.addEventListener("click", () => this._setView(tab.dataset.view)));
    this.querySelector("[data-goto]")?.addEventListener("click", (e) => {
      e.preventDefault(); this._setView("opensession");
    });

    this._wireDropzone();
    this._wireTheme();
    this.querySelector(".help-btn")?.addEventListener("click", () => this._toggleHelp());
    this._renderView();
    this._renderSources();
  }

  // ---- view switching ----------------------------------------------------

  _setView(view) {
    if (!VIEWS.some((v) => v[0] === view)) return;
    this._view = view;
    this.querySelectorAll(".view-tab").forEach((t) => {
      const on = t.dataset.view === view;
      t.classList.toggle("on", on);
      t.setAttribute("aria-selected", String(on));
    });
    this._renderView();
  }

  _renderView() {
    const host = this._host;
    host.className = "view-host view-" + this._view;
    switch (this._view) {
      case "sessions": host.innerHTML = ""; host.appendChild(this._sessionsView()); break;
      case "timeline": host.innerHTML = `<cv-timeline></cv-timeline>`; break;
      case "compare": host.innerHTML = `<cv-compare></cv-compare>`; break;
      case "stats": host.innerHTML = `<cv-stats></cv-stats>`; break;
      case "loom": host.innerHTML = `<cv-loom></cv-loom>`; break;
      case "fleet": host.innerHTML = `<cv-fleet></cv-fleet>`; break;
      case "opensession": host.innerHTML = `<cv-opensession></cv-opensession>`; break;
    }
    this._refreshViews();
  }

  // The classic two-pane sessions view, built once and cached.
  _sessionsView() {
    if (!this._sessionsLayout) {
      const layout = document.createElement("div");
      layout.className = "layout";
      layout.innerHTML = `
        <aside class="pane pane-list" aria-label="Sessions">
          <cv-session-list></cv-session-list>
        </aside>
        <section class="pane pane-transcript" aria-label="Transcript">
          <button type="button" class="back-btn" aria-label="Back to session list">← sessions</button>
          <cv-transcript></cv-transcript>
        </section>`;
      this._sessionsLayout = layout;
      this._list = layout.querySelector("cv-session-list");
      this._transcript = layout.querySelector("cv-transcript");

      this._list.addEventListener("select", (e) => {
        this._transcript.session = e.detail.session;
        layout.classList.add("show-transcript");
        layout.querySelector(".pane-transcript")?.scrollTo?.(0, 0);
      });
      layout.querySelector(".back-btn").addEventListener("click", () => layout.classList.remove("show-transcript"));
    }
    return this._sessionsLayout;
  }

  // Push the current pool into whichever view is mounted.
  _refreshViews() {
    if (this._list) {
      this._list.sessions = this._sessions;
      if (!this._list.selectedId && this._sessions[0]) {
        this._transcript.session = this._sessions[0];
        this._list.selectedId = this._sessions[0].id;
      }
    }
    const t = this._host?.querySelector("cv-timeline"); if (t) t.sessions = this._sessions;
    const c = this._host?.querySelector("cv-compare"); if (c) c.sessions = this._sessions;
    const st = this._host?.querySelector("cv-stats"); if (st) st.sessions = this._sessions;
    const lo = this._host?.querySelector("cv-loom"); if (lo) lo.sessions = this._sessions;

    // Timeline → open in the sessions view.
    const tl = this._host?.querySelector("cv-timeline");
    if (tl && !tl._wired) {
      tl._wired = true;
      tl.addEventListener("open", (e) => this._openInSessions(e.detail.session));
    }
  }

  _openInSessions(session) {
    this._setView("sessions");
    const v = this._sessionsView();
    this._transcript.session = session;
    this._list.selectedId = session.id;
    v.classList.add("show-transcript");
  }

  // ---- sources / theme / dropzone ---------------------------------------

  _renderSources() {
    if (!this._sourcesEl) return;
    if (!this._sources.length) { this._sourcesEl.innerHTML = ""; return; }
    this._sourcesEl.innerHTML = this._sources.map((s) =>
      `<span class="src-chip" title="${esc(s.name)}">${esc(s.name)} <b>${s.count}</b></span>`).join("");
  }

  _wireTheme() {
    const root = document.documentElement;
    const stored = localStorage.getItem("cv-theme");
    if (stored) root.setAttribute("data-theme", stored);
    this.querySelector(".theme-toggle").addEventListener("click", () => {
      const cur = root.getAttribute("data-theme");
      const next = cur === "dark" ? "light" : cur === "light" ? "auto" : "dark";
      root.setAttribute("data-theme", next);
      localStorage.setItem("cv-theme", next);
    });
  }

  // ---- keyboard shortcuts + help overlay --------------------------------

  _wireKeyboard() {
    this._onKeydown = (e) => {
      // Don't hijack typing in inputs/textareas/contenteditable/selects.
      const t = e.target;
      const typing = t && (t.isContentEditable ||
        /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName));

      if (e.key === "Escape") {
        if (this._helpOpen) { this._toggleHelp(false); e.preventDefault(); return; }
        // In the narrow sessions layout, Esc returns to the list.
        const layout = this._sessionsLayout;
        if (this._view === "sessions" && layout?.classList.contains("show-transcript")) {
          layout.classList.remove("show-transcript"); e.preventDefault(); return;
        }
        if (typing) t.blur();
        return;
      }

      if (typing) return;
      if (e.metaKey || e.ctrlKey || e.altKey) return;

      // "?" (Shift+/) toggles help.
      if (e.key === "?") { this._toggleHelp(); e.preventDefault(); return; }
      // "/" focuses the session search (switching to sessions first).
      if (e.key === "/") {
        if (this._view !== "sessions") this._setView("sessions");
        const search = this._list?.querySelector?.(".search");
        if (search) { search.focus(); search.select?.(); e.preventDefault(); }
        return;
      }
      // "t" cycles theme.
      if (e.key === "t") { this.querySelector(".theme-toggle")?.click(); e.preventDefault(); return; }
      // Number keys 1..N switch tabs.
      if (/^[1-9]$/.test(e.key)) {
        const idx = Number(e.key) - 1;
        if (VIEWS[idx]) { this._setView(VIEWS[idx][0]); e.preventDefault(); }
        return;
      }
      // j/k + arrows: move selection within the session list.
      if (this._view === "sessions" && ["j", "k", "ArrowDown", "ArrowUp"].includes(e.key)) {
        const dir = (e.key === "j" || e.key === "ArrowDown") ? 1 : -1;
        if (this._list?.moveSelection?.(dir)) e.preventDefault();
        return;
      }
    };
    document.addEventListener("keydown", this._onKeydown);
  }

  _toggleHelp(force) {
    this._helpOpen = force === undefined ? !this._helpOpen : !!force;
    let overlay = this.querySelector(".help-overlay");
    if (this._helpOpen && !overlay) {
      overlay = document.createElement("div");
      overlay.className = "help-overlay";
      overlay.setAttribute("role", "dialog");
      overlay.setAttribute("aria-modal", "true");
      overlay.setAttribute("aria-label", "Keyboard shortcuts");
      const rows = [
        ["1 – 7", "Switch view (Sessions, Timeline, Compare, …)"],
        ["/", "Focus session search"],
        ["j / k  ·  ↓ / ↑", "Move selection in the session list"],
        ["Enter", "Open the focused session"],
        ["t", "Cycle theme (dark → light → auto)"],
        ["Esc", "Close this help · back to the session list"],
        ["?", "Toggle this help"],
      ];
      overlay.innerHTML = `
        <div class="help-card">
          <div class="help-head"><h2>Keyboard shortcuts</h2>
            <button type="button" class="mini-btn help-close" aria-label="Close">esc ✕</button></div>
          <table class="help-table">${rows.map(([k, d]) =>
            `<tr><td class="help-key">${k.split(/\s+/).map((p) => /^[·–]$/.test(p) ? esc(p) : `<kbd>${esc(p)}</kbd>`).join(" ")}</td><td>${esc(d)}</td></tr>`).join("")}</table>
          <p class="help-foot muted">claurdvoyant runs entirely in your browser — nothing is uploaded.</p>
        </div>`;
      overlay.addEventListener("click", (e) => { if (e.target === overlay) this._toggleHelp(false); });
      overlay.querySelector(".help-close").addEventListener("click", () => this._toggleHelp(false));
      this.appendChild(overlay);
      overlay.querySelector(".help-close").focus();
    } else if (!this._helpOpen && overlay) {
      overlay.remove();
    }
  }

  _wireDropzone() {
    const dz = this._dropzone;
    const open = () => this._fileInput.click();
    dz.addEventListener("click", (e) => { if (e.target.tagName !== "INPUT") open(); });
    dz.addEventListener("keydown", (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); open(); } });
    this._fileInput.addEventListener("change", () => {
      const files = [...(this._fileInput.files || [])];
      if (files.length) this._handleFiles(files);
      this._fileInput.value = "";
    });

    ["dragenter", "dragover"].forEach((ev) =>
      dz.addEventListener(ev, (e) => { e.preventDefault(); dz.classList.add("drag"); }));
    ["dragleave", "drop"].forEach((ev) =>
      dz.addEventListener(ev, (e) => { e.preventDefault(); if (ev === "dragleave" && dz.contains(e.relatedTarget)) return; dz.classList.remove("drag"); }));
    dz.addEventListener("drop", (e) => {
      const files = [...(e.dataTransfer?.files || [])];
      if (files.length) this._handleFiles(files);
    });
  }

  async _handleFiles(files) {
    // Starting fresh: clear the sample dataset on the first real load.
    if (this._isSample) { this._sessions = []; this._sources = []; this._isSample = false; }

    let added = 0, failed = 0;
    for (const file of files) {
      this._setStatus(`Reading ${file.name}…`);
      try {
        const got = await this._ingestOne(file);
        if (got.length) {
          this._sessions = this._mergePool(this._sessions, got);
          this._sources.push({ name: file.name, count: got.length });
          added += got.length;
        } else {
          this._setStatus(`No sessions found in ${file.name}.`, "warn");
        }
      } catch (err) {
        console.error(err);
        failed++;
        this._setStatus(err?.message || `Failed to read ${file.name}.`, "error");
      }
    }

    this._renderSources();
    this._refreshViews();
    if (this._list && this._sessions[0] && !this._list.selectedId) {
      this._transcript.session = this._sessions[0];
      this._list.selectedId = this._sessions[0].id;
    }
    if (added) {
      this._setStatus(`Pool now has ${this._sessions.length} session${this._sessions.length === 1 ? "" : "s"} from ${this._sources.length} source${this._sources.length === 1 ? "" : "s"}.`, failed ? "warn" : "ok");
    }
  }

  async _ingestOne(file) {
    const name = file.name || "file";
    if (/\.json$/i.test(name) || file.type === "application/json") {
      const text = await file.text();
      const data = JSON.parse(text);
      return normalizeSessions(data);
    }
    if (/\.zip$/i.test(name) || file.type === "application/zip") {
      const buf = new Uint8Array(await file.arrayBuffer());
      const raw = await this._ingestZip(buf);
      return normalizeSessions(raw);
    }
    throw new Error(`"${name}" isn't a .zip or .json file.`);
  }

  // Merge new sessions into the pool, de-duplicating by id (last wins is fine;
  // we keep the first occurrence to preserve source ordering, but give a fresh
  // id to any collision so nothing is silently dropped).
  _mergePool(pool, incoming) {
    const seen = new Set(pool.map((s) => s.id));
    const out = pool.slice();
    for (const s of incoming) {
      if (s.id && seen.has(s.id)) s.id = `${s.id}~${randomId().slice(0, 4)}`;
      seen.add(s.id);
      out.push(s);
    }
    return out;
  }

  _setStatus(msg, kind = "") {
    if (!this._status) return;
    this._status.textContent = msg;
    this._status.className = "dz-status muted" + (kind ? " " + kind : "");
  }

  _updateStatus() {
    if (this._wasmState === "missing") {
      this._setStatus(
        "Demo mode: the WASM parser isn't in this build, so .zip ingest is disabled. You can still drop OpenSession .json files and explore the sample.",
        "warn"
      );
    }
  }
}

customElements.define("cv-app", CvApp);
