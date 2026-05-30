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

class CvTranscript extends HTMLElement {
  constructor() {
    super();
    this._session = null;
    // When true, render a "+" affordance on each message so a host (the loom)
    // can collect messages. Hidden by default.
    this._pickMode = false;
  }

  set session(s) { this._session = s || null; this.render(); }
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

    this.innerHTML = `
      ${this._headerHtml(s)}
      <div class="turns">${(s.messages || []).map((m, i) => this._messageHtml(m, i)).join("")}</div>
    `;

    // Export menu.
    this.querySelector("[data-export-json]")?.addEventListener("click", () => {
      downloadFile(`${slug(sessionLabel(s))}.opensession.json`,
        JSON.stringify(toOpenSession(s), null, 2), "application/json");
    });
    this.querySelector("[data-export-md]")?.addEventListener("click", () => {
      downloadFile(`${slug(sessionLabel(s))}.md`, toMarkdown(s), "text/markdown");
    });

    // Pick buttons (loom collection) — emit a bubbling event the host handles.
    this.querySelectorAll("[data-pick]").forEach((btn) => {
      btn.addEventListener("click", () => {
        const idx = Number(btn.dataset.pick);
        const m = (s.messages || [])[idx];
        if (m) this.dispatchEvent(new CustomEvent("pick-message", {
          detail: { session: s, message: m, index: idx }, bubbles: true,
        }));
      });
    });

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
        const pre = `<pre><code>${esc(input)}</code></pre>`;
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
          ? `<details class="tool-details"><summary class="muted">details</summary><pre><code>${esc(pretty(b.details))}</code></pre></details>`
          : "";
        const big = content.length > 600;
        const pre = `<pre><code>${esc(content)}</code></pre>`;
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
