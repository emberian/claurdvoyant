// <cv-transcript> — renders a single Session as role-labeled turns.
//
// API: transcript.session = Session    (setter; triggers render)
//
// Renders text blocks, collapsible thinking, tool_use as a labeled JSON code
// block, tool_result (with error styling), and images as labeled placeholders.
import "./cv-harness-badge.js";
import {
  esc, pretty, fmtTime, sessionLabel, shortPath, sumTokens, msgCount,
  toOpenSession, toMarkdown, downloadFile, slug, ROLE_LABELS, HARNESS_LABELS,
} from "./util.js";
import { renderMarkdown, renderCodeBlock } from "../markdown.js";
import { getSubagents, getSubagent, getEvents } from "./hydrate.js";

class CvTranscript extends HTMLElement {
  constructor() {
    super();
    this._session = null;
    // When true, render a "+" affordance on each message so a host (the loom)
    // can collect messages. Hidden by default.
    this._pickMode = false;
  }

  set session(s) {
    this._session = s || null;
    this.render();
  }
  get session() { return this._session; }

  set pickMode(v) { this._pickMode = !!v; this.render(); }

  connectedCallback() { if (!this.childElementCount) this.render(); }

  render() {
    const s = this._session;
    if (!s) {
      this.innerHTML = `<div class="transcript-empty muted">
        <p>Select a session to view its transcript.</p>
      </div>`;
      return;
    }

    // `content-visibility:auto` on `.turn` (CSS) makes the browser skip layout/paint for off-screen
    // turns and remember each turn's real height once measured — native, correct virtualization. (The
    // old hand-rolled sliding window mapped scroll through *estimated* heights, so one tall Gemini
    // message threw off the math and left blank gaps on scroll.)
    //
    // Building the HTML is the only O(n) cost left (markdown per block), so for huge sessions (20k+
    // messages exist) we render the first screenful synchronously, then append the rest in
    // rAF-scheduled chunks — instant open, no freeze, and scrolling works as chunks fill in.
    // `_pump` renders from `_cursor` to the current end of `s.messages`, so paged loads
    // (cvd's windowed endpoint via cv-app) just push and re-pump.
    this.innerHTML = `${this._headerHtml(s)}<div class="turns"></div>`;
    this._wireHeader(s);
    this._turns = this.querySelector(".turns");
    this._cursor = 0;
    this._pumping = false;
    this._moreInFlight = false;
    this._io?.disconnect();
    this._io = null;
    this._pump();

    this._mountSubagents(s);
    this._mountEvents(s);
  }

  /** Render any not-yet-rendered messages in rAF-scheduled chunks (first chunk synchronously). */
  _pump() {
    const s = this._session;
    if (!s || this._pumping) return;
    this._pumping = true;
    const CHUNK = 250;
    const step = () => {
      if (this._session !== s) { this._pumping = false; return; } // session changed — abandon
      const messages = s.messages || [];
      const end = Math.min(messages.length, this._cursor + CHUNK);
      let html = "";
      for (; this._cursor < end; this._cursor++) html += this._messageHtml(messages[this._cursor], this._cursor);
      if (html) this._turns.insertAdjacentHTML("beforeend", html);
      this._wireBlocks(this._turns);
      if (this._cursor < messages.length) requestAnimationFrame(step);
      else { this._pumping = false; this._renderPager(); }
    };
    step();
  }

  // ---- paged loading ("load more") ----------------------------------------
  // When the session came from cvd's windowed `/messages` endpoint, `session._paged` marks that
  // more messages exist server-side. We show a "load more" footer and also fire when it scrolls
  // into view; the host (cv-app) listens for "load-more", fetches the next window, pushes onto
  // `session.messages`, and calls `notifyAppended()`.

  _renderPager() {
    const s = this._session;
    let pager = this.querySelector(".load-more");
    if (!s || !s._paged) {
      pager?.remove();
      this._io?.disconnect();
      this._io = null;
      return;
    }
    if (!pager) {
      pager = document.createElement("div");
      pager.className = "load-more";
      pager.innerHTML = `
        <button type="button" class="mini-btn load-more-btn">Load more messages</button>
        <span class="load-more-note muted"></span>`;
      this.appendChild(pager);
      pager.querySelector(".load-more-btn").addEventListener("click", () => this._requestMore());
      if (typeof IntersectionObserver === "function") {
        this._io = new IntersectionObserver((entries) => {
          if (entries.some((e) => e.isIntersecting)) this._requestMore();
        });
        this._io.observe(pager);
      }
    }
    const loaded = (s.messages || []).length;
    const total = s.message_count && s.message_count > loaded ? s.message_count : null;
    pager.querySelector(".load-more-note").textContent =
      `showing ${loaded}${total ? ` of ${total}` : ""} messages`;
  }

