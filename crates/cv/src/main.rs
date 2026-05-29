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
    /// Build/refresh the SQLite FTS index that makes `cv search` instant.
    Index,
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
        Cmd::Search { query, harness, limit } => cmd_search(&query, harness, limit),
        Cmd::Show { id, harness, json } => cmd_show(&id, harness, json),
        Cmd::Export { id, format, harness } => cmd_export(&id, &format, harness),
        Cmd::Convert { id, to, from, out, cwd } => cmd_convert(&id, &to, from, out, cwd),
        Cmd::Port { id, to, from, to_dir, out, no_context } => cmd_port(&id, to, from, to_dir, out, no_context),
        Cmd::Scry { harness, cwd, interval, existing } => cmd_scry(harness, cwd, interval, existing),
        Cmd::Index => cmd_index(),
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

fn cmd_index() -> Result<()> {
    let path = cv_core::index::default_index_path();
    let idx = cv_core::index::Index::open_or_create(path.clone())?;
    eprintln!("✦ indexing all sessions…");
    let n = idx.rebuild()?;
    println!("indexed {n} session(s) → {}", path.display());
    Ok(())
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

fn cmd_search(query: &str, harness: Option<String>, limit: usize) -> Result<()> {
    let want = parse_harness(&harness)?;

    // Fast path: a prebuilt FTS index. If it exists, it's authoritative — an empty result means
    // "no match", not "fall back to a 90-second live scan". Only scan live if there's no index.
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
