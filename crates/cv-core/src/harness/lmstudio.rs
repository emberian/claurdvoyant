//! LM Studio desktop app adapter — local chats under `~/.lmstudio/conversations/`.
//!
//! LM Studio is a closed-source local-LLM desktop app, but it stores its chat transcripts as
//! **plaintext JSON**, one file per conversation:
//!
//! - **Transcripts:** `~/.lmstudio/conversations/<ms-epoch>.conversation.json`. The filename stem is
//!   a millisecond-epoch id (also the chat's `createdAt`). There is no `cwd` — it's a chat app, not
//!   an agent, so [`Session::cwd`] is always `None`.
//! - **Top-level fields:** `name` (user/auto title, may be `null` or `"Untitled"`), `createdAt`
//!   (ms epoch), `messages[]`, `systemPrompt`, `tokenCount`, `pinned`, `perChatPredictionConfig`
//!   (holds a per-chat `llm.prediction.systemPrompt` override under `fields[]`).
//! - **`messages[]`:** each entry is `{versions:[...], currentlySelected:<idx>}`. LM Studio keeps
//!   every regenerated/edited variant of a turn in `versions[]`; we render the `currentlySelected`
//!   one (falling back to the last).
//! - **A version** is `{type, role, ...}`:
//!     * `type:"singleStep"` (typically `role:"user"`): `content:[{type:"text",text} |
//!       {type:"file",fileIdentifier|identifier,name?,fileType,sizeBytes}]`.
//!     * `type:"multiStep"` (`role:"assistant"`): `steps:[...]` where each step `type` ∈
//!       `contentBlock` | `debugInfoBlock` | `status`.
//!         - `contentBlock`: `content:[{type:"text",text}]`. A `style.type=="thinking"` marks the
//!           block as reasoning (`<think>`); these get [`Block::Thinking`]. `genInfo` carries the
//!           model (`indexedModelIdentifier`/`identifier`, e.g. `openai/gpt-oss-120b`) and `stats`
//!           (`promptTokensCount`/`predictedTokensCount` → [`Usage`]). The `stepIdentifier` is a
//!           `"<ms-epoch>-<rand>"` string; its prefix is a usable per-step timestamp.
//!         - `debugInfoBlock` / `status`: app-internal (naming technique, prompt-processing status);
//!           ignored for content.
//!
//! Fidelity notes / variants (sniffed by content, never a version field):
//! - There are **no first-class tool-call structures** on disk. Models like gpt-oss embed tool calls
//!   inline in the assistant text via channel markers; we preserve that text verbatim (no `ToolUse`).
//! - File attachments are references only (`fileIdentifier`), not inlined bytes; images → [`Block::Image`],
//!   other files → [`Block::File`]. The actual bytes live under `~/.lmstudio/.internal/files/`.
//! - Older/newer schema drift is handled by being totally tolerant: unknown step/part types are
//!   skipped, a `content` that is a string OR an array of `{type,text}` parts both work, and a
//!   missing `~/.lmstudio` dir yields an empty discover (never a panic).

use super::Adapter;
use crate::ir::*;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub struct LmStudio {
    root: Option<PathBuf>,
}

impl LmStudio {
    pub fn new() -> Self {
        let root = dirs::home_dir().map(|h| h.join(".lmstudio").join("conversations"));
        LmStudio {
            root: root.filter(|p| p.exists()),
        }
    }
}

impl Default for LmStudio {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for LmStudio {
    fn harness(&self) -> Harness {
        Harness::LmStudio
    }

