# Harness session formats (reverse-engineered)

This is the ground-truth catalog of how each harness stores sessions on disk, reverse-engineered from a
real machine (2026-05-29). It drives the adapters in `cv-core`. Keep it accurate; it is the spec.

The unifying insight: **all harnesses encode the working directory (cwd) into where/how they store a
session.** That cwd-coupling is exactly why sessions are "dir-jailed" and hard to find. claurdvoyant
decouples them via a unified IR.

---

## Claude Code — `~/.claude/`

- **Transcripts:** `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl` (one session per file, JSONL).
- **cwd encoding (dir name):** leading `-`, then `/` → `-`, and `.` → `-`. **Lossy / not reversible**
  (original `-` and `.` collide). → *Do not decode the dir name; read `cwd` from inside the transcript.*
- **Per-line `type` values:** `user`, `assistant`, `attachment`, `mode`, `permission-mode`, `ai-title`,
  `last-prompt`, plus `summary`.
- **Threading:** every `user`/`assistant`/`attachment` line has `uuid` + `parentUuid` (null at root) →
  a linked list / DAG. `last-prompt.leafUuid` points at the tail.
- **Message line fields:** `message.role`, `message.content` (string for simple user msgs; array of blocks
  `{type: text|tool_use|thinking}` for assistant; `tool_result` blocks for tool returns). Also `cwd`,
  `gitBranch`, `version`, `sessionId`, `timestamp` (ISO-8601), `model` (assistant), `usage` (tokens).
- **Tool results:** carried on a `user` line as `content[].type=="tool_result"` (`tool_use_id`, `content`,
  `is_error`) plus a richer `toolUseResult` sidecar object.
- **Subagents:** `<sessionId>/subagents/agent-<id>.jsonl` (+ `.meta.json` with `agentType`,`description`).
- **Indexes/sidecars:** `~/.claude/history.jsonl` (`{display,timestamp(ms),project,sessionId}`),
  `~/.claude/sessions/<pid>.json` (live process state), `~/.claude/tasks/<sessionId>/<n>.json` (todos).

## Codex CLI — `~/.codex/`

- **Transcripts (modern):** `~/.codex/sessions/YYYY/MM/DD/rollout-<ISO>-<ULID>.jsonl`. Archived under
  `~/.codex/archived_sessions/`. **Legacy (2025):** single JSON file `{session, items[]}`.
- **Session id:** ULIDv7 (e.g. `019bddb0-5c24-76c3-a09b-af12207f3b00`).
- **JSONL line types:** `session_meta` (first line: `payload.{id,cwd,cli_version,instructions,model_provider,
  git{commit_hash,branch}}`), `response_item` (`payload.type` ∈ message | function_call |
  function_call_output | custom_tool_call | reasoning | token_count), `event_msg`
  (`user_message`/`agent_message`), `turn_context` (per-turn `{cwd,model,sandbox_policy,...}`).
- **Message content:** `content[].type=="input_text"` (+ output_text). **Reasoning** is summarized text +
  `encrypted_content` (opaque). Tool calls: `function_call{name,arguments(JSON string),call_id}` paired
  with `function_call_output{call_id,output}`.
- **Index (sqlite):** `~/.codex/state_5.sqlite` → `threads(id, rollout_path, cwd, title, created_at,
  updated_at, model, git_branch, tokens_used, archived, preview, ...)`. Also `session_index.jsonl`
  (`{id, thread_name, updated_at}`) and `history.jsonl` (`{session_id, ts, text}`).

## Grok CLI — `~/.grok/`

- **Transcripts:** `~/.grok/sessions/<percent-encoded-cwd>/<sessionId>/` — a *directory* per session
  containing `chat_history.jsonl`, `events.jsonl`, `updates.jsonl`, `summary.json`, `system_prompt.txt`.
- **cwd encoding:** percent-encoding of the absolute cwd (`%2F` = `/`). Reversible.
- **session id:** UUIDv7. **chat_history.jsonl line:** `{type: system|user|assistant, content}` where
  user `content` is `[{type:text,text}]`, assistant `content` is a string and carries
  `reasoning{text,encrypted,id}`, `model_id`, `model_fingerprint`.
- **summary.json:** `{info{id,cwd}, created_at, updated_at, num_messages, current_model_id, git_root_dir,
  git_remotes[], head_commit, head_branch, agent_name, ...}`.
- **Indexes (sqlite):** `~/.grok/sessions/session_search.sqlite` (FTS5: `session_docs` + `session_docs_fts`
  over title+content), `~/.grok/worktrees.db` (`worktrees(id,path,session_id,...)`), and per-cwd
  `prompt_history.jsonl` (`{timestamp,session_id,prompt,is_bash}`).

## OpenCode — `~/.local/share/opencode/` (NOT `~/.opencode`, which is plugins/cache)

- **Storage:** `~/.local/share/opencode/storage/session/<...>.json` (session records) and
  `storage/message/<sessionID>/<messageID>.json` (one file per message). Also `~/.local/state/opencode/`
  and `prompt-history.jsonl`.
- **Message record:** `{id: msg_*, sessionID: ses_*, role, summary{title,body,diffs[]}, ...}` plus model info.

## Gemini / Antigravity — `~/.gemini/`

- **Antigravity transcripts:** `~/.gemini/antigravity/conversations/<uuid>.pb` — **protobuf, opaque**
  (no .proto on disk). Best-effort only.
- **Readable fallback:** `~/.gemini/tmp/<hash>/logs.json` — array of `{sessionId, messageId, type, message,
  timestamp}` (user messages reliably; assistant inconsistently).
- **Sidecars:** `~/.gemini/antigravity/brain/<conv-id>/.system_generated/logs/overview.txt` (summary),
  `knowledge/`, `context_state/`, `implicit/`.
- Note: open-source `gemini-cli` (cloned at `~/pug/gemini-cli`) uses JSON checkpoints, a *different* format
  from the closed Antigravity IDE — consult its `packages/cli/src/utils/sessions.ts` for that variant.

---

## Prior art (don't reinvent; do unify)

- `claude-code-transcripts` (Rust, lib.rs) — typed parser, **Claude only**, parse + HTML.
- `agent-transcript-parser` (Python) — Claude↔Codex conversion, "lossless on round-trip". Only those two.
- `trail-cli`, Contextify, Automagik, `claude_codex_bridge` — Python, 2–3 harnesses, mostly read/HTML.

Gap claurdvoyant fills: one **Rust** IR across **all five** harnesses with **index + search + port + convert**.
