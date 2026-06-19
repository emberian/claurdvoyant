// A small embedded sample dataset matching the cv-core IR serde shape.
// Used so the UI is demoable without uploading a .zip (and as the fallback
// when the wasm module isn't built/present).
//
// Session JSON shape (serde of the Rust IR):
//   { id, harness, cwd?, title?, created_at?, updated_at?, model?, git?,
//     messages: [ { id?, parent_id?, role, timestamp?, model?, content: [Block], usage? } ],
//     source_path? }
// Block is tagged by `kind`.

export const SAMPLE_SESSIONS = [
  {
    id: "c0ffee-claude-001",
    harness: "claude",
    cwd: "/Users/ember/pug/clustervision",
    title: "Wire up the WASM ingest contract",
    created_at: "2026-05-28T17:12:04Z",
    updated_at: "2026-05-28T17:41:55Z",
    model: "claude-opus-4-8",
    git: { branch: "dev", commit: "a1b2c3d", remote: "git@github.com:ember/clustervision.git" },
    source_path: ".claude/projects/-Users-ember-pug-clustervision/c0ffee.jsonl",
    messages: [
      {
        id: "m1",
        role: "user",
        timestamp: "2026-05-28T17:12:04Z",
        content: [
          { kind: "text", text: "Can you read the IR and tell me what `ingest_zip` should return? I want the web UI to match exactly." }
        ]
      },
      {
        id: "m2",
        parent_id: "m1",
        role: "assistant",
        timestamp: "2026-05-28T17:12:30Z",
        model: "claude-opus-4-8",
        usage: { input_tokens: 1840, output_tokens: 320, cache_read_tokens: 12000 },
        content: [
          { kind: "thinking", text: "The contract says ingest_zip returns a JSON string that is an array of Session objects. Let me confirm the Block tag names from ir.rs: text, thinking, tool_use, tool_result, image. Role is lowercased. Harness lowercased too." },
          { kind: "text", text: "`ingest_zip(bytes)` returns a JSON **string** — an array of `Session` objects. Each block is tagged by `kind`. Let me grep the source to be sure of the field names." },
          { kind: "tool_use", id: "t1", name: "Grep", input: { pattern: "tag = \"kind\"", path: "crates/cv-core/src/ir.rs" } }
        ]
      },
      {
        id: "m3",
        parent_id: "m2",
        role: "tool",
        timestamp: "2026-05-28T17:12:31Z",
        content: [
          { kind: "tool_result", tool_use_id: "t1", content: "crates/cv-core/src/ir.rs:213:#[serde(tag = \"kind\", rename_all = \"snake_case\")]\npub enum Block {\n    Text { text: String },\n    Thinking { text, signature?, encrypted? },\n    ToolUse { id, name, input },\n    ToolResult { tool_use_id, content, is_error },\n    Image { media_type?, data_ref? },\n}", is_error: false }
        ]
      },
      {
        id: "m4",
        parent_id: "m3",
        role: "assistant",
        timestamp: "2026-05-28T17:13:10Z",
        model: "claude-opus-4-8",
        usage: { input_tokens: 2100, output_tokens: 410 },
        content: [
          { kind: "text", text: "Confirmed. Tags are snake_case: `text`, `thinking`, `tool_use`, `tool_result`, `image`. Roles and harness names are lowercase. You can render `tool_use.input` as pretty JSON in a code block, and `tool_result.content` as a (possibly large) preformatted block with an error style when `is_error` is true." }
        ]
      }
    ]
  },
  {
    id: "feed-codex-7",
    harness: "codex",
    cwd: "/Users/ember/work/payments-svc",
    title: "Fix the off-by-one in the retry backoff",
    created_at: "2026-05-27T09:02:00Z",
    updated_at: "2026-05-27T09:20:18Z",
    model: "gpt-5-codex",
    git: { branch: "main", commit: "9988776" },
    source_path: ".codex/sessions/2026/05/27/feed-codex-7.jsonl",
    messages: [
      {
        role: "system",
        content: [{ kind: "text", text: "You are a coding agent operating in the payments-svc repository. Be terse." }]
      },
      {
        role: "user",
        timestamp: "2026-05-27T09:02:00Z",
        content: [{ kind: "text", text: "The retry loop fires one extra time past max_attempts. Find and fix it." }]
      },
      {
        role: "assistant",
        timestamp: "2026-05-27T09:03:40Z",
        model: "gpt-5-codex",
        usage: { input_tokens: 5200, output_tokens: 180 },
        content: [
          { kind: "thinking", text: "Classic `<=` vs `<`. Let me search the retry module." },
          { kind: "tool_use", id: "s1", name: "shell", input: { command: ["bash", "-lc", "grep -rn 'attempt' src/retry.rs"] } }
        ]
      },
      {
        role: "tool",
        timestamp: "2026-05-27T09:03:41Z",
        content: [{ kind: "tool_result", tool_use_id: "s1", content: "src/retry.rs:42:        while attempt <= max_attempts {", is_error: false }]
      },
      {
        role: "assistant",
        timestamp: "2026-05-27T09:04:20Z",
        model: "gpt-5-codex",
        content: [
          { kind: "text", text: "Found it. `while attempt <= max_attempts` should be `<`. With a 1-based `attempt` starting at 1, `<=` runs `max_attempts + 1` times. Patching now." },
          { kind: "tool_use", id: "s2", name: "apply_patch", input: { patch: "*** Update File: src/retry.rs\n@@\n-        while attempt <= max_attempts {\n+        while attempt < max_attempts {" } }
        ]
      },
      {
        role: "tool",
        timestamp: "2026-05-27T09:04:21Z",
        content: [{ kind: "tool_result", tool_use_id: "s2", content: "Applied patch to src/retry.rs", is_error: false }]
      }
    ]
  },
  {
    id: "grok-img-3",
    harness: "grok",
    cwd: "/tmp/scratch",
    title: "Look at this screenshot",
    updated_at: "2026-05-20T14:00:00Z",
    messages: [
      {
        role: "user",
        content: [
          { kind: "text", text: "What's wrong with this layout?" },
          { kind: "image", media_type: "image/png", data_ref: "attachments/screenshot-1.png" }
        ]
      },
      {
        role: "assistant",
        content: [
          { kind: "thinking", encrypted: "AQEBxR3k…opaque…", text: "" },
          { kind: "text", text: "The sidebar is overflowing its container — the flex child needs `min-width: 0` so it can shrink below its content size." }
        ]
      }
    ]
  },
  {
    id: "claude-loom-branch-a",
    harness: "claude",
    cwd: "/Users/ember/pug/clustervision",
    title: "Wire up the WASM ingest contract (branch A)",
    created_at: "2026-05-28T17:12:04Z",
    updated_at: "2026-05-28T17:30:00Z",
    model: "claude-opus-4-8",
    git: { branch: "loom-a", commit: "a1b2c3d" },
    messages: [
      { id: "m1", role: "user", timestamp: "2026-05-28T17:12:04Z",
        content: [{ kind: "text", text: "Can you read the IR and tell me what `ingest_zip` should return? I want the web UI to match exactly." }] },
      { id: "mb2", parent_id: "m1", role: "assistant", timestamp: "2026-05-28T17:20:00Z", model: "claude-opus-4-8",
        content: [
          { kind: "text", text: "Branch A: let's just trust the docs — it returns a JSON string of Session[]. Shipping." },
          { kind: "file", mime: "text/x-rust", path: "crates/cv-web/src/lib.rs", source: "diff" }
        ] }
    ]
  },
  {
    // An OpenSession-format (camelCase) session, to exercise the JSON loader /
    // normalizer. Dropping a .json shaped like this loads directly — no wasm.
    openSession: "0.1",
    harness: "openSession",
    id: "open-demo-42",
    cwd: "/Users/ember/lab/opensession",
    title: "OpenSession round-trip demo",
    model: "gpt-5-codex",
    createdAt: "2026-05-29T10:00:00Z",
    updatedAt: "2026-05-29T10:08:00Z",
    git: { branch: "main", commit: "deadbeef", remote: "git@github.com:ember/open.git" },
    messages: [
      { id: "o1", role: "user", timestamp: "2026-05-29T10:00:00Z",
        content: [{ kind: "text", text: "Prove the camelCase OpenSession shape loads and renders." }] },
      { id: "o2", parentId: "o1", role: "assistant", timestamp: "2026-05-29T10:01:00Z", model: "gpt-5-codex",
        usage: { inputTokens: 900, outputTokens: 120, cacheReadTokens: 4000 },
        content: [
          { kind: "thinking", redacted: true },
          { kind: "text", text: "Here's a tool call using the OpenSession `toolUse` kind:" },
          { kind: "toolUse", id: "c1", name: "Bash", input: { command: "cargo test --workspace" } }
        ] },
      { id: "o3", parentId: "o2", role: "tool", timestamp: "2026-05-29T10:02:00Z",
        content: [{ kind: "toolResult", toolUseId: "c1", toolName: "Bash", status: "ok", isError: false,
          content: "running 128 tests\n…\ntest result: ok. 128 passed; 0 failed",
          details: { exitCode: 0, durationMs: 8421 } }] }
    ]
  }
];

export default SAMPLE_SESSIONS;