    fn storage_root(&self) -> Option<PathBuf> {
        // Detect the install even if no conversations dir exists yet.
        self.root
            .clone()
            .or_else(|| dirs::home_dir().map(|h| h.join(".lmstudio")).filter(|p| p.exists()))
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(root) = &self.root else {
            return Ok(vec![]);
        };
        let mut out = Vec::new();
        let entries = match fs::read_dir(root) {
            Ok(e) => e,
            Err(_) => return Ok(vec![]),
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if !is_conversation_file(&path) {
                continue;
            }
            match scan(&path) {
                Ok(Some(r)) => out.push(r),
                Ok(None) => {}
                Err(e) => eprintln!("cv: skipping {}: {e:#}", path.display()),
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        let text = fs::read_to_string(&r.path)
            .with_context(|| format!("reading {}", r.path.display()))?;
        let v: Value = serde_json::from_str(&text)
            .with_context(|| format!("parsing {}", r.path.display()))?;

        let created_at = v.get("createdAt").and_then(ms_to_dt);
        let updated_at = file_mtime(&r.path).or(created_at);

        let mut messages = Vec::new();
        let mut session_model: Option<String> = None;
        if let Some(arr) = v.get("messages").and_then(Value::as_array) {
            for raw in arr {
                if let Some(m) = parse_message(raw, &mut session_model) {
                    messages.push(m);
                }
            }
        }

        let mut extra = serde_json::Map::new();
        // Preserve the per-chat / global system prompt so it isn't silently dropped.
        if let Some(sp) = system_prompt(&v) {
            if !sp.trim().is_empty() {
                extra.insert("systemPrompt".into(), Value::String(sp));
            }
        }
        if let Some(p) = v.get("pinned").and_then(Value::as_bool).filter(|p| *p) {
            extra.insert("pinned".into(), Value::Bool(p));
        }

        Ok(Session {
            id: r.id.clone(),
            harness: Harness::LmStudio,
            cwd: None, // LM Studio is a chat app: no working directory.
            title: r.title.clone(),
            created_at,
            updated_at,
            model: session_model,
            git: None,
            messages,
            source_path: Some(r.path.clone()),
            extra,
        })
    }
}

/// Files we treat as conversations: `*.conversation.json` (the real format) and, defensively, any
/// `*.json` directly in the conversations dir.
fn is_conversation_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => name.ends_with(".conversation.json") || name.ends_with(".json"),
        None => false,
    }
}

/// Build a cheap [`SessionRef`] without parsing every step.
fn scan(path: &Path) -> Result<Option<SessionRef>> {
    let text = fs::read_to_string(path)?;
    let v: Value = serde_json::from_str(&text)?;

    let id = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            n.trim_end_matches(".conversation.json")
                .trim_end_matches(".json")
                .to_string()
        })
        .unwrap_or_default();

    let title = chat_title(&v);
    let created_at = v.get("createdAt").and_then(ms_to_dt);
    let updated_at = file_mtime(path).or(created_at);
    let message_count = v
        .get("messages")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);

    Ok(Some(SessionRef {
        id,
        harness: Harness::LmStudio,
        path: path.to_path_buf(),
        cwd: None,
        title,
        created_at,
        updated_at,
        message_count,
    }))
}

/// Title: the explicit `name` (when meaningful), else the first user text. LM Studio writes
/// `"Untitled"` / `"Untitled - Branched"` for un-named chats, so we treat those as absent.
fn chat_title(v: &Value) -> Option<String> {
    if let Some(name) = v.get("name").and_then(Value::as_str) {
        let t = name.trim();
        if !t.is_empty() && t != "Untitled" && !t.starts_with("Untitled -") {
            return Some(crate::ir::truncate(t, 80));
        }
    }
    // Fall back to the first user text part.
    let messages = v.get("messages").and_then(Value::as_array)?;
    for msg in messages {
        let version = selected_version(msg)?;
        if version.get("role").and_then(Value::as_str) == Some("user") {
            let t = collect_text_parts(version.get("content"));
            if !t.trim().is_empty() {
                return Some(crate::ir::truncate(&t, 80));
            }
        }
    }
    None
}

