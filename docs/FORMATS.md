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
  `last-prompt`, `queue-operation`, plus `summary` and `system` (subtyped — see below).
- **Threading:** every `user`/`assistant`/`attachment` line has `uuid` + `parentUuid` (null at root) →
  a linked list / DAG. `last-prompt.leafUuid` points at the tail.
- **Message line fields:** `message.role`, `message.content` (string for simple user msgs; array of blocks
  `{type: text|thinking|redacted_thinking|tool_use|image|document}` for assistant; `tool_result` blocks
  for tool returns). Also `cwd`, `gitBranch`, `version`, `sessionId`, `timestamp` (ISO-8601), `model`
  (assistant), `usage` (tokens), `requestId`, `message.{id,stop_reason}`.
- **Tool results:** carried on a `user` line as `content[].type=="tool_result"` (`tool_use_id`, `content`,
  `is_error`) plus a richer `toolUseResult` sidecar object (`structuredPatch`/`oldTodos`/`newTodos`/
  `stdout`/`stderr`/file contents…). A user line whose content is *only* tool_results is a Tool turn.
- **`system` records** (`subtype`): `compact_boundary` (with `compactMetadata`), `local_command` (slash
  commands — `content` wraps `<command-name>/foo</command-name>` + `<command-args>` for the invocation,
  or `<local-command-stdout>…</local-command-stdout>` for the output), `away_summary`, `api_error`
  (`level:error`), `scheduled_task_fire`/`informational` (small `content` notices), `model_refusal_fallback`
  (`content` + `{originalModel, fallbackModel, apiRefusalCategory, apiRefusalExplanation, trigger,
  retractedMessageUuids}` — a provider safety refusal that re-routed to another model), `stop_hook_summary`
  (no `content`; **the record of a Stop-hook firing** — `{hookCount, hookInfos:[{command,…}], hookErrors[],
  hasOutput, hookAdditionalContext, stopReason, toolUseID}`; cv synthesizes a `⛓ stop hook` line when there
  was output/error, else drops it), plus bodyless `turn_duration`/`agents_killed`.
- **Compaction** (`compact_boundary`): `compactMetadata = {trigger: manual|auto, preTokens, durationMs,
  preservedSegment:{headUuid,anchorUuid,tailUuid}, preservedMessages:{…}}`. The boundary is immediately
  followed by a `user` message `isCompactSummary:true` whose `parentUuid == boundary.uuid` and whose body
  is the **generated summary that seeds the next context window** (the lost pre-compaction context). A
  session compacts repeatedly (the reference transcript: 11×).
