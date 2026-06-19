// <cv-opensession> — an in-app presentation of the OpenSession standard.
// Featured on the site at pug's request. The gist is distilled from
// docs/OPENSESSION.md (kept in sync by hand — this is a static viewer).
import { esc } from "./util.js";

// Each principle: [icon, heading, body]. Icons are tiny inline glyphs.
const PRINCIPLES = [
  ["🦴", "A session is an ordered list of messages, plus provenance.",
   "That's the whole spine. Don't overthink it."],
  ["📍", "The working directory is metadata, never identity.",
   "The original sin of every harness is coupling a session to its cwd — in the filename, no less. OpenSession records cwd as a plain, freely-rewritable field. Portability is the default."],
  ["🧱", "Content is a list of typed blocks.",
   "A single assistant turn legitimately contains reasoning, prose, and several tool calls. A flat string can't represent that without lying."],
  ["🎭", "Four roles, normalized.",
   "system, user, assistant, tool. Several harnesses smuggle tool results into a user turn — OpenSession promotes them to tool so conversions don't misattribute them."],
  ["🧠", "Reasoning is first-class but may be opaque.",
   "Codex and Grok ship encrypted reasoning blobs. A thinking block carries plaintext when available and an opaque payload when not. Never pretend opaque data is readable."],
  ["🔗", "Tool calls and results are linked by id, not adjacency.",
   "Interleaving and parallel tool calls are real."],
  ["⚖️", "Lossy-but-honest beats lossless-but-brittle.",
   "A per-message extra bag preserves harness-specific fields, but the core stays small enough that every harness can fill it."],
  ["🩹", "Be tolerant on the way in.",
   "Real transcripts have corrupt lines and missing fields. A parser that rejects a session over one malformed line is worse than useless."],
];

const SCHEMA = `{
  "openSession": "0.2",            // format version
  "harness": "claude",             // origin: claude|codex|grok|opencode|gemini|hermes|openclaw|...
  "id": "da9174f4-…",              // session id (native if possible)
  "cwd": "/Users/ember/pug/x",     // METADATA, not identity — freely rewritable
  "title": "…",                    // human/AI label (optional)
  "model": "claude-opus-4-8",      // primary model (optional)
  "createdAt": "2026-05-29T21:…Z", // ISO-8601 (optional)
  "updatedAt": "2026-05-29T22:…Z",
  "git": { "branch": "main", "commit": "…", "remote": "…" },   // optional
  "extra": { },                    // session-level harness passthrough (optional)
  "messages": [
    {
      "id": "uuid",                // optional
      "parentId": "uuid|null",     // optional threading (DAG); omit for linear
      "role": "assistant",         // system|user|assistant|tool
      "timestamp": "…",            // optional
      "model": "…",                // optional, per-message
      "usage": { "inputTokens": 0, "outputTokens": 0,
                 "cacheReadTokens": 0, "cacheCreationTokens": 0 },
      "content": [                 // ordered, typed blocks
        { "kind": "thinking", "text": "…", "signature": "…", "encrypted": "…", "redacted": false },
        { "kind": "text", "text": "…" },
        { "kind": "toolUse", "id": "call_1", "name": "Bash", "input": { "command": "ls" } },
        { "kind": "toolResult", "toolUseId": "call_1", "content": "…", "isError": false },
        { "kind": "file", "mime": "application/pdf", "path": "spec.pdf", "source": "file:///…" },
        { "kind": "image", "mediaType": "image/png", "dataRef": "…" }
      ],
      "extra": { }                 // harness-specific passthrough (optional)
    }
  ]
}`;

// [kind, meaning, key fields, css-class-for-color]
const BLOCKS = [
  ["text", "plain prose", "text", "text"],
  ["thinking", "reasoning / chain-of-thought", "text, signature?, encrypted? (opaque), redacted?", "thinking"],
  ["toolUse", "a tool/function invocation", "id, name, input (arbitrary JSON)", "tooluse"],
  ["toolResult", "the result of one", "toolUseId, content, isError, toolName?, status?, details?", "toolresult"],
  ["file", "a file/resource attachment (never inlined bytes)", "mime?, path?, source?", "file"],
  ["image", "an image reference (never inlined bytes)", "mediaType?, dataRef?", "image"],
];

// Harnesses we ingest — shown as a wall of harness-colored badges.
const HARNESSES = [
  ["claude", "Claude"], ["codex", "Codex"], ["grok", "Grok"],
  ["opencode", "OpenCode"], ["gemini", "Gemini"], ["hermes", "Hermes"],
  ["openclaw", "OpenClaw"],
];

