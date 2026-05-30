//! `cv` — the claurdvoyant CLI.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use cv_core::ir::*;
use cv_core::watch::{Filter, Watcher};
use cv_core::EmitOptions;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "cv",
    version,
    about = "claurdvoyant — search, read, and port AI agent sessions across harnesses"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List discovered sessions across all harnesses.
    Ls {
        /// Only this harness (claude, codex, grok, opencode, gemini).
        #[arg(long)]
        harness: Option<String>,
        /// Only sessions whose cwd contains this substring.
        #[arg(long)]
        cwd: Option<String>,
        /// Max rows to show.
        #[arg(long, default_value_t = 40)]
        limit: usize,
    },
    /// Full-text search across all session content.
    Search {
        query: String,
        #[arg(long)]
        harness: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Use semantic (embedding) search instead of full-text. Requires `cv index --semantic`
        /// to have been run; downloads a small embedding model on first use.
        #[arg(long)]
        semantic: bool,
    },
    /// Print a single session (by id or id-prefix).
    Show {
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Emit the raw unified IR as JSON instead of a rendered transcript.
        #[arg(long)]
        json: bool,
    },
    /// Export a session to markdown or JSON (stdout).
    Export {
        id: String,
        #[arg(long, default_value = "md")]
        format: String,
        #[arg(long)]
        harness: Option<String>,
    },
    /// Convert a session into another harness's native format (cross-harness port).
    Convert {
        id: String,
        /// Target harness (claude, codex, grok, …).
        #[arg(long)]
        to: String,
        /// Source harness hint (otherwise auto-detected by id).
        #[arg(long)]
        from: Option<String>,
        /// Write under this directory instead of the target's real storage root (safe dry run).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Rehome the converted session to this working directory.
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Rehome a session to a different working directory (and optionally another harness).
    Port {
        id: String,
        /// Target harness (defaults to the source harness).
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        from: Option<String>,
        /// New working directory for the ported session.
        #[arg(long = "to-dir")]
        to_dir: Option<PathBuf>,
        /// Write under this directory instead of the target's real storage root.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Don't copy project context files (CLAUDE.md, MEMORY.md, AGENTS.md, …) to the new cwd.
        #[arg(long = "no-context")]
        no_context: bool,
    },
    /// Follow live agent activity across harnesses (tail -f for sessions).
    Scry {
        #[arg(long)]
        harness: Option<String>,
        /// Only follow sessions whose cwd contains this substring.
        #[arg(long)]
        cwd: Option<String>,
        /// Poll interval in seconds.
        #[arg(long, default_value_t = 2.0)]
        interval: f64,
        /// Also emit the sessions that already exist at startup (default: only new activity).
        #[arg(long)]
        existing: bool,
    },
    /// Build/refresh the tantivy full-text index that makes `cv search` instant.
    Index {
        /// Also build semantic embeddings (`cv search --semantic`). Downloads a small embedding
        /// model (~30MB) on first use.
        #[arg(long)]
        semantic: bool,
    },
    /// Fleet analytics over all discovered sessions.
    Stats,
    /// Print (or with --launch, run) the resume incantation for a session in its native harness.
    Resume {
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Actually spawn the harness (cd to the session's cwd) instead of just printing.
        #[arg(long)]
        launch: bool,
    },
    /// Render a session's message threading (DAG if parent_ids exist, else a numbered list).
    Tree {
        id: String,
        #[arg(long)]
        harness: Option<String>,
    },
    /// Post to / read from the agent coordination board.
    Board {
        #[command(subcommand)]
        action: BoardCmd,
    },
}

#[derive(Subcommand)]
enum BoardCmd {
    /// Post a message to a channel.
    Post {
        channel: String,
        body: String,
        #[arg(long, default_value = "cv")]
        from: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[arg(long = "session-ref")]
        session_ref: Option<String>,
    },
    /// Read messages from a channel.
    Read {
        channel: String,
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// List all channels.
    Channels,
    /// Follow a channel live; with --match, exit when a body contains the substring.
    Watch {
        channel: String,
        #[arg(long)]
        since: Option<String>,
        #[arg(long = "match")]
        pattern: Option<String>,
        #[arg(long, default_value_t = 2.0)]
        interval: f64,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Ls { harness, cwd, limit } => cmd_ls(harness, cwd, limit),
        Cmd::Search { query, harness, limit, semantic } => cmd_search(&query, harness, limit, semantic),
        Cmd::Show { id, harness, json } => cmd_show(&id, harness, json),
        Cmd::Export { id, format, harness } => cmd_export(&id, &format, harness),
        Cmd::Convert { id, to, from, out, cwd } => cmd_convert(&id, &to, from, out, cwd),
        Cmd::Port { id, to, from, to_dir, out, no_context } => cmd_port(&id, to, from, to_dir, out, no_context),
        Cmd::Scry { harness, cwd, interval, existing } => cmd_scry(harness, cwd, interval, existing),
        Cmd::Index { semantic } => cmd_index(semantic),
        Cmd::Stats => cmd_stats(),
        Cmd::Resume { id, harness, launch } => cmd_resume(&id, harness, launch),
        Cmd::Tree { id, harness } => cmd_tree(&id, harness),
        Cmd::Board { action } => cmd_board(action),
    }
}

fn cmd_board(action: BoardCmd) -> Result<()> {
    use cv_core::board;
    match action {
        BoardCmd::Post { channel, body, from, kind, tags, session_ref } => {
            let m = board::post(&channel, &from, &body, kind.as_deref(), tags, session_ref)?;
            println!("✦ posted {} to #{}", short_id(&m.id), channel);
        }
        BoardCmd::Read { channel, since, limit, json } => {
            let msgs = board::read(&channel, since.as_deref(), limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&msgs)?);
            } else {
                for m in &msgs {
                    print_board_msg(m);
                }
                if msgs.is_empty() {
                    println!("(no messages on #{channel})");
                }
            }
        }
        BoardCmd::Channels => {
            for c in board::channels()? {
                println!("#{c}");
            }
        }
        BoardCmd::Watch { channel, since, pattern, interval } => {
            eprintln!("✦ watching #{channel} … (Ctrl-C to stop)");
            let mut cursor = since;
            loop {
                for m in board::read(&channel, cursor.as_deref(), 0)? {
                    cursor = Some(m.id.clone());
                    print_board_msg(&m);
                    if let Some(p) = &pattern {
                        if m.body.contains(p.as_str()) {
                            println!("✓ matched {p:?} — done");
                            return Ok(());
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs_f64(interval.max(0.25)));
            }
        }
    }
    Ok(())
}

fn print_board_msg(m: &cv_core::board::BoardMessage) {
    println!(
        "{}  {}  ({}) {}",
        m.ts.format("%H:%M:%S"),
        m.from,
        m.kind,
        m.body
    );
}

fn cmd_index(semantic: bool) -> Result<()> {
    eprintln!("✦ building full-text index…");
    let n = cv_search::index_all(None)?;
    println!(
        "indexed {n} session(s) → {}",
        cv_search::default_tantivy_dir().display()
    );
    if semantic {
        eprintln!("✦ embedding sessions (downloads a small model on first use)…");
        let e = cv_search::embed_all(None)?;
        println!(
            "embedded {e} session(s) → {}",
            cv_search::default_embeddings_path().display()
        );
    }
    Ok(())
}

// ---------- stats ----------

fn cmd_stats() -> Result<()> {
    use std::collections::HashMap;
    let refs = cv_core::discover_all();
    let total = refs.len();
    if total == 0 {
        println!("no sessions discovered.");
        return Ok(());
    }

    let mut per_harness: HashMap<&'static str, usize> = HashMap::new();
    let mut per_cwd: HashMap<String, usize> = HashMap::new();
    let mut total_messages: usize = 0;
    let mut min_created: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut max_updated: Option<chrono::DateTime<chrono::Utc>> = None;

    for r in &refs {
        *per_harness.entry(r.harness.as_str()).or_default() += 1;
        total_messages += r.message_count;
        let cwd = r
            .cwd
            .as_deref()
            .map(home_rel)
            .unwrap_or_else(|| "(no cwd)".into());
        *per_cwd.entry(cwd).or_default() += 1;
        if let Some(c) = r.created_at {
            min_created = Some(min_created.map_or(c, |m| m.min(c)));
        }
        if let Some(u) = r.updated_at {
            max_updated = Some(max_updated.map_or(u, |m| m.max(u)));
        }
    }

    println!("✦ claurdvoyant fleet stats\n");
    println!("{total} session(s) · {total_messages} message(s)\n");

    println!("by harness:");
    let mut hv: Vec<_> = per_harness.into_iter().collect();
    hv.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    for (h, n) in hv {
        println!("  {h:12} {n:>5}");
    }

    println!("\ntop cwds:");
    let mut cv: Vec<_> = per_cwd.into_iter().collect();
    cv.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (c, n) in cv.into_iter().take(10) {
        println!("  {n:>5}  {}", truncate(&c, 70));
    }

    println!("\ndate range:");
    println!(
        "  earliest created: {}",
        min_created
            .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "?".into())
    );
    println!(
        "  latest updated:   {}",
        max_updated
            .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "?".into())
    );
    Ok(())
}

