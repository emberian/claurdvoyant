// <cv-fleet> — the Fleet Dashboard. 📡
//
// Connects to a running `cvd serve` HTTP API and live-displays the agent fleet
// by polling every ~2s. CORS is enabled server-side, so plain cross-origin
// `fetch` works from this static page.
//
// Endpoints (base URL default http://localhost:7777):
//   GET /api/health                 → { ok, version?, ... }
//   GET /api/sessions?limit=N       → Session[] (IR) — recent activity feed
//   GET /api/channels               → string[]  — board channel names
//   GET /api/board/<channel>        → BoardMessage[]  ({id,channel,from,ts,kind,body,tags,session_ref})
//   GET /api/claims/<channel>       → [{key,owner,expires_at}]  (distributed locks)
//   GET /api/who/<channel>          → string[]  — present agents (recent heartbeats)
//
// Renders: an active-agents strip, a live board feed, a claims table, and the
// recent-sessions activity feed. Auto-refresh with a pause toggle. If the API
// is unreachable, a friendly "start `cvd serve`" hint (and a static board .json
// loader) is shown instead.
//
// PERFORMANCE: the static shell (toolbar + section scaffolding) is built ONCE in
// connectedCallback. Each poll then patches only the sections whose data changed,
// diffing against the last-rendered payload (`this._last`) and skipping DOM writes
// when a section is unchanged. The board feed is capped to the most recent
// BOARD_CAP messages in the DOM (with a muted note when older ones are hidden).
import "./cv-harness-badge.js";
import { esc, fmtTime, sessionLabel, shortPath, HARNESS_LABELS } from "./util.js";

const BASE_LS = "cv-fleet-base";
const CHAN_LS = "cv-fleet-channel";
const DEFAULT_BASE = "http://localhost:7777";
const DEFAULT_CHANNEL = "fleet";
const POLL_MS = 2000;
const SESSION_LIMIT = 25;
const BOARD_CAP = 200; // most-recent board messages kept in the DOM