- **Subagents (two tiers):**
  - *Directly-spawned* (`Agent`/`Task` tool): `<sessionId>/subagents/agent-<agentId>.jsonl` +
    `agent-<agentId>.meta.json` = `{agentType, description, toolUseId}`. **`toolUseId` links the child
    back to the exact `Agent`/`Task` tool_use in the parent transcript.** The child's records carry
    `isSidechain:true` + `agentId`; its *return value* is the last assistant text turn (surfaced in the
    parent's tool_result).
  - *Workflow* (`Workflow` tool): TWO sidecar locations per run.
    - `<sessionId>/subagents/workflows/<wf_runId>/agent-<agentId>.jsonl` (+ meta
      `{agentType:"workflow-subagent"}`) **plus `journal.jsonl`** = the orchestrator's structured log:
      `{type:started|result, agentId, key, result}`. **`result` is the agent's real return value**, in one
      of two shapes: an object `{status, summary, …}` (status vocab is per-workflow:
      `done`/`partial`/`GREEN`/`proven`/`blocked`/`welded`/…) or a plain string (the whole freeform return).
    - `<sessionId>/workflows/wf_<runId>.json` = the **run STATE** (first-class): `{workflowName, status
      (completed|killed), summary, error, defaultModel, agentCount, totalTokens, totalToolCalls, durationMs,
      scriptPath, script (inline), phases:[{title,detail}], workflowProgress:[…]}`. `workflowProgress`
      interleaves `{type:workflow_phase, index, title}` headers with `{type:workflow_agent, index, label,
      phaseIndex, agentId, model, state (done|error|progress|start), tokens, toolCalls, durationMs,
      promptPreview, resultPreview, error, cached}` — the phase tree with every agent's telemetry+outcome
      under its phase. The driving script also lives at `<sessionId>/workflows/scripts/<name>-wf_<runId>.js`.
- **Attribution:** assistant/user turns may carry `attributionAgent` / `attributionMcpServer` /
  `attributionMcpTool` / `attributionSkill` (MCP tool_use `name`s are `mcp__<server>__<tool>`; skill
  invocations use the `Skill` tool). `attachment` records carry side-band UI deltas (`deferred_tools_delta`
  with `addedNames`/`removedNames`, etc.) — non-conversational.
- **Indexes/sidecars:** `~/.claude/history.jsonl` (`{display,timestamp(ms),project,sessionId}`),
  `~/.claude/sessions/<pid>.json` (live process state), `~/.claude/tasks/<sessionId>/<n>.json` (todos),
  `<sessionId>/tool-results/<id>.txt` (deferred large tool-result bodies).
- **cv surfacing:** `cv tree <id>` appends the sub-agent forest (direct + per-workflow, with journaled
  outcomes); `cv show <id> --subagents` lists every agent with its return; `cv show <id> --agent <aid>`
  renders one sub-agent transcript; `cv events <id> --subagents` extracts the whole forest's tool activity,
  attributed per agent. **`cv workflow <id> [<runId>]`** renders a run's phase tree → agents → outcomes (+
  `--script`); **`cv tools <id>`** is the cross-agent tool-analytics surface (aggregate / `--agent` /
  `--tool` / `--workflow` / `--across` / `--timeline`); **`cv compaction <id>`** lists every boundary +
  summary (`--summaries` for full text); **`cv show <id> --pre-compaction <N>`** reads the lost pre-span.

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
- Also handled now: the gemini-cli JSON **chat recordings** (`~/.gemini/tmp/<projectHash>/chats/session-*.json`
  legacy object + modern append-only `.jsonl` with `$set`/`$rewindTo`) and `checkpoint-*.json` — far richer
  than logs.json (real assistant turns, thoughts, tool calls). Qwen Code reuses this format under `~/.qwen/`.

## Hermes (Nous) — `~/.hermes/state.db` (SQLite, schema v14; `$HERMES_HOME` overrides)

- `sessions` + `messages` tables (OpenAI-shaped rows). cwd is NOT persisted (runtime-only).
- Multimodal content uses a `\x00json:` sentinel prefix. Reasoning spans several columns
  (`reasoning`, `reasoning_content`, `reasoning_details`, `codex_reasoning_items`, `codex_message_items`).
- **Schema drift:** Hermes ALTERs columns in live (no version gate) — older DBs lack newer columns, so probe
  `PRAGMA table_info` and select only what exists. Compression chains link sessions via `parent_session_id`
  (parent `end_reason='compression'`); walk root→tip and dedup the replayed boundary user message.

## OpenClaw — `~/.openclaw/agents/<agentId>/sessions/`

- `sessions.json` index + `<sid>.jsonl` (and `<sid>-topic-<id>.jsonl`) transcripts; a `{type:session,version:N}`
  header (v1/v2 linear → v3 parent-linked, migrated in place) then `{type:message,...}` lines.
- Roles user/assistant/toolResult + custom (bashExecution, branchSummary, compactionSummary, custom).
  Blocks: text(+textSignature), thinking(+thinkingSignature,redacted), toolCall, image. Secrets redacted at
  write time (no fixed sentinel). ACP-bridged sessions are text-only echoes (`model:"acp-runtime"`).

## Cursor IDE — `~/Library/Application Support/Cursor/User/` (mac; `%APPDATA%/Cursor/User` Win; `$XDG_CONFIG_HOME/Cursor/User` Linux)

- VS Code-derived; SQLite `state.vscdb` (`ItemTable` + `cursorDiskKV`; JSON values, TEXT or BLOB). Open READ-ONLY.
- **Global content:** `globalStorage/state.vscdb` → `cursorDiskKV`: `composerData:<id>` (one thread; metadata +
  either inline `conversation[]` (older `_v`) or `fullConversationHeadersOnly[]` pointers (newer)), and
  `bubbleId:<composerId>:<bubbleId>` (one message: `type` 1=user/2=assistant, `text`, `richText` Lexical,
  `thinking{text}`, `toolFormerData{name,rawArgs,params,result,error,status}`, `tokenCount`).
