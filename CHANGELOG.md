# Changelog

## v0.9.20 — the takeover audit (2026-07-02)

A fresh set of eyes (Claude Fable 5) read the whole codebase, then a swarm of
agents fixed what the review found — in parallel, in one tree, using cv to read
its own sessions along the way. Everything below shipped with regression tests;
the full workspace suite, clippy, the wasm target, and the real-corpus
invariant tests are green.

### Security

- **`cvd serve` no longer trusts the whole internet.** The wildcard
  `Access-Control-Allow-Origin: *` — which let any website a user visited fetch
  their entire transcript corpus off `127.0.0.1:7777` — is gone. CORS is now an
  allow-list (tauri + same-host origins, echoed with `Vary: Origin`); the
  `Host` header is validated on loopback binds (kills DNS rebinding); binding a
  non-loopback address requires `--token`/`$CVD_TOKEN` or an explicit
  `--insecure-expose`; `/api/*` supports bearer-token auth; workers are
  panic-isolated.
- **Redaction got real teeth.** Truncated PEM bodies (the common
  clipped-tool-output case) are now caught; new token families: Stripe
  `sk_live_`/`rk_live_`, GitLab `glpat-`, Slack `xoxc-/xoxs-/xapp-`, `npm_`,
  HuggingFace `hf_`, Groq `gsk_`, xAI `xai-`, DigitalOcean `dop_v1_`, Shopify
  `shpat_/shpss_`; connection-string passwords (`scheme://user:pass@host`);
  case-insensitive `bearer`; `Proxy-Authorization`; `git.remote` and
  session/message `extra` maps are scrubbed. Root-cause fix: the keyword-blob
  scanner only ever matched blobs at end-of-input — quoted mid-sentence secrets
  now redact. Assignment matching no longer mangles code like
  `let token = get_token();`.
- **The web viewer bounds zip extraction** (256 MB/entry, 512 MB total,
  header sizes distrusted) and markdown links reject protocol-relative
  `//evil.com` navigation.
- **`cv distill`/`loom --generate` print an egress notice** naming the
  provider, model, and payload size before any transcript leaves the machine.

### Index integrity

- A parse error mid-session no longer commits truncated index docs stamped
  fresh-forever; the error path deletes the partial docs.
- Plain `cv index` no longer silently deletes the sub-agent forest folded in by
  a previous `cv index --subagents`.
- An FTS-fresh but events-stale sub-agent no longer loses its search docs.
- The event catalog keys freshness on `(mtime, size)` like the FTS index — a
  mass mtime bump no longer triggers a whole-corpus re-parse.
- `cv search` without an index caps its in-memory haystack (256 KB/session)
  instead of materializing multi-GB sessions; with a stale index, "no matches"
  now says how far behind the index is.
- Semantic search validates the stored model id and vector dimensions instead
  of silently ranking garbage after a model switch.
- A failed index open during *search* propagates the error instead of deleting
  and recreating the index directory.

### Prune / resume safety

- `--window N` now honors its contract: the kept tail is the largest one
  **≤ N** real tokens (it previously overshot by up to one turn), with a loud
  warning when a single turn alone exceeds the budget.
- Revive derives its "honest" context figure from recorded usage deltas
  (byte-estimate only as a fallback floor) and never rewrites records below
  what the evidence supports — a revived session can no longer sail through the
  resume gate and blow the real context limit.
- Sub-agent sidechain records no longer poison window sizing, revive
  arithmetic, or re-root selection.
- Sidecar payload ids are unique (no more unretrievable payloads behind
  colliding `unknown` ids); `--retrieve` errors on duplicates instead of
  silently returning the last one.
- Windowing keeps the session title record and records trailing the final
  turn; `--drop` markers no longer advertise a retrieval that can't happen.
- Pruning parses each line once instead of ~6× (roughly half the peak memory
  on the GB-scale sessions prune exists for).

### Conversion fidelity

- **`cv convert`/`cv port` actually verify now**: the previously-dead
  `emit_verified` machinery runs on every conversion and prints what the
  target format loses.
- Same-harness ports parse in format-complete mode: Claude→Claude ports replay
  system records (compact boundaries, slash-commands, hooks) with parent chains
  re-linked — no more dangling `parentUuid`s in a rehomed transcript.
- →Codex: the model now survives (emitted as a real `turn_context` record, not
  a misused `model_provider` field) and `is_error` tool results round-trip
  (object form with `success:false`).
- →Claude: user turns with images/files/mixed content emit faithful array
  content instead of being flattened to text.
- `serde_json/preserve_order` is on workspace-wide: ChatGPT exports missing
  `current_node` and Zed multi-tool-result messages no longer get shuffled
  into key-sorted order.
- One emit registry: `Adapter::emit`/`can_emit` (never called, frequently
  lying) are gone; the dispatch table in `emit.rs` is the single source of
  truth and `supported_targets()` derives from it.
- Emitting a lazily-parsed session materializes spans first instead of
  panicking or serializing span structs into the output.

### Core

- Query calculus: bare URLs are needles (with a did-you-mean guard for real
  typos); quoted phrases with commas stay literal; `until:`/`since:` work;
  `updated:2026-06-01` means the whole day, not exactly-midnight; `msgs`
  counts user+assistant consistently across prefilter and full match.
- Board claims use real advisory file locks (kernel-released on process
  death) — the 20-second lock-steal path that could crown two winners is gone.
- The Claude sniffer tolerates transcripts opening with long runs of meta
  records; discovery timestamps are min/max rather than first/last (clock-skew
  robustness); `find()`'s slow path filters vanished files like the fast path;
  the claude seek path re-checks file size after opening (mirroring codex).
- MCP server: requests dispatch concurrently — a 120s `await_omen` no longer
  blocks every other tool call; `list_sessions`/`search_sessions`/
  `observe_stream`/`project_sessions` use the catalog fast path instead of a
  full fleet scan; malformed numeric args are protocol errors instead of
  silent defaults.

### App / UX

- Transcripts from dropped files render windowed (200 at a time) instead of
  freezing the tab on 30k-message sessions; chunk wiring is O(chunk) not
  O(session); large tool blocks have copy buttons; the activity heatmap clamps
  to 6 years so one 1970-dated session can't explode the DOM.
- `cv prune --range` accepts the same grammar as `cv show --range`;
  `cv ls --harness` help lists the full harness set; prune warnings surface in
  the CLI and MCP results.

## v0.9.18 and earlier

See git tags.
