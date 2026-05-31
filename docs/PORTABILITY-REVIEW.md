# Port-a-sesh correctness review — live round-trip (2026-05-31)

Goal: verify that `cv convert` produces sessions that the **target harness's own binary**
can `--resume` and that **Basically Work** (the prior conversation is in the model's
context). Method: plant an unguessable passphrase ("VELVET-OTTER-7731") + a number (8842)
in a source session, convert, resume the target CLI headlessly, and ask it to recall them.
A correct port recalls; a broken one answers UNKNOWN.

Harnesses tested live (real installed binaries): **claude-code 2.1.158**, **codex-cli 0.131.0**,
**grok (Grok Build) 0.1.219**. Priority directed pairs: claude↔codex, grok↔codex.

## Result matrix (after fixes)

| Direction            | Discovery | Context recall | Notes |
|----------------------|-----------|----------------|-------|
| claude → codex       | ✅        | ✅ `CODE / NUMBER` | needed fix #1 |
| codex → claude       | ✅        | ✅ (transcript-confirmed) | symlink caveat below |
| codex → grok         | ✅        | ✅                 | needed fix #2 |
| grok → codex         | ✅        | ✅                 | relies on fix #1 |
| codex → grok (real, w/ reasoning) | ✅ | ✅       | needed fix #3 |
| claude → grok        | ✅        | ✅                 | confirms claude↔grok |
| claude → hermes      | ✅ (loads)| n/a (see #4/#5)    | needed fix #4; model caveat |
| hermes → codex       | ✅        | ✅                 | hermes parser is correct |

All four priority directed pairs **failed before** these fixes and **pass after**.
Second round added claude→grok, and the hermes pair (sqlite state.db).

## Bugs found & fixed (all live-validated)

### Fix #1 — codex emit dropped the model-visible conversation  *(crates/cv-core/src/emit.rs, emit_codex)*
Codex stores two parallel streams: `response_item` records (what's replayed into the model
on resume) and `event_msg` records (UI transcript only). Our emitter wrote **user and
assistant text only as `event_msg`** — so a resumed codex session reconstructed a context
with *zero* user prompts and *zero* assistant replies. Proven: pre-fix probe → `UNKNOWN`;
after emitting `response_item/message` (user→`input_text`, assistant→`output_text`) →
recalls the passphrase. The codex *parser* already dedups the dual representation
(`has_events`), so round-trip message counts are unaffected (18/18 emit tests still pass).

### Fix #2 — grok emit produced an undiscoverable session  *(emit_grok summary.json)*
`grok --resume` returned **"Session does not exist"** for our output. Root cause: our
`summary.json` omitted fields the loader requires (notably `chat_format_version`; real
summaries also carry `num_messages`, `next_trace_turn`, `grok_home`, `last_active_at`,
`agent_name`). Isolation test: a byte-complete real session relocated to a fresh id under a
new cwd resumed fine ("ALIVE"), so discovery is by-scan — the blocker was the malformed
summary. After writing the full field set, grok discovers and resumes. (Discovery is by
URL-encoded cwd dir; see caveat.)

### Fix #3 — cross-provider encrypted reasoning broke grok resume  *(emit_grok reasoning)*
Grok rejected ported history with *"Could not decrypt the provided encrypted_content … This
session's conversation history is incompatible with the current model."* OpenAI's
`encrypted_content` reasoning blobs are provider/account-bound and meaningless to xAI's
backend. Fix: only carry the `encrypted` blob for a **Grok→Grok** port (`session.harness ==
Harness::Grok`); otherwise emit the plain reasoning text and drop the blob. Also: default
`current_model_id` to `grok-build` unless the source model is itself a grok model (a foreign
model id would replay history against an incompatible backend). Validated on a **real** codex
session containing reasoning → grok resumes and quotes the first message verbatim, no error.

### Fix #4 — hermes emit couldn't write into an existing state.db  *(emit_hermes)*
`cv convert --to hermes` (no `--out`) failed with *"creating /Users/.../state.db: File exists"*
whenever hermes was actually installed. Cause: the hermes adapter's `storage_root()` returns the
**state.db file path**, but `emit_hermes` treated `out_dir` as a directory and called
`create_dir_all` on it. Fix: detect whether `out_dir` is the db file (is_file / `.db` ext) and use
it directly, creating only its parent. After this, claude/codex→hermes emit the session + messages
+ FTS rows correctly and hermes discovers and loads them (verified: a 4-message port appears in
`sessions`/`messages`, and hermes→codex round-trips the needle).

### Fix #5 — hermes resume hint printed the wrong flag  *(emit_hermes resume_hint)*
Hint said `hermes --session <id>`; the actual resume flag is `hermes --resume <id>`. Corrected.

## Caveats / not-yet-closed (honest list)

- **hermes resume needs a usable model.** Hermes reads the session row's `model` and sends it as
  `modelId`; a model-less source (no `session.model`) → empty `modelId` → API validation error, and
  hermes does NOT fall back to a default (tested both `''` and `NULL`). Real ports carry the source
  model (e.g. codex→hermes writes `gpt-5.2-codex`), so this only bites model-less sources — but that
  ported model must be one the user's hermes is authed/configured for, or they must pass `-m`. Worth
  emitting a default or warning when `session.model` is None.

- **Symlink/realpath jail (claude target).** Claude resolves cwd via realpath; macOS `/tmp`
  → `/private/tmp`. Porting to a symlinked path writes `~/.claude/projects/-tmp-…` while
  claude looks under `-private-tmp-…` → "No conversation found". `cv` should canonicalize the
  target cwd (or warn) when emitting claude. Likely also affects `/var`, `/home` symlinks.
- **Not tested live:** claude↔grok direct, all hermes pairs, and full real claude→codex
  sessions containing tool calls (only synthetic text + one real reasoning session covered).
  emit_codex already routes tool_use→`function_call` / tool_result→`function_call_output` as
  response_items, so those *should* be fine, but it's unverified end-to-end.
- **Claude usage limit** was hit mid-review; codex→claude recall was confirmed from the
  appended transcript rather than a fresh probe.

## Test methodology (reusable)
1. Synthetic source with planted needle (or a real session with a known first message).
2. `cv convert <id> --from X --to Y --cwd <non-symlink sandbox>`.
3. `codex exec --sandbox read-only resume <id> "<probe>"` / `grok -p "<probe>" -r <id>` /
   `claude -p "<probe>" --resume <id>` — run from the sandbox cwd.
4. Compare recall vs. a no-resume control.
