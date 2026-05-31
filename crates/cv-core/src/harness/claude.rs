//! Claude Code adapter — `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl`.
//!
//! See `docs/FORMATS.md`. Key points: one session per `.jsonl` file; each line is a typed record;
//! `cwd` is read from inside the transcript (the dir-name encoding is lossy), and the conversation is
//! threaded via `uuid`/`parentUuid`.

use super::Adapter;
use crate::ir::*;
use crate::stream::{CollectSink, Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use walkdir::WalkDir;

pub struct Claude {
    root: Option<PathBuf>,
}

impl Claude {
    pub fn new() -> Self {
        let root = dirs::home_dir().map(|h| h.join(".claude").join("projects"));
        Claude {
            root: root.filter(|p| p.exists()),
        }
    }
}

impl Default for Claude {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapter for Claude {
    fn harness(&self) -> Harness {
        Harness::Claude
    }

    fn storage_root(&self) -> Option<PathBuf> {
        self.root.clone()
    }

    fn discover(&self) -> Result<Vec<SessionRef>> {
        let Some(root) = &self.root else {
            return Ok(vec![]);
        };
        // Session files sit at projects/<encoded>/<sid>.jsonl (depth 2). Subagent transcripts live
        // deeper (…/<sid>/subagents/…), which max_depth(2) naturally excludes. Collect paths (cheap),
        // then scan (read + parse) them in parallel.
        let paths: Vec<_> = WalkDir::new(root)
            .min_depth(2)
            .max_depth(2)
            .into_iter()
            .filter_map(|e| e.ok())
            .map(|e| e.into_path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .collect();
        Ok(crate::par_filter_map(paths, |path| {
            crate::discover_cache::cached_scan(&path, || match scan(&path) {
                Ok(r) => Some(r),
                Err(e) => {
                    eprintln!("cv: skipping {}: {e:#}", path.display());
                    None
                }
            })
        }))
    }

    fn parse(&self, r: &SessionRef) -> Result<Session> {
        crate::stream::collect(self, r)
    }

    fn stream(
        &self,
        r: &SessionRef,
        opts: &ParseOptions,
        sink: &mut dyn MessageSink,
    ) -> Result<Session> {
        // Stream the transcript line-by-line — a single Claude session can be >1 GB, and
        // `read_to_string` would resident-spike the whole file (this OOM-killed cv on the
        // 1.35 GB polyana transcript). `BufReader::lines()` keeps peak at O(largest line); handing
        // each message to `sink` (instead of accumulating a `Vec`) keeps it at O(largest message).
        let file = fs::File::open(&r.path)
            .with_context(|| format!("opening {}", r.path.display()))?;
        Ok(stream_reader(
            &r.id,
            BufReader::new(file),
            Some(r.path.clone()),
            opts,
            sink,
        ))
    }

    fn can_emit(&self) -> bool {
        false // TODO: Claude as a conversion target
    }
}

/// Fully parse a Claude `.jsonl` transcript from its text contents into a [`Session`] (full
/// fidelity, including `extra` sidecars).
///
/// Pure (no filesystem). `id` is the session id (usually the file stem); `source_path` is recorded
/// for provenance when known. This is the whole-`Session` convenience over [`stream_str`].
pub fn parse_str(id: &str, text: &str, source_path: Option<PathBuf>) -> Session {
    let mut sink = CollectSink::default();
    let mut session = stream_str(id, text, source_path, &ParseOptions::full(), &mut sink);
    session.messages = sink.messages;
    session
}

/// Stream a transcript's text into `sink`, returning the [`Session`] metadata with empty
/// `messages`. The pure (no-filesystem) core that both [`parse_str`] and the on-disk
/// [`Adapter::stream`] delegate to.
pub fn stream_str(
    id: &str,
    text: &str,
    source_path: Option<PathBuf>,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Session {
    let mut session = new_session(id, source_path);
    for line in text.lines() {
        if ingest_line(&mut session, line, opts, sink) == Flow::Stop {
            break;
        }
    }
    session
}

/// Fully parse from a buffered reader (full fidelity) — the whole-`Session` convenience over
/// [`stream_reader`], used by tests. The on-disk [`Adapter::parse`] goes through
/// [`crate::stream::collect`] → [`Adapter::stream`] → [`stream_reader`] instead.
pub fn parse_reader<R: BufRead>(id: &str, reader: R, source_path: Option<PathBuf>) -> Session {
    let mut sink = CollectSink::default();
    let mut session = stream_reader(id, reader, source_path, &ParseOptions::full(), &mut sink);
    session.messages = sink.messages;
    session
}

/// Streaming parse from a buffered reader — the memory-safe core the on-disk [`Adapter::stream`]
/// uses. A single Claude transcript can exceed 1 GB; reading it whole (`read_to_string`)
/// resident-spiked cv to ~65 GB across the corpus and OOM-killed it. Reading line-by-line and
/// handing each message to `sink` keeps peak at O(largest line). Returns the [`Session`] metadata
/// with empty `messages` (they went to the sink).
pub fn stream_reader<R: BufRead>(
    id: &str,
    reader: R,
    source_path: Option<PathBuf>,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Session {
    let mut session = new_session(id, source_path);
    for line in reader.lines() {
        // A read error on one line (rare: invalid UTF-8 chunk) shouldn't abort the whole session.
        let Ok(line) = line else { continue };
        if ingest_line(&mut session, &line, opts, sink) == Flow::Stop {
            break;
        }
    }
    session
}

/// An empty Claude [`Session`] shell, ready to accumulate records.
fn new_session(id: &str, source_path: Option<PathBuf>) -> Session {
    Session {
        id: id.to_string(),
        harness: Harness::Claude,
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        model: None,
        git: None,
        messages: Vec::new(),
        source_path,
        extra: serde_json::Map::new(),
    }
}

/// Fold one JSONL record into `session`: update session-level metadata, and hand any conversational
/// message to `sink`. Tolerates blank/corrupt lines (skips them). Returns the sink's [`Flow`] so the
/// caller can stop early. `opts` gates how much of each message is materialized.
fn ingest_line(
    session: &mut Session,
    line: &str,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Flow {
    let line = line.trim();
    if line.is_empty() {
        return Flow::Continue;
    }
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return Flow::Continue, // tolerate the occasional corrupt line
    };
    let ty = v.get("type").and_then(Value::as_str).unwrap_or("");

    // session-level metadata records
    match ty {
        "ai-title" => {
            if let Some(t) = v.get("aiTitle").and_then(Value::as_str) {
                session.title = Some(t.to_string());
            }
            return Flow::Continue;
        }
        // `summary`/`last-prompt` carry a title-ish/leaf pointer but no message body.
        // `summary` lines have a `summary` string we can fall back to for the title.
        "summary" => {
            if session.title.is_none() {
                if let Some(t) = v.get("summary").and_then(Value::as_str) {
                    session.title = Some(t.to_string());
                }
            }
            return Flow::Continue;
        }
        // Pure bookkeeping / live-process records with no conversational payload.
        // `progress`, `started`, `result` are sub-agent hook/streaming telemetry;
        // `queue-operation` is the input queue; `mode`/`permission-mode`/`attachment`/
        // `last-prompt` are UI state. Ignore (unknown types fall through and are ignored too).
        "mode" | "permission-mode" | "last-prompt" | "attachment" | "progress" | "started"
        | "result" | "queue-operation" | "x-quota" | "file-history-snapshot" => return Flow::Continue,
        _ => {}
    }

    if session.cwd.is_none() {
        if let Some(cwd) = v.get("cwd").and_then(Value::as_str) {
            session.cwd = Some(PathBuf::from(cwd));
        }
    }
    if session.git.is_none() {
        if let Some(branch) = v.get("gitBranch").and_then(Value::as_str) {
            session.git = Some(GitInfo {
                branch: Some(branch.to_string()),
                ..Default::default()
            });
        }
    }
    if let Some(ts) = v.get("timestamp").and_then(Value::as_str).and_then(parse_ts) {
        session.created_at.get_or_insert(ts);
        session.updated_at = Some(ts);
    }

    if let Some(msg) = parse_message(ty, &v, opts) {
        if session.model.is_none() {
            session.model = msg.model.clone();
        }
        return sink.message(msg);
    }
    Flow::Continue
}

/// Sub-agent transcripts spawned by `parent_path`'s session (Claude Code's Task tool). They live at
/// `<projects>/<encoded>/<sid>/subagents/<agent>.jsonl` — a sibling `subagents/` dir next to the
/// parent's `<sid>.jsonl`. Returned as lightweight refs (newest first), scanned in parallel.
pub fn subagent_refs(parent_path: &std::path::Path) -> Vec<SessionRef> {
    let Some(stem) = parent_path.file_stem().and_then(|s| s.to_str()) else {
        return vec![];
    };
    let Some(dir) = parent_path.parent().map(|d| d.join(stem).join("subagents")) else {
        return vec![];
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                paths.push(p);
            }
        }
    }
    let mut refs = crate::par_filter_map(paths, |p| scan(&p).ok());
    refs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    refs
}

/// Cheap metadata-only scan for `discover`.
fn scan(path: &std::path::Path) -> Result<SessionRef> {
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);

    let mut cwd = None;
    let mut title = None;
    let mut created_at: Option<DateTime<Utc>> = None;
    let mut updated_at: Option<DateTime<Utc>> = None;
    let mut message_count = 0usize;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        if ty == "ai-title" {
            if let Some(t) = v.get("aiTitle").and_then(Value::as_str) {
                title = Some(t.to_string());
            }
        }
        if cwd.is_none() {
            if let Some(c) = v.get("cwd").and_then(Value::as_str) {
                cwd = Some(PathBuf::from(c));
            }
        }
        if let Some(ts) = v.get("timestamp").and_then(Value::as_str).and_then(parse_ts) {
            created_at.get_or_insert(ts);
            updated_at = Some(ts);
        }
        if matches!(ty, "user" | "assistant") {
            message_count += 1;
        }
    }

    Ok(SessionRef {
        id,
        harness: Harness::Claude,
        path: path.to_path_buf(),
        cwd,
        title,
        created_at,
        updated_at,
        message_count,
    })
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Turn one record into an IR [`Message`]. Handles `user`/`assistant` (the conversation) and the
/// content-bearing `system` lines (compaction notices, away/error summaries, slash-command output).
/// Returns `None` for line types with no conversational payload.
fn parse_message(ty: &str, v: &Value, opts: &ParseOptions) -> Option<Message> {
    if ty == "system" {
        return parse_system_message(v, opts);
    }

    let role = match ty {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };

    let msg = v.get("message")?;
    let id = v
        .get("uuid")
        .and_then(Value::as_str)
        .map(str::to_string);
    let parent_id = v
        .get("parentUuid")
        .and_then(Value::as_str)
        .map(str::to_string);
    let timestamp = v.get("timestamp").and_then(Value::as_str).and_then(parse_ts);
    let model = msg.get("model").and_then(Value::as_str).map(str::to_string);
    let usage = msg.get("usage").map(parse_usage);

    let mut blocks = Vec::new();
    match msg.get("content") {
        Some(Value::String(s)) => blocks.push(Block::Text { text: s.clone() }),
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(b) = parse_block(item) {
                    blocks.push(b);
                }
            }
        }
        _ => {}
    }

    // A `user` line carrying only tool results is really a Tool turn.
    let role = if role == Role::User
        && !blocks.is_empty()
        && blocks
            .iter()
            .all(|b| matches!(b, Block::ToolResult { .. }))
    {
        Role::Tool
    } else {
        role
    };

    let mut extra = Map::new();
    if opts.extra {
        collect_extra(v, msg, &mut extra);
    }

    Some(Message {
        id,
        parent_id,
        role,
        timestamp,
        model,
        content: blocks,
        usage,
        extra,
    })
}