// ---------- resume ----------

fn cmd_resume(id: &str, harness: Option<String>, launch: bool) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, _adapter) =
        cv_core::find(id, want)?.with_context(|| format!("no session matching {id:?}"))?;
    let cwd = r.cwd.clone();
    let (program, args) = resume_command(r.harness, &r.id);

    if launch {
        let dir = cwd.clone().unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        eprintln!(
            "✦ launching: (cd {}) {} {}",
            home_rel(&dir),
            program,
            args.join(" ")
        );
        let status = std::process::Command::new(&program)
            .args(&args)
            .current_dir(&dir)
            .status()
            .with_context(|| format!("failed to launch {program:?}"))?;
        if !status.success() {
            bail!("{program} exited with status {status}");
        }
        return Ok(());
    }

    // Print the incantation.
    if let Some(dir) = &cwd {
        println!("cd {}", shell_quote(&dir.display().to_string()));
    }
    println!("{} {}", program, args.join(" "));
    Ok(())
}

/// Best-known resume incantation per harness: the program + its args (the cwd is handled
/// separately, since most harnesses resume relative to the directory they're launched in).
fn resume_command(h: Harness, id: &str) -> (String, Vec<String>) {
    match h {
        Harness::Claude => ("claude".into(), vec!["--resume".into(), id.into()]),
        Harness::Codex => ("codex".into(), vec!["resume".into(), id.into()]),
        Harness::Grok => ("grok".into(), vec!["--resume".into(), id.into()]),
        Harness::OpenCode => ("opencode".into(), vec!["--session".into(), id.into()]),
        Harness::Gemini => ("gemini".into(), vec!["--resume".into(), id.into()]),
        Harness::Hermes => ("hermes".into(), vec!["resume".into(), id.into()]),
        Harness::OpenClaw => ("openclaw".into(), vec!["--resume".into(), id.into()]),
        // Desktop/IDE apps and others have no documented CLI resume — emit a best-effort hint.
        Harness::Cursor | Harness::ClaudeApp | Harness::ChatGptApp => (
            format!("# no CLI resume for {h}; open the app and find session"),
            vec![id.into()],
        ),
    }
}

fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b'~' | b'+' | b':' | b'@'))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

// ---------- tree ----------

fn cmd_tree(id: &str, harness: Option<String>) -> Result<()> {
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

fn parse_harness(s: &Option<String>) -> Result<Option<Harness>> {
    match s {
        None => Ok(None),
        Some(s) => Harness::parse(s)
            .map(Some)
            .with_context(|| format!("unknown harness: {s}")),
    }
}

fn cmd_ls(harness: Option<String>, cwd: Option<String>, limit: usize) -> Result<()> {
    let want = parse_harness(&harness)?;
    let mut refs: Vec<SessionRef> = cv_core::discover_all()
        .into_iter()
        .filter(|r| want.map_or(true, |h| r.harness == h))
        .filter(|r| match &cwd {
            None => true,
            Some(c) => r
                .cwd
                .as_ref()
                .map(|p| p.to_string_lossy().contains(c))
                .unwrap_or(false),
        })
        .collect();

    refs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    let total = refs.len();
    println!("{total} session(s)\n");
    for r in refs.iter().take(limit) {
        println!(
            "{:8}  {:8}  {:10}  {:>4} msg  {}",
            r.harness.as_str(),
            short_id(&r.id),
            r.updated_at
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "----------".into()),
            r.message_count,
            r.title
                .clone()
                .map(|t| truncate(&t, 60))
                .unwrap_or_else(|| dim_cwd(r.cwd.as_deref())),
        );
    }
    if total > limit {
        println!("\n… {} more (use --limit)", total - limit);
    }
    Ok(())
}