// ---- injected styles (swarm-safe: never touch styles.css) -----------------
// These AUGMENT the global .fleet-* rules already in styles.css with the app's
// amethyst polish — glows, gradient cards, a live pulse, harness-tinted accents.
// All motion is gated behind prefers-reduced-motion: no-preference.
const FLEET_STYLES = `
  /* gradient, glowing cards */
  cv-fleet .fleet-card {
    background: linear-gradient(160deg, color-mix(in srgb, var(--accent) 5%, var(--bg-elev)), var(--bg-elev));
    box-shadow: 0 1px 2px color-mix(in srgb, var(--accent) 8%, transparent),
                0 8px 26px -16px color-mix(in srgb, var(--accent) 40%, transparent);
    transition: box-shadow 0.25s ease, border-color 0.25s ease;
  }
  cv-fleet .fleet-card:hover {
    border-color: color-mix(in srgb, var(--accent) 35%, var(--border));
    box-shadow: 0 1px 2px color-mix(in srgb, var(--accent) 10%, transparent),
                0 10px 30px -14px color-mix(in srgb, var(--accent) 55%, transparent);
  }
  cv-fleet .fleet-card h3 {
    display: flex; align-items: center; gap: 8px;
  }
  cv-fleet .fleet-card h3::before {
    content: ""; width: 4px; height: 14px; border-radius: 3px; flex: none;
    background: linear-gradient(var(--accent), color-mix(in srgb, var(--accent) 45%, var(--h-grok)));
    box-shadow: 0 0 8px -1px color-mix(in srgb, var(--accent) 70%, transparent);
  }

  /* toolbar — a faint amethyst card */
  cv-fleet .fleet-toolbar {
    background: linear-gradient(120deg, color-mix(in srgb, var(--accent) 7%, var(--bg-elev)), var(--bg-elev));
    border-radius: var(--radius);
    box-shadow: 0 0 0 1px color-mix(in srgb, var(--accent) 10%, transparent);
  }
  cv-fleet .fleet-base:focus, cv-fleet .fleet-channel:focus {
    outline: none; border-color: var(--accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 22%, transparent);
  }

  /* connection dot — pulse when live */
  cv-fleet .fleet-dot { position: relative; }
  cv-fleet .fleet-dot-ok::after {
    content: ""; position: absolute; inset: 0; border-radius: 50%;
    box-shadow: 0 0 0 0 color-mix(in srgb, var(--ok) 60%, transparent);
  }

  /* presence chips — glow + a breathing pulse dot */
  cv-fleet .who-chip {
    background: color-mix(in srgb, var(--accent) 6%, var(--bg-sunk));
    box-shadow: 0 0 0 1px color-mix(in srgb, var(--accent) 10%, transparent);
    transition: transform 0.12s ease, box-shadow 0.2s ease;
  }
  cv-fleet .who-chip:hover {
    transform: translateY(-1px);
    box-shadow: 0 0 0 1px color-mix(in srgb, var(--accent) 25%, transparent),
                0 4px 14px -6px color-mix(in srgb, var(--accent) 60%, transparent);
  }
  cv-fleet .who-pulse {
    background: var(--ok);
    box-shadow: 0 0 6px -1px color-mix(in srgb, var(--ok) 80%, transparent);
  }
  cv-fleet .who-chip[data-harness="claude"] .who-pulse { background: var(--h-claude); box-shadow: 0 0 6px -1px var(--h-claude); }
  cv-fleet .who-chip[data-harness="codex"] .who-pulse { background: var(--h-codex); box-shadow: 0 0 6px -1px var(--h-codex); }
  cv-fleet .who-chip[data-harness="grok"] .who-pulse { background: var(--h-grok); box-shadow: 0 0 6px -1px var(--h-grok); }
  cv-fleet .who-chip[data-harness="opencode"] .who-pulse { background: var(--h-opencode); box-shadow: 0 0 6px -1px var(--h-opencode); }
  cv-fleet .who-chip[data-harness="gemini"] .who-pulse { background: var(--h-gemini); box-shadow: 0 0 6px -1px var(--h-gemini); }
  cv-fleet .who-chip[data-harness="hermes"] .who-pulse { background: var(--h-hermes); box-shadow: 0 0 6px -1px var(--h-hermes); }
  cv-fleet .who-chip[data-harness="openclaw"] .who-pulse { background: var(--h-openclaw); box-shadow: 0 0 6px -1px var(--h-openclaw); }

  /* board messages — soft lift + glow tinted by kind */
  cv-fleet .board-msg { transition: box-shadow 0.2s ease, transform 0.12s ease; }
  cv-fleet .board-msg:hover {
    transform: translateX(1px);
    box-shadow: 0 4px 16px -8px color-mix(in srgb, var(--accent) 50%, transparent);
  }
  cv-fleet .board-kind-event:hover { box-shadow: 0 4px 16px -8px color-mix(in srgb, var(--accent) 60%, transparent); }
  cv-fleet .board-kind-status:hover { box-shadow: 0 4px 16px -8px color-mix(in srgb, var(--h-codex) 60%, transparent); }
  cv-fleet .board-kind-request:hover { box-shadow: 0 4px 16px -8px color-mix(in srgb, var(--warn) 60%, transparent); }

  /* muted "older hidden" note under a capped feed */
  cv-fleet .board-trunc { font-size: 11px; padding: 4px 2px 0; text-align: center; opacity: 0.8; }

  /* recent-session rows */
  cv-fleet .fleet-session { transition: border-color 0.2s ease, box-shadow 0.2s ease; }
  cv-fleet .fleet-session:hover {
    border-color: color-mix(in srgb, var(--accent) 30%, var(--border));
    box-shadow: 0 4px 14px -8px color-mix(in srgb, var(--accent) 45%, transparent);
  }

  @media (prefers-reduced-motion: no-preference) {
    cv-fleet .fleet-dot-ok::after { animation: cv-fleet-ping 2.4s ease-out infinite; }
    cv-fleet .who-pulse { animation: cv-fleet-breathe 2.2s ease-in-out infinite; }
  }
  @keyframes cv-fleet-ping {
    0%   { box-shadow: 0 0 0 0 color-mix(in srgb, var(--ok) 55%, transparent); }
    70%  { box-shadow: 0 0 0 7px color-mix(in srgb, var(--ok) 0%, transparent); }
    100% { box-shadow: 0 0 0 0 color-mix(in srgb, var(--ok) 0%, transparent); }
  }
  @keyframes cv-fleet-breathe {
    0%, 100% { opacity: 0.55; transform: scale(0.85); }
    50%      { opacity: 1;    transform: scale(1.1); }
  }
`;