// A tiny, naive JSONC syntax highlighter for the schema block. It only ever
// runs over our own trusted SCHEMA constant, but we still esc() first and only
// ever wrap already-escaped, regex-recognized tokens in <span>, so no raw
// untrusted HTML can sneak through.
function highlightJsonc(src) {
  const safe = esc(src);
  // Order matters: comments first (they can contain quotes/braces), then strings,
  // then numbers/keywords/punctuation.
  return safe
    .replace(/(\/\/[^\n]*)/g, '<span class="os-c-com">$1</span>')
    .replace(/(&quot;(?:[^&]|&(?!quot;))*?&quot;)(\s*:)/g, '<span class="os-c-key">$1</span>$2')
    .replace(/(:\s*)(&quot;(?:[^&]|&(?!quot;))*?&quot;)/g, '$1<span class="os-c-str">$2</span>')
    .replace(/\b(true|false|null)\b/g, '<span class="os-c-lit">$1</span>')
    .replace(/(:\s*)(-?\d+(?:\.\d+)?)/g, '$1<span class="os-c-num">$2</span>');
}

// The IR tree diagram, rendered as an inline SVG. Color-coded by block kind so
// it matches the legend. This is the "spec brought to life": one example
// session's structure, Session → Message{role} → Block{kind}.
function irDiagram() {
  // node(x, y, w, label, sub, cls)
  const box = (x, y, w, h, cls, label, sub) => `
    <g class="os-node os-node-${cls}" transform="translate(${x} ${y})">
      <rect width="${w}" height="${h}" rx="9"></rect>
      <text class="os-node-label" x="${w / 2}" y="${sub ? 19 : 23}">${esc(label)}</text>
      ${sub ? `<text class="os-node-sub" x="${w / 2}" y="34">${esc(sub)}</text>` : ""}
    </g>`;
  // connector path from a parent's bottom-center to a child's left edge (elbow)
  const elbow = (x1, y1, x2, y2) =>
    `<path class="os-edge" d="M ${x1} ${y1} C ${x1} ${(y1 + y2) / 2}, ${x2 - 26} ${y2}, ${x2} ${y2}"/>`;

  return `
  <svg class="os-svg" viewBox="0 0 720 392" role="img"
       aria-label="OpenSession IR tree: a Session contains ordered Messages; each Message has a role and an ordered list of typed content Blocks.">
    <!-- edges (drawn first, under the nodes) -->
    ${elbow(110, 64, 250, 56)}
    ${elbow(110, 64, 250, 216)}
    ${elbow(360, 88, 500, 36)}
    ${elbow(360, 88, 500, 92)}
    ${elbow(360, 88, 500, 148)}
    ${elbow(360, 248, 500, 204)}
    ${elbow(360, 248, 500, 260)}
    ${elbow(360, 248, 500, 316)}

    <!-- Session root -->
    ${box(20, 40, 90, 48, "session", "Session", "+ provenance")}

    <!-- Messages -->
    ${box(250, 36, 110, 52, "user", "Message", "role: user")}
    ${box(250, 196, 110, 52, "assistant", "Message", "role: assistant")}

    <!-- Blocks of the user message -->
    ${box(500, 16, 200, 40, "text", "text", "“refactor the parser”")}

    <!-- Blocks of the assistant message -->
    ${box(500, 72, 200, 40, "thinking", "thinking", "encrypted • opaque")}
    ${box(500, 128, 200, 40, "text", "text", "“On it — running tests.”")}

    ${box(500, 184, 200, 40, "tooluse", "toolUse", "Bash · id: call_1")}
    ${box(500, 240, 200, 40, "toolresult", "toolResult", "→ call_1 · ok")}
    ${box(500, 296, 200, 40, "image", "image", "image/png · dataRef")}
  </svg>`;
}

class CvOpenSession extends HTMLElement {
  connectedCallback() {
    this.injectStyles();
    this.render();
    this.wire();
  }

  injectStyles() {
    if (document.getElementById("cv-opensession-styles")) return;
    const el = document.createElement("style");
    el.id = "cv-opensession-styles";
    el.textContent = STYLES;
    document.head.appendChild(el);
  }