fn cmd_search(query: &str, harness: Option<String>, limit: usize, semantic: bool) -> Result<()> {
    let want = parse_harness(&harness)?;

    // Semantic search: embed the query and rank stored vectors. Requires `cv index --semantic`.
    if semantic {
        let hits = cv_search::semantic_search(None, query, limit.saturating_mul(4))
            .context("semantic search failed (run `cv index --semantic` first?)")?;
        render_search_hits(&hits, want, limit, query, "semantic");
        return Ok(());
    }

    // Preferred path: the tantivy full-text index (real tokenization + BM25). Authoritative when
    // present — an empty result means "no match", not "fall back to a live scan".
    if cv_search::default_tantivy_dir().exists() {
        match cv_search::text_search(None, query, limit.saturating_mul(4)) {
            Ok(hits) => {
                render_search_hits(&hits, want, limit, query, "index");
                return Ok(());
            }
            Err(e) => eprintln!("(tantivy index unavailable: {e:#}; trying sqlite/live)"),
        }
    }

    // Fallback: a prebuilt SQLite FTS index. If it exists, it's authoritative — an empty result
    // means "no match", not "fall back to a 90-second live scan". Only scan live if there's none.
    let idx_path = cv_core::index::default_index_path();
    if idx_path.exists() {
        match cv_core::index::Index::open_or_create(idx_path)
            .and_then(|idx| idx.search(query, limit.saturating_mul(4)))
        {
            Ok(found) => {
                let rows: Vec<_> = found
                    .into_iter()
                    .filter(|h| want.map_or(true, |w| h.harness == w))
                    .take(limit)
                    .collect();
                if rows.is_empty() {
                    println!("no matches for {query:?} (index; try `cv index` to refresh)");
                }
                for h in rows {
                    println!(
                        "{:8}  {:8}  {:10}  {}",
                        h.harness.as_str(),
                        short_id(&h.id),
                        h.updated_at
                            .map(|d| d.format("%Y-%m-%d").to_string())
                            .unwrap_or_else(|| "----------".into()),
                        h.title.clone().unwrap_or_default(),
                    );
                    if !h.snippet.trim().is_empty() {
                        println!("          … {}", truncate(&h.snippet, 120));
                    }
                }
                return Ok(());
            }
            Err(e) => eprintln!("(index unavailable: {e:#}; scanning live)"),
        }
    } else {
        eprintln!("(no index yet — scanning live; run `cv index` for instant search)");
    }
    cmd_search_live(query, want, limit)
}

/// Render a slice of cv-search [`cv_search::Hit`]s with harness/short-id/date/title/snippet,
/// applying the `--harness` filter and `--limit`. `source` labels the empty-result hint.
fn render_search_hits(
    hits: &[cv_search::Hit],
    want: Option<Harness>,
    limit: usize,
    query: &str,
    source: &str,
) {
    // Dates aren't carried on Hit; pull them cheaply from discovery (no parse) for the rows we show.
    let dates = session_date_map();
    let rows: Vec<&cv_search::Hit> = hits
        .iter()
        .filter(|h| want.map_or(true, |w| h.harness == w.as_str()))
        .take(limit)
        .collect();
    if rows.is_empty() {
        let hint = if source == "semantic" {
            "(semantic; run `cv index --semantic` to (re)build embeddings)"
        } else {
            "(index; try `cv index` to refresh)"
        };
        println!("no matches for {query:?} {hint}");
        return;
    }
    for h in rows {
        let date = dates
            .get(&h.id)
            .and_then(|d| *d)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "----------".into());
        println!(
            "{:8}  {:8}  {:10}  {}",
            h.harness,
            short_id(&h.id),
            date,
            h.title.clone().unwrap_or_default(),
        );
        if !h.snippet.trim().is_empty() {
            println!("          … {}", truncate(&h.snippet, 120));
        }
    }
}

/// id → updated_at (falling back to created_at) from a cheap discovery pass.
fn session_date_map() -> std::collections::HashMap<String, Option<chrono::DateTime<chrono::Utc>>> {
    cv_core::discover_all()
        .into_iter()
        .map(|r| (r.id, r.updated_at.or(r.created_at)))
        .collect()
}

