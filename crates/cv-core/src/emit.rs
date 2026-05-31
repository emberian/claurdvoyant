//! port-a-sesh: emit an IR [`Session`] back into a harness's native on-disk format.
//!
//! This is the engine behind `cv convert` (cross-harness) and `cv port` (rehome a session to another
//! cwd / move local↔remote). Conversion is `parse(source) -> IR -> emit(target)`.
//!
//! Correctness bar: a session emitted for harness H must round-trip back through that harness's
//! parser (`emit(s, H) |> parse == s`, modulo fields H cannot represent). See `docs/FORMATS.md` for
//! each target's on-disk schema.
//!
//! NOTE: This module is owned by the port-a-sesh work. The free function [`emit`] is the public entry
//! point; `cv-core::lib` re-exports it and the `cv` CLI calls it.

use crate::harness::EmitResult;
use crate::ir::{Block, GitInfo, Harness, Role, Session, SessionRef};
use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde_json::{Map, Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Knobs for emitting/porting a session.
#[derive(Debug, Clone, Default)]
pub struct EmitOptions {
    /// Override the recorded working directory (rehome the session to a new project).
    pub new_cwd: Option<PathBuf>,
    /// Force a specific new session id (otherwise a fresh one is generated).
    pub new_id: Option<String>,
}

/// Emit `session` into `target`'s native format under `out_dir` (the target harness's storage root,
/// or any directory for a dry run). Returns where it was written + how to resume it.
pub fn emit(
    session: &Session,
    target: Harness,
    out_dir: &Path,
    opts: &EmitOptions,
) -> Result<EmitResult> {
    match target {
        Harness::Claude => emit_claude(session, out_dir, opts),
        Harness::Codex => emit_codex(session, out_dir, opts),
        Harness::Grok => emit_grok(session, out_dir, opts),
        Harness::OpenCode => emit_opencode(session, out_dir, opts),
        Harness::OpenClaw => emit_openclaw(session, out_dir, opts),
        Harness::Gemini => emit_gemini(session, out_dir, opts),
        #[cfg(feature = "sqlite")]
        Harness::Hermes => emit_hermes(session, out_dir, opts),
        #[cfg(not(feature = "sqlite"))]
        Harness::Hermes => {
            anyhow::bail!("emit to hermes requires the `sqlite` feature")
        }
        // Emitters living in their own adapter modules.
        Harness::Kimi => crate::harness::kimi::emit(session, out_dir, opts),
        Harness::LmStudio => crate::harness::lmstudio::emit(session, out_dir, opts),
        Harness::Cline => crate::harness::cline::emit(session, out_dir, opts),
        Harness::Roo => crate::harness::roo::emit(session, out_dir, opts),
        Harness::Continue => crate::harness::continuedev::emit(session, out_dir, opts),
        // Qwen Code stores the same ConversationRecord shape Gemini does, so it reuses that emitter.
        Harness::Qwen => emit_gemini(session, out_dir, opts),
        // Parse-only harnesses (Cursor, Goose, desktop apps, …) aren't conversion targets.
        other => anyhow::bail!("emit to {other} is not supported yet"),
    }
}

/// Emit `session` into `target`, then re-parse the written output with `target`'s own adapter and
/// diff it against the input IR. Returns the [`EmitResult`] plus a list of human-readable
/// lossy-conversion warnings (empty when the round-trip is clean). The CLI prints these so a user
/// porting a session knows what (if anything) didn't survive.
///
/// This never changes what `emit` writes — it's a read-back verification pass on top of it.
pub fn emit_verified(
    session: &Session,
    target: Harness,
    out_dir: &Path,
    opts: &EmitOptions,
) -> Result<(EmitResult, Vec<String>)> {
    let result = emit(session, target, out_dir, opts)?;
    let warnings = match reparse_emitted(target, &result) {
        Ok(reparsed) => diff_lossy(session, &reparsed),
        Err(e) => vec![format!("could not verify round-trip ({e:#})")],
    };
    Ok((result, warnings))
}

/// Re-parse a just-emitted session back into the IR using `target`'s adapter, so we can diff it
/// against the source. Mirrors how each adapter's `parse` keys off a [`SessionRef`].
fn reparse_emitted(target: Harness, result: &EmitResult) -> Result<Session> {
    use crate::harness::Adapter;
    let sref = |id: String, path: PathBuf| SessionRef {
        id,
        harness: target,
        path,
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        message_count: 0,
    };
    match target {
        Harness::Claude => {
            let id = result.new_id.clone();
            crate::harness::claude::Claude::new().parse(&sref(id, result.path.clone()))
        }
        Harness::Codex => crate::harness::codex::Codex::new()
            .parse(&sref(String::new(), result.path.clone())),
        Harness::Grok => crate::harness::grok::Grok::new()
            .parse(&sref(result.new_id.clone(), result.path.clone())),
        Harness::OpenClaw => crate::harness::openclaw::OpenClaw::new()
            .parse(&sref(result.new_id.clone(), result.path.clone())),
        Harness::Gemini => crate::harness::gemini::Gemini::new()
            .parse(&sref(result.new_id.clone(), result.path.clone())),
        Harness::OpenCode => reparse_opencode(result),
        #[cfg(feature = "sqlite")]
        Harness::Hermes => reparse_hermes(result),
        // The emitters whose output is parseable directly from the EmitResult path.
        Harness::Kimi => crate::harness::kimi::Kimi::new()
            .parse(&sref(result.new_id.clone(), result.path.clone())),
        Harness::LmStudio => crate::harness::lmstudio::LmStudio::new()
            .parse(&sref(result.new_id.clone(), result.path.clone())),
        Harness::Cline => crate::harness::cline::Cline::new()
            .parse(&sref(result.new_id.clone(), result.path.clone())),
        Harness::Roo => crate::harness::roo::Roo::new()
            .parse(&sref(result.new_id.clone(), result.path.clone())),
        Harness::Continue => crate::harness::continuedev::Continue::new()
            .parse(&sref(result.new_id.clone(), result.path.clone())),
        // Qwen reuses Gemini's emitter (same record shape) but its own parser.
        Harness::Qwen => crate::harness::qwen::Qwen::new()
            .parse(&sref(result.new_id.clone(), result.path.clone())),
        other => anyhow::bail!("re-parse for {other} not supported"),
    }
}

/// OpenCode resolves message/part dirs relative to `$HOME/.local/share/opencode/storage`; the emit
/// target dir is that storage root. Point HOME at its grandparent-of-grandparent so `discover()`
/// finds the session. Serialized against other HOME mutators in this crate's tests.
fn reparse_opencode(result: &EmitResult) -> Result<Session> {
    use crate::harness::Adapter;
    let _guard = HOME_ENV_LOCK.lock().unwrap();
    // result.path is .../storage/session/<projectID>/<id>.json — walk up to the synthetic HOME.
    let storage = result
        .path
        .ancestors()
        .find(|p| p.file_name().map(|n| n == "storage").unwrap_or(false))
        .context("locating opencode storage root for re-parse")?;
    let home = storage
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .context("locating opencode synthetic HOME for re-parse")?
        .to_path_buf();
    let prev = std::env::var_os("HOME");
    unsafe {
        std::env::set_var("HOME", &home);
    }
    let oc = crate::harness::opencode::OpenCode::new();
    let parsed = oc.discover().ok().and_then(|refs| {
        refs.into_iter()
            .find(|r| r.id == result.new_id)
            .and_then(|r| oc.parse(&r).ok())
    });
    unsafe {
        match prev {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }
    parsed.context("re-discovering emitted opencode session")
}

/// Hermes parses from `$HERMES_HOME/state.db`; point it at the emitted db's directory.
#[cfg(feature = "sqlite")]
fn reparse_hermes(result: &EmitResult) -> Result<Session> {
    use crate::harness::Adapter;
    let _guard = HOME_ENV_LOCK.lock().unwrap();
    let home = result
        .path
        .parent()
        .context("locating hermes state.db dir for re-parse")?
        .to_path_buf();
    let prev = std::env::var_os("HERMES_HOME");
    unsafe {
        std::env::set_var("HERMES_HOME", &home);
    }
    let h = crate::harness::hermes::Hermes::new();
    let parsed = h.discover().ok().and_then(|refs| {
        refs.into_iter()
            .find(|r| r.id == result.new_id)
            .and_then(|r| h.parse(&r).ok())
    });
    unsafe {
        match prev {
            Some(p) => std::env::set_var("HERMES_HOME", p),
            None => std::env::remove_var("HERMES_HOME"),
        }
    }
    parsed.context("re-discovering emitted hermes session")
}

/// Serializes HOME / HERMES_HOME env mutation during verification re-parse (process-global state).
static HOME_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Count notable content features of a session, for before/after lossiness comparison.
struct ContentStats {
    tool_uses: usize,
    tool_results: usize,
    images: usize,
    /// Thinking blocks carrying real summary/raw text (not just an encrypted blob).
    thinking_with_text: usize,
    /// Thinking blocks carrying only an encrypted blob (text empty).
    thinking_encrypted_only: usize,
    system_turns: usize,
}

fn content_stats(session: &Session) -> ContentStats {
    let mut s = ContentStats {
        tool_uses: 0,
        tool_results: 0,
        images: 0,
        thinking_with_text: 0,
        thinking_encrypted_only: 0,
        system_turns: 0,
    };
    for m in &session.messages {
        if m.role == Role::System {
            s.system_turns += 1;
        }
        for b in &m.content {
            match b {
                Block::ToolUse { .. } => s.tool_uses += 1,
                Block::ToolResult { .. } => s.tool_results += 1,
                Block::Image { .. } => s.images += 1,
                Block::Thinking { text, .. } => {
                    if text.trim().is_empty() {
                        s.thinking_encrypted_only += 1;
                    } else {
                        s.thinking_with_text += 1;
                    }
                }
                _ => {}
            }
        }
    }
    s
}

/// Human-readable lossy-conversion warnings: what the source IR had that didn't survive the
/// emit→re-parse round-trip. An empty Vec means a clean round-trip (modulo fields the target can't
/// represent, which we don't flag because they're inherent to the format).
fn diff_lossy(input: &Session, reparsed: &Session) -> Vec<String> {
    let before = content_stats(input);
    let after = content_stats(reparsed);
    let mut warnings = Vec::new();

    if after.tool_uses < before.tool_uses {
        let n = before.tool_uses - after.tool_uses;
        warnings.push(format!(
            "dropped {n} tool call{}",
            if n == 1 { "" } else { "s" }
        ));
    }
    if after.tool_results < before.tool_results {
        let n = before.tool_results - after.tool_results;
        warnings.push(format!(
            "dropped {n} tool result{}",
            if n == 1 { "" } else { "s" }
        ));
    }
    if after.images < before.images {
        let n = before.images - after.images;
        warnings.push(format!(
            "{n} image{} dropped or reduced to a placeholder",
            if n == 1 { "" } else { "s" }
        ));
    }
    // Reasoning text that didn't come back, or came back only as an encrypted/summary blob.
    if after.thinking_with_text < before.thinking_with_text {
        let lost = before.thinking_with_text - after.thinking_with_text;
        if after.thinking_encrypted_only > before.thinking_encrypted_only {
            warnings.push(format!(
                "{lost} reasoning block{} preserved as encrypted/summary only (raw text not stored)",
                if lost == 1 { "" } else { "s" }
            ));
        } else {
            warnings.push(format!(
                "dropped {lost} reasoning block{}",
                if lost == 1 { "" } else { "s" }
            ));
        }
    }
    if after.system_turns < before.system_turns {
        let n = before.system_turns - after.system_turns;
        warnings.push(format!(
            "dropped {n} system turn{} (target has no standalone system message)",
            if n == 1 { "" } else { "s" }
        ));
    }
    warnings
}

/// Which targets [`emit`] can currently write.
pub fn supported_targets() -> &'static [Harness] {
    &[
        Harness::Claude,
        Harness::Codex,
        Harness::Grok,
        Harness::OpenCode,
        Harness::OpenClaw,
        Harness::Gemini,
        #[cfg(feature = "sqlite")]
        Harness::Hermes,
        Harness::Kimi,
        Harness::LmStudio,
        Harness::Cline,
        Harness::Roo,
        Harness::Continue,
        Harness::Qwen,
    ]
}

/// Effective cwd after applying any rehome override.
fn effective_cwd(session: &Session, opts: &EmitOptions) -> Option<PathBuf> {
    opts.new_cwd.clone().or_else(|| session.cwd.clone())
}

/// Branch name (if any) for git-flavored metadata fields.
fn branch_of(session: &Session) -> Option<String> {
    session.git.as_ref().and_then(|g| g.branch.clone())
}

// ------------------------------------------------------------------------------------------------
// Claude
// ------------------------------------------------------------------------------------------------

/// Encode a cwd into Claude's project-dir name: leading `-`, then every `/` and `.` becomes `-`.
fn claude_encode_cwd(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    // Strip a single leading '/' so the mandatory leading '-' isn't doubled.
    let s = s.strip_prefix('/').unwrap_or(&s);
    let mut out = String::with_capacity(s.len() + 1);
    out.push('-');
    for ch in s.chars() {
        match ch {
            '/' | '.' => out.push('-'),
            c => out.push(c),
        }
    }
    out
}

fn emit_claude(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let new_id = opts
        .new_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let dir_name = cwd
        .as_ref()
        .map(|p| claude_encode_cwd(p))
        .unwrap_or_else(|| "-".to_string());
    let branch = branch_of(session);

    let proj_dir = out_dir.join(&dir_name);
    fs::create_dir_all(&proj_dir)
        .with_context(|| format!("creating {}", proj_dir.display()))?;
    let file_path = proj_dir.join(format!("{new_id}.jsonl"));

    let mut lines: Vec<Value> = Vec::new();
    let ts0 = session
        .created_at
        .or(session.updated_at)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);

    // ai-title line (so the title round-trips).
    if let Some(title) = &session.title {
        lines.push(json!({
            "type": "ai-title",
            "aiTitle": title,
            "sessionId": new_id,
            "cwd": cwd_str,
            "timestamp": ts0,
        }));
    }

    // Thread the conversation by uuid / parentUuid.
    let mut parent: Option<String> = None;
    for msg in &session.messages {
        // Claude transcripts have no standalone system turn; skip to avoid polluting user text.
        if msg.role == Role::System {
            continue;
        }
        let uuid = Uuid::new_v4().to_string();
        let ts = msg
            .timestamp
            .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
            .unwrap_or_else(|| ts0.clone());

        // Common envelope fields present on every threaded line.
        let mut line = Map::new();
        line.insert("uuid".into(), json!(uuid));
        line.insert("parentUuid".into(), json!(parent));
        line.insert("sessionId".into(), json!(new_id));
        line.insert("cwd".into(), json!(cwd_str));
        line.insert("timestamp".into(), json!(ts));
        if let Some(b) = &branch {
            line.insert("gitBranch".into(), json!(b));
        }

        match msg.role {
            Role::User => {
                // Simple user text → string content.
                let text = msg.text().unwrap_or_default();
                line.insert("type".into(), json!("user"));
                line.insert(
                    "message".into(),
                    json!({ "role": "user", "content": text }),
                );
            }
            Role::Assistant => {
                line.insert("type".into(), json!("assistant"));
                let blocks = claude_assistant_blocks(&msg.content);
                let mut message = Map::new();
                message.insert("role".into(), json!("assistant"));
                if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                    message.insert("model".into(), json!(model));
                }
                message.insert("content".into(), Value::Array(blocks));
                line.insert("message".into(), Value::Object(message));
            }
            Role::Tool => {
                // Tool results ride on a `user` line whose content is tool_result blocks.
                let blocks = claude_tool_result_blocks(&msg.content);
                line.insert("type".into(), json!("user"));
                line.insert(
                    "message".into(),
                    json!({ "role": "user", "content": Value::Array(blocks) }),
                );
            }
            Role::System => unreachable!("system turns are skipped above"),
        }

        lines.push(Value::Object(line));
        parent = Some(uuid);
    }

    write_jsonl(&file_path, &lines)?;

    let resume = match &cwd {
        Some(c) => format!("claude --resume {new_id}  (run from {})", c.display()),
        None => format!("claude --resume {new_id}"),
    };
    Ok(EmitResult {
        path: file_path,
        new_id,
        resume_hint: Some(resume),
    })
}

