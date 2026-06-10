//! `cv show` / `cv export` / `cv tree` / `cv diff` / `cv redact` — reading sessions,
//! plus the streaming renderer (`stream_session_render`) and its header/message helpers.

use crate::util::{home_rel, parse_harness, parse_msg_range, short_id};
use anyhow::{bail, Context, Result};
use cv_core::ir::{truncate, Block, Harness, Message, Role, Session, SessionRef};
use cv_core::Adapter;
use std::path::PathBuf;

pub(crate) fn cmd_show(id: &str, harness: Option<String>, json: bool, range: Option<String>) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, adapter) = cv_core::find(id, want)?.with_context(|| format!("no session matching {id:?}"))?;
    let range = range.as_deref().map(parse_msg_range).transpose()?;

    // JSON wants the whole IR (incl. `extra`), so it materializes; the rendered transcript streams.
    if json {
        let mut session = adapter.parse(&r)?;
        if let Some((start, end)) = range {
            let end = end.unwrap_or(session.messages.len()).min(session.messages.len());
            let start = start.min(end);
            session.messages = session.messages.drain(start..end).collect();
        }
        println!("{}", serde_json::to_string_pretty(&session)?);
        return Ok(());
    }

    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    stream_session_render(adapter.as_ref(), &r, &mut out, show_header, show_message, range)?;
    use std::io::Write;
    out.flush()?;
    Ok(())
}

pub(crate) fn cmd_export(id: &str, format: &str, harness: Option<String>) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, adapter) = cv_core::find(id, want)?.with_context(|| format!("no session matching {id:?}"))?;
    match format {
        "md" | "markdown" => {
            let mut out = std::io::BufWriter::new(std::io::stdout().lock());
            stream_session_render(adapter.as_ref(), &r, &mut out, md_header, md_message, None)?;
            use std::io::Write;
            out.flush()?;
        }
        // JSON and HTML need the whole session at once (full IR / a single self-contained document).
        "json" => {
            let session = adapter.parse(&r)?;
            println!("{}", serde_json::to_string_pretty(&session)?);
        }
        "html" => {
            let session = adapter.parse(&r)?;
            print!("{}", cv_core::html::to_html(&session));
        }
        other => bail!("unknown format {other:?} (use md, json, or html)"),
    }
    Ok(())
}

pub(crate) fn cmd_redact(id: &str, harness: Option<String>, format: &str, stats: bool) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, adapter) =
        cv_core::find(id, want)?.with_context(|| format!("no session matching {id:?}"))?;
    let session = adapter.parse(&r)?;

    let (redacted, st) = cv_core::redact::redact_with(&session, &Default::default());

    match format {
        "json" => println!("{}", serde_json::to_string_pretty(&redacted)?),
        "md" | "markdown" => print!("{}", cv_core::render::to_markdown(&redacted)),
        other => bail!("unknown format {other:?} (use md or json)"),
    }

    if stats {
        eprintln!(
            "✦ redacted {} item(s): {} api_key, {} private_key, {} jwt, {} email, {} blob, {} assignment",
            st.total(),
            st.api_keys,
            st.private_keys,
            st.jwts,
            st.emails,
            st.blobs,
            st.assignments,
        );
    }
    Ok(())
}

// ---------- tree ----------

pub(crate) fn cmd_tree(id: &str, harness: Option<String>) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, adapter) =
        cv_core::find(id, want)?.with_context(|| format!("no session matching {id:?}"))?;
    let session = adapter.parse(&r)?;

    println!("# {}", session.label());
    println!(
        "{} · {} · {} msg",
        session.harness,
        session.id,
        session.messages.len()
    );
    println!();

    // Threaded view only if at least one message carries a parent_id.
    let has_threading = session.messages.iter().any(|m| m.parent_id.is_some());
    if has_threading {
        render_tree_dag(&session);
    } else {
        for (i, m) in session.messages.iter().enumerate() {
            println!("{:>4}. {}", i + 1, tree_line(m));
        }
    }
    Ok(())
}