- **cwd:** per-workspace `workspaceStorage/<hash>/state.vscdb` `ItemTable.composer.composerData.allComposers[]`
  links composerIds → workspace; sibling `workspace.json` `folder:"file://…"` is the cwd. Legacy: in-workspace
  `workbench.panel.aichat.view.aichat.chatdata`. Closed-source & churny (`_v` 1..=10+) — skip the unknown.

## Desktop apps (detected, not parseable)

- **Claude app** — `~/Library/Application Support/Claude/` (Electron). Transcripts are **server-side** (claude.ai);
  local LevelDB/IndexedDB holds only auth/settings/UI state. `claude-code-sessions/*.json` are metadata stubs
  pointing (via `cliSessionId`) at `~/.claude/projects/` transcripts the Claude Code adapter already handles.
- **ChatGPT app** — `~/Library/Application Support/com.openai.chat/` (native AppKit, not Electron). Offline
  history is local as one **encrypted** file per conversation (`conversations-v3-<acct>/<uuid>.data`); the key is
  app-held (not in a readable Keychain item). We detect the install + count convos but cannot decrypt.

## Kimi CLI (MoonshotAI) — `$KIMI_SHARE_DIR` or `~/.kimi`

- Sessions at `sessions/<md5(cwd)>/<uuid>/context.jsonl` (modern dir form) or `sessions/<md5(cwd)>/<uuid>.jsonl`
  (legacy flat). Project-dir name is `md5(cwd_utf8).hexdigest()` (or `<kaos>_<md5>` for non-local KAOS); cwd is
  recovered from `~/.kimi/kimi.json` `work_dirs[]`. `context.jsonl` roles `_system_prompt`/`user`/`assistant`/
  `tool` (+ skippable `_checkpoint`/`_usage`); content is a bare string or a list of `type`-tagged Parts
  (`text`/`think`/`image_url`/…); tool calls carry a JSON-string `arguments`. A `wire.jsonl` sidecar
  (`protocol_version` 1.x) enriches tool results + token usage. `state.json` → title; `context_N.jsonl` are
  compaction segments. Read-only.

## Qwen Code — `~/.qwen/`

- A **gemini-cli fork**: byte-identical format under `~/.qwen/tmp/<projectHash>/{logs.json, chats/session-*.json|jsonl,
  checkpoint-*.json}`. The adapter delegates to the Gemini parser and re-tags `Harness::Qwen`. (Path delta only.)

## LM Studio — `~/.lmstudio/conversations/<ms-epoch>.conversation.json`

- Plaintext JSON, one file per chat (filename stem = id = `createdAt` ms). No cwd (chat app). `messages[]` =
  `{versions:[…], currentlySelected}` (every regenerated variant kept; read the selected one). `singleStep` (user)
  content parts `text`/`file`; `multiStep` (assistant) `steps[]` of `contentBlock` (`style.type=="thinking"` →
  reasoning; `genInfo` → model + tokens). No first-class tool calls (gpt-oss inline `<|channel|>` markers kept as
  text). Attachments are references; bytes under `~/.lmstudio/.internal/files/`.

## Cline — VS Code extension (`<globalStorage>/saoudrizwan.claude-dev/`)

- Tasks at `<globalStorage>/saoudrizwan.claude-dev/tasks/<taskId>/` (also `~/.cline/tasks/`): `api_conversation_history.json`
  (a JSON **array of raw Anthropic Messages-API objects** — maps ~1:1 onto the Claude block model), `ui_messages.json`
  (UI events, timestamp enrichment), `task_metadata.json`. globalStorage roots: `<Editor>/User/globalStorage/` for
  `<Editor>` ∈ {Code, Code - Insiders, Cursor, VSCodium}. `<taskId>` ms-epoch → `created_at`. cwd from the first user
  msg's `<environment_details>`. A `user` turn of only `tool_result` blocks → a Tool turn.

## Roo Code — a Cline fork (`<globalStorage>/rooveterinaryinc.roo-cline|roo-code/`)

- Identical per-task layout/schema to Cline, different extension namespace (+ `~/.roo/tasks/`). Reuses the Cline parser,
  tagged `Harness::Roo`.

## Continue (continue.dev) — `~/.continue/sessions/` (`$CONTINUE_GLOBAL_DIR` overrides)

