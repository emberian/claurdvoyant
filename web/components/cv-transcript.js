// <cv-transcript> — renders a single Session as role-labeled turns.
//
// API: transcript.session = Session    (setter; triggers render)
//
// Renders text blocks, collapsible thinking, tool_use as a labeled JSON code
// block, tool_result (with error styling), and images as labeled placeholders.
import "./cv-harness-badge.js";
import {
  esc, pretty, fmtTime, sessionLabel, shortPath, ROLE_LABELS, HARNESS_LABELS,
} from "./util.js";

class CvTranscript extends HTMLElement {
  constructor() {
    super();
    this._session = null;
  }

  set session(s) { this._session = s || null; this.render(); }
  get session() { return this._session; }

  connectedCallback() { if (!this.childElementCount) this.render(); }

  render() {
    const s = this._session;
    if (!s) {
      this.innerHTML = `<div class="transcript-empty muted">
        <p>Select a session to view its transcript.</p>
      </div>`;
      return;
    }

    this.innerHTML = `
      ${this._headerHtml(s)}
      <div class="turns">${(s.messages || []).map((m) => this._messageHtml(m)).join("")}</div>
    `;

    // Toggle thinking blocks.
    this.querySelectorAll(".thinking > summary").forEach((sum) => {
      // <details> handles toggling natively; nothing to wire.
    });
    // Copy buttons.
    this.querySelectorAll("[data-copy]").forEach((btn) => {
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
    if (s.id) meta.push(`<span class="kv"><span class="k">id</span>${esc(s.id)}</span>`);

    return `
      <header class="transcript-header">
        <div class="th-title">
          <cv-harness-badge harness="${esc(h)}"></cv-harness-badge>
          <h2>${esc(sessionLabel(s))}</h2>
        </div>
        <div class="th-meta">${meta.join("")}</div>
        ${s.source_path ? `<div class="th-source muted" title="${esc(s.source_path)}">${esc(s.source_path)}</div>` : ""}
      </header>`;
  }

  _messageHtml(m) {
    const role = (m.role || "").toLowerCase();
    const roleLabel = ROLE_LABELS[role] || role || "?";
    const when = fmtTime(m.timestamp);
    const usage = this._usageHtml(m.usage);
    const model = m.model ? `<span class="turn-model">${esc(m.model)}</span>` : "";
    const blocks = (m.content || []).map((b) => this._blockHtml(b)).join("");
    return `
      <article class="turn turn-${esc(role)}">
        <div class="turn-head">
          <span class="turn-role">${esc(roleLabel)}</span>
          ${model}
          ${when ? `<span class="turn-when muted">${esc(when)}</span>` : ""}
          ${usage}
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

      case "thinking":
        return `
          <details class="block thinking">
            <summary>Thinking${b.encrypted ? " (encrypted)" : ""}</summary>
            <div class="thinking-body">${this._renderText(b.text || (b.encrypted ? "[encrypted reasoning blob]" : ""))}</div>
          </details>`;

      case "tool_use": {
        const input = b.input == null ? "" : pretty(b.input);
        return `
          <div class="block tool-use">
            <div class="block-label">
              <span class="tool-glyph">⚙</span> tool_use
              <span class="tool-name">${esc(b.name || "?")}</span>
              <button type="button" class="copy-btn" data-copy aria-label="Copy input">copy</button>
            </div>
            <pre><code>${esc(input)}</code></pre>
          </div>`;
      }

      case "tool_result": {
        const err = b.is_error ? " is-error" : "";
        return `
          <div class="block tool-result${err}">
            <div class="block-label">
              <span class="tool-glyph">${b.is_error ? "✖" : "↳"}</span>
              tool_result${b.is_error ? " (error)" : ""}
              <button type="button" class="copy-btn" data-copy aria-label="Copy result">copy</button>
            </div>
            <pre><code>${esc(b.content || "")}</code></pre>
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
        return `<div class="block muted">[unknown block: ${esc(b?.kind ?? "null")}]</div>`;
    }
  }

  // Render plain text, preserving paragraph/line breaks and pulling fenced
  // ```code``` blocks into <pre>. Intentionally minimal (no full markdown).
  _renderText(text) {
    text = String(text ?? "");
    const parts = text.split(/(```[\s\S]*?```)/g);
    return parts.map((part) => {
      const fence = part.match(/^```([^\n]*)\n?([\s\S]*?)```$/);
      if (fence) {
        const lang = fence[1].trim();
        return `<pre class="code-fence"${lang ? ` data-lang="${esc(lang)}"` : ""}><code>${esc(fence[2])}</code></pre>`;
      }
      if (!part) return "";
      // Inline `code`, then escape, then line breaks.
      return part
        .split(/(`[^`]+`)/g)
        .map((seg) => {
          const inline = seg.match(/^`([^`]+)`$/);
          if (inline) return `<code class="inline">${esc(inline[1])}</code>`;
          return esc(seg).replace(/\n/g, "<br>");
        })
        .join("");
    }).join("");
  }
}

customElements.define("cv-transcript", CvTranscript);