/// Render messages as an indented DAG by `parent_id`. Roots (no/unknown parent) sit at depth 0.
fn render_tree_dag(session: &Session) {
    use std::collections::HashMap;
    // children: parent_id -> ordered list of child message indices.
    let mut by_id: HashMap<&str, usize> = HashMap::new();
    for (i, m) in session.messages.iter().enumerate() {
        if let Some(id) = &m.id {
            by_id.insert(id.as_str(), i);
        }
    }
    let mut children: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
    for (i, m) in session.messages.iter().enumerate() {
        let parent = m
            .parent_id
            .as_deref()
            .and_then(|p| by_id.get(p).copied())
            .filter(|&p| p != i);
        children.entry(parent).or_default().push(i);
    }

    fn walk(
        node: Option<usize>,
        depth: usize,
        session: &Session,
        children: &std::collections::HashMap<Option<usize>, Vec<usize>>,
    ) {
        if let Some(kids) = children.get(&node) {
            for &c in kids {
                let indent = "  ".repeat(depth);
                println!("{indent}• {}", tree_line(&session.messages[c]));
                walk(Some(c), depth + 1, session, children);
            }
        }
    }
    walk(None, 0, session, &children);
}

/// One-line preview of a message for the tree: role, markers for tool turns / sub-agent spawns,
/// and a text preview.
fn tree_line(m: &Message) -> String {
    let role = cv_core::render::role_label(m.role);
    let mut tags = Vec::new();
    let has_tool_use = m
        .content
        .iter()
        .any(|b| matches!(b, Block::ToolUse { .. }));
    let has_tool_result = m
        .content
        .iter()
        .any(|b| matches!(b, Block::ToolResult { .. }));
    if has_tool_use {
        tags.push("🔧 tool".to_string());
        // Surface a sub-agent spawn if the tool looks like one.
        for b in &m.content {
            if let Block::ToolUse { name, .. } = b {
                let n = name.to_ascii_lowercase();
                if n.contains("task") || n.contains("agent") || n.contains("dispatch") || n.contains("spawn") {
                    tags.push(format!("↳ sub-agent ({name})"));
                }
            }
        }
    }
    if has_tool_result {
        tags.push("↩ result".to_string());
    }
    // Sub-agent spawns recorded in `extra` (harness-specific).
    if let Some(sub) = sub_agent_from_extra(m) {
        tags.push(format!("↳ sub-agent ({sub})"));
    }

    let preview = m
        .text()
        .map(|t| truncate(&t, 80))
        .unwrap_or_else(|| {
            if has_tool_use {
                m.content
                    .iter()
                    .find_map(|b| match b {
                        Block::ToolUse { name, .. } => Some(format!("[{name}]")),
                        _ => None,
                    })
                    .unwrap_or_default()
            } else {
                String::new()
            }
        });

    let tagstr = if tags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", tags.join(", "))
    };
    format!("{role}{tagstr}  {preview}")
}

/// Look for a sub-agent spawn recorded in `Message::extra` under common harness keys.
fn sub_agent_from_extra(m: &Message) -> Option<String> {
    for key in ["subagent", "sub_agent", "subAgent", "spawn", "agent", "child_agent"] {
        if let Some(v) = m.extra.get(key) {
            return Some(match v {
                serde_json::Value::String(s) => s.clone(),
                other => truncate(&other.to_string(), 40),
            });
        }
    }
    None
}

// ---------- diff ----------