/// The system prompt: the per-chat override (`perChatPredictionConfig.fields[key==
/// llm.prediction.systemPrompt]`) takes precedence over the top-level `systemPrompt`.
fn system_prompt(v: &Value) -> Option<String> {
    if let Some(fields) = v
        .pointer("/perChatPredictionConfig/fields")
        .and_then(Value::as_array)
    {
        for f in fields {
            if f.get("key").and_then(Value::as_str) == Some("llm.prediction.systemPrompt") {
                if let Some(val) = f.get("value").and_then(Value::as_str) {
                    if !val.trim().is_empty() {
                        return Some(val.to_string());
                    }
                }
            }
        }
    }
    v.get("systemPrompt")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

/// Pick the `currentlySelected` version of a message turn, falling back to the last / first.
fn selected_version(msg: &Value) -> Option<&Value> {
    let versions = msg.get("versions").and_then(Value::as_array)?;
    if versions.is_empty() {
        return None;
    }
    let idx = msg
        .get("currentlySelected")
        .and_then(Value::as_u64)
        .map(|i| i as usize)
        .filter(|i| *i < versions.len())
        .unwrap_or(versions.len() - 1);
    versions.get(idx)
}

/// Parse one message turn into an IR [`Message`]. Updates `session_model` with the first model seen.
fn parse_message(msg: &Value, session_model: &mut Option<String>) -> Option<Message> {
    let version = selected_version(msg)?;
    let role = match version.get("role").and_then(Value::as_str) {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        Some("system") => Role::System,
        _ => return None,
    };
    let mut m = Message::new(role);

    match version.get("type").and_then(Value::as_str) {
        // Assistant turns: a list of steps (contentBlock / debugInfoBlock / status).
        Some("multiStep") => {
            if let Some(steps) = version.get("steps").and_then(Value::as_array) {
                for step in steps {
                    parse_step(step, &mut m, session_model);
                }
            }
        }
        // User (and any other single-step) turns: content parts directly on the version.
        _ => {
            push_content_parts(version.get("content"), &mut m, false);
        }
    }

    if m.content.is_empty() {
        return None;
    }
    Some(m)
}

/// Parse one assistant `step`. Only `contentBlock` carries content; others are app-internal.
fn parse_step(step: &Value, m: &mut Message, session_model: &mut Option<String>) {
    if step.get("type").and_then(Value::as_str) != Some("contentBlock") {
        return; // debugInfoBlock / status — nothing to render.
    }

    // Model + usage live on genInfo.
    if let Some(gen) = step.get("genInfo") {
        if session_model.is_none() {
            if let Some(model) = gen
                .get("indexedModelIdentifier")
                .or_else(|| gen.get("identifier"))
                .and_then(Value::as_str)
            {
                let model = model.to_string();
                if m.model.is_none() {
                    m.model = Some(model.clone());
                }
                *session_model = Some(model);
            }
        }
        if m.usage.is_none() {
            if let Some(u) = parse_usage(gen.get("stats")) {
                m.usage = Some(u);
            }
        }
    }

    // Per-step timestamp from the stepIdentifier prefix ("<ms-epoch>-<rand>").
    if m.timestamp.is_none() {
        if let Some(ts) = step
            .get("stepIdentifier")
            .and_then(Value::as_str)
            .and_then(step_id_ts)
        {
            m.timestamp = Some(ts);
        }
    }

    // A `style.type == "thinking"` block is chain-of-thought reasoning.
    let is_thinking = step
        .pointer("/style/type")
        .and_then(Value::as_str)
        == Some("thinking");
    push_content_parts(step.get("content"), m, is_thinking);
}

/// Append text/file/image parts from a `content` array (or bare string) onto `m`.
/// When `as_thinking`, text parts become [`Block::Thinking`] instead of [`Block::Text`].
fn push_content_parts(content: Option<&Value>, m: &mut Message, as_thinking: bool) {
    match content {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                push_text(m, s.clone(), as_thinking);
            }
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            if !t.is_empty() {
                                push_text(m, t.to_string(), as_thinking);
                            }
                        }
                    }
                    Some("file") => push_file(m, part),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn push_text(m: &mut Message, text: String, as_thinking: bool) {
    if as_thinking {
        // Explicit reasoning steps (`style.type=="thinking"`) stay a single Thinking block.
        m.content.push(Block::Thinking {
            text,
            signature: None,
            encrypted: None,
            redacted: false,
        });
    } else if crate::harmony::looks_like_harmony(&text) {
        // gpt-oss (common in LM Studio) embeds Harmony channel markers in plain assistant text:
        // analysis → Thinking, commentary tool calls → ToolUse, final → Text. Decode them into
        // first-class IR blocks instead of leaving the raw `<|channel|>…` markers in the text.
        m.content.extend(crate::harmony::decode_content(&text));
    } else {
        m.content.push(Block::Text { text });
    }
}

