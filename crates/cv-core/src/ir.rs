//! The unified intermediate representation (IR).
//!
//! Every harness parses *into* these types, and cross-harness conversion emits *out of* them.
//! The IR is deliberately a superset: fields that a given harness doesn't have are `None`/empty,
//! and harness-specific extras ride along in [`Message::extra`] so conversions can be as lossless
//! as the target allows.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Which agent harness a session came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    Claude,
    Codex,
    Grok,
    OpenCode,
    Gemini,
    Hermes,
    OpenClaw,
    /// Cursor IDE (chat/composer history in `state.vscdb`).
    Cursor,
    /// The Claude desktop app (macOS/Windows).
    ClaudeApp,
    /// The ChatGPT desktop app (macOS/Windows).
    ChatGptApp,
    /// Kimi CLI (MoonshotAI), `~/.kimi`.
    Kimi,
    /// Qwen Code CLI (a gemini-cli fork), `~/.qwen`.
    Qwen,
    /// LM Studio desktop app, `~/.lmstudio`.
    LmStudio,
    /// Cline (VS Code extension), per-task Anthropic-format JSON.
    Cline,
    /// Roo Code (a Cline fork).
    Roo,
    /// Continue (VS Code/JetBrains), `~/.continue/sessions`.
    Continue,
    /// Goose (Block), `~/.local/share/goose/sessions`.
    Goose,
}

impl Harness {
    pub const ALL: [Harness; 17] = [
        Harness::Claude,
        Harness::Codex,
        Harness::Grok,
        Harness::OpenCode,
        Harness::Gemini,
        Harness::Hermes,
        Harness::OpenClaw,
        Harness::Cursor,
        Harness::ClaudeApp,
        Harness::ChatGptApp,
        Harness::Kimi,
        Harness::Qwen,
        Harness::LmStudio,
        Harness::Cline,
        Harness::Roo,
        Harness::Continue,
        Harness::Goose,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Grok => "grok",
            Harness::OpenCode => "opencode",
            Harness::Gemini => "gemini",
            Harness::Hermes => "hermes",
            Harness::OpenClaw => "openclaw",
            Harness::Cursor => "cursor",
            Harness::ClaudeApp => "claude-app",
            Harness::ChatGptApp => "chatgpt-app",
            Harness::Kimi => "kimi",
            Harness::Qwen => "qwen",
            Harness::LmStudio => "lmstudio",
            Harness::Cline => "cline",
            Harness::Roo => "roo",
            Harness::Continue => "continue",
            Harness::Goose => "goose",
        }
    }

    pub fn parse(s: &str) -> Option<Harness> {
        Some(match s.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "cc" => Harness::Claude,
            "codex" => Harness::Codex,
            "grok" => Harness::Grok,
            "opencode" | "oc" => Harness::OpenCode,
            "gemini" | "antigravity" => Harness::Gemini,
            "hermes" | "hermes-agent" => Harness::Hermes,
            "openclaw" | "claw" => Harness::OpenClaw,
            "cursor" => Harness::Cursor,
            "claude-app" | "claude-desktop" | "claudeapp" => Harness::ClaudeApp,
            "chatgpt-app" | "chatgpt" | "chatgpt-desktop" | "openai-app" => Harness::ChatGptApp,
            "kimi" | "kimi-cli" => Harness::Kimi,
            "qwen" | "qwen-code" => Harness::Qwen,
            "lmstudio" | "lm-studio" => Harness::LmStudio,
            "cline" => Harness::Cline,
            "roo" | "roo-code" | "roocode" => Harness::Roo,
            "continue" | "continuedev" => Harness::Continue,
            "goose" => Harness::Goose,
            _ => return None,
        })
    }
}