/// `system` records cover a grab-bag of subtypes. Only some carry user-visible text worth surfacing
/// as a [`Role::System`] message; the rest (timing, hook bookkeeping, killed-agent notices) have no
/// body and are dropped. We preserve `subtype`/`level`/metadata in `extra` either way.
fn parse_system_message(v: &Value, opts: &ParseOptions) -> Option<Message> {
    let subtype = v.get("subtype").and_then(Value::as_str).unwrap_or("");
    // Body text lives either at top-level `content` (a string) or, for compaction, is implied.
    let content = v.get("content").and_then(Value::as_str);

    // Subtypes with no human-readable body — keep them out of the message stream.
    let bodyless = matches!(
        subtype,
        "turn_duration" | "agents_killed" | "stop_hook_summary"
    );
    if bodyless || (content.is_none() && subtype != "compact_boundary") {
        return None;
    }

    let text = match content {
        Some(c) => c.to_string(),
        // compact_boundary often has only structured metadata; synthesize a marker.
        None => "[conversation compacted]".to_string(),
    };

    let id = v.get("uuid").and_then(Value::as_str).map(str::to_string);
    let parent_id = v
        .get("parentUuid")
        .and_then(Value::as_str)
        .map(str::to_string);
    let timestamp = v.get("timestamp").and_then(Value::as_str).and_then(parse_ts);

    let mut extra = Map::new();
    if opts.extra {
        if !subtype.is_empty() {
            extra.insert("subtype".into(), Value::String(subtype.to_string()));
        }
        for key in [
            "level",
            "compactMetadata",
            "cause",
            "error",
            "isMeta",
            "isSidechain",
            "agentId",
            "slug",
            "logicalParentUuid",
        ] {
            if let Some(val) = v.get(key) {
                extra.insert(key.to_string(), val.clone());
            }
        }
    }

    Some(Message {
        id,
        parent_id,
        role: Role::System,
        timestamp,
        model: None,
        content: vec![Block::Text { text }],
        usage: None,
        extra,
    })
}

