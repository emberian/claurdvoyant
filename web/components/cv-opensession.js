// <cv-opensession> — an in-app presentation of the OpenSession standard.
// Featured on the site at pug's request. The gist is distilled from
// docs/OPENSESSION.md (kept in sync by hand — this is a static viewer).
import { esc } from "./util.js";

const PRINCIPLES = [
  ["A session is an ordered list of messages, plus provenance.", "That's the whole spine. Don't overthink it."],
  ["The working directory is metadata, never identity.", "The original sin of every harness is coupling a session to its cwd — in the filename, no less. OpenSession records cwd as a plain, freely-rewritable field. Portability is the default."],
  ["Content is a list of typed blocks.", "A single assistant turn legitimately contains reasoning, prose, and several tool calls. A flat string can't represent that without lying."],
  ["Four roles, normalized.", "system, user, assistant, tool. Several harnesses smuggle tool results into a user turn — OpenSession promotes them to tool so conversions don't misattribute them."],
  ["Reasoning is first-class but may be opaque.", "Codex and Grok ship encrypted reasoning blobs. A thinking block carries plaintext when available and an opaque payload when not. Never pretend opaque data is readable."],
  ["Tool calls and results are linked by id, not adjacency.", "Interleaving and parallel tool calls are real."],
  ["Lossy-but-honest beats lossless-but-brittle.", "A per-message extra bag preserves harness-specific fields, but the core stays small enough that every harness can fill it."],
  ["Be tolerant on the way in.", "Real transcripts have corrupt lines and missing fields. A parser that rejects a session over one malformed line is worse than useless."],
];

const SCHEMA = `{
  "openSession": "0.1",            // format version
  "harness": "claude",             // origin: claude|codex|grok|opencode|gemini|hermes|openclaw|...
  "id": "da9174f4-…",              // session id (native if possible)
  "cwd": "/Users/ember/pug/x",     // METADATA, not identity — freely rewritable
  "title": "…",                    // human/AI label (optional)
  "model": "claude-opus-4-8",      // primary model (optional)
  "createdAt": "2026-05-29T21:…Z", // ISO-8601 (optional)
  "updatedAt": "2026-05-29T22:…Z",
  "git": { "branch": "main", "commit": "…", "remote": "…" },   // optional
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
        { "kind": "thinking", "text": "…", "signature": "…", "encrypted": "…" },
        { "kind": "text", "text": "…" },
        { "kind": "toolUse", "id": "call_1", "name": "Bash", "input": { "command": "ls" } },
        { "kind": "toolResult", "toolUseId": "call_1", "content": "…", "isError": false },
        { "kind": "image", "mediaType": "image/png", "dataRef": "…" }
      ],
      "extra": { }                 // harness-specific passthrough (optional)
    }
  ]
}`;

const BLOCKS = [
  ["text", "plain prose", "text"],
  ["thinking", "reasoning / chain-of-thought", "text, signature?, encrypted? (opaque blob)"],
  ["toolUse", "a tool/function invocation", "id, name, input (arbitrary JSON)"],
  ["toolResult", "the result of one", "toolUseId, content, isError"],
  ["image", "an image reference (never inlined bytes)", "mediaType?, dataRef?"],
];

class CvOpenSession extends HTMLElement {
  connectedCallback() { this.render(); }

  render() {
    this.innerHTML = `
      <div class="os-page">
        <header class="os-hero">
          <div class="os-emoji">🧬</div>
          <h2>OpenSession</h2>
          <p class="os-sub">An interchange format for agent sessions. <em>Draft 0.1 — a claurdvoyant proposal.</em></p>
        </header>

        <section class="os-intro">
          <p>Every coding-agent harness records its conversations, and <strong>every single one invented its own format</strong>:
          Claude threads by UUID, Codex emits an event stream (twice), Grok splits a session across four files in a
          percent-encoded directory, OpenCode stores one JSON file per message <em>and</em> per content part, Gemini uses
          opaque protobuf, Hermes uses SQLite, OpenClaw uses yet another JSONL dialect. We've parsed all of them — and the
          striking thing is <strong>how similar they actually are underneath</strong>.</p>
          <p>OpenSession writes those ideas down. It's the format the harnesses would have agreed on if they'd talked first.
          claurdvoyant ingests any of them and can <strong>export OpenSession</strong> from any session or the loom.</p>
        </section>

        <section class="os-section">
          <h3>Design principles — the lessons, earned the hard way</h3>
          <ol class="os-principles">
            ${PRINCIPLES.map(([h, b]) => `<li><strong>${esc(h)}</strong><span class="muted"> ${esc(b)}</span></li>`).join("")}
          </ol>
        </section>

        <section class="os-section">
          <h3>The schema (v0.1)</h3>
          <pre class="os-schema"><code>${esc(SCHEMA)}</code></pre>
        </section>

        <section class="os-section">
          <h3>Block kinds</h3>
          <table class="os-table">
            <thead><tr><th>kind</th><th>meaning</th><th>key fields</th></tr></thead>
            <tbody>
              ${BLOCKS.map(([k, m, f]) => `<tr><td><code>${esc(k)}</code></td><td>${esc(m)}</td><td class="muted">${esc(f)}</td></tr>`).join("")}
            </tbody>
          </table>
          <p class="muted">New block kinds are additive; consumers MUST ignore kinds they don't recognize (forward-compatibility).</p>
        </section>

        <section class="os-section">
          <h3>Not in v0.1</h3>
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
}

customElements.define("cv-opensession", CvOpenSession);