function injectStyles() {
  if (document.getElementById("cv-fleet-styles")) return;
  const el = document.createElement("style");
  el.id = "cv-fleet-styles";
  el.textContent = FLEET_STYLES;
  document.head.appendChild(el);
}

class CvFleet extends HTMLElement {
  constructor() {
    super();
    this._base = this._read(BASE_LS) || DEFAULT_BASE;
    this._channel = this._read(CHAN_LS) || DEFAULT_CHANNEL;
    this._paused = false;
    this._timer = null;
    this._inflight = false;

    // Last-known data + connection state.
    this._state = "idle"; // idle | ok | error
    this._health = null;
    this._err = "";
    this._channels = [];
    this._board = [];
    this._claims = [];
    this._who = [];
    this._sessions = [];
    this._lastUpdate = 0;
    this._staticMode = false; // loaded a board .json instead of live polling

    // Shell built? + per-section signatures of what's currently in the DOM, so
    // each poll can diff and skip untouched sections.
    this._shell = false;
    this._mode = null; // "dash" | "offline" — which content view is mounted
    this._last = {};   // section → signature string of last-rendered data
  }

  connectedCallback() {
    injectStyles();
    this.render();
    if (!this._paused) this._start();
  }
  disconnectedCallback() { this._stop(); }

  _read(k) { try { return localStorage.getItem(k) || ""; } catch { return ""; } }
  _write(k, v) { try { localStorage.setItem(k, v); } catch { /* ignore */ } }

  // ---- polling lifecycle -------------------------------------------------

  _start() {
    this._stop();
    this._staticMode = false;
    this._poll();
    this._timer = setInterval(() => { if (!this._paused) this._poll(); }, POLL_MS);
  }
  _stop() {
    if (this._timer) { clearInterval(this._timer); this._timer = null; }
    // Abort any in-flight poll so its late resolution can't clobber the UI
    // after we've torn down / reconnected.
    if (this._ctrl) { try { this._ctrl.abort(); } catch { /* ignore */ } this._ctrl = null; }
    this._inflight = false;
  }

  async _fetchJson(path, signal) {
    const url = this._base.replace(/\/+$/, "") + path;
    const resp = await fetch(url, { signal, headers: { Accept: "application/json" } });
    if (!resp.ok) throw new Error(`${path} → HTTP ${resp.status}`);
    return resp.json();
  }