fn cmd_search_live(query: &str, want: Option<Harness>, limit: usize) -> Result<()> {
    let needle = query.to_lowercase();
    let mut hits = 0;

    for adapter in cv_core::harness::all() {
        if want.map_or(false, |h| adapter.harness() != h) || adapter.storage_root().is_none() {
            continue;
        }
        for r in adapter.discover()? {
            let session = match adapter.parse(&r) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let hay = session.searchable_text().to_lowercase();
            if let Some(pos) = hay.find(&needle) {
                hits += 1;
                println!(
                    "{:8}  {:8}  {:10}  {}",
                    session.harness.as_str(),
                    short_id(&session.id),
                    session
                        .updated_at
                        .map(|d| d.format("%Y-%m-%d").to_string())
                        .unwrap_or_else(|| "----------".into()),
                    session.label(),
                );
                println!("          … {}", snippet(&hay, pos, needle.len()));
                if hits >= limit {
                    println!("\n(stopped at {limit} hits; use --limit)");
                    return Ok(());
                }
            }
        }
    }
    if hits == 0 {
        println!("no matches for {query:?}");
    }
    Ok(())
}

fn cmd_show(id: &str, harness: Option<String>, json: bool) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, adapter) = cv_core::find(id, want)?.with_context(|| format!("no session matching {id:?}"))?;
    let session = adapter.parse(&r)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&session)?);
        return Ok(());
    }

    println!("# {}", session.label());
    println!(
        "{} · {} · {}{}",
        session.harness,
        session.id,
        session
            .cwd
            .as_deref()
            .map(home_rel)
            .unwrap_or_else(|| "?".into()),
        session
            .model
            .as_ref()
            .map(|m| format!(" · {m}"))
            .unwrap_or_default()
    );
    println!();
    for m in &session.messages {
        print_message(m);
    }
    Ok(())
}

fn cmd_export(id: &str, format: &str, harness: Option<String>) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, adapter) = cv_core::find(id, want)?.with_context(|| format!("no session matching {id:?}"))?;
    let session = adapter.parse(&r)?;
    match format {
        "json" => println!("{}", serde_json::to_string_pretty(&session)?),
        "md" | "markdown" => print!("{}", to_markdown(&session)),
        other => bail!("unknown format {other:?} (use md or json)"),
    }
    Ok(())
}

fn cmd_convert(
    id: &str,
    to: &str,
    from: Option<String>,
    out: Option<PathBuf>,
    cwd: Option<PathBuf>,
) -> Result<()> {
    let from_h = parse_harness(&from)?;
    let to_h = Harness::parse(to).with_context(|| format!("unknown target harness: {to}"))?;
    let (r, adapter) =
        cv_core::find(id, from_h)?.with_context(|| format!("no session matching {id:?}"))?;
    let session = adapter.parse(&r)?;
    emit_session(&session, to_h, out, EmitOptions { new_cwd: cwd, new_id: None })
}

fn cmd_port(
    id: &str,
    to: Option<String>,
    from: Option<String>,
    to_dir: Option<PathBuf>,
    out: Option<PathBuf>,
    no_context: bool,
) -> Result<()> {
    let from_h = parse_harness(&from)?;
    let (r, adapter) =
        cv_core::find(id, from_h)?.with_context(|| format!("no session matching {id:?}"))?;
    let session = adapter.parse(&r)?;
    // Default to the same harness — a pure rehome.
    let to_h = match to {
        Some(s) => Harness::parse(&s).with_context(|| format!("unknown target harness: {s}"))?,
        None => session.harness,
    };
    let new_cwd = to_dir.clone();
    emit_session(&session, to_h, out, EmitOptions { new_cwd: to_dir, new_id: None })?;

    // Carry the project's context files to the new home, so the ported session keeps its memory.
    if !no_context {
        if let (Some(src), Some(dst)) = (session.cwd.as_deref(), new_cwd.as_deref()) {
            carry_context(src, dst);
        }
    }
    Ok(())
}

/// Project context files a harness reads from the cwd. We copy these alongside a ported session so
/// it lands with its memory/instructions intact. Best-effort: never overwrite, never fatal.
const CONTEXT_FILES: &[&str] = &[
    "CLAUDE.md",
    "CLAUDE.local.md",
    "AGENTS.md",
    "GEMINI.md",
    "MEMORY.md",
    ".cursorrules",
    ".windsurfrules",
];

fn carry_context(src: &Path, dst: &Path) {
    if src == dst {
        return;
    }
    let mut copied = Vec::new();
    for name in CONTEXT_FILES {
        let from = src.join(name);
        if !from.is_file() {
            continue;
        }
        let to = dst.join(name);
        if to.exists() {
            eprintln!("  ↳ context: {name} already exists at target — left as-is");
            continue;
        }
        match fs::create_dir_all(dst).and_then(|_| fs::copy(&from, &to)) {
            Ok(_) => copied.push(*name),
            Err(e) => eprintln!("  ↳ context: couldn't copy {name}: {e}"),
        }
    }
    if !copied.is_empty() {
        println!("  ↳ carried context: {}", copied.join(", "));
    }
}

