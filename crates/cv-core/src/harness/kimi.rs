//! Kimi CLI (MoonshotAI) adapter — `~/.kimi/sessions/<md5(cwd)>/<session-uuid>/context.jsonl`.
//!
//! ## Storage layout
//! Root is `$KIMI_SHARE_DIR` or `~/.kimi`. Each work directory maps to a project dir whose name is
//! **`md5(cwd_utf8).hexdigest()`** (lowercase hex), or `"<kaos>_<md5>"` when the KAOS isn't `local`.
//! The cwd is NOT stored in the transcript; we recover it from `~/.kimi/kimi.json`'s `work_dirs[]`
//! (`{path, kaos, last_session_id}`) by matching `md5(path) == <hash>`.
//!
//! Two on-disk shapes (sniffed, never via a version field):
//! - **Modern (dir form):** `sessions/<hash>/<uuid>/context.jsonl` plus a `wire.jsonl` sidecar
//!   (timestamps + token usage), optional `state.json` (`custom_title`), and archived pre-compaction
//!   segments `context_1.jsonl…context_N.jsonl` (we read the live `context.jsonl`, recording the
//!   count of segments in `session.extra`).
//! - **Legacy (flat form):** `sessions/<hash>/<uuid>.jsonl` — the transcript only, no sidecar/dir.
//!   (kimi-cli migrates these to the dir form on next open; we still read them in place.)
//!
//! ## `context.jsonl` records (one JSON object per line)
//! - `{"role":"_system_prompt","content":str}` → System message.
//! - `{"role":"_checkpoint","id":N}` → skipped (UI bookmark).
//! - `{"role":"_usage","token_count":N}` → skipped (running total; real usage comes from wire).
//! - `{"role":"user","content": str | [Part]}` → User. `content` is a BARE STRING for a single text
//!   part, else a list of Parts.
//! - `{"role":"assistant","content":[Part],"tool_calls":[…]}` → Assistant.
//! - `{"role":"tool","content": str | [Part],"tool_call_id":id}` → Tool (one ToolResult). The first
//!   text part is often a `<system>summary</system>` line.
//!
//! ### Part (`type`-tagged)
//! - `text{text}` → Text.
//! - `think{think,encrypted}` → Thinking{ text: think, encrypted }.
//! - `image_url{image_url:{url,id}}` → Image (data_ref = url).
//! - `audio_url{…}` / `video_url{…}` → File (source = url). (Media URLs nest under a key named after
//!   the type.)
//!
//! ### tool_calls[]
//! `{type:"function", id, function:{name, arguments}}` → ToolUse{id,name, input}. `arguments` is a
//! JSON-encoded *string*; we parse it (null/empty ⇒ `{}`).
//!
//! ## wire.jsonl sidecar (optional enrichment, like grok's `updates.jsonl`)
//! `{"type":"metadata","protocol_version":"1.x"}` header, then per-event lines
//! `{"timestamp": epoch_float, "message": {"type": …, "payload": …}}`. We mine:
//! - `ToolResult.payload.return_value{is_error,output,message,display,extras}` → richer ToolResult.
//! - `StatusUpdate.payload.token_usage{input_other,output,input_cache_read,input_cache_creation}`
//!   and `message_id` (chatcmpl-…) → attached to the most recent assistant message.
//! - `TurnBegin`/event timestamps → first/last session timestamps when no file mtime is better.
//! We tolerate protocol_version drift (1.1–1.9) and unknown message types: anything we don't
//! recognize is ignored.

use super::Adapter;
use crate::ir::*;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub struct Kimi {
    /// `<root>/sessions`, if it exists.
    sessions: Option<PathBuf>,
    /// The share root itself (`~/.kimi`), for locating `kimi.json` / `config.toml`.
    root: Option<PathBuf>,
}

