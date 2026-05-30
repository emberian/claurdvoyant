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
import "./cv-harness-badge.js";
import { esc, fmtTime, sessionLabel, shortPath, HARNESS_LABELS } from "./util.js";

const BASE_LS = "cv-fleet-base";
const CHAN_LS = "cv-fleet-channel";
const DEFAULT_BASE = "http://localhost:7777";
const DEFAULT_CHANNEL = "fleet";
const POLL_MS = 2000;
const SESSION_LIMIT = 25;

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
  }

  connectedCallback() {
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

  render() {
    this.innerHTML = `
      <div class="view-head">
        <h2>📡 Fleet</h2>
        <span class="muted">Live view of a running <code>cvd serve</code> — agents, board, claims, recent sessions.</span>
      </div>
      ${this._toolbarHtml()}
      ${this._state === "error" && !this._staticMode ? this._offlineHtml() : this._dashHtml()}
    `;
    this._wire();
  }

  _toolbarHtml() {
    const dot = this._staticMode ? "static" : this._state;
    const stamp = this._lastUpdate ? `updated ${fmtTime(new Date(this._lastUpdate).toISOString()).replace(/^.*?, /, "")}` : "—";
    const chanOpts = [...new Set([this._channel, DEFAULT_CHANNEL, ...this._channels])]
      .filter(Boolean)
      .map((c) => `<option value="${esc(c)}"${c === this._channel ? " selected" : ""}>${esc(c)}</option>`).join("");
    return `
      <div class="fleet-toolbar">
        <span class="fleet-dot fleet-dot-${esc(dot)}" title="connection"></span>
        <label class="fleet-field">
          <span class="muted">base URL</span>
          <input class="fleet-base" type="text" value="${esc(this._base)}" placeholder="${DEFAULT_BASE}" aria-label="cvd serve base URL" />
        </label>
        <label class="fleet-field">
          <span class="muted">channel</span>
          <select class="fleet-channel" aria-label="board channel">${chanOpts}</select>
        </label>
        <button type="button" class="mini-btn" data-fleet-connect>${this._staticMode ? "go live" : "reconnect"}</button>
        <button type="button" class="mini-btn" data-fleet-pause aria-pressed="${this._paused}">${this._paused ? "▶ resume" : "⏸ pause"}</button>
        <button type="button" class="mini-btn" data-fleet-refresh title="Poll now">⟳</button>
        <span class="fleet-stamp muted">${this._staticMode ? "static board" : esc(stamp)}</span>
        ${this._health?.version ? `<span class="fleet-ver muted" title="server version">v${esc(String(this._health.version))}</span>` : ""}
      </div>`;
  }

  _offlineHtml() {
    return `
      <div class="fleet-offline">
        <div class="fo-glyph">🛰️</div>
        <h3>Can't reach the fleet API</h3>
        <p class="muted"><code>${esc(this._base)}</code> — ${esc(this._err || "no response")}.</p>
        <p>Start the daemon's HTTP API and make sure CORS is enabled:</p>
        <pre><code>cvd serve --addr 127.0.0.1:7777</code></pre>
        <p class="muted">Then click <strong>reconnect</strong>. The dashboard polls
        <code>/api/health</code>, <code>/api/sessions</code>, <code>/api/channels</code>,
        <code>/api/board/${esc(this._channel)}</code>, <code>/api/claims/${esc(this._channel)}</code>,
        and <code>/api/who/${esc(this._channel)}</code> every ${POLL_MS / 1000}s.</p>
        <div class="fo-static">
          <span class="muted">No server handy? Load a static board export:</span>
          <input type="file" class="fleet-static-input" accept=".json,application/json" aria-label="Load static board JSON" />
        </div>
      </div>`;
  }

  _dashHtml() {
    return `
      <div class="fleet-grid">
        <section class="fleet-card fleet-who">
          <h3>Active agents <span class="muted">(${this._who.length})</span></h3>
          ${this._whoHtml()}
        </section>
        <section class="fleet-card fleet-claims">
          <h3>Claims <span class="muted">(${this._claims.length})</span></h3>
          ${this._claimsHtml()}
        </section>
        <section class="fleet-card fleet-board">
          <h3>#${esc(this._channel)} board <span class="muted">(${this._board.length})</span></h3>
          ${this._boardHtml()}
        </section>
        <section class="fleet-card fleet-sessions">
          <h3>Recent sessions <span class="muted">(${this._sessions.length})</span></h3>
          ${this._sessionsHtml()}
        </section>
      </div>`;
  }

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
    // Newest first.
    const msgs = this._board.slice().reverse();
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
    }).join("")}</ul>`;
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

  _wire() {
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

    const fileEl = this.querySelector(".fleet-static-input");
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