/// A `{type:"file",...}` part. `fileType:"image"` → [`Block::Image`]; otherwise [`Block::File`].
/// The reference is `identifier`/`fileIdentifier` (bytes live under `.lmstudio/.internal/files/`).
fn push_file(m: &mut Message, part: &Value) {
    let data_ref = part
        .get("identifier")
        .or_else(|| part.get("fileIdentifier"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let name = part.get("name").and_then(Value::as_str).map(str::to_string);
    let file_type = part.get("fileType").and_then(Value::as_str);

    if file_type == Some("image") {
        m.content.push(Block::Image {
            media_type: None,
            data_ref: data_ref.or(name),
        });
    } else {
        m.content.push(Block::File {
            mime: file_type.map(str::to_string),
            path: name,
            source: data_ref,
        });
    }
}

/// Concatenate `{type:"text"}` parts of a `content` value into a single string (for titles).
fn collect_text_parts(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Map LM Studio's `genInfo.stats` to IR [`Usage`].
fn parse_usage(stats: Option<&Value>) -> Option<Usage> {
    let stats = stats?;
    let input = stats.get("promptTokensCount").and_then(Value::as_u64);
    let output = stats.get("predictedTokensCount").and_then(Value::as_u64);
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: None,
        cache_creation_tokens: None,
    })
}

/// A `stepIdentifier` is `"<ms-epoch>-<rand>"`; mine its prefix as a timestamp.
fn step_id_ts(id: &str) -> Option<DateTime<Utc>> {
    let prefix = id.split('-').next()?;
    let ms: i64 = prefix.parse().ok()?;
    // Sanity bound: between ~2010 and ~2100 (ms epoch).
    if !(1_200_000_000_000..=4_000_000_000_000).contains(&ms) {
        return None;
    }
    DateTime::from_timestamp_millis(ms)
}

fn ms_to_dt(v: &Value) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_millis(v.as_i64()?)
}

