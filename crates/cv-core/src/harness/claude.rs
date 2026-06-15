//! Claude Code adapter — `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl`.
//!
//! See `docs/FORMATS.md`. Key points: one session per `.jsonl` file; each line is a typed record;
//! `cwd` is read from inside the transcript (the dir-name encoding is lossy), and the conversation is
//! threaded via `uuid`/`parentUuid`.

use super::{parse_ts, Adapter};
use crate::ir::*;
use crate::lazy::{Span, Text, INLINE_MAX};
use crate::stream::{CollectSink, Flow, MessageSink, ParseOptions};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::value::RawValue;
use serde_json::{Map, Value};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use walkdir::WalkDir;

/// Context for turning large content fields into lazy [`Span`]s during streaming: the raw bytes of
/// the current record line and the file offset where they begin. Present only on the on-disk
/// streaming path (which knows file offsets); the pure-string [`parse_str`] path passes `None` and
/// keeps content inline.
struct SpanCtx<'a> {
    slice: &'a [u8],
    base_off: u64,
}

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
    // Pure-string path (parse_str, tests): no file offsets, so all content stays inline.
    super::for_each_json_line_str(text, |v| ingest_value(&mut session, &v, opts, sink, None));
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
    stream_reader_from(id, reader, source_path, 0, opts, sink)
}