  _requestMore() {
    if (this._moreInFlight || !this._session?._paged) return;
    this._moreInFlight = true;
    const btn = this.querySelector(".load-more-btn");
    if (btn) { btn.disabled = true; btn.textContent = "Loading…"; }
    this.dispatchEvent(new CustomEvent("load-more", {
      detail: { session: this._session }, bubbles: true,
    }));
  }

  /** Host callback once a "load-more" fetch settled (messages appended and/or `_paged` updated). */
  notifyAppended() {
    this._moreInFlight = false;
    const btn = this.querySelector(".load-more-btn");
    if (btn) { btn.disabled = false; btn.textContent = "Load more messages"; }
    this._pump();
    this._renderPager();
  }

  // ---- events panel --------------------------------------------------------
  // What the session DID: file edits/commands/errors from the event catalog (cvd's `/events`
  // endpoint / the desktop's `local_events`). Collapsible, below the header, mirroring the
  // sub-agents panel. Sessions without events (or an older cvd) simply get no panel.
  async _mountEvents(session) {
    if (!session || session._isSubagent) return;
    const events = await getEvents(session);
    if (this._session !== session || !events.length) return;
    const counts = {};
    for (const e of events) counts[e.kind] = (counts[e.kind] || 0) + 1;
    const summary = [
      counts.file_edit ? `${counts.file_edit} edit${counts.file_edit === 1 ? "" : "s"}` : "",
      counts.command ? `${counts.command} command${counts.command === 1 ? "" : "s"}` : "",
      counts.error ? `${counts.error} error${counts.error === 1 ? "" : "s"}` : "",
      counts.file_read ? `${counts.file_read} read${counts.file_read === 1 ? "" : "s"}` : "",
    ].filter(Boolean).join(" · ");

    const panel = document.createElement("div");
    panel.className = "sub-panel ev-panel";
    panel.innerHTML = `
      <button type="button" class="sub-panel-head" aria-expanded="false">
        <span class="sub-fork" aria-hidden="true">⚡</span>
        <b>${events.length}</b> event${events.length === 1 ? "" : "s"}${summary ? ` — ${esc(summary)}` : ""}
        <span class="sub-caret" aria-hidden="true">▸</span>
      </button>
      <div class="sub-list ev-list" hidden>${events.map((e) => this._eventRowHtml(e)).join("")}</div>`;
    // Below the sub-agents panel when present, else right under the header.
    const anchor = this.querySelector(".sub-panel") || this.querySelector(".transcript-header");
    if (anchor) anchor.insertAdjacentElement("afterend", panel);
    else this.insertAdjacentElement("afterbegin", panel);

    const list = panel.querySelector(".ev-list");
    panel.querySelector(".sub-panel-head").addEventListener("click", (e) => {
      const open = list.hasAttribute("hidden");
      list.toggleAttribute("hidden", !open);
      e.currentTarget.setAttribute("aria-expanded", String(open));
      e.currentTarget.querySelector(".sub-caret").textContent = open ? "▾" : "▸";
    });
  }

  _eventRowHtml(e) {
    const glyphs = { file_edit: "✎", file_read: "👁", command: "$", error: "✖", tool: "⚙" };
    const glyph = glyphs[e.kind] || "⚙";
    const target = e.target
      ? (e.kind === "file_edit" || e.kind === "file_read" || e.kind === "tool"
        ? shortPath(e.target, 4)
        : e.target)
      : "";
    const when = e.ts ? fmtTime(e.ts * 1000) : "";
    return `
      <div class="ev-row ev-${esc(e.kind)}">
        <span class="ev-glyph" aria-hidden="true">${glyph}</span>
        <span class="ev-kind">${esc(e.kind)}</span>
        ${e.tool ? `<span class="ev-tool muted">${esc(e.tool)}</span>` : ""}
        <span class="ev-target" title="${esc(e.target || "")}">${esc(target)}</span>
        ${when ? `<span class="ev-when muted">${esc(when)}</span>` : ""}
        ${e.detail ? `<div class="ev-detail muted">↳ ${esc(e.detail)}</div>` : ""}
      </div>`;
  }

