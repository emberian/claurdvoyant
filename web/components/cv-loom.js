// <cv-loom> — the splice / loom composer. ✨ The headline.
//
// A three-pane workspace:
//   1. Source picker  — choose any loaded session; its transcript renders with
//      "＋ loom" buttons on each message (pickMode on <cv-transcript>).
//   2. Composition lane — the messages you've collected, in order. Reorder
//      (▲ ▼ or drag), remove (✕), and "fork from here" (drop everything after).
//   3. Live preview     — the composed transcript rendered as it will export.
//
// Beyond rearranging, the loom can actually *loom*: with an OpenRouter API key
// it GENERATES a continuation from the current composition and appends the
// model's reply as a new assistant message. "Fork from here" + generate spawns
// sibling BRANCHES (named variants) you can switch between — grow a transcript
// into alternate futures and compare them in the Compare view.
//
// Export: download the active branch as an OpenSession JSON document (or .md) —
// pure client-side, no wasm.
//
// API: loom.sessions = Session[]   (setter)
import "./cv-transcript.js";
import "./cv-harness-badge.js";
import {
  esc, sessionLabel, ROLE_LABELS, toOpenSession, toMarkdown,
  downloadFile, slug, randomId,
} from "./util.js";
import {
  generate, getKey, setKey, getModel, setModel, hasKey,
  MODEL_PRESETS, DEFAULT_MODEL,
} from "../openrouter.js";

class CvLoom extends HTMLElement {
  constructor() {
    super();
    this._sessions = [];
    this._sourceId = null;
    this._title = "Spliced session";
    this._dragUid = null;

    // Branch tree. Each branch is a named variant holding its own lane:
    //   { id, name, lane: [{ uid, message, from, harness }], composedId }
    // The active branch is what every pane reads/writes.
    this._branches = [];
    this._activeBranchId = null;
    this._newBranch("main");

    this._showSettings = false;
    this._gen = { busy: false, error: "", streamUid: null };
    this._abort = null;
  }

  // ---- branch model ------------------------------------------------------

  _newBranch(name, lane = []) {
    const b = { id: randomId(), name: name || `branch ${this._branches.length + 1}`, lane, composedId: randomId() };
    this._branches.push(b);
    this._activeBranchId = b.id;
    return b;
  }
  _branch() { return this._branches.find((b) => b.id === this._activeBranchId) || this._branches[0]; }
  get _lane() { return this._branch().lane; }
  set _lane(v) { this._branch().lane = v; }

  set sessions(arr) {
    this._sessions = Array.isArray(arr) ? arr : [];
    if (!this._sessions.find((s) => (s.id || "") === this._sourceId)) {
      this._sourceId = this._sessions[0]?.id ?? null;
    }
    this.render();
  }
  get sessions() { return this._sessions; }

  connectedCallback() { this.render(); }

  disconnectedCallback() { this._abort?.abort(); }

  _source() { return this._sessions.find((s) => (s.id || "") === this._sourceId) || null; }

  _composed() {
    const b = this._branch();
    return {
      openSession: "0.1",
      harness: "opensession",
      id: b.composedId,
      title: this._title + (this._branches.length > 1 ? ` — ${b.name}` : ""),
      messages: b.lane.map((e) => e.message),
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
        <span class="muted">Splice messages from any session — or ⚡ generate continuations with OpenRouter — and weave alternate branches.</span>
        <button type="button" class="mini-btn loom-settings-btn" data-settings title="OpenRouter generation settings">⚙ generation</button>
      </div>
      ${this._settingsHtml()}
      ${this._branchBarHtml()}
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
          ${this._genBarHtml()}
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
      downloadFile(`${slug(this._composed().title)}.opensession.json`,
        JSON.stringify(toOpenSession(this._composed()), null, 2), "application/json");
    });
    this.querySelector("[data-export-md]")?.addEventListener("click", () => {
      downloadFile(`${slug(this._composed().title)}.md`, toMarkdown(this._composed()), "text/markdown");
    });

