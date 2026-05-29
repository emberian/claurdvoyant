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
}

impl Harness {
    pub const ALL: [Harness; 10] = [
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
        self.title
            .clone()
            .or_else(|| self.first_user_text())
            .map(|s| truncate(&s, 72))
            .unwrap_or_else(|| "(untitled)".into())
    }

    /// All textual content concatenated — used to build a search index.
    pub fn searchable_text(&self) -> String {
        let mut out = String::new();
        if let Some(t) = &self.title {
            out.push_str(t);
            out.push('\n');
        }
        for m in &self.messages {
            for b in &m.content {
                match b {
                    Block::Text { text } | Block::Thinking { text, .. } => {
                        out.push_str(text);
                        out.push('\n');
                    }
                    Block::ToolUse { name, input, .. } => {
                        out.push_str(name);
                        out.push(' ');
                        out.push_str(&input.to_string());
                        out.push('\n');
                    }
                    Block::ToolResult { content, .. } => {
                        out.push_str(content);
                        out.push('\n');
                    }
                    Block::Image { .. } => {}
                }
            }
        }
        out
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
        text: String,
    },
    /// Extended reasoning / chain-of-thought. `encrypted` holds an opaque blob some providers emit.
    Thinking {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        encrypted: Option<String>,
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
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    Image {
        #[serde(skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
        /// Path or opaque reference; we don't inline image bytes into the IR.
        #[serde(skip_serializing_if = "Option::is_none")]
        data_ref: Option<String>,
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
    pub message_count: usize,
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max {
        s
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