  // ---- sub-agent tree -----------------------------------------------------
  // If this session spawned sub-agents (Claude Code Task), show a collapsible tree below the header;
  // each child lazily loads its full transcript inline (recursively rendered by a nested transcript).
  async _mountSubagents(session) {
    if (!session || session._isSubagent) return; // sub-agents don't spawn their own
    const subs = await getSubagents(session);
    if (this._session !== session || !subs.length) return; // changed / none
    this._subs = subs;
    const panel = document.createElement("div");
    panel.className = "sub-panel";
    panel.innerHTML = `
      <button type="button" class="sub-panel-head" aria-expanded="false">
        <span class="sub-fork" aria-hidden="true">⑂</span>
        <b>${subs.length}</b> sub-agent${subs.length === 1 ? "" : "s"} spawned
        <span class="sub-caret" aria-hidden="true">▸</span>
      </button>
      <div class="sub-list" hidden>${subs.map((s, i) => this._subRowHtml(s, i)).join("")}</div>`;
    const header = this.querySelector(".transcript-header");
    if (header) header.insertAdjacentElement("afterend", panel);
    else this.insertAdjacentElement("afterbegin", panel);

    const list = panel.querySelector(".sub-list");
    panel.querySelector(".sub-panel-head").addEventListener("click", (e) => {
      const open = list.hasAttribute("hidden");
      list.toggleAttribute("hidden", !open);
      e.currentTarget.setAttribute("aria-expanded", String(open));
      e.currentTarget.querySelector(".sub-caret").textContent = open ? "▾" : "▸";
    });
    list.addEventListener("click", (e) => {
      const row = e.target.closest(".sub-row");
      if (row) this._toggleSub(row);
    });
  }

  _subRowHtml(s, i) {
    const when = fmtTime(s.updated_at || s.created_at);
    const n = msgCount(s);
    return `
      <div class="sub-row" data-idx="${i}">
        <div class="sub-row-head">
          <span class="sub-dot" aria-hidden="true"></span>
          <span class="sub-row-title">${esc(s.id || `sub-agent ${i + 1}`)}</span>
          <span class="sub-row-meta muted">${n} msg${n === 1 ? "" : "s"} · ${esc(when)}</span>
          <span class="sub-row-caret" aria-hidden="true">▸</span>
        </div>
        <div class="sub-row-body" hidden></div>
      </div>`;
  }

  async _toggleSub(row) {
    const body = row.querySelector(".sub-row-body");
    const caret = row.querySelector(".sub-row-caret");
    if (!body.hasAttribute("hidden")) {
      body.setAttribute("hidden", "");
      caret.textContent = "▸";
      row.classList.remove("open");
      return;
    }
    body.removeAttribute("hidden");
    caret.textContent = "▾";
    row.classList.add("open");
    if (body.dataset.loaded) return;
    body.dataset.loaded = "1";
    body.innerHTML = `<div class="muted sub-loading">Loading sub-agent…</div>`;
    const stub = this._subs[+row.dataset.idx];
    try {
      const full = await getSubagent(stub._parentHarness, stub._parentId, stub.id);
      full._isSubagent = true; // so the nested transcript doesn't re-query sub-agents
      body.innerHTML = "";
      const nested = document.createElement("cv-transcript");
      body.appendChild(nested);
      nested.session = full;
      // Relabel the row with the task prompt (its first user message) now that we have it.
      const label = sessionLabel(full);
      if (label && label !== "(untitled)") row.querySelector(".sub-row-title").textContent = label;
    } catch (e) {
      body.innerHTML = `<div class="muted">Couldn't load this sub-agent (${esc(String(e?.message || e))}).</div>`;
    }
  }

  // Header-level controls (export buttons) — wired once per render.
  _wireHeader(s) {
    this.querySelector("[data-export-json]")?.addEventListener("click", () => {
      downloadFile(`${slug(sessionLabel(s))}.opensession.json`,
        JSON.stringify(toOpenSession(s), null, 2), "application/json");
    });
    this.querySelector("[data-export-md]")?.addEventListener("click", () => {
      downloadFile(`${slug(sessionLabel(s))}.md`, toMarkdown(s), "text/markdown");
    });
  }

