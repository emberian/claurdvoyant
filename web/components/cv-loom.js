// <cv-loom> — the splice / loom composer. ✨ The headline.
//
// A three-pane workspace:
//   1. Source picker  — choose any loaded session; its transcript renders with
//      "＋ loom" buttons on each message (pickMode on <cv-transcript>).
//   2. Composition lane — the messages you've collected, in order. Reorder
//      (▲ ▼ or drag), remove (✕), and "fork from here" (drop everything after).
//   3. Live preview     — the composed transcript rendered as it will export.
//
// Export: download the lane as an OpenSession JSON document — pure client-side,
// no wasm. You're just rearranging already-parsed message objects.
//
// API: loom.sessions = Session[]   (setter)
import "./cv-transcript.js";
import "./cv-harness-badge.js";
import {
  esc, sessionLabel, ROLE_LABELS, toOpenSession, toMarkdown,
  downloadFile, slug, randomId,
} from "./util.js";

class CvLoom extends HTMLElement {
  constructor() {
    super();
    this._sessions = [];
    this._sourceId = null;
    // Composition lane: array of { uid, message, fromSession } — message is a
    // (deep-ish) clone so edits never mutate the source pool.
    this._lane = [];
    this._title = "Spliced session";
    this._dragUid = null;
  }

  set sessions(arr) {
    this._sessions = Array.isArray(arr) ? arr : [];
    if (!this._sessions.find((s) => (s.id || "") === this._sourceId)) {
      this._sourceId = this._sessions[0]?.id ?? null;
    }
    this.render();
  }
  get sessions() { return this._sessions; }

  connectedCallback() { this.render(); }

  _source() { return this._sessions.find((s) => (s.id || "") === this._sourceId) || null; }

  _composed() {
    return {
      openSession: "0.1",
      harness: "opensession",
      id: this._composedId || (this._composedId = randomId()),
      title: this._title,
      messages: this._lane.map((e) => e.message),
    };
  }

  render() {
    if (!this._sessions.length) {
      this.innerHTML = `<div class="view-empty muted"><p>Load sessions, then splice their messages here.</p></div>`;
      return;
    }
    const srcOpts = this._sessions.map((s) =>
      `<option value="${esc(s.id || "")}"${(s.id || "") === this._sourceId ? " selected" : ""}>${esc(sessionLabel(s))}</option>`).join("");

    this.innerHTML = `
      <div class="view-head">
        <h2>✨ Loom</h2>
        <span class="muted">Select messages from any session and weave them into a new composition.</span>
      </div>
      <div class="loom-grid">
        <section class="loom-source">
          <div class="loom-pane-head">
            <strong>Source</strong>
            <select class="loom-source-pick" aria-label="Source session">${srcOpts}</select>
          </div>
          <div class="loom-source-body">
            <cv-transcript class="loom-transcript"></cv-transcript>
          </div>
        </section>

        <section class="loom-lane">
          <div class="loom-pane-head">
            <strong>Composition</strong>
            <span class="muted">${this._lane.length} message${this._lane.length === 1 ? "" : "s"}</span>
          </div>
          <div class="loom-title-row">
            <input class="loom-title" type="text" value="${esc(this._title)}" aria-label="Composition title" placeholder="Composition title" />
          </div>
          <ol class="lane-list">${this._laneHtml()}</ol>
          <div class="loom-actions">
            <button type="button" class="mini-btn" data-clear ${this._lane.length ? "" : "disabled"}>clear</button>
            <button type="button" class="btn-accent" data-export-json ${this._lane.length ? "" : "disabled"}>⬇ OpenSession .json</button>
            <button type="button" class="mini-btn" data-export-md ${this._lane.length ? "" : "disabled"}>⬇ .md</button>
          </div>
        </section>

        <section class="loom-preview">
          <div class="loom-pane-head"><strong>Live preview</strong></div>
          <div class="loom-preview-body"><cv-transcript class="loom-preview-transcript"></cv-transcript></div>
        </section>
      </div>
    `;

    // Source transcript in pick mode.
    const srcT = this.querySelector(".loom-transcript");
    srcT.pickMode = true;
    srcT.session = this._source();
    srcT.addEventListener("pick-message", (e) => this._addMessage(e.detail.session, e.detail.message));

    // Preview transcript.
    this._renderPreview();

    // Wire controls.
    this.querySelector(".loom-source-pick").addEventListener("change", (e) => {
      this._sourceId = e.target.value;
      this.querySelector(".loom-transcript").session = this._source();
    });
    this.querySelector(".loom-title").addEventListener("input", (e) => {
      this._title = e.target.value;
      this._renderPreview();
    });
    this.querySelector("[data-clear]")?.addEventListener("click", () => { this._lane = []; this.render(); });
    this.querySelector("[data-export-json]")?.addEventListener("click", () => {
      downloadFile(`${slug(this._title)}.opensession.json`,
        JSON.stringify(toOpenSession(this._composed()), null, 2), "application/json");
    });
    this.querySelector("[data-export-md]")?.addEventListener("click", () => {
      downloadFile(`${slug(this._title)}.md`, toMarkdown(this._composed()), "text/markdown");
    });

    this._wireLane();
  }