/// Compare two sessions message-by-message: a shared prefix (`=`) then a divergence marked
/// `<` (only in A) / `>` (only in B). Comparison is on role + `Message::text()`.
pub(crate) fn cmd_diff(a: &str, b: &str, harness: Option<String>) -> Result<()> {
    let default = parse_harness(&harness)?;
    // Each side may carry its own `harness:id` prefix so the two sessions can live in *different*
    // harnesses — applying one `--harness` to both made cross-harness diffs impossible. A side with
    // no recognized prefix falls back to the shared `--harness` (or unconstrained search).
    let (wa, ida) = split_side_harness(a, default);
    let (wb, idb) = split_side_harness(b, default);
    let (ra, aa) = cv_core::find(ida, wa)?.with_context(|| format!("no session matching {a:?}"))?;
    let (rb, ab) = cv_core::find(idb, wb)?.with_context(|| format!("no session matching {b:?}"))?;
    let sa = aa.parse(&ra)?;
    let sb = ab.parse(&rb)?;

    println!("A {:8} {:8}  {} msg", sa.harness.as_str(), short_id(&sa.id), sa.messages.len());
    println!("B {:8} {:8}  {} msg", sb.harness.as_str(), short_id(&sb.id), sb.messages.len());
    println!();

    let key = |m: &Message| (m.role, m.text().unwrap_or_default());
    let na = sa.messages.len();
    let nb = sb.messages.len();

    // Shared prefix: matching role+text from the top.
    let mut shared = 0;
    while shared < na && shared < nb && key(&sa.messages[shared]) == key(&sb.messages[shared]) {
        println!("= {}", diff_line(&sa.messages[shared]));
        shared += 1;
    }
    // After divergence, list A's remainder then B's remainder.
    for m in &sa.messages[shared..] {
        println!("< {}", diff_line(m));
    }
    for m in &sb.messages[shared..] {
        println!("> {}", diff_line(m));
    }

    println!(
        "\n{shared} shared, {} only-in-A, {} only-in-B",
        na - shared,
        nb - shared
    );
    Ok(())
}

/// Split an optional `<harness>:<id>` prefix off a diff side. Only treats the part before the first
/// `:` as a harness when it actually names one; otherwise the whole string is the id and `fallback`
/// (the shared `--harness`) applies.
fn split_side_harness(spec: &str, fallback: Option<Harness>) -> (Option<Harness>, &str) {
    if let Some((head, rest)) = spec.split_once(':') {
        if let Some(h) = Harness::parse(head) {
            return (Some(h), rest);
        }
    }
    (fallback, spec)
}

/// One-line `role: text-preview` for a diff row.
fn diff_line(m: &Message) -> String {
    let role = cv_core::render::role_label(m.role);
    let text = m.text().unwrap_or_default();
    format!("{role:9} {}", truncate(&text, 80))
}

// ---------- rendering helpers ----------

/// Everything a streamed renderer needs for a session's header — derived from the cheap
/// [`SessionRef`] (label/cwd) plus the model captured from the first assistant turn.
pub(crate) struct HeaderInfo {
    harness: Harness,
    id: String,
    cwd: Option<PathBuf>,
    label: String,
    model: Option<String>,
}