  // Per-message controls (pick + copy). Re-run for any freshly-mounted window
  // of messages (virtualization), scoped to `root` to avoid double-binding.
  _wireBlocks(root) {
    const s = this._session;
    root.querySelectorAll("[data-pick]:not([data-wired])").forEach((btn) => {
      btn.setAttribute("data-wired", "1");
      btn.addEventListener("click", () => {
        const idx = Number(btn.dataset.pick);
        const m = (s?.messages || [])[idx];
        if (m) this.dispatchEvent(new CustomEvent("pick-message", {
          detail: { session: s, message: m, index: idx }, bubbles: true,
        }));
      });
    });
    root.querySelectorAll("[data-copy]:not([data-wired])").forEach((btn) => {
      btn.setAttribute("data-wired", "1");
      btn.addEventListener("click", async () => {
        const pre = btn.closest(".block")?.querySelector("pre");
        const text = pre ? pre.textContent : "";
        try {
          await navigator.clipboard.writeText(text);
          const old = btn.textContent;
          btn.textContent = "copied";
          setTimeout(() => { btn.textContent = old; }, 1200);
        } catch { /* clipboard may be unavailable */ }
      });
    });
  }

  _headerHtml(s) {
    const h = (s.harness || "").toLowerCase();
    const meta = [];
    if (s.model) meta.push(`<span class="kv"><span class="k">model</span>${esc(s.model)}</span>`);
    if (s.cwd) meta.push(`<span class="kv" title="${esc(s.cwd)}"><span class="k">cwd</span>${esc(shortPath(s.cwd, 4))}</span>`);
    if (s.git?.branch) meta.push(`<span class="kv"><span class="k">branch</span>${esc(s.git.branch)}</span>`);
    if (s.git?.commit) meta.push(`<span class="kv"><span class="k">commit</span>${esc(String(s.git.commit).slice(0, 8))}</span>`);
    const when = fmtTime(s.updated_at || s.created_at);
    if (when) meta.push(`<span class="kv"><span class="k">updated</span>${esc(when)}</span>`);
    const tok = sumTokens(s);
    if (tok.total) meta.push(`<span class="kv" title="total token usage"><span class="k">tokens</span>${tok.input.toLocaleString()}↓ ${tok.output.toLocaleString()}↑</span>`);
    if (s.id) meta.push(`<span class="kv"><span class="k">id</span>${esc(s.id)}</span>`);

    return `
      <header class="transcript-header">
        <div class="th-title">
          <cv-harness-badge harness="${esc(h)}"></cv-harness-badge>
          <h2>${esc(sessionLabel(s))}</h2>
          <div class="th-actions">
            <button type="button" class="mini-btn" data-export-md title="Download as Markdown">⬇ .md</button>
            <button type="button" class="mini-btn" data-export-json title="Download as OpenSession JSON">⬇ .json</button>
          </div>
        </div>
        <div class="th-meta">${meta.join("")}</div>
        ${s.source_path ? `<div class="th-source muted" title="${esc(s.source_path)}">${esc(s.source_path)}</div>` : ""}
      </header>`;
  }

  _messageHtml(m, idx) {
    const role = (m.role || "").toLowerCase();
    const roleLabel = ROLE_LABELS[role] || role || "?";
    const when = fmtTime(m.timestamp);
    const usage = this._usageHtml(m.usage);
    const model = m.model ? `<span class="turn-model">${esc(m.model)}</span>` : "";
    const blocks = (m.content || []).map((b) => this._blockHtml(b)).join("");
    const pick = this._pickMode
      ? `<button type="button" class="pick-btn" data-pick="${idx}" title="Add this message to the loom">＋ loom</button>`
      : "";
    return `
      <article class="turn turn-${esc(role)}">
        <div class="turn-head">
          <span class="turn-role">${esc(roleLabel)}</span>
          ${model}
          ${when ? `<span class="turn-when muted">${esc(when)}</span>` : ""}
          ${usage}
          ${pick}
        </div>
        <div class="turn-body">${blocks || '<div class="muted block-empty">(empty)</div>'}</div>
      </article>`;
  }

  _usageHtml(u) {
    if (!u) return "";
    const bits = [];
    if (u.input_tokens != null) bits.push(`${u.input_tokens}↓`);
    if (u.output_tokens != null) bits.push(`${u.output_tokens}↑`);
    if (u.cache_read_tokens) bits.push(`${u.cache_read_tokens} cached`);
    if (!bits.length) return "";
    return `<span class="turn-usage muted" title="token usage">${esc(bits.join(" · "))}</span>`;
  }