fn emit_session(
    session: &Session,
    to_h: Harness,
    out: Option<PathBuf>,
    opts: EmitOptions,
) -> Result<()> {
    if !cv_core::emit::supported_targets().contains(&to_h) {
        bail!(
            "emitting to {to_h} isn't supported yet — the source parses fine ({} messages), but the \
             {to_h} emitter is still TODO (supported: {})",
            session.messages.len(),
            cv_core::emit::supported_targets()
                .iter()
                .map(|h| h.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let out_dir = match out {
        Some(d) => d,
        None => cv_core::harness::for_harness(to_h)
            .and_then(|a| a.storage_root())
            .with_context(|| {
                format!("{to_h} doesn't appear installed; pass --out <dir> to write somewhere")
            })?,
    };
    let res = cv_core::emit(session, to_h, &out_dir, &opts)?;
    println!("✦ wrote {} ({})", res.path.display(), res.new_id);
    if let Some(hint) = res.resume_hint {
        println!("  ↳ {hint}");
    }
    Ok(())
}

fn cmd_scry(
    harness: Option<String>,
    cwd: Option<String>,
    interval: f64,
    existing: bool,
) -> Result<()> {
    let filter = Filter {
        harness: parse_harness(&harness)?,
        cwd_contains: cwd,
    };
    let mut watcher = Watcher::new(filter, existing);
    eprintln!("✦ scrying for agent activity… (Ctrl-C to stop)");
    watcher.run(Duration::from_secs_f64(interval.max(0.25)), |ev| {
        let r = &ev.reference;
        let tag = match ev.kind {
            cv_core::watch::EventKind::New => "✷ new",
            cv_core::watch::EventKind::Updated => "   +",
        };
        let where_ = r
            .cwd
            .as_deref()
            .map(home_rel)
            .unwrap_or_else(|| "?".into());
        println!(
            "{tag}  {:8} {:8}  {}  ({} msg)",
            r.harness.as_str(),
            short_id(&r.id),
            where_,
            ev.new_messages.len()
        );
        for m in &ev.new_messages {
            if let Some(t) = m.text() {
                println!("      {} {}", cv_core::render::role_label(m.role), truncate(&t, 100));
            }
        }
    });
}

// ---------- rendering helpers ----------

fn print_message(m: &Message) {
    let tag = match m.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    println!("── {tag} ──");
    for b in &m.content {
        match b {
            Block::Text { text } => println!("{text}"),
            Block::Thinking { text, .. } => println!("[thinking] {}", truncate(text, 200)),
            Block::ToolUse { name, input, .. } => {
                println!("[tool_use {name}] {}", truncate(&input.to_string(), 200))
            }
            Block::ToolResult { content, is_error, .. } => println!(
                "[tool_result{}] {}",
                if *is_error { " error" } else { "" },
                truncate(content, 200)
            ),
            Block::File { path, source, .. } => println!(
                "[file: {}]",
                path.as_deref().or(source.as_deref()).unwrap_or("?")
            ),
            Block::Image { .. } => println!("[image]"),
        }
    }
    println!();
}

fn to_markdown(s: &Session) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", s.label()));
    out.push_str(&format!(
        "- harness: {}\n- id: {}\n- cwd: {}\n",
        s.harness,
        s.id,
        s.cwd.as_deref().map(home_rel).unwrap_or_else(|| "?".into())
    ));
    if let Some(m) = &s.model {
        out.push_str(&format!("- model: {m}\n"));
    }
    out.push('\n');
    for m in &s.messages {
        let who = match m.role {
            Role::System => "System",
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::Tool => "Tool",
        };
        out.push_str(&format!("## {who}\n\n"));
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
    }
    out
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn snippet(hay: &str, pos: usize, len: usize) -> String {
    let start = pos.saturating_sub(40);
    let end = (pos + len + 40).min(hay.len());
    let s = &hay[floor_char(hay, start)..ceil_char(hay, end)];
    truncate(&s.replace('\n', " "), 120)
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
fn ceil_char(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max {
        s
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn home_rel(p: &Path) -> String {
    if let Some(home) = dirs_home() {
        if let Ok(rest) = p.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    p.display().to_string()
}

fn dim_cwd(p: Option<&Path>) -> String {
    p.map(home_rel).unwrap_or_else(|| "(no cwd)".into())
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}