  async _poll() {
    if (this._inflight || this._staticMode) return;
    this._inflight = true;
    const ctrl = new AbortController();
    this._ctrl = ctrl;
    const to = setTimeout(() => ctrl.abort(), POLL_MS + 1500);
    try {
      const health = await this._fetchJson("/api/health", ctrl.signal);
      this._health = health || { ok: true };
      this._state = "ok";
      this._err = "";

      // Channels (to populate the picker), then the per-channel data + sessions.
      const [channels, sessions] = await Promise.all([
        this._fetchJson("/api/channels", ctrl.signal).catch(() => []),
        this._fetchJson(`/api/sessions?limit=${SESSION_LIMIT}`, ctrl.signal).catch(() => []),
      ]);
      this._channels = Array.isArray(channels) ? channels : [];
      this._sessions = this._asSessions(sessions);

      const ch = encodeURIComponent(this._channel);
      const [board, claims, who] = await Promise.all([
        this._fetchJson(`/api/board/${ch}`, ctrl.signal).catch(() => []),
        this._fetchJson(`/api/claims/${ch}`, ctrl.signal).catch(() => []),
        this._fetchJson(`/api/who/${ch}`, ctrl.signal).catch(() => []),
      ]);
      this._board = this._asBoard(board);
      this._claims = this._asClaims(claims);
      this._who = this._asWho(who);

      this._lastUpdate = Date.now();
    } catch (err) {
      // If this poll was superseded (reconnect/teardown aborted it), drop it
      // silently — its late result must not clobber the current view.
      if (this._ctrl !== ctrl) { clearTimeout(to); return; }
      this._state = "error";
      this._err = err?.name === "AbortError" ? "request timed out" : (err?.message || String(err));
    } finally {
      clearTimeout(to);
      // Only the current poll clears the inflight flag and repaints.
      if (this._ctrl === ctrl) {
        this._inflight = false;
        this._ctrl = null;
        this.render();
      }
    }
  }

  // ---- response normalizers (tolerant of small shape differences) --------

  _asSessions(data) {
    const arr = Array.isArray(data) ? data : Array.isArray(data?.sessions) ? data.sessions : [];
    return arr;
  }
  _asBoard(data) {
    const arr = Array.isArray(data) ? data : Array.isArray(data?.messages) ? data.messages : [];
    return arr.filter((m) => m && typeof m === "object");
  }
  _asClaims(data) {
    // Accept [{key,owner,expires_at}] OR a map {key: {owner,expires_at}} OR
    // tuples [[key,owner,expires_at]].
    if (Array.isArray(data)) {
      return data.map((c) => Array.isArray(c)
        ? { key: c[0], owner: c[1], expires_at: c[2] }
        : { key: c.key, owner: c.owner, expires_at: c.expires_at ?? c.expiresAt });
    }
    if (data && typeof data === "object") {
      return Object.entries(data).map(([key, v]) =>
        typeof v === "object" ? { key, owner: v.owner, expires_at: v.expires_at ?? v.expiresAt } : { key, owner: String(v) });
    }
    return [];
  }
  _asWho(data) {
    if (Array.isArray(data)) return data.map((w) => (typeof w === "string" ? w : w?.from || w?.name || String(w)));
    if (Array.isArray(data?.who)) return data.who;
    return [];
  }

  // ---- rendering ---------------------------------------------------------
  //
  // render() builds the shell once, then patches sections incrementally. It is
  // safe to call on every poll: untouched sections are diffed away.

  render() {
    if (!this._shell) this._buildShell();
    this._patchToolbar();

    const wantMode = (this._state === "error" && !this._staticMode) ? "offline" : "dash";
    const content = this._content;
    if (this._mode !== wantMode) {
      // Switch the mounted content view. This is the only place innerHTML is
      // used for content, and only when the *kind* of view changes.
      this._mode = wantMode;
      this._last = {}; // invalidate section signatures for the new view
      content.innerHTML = wantMode === "offline" ? this._offlineHtml() : this._dashShellHtml();
      this._wireContent();
    }

    if (wantMode === "offline") {
      this._patchOffline();
    } else {
      this._patchWho();
      this._patchClaims();
      this._patchBoard();
      this._patchSessions();
    }
  }