fn claude_assistant_blocks(content: &[Block]) -> Vec<Value> {
    let mut out = Vec::new();
    for b in content {
        match b {
            Block::Text { text } => out.push(json!({ "type": "text", "text": text })),
            Block::Thinking { text, signature, .. } => {
                let mut m = Map::new();
                m.insert("type".into(), json!("thinking"));
                m.insert("thinking".into(), json!(text));
                if let Some(sig) = signature {
                    m.insert("signature".into(), json!(sig));
                }
                out.push(Value::Object(m));
            }
            Block::ToolUse { id, name, input } => out.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            })),
            // Tool results don't belong on an assistant line; skip (emitted on the Tool turn).
            Block::ToolResult { .. } => {}
            Block::File { path, source, .. } => {
                let label = path.as_deref().or(source.as_deref()).unwrap_or("?");
                out.push(json!({ "type": "text", "text": format!("[file: {label}]") }));
            }
            Block::Image { media_type, data_ref } => {
                // Reconstruct the Anthropic `source` from our `data_ref` so the reference survives a
                // round-trip (the parser keeps a ref, not the bytes — but dropping it here lost even
                // the `file:`/url pointer). Mirror the three shapes claude.rs parses.
                let mut source = serde_json::Map::new();
                if let Some(mt) = media_type {
                    source.insert("media_type".into(), json!(mt));
                }
                match data_ref.as_deref() {
                    Some(r) if r.starts_with("file:") => {
                        source.insert("type".into(), json!("file"));
                        source.insert("file_id".into(), json!(&r["file:".len()..]));
                    }
                    Some("base64:inline") => {
                        // Bytes were intentionally not retained in the IR; mark the kind.
                        source.insert("type".into(), json!("base64"));
                    }
                    Some(r) => {
                        source.insert("type".into(), json!("url"));
                        source.insert("url".into(), json!(r));
                    }
                    None => {}
                }
                out.push(json!({ "type": "image", "source": Value::Object(source) }));
            }
        }
    }
    if out.is_empty() {
        out.push(json!({ "type": "text", "text": "" }));
    }
    out
}

fn claude_tool_result_blocks(content: &[Block]) -> Vec<Value> {
    let mut out = Vec::new();
    for b in content {
        if let Block::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } = b
        {
            out.push(json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
                "is_error": is_error,
            }));
        }
    }
    out
}

// ------------------------------------------------------------------------------------------------
// Codex
// ------------------------------------------------------------------------------------------------