/// [`stream_reader`] generalized to a reader positioned at byte `start_off` (a **record start**)
/// of the source — the seek-cooperation entry [`crate::offsets::stream_range`] drives after
/// seeking to a recorded message offset. Every claude record parses independently of the skipped
/// prefix (per-line state only feeds *session* metadata, never message content), so the messages
/// streamed from here are identical to the tail of a full stream. With `start_off > 0` the
/// returned `Session` metadata is partial (head records were skipped) — seek callers use the
/// recorded metadata instead.
pub(crate) fn stream_reader_from<R: BufRead>(
    id: &str,
    reader: R,
    source_path: Option<PathBuf>,
    start_off: u64,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Session {
    let mut session = new_session(id, source_path);
    // Read raw bytes (not `lines()`) so we know each record's byte offset in the file — that's what
    // lets large content fields become lazy [`Span`]s pointing back into the source instead of owned
    // strings. `read_until` keeps peak at O(largest line) just like `lines()` did.
    let mut reader = reader;
    let mut buf: Vec<u8> = Vec::new();
    let mut file_off: u64 = start_off;
    loop {
        buf.clear();
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if ingest_bytes(&mut session, &buf[..n], file_off, opts, sink) == Flow::Stop {
            break;
        }
        file_off += n as u64;
    }
    session
}

/// Streaming ingest of one raw record line (with its file offset) — parses, updates metadata, and
/// emits the message to `sink`, turning large content fields into [`Span`]s into the source file.
fn ingest_bytes(
    session: &mut Session,
    buf: &[u8],
    file_off: u64,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
) -> Flow {
    // Trim leading/trailing ASCII whitespace (incl. the trailing `\n`), tracking how far the content
    // shifted so span offsets stay relative to the file.
    let lead = buf.iter().take_while(|b| b.is_ascii_whitespace()).count();
    let end = buf.len() - buf[lead..].iter().rev().take_while(|b| b.is_ascii_whitespace()).count();
    if lead >= end {
        return Flow::Continue;
    }
    let slice = &buf[lead..end];
    let v: Value = match serde_json::from_slice(slice) {
        Ok(v) => v,
        Err(_) => return Flow::Continue,
    };
    // Only defer large content to spans when the consumer asked for it (partial-access). Bulk/full
    // consumers read everything, so inline content is cheaper (no mmap on resolve).
    let ctx = opts.spans.then(|| SpanCtx {
        slice,
        base_off: file_off + lead as u64,
    });
    ingest_value(session, &v, opts, sink, ctx.as_ref())
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

/// Shared record handler: session-level metadata + emit the conversational message to `sink`. With a
/// [`SpanCtx`] (streaming on-disk path) large content fields become [`Span`]s; without it (string
/// path) they're inline.
fn ingest_value(
    session: &mut Session,
    v: &Value,
    opts: &ParseOptions,
    sink: &mut dyn MessageSink,
    span: Option<&SpanCtx>,
) -> Flow {
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

    if let Some(mut msg) = parse_message(ty, v, opts, span) {
        // Offset-recording pass: stamp the record's byte offset so `cv index` can persist a seek
        // point for this message (see [`crate::offsets`]). Requires the on-disk span path (`span`
        // carries the offset); ordinary lazy/bulk/full streams never set `opts.offsets`.
        if opts.offsets {
            if let Some(ctx) = span {
                msg.extra
                    .insert(crate::offsets::OFFSET_KEY.into(), ctx.base_off.into());
            }
        }
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
    refs.sort_by_key(|r| std::cmp::Reverse(r.created_at));
    refs
}

/// A sub-agent transcript enriched with the metadata Claude Code records *around* it: the
/// `*.meta.json` sidecar (agent type / human description / the spawning tool_use id), the workflow
/// it belongs to (when spawned by a `Workflow`), and — for workflow agents — the structured result
/// the orchestrator journaled (`status` + `summary`, the agent's *real* return value).
///
/// This is the second dimension of a Claude session that flat parsing misses entirely: a deep
/// session fans out into hundreds of these, and the meta/journal sidecars are where their *purpose*
/// and *outcome* live.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SubagentInfo {
    /// The transcript itself (id = `agent-<agentId>`), parseable like any other session.
    pub session: SessionRef,
    /// `agentType` from the sidecar: the subagent kind (`general-purpose`, `Explore`,
    /// `workflow-subagent`, a named custom agent, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// `description` from the sidecar: the human one-line task the parent gave it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `toolUseId` from the sidecar: the `Agent`/`Task` tool_use in the *parent* transcript that
    /// spawned this agent. Links the child back to the exact turn that launched it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// The workflow run id (`wf_…`) this agent belongs to, or `None` for a directly-spawned
    /// (`Agent`/`Task`) sub-agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// For workflow agents: the journaled outcome status (`done` / `partial` / `failed` / …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_status: Option<String>,
    /// For workflow agents: the journaled result summary — the agent's structured return value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
}

impl SubagentInfo {
    /// `agentId` (the `agent-<id>` stem with the `agent-` prefix stripped) — the key the workflow
    /// journal and the transcript records use.
    pub fn agent_id(&self) -> &str {
        self.session.id.strip_prefix("agent-").unwrap_or(&self.session.id)
    }
}

/// The full sub-agent forest a Claude session spawned: every directly-spawned (`Agent`/`Task`)
/// sub-agent **and** every `Workflow` sub-agent (which live one level deeper, under
/// `subagents/workflows/<wf>/`), each enriched with its `*.meta.json` sidecar and — for workflow
/// agents — the orchestrator's journaled result.
///
/// Flat [`subagent_refs`] only sees the top-level `subagents/*.jsonl`, so it misses the entire
/// workflow tier (which on a heavy session is the *majority* of the agents) and drops the
/// meta/journal sidecars that carry each agent's purpose and outcome. Newest first.
pub fn subagent_tree(parent_path: &std::path::Path) -> Vec<SubagentInfo> {
    let Some(stem) = parent_path.file_stem().and_then(|s| s.to_str()) else {
        return vec![];
    };
    let Some(base) = parent_path.parent().map(|d| d.join(stem).join("subagents")) else {
        return vec![];
    };

    let mut out: Vec<SubagentInfo> = Vec::new();

    // Tier 1: directly-spawned sub-agents at `subagents/agent-*.jsonl`.
    out.extend(agents_in_dir(&base, None));

    // Tier 2: workflow sub-agents at `subagents/workflows/<wf>/agent-*.jsonl`, grouped by run id,
    // each annotated with the result the workflow `journal.jsonl` recorded for its agentId.
    let wf_root = base.join("workflows");
    if let Ok(rd) = fs::read_dir(&wf_root) {
        let wf_dirs: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        let groups = crate::par_flat_map(wf_dirs, |wf_dir| {
            let run_id = wf_dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let journal = read_workflow_journal(&wf_dir.join("journal.jsonl"));
            agents_in_dir(&wf_dir, Some((&run_id, &journal)))
        });
        out.extend(groups);
    }

    out.sort_by_key(|s| std::cmp::Reverse(s.session.created_at));
    out
}

/// A workflow `journal.jsonl` reduced to `agentId → (status, summary)` — the orchestrator's
/// journaled result per agent (both fields optional: string results carry no separate status).
type JournalResults = std::collections::HashMap<String, (Option<String>, Option<String>)>;

/// Scan one directory for `agent-*.jsonl` transcripts, pairing each with its `*.meta.json` sidecar
/// and (for workflow dirs) the journaled result for its agentId. `wf` is `Some((run_id, journal))`
/// for a workflow dir, `None` for the top-level `subagents/` dir.
fn agents_in_dir(
    dir: &std::path::Path,
    wf: Option<(&str, &JournalResults)>,
) -> Vec<SubagentInfo> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            // Only `agent-*.jsonl`; skip `journal.jsonl` and the `.meta.json` sidecars.
            let is_agent = p.extension().and_then(|x| x.to_str()) == Some("jsonl")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("agent-"));
            if is_agent {
                paths.push(p);
            }
        }
    }
    crate::par_filter_map(paths, |p| {
        let session = scan(&p).ok()?;
        let meta = read_meta(&p.with_extension("meta.json"));
        let agent_id = session.id.strip_prefix("agent-").unwrap_or(&session.id).to_string();
        let (status, summary) = wf
            .and_then(|(_, j)| j.get(&agent_id).cloned())
            .unwrap_or((None, None));
        Some(SubagentInfo {
            session,
            agent_type: meta.0,
            description: meta.1,
            tool_use_id: meta.2,
            workflow: wf.map(|(id, _)| id.to_string()),
            result_status: status,
            result_summary: summary,
        })
    })
}

/// Read an `agent-*.meta.json` sidecar → `(agentType, description, toolUseId)`. Any read/parse
/// failure (or an absent file) is a clean `(None, None, None)`.
fn read_meta(path: &std::path::Path) -> (Option<String>, Option<String>, Option<String>) {
    let Ok(txt) = fs::read_to_string(path) else {
        return (None, None, None);
    };
    let Ok(v) = serde_json::from_str::<Value>(&txt) else {
        return (None, None, None);
    };
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    (s("agentType"), s("description"), s("toolUseId"))
}