  _buildShell() {
    this.innerHTML = `
      <div class="view-head">
        <h2>📡 Fleet</h2>
        <span class="muted">Live view of a running <code>cvd serve</code> — agents, board, claims, recent sessions.</span>
      </div>
      <div class="fleet-toolbar">
        <span class="fleet-dot" title="connection"></span>
        <label class="fleet-field">
          <span class="muted">base URL</span>
          <input class="fleet-base" type="text" placeholder="${DEFAULT_BASE}" aria-label="cvd serve base URL" />
        </label>
        <label class="fleet-field">
          <span class="muted">channel</span>
          <select class="fleet-channel" aria-label="board channel"></select>
        </label>
        <button type="button" class="mini-btn" data-fleet-connect>reconnect</button>
        <button type="button" class="mini-btn" data-fleet-pause aria-pressed="false">⏸ pause</button>
        <button type="button" class="mini-btn" data-fleet-refresh title="Poll now">⟳</button>
        <span class="fleet-stamp muted">—</span>
        <span class="fleet-ver muted" title="server version" hidden></span>
      </div>
      <div class="fleet-content"></div>
    `;
    this._shell = true;
    this._content = this.querySelector(".fleet-content");
    // The base-URL <input> value is owned by the user while focused; set it once.
    const baseEl = this.querySelector(".fleet-base");
    if (baseEl) baseEl.value = this._base;
    this._wireToolbar();
  }

  // Patch the live status bits of the always-present toolbar without rebuilding
  // its inputs (which would steal focus / clobber the channel selection).
  _patchToolbar() {
    const dot = this._staticMode ? "static" : this._state;
    const dotEl = this.querySelector(".fleet-dot");
    if (dotEl) dotEl.className = `fleet-dot fleet-dot-${esc(dot)}`;

    const stamp = this._lastUpdate
      ? `updated ${fmtTime(new Date(this._lastUpdate).toISOString()).replace(/^.*?, /, "")}`
      : "—";
    const stampEl = this.querySelector(".fleet-stamp");
    const stampText = this._staticMode ? "static board" : stamp;
    if (stampEl && stampEl.textContent !== stampText) stampEl.textContent = stampText;

    const verEl = this.querySelector(".fleet-ver");
    if (verEl) {
      const v = this._health?.version;
      if (v) { verEl.textContent = `v${v}`; verEl.hidden = false; }
      else { verEl.hidden = true; }
    }

    const connectEl = this.querySelector("[data-fleet-connect]");
    if (connectEl) connectEl.textContent = this._staticMode ? "go live" : "reconnect";
    const pauseEl = this.querySelector("[data-fleet-pause]");
    if (pauseEl) {
      pauseEl.textContent = this._paused ? "▶ resume" : "⏸ pause";
      pauseEl.setAttribute("aria-pressed", String(this._paused));
    }

    // Channel <select>: rebuild options only when the set changes, preserving
    // the current selection (don't disturb an open dropdown otherwise).
    const sel = this.querySelector(".fleet-channel");
    if (sel) {
      const opts = [...new Set([this._channel, DEFAULT_CHANNEL, ...this._channels])].filter(Boolean);
      const sig = opts.join("") + "|" + this._channel;
      if (this._last.chan !== sig) {
        this._last.chan = sig;
        sel.innerHTML = opts
          .map((c) => `<option value="${esc(c)}"${c === this._channel ? " selected" : ""}>${esc(c)}</option>`)
          .join("");
        sel.value = this._channel;
      }
    }
  }

  // ---- offline view ------------------------------------------------------

  _offlineHtml() {
    return `
      <div class="fleet-offline">
        <div class="fo-glyph">🛰️</div>
        <h3>Can't reach the fleet API</h3>
        <p class="fo-err muted"></p>
        <p>Start the daemon's HTTP API and make sure CORS is enabled:</p>
        <pre><code>cvd serve --addr 127.0.0.1:7777</code></pre>
        <p class="muted">Then click <strong>reconnect</strong>. The dashboard polls
        <code>/api/health</code>, <code>/api/sessions</code>, <code>/api/channels</code>,
        <code class="fo-ep-board"></code>, <code class="fo-ep-claims"></code>,
        and <code class="fo-ep-who"></code> every ${POLL_MS / 1000}s.</p>
        <div class="fo-static">
          <span class="muted">No server handy? Load a static board export:</span>
          <input type="file" class="fleet-static-input" accept=".json,application/json" aria-label="Load static board JSON" />
        </div>
      </div>`;
  }