fn emit_codex(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let new_id = opts
        .new_id
        .clone()
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    let cwd = effective_cwd(session, opts);

    let now = session.created_at.unwrap_or_else(Utc::now);
    // Path: out_dir/YYYY/MM/DD/rollout-<iso8601 colons→dashes>-<uuid>.jsonl
    let date_dir = out_dir.join(now.format("%Y").to_string())
        .join(now.format("%m").to_string())
        .join(now.format("%d").to_string());
    fs::create_dir_all(&date_dir)
        .with_context(|| format!("creating {}", date_dir.display()))?;
    let iso = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    let iso_dashed = iso.replace(':', "-");
    let file_path = date_dir.join(format!("rollout-{iso_dashed}-{new_id}.jsonl"));

    let ts_str = now.to_rfc3339_opts(SecondsFormat::Millis, true);

    let mut lines: Vec<Value> = Vec::new();

    // session_meta first line.
    let mut meta = Map::new();
    meta.insert("id".into(), json!(new_id));
    meta.insert("timestamp".into(), json!(ts_str));
    if let Some(c) = &cwd {
        meta.insert("cwd".into(), json!(c.to_string_lossy()));
    }
    meta.insert("source".into(), json!("cli"));
    meta.insert("originator".into(), json!("claurdvoyant"));
    meta.insert("cli_version".into(), json!(env!("CARGO_PKG_VERSION")));
    if let Some(model) = &session.model {
        meta.insert("model_provider".into(), json!(model));
    }
    if let Some(g) = &session.git {
        meta.insert("git".into(), codex_git(g));
    }
    lines.push(json!({
        "type": "session_meta",
        "timestamp": ts_str,
        "payload": Value::Object(meta),
    }));

    for msg in &session.messages {
        let ts = msg
            .timestamp
            .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
            .unwrap_or_else(|| ts_str.clone());

        match msg.role {
            Role::System => {
                let text = msg.text().unwrap_or_default();
                lines.push(codex_response_item(
                    &ts,
                    json!({
                        "type": "message",
                        "role": "developer",
                        "content": [{ "type": "input_text", "text": text }],
                    }),
                ));
            }
            Role::User => {
                let text = msg.text().unwrap_or_default();
                // The `response_item` message is what Codex feeds back into the model on resume;
                // the `event_msg` is only the UI transcript event. Emit BOTH (matching real
                // rollouts) — without the response_item the resumed model has no memory of the
                // user's turns. User content uses `input_text`.
                lines.push(codex_response_item(
                    &ts,
                    json!({
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": text }],
                    }),
                ));
                lines.push(codex_event_msg(
                    &ts,
                    json!({ "type": "user_message", "message": text }),
                ));
            }
            Role::Assistant => {
                // Split assistant content into NL text (event_msg) and structured items.
                for b in &msg.content {
                    match b {
                        Block::Text { text } => {
                            // Same as user turns: the response_item carries the assistant text
                            // into the resumed model context; the event_msg is UI-only. Assistant
                            // content uses `output_text`.
                            lines.push(codex_response_item(
                                &ts,
                                json!({
                                    "type": "message",
                                    "role": "assistant",
                                    "content": [{ "type": "output_text", "text": text }],
                                }),
                            ));
                            lines.push(codex_event_msg(
                                &ts,
                                json!({ "type": "agent_message", "message": text }),
                            ));
                        }
                        Block::Thinking { text, encrypted, .. } => {
                            let mut p = Map::new();
                            p.insert("type".into(), json!("reasoning"));
                            // The IR's Thinking text is the raw chain-of-thought (the parser
                            // prefers `content` over `summary`); preserve it as raw `content` so
                            // re-parsing recovers it verbatim. A distinct summary, if the source
                            // adapter stashed one in `extra.reasoning_summary`, rides in `summary`.
                            p.insert(
                                "content".into(),
                                json!([{ "type": "reasoning_text", "text": text }]),
                            );
                            let summary = msg
                                .extra
                                .get("reasoning_summary")
                                .and_then(Value::as_str)
                                .unwrap_or(text);
                            p.insert(
                                "summary".into(),
                                json!([{ "type": "summary_text", "text": summary }]),
                            );
                            if let Some(enc) = encrypted {
                                p.insert("encrypted_content".into(), json!(enc));
                            }
                            lines.push(codex_response_item(&ts, Value::Object(p)));
                        }
                        Block::ToolUse { id, name, input } => {
                            let args = serde_json::to_string(input)
                                .unwrap_or_else(|_| "{}".to_string());
                            lines.push(codex_response_item(
                                &ts,
                                json!({
                                    "type": "function_call",
                                    "call_id": id,
                                    "name": name,
                                    "arguments": args,
                                }),
                            ));
                        }
                        Block::File { path, source, .. } => {
                            let label =
                                path.as_deref().or(source.as_deref()).unwrap_or("?");
                            lines.push(codex_event_msg(
                                &ts,
                                json!({
                                    "type": "agent_message",
                                    "message": format!("[file: {label}]"),
                                }),
                            ));
                        }
                        Block::ToolResult { .. } | Block::Image { .. } => {}
                    }
                }
            }
            Role::Tool => {
                for b in &msg.content {
                    if let Block::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = b
                    {
                        lines.push(codex_response_item(
                            &ts,
                            json!({
                                "type": "function_call_output",
                                "call_id": tool_use_id,
                                "output": content,
                            }),
                        ));
                    }
                }
            }
        }
    }

    write_jsonl(&file_path, &lines)?;

    Ok(EmitResult {
        path: file_path,
        new_id: new_id.clone(),
        resume_hint: Some(format!("codex resume {new_id}")),
    })
}

fn codex_event_msg(ts: &str, payload: Value) -> Value {
    json!({ "type": "event_msg", "timestamp": ts, "payload": payload })
}

fn codex_response_item(ts: &str, payload: Value) -> Value {
    json!({ "type": "response_item", "timestamp": ts, "payload": payload })
}

fn codex_git(g: &GitInfo) -> Value {
    let mut m = Map::new();
    if let Some(b) = &g.branch {
        m.insert("branch".into(), json!(b));
    }
    if let Some(c) = &g.commit {
        m.insert("commit_hash".into(), json!(c));
    }
    if let Some(r) = &g.remote {
        m.insert("repository_url".into(), json!(r));
    }
    Value::Object(m)
}

// ------------------------------------------------------------------------------------------------
// Grok (best-effort)
// ------------------------------------------------------------------------------------------------

/// Percent-encode everything except unreserved chars, so `/` → `%2F` (matching Grok's dir layout).
const GROK_ENCODE: &AsciiSet = &CONTROLS
    .add(b'/')
    .add(b' ')
    .add(b'.')
    .add(b'%')
    .add(b':')
    .add(b'\\');

fn emit_grok(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let new_id = opts
        .new_id
        .clone()
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let enc_cwd = utf8_percent_encode(&cwd_str, GROK_ENCODE).to_string();

    let session_dir = out_dir.join(&enc_cwd).join(&new_id);
    fs::create_dir_all(&session_dir)
        .with_context(|| format!("creating {}", session_dir.display()))?;

    let created = session
        .created_at
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let updated = session
        .updated_at
        .or(session.created_at)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);

    // summary.json
    let mut summary = Map::new();
    summary.insert(
        "info".into(),
        json!({ "id": new_id, "cwd": cwd_str }),
    );
    summary.insert("created_at".into(), json!(created));
    summary.insert("updated_at".into(), json!(updated));
    summary.insert("last_active_at".into(), json!(updated));
    summary.insert(
        "num_chat_messages".into(),
        json!(session.messages.len()),
    );
    // Real Grok summaries carry these too; without at least `chat_format_version`
    // the loader rejects the dir with "Session does not exist". (Verified live against
    // grok 0.1.219 — adding the full field set is what makes `grok --resume` discover it.)
    let num_turns = session
        .messages
        .iter()
        .filter(|m| matches!(m.role, Role::User))
        .count();
    summary.insert("num_messages".into(), json!(num_turns));
    summary.insert("next_trace_turn".into(), json!(0));
    summary.insert("chat_format_version".into(), json!(1));
    summary.insert("agent_name".into(), json!("grok-build"));
    if let Some(home) = std::env::var_os("HOME") {
        summary.insert(
            "grok_home".into(),
            json!(format!("{}/.grok", Path::new(&home).to_string_lossy())),
        );
    }
    // Carry the source model only if it's actually a Grok model; otherwise default to
    // `grok-build`. A foreign model id (e.g. `gpt-5.5`) would make Grok try to replay the
    // history against an incompatible backend.
    let model_id = session
        .model
        .as_deref()
        .filter(|m| m.to_ascii_lowercase().contains("grok"))
        .unwrap_or("grok-build");
    summary.insert("current_model_id".into(), json!(model_id));
    if let Some(g) = &session.git {
        if let Some(b) = &g.branch {
            summary.insert("head_branch".into(), json!(b));
        }
        if let Some(c) = &g.commit {
            summary.insert("head_commit".into(), json!(c));
        }
        if let Some(r) = &g.remote {
            summary.insert("git_remotes".into(), json!([r]));
        }
    }
    summary.insert(
        "session_summary".into(),
        json!(session.title.as_deref().unwrap_or("")),
    );
    let summary_path = session_dir.join("summary.json");
    fs::write(&summary_path, serde_json::to_string_pretty(&summary)?)
        .with_context(|| format!("writing {}", summary_path.display()))?;

    // chat_history.jsonl
    let mut lines: Vec<Value> = Vec::new();
    for msg in &session.messages {
        match msg.role {
            Role::System => {
                let text = grok_concat_text(msg);
                lines.push(json!({ "type": "system", "content": text }));
            }
            Role::User => {
                let text = grok_concat_text(msg);
                lines.push(json!({
                    "type": "user",
                    "content": [{ "type": "text", "text": text }],
                }));
            }
            Role::Assistant => {
                let mut line = Map::new();
                line.insert("type".into(), json!("assistant"));
                line.insert("content".into(), json!(grok_concat_text(msg)));
                if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                    line.insert("model_id".into(), json!(model));
                }
                for b in &msg.content {
                    if let Block::Thinking { text, encrypted, .. } = b {
                        let mut r = Map::new();
                        r.insert("text".into(), json!(text));
                        // `encrypted` reasoning blobs are provider/account-bound. Carrying one
                        // from a foreign harness (e.g. OpenAI's `encrypted_content`) makes Grok's
                        // backend fail with "Could not decrypt the provided encrypted_content".
                        // Only preserve it for a Grok→Grok port.
                        if let Some(enc) = encrypted {
                            if session.harness == Harness::Grok {
                                r.insert("encrypted".into(), json!(enc));
                            }
                        }
                        line.insert("reasoning".into(), Value::Object(r));
                        break;
                    }
                }
                // Emit assistant tool calls. The Grok parser reads `tool_calls[]` with
                // `arguments` as a JSON-encoded *string*, so re-encode the structured input.
                let calls: Vec<Value> = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        Block::ToolUse { id, name, input } => Some(json!({
                            "id": id,
                            "name": name,
                            "arguments": serde_json::to_string(input)
                                .unwrap_or_else(|_| "{}".to_string()),
                        })),
                        _ => None,
                    })
                    .collect();
                if !calls.is_empty() {
                    line.insert("tool_calls".into(), Value::Array(calls));
                }
                lines.push(Value::Object(line));
            }
            // Tool turns map to `tool_result` lines (the parser turns these back into Role::Tool).
            Role::Tool => {
                for b in &msg.content {
                    if let Block::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = b
                    {
                        lines.push(json!({
                            "type": "tool_result",
                            "tool_call_id": tool_use_id,
                            "content": content,
                        }));
                    }
                }
            }
        }
    }
    let chat_path = session_dir.join("chat_history.jsonl");
    write_jsonl(&chat_path, &lines)?;

    Ok(EmitResult {
        path: session_dir,
        new_id: new_id.clone(),
        resume_hint: Some(format!("grok --resume {new_id}")),
    })
}