/// Render a session to `out` by **streaming** — each message is rendered and written as it arrives,
/// then dropped, so a multi-GB transcript renders at O(largest message) instead of materializing the
/// whole `Session`. The header needs the label and model, which come from the first user/assistant
/// turns, so it's held behind a small bounded buffer (`HOLDBACK` messages) until those are known and
/// then flushed ahead of the body — header info lives in the first turns, so the buffer stays tiny.
pub(crate) fn stream_session_render<W: std::io::Write>(
    adapter: &dyn Adapter,
    r: &SessionRef,
    out: &mut W,
    header: impl Fn(&HeaderInfo) -> String,
    render_msg: impl Fn(&Message) -> String,
    range: Option<(usize, Option<usize>)>,
) -> Result<()> {
    use cv_core::{Flow, MessageSink, ParseOptions};
    const HOLDBACK: usize = 24;

    struct Sink<'w, W, H, R> {
        out: &'w mut W,
        harness: Harness,
        id: String,
        cwd: Option<PathBuf>,
        title: Option<String>,
        model: Option<String>,
        first_user: Option<String>,
        header: H,
        render_msg: R,
        buf: Vec<String>,
        printed: bool,
        result: std::io::Result<()>,
        resolver: cv_core::Resolver,
        // Windowed view: 0-based message index we're at, plus the [start, end) bounds. Messages
        // outside the window are never materialized — their on-disk content is never touched.
        idx: usize,
        start: usize,
        end: Option<usize>,
    }
    impl<W: std::io::Write, H: Fn(&HeaderInfo) -> String, R: Fn(&Message) -> String> Sink<'_, W, H, R> {
        fn write(&mut self, s: &str) {
            if self.result.is_ok() {
                self.result = self.out.write_all(s.as_bytes());
            }
        }
        fn flush_header(&mut self) {
            if self.printed {
                return;
            }
            let info = HeaderInfo {
                harness: self.harness,
                id: self.id.clone(),
                cwd: self.cwd.clone(),
                label: cv_core::label_from(self.title.as_deref(), self.first_user.as_deref()),
                model: self.model.clone(),
            };
            let head = (self.header)(&info);
            self.write(&head);
            let buffered = std::mem::take(&mut self.buf);
            for s in buffered {
                self.write(&s);
            }
            self.printed = true;
        }
    }
    impl<W: std::io::Write, H: Fn(&HeaderInfo) -> String, R: Fn(&Message) -> String> MessageSink
        for Sink<'_, W, H, R>
    {
        fn meta(&mut self, s: &Session) {
            // Authoritative *parsed* session metadata, delivered before the body by the bridge
            // `stream` and by adapters that call `sink.meta` (e.g. codex). The parsed title overrides
            // the discovery-time `SessionRef` title so the header matches a full parse — some
            // adapters' discovery title differs from the parsed one (codex's discovery title is the
            // first user record, which the parse skips). Model/cwd fill in if discovery lacked them.
            self.title = s.title.clone();
            if self.model.is_none() {
                self.model = s.model.clone();
            }
            if self.cwd.is_none() {
                self.cwd = s.cwd.clone();
            }
        }
        fn message(&mut self, m: Message) -> Flow {
            if self.result.is_err() {
                return Flow::Stop;
            }
            let idx = self.idx;
            self.idx += 1;

            // Past the window's end: nothing left to render — stop early so we never read the
            // rest of the file (the whole point of a windowed show on a huge session).
            if let Some(end) = self.end {
                if idx >= end {
                    return Flow::Stop;
                }
            }

            // Model is a small already-parsed field — pick it up even from out-of-window messages
            // so the header stays accurate, without touching any content bytes.
            if self.model.is_none() {
                if let Some(md) = &m.model {
                    self.model = Some(md.clone());
                }
            }

            // Before the window: skip entirely. We do NOT materialize, so no span content is read.
            if idx < self.start {
                return Flow::Continue;
            }

            // In-window: resolve this message's lazy content spans (peak = one message) and render.
            let mut m = m;
            m.materialize(&self.resolver);
            if self.first_user.is_none() && m.role == Role::User {
                if let Some(t) = m.text() {
                    if !t.trim().is_empty() {
                        self.first_user = Some(t);
                    }
                }
            }
            let rendered = (self.render_msg)(&m);
            drop(m);
            if self.printed {
                self.write(&rendered);
            } else {
                self.buf.push(rendered);
                let title_known = self.title.is_some() || self.first_user.is_some();
                if (self.model.is_some() && title_known) || self.buf.len() >= HOLDBACK {
                    self.flush_header();
                }
            }
            if self.result.is_err() {
                Flow::Stop
            } else {
                Flow::Continue
            }
        }
    }

    let mut sink = Sink {
        out,
        harness: r.harness,
        id: r.id.clone(),
        cwd: r.cwd.clone(),
        title: r.title.clone(),
        model: None,
        first_user: None,
        header,
        render_msg,
        buf: Vec::new(),
        printed: false,
        result: Ok(()),
        resolver: cv_core::Resolver::new(Some(r.path.clone())),
        idx: 0,
        start: range.map(|(s, _)| s).unwrap_or(0),
        end: range.and_then(|(_, e)| e),
    };
    // Windowed: lazy spans, so out-of-window giant fields arrive as 16-byte handles and only
    // in-window messages materialize (the sink already resolves per message). A full show reads
    // every byte regardless — spans would only add resolve overhead there (floor = C).
    let opts = if range.is_some() {
        ParseOptions::lazy()
    } else {
        ParseOptions::bulk()
    };
    adapter.stream(r, &opts, &mut sink)?;
    sink.flush_header(); // short sessions (no assistant turn / < HOLDBACK msgs) flush here
    sink.result?;
    Ok(())
}

