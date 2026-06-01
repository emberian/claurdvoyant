//! Training-dataset export: render a [`Session`] into one JSONL record in a format that
//! fine-tuning toolchains ingest directly —
//!
//! - `chatml`   → `{"messages":[{"role","content"}]}` (OpenAI-style; the most portable)
//! - `sharegpt` → `{"conversations":[{"from","value"}]}`
//!
//! Unsloth Studio, TRL, and HuggingFace `datasets` load these with no bespoke adapter (Studio's
//! importer auto-detects both). One session → one line; the caller streams the corpus (so memory
//! stays O(one session)) and optionally applies [`crate::redact`] first.
//!
//! Tool calls/results are folded into the message text as fenced `tool_call` / `tool_result`
//! blocks (v1: the model learns the tool-use *pattern* in-band, and it imports as plain chatml).
//! Reasoning is kept in a `<thinking>` wrapper — it's high-signal for distilling into smaller
//! models, and a downstream filter can strip it if a given run wants answer-only SFT.

use crate::ir::{Block, Role, Session};
use serde_json::{json, Value};

/// Serialize a session as a ChatML record. Returns `None` if it has no non-empty turns.
pub fn to_chatml(session: &Session) -> Option<Value> {
    let resolver = session.resolver();
    let messages: Vec<Value> = session
        .messages
        .iter()
        .filter_map(|m| {
            let content = render_blocks(&m.content, &resolver);
            if content.trim().is_empty() {
                return None;
            }
            let role = match m.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            Some(json!({ "role": role, "content": content }))
        })
        .collect();
    if messages.is_empty() {
        return None;
    }
    Some(json!({ "messages": messages }))
}

/// Serialize a session as a ShareGPT record. Returns `None` if it has no non-empty turns.
pub fn to_sharegpt(session: &Session) -> Option<Value> {
    let resolver = session.resolver();
    let conversations: Vec<Value> = session
        .messages
        .iter()
        .filter_map(|m| {
            let value = render_blocks(&m.content, &resolver);
            if value.trim().is_empty() {
                return None;
            }
            let from = match m.role {
                Role::System => "system",
                Role::User => "human",
                Role::Assistant => "gpt",
                Role::Tool => "tool",
            };
            Some(json!({ "from": from, "value": value }))
        })
        .collect();
    if conversations.is_empty() {
        return None;
    }
    Some(json!({ "conversations": conversations }))
}

/// Flatten one message's blocks into a single training-text string, resolving lazy content spans
/// against `resolver` (so a giant field is materialized transiently, not held).
fn render_blocks(blocks: &[Block], resolver: &crate::lazy::Resolver) -> String {
    let mut parts: Vec<String> = Vec::new();
    for b in blocks {
        match b {
            Block::Text { text } => {
                let text = text.resolve(resolver);
                if !text.trim().is_empty() {
                    parts.push(text.into_owned());
                }
            }
            Block::Thinking { text, redacted, .. } => {
                let text = text.resolve(resolver);
                if !*redacted && !text.trim().is_empty() {
                    parts.push(format!("<thinking>\n{text}\n</thinking>"));
                }
            }
            Block::ToolUse { name, input, .. } => {
                let args = serde_json::to_string(input).unwrap_or_default();
                parts.push(format!("```tool_call\n{name} {args}\n```"));
            }
            Block::ToolResult { content, is_error, .. } => {
                let tag = if *is_error { "tool_result error" } else { "tool_result" };
                let content = content.resolve(resolver);
                parts.push(format!("```{tag}\n{content}\n```"));
            }
            Block::Image { media_type, .. } => {
                let m = media_type.as_deref().map(|m| format!(": {m}")).unwrap_or_default();
                parts.push(format!("[image{m}]"));
            }
            Block::File { path, mime, .. } => {
                let label = path.as_deref().or(mime.as_deref()).unwrap_or("attachment");
                parts.push(format!("[file: {label}]"));
            }
        }
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Harness, Message};

    fn msg(role: Role, blocks: Vec<Block>) -> Message {
        Message {
            id: None,
            parent_id: None,
            role,
            timestamp: None,
            model: None,
            content: blocks,
            usage: None,
            extra: serde_json::Map::new(),
        }
    }

    fn session(messages: Vec<Message>) -> Session {
        Session {
            id: "t".into(),
            harness: Harness::Claude,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            model: None,
            git: None,
            messages,
            source_path: None,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn chatml_maps_roles_and_folds_tools() {
        let s = session(vec![
            msg(Role::User, vec![Block::Text { text: "fix the bug".into() }]),
            msg(
                Role::Assistant,
                vec![
                    Block::Thinking { text: "check the log".into(), signature: None, encrypted: None, redacted: false },
                    Block::ToolUse { id: "1".into(), name: "Bash".into(), input: json!({"cmd": "grep x"}) },
                ],
            ),
            msg(Role::Tool, vec![Block::ToolResult { tool_use_id: "1".into(), content: "found it".into(), is_error: false, tool_name: None, status: None, details: None }]),
        ]);
        let v = to_chatml(&s).expect("non-empty");
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
        assert!(msgs[1]["content"].as_str().unwrap().contains("<thinking>"));
        assert!(msgs[1]["content"].as_str().unwrap().contains("```tool_call"));
        assert_eq!(msgs[2]["role"], "tool");
        assert!(msgs[2]["content"].as_str().unwrap().contains("found it"));
    }

    #[test]
    fn empty_session_is_none() {
        assert!(to_chatml(&session(vec![])).is_none());
        assert!(to_chatml(&session(vec![msg(Role::User, vec![Block::Text { text: "  ".into() }])])).is_none());
    }

    #[test]
    fn sharegpt_uses_from_value() {
        let s = session(vec![msg(Role::User, vec![Block::Text { text: "hi".into() }])]);
        let v = to_sharegpt(&s).unwrap();
        assert_eq!(v["conversations"][0]["from"], "human");
        assert_eq!(v["conversations"][0]["value"], "hi");
    }
}