/// Parse a workflow `journal.jsonl` into a map from `agentId` → `(status, summary)`, taking the
/// last `result` record for each agent (a `result` supersedes the earlier `started`). The journal
/// is the orchestrator's structured-output log — the sub-agent's real return value, which never
/// appears in the parent transcript.
///
/// The `result` payload comes in two shapes across workflows: a **structured object**
/// `{status, summary, …}` (status vocabularies are per-workflow: `done`/`partial`/`GREEN`/
/// `proven`/`blocked`/…), or a **plain string** (the agent's whole freeform return, with no
/// separate status). Both are normalized here.
fn read_workflow_journal(path: &std::path::Path) -> JournalResults {
    let mut map = std::collections::HashMap::new();
    let Ok(file) = fs::File::open(path) else {
        return map;
    };
    super::for_each_json_line(BufReader::new(file), |v| {
        if v.get("type").and_then(Value::as_str) == Some("result") {
            if let Some(agent_id) = v.get("agentId").and_then(Value::as_str) {
                let (status, summary) = match v.get("result") {
                    // Structured form: pull status + summary; if there's no `summary` key fall back
                    // to the object's compact JSON so the outcome isn't lost.
                    Some(obj @ Value::Object(_)) => {
                        let status = obj.get("status").and_then(Value::as_str).map(str::to_string);
                        let summary = obj
                            .get("summary")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .or_else(|| Some(obj.to_string()));
                        (status, summary)
                    }
                    // Plain-string form: the whole string is the summary, no separate status.
                    Some(Value::String(s)) => (None, Some(s.clone())),
                    _ => (None, None),
                };
                map.insert(agent_id.to_string(), (status, summary));
            }
        }
        Flow::Continue
    });
    map
}