impl std::fmt::Display for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A fully-parsed session in the unified representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub harness: Harness,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<GitInfo>,
    pub messages: Vec<Message>,
    /// Where this session was read from on disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<PathBuf>,
    /// Session-level metadata that doesn't have a first-class home yet.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Session {
    /// First non-empty user text, used as a fallback title / preview.
    pub fn first_user_text(&self) -> Option<String> {
        self.messages
            .iter()
            .find(|m| m.role == Role::User)
            .and_then(|m| m.text().filter(|t| !t.trim().is_empty()))
    }

    /// A short human label for listings.
    pub fn label(&self) -> String {
        label_from(self.title.as_deref(), self.first_user_text().as_deref())
    }

    /// All textual content concatenated — used to build a search index.
    ///
    /// The per-message projection lives in [`Message::append_searchable`]; this just prepends the
    /// title and folds every message into one `String`.
    pub fn searchable_text(&self) -> String {
        let mut out = String::new();
        if let Some(t) = &self.title {
            out.push_str(t);
            out.push('\n');
        }
        for m in &self.messages {
            m.append_searchable(&mut out);
        }
        out
    }

    /// A [`Resolver`](crate::lazy::Resolver) for this session's lazy content spans, rooted at its
    /// `source_path`. Streaming consumers create one and resolve each block as it passes.
    pub fn resolver(&self) -> crate::lazy::Resolver {
        crate::lazy::Resolver::new(self.source_path.clone())
    }

    /// Resolve every lazy content [`Span`](crate::lazy::Span) in place, so the session owns all its
    /// content inline. Whole-session consumers (cross-harness emit building output in memory, JSON
    /// serialization) call this; streaming consumers resolve per-block instead. After it, every
    /// content `Text` is `Inline`, so `Deref`/`Display`/`searchable_text` are safe.
    pub fn materialize(&mut self) {
        let resolver = self.resolver();
        for m in &mut self.messages {
            m.materialize(&resolver);
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    /// A tool/function result fed back to the model. (Some harnesses model this as a `user` turn;
    /// we keep it distinct so conversions can re-encode it correctly.)
    Tool,
}

/// One message/turn in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub content: Vec<Block>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Harness-specific fields preserved verbatim for lossless-ish round-tripping.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Message {
    pub fn new(role: Role) -> Self {
        Message {
            id: None,
            parent_id: None,
            role,
            timestamp: None,
            model: None,
            content: Vec::new(),
            usage: None,
            extra: serde_json::Map::new(),
        }
    }

    /// Resolve this message's lazy content spans in place against `resolver`, so its content is owned
    /// inline. Streaming consumers call this per message (peak = one message) instead of holding a
    /// whole materialized session.
    pub fn materialize(&mut self, resolver: &crate::lazy::Resolver) {
        for b in &mut self.content {
            let slot = match b {
                Block::Text { text } | Block::Thinking { text, .. } => text,
                Block::ToolResult { content, .. } => content,
                _ => continue,
            };
            if slot.is_span() {
                let owned = slot.resolve(resolver).into_owned();
                *slot = crate::lazy::Text::Inline(owned);
            }
        }
    }

    /// Append this message's searchable text to `out` — the single canonical projection behind
    /// both [`Session::searchable_text`] (whole session) and
    /// [`append_searchable`](crate::stream::append_searchable) (streaming, per message).
    /// Writes into the caller's buffer so neither path allocates intermediates.
    pub fn append_searchable(&self, out: &mut String) {
        use std::fmt::Write as _;
        for b in &self.content {
            match b {
                Block::Text { text } | Block::Thinking { text, .. } => {
                    out.push_str(text);
                    out.push('\n');
                }
                Block::ToolUse { name, input, .. } => {
                    out.push_str(name);
                    out.push(' ');
                    // `Display` for `serde_json::Value` produces exactly `to_string()`, without
                    // the intermediate allocation.
                    let _ = write!(out, "{input}");
                    out.push('\n');
                }
                Block::ToolResult { content, .. } => {
                    out.push_str(content);
                    out.push('\n');
                }
                Block::File { path, source, .. } => {
                    if let Some(p) = path.as_deref().or(source.as_deref()) {
                        out.push_str(p);
                        out.push('\n');
                    }
                }
                Block::Image { .. } => {}
            }
        }
    }

    /// Concatenated plain text of this message's text blocks (ignores thinking/tools).
    pub fn text(&self) -> Option<String> {
        let mut s = String::new();
        for b in &self.content {
            if let Block::Text { text } = b {
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(text);
            }
        }
        (!s.is_empty()).then_some(s)
    }
}

/// A unit of message content. Tagged so it (de)serializes to clean JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: crate::lazy::Text,
    },
    /// Extended reasoning / chain-of-thought. `encrypted` holds an opaque blob some providers emit.
    Thinking {
        text: crate::lazy::Text,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        encrypted: Option<String>,
        /// Whether the provider redacted this reasoning content.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        redacted: bool,
    },
    /// A tool/function invocation by the assistant.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// The result of a tool/function call.
    ToolResult {
        tool_use_id: String,
        content: crate::lazy::Text,
        #[serde(default)]
        is_error: bool,
        /// Name of the tool this result is for, when the adapter knows it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_name: Option<String>,
        /// Adapter-computed status string (e.g. "completed", "error", "running").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        /// Structured extra details about the result.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
    },
    Image {
        #[serde(skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
        /// Path or opaque reference; we don't inline image bytes into the IR.
        #[serde(skip_serializing_if = "Option::is_none")]
        data_ref: Option<String>,
    },
    /// A first-class file/dir/resource attachment.
    File {
        #[serde(skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
}

/// A lightweight handle to a session discovered on disk, cheap to produce for listings/search
/// without parsing the whole transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRef {
    pub id: String,
    pub harness: Harness,
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    /// Number of conversational messages: **user + assistant turns only**.
    ///
    /// This is the contract every adapter must meet — do not count system/meta records, tool
    /// results, sidecar files, or directory entries. The number a person would call "how long is
    /// this conversation", comparable across harnesses. Adapters that can't know it cheaply
    /// should compute it from the same records they'd parse into [`Role::User`]/[`Role::Assistant`]
    /// messages, not from a proxy like file count.
    pub message_count: usize,
}

/// A short human label from a title and/or first-user-text fallback — the projection
/// [`Session::label`] makes, exposed so streaming consumers can build it without a whole `Session`.
pub fn label_from(title: Option<&str>, first_user_text: Option<&str>) -> String {
    title
        .or(first_user_text)
        .map(|s| truncate(s, 72))
        .unwrap_or_else(|| "(untitled)".into())
}

/// Flatten newlines to spaces and truncate to at most `max` chars, ending with `…` when cut.
/// The one canonical truncation used by labels, renderers, and listings.
pub fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max {
        s
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