/// Concatenate a message's text blocks (Grok chat_history carries plain text only).
fn grok_concat_text(msg: &crate::ir::Message) -> String {
    let mut s = String::new();
    for b in &msg.content {
        if let Block::Text { text } = b {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(text);
        }
    }
    s
}

// ------------------------------------------------------------------------------------------------
// OpenCode (parts generation)
// ------------------------------------------------------------------------------------------------

/// Epoch-millis for an optional timestamp, falling back to `now`.
fn epoch_ms(t: Option<DateTime<Utc>>) -> i64 {
    t.unwrap_or_else(Utc::now).timestamp_millis()
}

/// Emit into OpenCode's newer "parts" storage generation:
///   out_dir/session/<projectID>/ses_<id>.json
///   out_dir/message/<sid>/msg_<mid>.json
///   out_dir/part/<mid>/prt_<pid>.json
///
/// The OpenCode reader (see `harness/opencode.rs`) keys parts by *messageID*, orders messages by
/// `time.created` and parts by filename, and splits tool parts into a trailing Tool-role IR
/// message. We honour all of that. Tool results are folded back onto the originating assistant
/// message's `tool` part (matched by `tool_use_id`), since OpenCode has no standalone tool turn.
fn emit_opencode(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let new_id = opts
        .new_id
        .clone()
        .unwrap_or_else(|| format!("ses_{}", Uuid::now_v7().simple()));
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    // OpenCode shards sessions under a per-project directory. We don't reproduce its exact project
    // hashing; a deterministic dir keyed off the cwd is enough for the reader (it WalkDirs).
    let project_id = if cwd_str.is_empty() {
        "global".to_string()
    } else {
        format!("prj_{}", short_hash(&cwd_str))
    };

    let session_dir = out_dir.join("session").join(&project_id);
    let msg_dir = out_dir.join("message").join(&new_id);
    fs::create_dir_all(&session_dir)
        .with_context(|| format!("creating {}", session_dir.display()))?;
    fs::create_dir_all(&msg_dir).with_context(|| format!("creating {}", msg_dir.display()))?;

    let created = epoch_ms(session.created_at.or(session.updated_at));
    let updated = epoch_ms(session.updated_at.or(session.created_at));

    // ── session metadata file ──
    let mut meta = Map::new();
    meta.insert("id".into(), json!(new_id));
    if !cwd_str.is_empty() {
        meta.insert("directory".into(), json!(cwd_str));
    }
    if let Some(t) = &session.title {
        meta.insert("title".into(), json!(t));
    }
    meta.insert(
        "time".into(),
        json!({ "created": created, "updated": updated }),
    );
    let ses_path = session_dir.join(format!("{new_id}.json"));
    fs::write(&ses_path, serde_json::to_string_pretty(&meta)?)
        .with_context(|| format!("writing {}", ses_path.display()))?;

    // Pre-index tool results (from Role::Tool turns) by tool_use_id so we can attach them to the
    // matching `tool` part on the assistant message that produced the call.
    let mut tool_results: std::collections::HashMap<String, &Block> =
        std::collections::HashMap::new();
    for msg in &session.messages {
        if msg.role == Role::Tool {
            for b in &msg.content {
                if let Block::ToolResult { tool_use_id, .. } = b {
                    tool_results.insert(tool_use_id.clone(), b);
                }
            }
        }
    }

    let mut seq: u64 = 0;
    for msg in &session.messages {
        // Tool turns are folded into the assistant's tool parts; don't emit standalone messages.
        if msg.role == Role::Tool {
            continue;
        }
        let role = match msg.role {
            Role::Assistant => "assistant",
            Role::System => "system",
            _ => "user",
        };
        let mid = format!("msg_{}", Uuid::now_v7().simple());
        let mts = epoch_ms(msg.timestamp.or(session.created_at));

        let mut mrec = Map::new();
        mrec.insert("id".into(), json!(mid));
        mrec.insert("sessionID".into(), json!(new_id));
        mrec.insert("role".into(), json!(role));
        mrec.insert("time".into(), json!({ "created": mts }));
        if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
            mrec.insert("modelID".into(), json!(model));
        }
        let mrec_path = msg_dir.join(format!("{mid}.json"));
        fs::write(&mrec_path, serde_json::to_string_pretty(&Value::Object(mrec))?)
            .with_context(|| format!("writing {}", mrec_path.display()))?;

        let part_dir = out_dir.join("part").join(&mid);
        fs::create_dir_all(&part_dir)
            .with_context(|| format!("creating {}", part_dir.display()))?;

        let mut pidx: u64 = 0;
        let mut write_part = |part: Value| -> Result<()> {
            // Zero-padded prefix keeps the reader's filename sort == emission order.
            let pid = format!("prt_{seq:08}_{pidx:04}_{}", Uuid::now_v7().simple());
            let p = part_dir.join(format!("{pid}.json"));
            fs::write(&p, serde_json::to_string_pretty(&part)?)
                .with_context(|| format!("writing {}", p.display()))?;
            pidx += 1;
            Ok(())
        };

        for b in &msg.content {
            match b {
                Block::Text { text } => {
                    write_part(json!({ "type": "text", "text": text }))?;
                }
                Block::Thinking { text, signature, .. } => {
                    let mut p = Map::new();
                    p.insert("type".into(), json!("reasoning"));
                    p.insert("text".into(), json!(text));
                    if let Some(sig) = signature {
                        p.insert(
                            "metadata".into(),
                            json!({ "anthropic": { "signature": sig } }),
                        );
                    }
                    write_part(Value::Object(p))?;
                }
                Block::ToolUse { id, name, input } => {
                    let mut state = Map::new();
                    state.insert("input".into(), input.clone());
                    // Attach the paired result, if we have one.
                    if let Some(Block::ToolResult {
                        content, is_error, ..
                    }) = tool_results.get(id).copied()
                    {
                        if *is_error {
                            state.insert("status".into(), json!("error"));
                            state.insert("error".into(), json!(content));
                        } else {
                            state.insert("status".into(), json!("completed"));
                            state.insert("output".into(), json!(content));
                        }
                    } else {
                        state.insert("status".into(), json!("completed"));
                    }
                    write_part(json!({
                        "type": "tool",
                        "callID": id,
                        "tool": name,
                        "state": Value::Object(state),
                    }))?;
                }
                Block::Image { media_type, data_ref } => {
                    let mut p = Map::new();
                    p.insert("type".into(), json!("file"));
                    if let Some(mt) = media_type {
                        p.insert("mime".into(), json!(mt));
                    }
                    if let Some(d) = data_ref {
                        p.insert("url".into(), json!(d));
                    }
                    write_part(Value::Object(p))?;
                }
                Block::File { mime, path, source } => {
                    let mut p = Map::new();
                    p.insert("type".into(), json!("file"));
                    if let Some(mt) = mime {
                        p.insert("mime".into(), json!(mt));
                    }
                    if let Some(url) = source.as_deref().or(path.as_deref()) {
                        p.insert("url".into(), json!(url));
                    }
                    write_part(Value::Object(p))?;
                }
                Block::ToolResult { .. } => {}
            }
        }
        seq += 1;
    }

    let resume = format!("opencode (session {new_id})");
    Ok(EmitResult {
        path: ses_path,
        new_id,
        resume_hint: Some(resume),
    })
}

/// A short, stable, filesystem-safe hash of a string (FNV-1a, hex). Used for project-dir names.
fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

// ------------------------------------------------------------------------------------------------
// OpenClaw
// ------------------------------------------------------------------------------------------------