/// The sub-agent's final return value as text: the last assistant text turn in its transcript
/// (what a `general-purpose`/`Explore` agent hands back to its parent, surfaced in the parent's
/// `Agent` tool_result). Reads only the small text fields, never materializing large content.
/// `None` for an empty/unreadable transcript or one whose final turn carried no text.
pub fn subagent_return(path: &std::path::Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut last: Option<String> = None;
    super::for_each_json_line(BufReader::new(file), |v| {
        if v.get("type").and_then(Value::as_str) == Some("assistant") {
            if let Some(Value::Array(items)) = v.get("message").and_then(|m| m.get("content")) {
                let mut buf = String::new();
                for it in items {
                    if it.get("type").and_then(Value::as_str) == Some("text") {
                        if let Some(t) = it.get("text").and_then(Value::as_str) {
                            if !buf.is_empty() {
                                buf.push('\n');
                            }
                            buf.push_str(t);
                        }
                    }
                }
                if !buf.trim().is_empty() {
                    last = Some(buf);
                }
            }
        }
        Flow::Continue
    });
    last
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

    super::for_each_json_line(reader, |v| {
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
        Flow::Continue
    });

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

/// Turn one record into an IR [`Message`]. Handles `user`/`assistant` (the conversation) and the
/// content-bearing `system` lines (compaction notices, away/error summaries, slash-command output).
/// Returns `None` for line types with no conversational payload.
fn parse_message(ty: &str, v: &Value, opts: &ParseOptions, span: Option<&SpanCtx>) -> Option<Message> {
    if ty == "system" {
        return parse_system_message(v, opts, span);
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
        Some(Value::String(s)) => {
            // Bare-string content: span it (the whole `message.content` string) when large.
            let text = match span {
                Some(ctx) if s.len() > INLINE_MAX => raw_msg_content_string_span(ctx)
                    .inspect(|sp| debug_assert_span(sp, ctx, s))
                    .map(Text::Span)
                    .unwrap_or_else(|| Text::Inline(s.clone())),
                _ => Text::Inline(s.clone()),
            };
            blocks.push(Block::Text { text });
        }
        Some(Value::Array(items)) => {
            // Raw items are the *same* JSON array as `items`, so they align 1:1 by index — giving
            // each block its source field's byte span for free.
            let raw_items = span.and_then(|ctx| raw_msg_content_items(ctx.slice));
            for (i, item) in items.iter().enumerate() {
                let raw = raw_items.as_ref().and_then(|r| r.get(i).copied());
                if let Some(b) = parse_block(item, raw.zip(span)) {
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
fn parse_system_message(v: &Value, opts: &ParseOptions, span: Option<&SpanCtx>) -> Option<Message> {
    let subtype = v.get("subtype").and_then(Value::as_str).unwrap_or("");
    // Body text lives either at top-level `content` (a string) or, for compaction, is implied.
    let content = v.get("content").and_then(Value::as_str);

    // `stop_hook_summary` carries no `content`, but it IS the record of a Stop-hook firing — its
    // `hookInfos`/`hookErrors` are the actual hook activity (a hook's command + output, or its
    // error). Surface it as a readable line when there's something to show (output or an error), so
    // hooks are visible rather than silently dropped; a no-op hook summary still leaves the stream.
    if subtype == "stop_hook_summary" {
        if let Some(body) = render_hook_summary(v) {
            return system_msg(v, Text::Inline(body), subtype, opts);
        }
        return None;
    }

    // Subtypes with no human-readable body — keep them out of the message stream.
    let bodyless = matches!(subtype, "turn_duration" | "agents_killed");
    if bodyless || (content.is_none() && subtype != "compact_boundary") {
        return None;
    }

    // Slash-command records (`subtype == "local_command"`) wrap their payload in pseudo-XML tags
    // (`<command-name>/foo</command-name>`, `<command-args>`, `<local-command-stdout>`). Rendering
    // the raw tag soup is noise — distill it to a readable one/two-liner, but keep it inline (these
    // are always small, so no span path is needed).
    if subtype == "local_command" {
        if let Some(c) = content {
            if let Some(rendered) = render_slash_command(c) {
                return system_msg(v, Text::Inline(rendered), subtype, opts);
            }
        }
    }

    let text: Text = match content {
        Some(c) => match span {
            Some(ctx) if c.len() > INLINE_MAX => top_content_span(ctx)
                .inspect(|sp| debug_assert_span(sp, ctx, c))
                .map(Text::Span)
                .unwrap_or_else(|| Text::Inline(c.to_string())),
            _ => Text::Inline(c.to_string()),
        },
        // compact_boundary often has only structured metadata; synthesize a marker.
        None => Text::Inline("[conversation compacted]".to_string()),
    };

    system_msg(v, text, subtype, opts)
}

/// Build a [`Role::System`] [`Message`] from a system record `v` with its already-resolved body
/// `text`. The shared tail of [`parse_system_message`] (and the slash-command path), so both keep
/// identical id/parent/timestamp/`extra` handling.
fn system_msg(v: &Value, text: Text, subtype: &str, opts: &ParseOptions) -> Option<Message> {
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
            // Hook activity (Stop/PreToolUse/… hook firings): the command, its output, and errors.
            "hookCount",
            "hookInfos",
            "hookErrors",
            "stopReason",
            "preventedContinuation",
            "toolUseID",
            // Model-refusal fallback (a provider safety refusal that re-routed to another model):
            // which models, the refusal category/explanation, and what got retracted.
            "originalModel",
            "fallbackModel",
            "apiRefusalCategory",
            "apiRefusalExplanation",
            "retractedMessageUuids",
            "trigger",
            "direction",
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

/// Distill a `stop_hook_summary` record into a readable one-liner, or `None` when the hook did
/// nothing worth surfacing (no output and no error). The record has no `content`; its activity lives
/// in `hookInfos[]` (each a fired hook, often with a `command` + captured output) and `hookErrors[]`.
fn render_hook_summary(v: &Value) -> Option<String> {
    let count = v.get("hookCount").and_then(Value::as_u64).unwrap_or(0);
    let errors = v.get("hookErrors").and_then(Value::as_array);
    let has_errors = errors.is_some_and(|e| !e.is_empty());
    let has_output = v.get("hasOutput").and_then(Value::as_bool).unwrap_or(false)
        || v.get("hookAdditionalContext").and_then(Value::as_str).is_some_and(|s| !s.is_empty());
    if !has_errors && !has_output {
        return None;
    }
    let label = if count == 1 { "hook".to_string() } else { format!("{count} hooks") };
    let mut s = format!("⛓ stop {label}");
    if let Some(infos) = v.get("hookInfos").and_then(Value::as_array) {
        // Name each fired hook by the head of its command (the most legible identifier).
        let names: Vec<String> = infos
            .iter()
            .filter_map(|i| i.get("command").and_then(Value::as_str))
            .map(|c| truncate(c.lines().next().unwrap_or(c).trim(), 60))
            .collect();
        if !names.is_empty() {
            s.push_str(&format!(": {}", names.join(" · ")));
        }
    }
    if let Some(ctx) = v.get("hookAdditionalContext").and_then(Value::as_str).filter(|c| !c.is_empty()) {
        s.push_str(&format!("\n  ↳ {}", truncate(ctx.trim(), 200)));
    }
    if let Some(errs) = errors.filter(|e| !e.is_empty()) {
        let joined: Vec<String> = errs
            .iter()
            .map(|e| truncate(e.as_str().unwrap_or(&e.to_string()).trim(), 100))
            .collect();
        s.push_str(&format!("\n  ✗ {}", joined.join("; ")));
    }
    Some(s)
}

/// Distill a slash-command `system`/`local_command` body to readable text. Two shapes occur:
/// the *invocation* (`<command-name>/foo</command-name>` + optional `<command-args>`) renders as
/// `/foo args`; the *output* (`<local-command-stdout>…</local-command-stdout>`) renders as the
/// captured stdout. Returns `None` when no recognizable tag is present (caller keeps the raw body).
fn render_slash_command(c: &str) -> Option<String> {
    if let Some(name) = tag_inner(c, "command-name") {
        let name = name.trim();
        let args = tag_inner(c, "command-args").map(|a| a.trim().to_string());
        return Some(match args.as_deref().filter(|a| !a.is_empty()) {
            Some(a) => format!("⌘ {name} {a}"),
            None => format!("⌘ {name}"),
        });
    }
    if let Some(out) = tag_inner(c, "local-command-stdout") {
        let out = out.trim();
        return Some(if out.is_empty() {
            "⌘ (no output)".to_string()
        } else {
            format!("⌘ stdout:\n{out}")
        });
    }
    None
}

/// Extract the inner text of the first `<tag>…</tag>` pair in `s`, if present.
fn tag_inner<'a>(s: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = s.find(&open)? + open.len();
    let end = s[start..].find(&close)? + start;
    Some(&s[start..end])
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

fn parse_block(item: &Value, span_field: Option<(&RawValue, &SpanCtx)>) -> Option<Block> {
    match item.get("type").and_then(Value::as_str)? {
        "text" => Some(Block::Text {
            text: spanned_or_inline(item.get("text").and_then(Value::as_str)?, "text", span_field),
        }),
        "thinking" => Some(Block::Thinking {
            text: spanned_or_inline(
                item.get("thinking").and_then(Value::as_str).unwrap_or(""),
                "thinking",
                span_field,
            ),
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
            text: String::new().into(),
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
        "tool_result" => {
            let cv = item.get("content");
            let coerced = coerce_text(cv);
            // Span only when the raw content is a *plain JSON string* (then the span body unescapes
            // back to exactly `coerced`); array/mixed content is transformed by `coerce_text`, so it
            // can't be a verbatim slice — keep it inline.
            let content = match span_field {
                Some((raw, ctx))
                    if coerced.len() > INLINE_MAX && matches!(cv, Some(Value::String(_))) =>
                {
                    span_of_string_field(raw, "content", ctx)
                        .inspect(|sp| debug_assert_span(sp, ctx, &coerced))
                        .map(Text::Span)
                        .unwrap_or_else(|| Text::Inline(coerced))
                }
                _ => Text::Inline(coerced),
            };
            Some(Block::ToolResult {
                tool_use_id: item
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                content,
                is_error: item.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                tool_name: None,
                status: None,
                details: None,
            })
        }
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

/// Make a [`Text`] for a content string: a lazy [`Span`] into the source when large and a span is
/// computable, else inline. The `debug_assert` re-resolves the span from the same bytes and checks it
/// equals `s`, so any offset/escape bug trips immediately in dev/test.
fn spanned_or_inline(s: &str, field: &str, span_field: Option<(&RawValue, &SpanCtx)>) -> Text {
    if s.len() > INLINE_MAX {
        if let Some((raw, ctx)) = span_field {
            if let Some(sp) = span_of_string_field(raw, field, ctx) {
                debug_assert_span(&sp, ctx, s);
                return Text::Span(sp);
            }
        }
    }
    Text::Inline(s.to_string())
}

/// The byte span of a string `field` inside a content item's raw JSON (`item_raw`), as an absolute
/// file offset. `None` if the field is absent or isn't a JSON string.
fn span_of_string_field(item_raw: &RawValue, field: &str, ctx: &SpanCtx) -> Option<Span> {
    let map: std::collections::HashMap<&str, &RawValue> =
        serde_json::from_str(item_raw.get()).ok()?;
    raw_string_span(map.get(field)?, ctx)
}

/// Span the *body* of a raw JSON string value as an absolute file offset (delegates to the shared
/// [`crate::lazy::json_string_span`]; relies on `fr` borrowing `ctx.slice`).
fn raw_string_span(fr: &RawValue, ctx: &SpanCtx) -> Option<Span> {
    crate::lazy::json_string_span(fr, ctx.slice, ctx.base_off)
}

/// The raw JSON of `message.content`, borrowing `slice`.
fn message_content_raw(slice: &[u8]) -> Option<&RawValue> {
    #[derive(serde::Deserialize)]
    struct P<'a> {
        #[serde(borrow)]
        message: Option<Pm<'a>>,
    }
    #[derive(serde::Deserialize)]
    struct Pm<'a> {
        #[serde(borrow, default)]
        content: Option<&'a RawValue>,
    }
    serde_json::from_slice::<P>(slice)
        .ok()
        .and_then(|p| p.message)
        .and_then(|m| m.content)
}

/// The content array items as raw JSON (aligned 1:1 with the `Value` array), or `None` if content
/// isn't an array.
fn raw_msg_content_items(slice: &[u8]) -> Option<Vec<&RawValue>> {
    let content = message_content_raw(slice)?;
    if content.get().as_bytes().first() == Some(&b'[') {
        serde_json::from_str::<Vec<&RawValue>>(content.get()).ok()
    } else {
        None
    }
}

/// Span for a bare-string `message.content` (the whole string is the content).
fn raw_msg_content_string_span(ctx: &SpanCtx) -> Option<Span> {
    let content = message_content_raw(ctx.slice)?;
    if content.get().as_bytes().first() == Some(&b'"') {
        raw_string_span(content, ctx)
    } else {
        None
    }
}

/// Span for a top-level `content` string (system records).
fn top_content_span(ctx: &SpanCtx) -> Option<Span> {
    #[derive(serde::Deserialize)]
    struct T<'a> {
        #[serde(borrow, default)]
        content: Option<&'a RawValue>,
    }
    let content = serde_json::from_slice::<T>(ctx.slice).ok()?.content?;
    if content.get().as_bytes().first() == Some(&b'"') {
        raw_string_span(content, ctx)
    } else {
        None
    }
}

/// Dev/test guard: resolve `sp` from the same record bytes and assert it equals the inline text the
/// span replaces. Compiled out in release.
#[cfg(debug_assertions)]
fn debug_assert_span(sp: &Span, ctx: &SpanCtx, expected: &str) {
    let lo = match sp.offset.checked_sub(ctx.base_off) {
        Some(x) => x as usize,
        None => {
            debug_assert!(false, "span offset before base");
            return;
        }
    };
    let hi = lo + sp.len as usize;
    debug_assert!(hi <= ctx.slice.len(), "span exceeds record slice");
    let raw = &ctx.slice[lo..hi.min(ctx.slice.len())];
    let got = if sp.escaped {
        let mut q = Vec::with_capacity(raw.len() + 2);
        q.push(b'"');
        q.extend_from_slice(raw);
        q.push(b'"');
        serde_json::from_slice::<String>(&q).unwrap_or_default()
    } else {
        String::from_utf8_lossy(raw).into_owned()
    };
    debug_assert_eq!(got, expected, "lazy span resolved to the wrong content");
}
#[cfg(not(debug_assertions))]
fn debug_assert_span(_: &Span, _: &SpanCtx, _: &str) {}

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
    fn stop_hook_summary_with_output_surfaces_and_keeps_hook_metadata() {
        // A Stop-hook that fired with output (and a sibling that did nothing): the first surfaces
        // as a readable ⛓ line carrying its hook command + context; the no-op one stays dropped.
        let text = [
            // Hook with output + a command name → surfaced.
            r#"{"type":"system","subtype":"stop_hook_summary","uuid":"h1","timestamp":"2026-06-11T00:00:00Z","hookCount":1,"hasOutput":true,"hookAdditionalContext":"keep working all night","hookInfos":[{"command":"alright well i'm going to bed. so.... best of luck"}],"hookErrors":[],"stopReason":"hook","toolUseID":"tu1"}"#,
            // No output, no error → dropped from the stream (a no-op hook summary).
            r#"{"type":"system","subtype":"stop_hook_summary","uuid":"h2","timestamp":"2026-06-11T00:01:00Z","hookCount":1,"hasOutput":false,"hookInfos":[{"command":"noop"}],"hookErrors":[]}"#,
            // A model_refusal_fallback: has content AND rich metadata that must ride in extra.
            r#"{"type":"system","subtype":"model_refusal_fallback","uuid":"r1","timestamp":"2026-06-11T00:02:00Z","content":"Fable 5's safety measures flagged this message.","originalModel":"claude-fable-5[1m]","fallbackModel":"claude-opus-4-8","trigger":"refusal"}"#,
        ]
        .join("\n");
        let s = parse_str("hooks", &text, None);
        let systems: Vec<_> = s.messages.iter().filter(|m| m.role == Role::System).collect();
        // The hook-with-output and the refusal surface; the no-op hook does not.
        assert_eq!(systems.len(), 2, "got {:?}", systems.iter().map(|m| m.text()).collect::<Vec<_>>());

        let hook = systems.iter().find(|m| m.extra.get("subtype").and_then(Value::as_str) == Some("stop_hook_summary")).unwrap();
        let body = hook.text().unwrap();
        assert!(body.starts_with("⛓ stop hook"), "hook body: {body:?}");
        assert!(body.contains("best of luck"), "hook command surfaced: {body:?}");
        assert!(body.contains("keep working all night"), "hook context surfaced: {body:?}");
        // Hook metadata preserved in extra.
        assert_eq!(hook.extra.get("hookCount").unwrap(), 1);
        assert!(hook.extra.get("hookInfos").is_some());
        assert_eq!(hook.extra.get("stopReason").unwrap(), "hook");

        // Refusal-fallback metadata preserved.
        let refusal = systems.iter().find(|m| m.extra.get("subtype").and_then(Value::as_str) == Some("model_refusal_fallback")).unwrap();
        assert_eq!(refusal.extra.get("originalModel").unwrap(), "claude-fable-5[1m]");
        assert_eq!(refusal.extra.get("fallbackModel").unwrap(), "claude-opus-4-8");
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
    fn slash_command_records_render_readably() {
        // The invocation record (`<command-name>` + `<command-args>`) and the output record
        // (`<local-command-stdout>`) both distill to clean text, not raw pseudo-XML.
        let text = [
            r#"{"type":"system","subtype":"local_command","uuid":"c1","timestamp":"2026-06-01T00:00:00Z","content":"<command-name>/usage</command-name>\n  <command-message>usage</command-message>\n  <command-args>--verbose</command-args>"}"#,
            r#"{"type":"system","subtype":"local_command","uuid":"c2","timestamp":"2026-06-01T00:00:01Z","content":"<local-command-stdout>Settings dialog dismissed</local-command-stdout>"}"#,
        ]
        .join("\n");
        let s = parse_str("slash", &text, None);
        let bodies: Vec<String> = s.messages.iter().filter_map(|m| m.text()).collect();
        assert!(bodies.iter().any(|b| b == "⌘ /usage --verbose"), "got {bodies:?}");
        assert!(bodies.iter().any(|b| b.starts_with("⌘ stdout:") && b.contains("Settings dialog dismissed")), "got {bodies:?}");
        // The raw tag text must NOT survive into the rendered body.
        assert!(!bodies.iter().any(|b| b.contains("<command-name>")), "raw tag leaked: {bodies:?}");
    }

    #[test]
    fn workflow_journal_handles_object_and_string_results() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("cv-wfj-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let jpath = dir.join("journal.jsonl");
        let mut f = std::fs::File::create(&jpath).unwrap();
        // Object form (status + summary), a started-only agent (no result), and a string form.
        writeln!(f, r#"{{"type":"started","key":"k1","agentId":"aaa"}}"#).unwrap();
        writeln!(f, r#"{{"type":"result","key":"k1","agentId":"aaa","result":{{"status":"done","summary":"closed the lane"}}}}"#).unwrap();
        writeln!(f, r#"{{"type":"started","key":"k2","agentId":"bbb"}}"#).unwrap();
        writeln!(f, r#"{{"type":"result","key":"k3","agentId":"ccc","result":"freeform return text"}}"#).unwrap();
        drop(f);

        let map = read_workflow_journal(&jpath);
        assert_eq!(map.get("aaa"), Some(&(Some("done".to_string()), Some("closed the lane".to_string()))));
        // started-only agent has no journaled result.
        assert!(!map.contains_key("bbb"));
        // string result becomes the summary, with no status.
        assert_eq!(map.get("ccc"), Some(&(None, Some("freeform return text".to_string()))));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn subagent_tree_discovers_both_tiers_with_meta_and_journal() {
        use std::io::Write;
        // Lay out a parent session with one directly-spawned sub-agent and one workflow holding
        // two agents (one journaled done, one a string return), mirroring the real on-disk shape.
        let root = std::env::temp_dir().join(format!("cv-subtree-{}", uuid::Uuid::new_v4()));
        let proj = root.join("projects").join("-enc");
        std::fs::create_dir_all(&proj).unwrap();
        let parent = proj.join("sess.jsonl");
        std::fs::write(&parent, "{\"type\":\"user\",\"uuid\":\"u\",\"message\":{\"role\":\"user\",\"content\":\"go\"}}\n").unwrap();

        let subs = proj.join("sess").join("subagents");
        std::fs::create_dir_all(&subs).unwrap();
        // Tier 1: a direct Agent sub-agent + its meta sidecar.
        let a1 = subs.join("agent-a111.jsonl");
        std::fs::write(&a1, "{\"type\":\"user\",\"uuid\":\"x\",\"message\":{\"role\":\"user\",\"content\":\"task one\"}}\n{\"type\":\"assistant\",\"uuid\":\"y\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"done one\"}]}}\n").unwrap();
        std::fs::write(subs.join("agent-a111.meta.json"), r#"{"agentType":"general-purpose","description":"do task one","toolUseId":"toolu_77"}"#).unwrap();

        // Tier 2: a workflow dir with two agents and a journal.
        let wf = subs.join("workflows").join("wf_abc-123");
        std::fs::create_dir_all(&wf).unwrap();
        std::fs::write(wf.join("agent-a222.jsonl"), "{\"type\":\"user\",\"uuid\":\"p\",\"message\":{\"role\":\"user\",\"content\":\"wtask\"}}\n").unwrap();
        std::fs::write(wf.join("agent-a222.meta.json"), r#"{"agentType":"workflow-subagent","description":"wf task"}"#).unwrap();
        std::fs::write(wf.join("agent-a333.jsonl"), "{\"type\":\"user\",\"uuid\":\"q\",\"message\":{\"role\":\"user\",\"content\":\"wtask2\"}}\n").unwrap();
        std::fs::write(wf.join("agent-a333.meta.json"), r#"{"agentType":"workflow-subagent"}"#).unwrap();
        let mut jf = std::fs::File::create(wf.join("journal.jsonl")).unwrap();
        writeln!(jf, r#"{{"type":"result","agentId":"a222","result":{{"status":"GREEN","summary":"wf agent did it"}}}}"#).unwrap();
        writeln!(jf, r#"{{"type":"result","agentId":"a333","result":"string return"}}"#).unwrap();
        drop(jf);

        let tree = subagent_tree(&parent);
        assert_eq!(tree.len(), 3, "two tiers: 1 direct + 2 workflow");

        let direct = tree.iter().find(|s| s.agent_id() == "a111").expect("direct agent");
        assert_eq!(direct.agent_type.as_deref(), Some("general-purpose"));
        assert_eq!(direct.description.as_deref(), Some("do task one"));
        assert_eq!(direct.tool_use_id.as_deref(), Some("toolu_77"));
        assert!(direct.workflow.is_none());
        assert_eq!(direct.result_status, None); // direct agents report via parent tool_result
        // its return value = the last assistant text turn.
        assert_eq!(subagent_return(&direct.session.path).as_deref(), Some("done one"));

        let w1 = tree.iter().find(|s| s.agent_id() == "a222").expect("wf agent 1");
        assert_eq!(w1.workflow.as_deref(), Some("wf_abc-123"));
        assert_eq!(w1.result_status.as_deref(), Some("GREEN"));
        assert_eq!(w1.result_summary.as_deref(), Some("wf agent did it"));

        let w2 = tree.iter().find(|s| s.agent_id() == "a333").expect("wf agent 2");
        assert_eq!(w2.result_status, None);
        assert_eq!(w2.result_summary.as_deref(), Some("string return"));

        std::fs::remove_dir_all(&root).ok();
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
    fn lazy_spans_resolve_to_same_text() {
        // A record with content well over INLINE_MAX: the lazy path must produce a Span whose
        // resolved text equals the inline parse, byte-for-byte (incl. escapes).
        use std::io::Write;
        let big = "x\n\"quoted\"\t".repeat(1000); // ~10 KB with escapes (\n, \", \t)
        let line = serde_json::json!({
            "type": "assistant",
            "uuid": "u1",
            "message": { "role": "assistant", "content": [{"type": "text", "text": big}] }
        })
        .to_string();
        let dir = std::env::temp_dir().join(format!("cv-lazy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{line}").unwrap();
        drop(f);

        let r = SessionRef {
            id: "s".into(),
            harness: Harness::Claude,
            path: path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 1,
        };
        // Lazy parse → expect a Span; resolving it must equal `big`.
        let mut sink = CollectSink::default();
        let file = BufReader::new(fs::File::open(&path).unwrap());
        let mut s = stream_reader_lazy(&r.id, file, Some(path.clone()), &mut sink);
        s.messages = sink.messages;
        match &s.messages[0].content[0] {
            Block::Text { text } => {
                assert!(text.is_span(), "large content should be a span");
                let resolver = s.resolver();
                assert_eq!(text.resolve(&resolver), big.as_str());
            }
            _ => panic!("expected text block"),
        }
        // And materialize() makes it inline and equal.
        s.materialize();
        match &s.messages[0].content[0] {
            Block::Text { text } => {
                assert!(!text.is_span());
                assert_eq!(text.inline_str(), Some(big.as_str()));
            }
            _ => panic!("expected text block"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    // Test helper: stream a file with span production enabled.
    fn stream_reader_lazy<R: BufRead>(
        id: &str,
        reader: R,
        source_path: Option<PathBuf>,
        sink: &mut dyn MessageSink,
    ) -> Session {
        stream_reader(id, reader, source_path, &ParseOptions::lazy(), sink)
    }

    /// The seek cooperation (REARCH Phase 2): under `lazy_offsets()` every message is stamped
    /// with its record's byte offset, and `stream_reader_from` replayed at any stamped offset
    /// reproduces exactly the suffix of the full stream — message-for-message, span-for-span.
    #[test]
    fn offset_stamps_replay_byte_identically_from_any_message() {
        use std::io::{Seek, SeekFrom, Write};
        let big = "wide \"payload\"\n".repeat(600); // > INLINE_MAX, with escapes → a Span
        let lines = [
            serde_json::json!({"type":"ai-title","aiTitle":"seek test"}),
            serde_json::json!({"type":"user","uuid":"u0","cwd":"/w","gitBranch":"main",
                "message":{"role":"user","content":"first question"}}),
            serde_json::json!({"type":"assistant","uuid":"a1","timestamp":"2026-06-01T00:00:01Z",
                "message":{"role":"assistant","model":"claude-test-1","content":[
                    {"type":"text","text":"answer one"},
                    {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}),
            serde_json::json!({"type":"user","uuid":"u2","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t1","content":big,"is_error":false}]}}),
            serde_json::json!({"type":"system","subtype":"away_summary","uuid":"s3",
                "content":"came back"}),
            serde_json::json!({"type":"user","uuid":"u4","message":{"role":"user","content":"second question"}}),
            serde_json::json!({"type":"assistant","uuid":"a5","message":{"role":"assistant",
                "model":"claude-test-1","content":[{"type":"text","text":"answer two"}]}}),
        ];
        let dir = std::env::temp_dir().join(format!("cv-claude-seek-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        for l in &lines {
            writeln!(f, "{l}").unwrap();
        }
        drop(f);

        let opts = ParseOptions::lazy_offsets();
        let mut full = CollectSink::default();
        stream_reader(
            "s",
            BufReader::new(fs::File::open(&path).unwrap()),
            Some(path.clone()),
            &opts,
            &mut full,
        );
        let full = full.messages;
        assert_eq!(full.len(), 6); // user, assistant, tool, system, user, assistant
        let offs: Vec<u64> = full
            .iter()
            .map(|m| {
                m.extra
                    .get(crate::offsets::OFFSET_KEY)
                    .and_then(Value::as_u64)
                    .expect("every message stamped under lazy_offsets")
            })
            .collect();
        assert!(offs.windows(2).all(|w| w[0] < w[1]), "offsets follow file order");

        for k in 1..full.len() {
            let mut file = fs::File::open(&path).unwrap();
            file.seek(SeekFrom::Start(offs[k])).unwrap();
            let mut replay = CollectSink::default();
            stream_reader_from(
                "s",
                BufReader::new(file),
                Some(path.clone()),
                offs[k],
                &opts,
                &mut replay,
            );
            assert_eq!(
                serde_json::to_value(&full[k..]).unwrap(),
                serde_json::to_value(&replay.messages).unwrap(),
                "replay from message {k}'s offset must equal the full-stream suffix"
            );
        }

        // And bulk/full options never stamp (their output stays byte-identical to before).
        let mut bulk = CollectSink::default();
        stream_reader(
            "s",
            BufReader::new(fs::File::open(&path).unwrap()),
            Some(path.clone()),
            &ParseOptions::bulk(),
            &mut bulk,
        );
        assert!(bulk
            .messages
            .iter()
            .all(|m| !m.extra.contains_key(crate::offsets::OFFSET_KEY)));

        std::fs::remove_dir_all(&dir).ok();
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
        // Both delegate to `ingest_value`; this guards the OOM-fix refactor against divergence.
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