- Index `sessions.json` (`[{sessionId, title, dateCreated, workspaceDirectory}]`) + per-session `<id>.json`
  (`{sessionId, title, workspaceDirectory, history:[item]}`). Each item's `message` is OpenAI-shaped (role
  user/assistant/system/tool; content string or `[{type:text}/{type:imageUrl}]`; assistant `toolCalls[]`;
  `role:"tool"` + `toolCallId`). `contextItems[]` → File refs. cwd = `workspaceDirectory`.

## Goose (Block) — modern SQLite + legacy `.jsonl`

- Data dir: Linux `~/.local/share/goose/sessions/`; macOS `~/Library/Application Support/Block.block.goose/sessions/`;
  Windows `%APPDATA%\Block\Block\goose\data\sessions\` (`$GOOSE_PATH_ROOT`/`$XDG_DATA_HOME` override). Modern:
  `sessions.db` (`sessions(working_dir→cwd, description→title, provider_name+model_config_json→model, …)` +
  `messages(role[user|assistant], content_json, …)`; open READ-ONLY, PRAGMA-probe columns). `content_json` = array of
  MCP-style `MessageContent` (`text`/`thinking`/`toolRequest`/`toolResponse`/…); tool results ride on `user` msgs →
  reclassified to Tool. Legacy: per-session `<name>.jsonl` (header line + one message per line).

## Zed — `<data_dir>/threads/threads.db` (SQLite + zstd blobs)

- **Location:** macOS `~/Library/Application Support/Zed/threads/threads.db`; Linux
  `$XDG_DATA_HOME/zed/threads/threads.db` (default `~/.local/share/zed/…`; flatpak
  `~/.var/app/dev.zed.Zed/data/zed/…`); Windows `%LOCALAPPDATA%\Zed\threads\threads.db`. A sibling
  `threads-db.0.mdb/` LMDB dir is a dead pre-SQLite store (heed era) — empty on the reference machine, ignored.
- **Schema:** one table, grown by un-gated `ALTER TABLE`s (probe `PRAGMA table_info`; old DBs lack the tail):
  `threads(id TEXT PK, summary TEXT, updated_at TEXT, data_type TEXT, data BLOB` + later
  `parent_id, worktree_branch, folder_paths, folder_paths_order, created_at)`. Timestamps are RFC3339
  with offset. `folder_paths` = workspace folders, **lexicographically sorted and `\n`-joined**;
  `folder_paths_order` = `,`-joined indices restoring the user's original order (first ordered path =
  primary worktree). `parent_id` links a **subagent** thread to its parent (we surface it as
  `extra.parent_thread_id`; the hierarchy itself is flattened — each thread is its own session).
- **Blob:** `data_type` `"zstd"` → one zstd frame (level 3, written by `zstd::encode_all`) of JSON;
  `"json"` → raw JSON (the code supports it; never observed). No message table — counting messages
  requires decoding the blob.
- **Blob JSON, three generations** (sniff by `version` + message shape; serde structs in
  zed `crates/agent/src/{db,legacy_thread,thread}.rs`):
  - **versionless** (oldest): `{summary, updated_at, messages:[{id:int, role, text, tool_uses, tool_results}]}` —
    plain `text` instead of segments.
  - **`0.1.0` / `0.2.0`** (agent1 `SerializedThread`): `{version, summary, updated_at,
    messages:[{id:int, role:"user"|"assistant"|"system", segments:[{type:"text"|"thinking"(+signature)
    |"RedactedThinking"(data)}], tool_uses:[{id,name,input}], tool_results:[{tool_use_id,is_error,
    content,output}], context:"…", creases:[…], is_hidden}], initial_project_snapshot,
    cumulative_token_usage, request_token_usage:[…], detailed_summary_state:{Generated:{text}},
    model:{provider,model}, completion_mode, tool_use_limit_reached, profile}`. In 0.1.0 `tool_results`
    rode on the **next user** message; 0.2.0 moved them onto the calling assistant message. Each agentic
    step is its **own assistant message** (a 235-"message" thread can be ~37 user turns). `context` is the
    rendered attached-files preamble; `creases` are editor fold ranges (UI-only).
  - **`0.3.0`** (agent2/ACP `DbThread`, flattened `version` added at save): `{version, title, messages,
    updated_at, detailed_summary, initial_project_snapshot, cumulative_token_usage,
    request_token_usage:{<user-msg-id>:usage}, model, profile, imported, subagent_context:
    {parent_thread_id, depth}, speed, thinking_enabled, thinking_effort, …}`. Messages are an
    **externally tagged enum**: `{"User":{id, content:[{"Text":…}|{"Mention":{uri,content}}|
    {"Image":{source,size}}]}}`, `{"Agent":{content:[{"Text":…}|{"Thinking":{text,signature}}|
    {"RedactedThinking":"…"}|{"ToolUse":{id,name,input,raw_input,…}}], tool_results:{<id>:
    {tool_use_id,tool_name,is_error,content,output}}, reasoning_details}}`, bare `"Resume"`, or
    `{"Compaction":{"Summary":"…"}}`. Tool-result `content` is `{"Text":…}`/`{"Image":…}`/plain string.
- **cwd:** `initial_project_snapshot.worktree_snapshots[0].worktree_path` (fallback: `folder_paths`
  column). The same snapshot's `git_state{remote_url, head_sha, current_branch, diff}` → `GitInfo`.
- **No per-message timestamps anywhere** — only thread-level created/updated.
- **Lossy notes:** extra worktrees of a multi-root workspace, `request_token_usage`, mention bodies
  (URI kept as a File block), and `creases` semantics aren't modeled; legacy `context` and 0.3.0
  `reasoning_details` ride in `extra`. We decode blobs with pure-Rust `ruzstd` (no C cross-compile cost).

## Account data exports — registered in `config.toml` (opt-in)

The archives you download via "Export data". One file (`conversations.json`, often split
`conversations-000.json … -NNN.json`) holds **many** conversations, so one file → many `SessionRef`s.
Distinct from the `claude`/`claude-app`/`chatgpt-app` harnesses. Account exports have no fixed home, so
discovery is **opt-in via the config index**: register source dirs/files with `cv config --add-export
<path>` (stored in `$XDG_CONFIG_HOME/claurdvoyant/config.toml` as `exports = [...]`; `$CV_EXPORTS`, a
`:`-separated list, is honored as an ad-hoc union on top). With nothing registered, no scan happens —
these archives are large and enumerating thousands of conversations costs seconds. Conversations are
**deduped by id** (overlapping exports repeat them). Both adapters parse-only (no emit). The two shapes
are sniffed by content (a file's first conversation): `mapping` ⇒ ChatGPT, `chat_messages` ⇒ Claude.

- **`chatgpt-export`** — each conversation: `{id|conversation_id, title, create_time, update_time,
  default_model_slug, current_node, mapping}`. `mapping` is a **DAG**: `node_id → {id, parent,
  children[], message}`; we walk the `root → current_node` chain (the active branch; regenerated/edited
  branches are excluded). A message: `{author:{role}, content:{content_type, parts|text}, recipient,
  create_time}`. `content_type` ∈ `text` (parts=strings) / `code` (`text`) / `multimodal_text` (parts
  incl. `{content_type:image_asset_pointer, asset_pointer:"file-service://…"}` → `Block::Image`) /
  `execution_output` / `tether_*` (browsing) / `user_editable_context` (custom instructions). On an
  assistant turn, `recipient` ≠ `all` (e.g. `python`, `dalle.text2im`, `browser`, `bio`) ⇒ a tool call.
- **`claude-export`** — each conversation: `{uuid, name, summary, created_at, updated_at,
  chat_messages[]}`. `chat_messages` are linear (via `parent_message_uuid`); `sender` ∈ human/assistant;
  `content[]` blocks ∈ `text` / `thinking` / `tool_use{id,name,input}` / `tool_result{tool_use_id,
  content,is_error}`; a flat `text` field is the fallback when there are no structured blocks.
- **Lossy notes (v1):** off-branch ChatGPT regenerations aren't surfaced; tool-call args beyond the
  rendered text aren't structured for ChatGPT (recipient-tool turns become a `ToolUse` carrying the
  text); image bytes are references (`asset_pointer`), not inlined.

---

## Prior art (don't reinvent; do unify)

- `claude-code-transcripts` (Rust, lib.rs) — typed parser, **Claude only**, parse + HTML.
- `agent-transcript-parser` (Python) — Claude↔Codex conversion, "lossless on round-trip". Only those two.
- `trail-cli`, Contextify, Automagik, `claude_codex_bridge` — Python, 2–3 harnesses, mostly read/HTML.

Gap claurdvoyant fills: one **Rust** IR across **all of them** (10 harnesses) with **index + search + port +
convert + live-follow + an MCP server + a coordination board**.