  render() {
    this.innerHTML = `
      <div class="os-page">
        <header class="os-hero">
          <div class="os-hero-glow" aria-hidden="true"></div>
          <div class="os-emoji">🧬</div>
          <h2>OpenSession</h2>
          <p class="os-sub">An interchange format for agent sessions. <em>Draft 0.2 — a clustervision proposal.</em></p>
          <div class="os-pills" aria-label="harnesses clustervision ingests">
            ${HARNESSES.map(([k, label]) =>
              `<span class="os-pill" data-harness="${esc(k)}">${esc(label)}</span>`).join("")}
            <span class="os-pill os-pill-arrow" aria-hidden="true">→</span>
            <span class="os-pill" data-harness="opensession">OpenSession</span>
          </div>
        </header>

        <section class="os-intro">
          <p>Every coding-agent harness records its conversations, and <strong>every single one invented its own format</strong>:
          Claude threads by UUID, Codex emits an event stream (twice), Grok splits a session across four files in a
          percent-encoded directory, OpenCode stores one JSON file per message <em>and</em> per content part, Gemini uses
          opaque protobuf, Hermes uses SQLite, OpenClaw uses yet another JSONL dialect. We've parsed all of them — and the
          striking thing is <strong>how similar they actually are underneath</strong>.</p>
          <p>OpenSession writes those ideas down. It's the format the harnesses would have agreed on if they'd talked first.
          clustervision ingests any of them and can <strong>export OpenSession</strong> from any session or the loom.</p>
        </section>

        <section class="os-section">
          <h3><span class="os-kicker">the tree</span>One example session, brought to life</h3>
          <p class="muted os-diagram-cap">A <strong>Session</strong> is an ordered list of <strong>Messages</strong>; each
          carries a <strong>role</strong> and an ordered list of typed <strong>Blocks</strong>. Color matches the legend below.</p>
          <div class="os-diagram">${irDiagram()}</div>
        </section>

        <section class="os-section">
          <h3><span class="os-kicker">the lessons</span>Design principles, earned the hard way</h3>
          <p class="muted os-cards-hint">Tap a card to expand the reasoning.</p>
          <div class="os-principles" role="list">
            ${PRINCIPLES.map(([icon, h, b], i) => `
              <button class="os-card" type="button" role="listitem"
                      aria-expanded="false" data-card="${i}">
                <span class="os-card-icon" aria-hidden="true">${esc(icon)}</span>
                <span class="os-card-h">${esc(h)}</span>
                <span class="os-card-body">${esc(b)}</span>
                <span class="os-card-chevron" aria-hidden="true">▾</span>
              </button>`).join("")}
          </div>
        </section>

        <section class="os-section">
          <h3><span class="os-kicker">the shape</span>The schema <code class="os-ver">v0.2</code></h3>
          <div class="os-schema-card">
            <div class="os-schema-bar" aria-hidden="true">
              <span class="os-dot os-dot-r"></span><span class="os-dot os-dot-y"></span><span class="os-dot os-dot-g"></span>
              <span class="os-schema-name">session.opensession.json</span>
            </div>
            <pre class="os-schema"><code>${highlightJsonc(SCHEMA)}</code></pre>
          </div>
        </section>

        <section class="os-section">
          <h3><span class="os-kicker">the blocks</span>Block kinds</h3>
          <div class="os-blocks">
            ${BLOCKS.map(([k, m, f, cls]) => `
              <div class="os-block os-block-${esc(cls)}">
                <span class="os-block-dot" aria-hidden="true"></span>
                <code class="os-block-kind">${esc(k)}</code>
                <span class="os-block-meaning">${esc(m)}</span>
                <span class="os-block-fields">${esc(f)}</span>
              </div>`).join("")}
          </div>
          <p class="muted">New block kinds are additive; consumers MUST ignore kinds they don't recognize (forward-compatibility).</p>
        </section>

        <section class="os-section">
          <h3><span class="os-kicker">the edges</span>Deliberately not in v0.x</h3>
          <ul class="os-list">
            <li><strong>System prompts / tool schemas</strong> — huge, harness-specific, rarely portable (stash in <code>extra</code>).</li>
            <li><strong>Project context files</strong> (CLAUDE.md, MEMORY.md, AGENTS.md) — these live <em>next to</em> a session, not inside it.</li>
            <li><strong>Billing / cost</strong> — belongs in <code>extra</code> until there's demand.</li>
          </ul>
        </section>

        <p class="os-foot muted">Galaxy-brained by pug. If you ship a harness, please consider emitting OpenSession too —
        then everyone's sessions are portable by construction. 🤝 &nbsp;·&nbsp;
        <a class="repo-link" href="./openSession.html" target="_blank" rel="noopener">standalone page ↗</a></p>
      </div>
    `;
  }

