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
use crate::ir::{Block, GitInfo, Harness, Role, Session};
use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
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
        other => anyhow::bail!("emit to {other} not implemented yet"),
    }
}

/// Which targets [`emit`] can currently write.
pub fn supported_targets() -> &'static [Harness] {
    &[Harness::Claude, Harness::Codex, Harness::Grok]
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
            Block::Image { media_type, .. } => out.push(json!({
                "type": "image",
                "source": { "media_type": media_type },
            })),
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
                lines.push(codex_event_msg(
                    &ts,
                    json!({ "type": "user_message", "message": text }),
                ));
            }
            Role::Assistant => {
                // Split assistant content into NL text (event_msg) and structured items.
                for b in &msg.content {
                    match b {
                        Block::Text { text } => lines.push(codex_event_msg(
                            &ts,
                            json!({ "type": "agent_message", "message": text }),
                        )),
                        Block::Thinking { text, encrypted, .. } => {
                            let mut p = Map::new();
                            p.insert("type".into(), json!("reasoning"));
                            p.insert(
                                "summary".into(),
                                json!([{ "type": "summary_text", "text": text }]),
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
    summary.insert(
        "num_chat_messages".into(),
        json!(session.messages.len()),
    );
    if let Some(model) = &session.model {
        summary.insert("current_model_id".into(), json!(model));
    }
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
    if let Some(t) = &session.title {
        summary.insert("session_summary".into(), json!(t));
    }
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
                        if let Some(enc) = encrypted {
                            r.insert("encrypted".into(), json!(enc));
                        }
                        line.insert("reasoning".into(), Value::Object(r));
                        break;
                    }
                }
                lines.push(Value::Object(line));
            }
            // Grok's chat_history doesn't model tool turns; best-effort drops them.
            Role::Tool => {}
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
    use crate::harness::{Adapter, claude::Claude, codex::Codex, grok::Grok};
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
        assert_eq!(parsed.git.and_then(|g| g.branch), Some("main".to_string()));
        // Thinking/reasoning survives on the assistant turn.
        assert!(parsed.messages.iter().any(|m| m
            .content
            .iter()
            .any(|b| matches!(b, Block::Thinking { .. }))));
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
}