    this.querySelector("[data-settings]")?.addEventListener("click", () => {
      this._showSettings = !this._showSettings; this.render();
    });

    this._wireSettings();
    this._wireBranchBar();
    this._wireGenBar();
    this._wireLane();
  }

  _renderPreview() {
    const p = this.querySelector(".loom-preview-transcript");
    if (p) p.session = this._lane.length ? this._composed() : null;
  }

  // ---- settings panel (OpenRouter key + model) ---------------------------

  _settingsHtml() {
    if (!this._showSettings) return "";
    const key = getKey();
    const model = getModel();
    const masked = key ? key.slice(0, 6) + "…" + key.slice(-4) : "";
    const opts = MODEL_PRESETS.map((m) =>
      `<option value="${esc(m)}"${m === model ? " selected" : ""}>${esc(m)}</option>`).join("");
    const inPresets = MODEL_PRESETS.includes(model);
    return `
      <div class="loom-settings">
        <div class="ls-row">
          <label class="ls-label" for="ls-key">OpenRouter API key</label>
          <input id="ls-key" class="ls-key" type="password" autocomplete="off" spellcheck="false"
            placeholder="sk-or-v1-…" value="${esc(key)}" aria-label="OpenRouter API key" />
          <button type="button" class="mini-btn" data-ls-clear ${key ? "" : "disabled"}>forget</button>
        </div>
        <div class="ls-row">
          <label class="ls-label" for="ls-model">Model</label>
          <select id="ls-model-preset" class="ls-model-preset" aria-label="Model preset">
            ${opts}
            <option value="__custom__"${inPresets ? "" : " selected"}>custom…</option>
          </select>
          <input id="ls-model" class="ls-model" type="text" value="${esc(model)}"
            placeholder="provider/model-id" aria-label="Model id" />
        </div>
        <p class="ls-note muted">
          The key is stored in <code>localStorage</code> on this device only and is sent
          <strong>only</strong> to <code>openrouter.ai</code> over HTTPS. ${masked ? `Currently: <code>${esc(masked)}</code>.` : "No key set yet."}
        </p>
      </div>`;
  }

  _wireSettings() {
    if (!this._showSettings) return;
    const keyEl = this.querySelector("#ls-key");
    const modelEl = this.querySelector("#ls-model");
    const presetEl = this.querySelector("#ls-model-preset");
    keyEl?.addEventListener("input", () => setKey(keyEl.value.trim()));
    this.querySelector("[data-ls-clear]")?.addEventListener("click", () => {
      setKey(""); this.render();
    });
    presetEl?.addEventListener("change", () => {
      if (presetEl.value !== "__custom__") {
        modelEl.value = presetEl.value;
        setModel(presetEl.value);
      }
      modelEl.focus();
    });
    modelEl?.addEventListener("input", () => {
      setModel(modelEl.value.trim() || DEFAULT_MODEL);
      // Reflect free-text into the preset selector.
      if (presetEl) presetEl.value = MODEL_PRESETS.includes(modelEl.value.trim()) ? modelEl.value.trim() : "__custom__";
    });
  }

  // ---- branch bar --------------------------------------------------------

  _branchBarHtml() {
    const tabs = this._branches.map((b) => {
      const on = b.id === this._activeBranchId;
      return `
        <button type="button" class="branch-tab${on ? " on" : ""}" data-branch="${esc(b.id)}" title="${esc(b.name)} · ${b.lane.length} msg">
          <span class="branch-name">${esc(b.name)}</span>
          <span class="branch-count muted">${b.lane.length}</span>
          ${this._branches.length > 1 ? `<span class="branch-x" data-branch-del="${esc(b.id)}" title="Delete branch">✕</span>` : ""}
        </button>`;
    }).join("");
    return `
      <div class="branch-bar" role="tablist" aria-label="Branches">
        <span class="branch-bar-label muted">branches</span>
        ${tabs}
        <button type="button" class="mini-btn branch-add" data-branch-dup title="Duplicate the active branch into a new sibling">＋ branch</button>
      </div>`;
  }

  _wireBranchBar() {
    this.querySelectorAll("[data-branch]").forEach((b) =>
      b.addEventListener("click", (e) => {
        if (e.target.closest("[data-branch-del]")) return;
        this._activeBranchId = b.dataset.branch; this.render();
      }));
    this.querySelectorAll("[data-branch-del]").forEach((x) =>
      x.addEventListener("click", (e) => {
        e.stopPropagation();
        this._deleteBranch(x.dataset.branchDel);
      }));
    this.querySelector("[data-branch-dup]")?.addEventListener("click", () => {
      const cur = this._branch();
      const lane = cur.lane.map((e) => ({ ...e, uid: randomId(), message: JSON.parse(JSON.stringify(e.message)) }));
      this._newBranch(`${cur.name}′`, lane);
      this.render();
    });
  }

  _deleteBranch(id) {
    if (this._branches.length <= 1) return;
    const i = this._branches.findIndex((b) => b.id === id);
    if (i < 0) return;
    this._branches.splice(i, 1);
    if (this._activeBranchId === id) this._activeBranchId = this._branches[Math.max(0, i - 1)].id;
    this.render();
  }

  // ---- generation bar ----------------------------------------------------

  _genBarHtml() {
    const model = getModel();
    const ready = hasKey();
    const busy = this._gen.busy;
    return `
      <div class="loom-gen">
        <button type="button" class="btn-gen" data-generate ${this._lane.length && !busy ? "" : "disabled"}
          title="${ready ? "Send the composition to OpenRouter and append the reply" : "Set an API key in ⚙ generation first"}">
          ${busy ? "⏳ generating…" : "⚡ generate continuation"}
        </button>
        ${busy ? `<button type="button" class="mini-btn" data-gen-stop>stop</button>` : ""}
        <span class="gen-model muted" title="active model">${esc(model)}</span>
        ${!ready ? `<button type="button" class="gen-needkey" data-settings-open>add API key →</button>` : ""}
        ${this._gen.error ? `<div class="gen-error" role="alert">${esc(this._gen.error)}</div>` : ""}
      </div>`;
  }

  _wireGenBar() {
    this.querySelector("[data-generate]")?.addEventListener("click", () => this._generate());
    this.querySelector("[data-gen-stop]")?.addEventListener("click", () => this._abort?.abort());
    this.querySelector("[data-settings-open]")?.addEventListener("click", () => {
      this._showSettings = true; this.render();
    });
  }

  async _generate() {
    if (this._gen.busy || !this._lane.length) return;
    if (!hasKey()) { this._showSettings = true; this._gen.error = "Add an OpenRouter API key first."; this.render(); return; }

    this._gen.busy = true;
    this._gen.error = "";
    this._abort = new AbortController();

    // Append a placeholder assistant message we stream into.
    const placeholder = {
      id: randomId(),
      role: "assistant",
      model: getModel(),
      content: [{ kind: "text", text: "" }],
      timestamp: new Date().toISOString(),
    };
    const entry = { uid: randomId(), message: placeholder, from: "⚡ generated", harness: "opensession" };
    this._lane.push(entry);
    this._gen.streamUid = entry.uid;
    this.render();

    const setText = (full) => {
      const m = this._lane.find((e) => e.uid === entry.uid)?.message;
      if (m) {
        m.content = [{ kind: "text", text: full }];
        // Update only the preview + this lane row's text, cheaply.
        this._renderPreview();
        const row = this.querySelector(`.lane-item[data-uid="${CSS.escape(entry.uid)}"] .lane-preview`);
        if (row) row.textContent = (full || "").replace(/\s+/g, " ").slice(0, 120) || "(generating…)";
      }
    };

    try {
      const messages = this._lane
        .filter((e) => e.uid !== entry.uid) // send everything *before* the placeholder
        .map((e) => e.message);
      const text = await generate({
        messages,
        signal: this._abort.signal,
        onToken: (_chunk, full) => setText(full),
      });
      setText(text);
      if (!text.trim()) {
        // Empty reply — drop the placeholder so we don't leave a blank turn.
        const i = this._lane.findIndex((e) => e.uid === entry.uid);
        if (i >= 0) this._lane.splice(i, 1);
        this._gen.error = "Model returned an empty response.";
      }
    } catch (err) {
      // Remove the placeholder on failure (unless we got partial streamed text).
      const cur = this._lane.find((e) => e.uid === entry.uid)?.message;
      const got = cur?.content?.[0]?.text?.trim();
      if (!got) {
        const i = this._lane.findIndex((e) => e.uid === entry.uid);
        if (i >= 0) this._lane.splice(i, 1);
      }
      this._gen.error = err?.name === "AbortError"
        ? "Generation stopped." + (got ? " Kept the partial reply." : "")
        : (err?.message || "Generation failed.");
    } finally {
      this._gen.busy = false;
      this._gen.streamUid = null;
      this._abort = null;
      this.render();
      this.querySelector(".lane-list")?.lastElementChild?.scrollIntoView({ block: "nearest" });
    }
  }

  // ---- lane (collection / reorder / fork) --------------------------------

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
      return `<li class="lane-empty muted">Empty — click <b>＋ loom</b> on any source message to add it, then <b>⚡ generate</b> a continuation.</li>`;
    }
    return this._lane.map((e, i) => {
      const m = e.message;
      const role = (m.role || "?").toLowerCase();
      const label = ROLE_LABELS[role] || role;
      const gen = e.from === "⚡ generated";
      const preview = this._previewText(m) || (this._gen.streamUid === e.uid ? "(generating…)" : "(empty)");
      return `
        <li class="lane-item lane-role-${esc(role)}${gen ? " lane-gen" : ""}" draggable="true" data-uid="${esc(e.uid)}">
          <div class="lane-grip" title="Drag to reorder">⋮⋮</div>
          <div class="lane-body">
            <div class="lane-head">
              <span class="turn-role">${esc(label)}</span>
              ${gen ? `<span class="lane-genbadge" title="generated by OpenRouter">⚡</span>` : `<cv-harness-badge harness="${esc(e.harness)}"></cv-harness-badge>`}
              <span class="lane-from muted" title="from ${esc(e.from)}">${esc(e.from)}</span>
            </div>
            <div class="lane-preview muted">${esc(preview)}</div>
          </div>
          <div class="lane-ctrls">
            <button type="button" class="lane-btn" data-up="${esc(e.uid)}" title="Move up" ${i === 0 ? "disabled" : ""}>▲</button>
            <button type="button" class="lane-btn" data-down="${esc(e.uid)}" title="Move down" ${i === this._lane.length - 1 ? "disabled" : ""}>▼</button>
            <button type="button" class="lane-btn" data-branchfork="${esc(e.uid)}" title="Fork into a new branch from here — keep up to and including this message">⑂</button>
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
    return "";
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
    // Fork from here → spawn a SIBLING BRANCH containing the prefix, switch to it.
    list.querySelectorAll("[data-branchfork]").forEach((b) => b.addEventListener("click", () => {
      const i = this._idx(b.dataset.branchfork);
      if (i < 0) return;
      const prefix = this._lane.slice(0, i + 1).map((e) => ({ ...e, uid: randomId(), message: JSON.parse(JSON.stringify(e.message)) }));
      const base = this._branch().name.replace(/′+$/, "");
      this._newBranch(`${base}·fork`, prefix);
      this.render();
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
    const lane = this._lane;
    const i = this._idx(uid), j = i + delta;
    if (i < 0 || j < 0 || j >= lane.length) return;
    [lane[i], lane[j]] = [lane[j], lane[i]];
    this.render();
  }
}

customElements.define("cv-loom", CvLoom);