  _renderPreview() {
    const p = this.querySelector(".loom-preview-transcript");
    if (p) p.session = this._lane.length ? this._composed() : null;
  }

  _addMessage(session, message) {
    // Clone so reordering/removal never mutates the source pool.
    const clone = JSON.parse(JSON.stringify(message));
    this._lane.push({ uid: randomId(), message: clone, from: sessionLabel(session), harness: (session.harness || "").toLowerCase() });
    this.render();
    // Keep the lane scrolled to the newest entry.
    this.querySelector(".lane-list")?.lastElementChild?.scrollIntoView({ block: "nearest" });
  }

  _laneHtml() {
    if (!this._lane.length) {
      return `<li class="lane-empty muted">Empty — click <b>＋ loom</b> on any source message to add it here.</li>`;
    }
    return this._lane.map((e, i) => {
      const m = e.message;
      const role = (m.role || "?").toLowerCase();
      const label = ROLE_LABELS[role] || role;
      const preview = this._previewText(m);
      return `
        <li class="lane-item lane-role-${esc(role)}" draggable="true" data-uid="${esc(e.uid)}">
          <div class="lane-grip" title="Drag to reorder">⋮⋮</div>
          <div class="lane-body">
            <div class="lane-head">
              <span class="turn-role">${esc(label)}</span>
              <cv-harness-badge harness="${esc(e.harness)}"></cv-harness-badge>
              <span class="lane-from muted" title="from ${esc(e.from)}">${esc(e.from)}</span>
            </div>
            <div class="lane-preview muted">${esc(preview)}</div>
          </div>
          <div class="lane-ctrls">
            <button type="button" class="lane-btn" data-up="${esc(e.uid)}" title="Move up" ${i === 0 ? "disabled" : ""}>▲</button>
            <button type="button" class="lane-btn" data-down="${esc(e.uid)}" title="Move down" ${i === this._lane.length - 1 ? "disabled" : ""}>▼</button>
            <button type="button" class="lane-btn" data-fork="${esc(e.uid)}" title="Fork from here — drop everything after">⑂</button>
            <button type="button" class="lane-btn lane-rm" data-rm="${esc(e.uid)}" title="Remove">✕</button>
          </div>
        </li>`;
    }).join("");
  }

  _previewText(m) {
    for (const b of m.content || []) {
      if (b.kind === "text" && b.text?.trim()) return b.text.trim().replace(/\s+/g, " ").slice(0, 120);
    }
    for (const b of m.content || []) {
      if (b.kind === "tool_use") return `⚙ ${b.name || "tool"}`;
      if (b.kind === "tool_result") return `↳ ${String(b.content || "").slice(0, 100)}`;
      if (b.kind === "thinking") return `💭 ${(b.text || "[opaque]").slice(0, 100)}`;
    }
    return "(empty)";
  }

  _idx(uid) { return this._lane.findIndex((e) => e.uid === uid); }

  _wireLane() {
    const list = this.querySelector(".lane-list");
    if (!list) return;

    list.querySelectorAll("[data-up]").forEach((b) => b.addEventListener("click", () => this._move(b.dataset.up, -1)));
    list.querySelectorAll("[data-down]").forEach((b) => b.addEventListener("click", () => this._move(b.dataset.down, +1)));
    list.querySelectorAll("[data-rm]").forEach((b) => b.addEventListener("click", () => {
      const i = this._idx(b.dataset.rm); if (i >= 0) this._lane.splice(i, 1); this.render();
    }));
    list.querySelectorAll("[data-fork]").forEach((b) => b.addEventListener("click", () => {
      const i = this._idx(b.dataset.fork);
      if (i >= 0) { this._lane = this._lane.slice(0, i + 1); this._composedId = randomId(); this.render(); }
    }));

    // Drag to reorder.
    list.querySelectorAll(".lane-item").forEach((li) => {
      li.addEventListener("dragstart", (e) => { this._dragUid = li.dataset.uid; li.classList.add("dragging"); e.dataTransfer.effectAllowed = "move"; });
      li.addEventListener("dragend", () => { this._dragUid = null; li.classList.remove("dragging"); list.querySelectorAll(".drop-target").forEach((x) => x.classList.remove("drop-target")); });
      li.addEventListener("dragover", (e) => { e.preventDefault(); li.classList.add("drop-target"); });
      li.addEventListener("dragleave", () => li.classList.remove("drop-target"));
      li.addEventListener("drop", (e) => {
        e.preventDefault();
        const from = this._idx(this._dragUid), to = this._idx(li.dataset.uid);
        if (from < 0 || to < 0 || from === to) return;
        const [moved] = this._lane.splice(from, 1);
        this._lane.splice(to, 0, moved);
        this.render();
      });
    });
  }

  _move(uid, delta) {
    const i = this._idx(uid), j = i + delta;
    if (i < 0 || j < 0 || j >= this._lane.length) return;
    [this._lane[i], this._lane[j]] = [this._lane[j], this._lane[i]];
    this.render();
  }
}

customElements.define("cv-loom", CvLoom);