/// Emit into OpenClaw's `agents/<agentId>/sessions/<sid>.jsonl` transcript (v3 parent-linked) plus
/// a `sessions.json` index entry. Mirrors `harness/openclaw.rs`'s reader: a `{type:session,…}`
/// header line followed by `{type:message,id,parentId,timestamp,message:{role,content,…}}` lines.
fn emit_openclaw(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let new_id = opts
        .new_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let agent_id = "main";
    let cwd = effective_cwd(session, opts);
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let sessions_dir = out_dir
        .join("agents")
        .join(agent_id)
        .join("sessions");
    fs::create_dir_all(&sessions_dir)
        .with_context(|| format!("creating {}", sessions_dir.display()))?;
    let file_path = sessions_dir.join(format!("{new_id}.jsonl"));

    let created = session
        .created_at
        .or(session.updated_at)
        .unwrap_or_else(Utc::now);
    let created_iso = created.to_rfc3339_opts(SecondsFormat::Millis, true);

    let mut lines: Vec<Value> = Vec::new();
    // Header line.
    let mut header = Map::new();
    header.insert("type".into(), json!("session"));
    header.insert("version".into(), json!(3));
    header.insert("id".into(), json!(new_id));
    header.insert("timestamp".into(), json!(created_iso));
    if !cwd_str.is_empty() {
        header.insert("cwd".into(), json!(cwd_str));
    }
    lines.push(Value::Object(header));

    // Message lines, parent-linked (v3).
    let mut parent: Option<String> = None;
    for msg in &session.messages {
        let entry_id = openclaw_short_id();
        let ts = msg.timestamp.unwrap_or(created);
        let ts_iso = ts.to_rfc3339_opts(SecondsFormat::Millis, true);
        let ts_ms = ts.timestamp_millis();

        let inner = match msg.role {
            Role::User => json!({
                "role": "user",
                "content": openclaw_content_blocks(&msg.content),
                "timestamp": ts_ms,
            }),
            Role::System => json!({
                "role": "system",
                "content": openclaw_content_blocks(&msg.content),
                "timestamp": ts_ms,
            }),
            Role::Assistant => {
                let mut m = Map::new();
                m.insert("role".into(), json!("assistant"));
                m.insert("content".into(), openclaw_content_blocks(&msg.content));
                m.insert("timestamp".into(), json!(ts_ms));
                if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                    m.insert("model".into(), json!(model));
                }
                Value::Object(m)
            }
            Role::Tool => {
                // OpenClaw models each tool result as its own `toolResult` message.
                let (tool_use_id, content, is_error, tool_name) =
                    openclaw_tool_result(&msg.content);
                let mut m = Map::new();
                m.insert("role".into(), json!("toolResult"));
                m.insert("toolCallId".into(), json!(tool_use_id));
                if let Some(tn) = tool_name {
                    m.insert("toolName".into(), json!(tn));
                }
                m.insert(
                    "content".into(),
                    json!([{ "type": "text", "text": content }]),
                );
                m.insert("isError".into(), json!(is_error));
                m.insert("timestamp".into(), json!(ts_ms));
                Value::Object(m)
            }
        };

        lines.push(json!({
            "type": "message",
            "id": entry_id,
            "parentId": parent,
            "timestamp": ts_iso,
            "message": inner,
        }));
        parent = Some(entry_id);
    }

    write_jsonl(&file_path, &lines)?;

    // sessions.json index: merge-or-create.
    let index_path = sessions_dir.join("sessions.json");
    let mut index: Map<String, Value> = fs::read_to_string(&index_path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let mut entry = Map::new();
    entry.insert("sessionId".into(), json!(new_id));
    if let Some(t) = session.title.as_ref().or(session.first_user_text().as_ref()) {
        entry.insert("label".into(), json!(crate::ir::truncate(t, 80)));
    }
    if !cwd_str.is_empty() {
        entry.insert("cwd".into(), json!(cwd_str));
    }
    entry.insert(
        "updatedAt".into(),
        json!(epoch_ms(session.updated_at.or(session.created_at))),
    );
    index.insert(new_id.clone(), Value::Object(entry));
    fs::write(&index_path, serde_json::to_string_pretty(&Value::Object(index))?)
        .with_context(|| format!("writing {}", index_path.display()))?;

    Ok(EmitResult {
        path: file_path,
        new_id: new_id.clone(),
        resume_hint: Some(format!("openclaw --session {new_id}")),
    })
}

/// 8-hex-char id slice, matching OpenClaw's entry-id convention.
fn openclaw_short_id() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// Map IR blocks → an OpenClaw content array (`text`/`thinking`/`toolCall`/`image`).
fn openclaw_content_blocks(content: &[Block]) -> Value {
    let mut out = Vec::new();
    for b in content {
        match b {
            Block::Text { text } => out.push(json!({ "type": "text", "text": text })),
            Block::Thinking { text, signature, .. } => {
                let mut m = Map::new();
                m.insert("type".into(), json!("thinking"));
                m.insert("thinking".into(), json!(text));
                if let Some(sig) = signature {
                    m.insert("thinkingSignature".into(), json!(sig));
                }
                out.push(Value::Object(m));
            }
            Block::ToolUse { id, name, input } => out.push(json!({
                "type": "toolCall",
                "id": id,
                "name": name,
                "arguments": input,
            })),
            Block::Image { media_type, data_ref } => {
                let mut m = Map::new();
                m.insert("type".into(), json!("image"));
                if let Some(mt) = media_type {
                    m.insert("mimeType".into(), json!(mt));
                }
                if let Some(d) = data_ref {
                    m.insert("data".into(), json!(d));
                }
                out.push(Value::Object(m));
            }
            Block::File { path, source, .. } => {
                let label = path.as_deref().or(source.as_deref()).unwrap_or("?");
                out.push(json!({ "type": "text", "text": format!("[file: {label}]") }));
            }
            Block::ToolResult { .. } => {}
        }
    }
    Value::Array(out)
}

/// Pull the (id, content, is_error, name) out of a Tool turn's blocks.
fn openclaw_tool_result(content: &[Block]) -> (String, String, bool, Option<String>) {
    for b in content {
        if let Block::ToolResult {
            tool_use_id,
            content,
            is_error,
            tool_name,
            ..
        } = b
        {
            return (
                tool_use_id.clone(),
                content.clone(),
                *is_error,
                tool_name.clone(),
            );
        }
    }
    (String::new(), String::new(), false, None)
}

// ------------------------------------------------------------------------------------------------
// Gemini (legacy ConversationRecord)
// ------------------------------------------------------------------------------------------------

/// Emit a gemini-cli legacy chat recording: `out_dir/<sessionId>.json`, a single whole-file
/// `ConversationRecord` `{sessionId, projectHash, startTime, lastUpdated, messages[]}`. The reader
/// (`harness/gemini.rs`) requires the file to live under a `chats/` dir to be picked up by
/// `discover`, but `parse_all_str` / `parse` will read it directly from any path.
fn emit_gemini(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    let new_id = opts
        .new_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let cwd = effective_cwd(session, opts);

    // gemini stores recordings under chats/; place ours there so discover() can find it too.
    let chats_dir = out_dir.join("chats");
    fs::create_dir_all(&chats_dir)
        .with_context(|| format!("creating {}", chats_dir.display()))?;
    let file_path = chats_dir.join(format!("session-{new_id}.json"));

    let start = session
        .created_at
        .or(session.updated_at)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    let last = session
        .updated_at
        .or(session.created_at)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true);

    // Index tool results by tool_use_id so they can be folded into the producing assistant's
    // `toolCalls[].result` (the canonical legacy shape the reader pairs from).
    let mut tool_results: std::collections::HashMap<String, (String, bool)> =
        std::collections::HashMap::new();
    for msg in &session.messages {
        if msg.role == Role::Tool {
            for b in &msg.content {
                if let Block::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } = b
                {
                    tool_results.insert(tool_use_id.clone(), (content.clone(), *is_error));
                }
            }
        }
    }

    let mut messages: Vec<Value> = Vec::new();
    for msg in &session.messages {
        let ts = msg
            .timestamp
            .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true));
        match msg.role {
            Role::User | Role::System => {
                let mty = if msg.role == Role::System { "info" } else { "user" };
                let mut m = Map::new();
                m.insert("id".into(), json!(Uuid::new_v4().to_string()));
                m.insert("type".into(), json!(mty));
                if let Some(t) = &ts {
                    m.insert("timestamp".into(), json!(t));
                }
                m.insert("content".into(), gemini_parts(&msg.content));
                messages.push(Value::Object(m));
            }
            Role::Assistant => {
                let mut m = Map::new();
                m.insert("id".into(), json!(Uuid::new_v4().to_string()));
                m.insert("type".into(), json!("gemini"));
                if let Some(t) = &ts {
                    m.insert("timestamp".into(), json!(t));
                }
                if let Some(model) = msg.model.as_ref().or(session.model.as_ref()) {
                    m.insert("model".into(), json!(model));
                }
                // Thoughts (thinking) ride in `thoughts[]`, the rest in `content`.
                let mut thoughts = Vec::new();
                for b in &msg.content {
                    if let Block::Thinking { text, .. } = b {
                        thoughts.push(json!({ "subject": "", "description": text }));
                    }
                }
                if !thoughts.is_empty() {
                    m.insert("thoughts".into(), Value::Array(thoughts));
                }
                m.insert("content".into(), gemini_parts(&msg.content));
                // Tool calls → toolCalls[] (legacy ToolCallRecord), folding in the paired result so
                // the reader emits a proper Tool turn for it.
                let mut calls = Vec::new();
                for b in &msg.content {
                    if let Block::ToolUse { id, name, input } = b {
                        let mut call = Map::new();
                        call.insert("id".into(), json!(id));
                        call.insert("name".into(), json!(name));
                        call.insert("args".into(), input.clone());
                        if let Some((content, is_error)) = tool_results.get(id) {
                            call.insert(
                                "status".into(),
                                json!(if *is_error { "error" } else { "success" }),
                            );
                            call.insert(
                                "result".into(),
                                json!([{
                                    "functionResponse": {
                                        "id": id,
                                        "name": name,
                                        "response": { "output": content },
                                    }
                                }]),
                            );
                        }
                        calls.push(Value::Object(call));
                    }
                }
                if !calls.is_empty() {
                    m.insert("toolCalls".into(), Value::Array(calls));
                }
                messages.push(Value::Object(m));
            }
            // Tool turns are folded into the assistant's toolCalls[].result above.
            Role::Tool => {}
        }
    }

    let mut record = Map::new();
    record.insert("sessionId".into(), json!(new_id));
    // gemini keys recordings by an opaque projectHash; synthesize a stable one from the cwd.
    let cwd_str = cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    record.insert("projectHash".into(), json!(short_hash(&cwd_str)));
    record.insert("startTime".into(), json!(start));
    record.insert("lastUpdated".into(), json!(last));
    if let Some(t) = &session.title {
        record.insert("summary".into(), json!(t));
    }
    if let Some(c) = &cwd {
        record.insert("directories".into(), json!([c.to_string_lossy()]));
    }
    record.insert("messages".into(), Value::Array(messages));

    fs::write(&file_path, serde_json::to_string_pretty(&Value::Object(record))?)
        .with_context(|| format!("writing {}", file_path.display()))?;

    Ok(EmitResult {
        path: file_path,
        new_id: new_id.clone(),
        resume_hint: Some(format!("gemini --resume {new_id}")),
    })
}