impl Kimi {
    pub fn new() -> Self {
        let root = std::env::var_os("KIMI_SHARE_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".kimi")))
            .filter(|p| p.exists());
        let sessions = root.as_ref().map(|r| r.join("sessions")).filter(|p| p.exists());
        Kimi { sessions, root }
    }
}

impl Default for Kimi {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for Kimi {
    fn harness(&self) -> Harness {
        Harness::Kimi
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.sessions.clone()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(sessions) = &self.sessions else {
            return Ok(vec![]);
        };
        // hash (dir basename, possibly "<kaos>_<md5>") -> cwd, from kimi.json.
        let hash_to_cwd = self
            .root
            .as_ref()
            .map(|r| load_work_dirs(&r.join("kimi.json")))
            .unwrap_or_default();
        let default_model = self
            .root
            .as_ref()
            .and_then(|r| default_model_from_config(&r.join("config.toml")));

        let mut out = Vec::new();
        let Ok(hash_dirs) = fs::read_dir(sessions) else {
            return Ok(out);
        };
        for hd in hash_dirs.filter_map(|e| e.ok()) {
            let hash_path = hd.path();
            let Some(basename) = hash_path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !hash_path.is_dir() {
                continue;
            }
            let cwd = hash_to_cwd.get(basename).cloned();
            let Ok(entries) = fs::read_dir(&hash_path) else {
                continue;
            };
            for e in entries.filter_map(|e| e.ok()) {
                let p = e.path();
                let name = e.file_name();
                let name = name.to_string_lossy();
                let (id, transcript, dir): (String, PathBuf, PathBuf) = if p.is_dir() {
                    // Modern: <uuid>/context.jsonl
                    let ctx = p.join("context.jsonl");
                    if !ctx.exists() {
                        continue;
                    }
                    (name.to_string(), ctx, p.clone())
                } else if name.ends_with(".jsonl") {
                    // Legacy: <uuid>.jsonl (flat). source dir is the hash dir.
                    let id = name.trim_end_matches(".jsonl").to_string();
                    (id, p.clone(), hash_path.clone())
                } else {
                    continue;
                };
                match scan(&id, &transcript, &dir, cwd.clone(), default_model.clone()) {
                    Some(r) => out.push(r),
                    None => {}
                }
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        // r.path is the session dir (modern) or the hash dir (legacy). Locate the transcript.
        let (transcript, wire) = locate_transcript(&r.path, &r.id);

        let mut s = Session {
            id: r.id.clone(),
            harness: Harness::Kimi,
            cwd: r.cwd.clone(),
            title: r.title.clone(),
            created_at: r.created_at,
            updated_at: r.updated_at,
            model: self
                .root
                .as_ref()
                .and_then(|root| default_model_from_config(&root.join("config.toml"))),
            git: None,
            messages: Vec::new(),
            source_path: Some(r.path.clone()),
            extra: serde_json::Map::new(),
        };

        // Sidecar enrichment from wire.jsonl (tolerant: absent/empty/malformed ⇒ empty).
        let enrich = wire.as_ref().map(read_wire_enrichment).unwrap_or_default();

        // Note archived compaction segments (context_1.jsonl…), if any, for fidelity bookkeeping.
        let seg_count = count_segments(&r.path);
        if seg_count > 0 {
            s.extra.insert(
                "compaction_segments".into(),
                Value::Number(seg_count.into()),
            );
        }

        let Ok(text) = fs::read_to_string(&transcript) else {
            return Ok(s);
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if let Some(m) = context_message(&v, &enrich) {
                s.messages.push(m);
            }
        }

        // Attach StatusUpdate usage / message_id to assistant turns in order. wire emits one
        // StatusUpdate per assistant step; we zip them onto assistant messages positionally.
        attach_status_updates(&mut s.messages, &enrich);

        // Per-message timestamps from wire (best-effort, positional over recognized turns).
        attach_timestamps(&mut s.messages, &enrich);

        Ok(s)
    }

    fn can_emit(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Discovery helpers
// ---------------------------------------------------------------------------

/// Parse `kimi.json` into a map of project-dir basename → cwd. The basename is `md5(path)` for the
/// `local` KAOS, else `"<kaos>_<md5>"`, matching kimi-cli's `WorkDirMeta.sessions_dir`.
fn load_work_dirs(kimi_json: &Path) -> HashMap<String, PathBuf> {
    let mut map = HashMap::new();
    let Ok(text) = fs::read_to_string(kimi_json) else {
        return map;
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return map;
    };
    let Some(dirs) = v.get("work_dirs").and_then(Value::as_array) else {
        return map;
    };
    for wd in dirs {
        let Some(path) = wd.get("path").and_then(Value::as_str) else {
            continue;
        };
        let kaos = wd.get("kaos").and_then(Value::as_str).unwrap_or("local");
        let hash = md5_hex(path.as_bytes());
        let basename = if kaos == "local" {
            hash
        } else {
            format!("{kaos}_{hash}")
        };
        map.insert(basename, PathBuf::from(path));
    }
    map
}

/// `default_model` from `config.toml`. We don't pull in a TOML parser for a single scalar: scan for
/// the top-level `default_model = "…"` line.
fn default_model_from_config(config_toml: &Path) -> Option<String> {
    let text = fs::read_to_string(config_toml).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("default_model") {
            let rest = rest.trim_start().strip_prefix('=')?.trim();
            let val = rest.trim_matches('"').trim_matches('\'');
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

/// Cheap scan of one transcript into a `SessionRef`. `dir` is the session's source dir (the
/// `<uuid>/` dir for modern, the hash dir for legacy). Returns `None` only on a fully unreadable
/// transcript path that doesn't exist.
fn scan(
    id: &str,
    transcript: &Path,
    dir: &Path,
    cwd: Option<PathBuf>,
    _default_model: Option<String>,
) -> Option<SessionRef> {
    let meta = fs::metadata(transcript).ok()?;
    let mtime: Option<DateTime<Utc>> = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| DateTime::<Utc>::from_timestamp_nanos(d.as_nanos() as i64));

    // title: state.json.custom_title (modern only) else first user text from the transcript.
    let mut title = read_custom_title(dir);
    let mut message_count = 0usize;
    let text = fs::read_to_string(transcript).unwrap_or_default();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let role = v.get("role").and_then(Value::as_str).unwrap_or("");
        match role {
            "user" | "assistant" | "tool" | "_system_prompt" => message_count += 1,
            _ => {}
        }
        if title.is_none() && role == "user" {
            let t = coerce_content_text(v.get("content"));
            if !t.trim().is_empty() {
                title = Some(crate::ir::truncate(&t, 80));
            }
        }
    }

    // wire gives nicer first/last timestamps when present.
    let wire = wire_path_for(dir, transcript);
    let (created, updated) = wire
        .as_ref()
        .and_then(|w| wire_time_bounds(w))
        .map(|(a, b)| (Some(a), Some(b)))
        .unwrap_or((mtime, mtime));

    Some(SessionRef {
        id: id.to_string(),
        harness: Harness::Kimi,
        path: dir.to_path_buf(),
        cwd,
        title,
        created_at: created,
        updated_at: updated,
        message_count,
    })
}

/// `state.json.custom_title`, when present and non-empty.
fn read_custom_title(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join("state.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get("custom_title")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| crate::ir::truncate(s, 80))
}

/// Given a `SessionRef.path` and id, find the transcript + optional wire sidecar.
fn locate_transcript(dir: &Path, id: &str) -> (PathBuf, Option<PathBuf>) {
    let modern = dir.join("context.jsonl");
    if modern.exists() {
        let wire = dir.join("wire.jsonl");
        return (modern, wire.exists().then_some(wire));
    }
    // Legacy flat: dir is the hash dir, transcript is <id>.jsonl, no wire.
    let flat = dir.join(format!("{id}.jsonl"));
    (flat, None)
}

/// Locate the wire sidecar for a transcript, if it lives next to it (modern only).
fn wire_path_for(dir: &Path, transcript: &Path) -> Option<PathBuf> {
    if transcript.file_name().and_then(|n| n.to_str()) == Some("context.jsonl") {
        let w = dir.join("wire.jsonl");
        return w.exists().then_some(w);
    }
    None
}

/// Count archived pre-compaction segments `context_1.jsonl…` next to a modern transcript.
fn count_segments(dir: &Path) -> u64 {
    let Ok(rd) = fs::read_dir(dir) else {
        return 0;
    };
    rd.filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| n.starts_with("context_") && n.ends_with(".jsonl"))
                .unwrap_or(false)
        })
        .count() as u64
}

// ---------------------------------------------------------------------------
// context.jsonl → IR
// ---------------------------------------------------------------------------

/// Convert one `context.jsonl` record into a `Message`, or `None` to skip it.
fn context_message(v: &Value, enrich: &WireEnrich) -> Option<Message> {
    let role = v.get("role").and_then(Value::as_str)?;
    match role {
        "_checkpoint" | "_usage" => None,
        "_system_prompt" => {
            let text = coerce_content_text(v.get("content"));
            let mut m = Message::new(Role::System);
            m.content.push(Block::Text { text });
            Some(m)
        }
        "user" => {
            let mut m = Message::new(Role::User);
            push_parts(&mut m, v.get("content"));
            (!m.content.is_empty()).then_some(m)
        }
        "assistant" => {
            let mut m = Message::new(Role::Assistant);
            push_parts(&mut m, v.get("content"));
            if let Some(calls) = v.get("tool_calls").and_then(Value::as_array) {
                for c in calls {
                    let id = c.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                    let func = c.get("function");
                    let name = func
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let input = parse_arguments(func.and_then(|f| f.get("arguments")));
                    m.content.push(Block::ToolUse { id, name, input });
                }
            }
            (!m.content.is_empty()).then_some(m)
        }
        "tool" => {
            let tool_use_id = v
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // Prefer the richer wire return_value if we have it; else the context text parts.
            let we = enrich.tool_results.get(&tool_use_id);
            let (content, is_error, details) = match we {
                Some(tr) => (
                    tr.output.clone(),
                    tr.is_error,
                    tr.extras.clone(),
                ),
                None => (coerce_content_text(v.get("content")), false, None),
            };
            let status = Some(if is_error { "error" } else { "completed" }.to_string());
            let mut m = Message::new(Role::Tool);
            m.content.push(Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                tool_name: None,
                status,
                details,
            });
            Some(m)
        }
        _ => None,
    }
}

/// Push a record's `content` (bare string or `[Part]`) onto a message as Blocks.
fn push_parts(m: &mut Message, content: Option<&Value>) {
    match content {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                m.content.push(Block::Text { text: s.clone() });
            }
        }
        Some(Value::Array(parts)) => {
            for p in parts {
                if let Some(b) = part_to_block(p) {
                    m.content.push(b);
                }
            }
        }
        _ => {}
    }
}

/// One `type`-tagged Part → a `Block`.
fn part_to_block(p: &Value) -> Option<Block> {
    let ty = p.get("type").and_then(Value::as_str)?;
    match ty {
        "text" => {
            let text = p.get("text").and_then(Value::as_str).unwrap_or("").to_string();
            Some(Block::Text { text })
        }
        "think" => {
            let text = p.get("think").and_then(Value::as_str).unwrap_or("").to_string();
            let encrypted = p.get("encrypted").and_then(Value::as_str).map(str::to_string);
            Some(Block::Thinking {
                text,
                signature: encrypted.clone(),
                encrypted,
                redacted: false,
            })
        }
        "image_url" => {
            let url = media_url(p, "image_url");
            Some(Block::Image {
                media_type: None,
                data_ref: url,
            })
        }
        "audio_url" | "video_url" => {
            let url = media_url(p, ty);
            Some(Block::File {
                mime: None,
                path: None,
                source: url,
            })
        }
        _ => None,
    }
}

/// Media parts nest `{url, id}` under a key named after the type, e.g. `image_url: {url, id}`.
fn media_url(p: &Value, key: &str) -> Option<String> {
    p.get(key)
        .and_then(|inner| inner.get("url"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// `arguments` is a JSON-encoded *string*. Parse it; `null`/empty/unparseable ⇒ `{}`.
fn parse_arguments(v: Option<&Value>) -> Value {
    match v {
        Some(Value::String(s)) if !s.is_empty() => {
            serde_json::from_str(s).unwrap_or_else(|_| Value::Object(Default::default()))
        }
        Some(Value::Object(_)) => v.cloned().unwrap(),
        _ => Value::Object(Default::default()),
    }
}

/// Flatten a record `content` (string or `[Part]`) to plain text (text parts joined by newline).
fn coerce_content_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") => p.get("text").and_then(Value::as_str).map(str::to_string),
                Some("think") => p.get("think").and_then(Value::as_str).map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// wire.jsonl sidecar enrichment
// ---------------------------------------------------------------------------

#[derive(Default)]
struct WireEnrich {
    /// tool_call_id → richer result from `ToolResult.return_value`.
    tool_results: HashMap<String, WireToolResult>,
    /// Per assistant step, in wire order.
    statuses: Vec<WireStatus>,
    /// Timestamps of `TurnBegin` and `ContentPart`/`ToolCall` events, in order (best-effort).
    msg_timestamps: Vec<DateTime<Utc>>,
}

struct WireToolResult {
    is_error: bool,
    output: String,
    extras: Option<Value>,
}

#[derive(Default)]
struct WireStatus {
    usage: Option<Usage>,
    message_id: Option<String>,
}

/// Parse `wire.jsonl`. Tolerant of `protocol_version` drift and unknown message types.
fn read_wire_enrichment(wire: &PathBuf) -> WireEnrich {
    let mut e = WireEnrich::default();
    let Ok(text) = fs::read_to_string(wire) else {
        return e;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        // header line: {"type":"metadata","protocol_version":"1.x"}
        if v.get("type").and_then(Value::as_str) == Some("metadata") {
            continue;
        }
        let Some(msg) = v.get("message") else {
            continue;
        };
        let mty = msg.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = msg.get("payload");
        match mty {
            "ToolResult" => {
                if let Some(p) = payload {
                    let id = p
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let rv = p.get("return_value");
                    let is_error = rv
                        .and_then(|r| r.get("is_error"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    // Prefer `output`, fall back to `message`.
                    let output = rv
                        .and_then(|r| r.get("output"))
                        .and_then(Value::as_str)
                        .or_else(|| rv.and_then(|r| r.get("message")).and_then(Value::as_str))
                        .unwrap_or("")
                        .to_string();
                    let extras = rv
                        .and_then(|r| r.get("extras"))
                        .filter(|x| !x.is_null())
                        .cloned();
                    if !id.is_empty() {
                        e.tool_results.insert(
                            id,
                            WireToolResult {
                                is_error,
                                output,
                                extras,
                            },
                        );
                    }
                }
            }
            "StatusUpdate" => {
                if let Some(p) = payload {
                    let usage = p.get("token_usage").map(wire_usage);
                    let message_id = p
                        .get("message_id")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    e.statuses.push(WireStatus { usage, message_id });
                }
            }
            _ => {}
        }
        // Record a timestamp for content-bearing turn boundaries.
        if matches!(mty, "TurnBegin" | "ContentPart" | "ToolCall") {
            if let Some(ts) = v.get("timestamp").and_then(Value::as_f64).and_then(epoch_to_dt) {
                e.msg_timestamps.push(ts);
            }
        }
    }
    e
}

fn wire_usage(tu: &Value) -> Usage {
    let g = |k: &str| tu.get(k).and_then(Value::as_u64);
    Usage {
        input_tokens: g("input_other"),
        output_tokens: g("output"),
        cache_read_tokens: g("input_cache_read"),
        cache_creation_tokens: g("input_cache_creation"),
    }
}

/// Attach wire StatusUpdate usage / message_id to assistant messages positionally (one StatusUpdate
/// per assistant step in wire order).
fn attach_status_updates(messages: &mut [Message], enrich: &WireEnrich) {
    if enrich.statuses.is_empty() {
        return;
    }
    let mut it = enrich.statuses.iter();
    for m in messages.iter_mut() {
        if m.role != Role::Assistant {
            continue;
        }
        let Some(st) = it.next() else { break };
        if m.usage.is_none() {
            m.usage = st.usage.clone();
        }
        if m.id.is_none() {
            m.id = st.message_id.clone();
        }
    }
}

/// Best-effort: stamp the session's first user turn with the first wire timestamp, since per-message
/// alignment across compaction is unreliable. We set the first user message's timestamp only.
fn attach_timestamps(messages: &mut [Message], enrich: &WireEnrich) {
    let Some(first) = enrich.msg_timestamps.first().copied() else {
        return;
    };
    if let Some(m) = messages.iter_mut().find(|m| m.role == Role::User) {
        if m.timestamp.is_none() {
            m.timestamp = Some(first);
        }
    }
}

/// First and last event timestamps in a wire file (epoch floats), for created/updated bounds.
fn wire_time_bounds(wire: &Path) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let text = fs::read_to_string(wire).ok()?;
    let mut first = None;
    let mut last = None;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if let Some(ts) = v.get("timestamp").and_then(Value::as_f64).and_then(epoch_to_dt) {
            if first.is_none() {
                first = Some(ts);
            }
            last = Some(ts);
        }
    }
    Some((first?, last?))
}

fn epoch_to_dt(secs: f64) -> Option<DateTime<Utc>> {
    let nanos = (secs * 1e9).round() as i64;
    Some(DateTime::<Utc>::from_timestamp_nanos(nanos))
}

// ---------------------------------------------------------------------------
// md5 (dependency-free; only used for hashing short cwd strings)
// ---------------------------------------------------------------------------

/// Lowercase hex md5 of `data`. A small self-contained RFC 1321 implementation so we don't add a
/// crate dependency just to match kimi-cli's project-dir naming.
fn md5_hex(data: &[u8]) -> String {
    let mut a0: u32 = 0x6745_2301;
    let mut b0: u32 = 0xefcd_ab89;
    let mut c0: u32 = 0x98ba_dcfe;
    let mut d0: u32 = 0x1032_5476;

    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76a_a478, 0xe8c7_b756, 0x2420_70db, 0xc1bd_ceee, 0xf57c_0faf, 0x4787_c62a, 0xa830_4613,
        0xfd46_9501, 0x6980_98d8, 0x8b44_f7af, 0xffff_5bb1, 0x895c_d7be, 0x6b90_1122, 0xfd98_7193,
        0xa679_438e, 0x49b4_0821, 0xf61e_2562, 0xc040_b340, 0x265e_5a51, 0xe9b6_c7aa, 0xd62f_105d,
        0x0244_1453, 0xd8a1_e681, 0xe7d3_fbc8, 0x21e1_cde6, 0xc337_07d6, 0xf4d5_0d87, 0x455a_14ed,
        0xa9e3_e905, 0xfcef_a3f8, 0x676f_02d9, 0x8d2a_4c8a, 0xfffa_3942, 0x8771_f681, 0x6d9d_6122,
        0xfde5_380c, 0xa4be_ea44, 0x4bde_cfa9, 0xf6bb_4b60, 0xbebf_bc70, 0x289b_7ec6, 0xeaa1_27fa,
        0xd4ef_3085, 0x0488_1d05, 0xd9d4_d039, 0xe6db_99e5, 0x1fa2_7cf8, 0xc4ac_5665, 0xf429_2244,
        0x432a_ff97, 0xab94_23a7, 0xfc93_a039, 0x655b_59c3, 0x8f0c_cc92, 0xffef_f47d, 0x8584_5dd1,
        0x6fa8_7e4f, 0xfe2c_e6e0, 0xa301_4314, 0x4e08_11a1, 0xf753_7e82, 0xbd3a_f235, 0x2ad7_d2bb,
        0xeb86_d391,
    ];

    // padding
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut m = [0u32; 16];
        for (i, w) in m.iter_mut().enumerate() {
            *w = u32::from_le_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f
                .wrapping_add(a)
                .wrapping_add(K[i])
                .wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = String::with_capacity(32);
    for word in [a0, b0, c0, d0] {
        for byte in word.to_le_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_reference() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        // The real cwd → project-dir hash observed in ~/.kimi.
        assert_eq!(
            md5_hex(b"/Users/ember/elide/breadstuffs"),
            "a917b40bc491ae8a7f6f508be5446785"
        );
    }

    #[test]
    fn work_dirs_map_local_and_kaos() {
        let json = r#"{"work_dirs":[
            {"path":"/Users/ember/elide/breadstuffs","kaos":"local","last_session_id":"x"},
            {"path":"/remote/proj","kaos":"box","last_session_id":null}
        ]}"#;
        let dir = write_temp("kimi.json", json);
        let map = load_work_dirs(&dir.join("kimi.json"));
        assert_eq!(
            map.get("a917b40bc491ae8a7f6f508be5446785").unwrap(),
            &PathBuf::from("/Users/ember/elide/breadstuffs")
        );
        // non-local KAOS → "<kaos>_<md5>"
        let h = md5_hex(b"/remote/proj");
        assert_eq!(
            map.get(&format!("box_{h}")).unwrap(),
            &PathBuf::from("/remote/proj")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_default_model() {
        let toml = "theme = \"dark\"\ndefault_model = \"kimi-code/kimi-for-coding\"\nfoo = 1\n";
        let dir = write_temp("config.toml", toml);
        assert_eq!(
            default_model_from_config(&dir.join("config.toml")).as_deref(),
            Some("kimi-code/kimi-for-coding")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_bare_string_and_list() {
        let bare: Value =
            serde_json::from_str(r#"{"role":"user","content":"hello"}"#).unwrap();
        let m = context_message(&bare, &WireEnrich::default()).unwrap();
        assert_eq!(m.role, Role::User);
        assert_eq!(m.text().as_deref(), Some("hello"));

        let list: Value = serde_json::from_str(
            r#"{"role":"user","content":[{"type":"text","text":"hi"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAA","id":null}}]}"#,
        )
        .unwrap();
        let m = context_message(&list, &WireEnrich::default()).unwrap();
        assert_eq!(m.text().as_deref(), Some("hi"));
        assert!(matches!(
            m.content[1],
            Block::Image {
                ref data_ref,
                ..
            } if data_ref.as_deref() == Some("data:image/png;base64,AAA")
        ));
    }

    #[test]
    fn assistant_think_and_tool_calls() {
        let line = r#"{"role":"assistant","content":[{"type":"think","think":"reasoning","encrypted":"BLOB"},{"type":"text","text":"Let me look."}],"tool_calls":[{"type":"function","id":"tool_1","function":{"name":"Shell","arguments":"{\"command\":\"cat .gitmodules\"}"}}]}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let m = context_message(&v, &WireEnrich::default()).unwrap();
        assert_eq!(m.role, Role::Assistant);
        match &m.content[0] {
            Block::Thinking { text, encrypted, signature, .. } => {
                assert_eq!(text, "reasoning");
                assert_eq!(encrypted.as_deref(), Some("BLOB"));
                assert_eq!(signature.as_deref(), Some("BLOB"));
            }
            o => panic!("expected thinking, got {o:?}"),
        }
        assert!(matches!(&m.content[1], Block::Text { text } if text == "Let me look."));
        match &m.content[2] {
            Block::ToolUse { id, name, input } => {
                assert_eq!(id, "tool_1");
                assert_eq!(name, "Shell");
                assert_eq!(input["command"], "cat .gitmodules");
            }
            o => panic!("expected tool_use, got {o:?}"),
        }
    }

    #[test]
    fn tool_result_bare_string_and_wire_enrichment() {
        // context.jsonl tool record (bare-string content), no wire → uses context text.
        let line = r#"{"role":"tool","content":"<system>Command executed successfully.</system>","tool_call_id":"tool_1"}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let m = context_message(&v, &WireEnrich::default()).unwrap();
        assert_eq!(m.role, Role::Tool);
        match &m.content[0] {
            Block::ToolResult { tool_use_id, content, is_error, status, .. } => {
                assert_eq!(tool_use_id, "tool_1");
                assert!(content.contains("Command executed successfully"));
                assert!(!is_error);
                assert_eq!(status.as_deref(), Some("completed"));
            }
            o => panic!("expected tool_result, got {o:?}"),
        }

        // With wire enrichment, output/is_error come from return_value.
        let mut enrich = WireEnrich::default();
        enrich.tool_results.insert(
            "tool_1".into(),
            WireToolResult {
                is_error: true,
                output: "boom".into(),
                extras: Some(serde_json::json!({"k":"v"})),
            },
        );
        let m = context_message(&v, &enrich).unwrap();
        match &m.content[0] {
            Block::ToolResult { content, is_error, status, details, .. } => {
                assert_eq!(content, "boom");
                assert!(is_error);
                assert_eq!(status.as_deref(), Some("error"));
                assert_eq!(details.as_ref().unwrap()["k"], "v");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn arguments_empty_becomes_object() {
        assert_eq!(parse_arguments(None), Value::Object(Default::default()));
        assert_eq!(
            parse_arguments(Some(&Value::String("".into()))),
            Value::Object(Default::default())
        );
        assert_eq!(
            parse_arguments(Some(&Value::String("not json".into()))),
            Value::Object(Default::default())
        );
    }

    #[test]
    fn internal_roles_skipped() {
        for line in [
            r#"{"role":"_checkpoint","id":3}"#,
            r#"{"role":"_usage","token_count":7291}"#,
        ] {
            let v: Value = serde_json::from_str(line).unwrap();
            assert!(context_message(&v, &WireEnrich::default()).is_none());
        }
        let sp: Value =
            serde_json::from_str(r#"{"role":"_system_prompt","content":"You are Kimi."}"#).unwrap();
        let m = context_message(&sp, &WireEnrich::default()).unwrap();
        assert_eq!(m.role, Role::System);
        assert_eq!(m.text().as_deref(), Some("You are Kimi."));
    }

    #[test]
    fn parses_fixture_modern_session() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/kimi/modern/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/11111111-1111-1111-1111-111111111111");
        if !dir.exists() {
            return;
        }
        let r = SessionRef {
            id: "11111111-1111-1111-1111-111111111111".into(),
            harness: Harness::Kimi,
            path: dir.clone(),
            cwd: Some(PathBuf::from("/proj")),
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let s = Kimi { sessions: None, root: None }.parse(&r).unwrap();
        assert!(s.messages.iter().any(|m| m.role == Role::System));
        assert!(s.messages.iter().any(|m| m.role == Role::User));
        // assistant with a tool_use, and a tool result enriched from wire (is_error=false).
        assert!(s
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(b, Block::ToolUse { .. }))));
        let tr = s
            .messages
            .iter()
            .find(|m| m.role == Role::Tool)
            .and_then(|m| m.content.first());
        match tr {
            Some(Block::ToolResult { content, is_error, .. }) => {
                assert!(content.contains("README.md"));
                assert!(!is_error);
            }
            _ => panic!("expected tool result"),
        }
        // assistant message picked up usage + message_id from wire StatusUpdate.
        let asst = s.messages.iter().find(|m| m.role == Role::Assistant).unwrap();
        assert_eq!(asst.usage.as_ref().and_then(|u| u.output_tokens), Some(42));
        assert_eq!(asst.id.as_deref(), Some("chatcmpl-abc"));
    }

    #[test]
    fn parses_fixture_legacy_flat_session() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/kimi/legacy/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        if !dir.exists() {
            return;
        }
        let id = "22222222-2222-2222-2222-222222222222";
        let r = SessionRef {
            id: id.into(),
            harness: Harness::Kimi,
            path: dir.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        };
        let s = Kimi { sessions: None, root: None }.parse(&r).unwrap();
        assert!(s.messages.iter().any(|m| m.role == Role::User));
        assert!(s.messages.iter().any(|m| m.role == Role::Assistant));
    }

    /// Write `name`→`contents` into a fresh temp dir, return the dir.
    fn write_temp(name: &str, contents: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!(
            "cv-kimi-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(name), contents).unwrap();
        d
    }
}
