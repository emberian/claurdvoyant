// <cv-transcript> — renders a single Session as role-labeled turns.
//
// API: transcript.session = Session    (setter; triggers render)
//
// Renders text blocks, collapsible thinking, tool_use as a labeled JSON code
// block, tool_result (with error styling), and images as labeled placeholders.
import "./cv-harness-badge.js";
import {
  esc, pretty, fmtTime, sessionLabel, shortPath, sumTokens,
  toOpenSession, toMarkdown, downloadFile, slug, ROLE_LABELS, HARNESS_LABELS,
} from "./util.js";
import { renderMarkdown, renderCodeBlock } from "../markdown.js";

// Above this message count we lazily render: only a window of messages is in
// the DOM at once, so a 12k-message session doesn't freeze the tab.
const VIRTUALIZE_THRESHOLD = 200;
const WINDOW_PAD = 6; // messages of overscan above/below the viewport

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
    this._virtual = null;
    this.render();
  }
  get session() { return this._session; }

  set pickMode(v) { this._pickMode = !!v; this.render(); }

  connectedCallback() { if (!this.childElementCount) this.render(); }

  disconnectedCallback() { this._teardownVirtual(); }

  _teardownVirtual() {
    if (this._scrollHost && this._onScroll) this._scrollHost.removeEventListener("scroll", this._onScroll);
    this._scrollHost = null;
    this._onScroll = null;
    this._virtual = null;
  }

  render() {
    this._teardownVirtual();
    const s = this._session;
    if (!s) {
      this.innerHTML = `<div class="transcript-empty muted">
        <p>Select a session to view its transcript.</p>
      </div>`;
      return;
    }

    const messages = s.messages || [];
    if (messages.length > VIRTUALIZE_THRESHOLD) {
      this._renderVirtual(s, messages);
    } else {
      this.innerHTML = `
        ${this._headerHtml(s)}
        <div class="turns">${messages.map((m, i) => this._messageHtml(m, i)).join("")}</div>
      `;
    }

    this._wireHeader(s);
    this._wireBlocks(this);
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

  // ---- virtualization ----------------------------------------------------
  // For very long transcripts we keep only a sliding window of messages in the
  // DOM. Spacer rows reserve the remaining vertical space so the scrollbar and
  // scroll position behave normally. Heights are estimated, then corrected with
  // measured values as messages scroll through, so the spacers stay honest.

  _renderVirtual(s, messages) {
    this.innerHTML = `
      ${this._headerHtml(s)}
      <div class="virtual-note muted">⚡ Lazy-rendering ${messages.length.toLocaleString()} messages for performance.</div>
      <div class="turns turns-virtual">
        <div class="v-spacer v-top" style="height:0"></div>
        <div class="v-window"></div>
        <div class="v-spacer v-bottom" style="height:0"></div>
      </div>`;

    const window_ = this.querySelector(".v-window");
    const topSpacer = this.querySelector(".v-top");
    const botSpacer = this.querySelector(".v-bottom");

    // Per-message estimated height; corrected as we measure rendered rows.
    const est = 120;
    const heights = new Array(messages.length).fill(est);

    this._virtual = { messages, heights, est, window_, topSpacer, botSpacer, start: -1, end: -1 };

    // Find the scroll container: the nearest ancestor that actually scrolls.
    this._scrollHost = this._findScrollHost();
    this._onScroll = () => this._syncWindow();
    this._scrollHost.addEventListener("scroll", this._onScroll, { passive: true });
    // Also respond to resize (layout changes window math).
    this._syncWindow();
    requestAnimationFrame(() => this._syncWindow());
  }

  _findScrollHost() {
    let el = this.parentElement;
    while (el && el !== document.body) {
      const oy = getComputedStyle(el).overflowY;
      if ((oy === "auto" || oy === "scroll") && el.scrollHeight > el.clientHeight + 4) return el;
      el = el.parentElement;
    }
    return this.parentElement || document.scrollingElement || document.documentElement;
  }

  _syncWindow() {
    const v = this._virtual;
    if (!v) return;
    const host = this._scrollHost;
    const hostRect = host.getBoundingClientRect();
    const turnsTop = this.querySelector(".turns-virtual").getBoundingClientRect().top;
    // Offset of the turns container relative to the scroll content top.
    const scrolled = host.scrollTop + (hostRect.top - turnsTop);
    const viewTop = scrolled;
    const viewBottom = scrolled + host.clientHeight;

    // Walk cumulative heights to find the visible range.
    let acc = 0, start = 0;
    while (start < v.messages.length && acc + v.heights[start] < viewTop) { acc += v.heights[start]; start++; }
    const topPad = acc;
    let end = start;
    while (end < v.messages.length && acc < viewBottom) { acc += v.heights[end]; end++; }

    start = Math.max(0, start - WINDOW_PAD);
    end = Math.min(v.messages.length, end + WINDOW_PAD);

    if (start === v.start && end === v.end) return;
    v.start = start; v.end = end;

    // Reserve space above the first rendered message.
    let above = 0; for (let i = 0; i < start; i++) above += v.heights[i];
    let below = 0; for (let i = end; i < v.messages.length; i++) below += v.heights[i];
    v.topSpacer.style.height = `${above}px`;
    v.botSpacer.style.height = `${below}px`;

    v.window_.innerHTML = v.messages.slice(start, end).map((m, j) => this._messageHtml(m, start + j)).join("");
    this._wireBlocks(v.window_);

    // Correct height estimates from the actually-rendered rows.
    let corrected = false;
    [...v.window_.children].forEach((node, j) => {
      const h = node.offsetHeight;
      const idx = start + j;
      if (h && Math.abs(h - v.heights[idx]) > 2) { v.heights[idx] = h; corrected = true; }
    });
    if (corrected) {
      let above2 = 0; for (let i = 0; i < start; i++) above2 += v.heights[i];
      let below2 = 0; for (let i = end; i < v.messages.length; i++) below2 += v.heights[i];
      v.topSpacer.style.height = `${above2}px`;
      v.botSpacer.style.height = `${below2}px`;
    }
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