  _blockHtml(b) {
    switch (b?.kind) {
      case "text":
        return `<div class="block block-text">${this._renderText(b.text || "")}</div>`;

      case "thinking": {
        const tag = b.redacted ? " (redacted)" : b.encrypted ? " (encrypted)" : "";
        const body = b.text
          ? this._renderText(b.text)
          : b.redacted ? '<span class="opaque-blob">[redacted reasoning]</span>'
          : b.encrypted ? '<span class="opaque-blob">[encrypted reasoning blob]</span>'
          : "";
        return `
          <details class="block thinking">
            <summary>💭 Thinking${tag}</summary>
            <div class="thinking-body">${body}${b.signature ? `<div class="sig muted" title="signature">sig: ${esc(String(b.signature).slice(0, 24))}…</div>` : ""}</div>
          </details>`;
      }

      case "tool_use": {
        const input = b.input == null ? "" : pretty(b.input);
        const big = input.length > 600;
        const pre = renderCodeBlock(input, "json");
        const label = `
          <div class="block-label">
            <span class="tool-glyph">⚙</span> tool_use
            <span class="tool-name">${esc(b.name || "?")}</span>
            <button type="button" class="copy-btn" data-copy aria-label="Copy input">copy</button>
          </div>`;
        return big
          ? `<details class="block tool-use"><summary class="tool-summary">⚙ <span class="tool-name">${esc(b.name || "?")}</span> <span class="muted">tool_use · ${input.length} chars</span></summary>${pre}</details>`
          : `<div class="block tool-use">${label}${pre}</div>`;
      }

      case "tool_result": {
        const err = b.is_error ? " is-error" : "";
        const content = String(b.content ?? "");
        const status = b.status ? `<span class="tool-status">${esc(b.status)}</span>` : "";
        const tname = b.tool_name ? `<span class="tool-name">${esc(b.tool_name)}</span>` : "";
        const details = b.details != null
          ? `<details class="tool-details"><summary class="muted">details</summary>${renderCodeBlock(pretty(b.details), "json")}</details>`
          : "";
        const big = content.length > 600;
        const pre = `<pre class="tool-out"><code>${esc(content)}</code></pre>`;
        const label = `
          <div class="block-label">
            <span class="tool-glyph">${b.is_error ? "✖" : "↳"}</span>
            tool_result${b.is_error ? " (error)" : ""} ${tname} ${status}
            <button type="button" class="copy-btn" data-copy aria-label="Copy result">copy</button>
          </div>`;
        return big
          ? `<details class="block tool-result${err}"><summary class="tool-summary">${b.is_error ? "✖" : "↳"} <span class="muted">tool_result${b.is_error ? " (error)" : ""} · ${content.length} chars</span> ${status}</summary>${pre}${details}</details>`
          : `<div class="block tool-result${err}">${label}${pre}${details}</div>`;
      }

      case "file": {
        const path = b.path || b.source || "";
        const mime = b.mime ? esc(b.mime) : "file";
        return `
          <div class="block file">
            <div class="file-card">
              <span class="file-glyph">📄</span>
              <div class="file-meta">
                <div class="file-path" title="${esc(path)}">${esc(shortPath(path, 4) || "(file)")}</div>
                <div class="file-type muted">${mime}${b.source && b.source !== path ? ` · ${esc(b.source)}` : ""}</div>
              </div>
            </div>
          </div>`;
      }

      case "image": {
        const mt = b.media_type ? esc(b.media_type) : "image";
        const ref = b.data_ref ? esc(b.data_ref) : "";
        return `
          <div class="block image">
            <div class="image-placeholder">
              <span class="image-glyph">🖼</span>
              <div class="image-meta">
                <div class="image-type">${mt}</div>
                ${ref ? `<div class="image-ref muted" title="${ref}">${esc(shortPath(b.data_ref, 3))}</div>` : ""}
              </div>
            </div>
          </div>`;
      }

      default:
        return `<div class="block unknown-block muted">[${esc(b?.kind ?? "null")} block]${b && Object.keys(b).length > 1 ? `<details><summary class="muted">raw</summary><pre><code>${esc(pretty(b))}</code></pre></details>` : ""}</div>`;
    }
  }

  // Render message prose as (safe) Markdown: headings, lists, blockquotes,
  // emphasis, links, inline + fenced code with a light highlighter. All source
  // text is escaped before any markup is injected (see markdown.js).
  _renderText(text) {
    return `<div class="md">${renderMarkdown(text)}</div>`;
  }
}

customElements.define("cv-transcript", CvTranscript);