/// Map IR blocks → a Gemini `Part[]` (`text` / thought `text` / `inlineData`). Tool calls are
/// emitted separately via `toolCalls[]`, so they're skipped here.
fn gemini_parts(content: &[Block]) -> Value {
    let mut out = Vec::new();
    for b in content {
        match b {
            Block::Text { text } => out.push(json!({ "text": text })),
            Block::Thinking { text, .. } => {
                out.push(json!({ "text": text, "thought": true }))
            }
            Block::Image { media_type, .. } => {
                let mut inline = Map::new();
                if let Some(mt) = media_type {
                    inline.insert("mimeType".into(), json!(mt));
                }
                out.push(json!({ "inlineData": Value::Object(inline) }));
            }
            Block::File { mime, path, source } => {
                let mut fd = Map::new();
                if let Some(mt) = mime {
                    fd.insert("mimeType".into(), json!(mt));
                }
                if let Some(uri) = source.as_deref().or(path.as_deref()) {
                    fd.insert("fileUri".into(), json!(uri));
                }
                out.push(json!({ "fileData": Value::Object(fd) }));
            }
            // Tool use/result handled out of band.
            Block::ToolUse { .. } | Block::ToolResult { .. } => {}
        }
    }
    Value::Array(out)
}

// ------------------------------------------------------------------------------------------------
// Hermes (SQLite)
// ------------------------------------------------------------------------------------------------

/// Emit into Hermes's `state.db` SQLite store (creating/appending). Mirrors the schema the Hermes
/// reader (`harness/hermes.rs`) expects: a `sessions` row + OpenAI-shaped `messages` rows
/// (role/content/tool_calls JSON/reasoning/timestamps as REAL unix secs). Multimodal content uses
/// the `\x00json:` sentinel prefix.
#[cfg(feature = "sqlite")]
fn emit_hermes(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    use rusqlite::{params, Connection};

    const MULTIMODAL_SENTINEL: &str = "\u{0}json:";

    let new_id = opts
        .new_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    // `out_dir` may be a directory to drop state.db into, OR the state.db file itself — the hermes
    // adapter's storage_root() returns the .db path, so `cv convert --to hermes` (no --out) passes
    // the existing file here. Treating it as a dir made `create_dir_all` fail with "File exists".
    let db_path = if out_dir.is_file() || out_dir.extension().is_some_and(|e| e == "db") {
        out_dir.to_path_buf()
    } else {
        out_dir.join("state.db")
    };
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let conn = Connection::open(&db_path)
        .with_context(|| format!("opening {}", db_path.display()))?;

    // Create the (subset of) v14 schema if the DB is fresh. `IF NOT EXISTS` makes append safe.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            source TEXT NOT NULL,
            user_id TEXT,
            model TEXT,
            model_config TEXT,
            system_prompt TEXT,
            parent_session_id TEXT,
            started_at REAL NOT NULL,
            ended_at REAL,
            end_reason TEXT,
            message_count INTEGER DEFAULT 0,
            tool_call_count INTEGER DEFAULT 0,
            input_tokens INTEGER DEFAULT 0,
            output_tokens INTEGER DEFAULT 0,
            cache_read_tokens INTEGER DEFAULT 0,
            cache_write_tokens INTEGER DEFAULT 0,
            reasoning_tokens INTEGER DEFAULT 0,
            title TEXT
         );
         CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT,
            tool_call_id TEXT,
            tool_calls TEXT,
            tool_name TEXT,
            timestamp REAL NOT NULL,
            token_count INTEGER,
            finish_reason TEXT,
            reasoning TEXT,
            reasoning_content TEXT,
            reasoning_details TEXT,
            codex_reasoning_items TEXT,
            codex_message_items TEXT,
            platform_message_id TEXT,
            observed INTEGER DEFAULT 0
         );",
    )
    .context("creating hermes schema")?;

    // FTS5 virtual tables + sync triggers, mirroring hermes_state.py's FTS_SQL / FTS_TRIGRAM_SQL.
    // Without these, a port into a *fresh* state.db is invisible to Hermes's search (which queries
    // `messages_fts`). The unicode61 table is part of bundled FTS5 and always available; the trigram
    // tokenizer is too in modern SQLite, but we still guard it so emit succeeds on any build that
    // lacks it. Created BEFORE inserting rows so the AFTER INSERT triggers populate the index.
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(content);
         CREATE TRIGGER IF NOT EXISTS messages_fts_insert AFTER INSERT ON messages BEGIN
             INSERT INTO messages_fts(rowid, content) VALUES (
                 new.id,
                 COALESCE(new.content, '') || ' ' || COALESCE(new.tool_name, '') || ' ' || COALESCE(new.tool_calls, '')
             );
         END;
         CREATE TRIGGER IF NOT EXISTS messages_fts_delete AFTER DELETE ON messages BEGIN
             DELETE FROM messages_fts WHERE rowid = old.id;
         END;
         CREATE TRIGGER IF NOT EXISTS messages_fts_update AFTER UPDATE ON messages BEGIN
             DELETE FROM messages_fts WHERE rowid = old.id;
             INSERT INTO messages_fts(rowid, content) VALUES (
                 new.id,
                 COALESCE(new.content, '') || ' ' || COALESCE(new.tool_name, '') || ' ' || COALESCE(new.tool_calls, '')
             );
         END;",
    )
    .context("creating hermes messages_fts (FTS5)")?;

    // Trigram table for CJK/substring search. Guarded: if the trigram tokenizer isn't compiled in,
    // skip it (and its triggers) rather than failing the whole emit. The unicode61 table above is
    // enough for Hermes to find ASCII content; the trigram index is a CJK nicety.
    let trigram_sql =
        "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts_trigram USING fts5(content, tokenize='trigram');
         CREATE TRIGGER IF NOT EXISTS messages_fts_trigram_insert AFTER INSERT ON messages BEGIN
             INSERT INTO messages_fts_trigram(rowid, content) VALUES (
                 new.id,
                 COALESCE(new.content, '') || ' ' || COALESCE(new.tool_name, '') || ' ' || COALESCE(new.tool_calls, '')
             );
         END;
         CREATE TRIGGER IF NOT EXISTS messages_fts_trigram_delete AFTER DELETE ON messages BEGIN
             DELETE FROM messages_fts_trigram WHERE rowid = old.id;
         END;
         CREATE TRIGGER IF NOT EXISTS messages_fts_trigram_update AFTER UPDATE ON messages BEGIN
             DELETE FROM messages_fts_trigram WHERE rowid = old.id;
             INSERT INTO messages_fts_trigram(rowid, content) VALUES (
                 new.id,
                 COALESCE(new.content, '') || ' ' || COALESCE(new.tool_name, '') || ' ' || COALESCE(new.tool_calls, '')
             );
         END;";
    if let Err(e) = conn.execute_batch(trigram_sql) {
        eprintln!("cv: hermes trigram FTS unavailable, skipping ({e}); unicode61 FTS still active");
    }

    let started = session
        .created_at
        .or(session.updated_at)
        .unwrap_or_else(Utc::now)
        .timestamp_millis() as f64
        / 1000.0;
    let ended = session
        .updated_at
        .or(session.created_at)
        .map(|t| t.timestamp_millis() as f64 / 1000.0);

    conn.execute(
        "INSERT OR REPLACE INTO sessions (id, source, model, started_at, ended_at, message_count, title) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            new_id,
            "claurdvoyant",
            session.model,
            started,
            ended,
            session.messages.len() as i64,
            session.title,
        ],
    )
    .context("inserting hermes session")?;

    let mut base_ts = started;
    for msg in &session.messages {
        let ts = msg
            .timestamp
            .map(|t| t.timestamp_millis() as f64 / 1000.0)
            .unwrap_or_else(|| {
                base_ts += 0.001;
                base_ts
            });

        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
            Role::Tool => "tool",
        };

        // Reasoning text/encrypted from any Thinking blocks.
        let mut reasoning = String::new();
        let mut reasoning_details: Option<String> = None;
        for b in &msg.content {
            if let Block::Thinking { text, encrypted, .. } = b {
                if !text.is_empty() {
                    if !reasoning.is_empty() {
                        reasoning.push_str("\n\n");
                    }
                    reasoning.push_str(text);
                }
                if let Some(enc) = encrypted {
                    reasoning_details = Some(
                        json!([{ "type": "reasoning.encrypted_content", "encrypted_content": enc }])
                            .to_string(),
                    );
                }
            }
        }
        let reasoning = (!reasoning.is_empty()).then_some(reasoning);

        if msg.role == Role::Tool {
            for b in &msg.content {
                if let Block::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } = b
                {
                    conn.execute(
                        "INSERT INTO messages (session_id, role, content, tool_call_id, timestamp) \
                         VALUES (?1, 'tool', ?2, ?3, ?4)",
                        params![new_id, content, tool_use_id, ts],
                    )
                    .context("inserting hermes tool message")?;
                }
            }
            continue;
        }

        // Content: plain text, or the multimodal sentinel form if there are images.
        let has_image = msg.content.iter().any(|b| matches!(b, Block::Image { .. }));
        let content: Option<String> = if has_image {
            let mut parts = Vec::new();
            for b in &msg.content {
                match b {
                    Block::Text { text } => {
                        parts.push(json!({ "type": "text", "text": text }))
                    }
                    Block::Image { data_ref, .. } => {
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": { "url": data_ref.clone().unwrap_or_default() },
                        }));
                    }
                    _ => {}
                }
            }
            Some(format!(
                "{MULTIMODAL_SENTINEL}{}",
                serde_json::to_string(&Value::Array(parts))?
            ))
        } else {
            msg.text()
        };

        // Assistant tool calls → OpenAI-shaped tool_calls JSON.
        let tool_calls: Option<String> = {
            let calls: Vec<Value> = msg
                .content
                .iter()
                .filter_map(|b| match b {
                    Block::ToolUse { id, name, input } => Some(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                        },
                    })),
                    _ => None,
                })
                .collect();
            (!calls.is_empty()).then(|| Value::Array(calls).to_string())
        };

        conn.execute(
            "INSERT INTO messages (session_id, role, content, tool_calls, timestamp, reasoning, reasoning_details) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![new_id, role, content, tool_calls, ts, reasoning, reasoning_details],
        )
        .context("inserting hermes message")?;
    }

    Ok(EmitResult {
        path: db_path,
        new_id: new_id.clone(),
        resume_hint: Some(format!("hermes --resume {new_id}")),
    })
}