/// Preserve Claude-specific record/sidecar fields that the IR has no first-class home for, so
/// conversions and the viewer can still surface them. This is where the rich `toolUseResult`
/// sidecar (diffs, todos, command stdout/stderr, structured patches) is kept verbatim.
fn collect_extra(v: &Value, msg: &Value, extra: &mut Map<String, Value>) {
    // The richest dropped field: the structured tool-result sidecar that sits *next to* the plain
    // `tool_result` text block (structuredPatch, oldTodos/newTodos, stdout/stderr, file contents…).
    if let Some(tur) = v.get("toolUseResult") {
        extra.insert("toolUseResult".into(), tur.clone());
    }
    // Threading / provenance fields beyond uuid/parentUuid.
    for key in [
        "logicalParentUuid",
        "isSidechain",
        "isMeta",
        "isCompactSummary",
        "isApiErrorMessage",
        "agentId",
        "slug",
        "version",
        "requestId",
        "parentToolUseID",
        "toolUseID",
        "sourceToolAssistantUUID",
        "attributionAgent",
        "attributionMcpServer",
        "attributionMcpTool",
        "attributionSkill",
        "userType",
    ] {
        if let Some(val) = v.get(key) {
            // Skip the overwhelmingly-common defaults to keep `extra` lean.
            let skip = matches!(
                (key, val),
                ("userType", Value::String(s)) if s == "external"
            ) || matches!((key, val), ("isSidechain", Value::Bool(false)));
            if !skip {
                extra.insert(key.to_string(), val.clone());
            }
        }
    }
    // Assistant API-response metadata (stop reason, the API message id, etc.).
    for key in ["id", "stop_reason", "stop_sequence", "stop_details"] {
        if let Some(val) = msg.get(key).filter(|val| !val.is_null()) {
            extra.insert(format!("message.{key}"), val.clone());
        }
    }
}