  _patchOffline() {
    const sig = `${this._base}${this._err}${this._channel}`;
    if (this._last.offline === sig) return;
    this._last.offline = sig;
    const errEl = this._content.querySelector(".fo-err");
    if (errEl) errEl.innerHTML = `<code>${esc(this._base)}</code> — ${esc(this._err || "no response")}.`;
    const setEp = (cls, txt) => { const e = this._content.querySelector(cls); if (e) e.textContent = txt; };
    setEp(".fo-ep-board", `/api/board/${this._channel}`);
    setEp(".fo-ep-claims", `/api/claims/${this._channel}`);
    setEp(".fo-ep-who", `/api/who/${this._channel}`);
  }

  // ---- dashboard shell + per-section patches -----------------------------

  _dashShellHtml() {
    return `
      <div class="fleet-grid">
        <section class="fleet-card fleet-who">
          <h3>Active agents <span class="muted fleet-who-n"></span></h3>
          <div class="fleet-who-body"></div>
        </section>
        <section class="fleet-card fleet-claims">
          <h3>Claims <span class="muted fleet-claims-n"></span></h3>
          <div class="fleet-claims-body"></div>
        </section>
        <section class="fleet-card fleet-board">
          <h3>#<span class="fleet-board-ch"></span> board <span class="muted fleet-board-n"></span></h3>
          <div class="fleet-board-body"></div>
        </section>
        <section class="fleet-card fleet-sessions">
          <h3>Recent sessions <span class="muted fleet-sessions-n"></span></h3>
          <div class="fleet-sessions-body"></div>
        </section>
      </div>`;
  }

  _patchWho() {
    const sig = JSON.stringify(this._who);
    const countEl = this._content.querySelector(".fleet-who-n");
    if (countEl) countEl.textContent = `(${this._who.length})`;
    if (this._last.who === sig) return;
    this._last.who = sig;
    this._content.querySelector(".fleet-who-body").innerHTML = this._whoHtml();
  }

  _patchClaims() {
    // Claims include live expiry labels, so re-render whenever the data OR the
    // "soon/stale" state could have shifted — key off the raw data plus a coarse
    // time bucket so the countdown stays roughly current without per-frame churn.
    const sig = JSON.stringify(this._claims) + "|" + Math.floor(Date.now() / 5000);
    const countEl = this._content.querySelector(".fleet-claims-n");
    if (countEl) countEl.textContent = `(${this._claims.length})`;
    if (this._last.claims === sig) return;
    this._last.claims = sig;
    this._content.querySelector(".fleet-claims-body").innerHTML = this._claimsHtml();
  }

  _patchBoard() {
    const chEl = this._content.querySelector(".fleet-board-ch");
    if (chEl) chEl.textContent = this._channel;
    const countEl = this._content.querySelector(".fleet-board-n");
    if (countEl) countEl.textContent = `(${this._board.length})`;

    const sig = this._channel + "|" + JSON.stringify(this._board);
    if (this._last.board === sig) return;
    const prevSig = this._last.board;
    this._last.board = sig;

    const body = this._content.querySelector(".fleet-board-body");
    // Preserve scroll position across the re-render. If the user is at the top
    // (newest, since the feed is newest-first), keep them pinned there.
    const feed = body.querySelector(".board-feed");
    const prevTop = feed ? feed.scrollTop : 0;
    const wasPinned = prevTop <= 2;

    body.innerHTML = this._boardHtml();

    const newFeed = body.querySelector(".board-feed");
    if (newFeed && prevSig != null) {
      newFeed.scrollTop = wasPinned ? 0 : prevTop;
    }
  }

  _patchSessions() {
    const sig = JSON.stringify(this._sessions.map((s) => [
      s.id, s.harness, sessionLabel(s), s.cwd, s.updated_at || s.created_at, (s.messages || []).length,
    ]));
    const countEl = this._content.querySelector(".fleet-sessions-n");
    if (countEl) countEl.textContent = `(${this._sessions.length})`;
    if (this._last.sessions === sig) return;
    this._last.sessions = sig;
    this._content.querySelector(".fleet-sessions-body").innerHTML = this._sessionsHtml();
  }