// ------------------------------------------------------------------------------------------------
// shared
// ------------------------------------------------------------------------------------------------

fn write_jsonl(path: &Path, lines: &[Value]) -> Result<()> {
    let mut buf = String::new();
    for v in lines {
        buf.push_str(&serde_json::to_string(v)?);
        buf.push('\n');
    }
    fs::write(path, buf).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{
        Adapter, claude::Claude, codex::Codex, gemini::Gemini, grok::Grok,
        opencode::OpenCode, openclaw::OpenClaw,
    };
    use crate::ir::*;

    fn temp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("cv-emit-{}", Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample_session(harness: Harness) -> Session {
        let mut sys = Message::new(Role::System);
        sys.content.push(Block::Text {
            text: "you are a helpful assistant".into(),
        });

        let mut user = Message::new(Role::User);
        user.content.push(Block::Text {
            text: "list the files please".into(),
        });

        let mut asst = Message::new(Role::Assistant);
        asst.model = Some("test-model".into());
        asst.content.push(Block::Thinking {
            text: "I should run ls".into(),
            signature: None,
            encrypted: None,
            redacted: false,
        });
        asst.content.push(Block::Text {
            text: "Sure, listing now.".into(),
        });
        asst.content.push(Block::ToolUse {
            id: "call_1".into(),
            name: "run_shell".into(),
            input: serde_json::json!({ "cmd": "ls" }),
        });

        let mut tool = Message::new(Role::Tool);
        tool.content.push(Block::ToolResult {
            tool_use_id: "call_1".into(),
            content: "file_a.txt\nfile_b.txt".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        });

        Session {
            id: "orig-id".into(),
            harness,
            cwd: Some(PathBuf::from("/Users/test/project")),
            title: Some("a test session".into()),
            created_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
            model: Some("test-model".into()),
            git: Some(GitInfo {
                branch: Some("main".into()),
                commit: None,
                remote: None,
            }),
            messages: vec![sys, user, asst, tool],
            source_path: None,
            extra: serde_json::Map::new(),
        }
    }

    fn texts(session: &Session, role: Role) -> Vec<String> {
        session
            .messages
            .iter()
            .filter(|m| m.role == role)
            .filter_map(|m| m.text())
            .collect()
    }

    fn tool_names(session: &Session) -> Vec<String> {
        let mut out = Vec::new();
        for m in &session.messages {
            for b in &m.content {
                if let Block::ToolUse { name, .. } = b {
                    out.push(name.clone());
                }
            }
        }
        out
    }

    #[test]
    fn claude_round_trip() {
        let s = sample_session(Harness::Claude);
        let out = temp_dir();
        let res = emit(&s, Harness::Claude, &out, &EmitOptions::default()).unwrap();
        assert!(res.path.exists());

        // Re-parse via the Claude adapter. id is derived from the filename stem.
        let id = res.path.file_stem().unwrap().to_str().unwrap().to_string();
        assert_eq!(id, res.new_id);
        let r = SessionRef {
            id,
            harness: Harness::Claude,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Claude::new().parse(&r).unwrap();

        assert_eq!(parsed.title.as_deref(), Some("a test session"));
        assert_eq!(parsed.cwd, Some(PathBuf::from("/Users/test/project")));
        assert_eq!(texts(&parsed, Role::User), vec!["list the files please"]);
        assert_eq!(texts(&parsed, Role::Assistant), vec!["Sure, listing now."]);
        assert_eq!(tool_names(&parsed), vec!["run_shell"]);
        // The Tool turn (tool_result) should survive as a Role::Tool message.
        let tool_results: Vec<_> = parsed
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .collect();
        assert_eq!(tool_results.len(), 1);
        // Thinking survives.
        assert!(parsed.messages.iter().any(|m| m
            .content
            .iter()
            .any(|b| matches!(b, Block::Thinking { .. }))));
        // gitBranch round-trips.
        assert_eq!(
            parsed.git.and_then(|g| g.branch),
            Some("main".to_string())
        );
    }

    #[test]
    fn claude_encode_cwd_format() {
        assert_eq!(
            claude_encode_cwd(Path::new("/Users/test/my.project")),
            "-Users-test-my-project"
        );
    }

    #[test]
    fn codex_round_trip() {
        let s = sample_session(Harness::Codex);
        let out = temp_dir();
        let res = emit(&s, Harness::Codex, &out, &EmitOptions::default()).unwrap();
        assert!(res.path.exists());

        let r = SessionRef {
            id: String::new(),
            harness: Harness::Codex,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Codex::new().parse(&r).unwrap();

        assert_eq!(parsed.id, res.new_id);
        assert_eq!(parsed.cwd, Some(PathBuf::from("/Users/test/project")));
        // event_msg user/assistant text survives.
        assert_eq!(texts(&parsed, Role::User), vec!["list the files please"]);
        assert_eq!(texts(&parsed, Role::Assistant), vec!["Sure, listing now."]);
        assert_eq!(tool_names(&parsed), vec!["run_shell"]);
        // function_call_output round-trips as a Tool message.
        let tool_results: Vec<_> = parsed
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .collect();
        assert_eq!(tool_results.len(), 1);
        if let Block::ToolResult { content, .. } = &tool_results[0].content[0] {
            assert_eq!(content, "file_a.txt\nfile_b.txt");
        } else {
            panic!("expected tool result");
        }
        // reasoning survives.
        assert!(parsed.messages.iter().any(|m| m
            .content
            .iter()
            .any(|b| matches!(b, Block::Thinking { .. }))));
        // git branch round-trips.
        assert_eq!(parsed.git.and_then(|g| g.branch), Some("main".to_string()));
        // System turn (developer) survives.
        assert!(parsed.messages.iter().any(|m| m.role == Role::System));
    }

    #[test]
    fn grok_round_trip() {
        let s = sample_session(Harness::Grok);
        let out = temp_dir();
        let res = emit(&s, Harness::Grok, &out, &EmitOptions::default()).unwrap();
        assert!(res.path.is_dir());
        assert!(res.path.join("summary.json").exists());
        assert!(res.path.join("chat_history.jsonl").exists());

        // r.path is the session directory for Grok.
        let r = SessionRef {
            id: res.new_id.clone(),
            harness: Harness::Grok,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Grok::new().parse(&r).unwrap();

        assert_eq!(parsed.id, res.new_id);
        assert_eq!(parsed.cwd, Some(PathBuf::from("/Users/test/project")));
        assert_eq!(texts(&parsed, Role::User), vec!["list the files please"]);
        assert_eq!(texts(&parsed, Role::Assistant), vec!["Sure, listing now."]);
        assert_eq!(parsed.git.clone().and_then(|g| g.branch), Some("main".to_string()));
        // Thinking/reasoning survives on the assistant turn.
        assert!(parsed.messages.iter().any(|m| m
            .content
            .iter()
            .any(|b| matches!(b, Block::Thinking { .. }))));
        // Tool calls now SURVIVE (previously emit dropped all tool turns).
        assert_eq!(tool_names(&parsed), vec!["run_shell"]);
        // And the assistant tool call's arguments round-trip.
        let tool_use_input = parsed.messages.iter().find_map(|m| {
            m.content.iter().find_map(|b| match b {
                Block::ToolUse { input, .. } => Some(input.clone()),
                _ => None,
            })
        });
        assert_eq!(
            tool_use_input.as_ref().and_then(|v| v.get("cmd")).and_then(|v| v.as_str()),
            Some("ls")
        );
        // The tool RESULT survives as a Role::Tool message with its content intact.
        let tool_results: Vec<_> = parsed
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .collect();
        assert_eq!(tool_results.len(), 1);
        if let Block::ToolResult { content, tool_use_id, .. } = &tool_results[0].content[0] {
            assert_eq!(content, "file_a.txt\nfile_b.txt");
            assert_eq!(tool_use_id, "call_1");
        } else {
            panic!("expected tool result");
        }
    }

    #[test]
    fn new_cwd_and_new_id_overrides() {
        let s = sample_session(Harness::Claude);
        let out = temp_dir();
        let opts = EmitOptions {
            new_cwd: Some(PathBuf::from("/tmp/rehomed")),
            new_id: Some("forced-id".into()),
        };
        let res = emit(&s, Harness::Claude, &out, &opts).unwrap();
        assert_eq!(res.new_id, "forced-id");
        assert!(res.path.to_string_lossy().contains("-tmp-rehomed"));
        assert!(res.path.ends_with("forced-id.jsonl"));
    }

    #[test]
    fn opencode_round_trip() {
        // OpenCode::new() reads $HOME/.local/share/opencode/storage and its parse() resolves
        // message/part dirs relative to that root. Redirect HOME at a temp home and emit there.
        // Serialize HOME mutation across this binary's threads.
        use std::sync::Mutex;
        static HOME_LOCK: Mutex<()> = Mutex::new(());
        let _guard = HOME_LOCK.lock().unwrap();

        let home = temp_dir();
        let storage = home.join(".local/share/opencode/storage");
        fs::create_dir_all(&storage).unwrap();

        let s = sample_session(Harness::OpenCode);
        let res = emit(&s, Harness::OpenCode, &storage, &EmitOptions::default()).unwrap();
        assert!(res.path.exists());

        let prev_home = std::env::var_os("HOME");
        // SAFETY: guarded by HOME_LOCK; restored before releasing the lock.
        unsafe {
            std::env::set_var("HOME", &home);
        }
        let oc = OpenCode::new();
        let refs = oc.discover().unwrap();
        let parsed = refs
            .iter()
            .find(|r| r.id == res.new_id)
            .map(|r| oc.parse(r).unwrap());
        unsafe {
            match prev_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
        drop(_guard);

        let parsed = parsed.expect("emitted opencode session should be discoverable");
        assert_eq!(parsed.cwd, Some(PathBuf::from("/Users/test/project")));
        assert_eq!(texts(&parsed, Role::User), vec!["list the files please"]);
        assert_eq!(texts(&parsed, Role::Assistant), vec!["Sure, listing now."]);
        assert_eq!(tool_names(&parsed), vec!["run_shell"]);
        // tool result split into a Tool turn
        let tool_results: Vec<_> = parsed
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .collect();
        assert_eq!(tool_results.len(), 1);
        if let Block::ToolResult { content, .. } = &tool_results[0].content[0] {
            assert_eq!(content, "file_a.txt\nfile_b.txt");
        } else {
            panic!("expected tool result");
        }
        // reasoning survives
        assert!(parsed.messages.iter().any(|m| m
            .content
            .iter()
            .any(|b| matches!(b, Block::Thinking { .. }))));
    }

    #[test]
    fn openclaw_round_trip() {
        let s = sample_session(Harness::OpenClaw);
        let out = temp_dir();
        let res = emit(&s, Harness::OpenClaw, &out, &EmitOptions::default()).unwrap();
        assert!(res.path.exists());
        // sessions.json index written
        assert!(res.path.parent().unwrap().join("sessions.json").exists());

        // OpenClaw::parse reads r.path directly; cwd/created come from the header line.
        let r = SessionRef {
            id: res.new_id.clone(),
            harness: Harness::OpenClaw,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = OpenClaw::new().parse(&r).unwrap();

        assert_eq!(parsed.id, res.new_id);
        assert_eq!(parsed.cwd, Some(PathBuf::from("/Users/test/project")));
        assert_eq!(texts(&parsed, Role::User), vec!["list the files please"]);
        assert_eq!(texts(&parsed, Role::Assistant), vec!["Sure, listing now."]);
        assert_eq!(tool_names(&parsed), vec!["run_shell"]);
        // tool result → toolResult message (Role::Tool)
        let tool_results: Vec<_> = parsed
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .collect();
        assert_eq!(tool_results.len(), 1);
        // thinking survives
        assert!(parsed.messages.iter().any(|m| m
            .content
            .iter()
            .any(|b| matches!(b, Block::Thinking { .. }))));
        // model promoted
        assert_eq!(parsed.model.as_deref(), Some("test-model"));
    }

    #[test]
    fn gemini_round_trip() {
        let s = sample_session(Harness::Gemini);
        let out = temp_dir();
        let res = emit(&s, Harness::Gemini, &out, &EmitOptions::default()).unwrap();
        assert!(res.path.exists());

        // gemini parse keys off the `chats/` dir in the path; emit places the file there.
        let r = SessionRef {
            id: res.new_id.clone(),
            harness: Harness::Gemini,
            path: res.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let parsed = Gemini::new().parse(&r).unwrap();

        assert_eq!(parsed.id, res.new_id);
        assert_eq!(parsed.title.as_deref(), Some("a test session"));
        assert_eq!(parsed.cwd, Some(PathBuf::from("/Users/test/project")));
        assert_eq!(texts(&parsed, Role::User), vec!["list the files please"]);
        assert_eq!(texts(&parsed, Role::Assistant), vec!["Sure, listing now."]);
        assert_eq!(tool_names(&parsed), vec!["run_shell"]);
        assert_eq!(parsed.model.as_deref(), Some("test-model"));
        // tool result is retagged to a Tool turn by the reader
        assert!(parsed.messages.iter().any(|m| m.role == Role::Tool
            && m.content.iter().any(|b| matches!(b, Block::ToolResult { content, .. }
                if content == "file_a.txt\nfile_b.txt"))));
        // thinking → thoughts survives
        assert!(parsed.messages.iter().any(|m| m
            .content
            .iter()
            .any(|b| matches!(b, Block::Thinking { .. }))));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn hermes_round_trip() {
        use crate::harness::hermes::Hermes;
        let s = sample_session(Harness::Hermes);
        let out = temp_dir();
        let res = emit(&s, Harness::Hermes, &out, &EmitOptions::default()).unwrap();
        assert!(res.path.exists());
        assert_eq!(res.path.file_name().unwrap(), "state.db");

        // Point Hermes at the emitted DB via HERMES_HOME (its new() honours it).
        use std::sync::Mutex;
        static HERMES_LOCK: Mutex<()> = Mutex::new(());
        let _guard = HERMES_LOCK.lock().unwrap();
        let prev = std::env::var_os("HERMES_HOME");
        unsafe {
            std::env::set_var("HERMES_HOME", &out);
        }
        let h = Hermes::new();
        let refs = h.discover().unwrap();
        let parsed = refs
            .iter()
            .find(|r| r.id == res.new_id)
            .map(|r| h.parse(r).unwrap());
        unsafe {
            match prev {
                Some(p) => std::env::set_var("HERMES_HOME", p),
                None => std::env::remove_var("HERMES_HOME"),
            }
        }
        drop(_guard);

        let parsed = parsed.expect("emitted hermes session should be discoverable");
        assert_eq!(parsed.id, res.new_id);
        assert_eq!(parsed.title.as_deref(), Some("a test session"));
        assert_eq!(parsed.model.as_deref(), Some("test-model"));
        assert_eq!(texts(&parsed, Role::User), vec!["list the files please"]);
        assert_eq!(texts(&parsed, Role::Assistant), vec!["Sure, listing now."]);
        assert_eq!(tool_names(&parsed), vec!["run_shell"]);
        // tool result row → Tool turn
        assert!(parsed.messages.iter().any(|m| m.role == Role::Tool
            && m.content.iter().any(|b| matches!(b, Block::ToolResult { content, .. }
                if content == "file_a.txt\nfile_b.txt"))));
        // reasoning → Thinking
        assert!(parsed.messages.iter().any(|m| m
            .content
            .iter()
            .any(|b| matches!(b, Block::Thinking { .. }))));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn hermes_emit_creates_fts_and_is_searchable() {
        use rusqlite::{Connection, OpenFlags};

        let s = sample_session(Harness::Hermes);
        let out = temp_dir();
        let res = emit(&s, Harness::Hermes, &out, &EmitOptions::default()).unwrap();
        let db = res.path.clone();
        assert_eq!(db.file_name().unwrap(), "state.db");

        let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();

        // The FTS virtual tables must exist (a fresh port is invisible to Hermes search without them).
        let table_exists = |name: &str| -> bool {
            conn.query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                [name],
                |_| Ok(()),
            )
            .is_ok()
        };
        assert!(table_exists("messages_fts"), "messages_fts must exist");
        // trigram is guarded; assert it exists when the tokenizer is available (it is in bundled FTS5).
        assert!(
            table_exists("messages_fts_trigram"),
            "messages_fts_trigram should exist with bundled FTS5"
        );

        // The triggers must have populated the index: an FTS MATCH finds an emitted message.
        let cnt: i64 = conn
            .query_row(
                "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH 'files'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(cnt >= 1, "FTS query for 'files' should match the emitted user message");

        // And the trigram index covers a substring of the assistant text.
        let cnt_t: i64 = conn
            .query_row(
                "SELECT count(*) FROM messages_fts_trigram WHERE messages_fts_trigram MATCH 'listing'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(cnt_t >= 1, "trigram FTS should find 'listing' substring");
    }

    /// A session whose only non-text payload is an image — a lossy target (Grok stores plain text
    /// only) should report the image as dropped.
    fn image_session() -> Session {
        let mut user = Message::new(Role::User);
        user.content.push(Block::Text { text: "look at this".into() });
        user.content.push(Block::Image {
            media_type: Some("image/png".into()),
            data_ref: Some("data:image/png;base64,AAAA".into()),
        });
        let mut asst = Message::new(Role::Assistant);
        asst.content.push(Block::Text { text: "nice picture".into() });

        Session {
            id: "img-id".into(),
            harness: Harness::Grok,
            cwd: Some(PathBuf::from("/Users/test/project")),
            title: Some("image session".into()),
            created_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
            model: Some("test-model".into()),
            git: None,
            messages: vec![user, asst],
            source_path: None,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn emit_verified_reports_lossy_grok() {
        // An image-bearing session ported to Grok (plain-text chat_history) should warn that the
        // image was dropped — the canonical lossy case.
        let img = image_session();
        let out = temp_dir();
        let (_res, warnings) =
            emit_verified(&img, Harness::Grok, &out, &EmitOptions::default()).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("image")),
            "expected an image-dropped warning, got: {warnings:?}"
        );

        // A tools+text+system+reasoning Grok session now round-trips cleanly (post tool-emit fix):
        // Grok keeps system turns, tool calls, tool results, and reasoning.
        let clean = sample_session(Harness::Grok);
        let out2 = temp_dir();
        let (_r2, w2) =
            emit_verified(&clean, Harness::Grok, &out2, &EmitOptions::default()).unwrap();
        assert!(w2.is_empty(), "expected clean Grok round-trip, got: {w2:?}");
    }

    #[test]
    fn emit_verified_clean_for_openclaw() {
        // OpenClaw represents text, thinking, tool calls/results, system turns — a clean round-trip.
        let s = sample_session(Harness::OpenClaw);
        let out = temp_dir();
        let (_res, warnings) =
            emit_verified(&s, Harness::OpenClaw, &out, &EmitOptions::default()).unwrap();
        // No tool/image/reasoning loss; OpenClaw keeps system turns and the title.
        assert!(
            warnings.is_empty(),
            "expected a clean round-trip for OpenClaw, got: {warnings:?}"
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn emit_verified_clean_for_hermes() {
        // Hermes keeps tools, reasoning, system turns, and title — should round-trip cleanly.
        let s = sample_session(Harness::Hermes);
        let out = temp_dir();
        let (_res, warnings) =
            emit_verified(&s, Harness::Hermes, &out, &EmitOptions::default()).unwrap();
        assert!(
            warnings.is_empty(),
            "expected a clean round-trip for Hermes, got: {warnings:?}"
        );
    }
}
