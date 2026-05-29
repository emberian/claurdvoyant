// <cv-app> — top-level shell: header, dropzone/upload, two-pane layout
// (session list + transcript). Wires the wasm ingest / sample fallback.
import "./cv-session-list.js";
import "./cv-transcript.js";
import { esc } from "./util.js";

class CvApp extends HTMLElement {
  constructor() {
    super();
    this._sessions = [];
    this._wasmState = "loading"; // loading | ready | missing
  }

  connectedCallback() {
    this.render();
    this._init();
  }

  async _init() {
    const main = await import("../main.js");
    this._ingestZip = main.ingestZip;
    this._loadSample = main.loadSample;

    // Resolve wasm availability and show appropriate hint.
    this._wasmState = await main.wasmReady();
    this._updateStatus();

    // Auto-load the sample dataset so the page is never empty.
    try {
      const sample = await main.loadSample();
      if (!this._sessions.length) this._setSessions(sample, { source: "sample" });
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
            <p class="tagline">Browse agent sessions from a dropped <code>.zip</code> — entirely in your browser.</p>
          </div>
        </div>
        <div class="header-actions">
          <button type="button" class="theme-toggle" title="Toggle light / dark" aria-label="Toggle theme">◐</button>
          <a class="repo-link" href="https://github.com/" target="_blank" rel="noopener" title="Project repository">source</a>
        </div>
      </header>

      <div class="dropzone" tabindex="0" role="button" aria-label="Upload or drop a .zip of an agent harness directory">
        <input type="file" class="file-input" accept=".zip,application/zip" hidden />
        <div class="dz-inner">
          <span class="dz-glyph" aria-hidden="true">📦</span>
          <div class="dz-text">
            <strong>Drop a <code>.zip</code> here</strong>
            <span class="muted">or click to choose — a <code>.claude/projects/…</code>, <code>.codex/sessions/…</code>, OpenCode <code>storage/…</code> tree, etc.</span>
          </div>
        </div>
        <div class="dz-status muted" aria-live="polite"></div>
      </div>

      <main class="layout">
        <aside class="pane pane-list" aria-label="Sessions">
          <cv-session-list></cv-session-list>
        </aside>
        <section class="pane pane-transcript" aria-label="Transcript">
          <button type="button" class="back-btn" aria-label="Back to session list">← sessions</button>
          <cv-transcript></cv-transcript>
        </section>
      </main>

      <footer class="app-footer muted">
        <span>All parsing happens locally; nothing is uploaded.</span>
      </footer>
    `;

    this._list = this.querySelector("cv-session-list");
    this._transcript = this.querySelector("cv-transcript");
    this._dropzone = this.querySelector(".dropzone");
    this._fileInput = this.querySelector(".file-input");
    this._status = this.querySelector(".dz-status");

    this._list.addEventListener("select", (e) => {
      this._transcript.session = e.detail.session;
      // On narrow screens, reveal the transcript pane.
      this.classList.add("show-transcript");
      this.querySelector(".pane-transcript")?.scrollTo?.(0, 0);
    });

    this.querySelector(".back-btn").addEventListener("click", () => {
      this.classList.remove("show-transcript");
    });

    this._wireDropzone();
    this._wireTheme();
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
    dz.addEventListener("click", (e) => {
      if (e.target.tagName !== "INPUT") open();
    });
    dz.addEventListener("keydown", (e) => {
      if (e.key === "Enter" || e.key === " ") { e.preventDefault(); open(); }
    });
    this._fileInput.addEventListener("change", () => {
      const f = this._fileInput.files?.[0];
      if (f) this._handleFile(f);
      this._fileInput.value = "";
    });

    ["dragenter", "dragover"].forEach((ev) =>
      dz.addEventListener(ev, (e) => { e.preventDefault(); dz.classList.add("drag"); }));
    ["dragleave", "drop"].forEach((ev) =>
      dz.addEventListener(ev, (e) => { e.preventDefault(); if (ev === "dragleave" && dz.contains(e.relatedTarget)) return; dz.classList.remove("drag"); }));
    dz.addEventListener("drop", (e) => {
      const f = e.dataTransfer?.files?.[0];
      if (f) this._handleFile(f);
    });
  }

  async _handleFile(file) {
    const name = file.name || "file";
    if (!/\.zip$/i.test(name) && file.type !== "application/zip") {
      this._setStatus(`"${name}" doesn't look like a .zip.`, "warn");
      return;
    }
    this._setStatus(`Reading ${name}…`);
    try {
      const buf = new Uint8Array(await file.arrayBuffer());
      const sessions = await this._ingestZip(buf);
      if (!sessions.length) {
        this._setStatus(`No sessions found in ${name}.`, "warn");
        return;
      }
      this._setSessions(sessions, { source: name });
      this._setStatus(`Loaded ${sessions.length} session${sessions.length === 1 ? "" : "s"} from ${name}.`, "ok");
    } catch (err) {
      console.error(err);
      this._setStatus(err?.message || "Failed to ingest the .zip.", "error");
    }
  }

  _setSessions(sessions, { source } = {}) {
    this._sessions = sessions;
    this._list.sessions = sessions;
    // Auto-select the first session for immediate context.
    const first = sessions[0];
    if (first) {
      this._transcript.session = first;
      this._list.selectedId = first.id;
    } else {
      this._transcript.session = null;
    }
    if (source === "sample") {
      this._setStatus("Showing the bundled sample dataset — drop a .zip to load your own.");
    }
  }

  _setStatus(msg, kind = "") {
    if (!this._status) return;
    this._status.textContent = msg;
    this._status.className = "dz-status muted" + (kind ? " " + kind : "");
  }

  _updateStatus() {
    if (this._wasmState === "missing") {
      this._setStatus(
        "Demo mode: the WASM parser isn't in this build, so .zip ingest is disabled. Exploring the sample dataset.",
        "warn"
      );
    }
  }
}

customElements.define("cv-app", CvApp);