  wire() {
    this.querySelectorAll(".os-card").forEach((card) => {
      card.addEventListener("click", () => {
        const open = card.getAttribute("aria-expanded") === "true";
        card.setAttribute("aria-expanded", open ? "false" : "true");
      });
    });
  }
}

const STYLES = `
.os-page { max-width: 860px; margin: 0 auto; padding: 8px 4px 56px; }

/* ---- hero ------------------------------------------------------------- */
.os-hero { position: relative; text-align: center; padding: 24px 16px 20px; }
.os-hero-glow {
  position: absolute; left: 50%; top: 50%; transform: translate(-50%, -50%);
  width: min(560px, 90%); height: 300px; pointer-events: none; z-index: 0;
  background: radial-gradient(closest-side,
    color-mix(in srgb, var(--accent) 30%, transparent), transparent);
  filter: blur(12px);
}
/* Everything EXCEPT the glow sits above it in normal flow — the glow must stay absolute (this rule
   was overriding its position:absolute, so its 300px height was shoving the content down). */
.os-hero > *:not(.os-hero-glow) { position: relative; z-index: 1; }
.os-emoji { font-size: 44px; line-height: 1; filter: drop-shadow(0 2px 10px color-mix(in srgb, var(--accent) 45%, transparent)); }
.os-hero h2 {
  margin: 10px 0 4px; font-size: 34px; letter-spacing: -0.5px;
  background: linear-gradient(120deg, var(--fg), var(--accent) 120%);
  -webkit-background-clip: text; background-clip: text; color: transparent;
}
.os-sub { color: var(--fg-muted); margin: 0 0 14px; }
.os-sub em { color: color-mix(in srgb, var(--accent) 70%, var(--fg)); font-style: normal; }

.os-pills { display: flex; flex-wrap: wrap; gap: 6px; justify-content: center; align-items: center; }
.os-pill {
  font: 600 11px/1 var(--sans); padding: 5px 9px; border-radius: 999px; color: #fff;
  box-shadow: 0 1px 0 rgba(0,0,0,.15) inset;
}
.os-pill[data-harness="claude"]   { background: var(--h-claude); }
.os-pill[data-harness="codex"]    { background: var(--h-codex); }
.os-pill[data-harness="grok"]     { background: var(--h-grok); }
.os-pill[data-harness="opencode"] { background: var(--h-opencode); color: #2a2000; }
.os-pill[data-harness="gemini"]   { background: var(--h-gemini); }
.os-pill[data-harness="hermes"]   { background: var(--h-hermes); }
.os-pill[data-harness="openclaw"] { background: var(--h-openclaw); }
.os-pill[data-harness="opensession"] {
  background: var(--h-opensession);
  box-shadow: 0 0 0 1px color-mix(in srgb, var(--accent) 60%, transparent),
              0 0 16px color-mix(in srgb, var(--accent) 55%, transparent);
}
.os-pill-arrow { background: transparent; color: var(--fg-muted); padding: 0 2px; font-size: 14px; box-shadow: none; }

/* ---- intro ------------------------------------------------------------ */
.os-intro { margin: 8px 0 4px; }
.os-intro p { line-height: 1.6; }

/* ---- sections / headings --------------------------------------------- */
.os-section { margin-top: 34px; }
.os-section h3 {
  display: flex; align-items: baseline; gap: 10px; flex-wrap: wrap;
  margin: 0 0 6px; font-size: 19px;
}
.os-kicker {
  font: 700 10px/1 var(--sans); letter-spacing: 1.5px; text-transform: uppercase;
  color: var(--accent); padding: 4px 8px; border-radius: 6px;
  background: color-mix(in srgb, var(--accent) 12%, transparent);
}
.os-ver {
  font: 600 12px var(--mono); color: var(--accent);
  background: color-mix(in srgb, var(--accent) 12%, transparent);
  padding: 1px 7px; border-radius: 999px;
}
.os-cards-hint, .os-diagram-cap { margin: 0 0 14px; font-size: 13px; }
.muted { color: var(--fg-muted); }

/* ---- IR diagram ------------------------------------------------------- */
.os-diagram {
  border: 1px solid var(--border); border-radius: var(--radius); padding: 12px;
  background:
    radial-gradient(120% 120% at 0% 0%, color-mix(in srgb, var(--accent) 7%, transparent), transparent 55%),
    var(--bg-elev);
  box-shadow: var(--shadow); overflow-x: auto;
}
.os-svg { width: 100%; min-width: 640px; height: auto; display: block; }
.os-edge {
  fill: none; stroke: var(--border-strong); stroke-width: 1.6;
  stroke-dasharray: 4 4;
}
.os-node rect {
  fill: var(--bg-sunk); stroke: var(--border-strong); stroke-width: 1.4;
}
.os-node-label { font: 600 12px var(--mono); fill: var(--fg); text-anchor: middle; }
.os-node-sub   { font: 11px var(--sans); fill: var(--fg-muted); text-anchor: middle; }
/* node accents per kind, using the same palette as the legend */
.os-node-session rect { fill: color-mix(in srgb, var(--accent) 22%, var(--bg-elev)); stroke: var(--accent); }
.os-node-session .os-node-label { fill: color-mix(in srgb, var(--accent) 80%, var(--fg)); }
.os-node-user rect       { stroke: var(--accent); }
.os-node-user .os-node-sub { fill: color-mix(in srgb, var(--accent) 70%, var(--fg-muted)); }
.os-node-assistant rect  { stroke: var(--h-codex); }
.os-node-assistant .os-node-sub { fill: color-mix(in srgb, var(--h-codex) 75%, var(--fg-muted)); }
.os-node-text rect       { stroke: color-mix(in srgb, var(--fg-muted) 70%, var(--border-strong)); }
.os-node-thinking rect   { stroke: var(--h-hermes); fill: color-mix(in srgb, var(--h-hermes) 10%, var(--bg-sunk)); }
.os-node-tooluse rect    { stroke: var(--h-codex); fill: color-mix(in srgb, var(--h-codex) 10%, var(--bg-sunk)); }
.os-node-toolresult rect { stroke: var(--h-opencode); fill: color-mix(in srgb, var(--h-opencode) 12%, var(--bg-sunk)); }
.os-node-image rect      { stroke: var(--h-gemini); fill: color-mix(in srgb, var(--h-gemini) 10%, var(--bg-sunk)); }

/* ---- principle cards -------------------------------------------------- */
.os-principles {
  display: grid; grid-template-columns: repeat(auto-fill, minmax(248px, 1fr)); gap: 12px;
}
.os-card {
  position: relative; text-align: left; cursor: pointer; font: inherit; color: inherit;
  display: grid; grid-template-columns: auto 1fr auto; grid-template-rows: auto auto;
  column-gap: 11px; align-items: start;
  padding: 14px 14px 14px 13px; border-radius: var(--radius);
  border: 1px solid var(--border); background: var(--bg-elev);
  box-shadow: var(--shadow); overflow: hidden;
}
.os-card::before {
  content: ""; position: absolute; left: 0; top: 0; bottom: 0; width: 3px;
  background: var(--accent); opacity: .55;
}
.os-card-icon {
  grid-row: 1; font-size: 20px; line-height: 1.1;
  filter: drop-shadow(0 1px 6px color-mix(in srgb, var(--accent) 40%, transparent));
}
.os-card-h { grid-row: 1; grid-column: 2; font-weight: 650; line-height: 1.35; align-self: center; }
.os-card-chevron {
  grid-row: 1; grid-column: 3; color: var(--fg-muted); font-size: 12px; align-self: center;
}
.os-card-body {
  grid-row: 2; grid-column: 2 / 4; min-height: 0; overflow: hidden;
  color: var(--fg-muted); font-size: 13.5px; line-height: 1.55;
  /* Collapsed by default; expand reveals the reasoning. max-height is reliable cross-browser
     (0fr grid rows don't collapse intrinsic-height content in WebKit). */
  max-height: 0; opacity: 0;
}
.os-card[aria-expanded="true"] .os-card-body { max-height: 22em; opacity: 1; margin-top: 9px; }
.os-card[aria-expanded="true"] .os-card-chevron { transform: rotate(180deg); }
.os-card[aria-expanded="true"] {
  border-color: color-mix(in srgb, var(--accent) 45%, var(--border));
  box-shadow: var(--shadow), 0 0 0 1px color-mix(in srgb, var(--accent) 22%, transparent),
              0 6px 26px color-mix(in srgb, var(--accent) 16%, transparent);
}
.os-card[aria-expanded="true"]::before { opacity: 1; box-shadow: 0 0 14px var(--accent); }
.os-card:hover { border-color: var(--border-strong); }
.os-card:hover::before { opacity: .9; }
.os-card:focus-visible { outline: 2px solid color-mix(in srgb, var(--accent) 60%, transparent); outline-offset: 2px; }
@media (prefers-reduced-motion: no-preference) {
  .os-card { transition: border-color .18s, box-shadow .25s; }
  .os-card-body { transition: max-height .3s ease, opacity .22s ease, margin-top .2s ease; }
  .os-card-chevron { transition: transform .25s ease; }
  .os-card::before { transition: opacity .2s, box-shadow .25s; }
  .os-pill { transition: transform .15s; }
  .os-pill:hover { transform: translateY(-1px); }
  .os-node rect, .os-block { transition: transform .15s; }
}

/* ---- schema card ------------------------------------------------------ */
.os-schema-card {
  border: 1px solid var(--border); border-radius: var(--radius); overflow: hidden;
  background: var(--bg-elev); box-shadow: var(--shadow);
}
.os-schema-bar {
  display: flex; align-items: center; gap: 7px; padding: 9px 13px;
  background: var(--bg-sunk); border-bottom: 1px solid var(--border);
}
.os-dot { width: 11px; height: 11px; border-radius: 50%; opacity: .85; }
.os-dot-r { background: #ec6a5e; } .os-dot-y { background: #f4bf4f; } .os-dot-g { background: #61c554; }
.os-schema-name { margin-left: 6px; font: 12px var(--mono); color: var(--fg-muted); }
.os-schema {
  margin: 0; padding: 16px 18px; overflow-x: auto;
  background:
    linear-gradient(color-mix(in srgb, var(--accent) 4%, transparent), transparent 40%),
    var(--bg-elev);
}
.os-schema code { font: 12.5px/1.65 var(--mono); color: var(--fg); }
.os-c-com  { color: var(--fg-muted); font-style: italic; }
.os-c-key  { color: var(--accent); }
.os-c-str  { color: color-mix(in srgb, var(--h-codex) 80%, var(--fg)); }
.os-c-num  { color: var(--h-grok); }
.os-c-lit  { color: var(--h-opencode); }

/* ---- block kinds ------------------------------------------------------ */
.os-blocks { display: grid; gap: 9px; }
.os-block {
  position: relative; display: grid; gap: 4px 12px; align-items: baseline;
  grid-template-columns: 14px 130px 1fr; grid-template-areas: "dot kind meaning" "dot kind fields";
  padding: 11px 14px; border: 1px solid var(--border); border-radius: var(--radius);
  background: var(--bg-elev); --bk: var(--accent);
}
.os-block-dot { grid-area: dot; width: 9px; height: 9px; border-radius: 50%; background: var(--bk);
  box-shadow: 0 0 9px color-mix(in srgb, var(--bk) 70%, transparent); align-self: center; }
.os-block-kind { grid-area: kind; font: 600 13px var(--mono); color: var(--bk); }
.os-block-meaning { grid-area: meaning; }
.os-block-fields { grid-area: fields; color: var(--fg-muted); font: 12px var(--mono); }
.os-block:hover { border-color: color-mix(in srgb, var(--bk) 45%, var(--border)); }
@media (prefers-reduced-motion: no-preference) { .os-block:hover { transform: translateX(2px); } }
.os-block-text       { --bk: color-mix(in srgb, var(--fg-muted) 60%, var(--fg)); }
.os-block-thinking   { --bk: var(--h-hermes); }
.os-block-tooluse    { --bk: var(--h-codex); }
.os-block-toolresult { --bk: var(--h-opencode); }
.os-block-file       { --bk: var(--h-claude); }
.os-block-image      { --bk: var(--h-gemini); }

/* ---- not-in / foot ---------------------------------------------------- */
.os-list { line-height: 1.7; padding-left: 20px; }
.os-list code, .os-intro code, .os-block code { font: .92em var(--mono); }
.os-foot { margin-top: 34px; padding-top: 16px; border-top: 1px solid var(--border); line-height: 1.6; }
.os-foot a { color: var(--accent); }

@media (max-width: 560px) {
  .os-block { grid-template-columns: 14px 1fr; grid-template-areas: "dot kind" "dot meaning" "dot fields"; }
}
`;

customElements.define("cv-opensession", CvOpenSession);