  // ---- section HTML builders ---------------------------------------------

  _whoHtml() {
    if (!this._who.length) return `<p class="muted fleet-empty">No agents heartbeating on <code>#${esc(this._channel)}</code>.</p>`;
    return `<div class="who-strip">${this._who.map((a) => {
      const h = this._harnessOf(a);
      return `<span class="who-chip" data-harness="${esc(h)}" title="${esc(a)}"><span class="who-pulse"></span>${esc(a)}</span>`;
    }).join("")}</div>`;
  }

  _claimsHtml() {
    if (!this._claims.length) return `<p class="muted fleet-empty">No active claims — nothing locked.</p>`;
    const now = Date.now();
    const rows = this._claims.map((c) => {
      const exp = c.expires_at ? new Date(c.expires_at).getTime() : 0;
      const left = exp ? exp - now : 0;
      const soon = left > 0 && left < 30000;
      const expLabel = exp ? (left > 0 ? this._dur(left) + " left" : "expired") : "—";
      return `
        <tr class="${left <= 0 ? "claim-stale" : soon ? "claim-soon" : ""}">
          <td class="claim-key" title="${esc(c.key)}">${esc(c.key)}</td>
          <td class="claim-owner"><span class="who-chip mini" data-harness="${esc(this._harnessOf(c.owner))}">${esc(c.owner || "?")}</span></td>
          <td class="claim-exp muted">${esc(expLabel)}</td>
        </tr>`;
    }).join("");
    return `<table class="claims-table"><thead><tr><th>key</th><th>owner</th><th>expires</th></tr></thead><tbody>${rows}</tbody></table>`;
  }

  _boardHtml() {
    if (!this._board.length) return `<p class="muted fleet-empty">No messages on <code>#${esc(this._channel)}</code> yet.</p>`;
    // Newest first, capped to the most recent BOARD_CAP for DOM health.
    const all = this._board.slice().reverse();
    const hidden = Math.max(0, all.length - BOARD_CAP);
    const msgs = hidden ? all.slice(0, BOARD_CAP) : all;
    const note = hidden
      ? `<li class="board-trunc muted" aria-hidden="true">… ${hidden} older message${hidden === 1 ? "" : "s"} hidden (showing newest ${BOARD_CAP})</li>`
      : "";
    return `<ul class="board-feed">${msgs.map((m) => {
      const when = m.ts ? fmtTime(m.ts).replace(/^.*?, /, "") : "";
      const kind = (m.kind || "msg").toLowerCase();
      const body = m.body || (kind === "presence" ? "· heartbeat ·" : kind === "ack" ? "· ack ·" : "");
      const tags = (m.tags || []).map((t) => `<span class="board-tag">${esc(t)}</span>`).join("");
      return `
        <li class="board-msg board-kind-${esc(kind)}">
          <div class="bm-head">
            <span class="bm-from" data-harness="${esc(this._harnessOf(m.from))}">${esc(m.from || "?")}</span>
            <span class="bm-kind">${esc(kind)}</span>
            <span class="bm-when muted">${esc(when)}</span>
          </div>
          ${body ? `<div class="bm-body">${esc(body)}</div>` : ""}
          ${tags ? `<div class="bm-tags">${tags}</div>` : ""}
        </li>`;
    }).join("")}${note}</ul>`;
  }

  _sessionsHtml() {
    if (!this._sessions.length) return `<p class="muted fleet-empty">No recent sessions reported.</p>`;
    return `<ul class="fleet-session-feed">${this._sessions.map((s) => {
      const h = (s.harness || "").toLowerCase();
      const when = fmtTime(s.updated_at || s.created_at);
      const n = (s.messages || []).length;
      return `
        <li class="fleet-session">
          <cv-harness-badge harness="${esc(h)}"></cv-harness-badge>
          <div class="fs-body">
            <div class="fs-title" title="${esc(sessionLabel(s))}">${esc(sessionLabel(s))}</div>
            <div class="fs-meta muted">
              ${s.cwd ? `<span class="fs-cwd" title="${esc(s.cwd)}">${esc(shortPath(s.cwd, 3))}</span>` : ""}
              ${n ? `<span>${n} msg</span>` : ""}
              ${when ? `<span>${esc(when.replace(/^.*?, /, ""))}</span>` : ""}
            </div>
          </div>
        </li>`;
    }).join("")}</ul>`;
  }