fn file_mtime(path: &Path) -> Option<DateTime<Utc>> {
    let mtime = fs::metadata(path).ok()?.modified().ok()?;
    Some(DateTime::<Utc>::from(mtime))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/lmstudio")
            .join(name)
    }

    #[test]
    fn parses_user_assistant_thinking_and_usage() {
        let path = fixture("sample.conversation.json");
        if !path.exists() {
            return; // fixture optional in minimal checkouts
        }
        let r = scan(&path).unwrap().unwrap();
        assert_eq!(r.id, "sample");
        assert_eq!(r.title.as_deref(), Some("Hello there"));
        assert_eq!(r.message_count, 2);

        let s = LmStudio { root: None }.parse(&r).unwrap();
        assert!(s.cwd.is_none(), "lmstudio chats have no cwd");
        assert_eq!(s.model.as_deref(), Some("openai/gpt-oss-120b"));
        assert_eq!(s.messages.len(), 2);

        // user turn
        let u = &s.messages[0];
        assert_eq!(u.role, Role::User);
        assert_eq!(u.text().as_deref(), Some("Hello there"));

        // assistant turn: thinking block (from style.thinking) then text, plus model+usage.
        let a = &s.messages[1];
        assert_eq!(a.role, Role::Assistant);
        assert!(matches!(a.content[0], Block::Thinking { .. }));
        match &a.content[0] {
            Block::Thinking { text, .. } => assert_eq!(text, "the user said hi"),
            _ => unreachable!(),
        }
        assert_eq!(a.text().as_deref(), Some("Hi! How can I help?"));
        assert_eq!(a.model.as_deref(), Some("openai/gpt-oss-120b"));
        let usage = a.usage.as_ref().expect("usage present");
        assert_eq!(usage.input_tokens, Some(12));
        assert_eq!(usage.output_tokens, Some(34));
        assert!(a.timestamp.is_some(), "timestamp from stepIdentifier");
    }

    #[test]
    fn empty_messages_and_untitled_yield_no_title() {
        let v: Value = serde_json::from_str(
            r#"{"name":"Untitled","createdAt":1754424586190,"messages":[]}"#,
        )
        .unwrap();
        assert!(chat_title(&v).is_none());
    }

    #[test]
    fn per_chat_system_prompt_overrides_top_level() {
        let v: Value = serde_json::from_str(
            r#"{"systemPrompt":"global","perChatPredictionConfig":{"fields":[{"key":"llm.prediction.systemPrompt","value":"tsundere"}]}}"#,
        )
        .unwrap();
        assert_eq!(system_prompt(&v).as_deref(), Some("tsundere"));
    }

    #[test]
    fn user_file_image_part_becomes_image_block() {
        let msg: Value = serde_json::from_str(
            r#"{"currentlySelected":0,"versions":[{"type":"singleStep","role":"user","content":[{"type":"text","text":"look"},{"type":"file","name":"image.png","identifier":"123 - 4.png","fileType":"image","sizeBytes":100}]}]}"#,
        )
        .unwrap();
        let mut model = None;
        let m = parse_message(&msg, &mut model).unwrap();
        assert_eq!(m.role, Role::User);
        assert!(matches!(m.content[1], Block::Image { .. }));
        match &m.content[1] {
            Block::Image { data_ref, .. } => assert_eq!(data_ref.as_deref(), Some("123 - 4.png")),
            _ => unreachable!(),
        }
    }

    #[test]
    fn selects_currently_selected_version() {
        let msg: Value = serde_json::from_str(
            r#"{"currentlySelected":1,"versions":[
                {"type":"singleStep","role":"user","content":[{"type":"text","text":"v0"}]},
                {"type":"singleStep","role":"user","content":[{"type":"text","text":"v1"}]}
            ]}"#,
        )
        .unwrap();
        let mut model = None;
        let m = parse_message(&msg, &mut model).unwrap();
        assert_eq!(m.text().as_deref(), Some("v1"));
    }

    #[test]
    fn missing_dir_discovers_empty() {
        let a = LmStudio { root: None };
        assert!(a.discover().unwrap().is_empty());
    }

    #[test]
    fn step_id_timestamp_parses_prefix() {
        let ts = step_id_ts("1754425094719-0.022738884687463323").unwrap();
        assert_eq!(ts.timestamp_millis(), 1754425094719);
        assert!(step_id_ts("garbage").is_none());
    }

    #[test]
    fn gpt_oss_harmony_text_decodes_into_blocks() {
        // gpt-oss output stored by LM Studio as raw Harmony-framed assistant text should be decoded
        // into Thinking + ToolUse + Text instead of a single text block full of `<|channel|>` markers.
        let path = fixture("harmony.conversation.json");
        if !path.exists() {
            return; // fixture optional in minimal checkouts
        }
        let r = scan(&path).unwrap().unwrap();
        let s = LmStudio { root: None }.parse(&r).unwrap();
        assert_eq!(s.messages.len(), 2);

        let a = &s.messages[1];
        assert_eq!(a.role, Role::Assistant);
        // No raw Harmony markers should survive in the decoded blocks.
        assert_eq!(a.content.len(), 3, "thinking + tooluse + text");

        match &a.content[0] {
            Block::Thinking { text, .. } => {
                assert!(text.contains("get_weather"));
                assert!(!text.contains("<|channel|>"));
            }
            other => panic!("expected Thinking, got {other:?}"),
        }
        match &a.content[1] {
            Block::ToolUse { name, input, .. } => {
                assert_eq!(name, "get_weather");
                assert_eq!(input["location"], "San Francisco, CA");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        match &a.content[2] {
            Block::Text { text } => {
                assert!(text.contains("sunny"));
                assert!(!text.contains("<|"));
            }
            other => panic!("expected Text, got {other:?}"),
        }
        // Model + usage still parsed from genInfo.
        assert_eq!(a.model.as_deref(), Some("openai/gpt-oss-120b"));
        assert_eq!(a.usage.as_ref().unwrap().output_tokens, Some(68));
    }
}
