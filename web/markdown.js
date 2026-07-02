// markdown.js — a small, dependency-free, XSS-safe Markdown → HTML renderer.
//
// Scope is deliberately a useful subset of CommonMark, tuned for agent
// transcripts: ATX headings, fenced + indented-free inline code, bullet/ordered
// lists (one level), blockquotes, horizontal rules, bold/italic/strikethrough,
// links (http/https/mailto only), and paragraphs with soft line breaks.
//
// SECURITY: every piece of source text is escaped *before* any markup is
// injected. We never insert untrusted text without escaping it first, and the
// only attributes we emit are a sanitized href and class names we control.

const ESC_RE = /[&<>"']/g;
const ESC_MAP = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" };
function esc(s) { return String(s ?? "").replace(ESC_RE, (c) => ESC_MAP[c]); }

// Only allow a safe set of URL schemes; everything else becomes inert.
function safeUrl(href) {
  const h = String(href ?? "").trim();
  if (/^(https?:|mailto:)/i.test(h)) return h;
  // Protocol-relative (`//evil.com`, and `/\` / `\\` variants browsers
  // normalize the same way) would navigate off-site — reject before the
  // relative-path checks below, which they'd otherwise satisfy.
  if (/^[/\\]{2}/.test(h)) return "";
  if (/^[^:]*$/.test(h) || h.startsWith("#") || h.startsWith("/") || h.startsWith("./") || h.startsWith("../")) return h;
  return "";
}

// ---- inline rendering ------------------------------------------------------
// Operates on a single line of already-trimmed source. We protect inline code
// spans first (escaping their contents and never parsing markup inside them),
// then run the remaining text through escape + emphasis/link transforms.

function renderInline(src) {
  const tokens = [];
  let rest = String(src ?? "");
  // 1) pull out `code spans` (and ``code with ` ticks``) as opaque tokens.
  const codeRe = /(`+)([\s\S]*?)\1/g;
  let last = 0, m;
  let out = "";
  while ((m = codeRe.exec(rest))) {
    out += inlineMarkup(rest.slice(last, m.index));
    out += `<code class="inline">${esc(m[2].replace(/^ | $/g, ""))}</code>`;
    last = m.index + m[0].length;
  }
  out += inlineMarkup(rest.slice(last));
  return out;
}

// Emphasis, links, autolinks, strikethrough — on a code-free fragment.
function inlineMarkup(text) {
  // Links: [label](url "title"?)  — label may contain escaped markup.
  // We process links before escaping the surrounding text by splitting.
  const linkRe = /\[([^\]]+)\]\(([^)\s]+)(?:\s+"[^"]*")?\)/g;
  let out = "";
  let last = 0, m;
  while ((m = linkRe.exec(text))) {
    out += emphasize(esc(text.slice(last, m.index)));
    const url = safeUrl(m[2]);
    const label = emphasize(esc(m[1]));
    out += url
      ? `<a href="${esc(url)}" target="_blank" rel="noopener noreferrer ugc">${label}</a>`
      : label;
    last = m.index + m[0].length;
  }
  out += emphasize(esc(text.slice(last)));
  return out;
}

// Bold / italic / strikethrough on already-escaped text (so the markers we look
// for can't have come from the source as literal HTML).
function emphasize(s) {
  return s
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/__([^_]+)__/g, "<strong>$1</strong>")
    .replace(/(^|[^*])\*([^*\s][^*]*?)\*(?!\*)/g, "$1<em>$2</em>")
    .replace(/(^|[^_])_([^_\s][^_]*?)_(?!_)/g, "$1<em>$2</em>")
    .replace(/~~([^~]+)~~/g, "<del>$1</del>");
}

// ---- tiny code highlighter -------------------------------------------------
// Not a real tokenizer — a tasteful, language-agnostic pass that wraps strings,
// comments, numbers, and a common keyword set in spans. All input is escaped
// first; we only ever wrap escaped text in <span class> we control.

const KEYWORDS = new Set((
  "fn let const var function return if else for while loop match impl struct enum trait pub use mod " +
  "class def import from export default async await yield new this self super extern crate where " +
  "type interface public private static void int string bool true false null nil none undefined " +
  "and or not in is as with try catch finally throw raise then end do begin"
).split(" "));

function highlight(code, lang) {
  // Split on strings/comments first so we don't tokenize inside them.
  const parts = [];
  const re = /("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`(?:[^`\\]|\\.)*`|\/\/[^\n]*|#[^\n]*|\/\*[\s\S]*?\*\/)/g;
  let last = 0, m;
  while ((m = re.exec(code))) {
    if (m.index > last) parts.push(["code", code.slice(last, m.index)]);
    const tok = m[0];
    const cls = tok.startsWith("//") || tok.startsWith("#") || tok.startsWith("/*") ? "c" : "s";
    parts.push([cls, tok]);
    last = m.index + tok.length;
  }
  if (last < code.length) parts.push(["code", code.slice(last)]);

  return parts.map(([kind, txt]) => {
    if (kind === "s") return `<span class="hl-str">${esc(txt)}</span>`;
    if (kind === "c") return `<span class="hl-com">${esc(txt)}</span>`;
    // plain code: a single tokenizing pass so injected spans never get re-scanned.
    // We match identifiers and numbers; everything else (punctuation, spaces) is
    // escaped verbatim. Because we escape each token as we emit it and never
    // re-scan our own output, keywords inside attribute strings can't collide.
    return txt.replace(/([A-Za-z_$][\w$]*)|(0x[0-9a-fA-F]+|\d+(?:\.\d+)?)|([^A-Za-z0-9_$]+)/g,
      (_, ident, num, other) => {
        if (ident != null) return KEYWORDS.has(ident) ? `<span class="hl-kw">${ident}</span>` : ident;
        if (num != null) return `<span class="hl-num">${num}</span>`;
        return esc(other);
      });
  }).join("");
}

/** Render a single fenced code block to HTML (used directly by transcripts). */
export function renderCodeBlock(code, lang) {
  const langAttr = lang ? ` data-lang="${esc(lang)}"` : "";
  return `<pre class="code-fence"${langAttr}><code>${highlight(String(code ?? ""), lang)}</code></pre>`;
}

// ---- block rendering -------------------------------------------------------

export function renderMarkdown(src) {
  const text = String(src ?? "").replace(/\r\n?/g, "\n");
  const lines = text.split("\n");
  const html = [];
  let i = 0;

  const flushPara = (buf) => {
    if (!buf.length) return;
    const joined = buf.join("\n").trim();
    if (joined) html.push(`<p>${renderInline(joined).replace(/\n/g, "<br>")}</p>`);
    buf.length = 0;
  };

  let para = [];
  while (i < lines.length) {
    const line = lines[i];

    // Fenced code block.
    const fence = line.match(/^\s*(`{3,}|~{3,})\s*([^\n`]*)$/);
    if (fence) {
      flushPara(para);
      const marker = fence[1][0];
      const lang = fence[2].trim();
      const body = [];
      i++;
      while (i < lines.length && !new RegExp(`^\\s*${marker}{3,}\\s*$`).test(lines[i])) {
        body.push(lines[i]); i++;
      }
      i++; // consume closing fence (or EOF)
      html.push(renderCodeBlock(body.join("\n"), lang));
      continue;
    }

    // ATX heading.
    const heading = line.match(/^(#{1,6})\s+(.*)$/);
    if (heading) {
      flushPara(para);
      const level = heading[1].length;
      html.push(`<h${level} class="md-h">${renderInline(heading[2].trim())}</h${level}>`);
      i++;
      continue;
    }

    // Horizontal rule.
    if (/^\s*([-*_])(?:\s*\1){2,}\s*$/.test(line)) {
      flushPara(para);
      html.push(`<hr class="md-hr" />`);
      i++;
      continue;
    }

    // Blockquote (collect consecutive > lines).
    if (/^\s*>\s?/.test(line)) {
      flushPara(para);
      const quote = [];
      while (i < lines.length && /^\s*>\s?/.test(lines[i])) {
        quote.push(lines[i].replace(/^\s*>\s?/, "")); i++;
      }
      html.push(`<blockquote class="md-quote">${renderMarkdown(quote.join("\n"))}</blockquote>`);
      continue;
    }

    // Lists (ordered / unordered). One level; nested content folded in.
    const ulItem = line.match(/^(\s*)[-*+]\s+(.*)$/);
    const olItem = line.match(/^(\s*)(\d+)[.)]\s+(.*)$/);
    if (ulItem || olItem) {
      flushPara(para);
      const ordered = !!olItem;
      const items = [];
      while (i < lines.length) {
        const u = lines[i].match(/^(\s*)[-*+]\s+(.*)$/);
        const o = lines[i].match(/^(\s*)(\d+)[.)]\s+(.*)$/);
        if (ordered && o) { items.push(o[3]); i++; }
        else if (!ordered && u) { items.push(u[2]); i++; }
        else if (lines[i].trim() === "") { break; }
        else if (/^\s{2,}\S/.test(lines[i]) && items.length) {
          // continuation line for the previous item
          items[items.length - 1] += "\n" + lines[i].trim(); i++;
        } else break;
      }
      const tag = ordered ? "ol" : "ul";
      const start = ordered && olItem[2] !== "1" ? ` start="${esc(olItem[2])}"` : "";
      html.push(`<${tag} class="md-list"${start}>${
        items.map((it) => `<li>${renderInline(it.trim())}</li>`).join("")
      }</${tag}>`);
      continue;
    }

    // Blank line separates paragraphs.
    if (line.trim() === "") { flushPara(para); i++; continue; }

    // Otherwise: accumulate into the current paragraph.
    para.push(line);
    i++;
  }
  flushPara(para);
  return html.join("\n");
}