  // Heuristic: derive a harness color from an agent/from string if it names one.
  _harnessOf(s) {
    const l = String(s || "").toLowerCase();
    for (const h of Object.keys(HARNESS_LABELS)) if (l.includes(h)) return h;
    return "";
  }

  _dur(ms) {
    const sec = Math.round(ms / 1000);
    if (sec < 60) return `${sec}s`;
    const min = Math.floor(sec / 60);
    if (min < 60) return `${min}m`;
    return `${Math.floor(min / 60)}h${min % 60 ? ` ${min % 60}m` : ""}`;
  }

  // ---- wiring ------------------------------------------------------------
  //
  // Toolbar listeners are bound once (the toolbar is built once). Content
  // listeners (offline file input) are (re)bound whenever the content view
  // is swapped.

  _wireToolbar() {
    const baseEl = this.querySelector(".fleet-base");
    const chanEl = this.querySelector(".fleet-channel");
    baseEl?.addEventListener("change", () => { this._base = baseEl.value.trim() || DEFAULT_BASE; this._write(BASE_LS, this._base); });
    baseEl?.addEventListener("keydown", (e) => { if (e.key === "Enter") { this._base = baseEl.value.trim() || DEFAULT_BASE; this._write(BASE_LS, this._base); this._start(); } });
    chanEl?.addEventListener("change", () => {
      this._channel = chanEl.value;
      this._write(CHAN_LS, this._channel);
      if (!this._staticMode) this._poll();
    });

    this.querySelector("[data-fleet-connect]")?.addEventListener("click", () => {
      this._base = (this.querySelector(".fleet-base")?.value || this._base).trim() || DEFAULT_BASE;
      this._write(BASE_LS, this._base);
      this._paused = false;
      this._start();
    });
    this.querySelector("[data-fleet-pause]")?.addEventListener("click", () => {
      this._paused = !this._paused;
      if (this._paused) this._stop();
      else this._start();
      this.render();
    });
    this.querySelector("[data-fleet-refresh]")?.addEventListener("click", () => {
      if (this._staticMode) this._start(); else this._poll();
    });
  }

  _wireContent() {
    const fileEl = this._content.querySelector(".fleet-static-input");
    fileEl?.addEventListener("change", async () => {
      const f = fileEl.files?.[0];
      if (!f) return;
      try {
        const data = JSON.parse(await f.text());
        this._loadStatic(data);
      } catch (e) {
        this._err = "Could not parse that JSON: " + (e?.message || e);
        this.render();
      }
    });
  }

  // Load a static board export: accepts an array of BoardMessages, or an object
  // { board?, channels?, claims?, who?, sessions?, channel? }.
  _loadStatic(data) {
    this._stop();
    this._staticMode = true;
    this._paused = true;
    this._state = "ok";
    this._err = "";
    if (Array.isArray(data)) {
      this._board = this._asBoard(data);
    } else if (data && typeof data === "object") {
      if (data.channel) { this._channel = data.channel; this._write(CHAN_LS, this._channel); }
      this._channels = Array.isArray(data.channels) ? data.channels : this._channels;
      this._board = this._asBoard(data.board ?? data.messages ?? []);
      this._claims = this._asClaims(data.claims ?? []);
      this._who = this._asWho(data.who ?? []);
      this._sessions = this._asSessions(data.sessions ?? []);
    }
    this._lastUpdate = Date.now();
    this.render();
  }
}

customElements.define("cv-fleet", CvFleet);
