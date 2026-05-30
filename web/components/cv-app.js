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
//   opensession— the OpenSession standard (<cv-opensession>)
import "./cv-session-list.js";
import "./cv-transcript.js";
import "./cv-timeline.js";
import "./cv-compare.js";
import "./cv-stats.js";
import "./cv-loom.js";
import "./cv-opensession.js";
import { esc, normalizeSessions, randomId } from "./util.js";

const VIEWS = [
  ["sessions", "Sessions", "🗂"],
  ["timeline", "Timeline", "📈"],
  ["compare", "Compare", "🔍"],
  ["stats", "Stats", "📊"],
  ["loom", "Loom", "✨"],
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
        this._setStatus("Showing the bundled sample dataset — drop one or more .zip / .json files to load your own.");
      }
    } catch (e) {
      console.warn("[claurdvoyant] failed to load sample:", e);
    }
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
          <button type="button" class="theme-toggle" title="Toggle light / dark" aria-label="Toggle theme">◐</button>
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