/// Header for `cv show` (mirrors the old eager header exactly).
pub(crate) fn show_header(h: &HeaderInfo) -> String {
    format!(
        "# {}\n{} · {} · {}{}\n\n",
        h.label,
        h.harness,
        h.id,
        h.cwd.as_deref().map(home_rel).unwrap_or_else(|| "?".into()),
        h.model.as_ref().map(|m| format!(" · {m}")).unwrap_or_default(),
    )
}

/// One rendered `cv show` message block (the String form of the old `print_message`).
pub(crate) fn show_message(m: &Message) -> String {
    let tag = match m.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let mut s = format!("── {tag} ──\n");
    for b in &m.content {
        match b {
            Block::Text { text } => {
                s.push_str(text);
                s.push('\n');
            }
            Block::Thinking { text, .. } => s.push_str(&format!("[thinking] {}\n", truncate(text, 200))),
            Block::ToolUse { name, input, .. } => {
                s.push_str(&format!("[tool_use {name}] {}\n", truncate(&input.to_string(), 200)))
            }
            Block::ToolResult { content, is_error, .. } => s.push_str(&format!(
                "[tool_result{}] {}\n",
                if *is_error { " error" } else { "" },
                truncate(content, 200)
            )),
            Block::File { path, source, .. } => s.push_str(&format!(
                "[file: {}]\n",
                path.as_deref().or(source.as_deref()).unwrap_or("?")
            )),
            Block::Image { .. } => s.push_str("[image]\n"),
        }
    }
    s.push('\n');
    s
}

/// Header for `cv export md`.
fn md_header(h: &HeaderInfo) -> String {
    let model = h
        .model
        .as_ref()
        .map(|m| format!("- model: {m}\n"))
        .unwrap_or_default();
    format!(
        "# {}\n\n- harness: {}\n- id: {}\n- cwd: {}\n{}\n",
        h.label,
        h.harness,
        h.id,
        h.cwd.as_deref().map(home_rel).unwrap_or_else(|| "?".into()),
        model,
    )
}

/// One rendered `cv export md` message section.
fn md_message(m: &Message) -> String {
    let who = match m.role {
        Role::System => "System",
        Role::User => "User",
        Role::Assistant => "Assistant",
        Role::Tool => "Tool",
    };
    let mut out = format!("## {who}\n\n");
    for b in &m.content {
        match b {
            Block::Text { text } => {
                out.push_str(text);
                out.push_str("\n\n");
            }
            Block::Thinking { text, .. } => {
                out.push_str("> 🧠 ");
                out.push_str(&text.replace('\n', "\n> "));
                out.push_str("\n\n");
            }
            Block::ToolUse { name, input, .. } => {
                out.push_str(&format!("**🔧 {name}**\n\n```json\n{input}\n```\n\n"));
            }
            Block::ToolResult { content, is_error, .. } => {
                out.push_str(&format!(
                    "**↩ result{}**\n\n```\n{}\n```\n\n",
                    if *is_error { " (error)" } else { "" },
                    truncate(content, 4000)
                ));
            }
            Block::File { path, source, .. } => {
                out.push_str(&format!(
                    "_[file: {}]_\n\n",
                    path.as_deref().or(source.as_deref()).unwrap_or("?")
                ));
            }
            Block::Image { .. } => out.push_str("_[image]_\n\n"),
        }
    }
    out
}