fn parse_block(item: &Value) -> Option<Block> {
    match item.get("type").and_then(Value::as_str)? {
        "text" => Some(Block::Text {
            text: item.get("text").and_then(Value::as_str)?.to_string(),
        }),
        "thinking" => Some(Block::Thinking {
            text: item
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            signature: item
                .get("signature")
                .and_then(Value::as_str)
                .map(str::to_string),
            encrypted: None,
            redacted: false,
        }),
        // Encrypted reasoning the API returns when thinking can't be shown in the clear. The plaintext
        // is intentionally absent; stash the opaque blob in `encrypted` so it's not silently lost.
        "redacted_thinking" => Some(Block::Thinking {
            text: String::new(),
            signature: None,
            encrypted: item
                .get("data")
                .and_then(Value::as_str)
                .map(str::to_string),
            redacted: true,
        }),
        "tool_use" => Some(Block::ToolUse {
            id: item.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
            name: item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            input: item.get("input").cloned().unwrap_or(Value::Null),
        }),
        "tool_result" => Some(Block::ToolResult {
            tool_use_id: item
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            content: coerce_text(item.get("content")),
            is_error: item.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            tool_name: None,
            status: None,
            details: None,
        }),
        "image" => {
            let source = item.get("source");
            let media_type = source
                .and_then(|s| s.get("media_type"))
                .and_then(Value::as_str)
                .map(str::to_string);
            // Sources come in three shapes: inline `base64` (we don't inline megabytes of pixels into
            // the IR — record only the kind), a Files-API `file_id`, or a `url`. Keep a reference, not
            // the bytes.
            let data_ref = source.and_then(|s| match s.get("type").and_then(Value::as_str) {
                Some("url") => s.get("url").and_then(Value::as_str).map(str::to_string),
                Some("file") => s
                    .get("file_id")
                    .and_then(Value::as_str)
                    .map(|id| format!("file:{id}")),
                Some("base64") | None => s
                    .get("data")
                    .and_then(Value::as_str)
                    .map(|_| "base64:inline".to_string()),
                Some(other) => Some(other.to_string()),
            });
            Some(Block::Image {
                media_type,
                data_ref,
            })
        }
        // Some user turns embed a `document` block (PDF/text attachments); surface it as a File.
        "document" => {
            let source = item.get("source");
            let mime = source
                .and_then(|s| s.get("media_type"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let path = item
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string);
            let data = source.and_then(|s| match s.get("type").and_then(Value::as_str) {
                Some("url") => s.get("url").and_then(Value::as_str).map(str::to_string),
                Some("file") => s
                    .get("file_id")
                    .and_then(Value::as_str)
                    .map(|id| format!("file:{id}")),
                _ => None,
            });
            Some(Block::File {
                mime,
                path,
                source: data,
            })
        }
        _ => None,
    }
}

/// Claude tool_result `content` is sometimes a string, sometimes `[{type:text,text}]`, and sometimes
/// a mix of text and `image` blocks (e.g. a screenshot tool). We flatten to text, replacing image
/// blocks with a placeholder so they aren't silently dropped from the searchable transcript.
fn coerce_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| match i.get("type").and_then(Value::as_str) {
                Some("image") => Some("[image]".to_string()),
                _ => i.get("text").and_then(Value::as_str).map(str::to_string),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn parse_usage(v: &Value) -> Usage {
    let get = |k: &str| v.get(k).and_then(Value::as_u64);
    Usage {
        input_tokens: get("input_tokens"),
        output_tokens: get("output_tokens"),
        cache_read_tokens: get("cache_read_input_tokens"),
        cache_creation_tokens: get("cache_creation_input_tokens"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claude/");
        std::fs::read_to_string(format!("{path}{name}"))
            .unwrap_or_else(|e| panic!("reading fixture {name}: {e}"))
    }

    #[test]
    fn rich_blocks_variant() {
        let s = parse_str("s1", &fixture("rich_blocks.jsonl"), None);
        // session-level metadata: ai-title wins over summary; cwd / git / model captured.
        assert_eq!(s.title.as_deref(), Some("Parser refactor session"));
        assert_eq!(s.cwd.as_deref(), Some(std::path::Path::new("/work/proj")));
        assert_eq!(s.git.as_ref().unwrap().branch.as_deref(), Some("main"));
        assert_eq!(s.model.as_deref(), Some("claude-opus-4"));

        // user, assistant, tool-result turn, image/document turn.
        assert_eq!(s.messages.len(), 4);

        let asst = &s.messages[1];
        assert_eq!(asst.role, Role::Assistant);
        // thinking (with signature), redacted_thinking (encrypted), text, tool_use all preserved.
        let kinds: Vec<_> = asst.content.iter().collect();
        assert!(matches!(kinds[0], Block::Thinking { signature: Some(sig), .. } if sig == "SIGREDACTED=="));
        assert!(
            matches!(kinds[1], Block::Thinking { text, encrypted: Some(e), signature: None, .. } if text.is_empty() && e == "ENCREDACTED==")
        );
        assert!(matches!(kinds[2], Block::Text { .. }));
        assert!(matches!(kinds[3], Block::ToolUse { name, .. } if name == "Edit"));
        // assistant API metadata + requestId captured in extra.
        assert_eq!(asst.extra.get("message.stop_reason").unwrap(), "tool_use");
        assert_eq!(asst.extra.get("message.id").unwrap(), "msg_abc");
        assert_eq!(asst.extra.get("requestId").unwrap(), "req_123");

        // tool-result user line is reclassified as a Tool turn, and the rich sidecar is preserved.
        let tool = &s.messages[2];
        assert_eq!(tool.role, Role::Tool);
        let tur = tool.extra.get("toolUseResult").expect("sidecar kept");
        assert!(tur.get("structuredPatch").is_some());
        assert_eq!(tur.get("filePath").unwrap(), "/work/proj/a.rs");

        // image + document.
        let media = &s.messages[3];
        assert!(matches!(&media.content[0], Block::Image { media_type: Some(m), data_ref: Some(r) } if m == "image/png" && r == "base64:inline"));
        assert!(matches!(&media.content[1], Block::File { mime: Some(m), path: Some(p), .. } if m == "application/pdf" && p == "spec.pdf"));
    }

    #[test]
    fn system_lines_variant() {
        let s = parse_str("s2", &fixture("system_lines.jsonl"), None);
        let systems: Vec<_> = s
            .messages
            .iter()
            .filter(|m| m.role == Role::System)
            .collect();
        // compact_boundary, away_summary, local_command, api_error surface; turn_duration +
        // agents_killed (bodyless) are dropped.
        assert_eq!(systems.len(), 4);
        assert!(systems
            .iter()
            .any(|m| m.text().as_deref() == Some("Conversation compacted")
                && m.extra.get("subtype").unwrap() == "compact_boundary"
                && m.extra.get("compactMetadata").is_some()));
        assert!(systems
            .iter()
            .any(|m| m.extra.get("subtype").unwrap() == "away_summary"));
        assert!(systems
            .iter()
            .any(|m| m.extra.get("subtype").unwrap() == "api_error"
                && m.extra.get("level").unwrap() == "error"));
        // no bodyless subtypes leaked in.
        assert!(!systems
            .iter()
            .any(|m| matches!(m.extra.get("subtype").and_then(Value::as_str), Some("turn_duration" | "agents_killed"))));
    }

    #[test]
    fn sidechain_subagent_variant() {
        let s = parse_str("sub1", &fixture("sidechain_subagent.jsonl"), None);
        // progress / started / result are non-conversational and skipped.
        assert_eq!(s.messages.len(), 2);
        let first = &s.messages[0];
        // isSidechain / isMeta / agentId / slug preserved for sub-agent reconstruction.
        assert_eq!(first.extra.get("isSidechain").unwrap(), true);
        assert_eq!(first.extra.get("isMeta").unwrap(), true);
        assert_eq!(first.extra.get("agentId").unwrap(), "a4a55af");
        assert_eq!(first.extra.get("slug").unwrap(), "magical-wondering-rabbit");
        // attribution metadata on the assistant turn.
        assert_eq!(
            s.messages[1].extra.get("attributionAgent").unwrap(),
            "general-purpose"
        );
    }

    #[test]
    fn compact_summary_variant() {
        let s = parse_str("s3", &fixture("compact_summary.jsonl"), None);
        let first = &s.messages[0];
        assert_eq!(first.extra.get("isCompactSummary").unwrap(), true);
        // todo sidecar preserved on the tool turn.
        let tool = s.messages.iter().find(|m| m.role == Role::Tool).unwrap();
        let tur = tool.extra.get("toolUseResult").unwrap();
        assert!(tur.get("newTodos").is_some());
    }

    #[test]
    fn common_defaults_stay_out_of_extra() {
        // A vanilla user line should not bloat `extra` with userType=external / isSidechain=false.
        let line = r#"{"type":"user","uuid":"u1","parentUuid":null,"userType":"external","isSidechain":false,"sessionId":"s","timestamp":"2026-05-01T00:00:00Z","message":{"role":"user","content":"hi"}}"#;
        let s = parse_str("s", line, None);
        assert!(s.messages[0].extra.is_empty());
    }

    #[test]
    fn tolerates_corrupt_and_unknown_lines() {
        let text = "not json\n{\"type\":\"x-quota\"}\n{\"type\":\"user\",\"uuid\":\"u1\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n";
        let s = parse_str("s", text, None);
        assert_eq!(s.messages.len(), 1);
    }

    #[test]
    fn bulk_opts_skip_extra_but_keep_text() {
        // The bulk path (`ParseOptions::bulk()`) must NOT materialize the fat `toolUseResult`
        // sidecar — that's the whole point of gating `extra` for index/search/dataset — while the
        // full path (`parse`) keeps it. The searchable text must be identical either way.
        let text = fixture("rich_blocks.jsonl");

        let full = parse_str("s1", &text, None);
        let tool_full = full.messages.iter().find(|m| m.role == Role::Tool).unwrap();
        assert!(
            tool_full.extra.get("toolUseResult").is_some(),
            "full parse must keep the sidecar"
        );

        let mut sink = CollectSink::default();
        let mut bulk = stream_str("s1", &text, None, &ParseOptions::bulk(), &mut sink);
        bulk.messages = sink.messages;
        for m in &bulk.messages {
            assert!(m.extra.is_empty(), "bulk opts must leave every message's extra empty");
        }
        assert_eq!(
            full.searchable_text(),
            bulk.searchable_text(),
            "gating extra must not change searchable text"
        );
    }

    #[test]
    fn stream_can_stop_early() {
        // A sink returning Stop ends the parse without visiting the rest of the transcript.
        let text = fixture("rich_blocks.jsonl");
        let mut seen = 0usize;
        let mut sink = |_m: Message| {
            seen += 1;
            Flow::Stop
        };
        let _ = stream_str("s1", &text, None, &ParseOptions::bulk(), &mut sink);
        assert_eq!(seen, 1, "Stop after the first message must halt streaming");
    }

    #[test]
    fn parse_reader_matches_parse_str() {
        // The streaming on-disk path (`parse_reader`, used by `Adapter::parse` to avoid loading a
        // multi-GB transcript whole) must produce the same Session as the whole-text `parse_str`.
        // Both delegate to `ingest_line`; this guards the OOM-fix refactor against divergence.
        let text = fixture("rich_blocks.jsonl");
        let whole = parse_str("s1", &text, None);
        let streamed = parse_reader("s1", std::io::Cursor::new(text.as_bytes()), None);
        assert_eq!(whole.title, streamed.title);
        assert_eq!(whole.cwd, streamed.cwd);
        assert_eq!(whole.git.as_ref().map(|g| g.branch.clone()), streamed.git.as_ref().map(|g| g.branch.clone()));
        assert_eq!(whole.model, streamed.model);
        assert_eq!(whole.created_at, streamed.created_at);
        assert_eq!(whole.updated_at, streamed.updated_at);
        assert_eq!(whole.messages.len(), streamed.messages.len());
        assert_eq!(whole.searchable_text(), streamed.searchable_text());
    }
}
