//! `cv` — the claurdvoyant CLI.

mod blame;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use cv_core::ir::*;
use cv_core::watch::{Filter, Watcher};
use cv_core::Adapter;
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
        /// Sort key: updated (default), created, or messages.
        #[arg(long = "sort-by", default_value = "updated", value_parser = ["updated", "created", "messages"])]
        sort_by: String,
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
        /// Only render messages in this 0-based, end-exclusive window: `<start>-<end>`,
        /// `<start>-` (through the last), or `-<end>` (from the first). Messages outside the
        /// window are never resolved — large content stays on disk, so a windowed view of a
        /// huge session reads only the bytes it shows.
        #[arg(long)]
        range: Option<String>,
    },
    /// Export a session to markdown, JSON, or self-contained HTML (stdout).
    Export {
        id: String,
        /// Output format: `md` (default), `json`, or `html`.
        #[arg(long, default_value = "md")]
        format: String,
        #[arg(long)]
        harness: Option<String>,
    },
    /// Export the corpus as a fine-tuning dataset (JSONL, one session per line).
    /// `chatml`/`sharegpt` import directly into Unsloth Studio / TRL / HF — no adapter.
    Dataset {
        /// `chatml` (default) → {"messages":[…]}, or `sharegpt` → {"conversations":[…]}.
        #[arg(long, default_value = "chatml")]
        format: String,
        /// Only this harness (claude, codex, hermes, …); omit for all.
        #[arg(long)]
        harness: Option<String>,
        /// Stop after N emitted records.
        #[arg(long)]
        limit: Option<usize>,
        /// Skip sessions with fewer than this many messages (drops trivial/empty ones).
        #[arg(long, default_value_t = 2)]
        min_messages: usize,
        /// Scrub secrets/PII from every session before emitting (cv_core::redact).
        #[arg(long)]
        redact: bool,
        /// Write to this file instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
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
    ///
    /// Incremental by default: only changed/new sessions are re-indexed and vanished ones reaped,
    /// so routine refreshes are fast and light. Use `--rebuild` to clear and rebuild from scratch.
    /// Tool-call events (file edits/reads, commands, errors) ride along on the same pass into the
    /// catalog, powering `cv events` and `cv touched`.
    Index {
        /// Also build semantic embeddings (`cv search --semantic`). Downloads a small embedding
        /// model (~30MB) on first use.
        #[arg(long)]
        semantic: bool,
        /// Clear and rebuild the index from scratch instead of incrementally updating it.
        #[arg(long)]
        rebuild: bool,
    },
    /// List what a session DID: its extracted events (file edits/reads, commands, errors).
    ///
    /// Events are ingested during `cv index`; a session that isn't cataloged yet (or whose file
    /// changed since) is ingested on the spot — one streamed pass, large content stays on disk.
    Events {
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Only this kind: file_edit, file_read, command, tool, or error.
        #[arg(long)]
        kind: Option<String>,
    },
    /// Sessions that touched a file — every session with a file_edit/file_read event on its path.
    ///
    /// The path is matched absolutely and by suffix, so `cv touched src/ir.rs` finds sessions
    /// that edited `/any/repo/src/ir.rs`. Run `cv index` first to (re)ingest events.
    Touched {
        path: String,
        /// Only sessions that EDITED the file (drop read-only appearances).
        #[arg(long)]
        edits_only: bool,
    },
    /// Code provenance: which agent session wrote this code, and what was it thinking?
    ///
    /// Correlates the file's git history with the event catalog's file_edit events — an agent
    /// edit shortly before a commit is strong evidence that session authored it. Each matched
    /// commit gets its best sessions plus a `cv show --range` hint into the conversation around
    /// the edit. Run `cv index` first to ingest events.
    Blame {
        file: String,
        /// Only these lines: `<line>` or `<line>,<endline>` (via `git blame -L`).
        #[arg(short = 'L', value_name = "LINE[,ENDLINE]")]
        lines: Option<String>,
        /// Also print the conversation window around the single best-matched edit.
        #[arg(long)]
        show: bool,
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
    /// Unified chronological feed across all harnesses (oldest → newest).
    Timeline {
        #[arg(long)]
        harness: Option<String>,
        /// Only sessions whose cwd contains this substring.
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long, default_value_t = 60)]
        limit: usize,
    },
    /// Compare two sessions message-by-message (great for loom branches).
    Diff {
        a: String,
        b: String,
        #[arg(long)]
        harness: Option<String>,
    },
    /// Compose a new session from spans of existing ones (`<id>:<start>-<end>`).
    Splice {
        /// One or more specs: `<id>:<start>-<end>`, `<id>:<start>-`, or `<id>` (whole session).
        #[arg(required = true)]
        specs: Vec<String>,
        /// Target harness for the composed session (defaults to the first spec's harness).
        #[arg(long)]
        to: Option<String>,
        /// Write under this directory instead of the target's real storage root.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Print the composed session instead of emitting it: md or json.
        #[arg(long)]
        export: Option<String>,
        /// Rehome the composed session to this working directory.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Generate a continuation of the composed session via an LLM and append it (the loom's
        /// generative step). Uses OPENROUTER/ANTHROPIC keys or LMSTUDIO_API_BASE for free local gen.
        #[arg(long)]
        generate: bool,
        /// Model for --generate (provider-specific; defaults to the provider's default).
        #[arg(long = "gen-model")]
        gen_model: Option<String>,
    },
    /// Distill a session into durable memory (decisions, gotchas, where things live) via an LLM.
    Distill {
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Model id (provider-specific; defaults to the provider's cheap/fast model).
        #[arg(long)]
        model: Option<String>,
        /// Frame the distillation as whole-project memory (may span more than one session).
        #[arg(long)]
        project: bool,
        /// Write the digest to this file instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        /// With --out, append under a dated header instead of overwriting (build a MEMORY.md).
        #[arg(long)]
        append: bool,
    },
    /// Semantic recall across the corpus: the most relevant past spans for a query (CLI recall).
    Recall {
        query: String,
        #[arg(short = 'k', default_value_t = 5)]
        k: usize,
        #[arg(long)]
        harness: Option<String>,
    },
    /// Scrub secrets/PII from a session and export it (safe to share).
    Redact {
        id: String,
        #[arg(long)]
        harness: Option<String>,
        #[arg(long, default_value = "md")]
        format: String,
        /// Also print redaction counts per class to stderr.
        #[arg(long)]
        stats: bool,
    },
    /// Loom graft: take base[..N], then graft other[M..] into one new branched session.
    Loom {
        base: String,
        #[arg(long)]
        at: usize,
        #[arg(long)]
        graft: String,
        #[arg(long)]
        from: usize,
        /// Target harness for the grafted session (defaults to the base's harness).
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        out: Option<PathBuf>,
        /// Print the grafted session instead of emitting it: md or json.
        #[arg(long)]
        export: Option<String>,
        /// Rehome the grafted session to this working directory.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Generate a continuation of the grafted branch via an LLM and append it.
        #[arg(long)]
        generate: bool,
        /// Model for --generate.
        #[arg(long = "gen-model")]
        gen_model: Option<String>,
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
    /// Post a question others can `reply` to; prints the request id (the correlation key).
    Request {
        channel: String,
        body: String,
        #[arg(long, default_value = "cv")]
        from: String,
    },
    /// Answer a request by its id.
    Reply {
        channel: String,
        /// The request (or message) id this answers.
        in_reply_to: String,
        body: String,
        #[arg(long, default_value = "cv")]
        from: String,
    },
    /// Collect all replies to a request id.
    Replies {
        channel: String,
        request_id: String,
    },
    /// Try to claim a task `key` (a soft lease). Exits non-zero on contention.
    Claim {
        channel: String,
        key: String,
        #[arg(long, default_value = "cv")]
        from: String,
        /// Lease duration in seconds.
        #[arg(long = "ttl-secs", default_value_t = 300)]
        ttl_secs: u64,
    },
    /// Release a held claim on `key`.
    Release {
        channel: String,
        key: String,
        #[arg(long, default_value = "cv")]
        from: String,
    },
    /// List the currently-active (un-expired) claims on a channel.
    Claims {
        channel: String,
    },
    /// List agents that heartbeat on a channel recently (also posts your own heartbeat).
    Who {
        channel: String,
        #[arg(long = "within-secs", default_value_t = 60)]
        within_secs: u64,
    },
    /// Acknowledge a message id with a tiny ack note.
    Ack {
        channel: String,
        message_id: String,
        #[arg(long, default_value = "cv")]
        from: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Ls { harness, cwd, limit, sort_by } => cmd_ls(harness, cwd, limit, &sort_by),
        Cmd::Search { query, harness, limit, semantic } => cmd_search(&query, harness, limit, semantic),
        Cmd::Show { id, harness, json, range } => cmd_show(&id, harness, json, range),
        Cmd::Export { id, format, harness } => cmd_export(&id, &format, harness),
        Cmd::Dataset { format, harness, limit, min_messages, redact, out } => {
            cmd_dataset(&format, harness, limit, min_messages, redact, out)
        }
        Cmd::Convert { id, to, from, out, cwd } => cmd_convert(&id, &to, from, out, cwd),
        Cmd::Port { id, to, from, to_dir, out, no_context } => cmd_port(&id, to, from, to_dir, out, no_context),
        Cmd::Scry { harness, cwd, interval, existing } => cmd_scry(harness, cwd, interval, existing),
        Cmd::Index { semantic, rebuild } => cmd_index(semantic, rebuild),
        Cmd::Events { id, harness, kind } => cmd_events(&id, harness, kind),
        Cmd::Touched { path, edits_only } => cmd_touched(&path, edits_only),
        Cmd::Blame { file, lines, show } => blame::cmd_blame(&file, lines.as_deref(), show),
        Cmd::Stats => cmd_stats(),
        Cmd::Resume { id, harness, launch } => cmd_resume(&id, harness, launch),
        Cmd::Tree { id, harness } => cmd_tree(&id, harness),
        Cmd::Board { action } => cmd_board(action),
        Cmd::Timeline { harness, cwd, limit } => cmd_timeline(harness, cwd, limit),
        Cmd::Diff { a, b, harness } => cmd_diff(&a, &b, harness),
        Cmd::Splice { specs, to, out, export, cwd, generate, gen_model } => cmd_splice(&specs, to, out, export, cwd, generate, gen_model),
        Cmd::Distill { id, harness, model, project, out, append } => {
            cmd_distill(&id, harness, model, project, out, append)
        }
        Cmd::Recall { query, k, harness } => cmd_recall(&query, k, harness),
        Cmd::Redact { id, harness, format, stats } => cmd_redact(&id, harness, &format, stats),
        Cmd::Loom { base, at, graft, from, to, out, export, cwd, generate, gen_model } => {
            cmd_loom(&base, at, &graft, from, to, out, export, cwd, generate, gen_model)
        }
    }
}

// ---------- distill / recall / redact ----------

fn cmd_distill(
    id: &str,
    harness: Option<String>,
    model: Option<String>,
    project: bool,
    out: Option<PathBuf>,
    append: bool,
) -> Result<()> {
    let want = parse_harness(&harness)?;
    let (r, adapter) =
        cv_core::find(id, want)?.with_context(|| format!("no session matching {id:?}"))?;
    let session = adapter.parse(&r)?;

    match cv_llm::available_provider() {
        None => {
            eprintln!(
                "no LLM provider configured. Set one of:\n  \
                 OPENROUTER_API_KEY=…   (OpenRouter; preferred)\n  \
                 ANTHROPIC_API_KEY=…    (Anthropic)\n  \
                 LMSTUDIO_API_BASE=local  (a free local LM Studio server at localhost:1234)"
            );
            std::process::exit(1);
        }
        Some(p) => eprintln!("✦ distilling via {p}…"),
    }

    let opts = cv_llm::DistillOptions { model, project };
    let digest = cv_llm::distill(&session, &opts).context("distillation failed")?;

    match out {
        None => print!("{digest}"),
        Some(path) => {
            if append {
                use std::io::Write;
                let date = session
                    .updated_at
                    .or(session.created_at)
                    .map(|d| d.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "----------".into());
                let header = format!("\n\n## {} ({})\n", session.label(), date);
                let mut f = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .with_context(|| format!("opening {} for append", path.display()))?;
                f.write_all(header.as_bytes())?;
                f.write_all(digest.as_bytes())?;
                if !digest.ends_with('\n') {
                    f.write_all(b"\n")?;
                }
                eprintln!("✦ appended distillation → {}", path.display());
            } else {
                fs::write(&path, &digest)
                    .with_context(|| format!("writing {}", path.display()))?;
                eprintln!("✦ wrote distillation → {}", path.display());
            }
        }
    }
    Ok(())
}

fn cmd_recall(query: &str, k: usize, harness: Option<String>) -> Result<()> {
    let want = parse_harness(&harness)?;
    let k = k.max(1);
    // Over-fetch when filtering by harness so we can still fill `k`.
    let fetch = if want.is_some() { (k * 4).max(20) } else { k };

    let (mut hits, mode) = match cv_search::semantic_search(None, query, fetch) {
        Ok(h) => (h, "semantic"),
        Err(e) => {
            eprintln!(
                "(semantic search unavailable: {e:#}; falling back to keyword mode — run \
                 `cv index --semantic` for semantic recall)"
            );
            (cv_search::text_search(None, query, fetch)?, "keyword")
        }
    };

    if let Some(h) = want {
        let w = h.as_str();
        hits.retain(|hit| hit.harness == w);
    }
    hits.truncate(k);

    if hits.is_empty() {
        println!("no recall matches for {query:?} ({mode})");
        return Ok(());
    }

    for hit in &hits {
        let cwd = hit
            .cwd
            .as_deref()
            .map(|c| home_rel(Path::new(c)))
            .unwrap_or_else(|| "(no cwd)".into());
        println!(
            "{:8}  {:8}  {:>6.3}  {}  ·  {}",
            hit.harness,
            short_id(&hit.id),
            hit.score,
            truncate(&hit.title.clone().unwrap_or_default(), 50),
            truncate(&cwd, 40),
        );
        let excerpt = recall_excerpt(hit, query);
        for line in excerpt.lines() {
            println!("      {line}");
        }
    }
    Ok(())
}

/// Best excerpt for a recall hit: prefer the stored snippet; otherwise load the session and render
/// a compact ~3-message window around the best textual match of `query`.
fn recall_excerpt(hit: &cv_search::Hit, query: &str) -> String {
    if !hit.snippet.trim().is_empty() {
        return truncate(&hit.snippet, 200);
    }
    let h = Harness::parse(&hit.harness);
    let Some((sref, adapter)) = cv_core::find(&hit.id, h).ok().flatten() else {
        return String::new();
    };
    let Ok(session) = adapter.parse(&sref) else {
        return String::new();
    };
    if session.messages.is_empty() {
        return String::new();
    }
    // Pick the message best matching the query (most query words present), with a small window.
    let needle = query.to_lowercase();
    let words: Vec<&str> = needle.split_whitespace().filter(|w| w.len() > 2).collect();
    let mut best = 0usize;
    let mut best_score = -1i64;
    for (i, m) in session.messages.iter().enumerate() {
        let t = m.text().unwrap_or_default().to_lowercase();
        if t.trim().is_empty() {
            continue;
        }
        let score = words.iter().filter(|w| t.contains(**w)).count() as i64
            + if t.contains(&needle) { 5 } else { 0 };
        if score > best_score {
            best_score = score;
            best = i;
        }
    }
    let lo = best.saturating_sub(1);
    let hi = (best + 2).min(session.messages.len());
    let mut out = String::new();
    for m in &session.messages[lo..hi] {
        if let Some(t) = m.text() {
            if !t.trim().is_empty() {
                out.push_str(cv_core::render::role_label(m.role));
                out.push_str(": ");
                out.push_str(&truncate(&t, 100));
                out.push('\n');
            }
        }
    }
    out.trim_end().to_string()
}

fn cmd_redact(id: &str, harness: Option<String>, format: &str, stats: bool) -> Result<()> {
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
        BoardCmd::Request { channel, body, from } => {
            let m = board::request(&channel, &from, &body)?;
            println!("✦ requested {} on #{}", short_id(&m.id), channel);
            println!("  ↳ reply with: cv board reply {channel} {} <body>", m.id);
        }
        BoardCmd::Reply { channel, in_reply_to, body, from } => {
            let m = board::reply(&channel, &from, &in_reply_to, &body)?;
            println!("✦ replied {} to {} on #{}", short_id(&m.id), short_id(&in_reply_to), channel);
        }
        BoardCmd::Replies { channel, request_id } => {
            let msgs = board::replies(&channel, &request_id)?;
            for m in &msgs {
                print_board_msg(m);
            }
            if msgs.is_empty() {
                println!("(no replies to {} on #{channel})", short_id(&request_id));
            }
        }
        BoardCmd::Claim { channel, key, from, ttl_secs } => {
            let lease = board::claim(&channel, &from, &key, Duration::from_secs(ttl_secs))?;
            match lease {
                Some(l) => {
                    println!(
                        "GRANTED  {key} → {} (expires {})",
                        l.owner,
                        l.expires_at.format("%Y-%m-%d %H:%M:%S")
                    );
                }
                None => {
                    let holder = board::active_claims(&channel)?
                        .into_iter()
                        .find(|(k, _, _)| *k == key)
                        .map(|(_, owner, _)| owner)
                        .unwrap_or_else(|| "someone".into());
                    println!("CONTENDED  {key} is held by {holder}");
                    std::process::exit(1);
                }
            }
        }
        BoardCmd::Release { channel, key, from } => {
            // We can only `release` a real `Lease` (its dir/channel fields are private). But we must
            // not *acquire* a free key just to drop it — that would print "released" for something we
            // never held. So first confirm `from` actually owns a live claim on `key`; only then
            // re-claim (which renews our own lease) and release it.
            let owned = board::active_claims(&channel)?
                .into_iter()
                .any(|(k, owner, _)| k == key && owner == from);
            if !owned {
                println!("(you don't hold {key} on #{channel} — nothing to release)");
            } else {
                match board::claim(&channel, &from, &key, Duration::from_secs(1))? {
                    Some(lease) => {
                        board::release(&lease)?;
                        println!("✦ released {key} on #{channel}");
                    }
                    None => {
                        println!("(claim {key} on #{channel} is held by another agent — nothing to release)");
                    }
                }
            }
        }
        BoardCmd::Claims { channel } => {
            let claims = board::active_claims(&channel)?;
            for (key, owner, expires) in &claims {
                println!(
                    "{key:30}  {owner:16}  expires {}",
                    expires.format("%Y-%m-%d %H:%M:%S")
                );
            }
            if claims.is_empty() {
                println!("(no active claims on #{channel})");
            }
        }
        BoardCmd::Who { channel, within_secs } => {
            // Record our own presence first, then list recent heartbeaters.
            board::heartbeat(&channel, "cv")?;
            let agents = board::who(&channel, Duration::from_secs(within_secs))?;
            for a in &agents {
                println!("{a}");
            }
            if agents.is_empty() {
                println!("(nobody seen on #{channel} in the last {within_secs}s)");
            }
        }
        BoardCmd::Ack { channel, message_id, from } => {
            let m = board::ack(&channel, &from, &message_id)?;
            println!("✦ acked {} on #{channel}", short_id(&message_id));
            let _ = m;
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

fn cmd_index(semantic: bool, rebuild: bool) -> Result<()> {
    eprintln!(
        "✦ {} full-text index…",
        if rebuild { "rebuilding" } else { "updating" }
    );
    let n = cv_search::index_all(None, rebuild)?;
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
    println!("events: extracted on the same pass → try `cv events <id>` / `cv touched <path>`");
    Ok(())
}

// ---------- events / touched ----------

fn cmd_events(id: &str, harness: Option<String>, kind: Option<String>) -> Result<()> {
    use cv_core::events;
    let want = parse_harness(&harness)?;
    let (r, _adapter) =
        cv_core::find(id, want)?.with_context(|| format!("no session matching {id:?}"))?;

    // Ensure this one session's events are current (cheap: a single streamed pass); a session
    // already cataloged at this mtime is a no-op.
    if events::needs_ingest(&r, events::file_mtime_ns(&r.path)) {
        events::ingest_ref(&r)?;
    }

    let rows = events::events_for(r.harness.as_str(), &r.id, kind.as_deref());
    if rows.is_empty() {
        match &kind {
            Some(k) => println!("no {k:?} events in {} (try without --kind)", short_id(&r.id)),
            None => println!("no tool events in {} (a chat-only session?)", short_id(&r.id)),
        }
        return Ok(());
    }

    println!(
        "{} event(s) in {}:{}\n",
        rows.len(),
        r.harness.as_str(),
        short_id(&r.id)
    );
    for e in &rows {
        let time = e
            .ts
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map(|d| d.format("%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "-----------".into());
        println!(
            "{:>5}  {:11}  {:9}  {:14}  {}",
            e.msg_idx,
            time,
            e.kind,
            e.tool.as_deref().unwrap_or("-"),
            e.target.as_deref().map(|t| truncate(t, 90)).unwrap_or_default(),
        );
        if let Some(d) = &e.detail {
            println!("       ↳ {}", truncate(d, 100));
        }
    }
    Ok(())
}

fn cmd_touched(path: &str, edits_only: bool) -> Result<()> {
    let rows = cv_core::events::sessions_touching(path, edits_only);
    if rows.is_empty() {
        println!(
            "no sessions {} {path:?} — events are ingested by `cv index` (run it first?)",
            if edits_only { "edited" } else { "touched" }
        );
        return Ok(());
    }
    println!("{} session(s) touched {path:?}:\n", rows.len());
    for t in &rows {
        let date = t
            .last_ts
            .and_then(|s| chrono::DateTime::from_timestamp(s, 0))
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "----------".into());
        let counts = match (t.edits, t.reads) {
            (0, n) => format!("{n} read(s)"),
            (n, 0) => format!("{n} edit(s)"),
            (e, r) => format!("{e} edit(s), {r} read(s)"),
        };
        println!(
            "{:8}  {:8}  {:10}  {:22}  {}",
            t.harness,
            short_id(&t.session_id),
            date,
            counts,
            t.title.as_deref().map(|s| truncate(s, 56)).unwrap_or_default(),
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
        Harness::Kimi => ("kimi".into(), vec!["--resume".into(), id.into()]),
        Harness::Qwen => ("qwen".into(), vec!["--resume".into(), id.into()]),
        // Desktop/IDE apps (and any future harness) have no documented CLI resume.
        _ => (
            format!("# no CLI resume for {h}; open the app and find the session"),
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

fn cmd_ls(harness: Option<String>, cwd: Option<String>, limit: usize, sort_by: &str) -> Result<()> {
    let want = parse_harness(&harness)?;
    let all = cv_core::discover_all();
    let discovered = all.len();
    let mut refs: Vec<SessionRef> = all
        .into_iter()
        .filter(|r| want.is_none_or(|h| r.harness == h))
        .filter(|r| match &cwd {
            None => true,
            Some(c) => r
                .cwd
                .as_ref()
                .is_some_and(|p| p.to_string_lossy().contains(c)),
        })
        .collect();

    match sort_by {
        "created" => refs.sort_by_key(|r| std::cmp::Reverse(r.created_at.or(r.updated_at))),
        "messages" => refs.sort_by_key(|r| std::cmp::Reverse(r.message_count)),
        // "updated" (the default; clap's value_parser admits nothing else)
        _ => refs.sort_by_key(|r| std::cmp::Reverse(r.updated_at.or(r.created_at))),
    }

    let total = refs.len();
    if total < discovered {
        println!("{total} session(s) (of {discovered} discovered; filtered)\n");
    } else {
        println!("{total} session(s)\n");
    }
    for r in refs.iter().take(limit) {
        println!(
            "{:8}  {:8}  {:10}  {:>4} msg  {}",
            r.harness.as_str(),
            short_id(&r.id),
            r.updated_at
                .or(r.created_at)
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

    // A retired sqlite FTS index may still be sitting on disk from older versions; tantivy is
    // canonical now, so let the user know it's safe to remove.
    if let Some(legacy) = legacy_sqlite_index_path() {
        if legacy.exists() {
            eprintln!(
                "(note: legacy sqlite index no longer used; safe to delete {})",
                legacy.display()
            );
        }
    }

    // Preferred path: the tantivy full-text index (real tokenization + BM25). Authoritative when
    // present — an empty result means "no match", not "fall back to a live scan".
    if cv_search::default_tantivy_dir().exists() {
        match cv_search::text_search(None, query, limit.saturating_mul(4)) {
            Ok(hits) => {
                render_search_hits(&hits, want, limit, query, "index");
                return Ok(());
            }
            Err(e) => eprintln!("(tantivy index unavailable: {e:#}; scanning live)"),
        }
    } else {
        eprintln!("(no index yet — scanning live; run `cv index` for instant search)");
    }
    cmd_search_live(query, want, limit)
}

/// Where the retired sqlite FTS index used to live: `$CLAURDVOYANT_HOME/index.sqlite` or
/// `~/.claurdvoyant/index.sqlite`. Only used to nudge cleanup of a stale file.
fn legacy_sqlite_index_path() -> Option<PathBuf> {
    std::env::var_os("CLAURDVOYANT_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs_home().map(|h| h.join(".claurdvoyant")))
        .map(|d| d.join("index.sqlite"))
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
    let rows: Vec<&cv_search::Hit> = hits
        .iter()
        .filter(|h| want.is_none_or(|w| h.harness == w.as_str()))
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
        // Dates ride on the hit straight from the index (FTS); semantic hits carry none.
        let date = h
            .updated_at
            .or(h.created_at)
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
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

fn cmd_search_live(query: &str, want: Option<Harness>, limit: usize) -> Result<()> {
    use cv_core::{Flow, Message, MessageSink, ParseOptions, Role};
    let needle = query.to_lowercase();
    let mut hits = 0;

    // A sink that builds one session's lowercased searchable haystack as messages stream past (each
    // dropped immediately), and grabs the first user text for the label fallback. Peak per session is
    // O(haystack) instead of O(whole Session) + a separate searchable_text copy + a lowercase copy.
    struct HaySink {
        hay: String,
        first_user: Option<String>,
        resolver: cv_core::Resolver,
    }
    impl MessageSink for HaySink {
        fn message(&mut self, mut m: Message) -> Flow {
            m.materialize(&self.resolver);
            if self.first_user.is_none() && m.role == Role::User {
                if let Some(t) = m.text() {
                    if !t.trim().is_empty() {
                        self.first_user = Some(t);
                    }
                }
            }
            let mut piece = String::new();
            cv_core::stream::append_searchable(&mut piece, &m);
            self.hay.push_str(&piece.to_lowercase());
            Flow::Continue
        }
    }

    for adapter in cv_core::harness::all() {
        if want.is_some_and(|h| adapter.harness() != h) || adapter.storage_root().is_none() {
            continue;
        }
        for r in adapter.discover()? {
            let mut sink = HaySink { hay: String::new(), first_user: None, resolver: cv_core::Resolver::new(Some(r.path.clone())) };
            let meta = match adapter.stream(&r, &ParseOptions::bulk(), &mut sink) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if let Some(pos) = sink.hay.find(&needle) {
                hits += 1;
                let label = cv_core::label_from(meta.title.as_deref(), sink.first_user.as_deref());
                println!(
                    "{:8}  {:8}  {:10}  {}",
                    r.harness.as_str(),
                    short_id(&r.id),
                    r.updated_at
                        .map(|d| d.format("%Y-%m-%d").to_string())
                        .unwrap_or_else(|| "----------".into()),
                    label,
                );
                println!("          … {}", snippet(&sink.hay, pos, needle.len()));
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

fn cmd_show(id: &str, harness: Option<String>, json: bool, range: Option<String>) -> Result<()> {
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

/// Parse `<start>-<end>` | `<start>-` | `-<end>` | `<start>` into a 0-based, end-exclusive
/// `(start, Option<end>)` window. `<start>` alone means just that single message.
fn parse_msg_range(spec: &str) -> Result<(usize, Option<usize>)> {
    let spec = spec.trim();
    match spec.split_once('-') {
        None => {
            let n: usize = spec
                .parse()
                .with_context(|| format!("bad range {spec:?}: expected <start>-<end>, <start>-, -<end>, or <n>"))?;
            Ok((n, Some(n + 1)))
        }
        Some((s, e)) => {
            let start = if s.trim().is_empty() {
                0
            } else {
                s.trim().parse().with_context(|| format!("bad range {spec:?}: start must be a number"))?
            };
            let end = if e.trim().is_empty() {
                None
            } else {
                Some(e.trim().parse().with_context(|| format!("bad range {spec:?}: end must be a number"))?)
            };
            if let Some(end) = end {
                if end < start {
                    bail!("bad range {spec:?}: end ({end}) is before start ({start})");
                }
            }
            Ok((start, end))
        }
    }
}

fn cmd_export(id: &str, format: &str, harness: Option<String>) -> Result<()> {
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

fn cmd_dataset(
    format: &str,
    harness: Option<String>,
    limit: Option<usize>,
    min_messages: usize,
    redact: bool,
    out: Option<PathBuf>,
) -> Result<()> {
    use std::io::Write;
    if !matches!(format, "chatml" | "sharegpt") {
        bail!("unknown format {format:?} (use chatml or sharegpt)");
    }
    let want = parse_harness(&harness)?;
    let mut writer: Box<dyn Write> = match &out {
        Some(p) => Box::new(std::io::BufWriter::new(fs::File::create(p)?)),
        None => Box::new(std::io::BufWriter::new(std::io::stdout())),
    };

    // Stream the corpus one session at a time (parse -> emit -> drop) so memory stays bounded.
    let mut emitted = 0usize;
    let mut skipped = 0usize;
    for r in cv_core::discover_all() {
        if let Some(w) = want {
            if r.harness != w {
                continue;
            }
        }
        let Some(adapter) = cv_core::harness::for_harness(r.harness) else {
            continue;
        };
        // Lazy opts: large content stays a span (held as a 16-byte ref), and `write_record` streams
        // it straight to the writer in chunks — so even a 700 MB record never materializes. `extra`
        // isn't read by dataset rendering, so it's skipped too.
        let session = match cv_core::stream::collect_with(adapter.as_ref(), &r, &cv_core::ParseOptions::lazy()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cv dataset: skipping {} ({}): {e:#}", r.id, r.harness);
                continue;
            }
        };
        if session.messages.len() < min_messages {
            skipped += 1;
            continue;
        }
        // Stream the record directly to the writer (chunked, JSON-escaped per chunk — never holds the
        // whole record). `--redact` scrubs one materialized message at a time inside the writer.
        let fmt = match format {
            "chatml" => cv_core::dataset::Format::Chatml,
            _ => cv_core::dataset::Format::ShareGpt,
        };
        let redact_opts = redact.then(cv_core::redact::RedactOptions::default);
        let wrote = cv_core::dataset::write_record(&session, &mut writer, fmt, redact_opts.as_ref())?;
        if wrote {
            writeln!(writer)?;
            emitted += 1;
            if let Some(l) = limit {
                if emitted >= l {
                    break;
                }
            }
        } else {
            skipped += 1;
        }
    }
    writer.flush()?;
    let dest = out
        .as_ref()
        .map(|p| format!(" to {}", p.display()))
        .unwrap_or_default();
    eprintln!("✦ {format}: wrote {emitted} record(s){dest} ({skipped} skipped)");
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

// ---------- timeline ----------

/// A unified chronological feed across all harnesses (oldest → newest, like a feed). Grouped by day.
fn cmd_timeline(harness: Option<String>, cwd: Option<String>, limit: usize) -> Result<()> {
    let want = parse_harness(&harness)?;
    let mut refs: Vec<SessionRef> = cv_core::discover_all()
        .into_iter()
        .filter(|r| want.is_none_or(|h| r.harness == h))
        .filter(|r| match &cwd {
            None => true,
            Some(c) => r
                .cwd
                .as_ref()
                .map(|p| p.to_string_lossy().contains(c))
                .unwrap_or(false),
        })
        .collect();

    // Sort ascending by updated_at (falling back to created_at), so the feed reads oldest → newest.
    let key = |r: &SessionRef| r.updated_at.or(r.created_at);
    refs.sort_by_key(key);

    let total = refs.len();
    // A feed shows the *most recent* window; keep the last `limit` rows but still oldest → newest.
    let shown = if total > limit { &refs[total - limit..] } else { &refs[..] };
    if total > limit {
        println!("… {} older (use --limit)\n", total - limit);
    }

    let mut last_day: Option<String> = None;
    for r in shown {
        let when = key(r);
        let day = when
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "----------".into());
        if last_day.as_deref() != Some(day.as_str()) {
            println!("── {day} ──");
            last_day = Some(day.clone());
        }
        let time = when
            .map(|d| d.format("%H:%M").to_string())
            .unwrap_or_else(|| "--:--".into());
        let title = r
            .title
            .clone()
            .map(|t| truncate(&t, 50))
            .unwrap_or_else(|| dim_cwd(r.cwd.as_deref()));
        println!(
            "  {}  {:8}  {:8}  {:24}  {}",
            time,
            r.harness.as_str(),
            short_id(&r.id),
            truncate(&dim_cwd(r.cwd.as_deref()), 24),
            title,
        );
    }
    println!("\n{total} session(s)");
    Ok(())
}

// ---------- diff ----------

/// Compare two sessions message-by-message: a shared prefix (`=`) then a divergence marked
/// `<` (only in A) / `>` (only in B). Comparison is on role + `Message::text()`.
fn cmd_diff(a: &str, b: &str, harness: Option<String>) -> Result<()> {
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

// ---------- splice / loom ----------

/// A parsed splice spec: which session, and a `[start, end)` window (end None → through the last).
struct SpliceSpec {
    id: String,
    start: usize,
    end: Option<usize>,
}

/// Parse `<id>:<start>-<end>` | `<id>:<start>-` | `<id>` into a [`SpliceSpec`].
fn parse_splice_spec(spec: &str) -> Result<SpliceSpec> {
    match spec.split_once(':') {
        None => Ok(SpliceSpec { id: spec.to_string(), start: 0, end: None }),
        Some((id, range)) => {
            let (s, e) = range
                .split_once('-')
                .with_context(|| format!("bad spec {spec:?}: range must be <start>-<end> or <start>-"))?;
            let start: usize = s
                .trim()
                .parse()
                .with_context(|| format!("bad spec {spec:?}: start must be a number"))?;
            let end = if e.trim().is_empty() {
                None
            } else {
                Some(
                    e.trim()
                        .parse()
                        .with_context(|| format!("bad spec {spec:?}: end must be a number"))?,
                )
            };
            Ok(SpliceSpec { id: id.to_string(), start, end })
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_splice(
    specs: &[String],
    to: Option<String>,
    out: Option<PathBuf>,
    export: Option<String>,
    cwd: Option<PathBuf>,
    generate: bool,
    gen_model: Option<String>,
) -> Result<()> {
    let parsed: Vec<SpliceSpec> = specs.iter().map(|s| parse_splice_spec(s)).collect::<Result<_>>()?;

    // Resolve + parse each spec's session FIRST, keeping the owners alive in a Vec so the spans can
    // borrow them. We pair each parsed session with the (start, end) it'll select.
    let mut owned: Vec<(Session, usize, Option<usize>)> = Vec::with_capacity(parsed.len());
    for sp in &parsed {
        let (r, adapter) =
            cv_core::find(&sp.id, None)?.with_context(|| format!("no session matching {:?}", sp.id))?;
        let session = adapter.parse(&r)?;
        owned.push((session, sp.start, sp.end));
    }

    let spans: Vec<cv_core::loom::Span<'_>> = owned
        .iter()
        .map(|(s, start, end)| cv_core::loom::Span { source: s, start: *start, end: *end })
        .collect();

    // Target harness: --to, else the first spec's source harness.
    let to_h = match &to {
        Some(s) => Harness::parse(s).with_context(|| format!("unknown target harness: {s}"))?,
        None => owned
            .first()
            .map(|(s, _, _)| s.harness)
            .context("splice needs at least one session")?,
    };

    let spliced = cv_core::loom::splice(&spans, None, to_h);
    finish_composed(spliced, to_h, to.is_some(), out, export, cwd, generate, gen_model)
}

#[allow(clippy::too_many_arguments)]
fn cmd_loom(
    base: &str,
    at: usize,
    graft: &str,
    from: usize,
    to: Option<String>,
    out: Option<PathBuf>,
    export: Option<String>,
    cwd: Option<PathBuf>,
    generate: bool,
    gen_model: Option<String>,
) -> Result<()> {
    let (rb, ab) = cv_core::find(base, None)?.with_context(|| format!("no session matching {base:?}"))?;
    let (rg, ag) = cv_core::find(graft, None)?.with_context(|| format!("no session matching {graft:?}"))?;
    let base_s = ab.parse(&rb)?;
    let graft_s = ag.parse(&rg)?;

    let grafted = cv_core::loom::graft(&base_s, at, &graft_s, from, None);
    // Default target harness is the base's (graft() already used it); honor --to if given.
    let to_h = match &to {
        Some(s) => Harness::parse(s).with_context(|| format!("unknown target harness: {s}"))?,
        None => base_s.harness,
    };
    // Re-stamp the harness if --to overrode it (graft used base.harness).
    let mut grafted = grafted;
    grafted.harness = to_h;
    finish_composed(grafted, to_h, to.is_some(), out, export, cwd, generate, gen_model)
}

/// Shared tail for splice/loom: emit to a harness when `--to`/`--out` is in play, otherwise print a
/// summary and (with `--export md|json`) the composed session to stdout.
#[allow(clippy::too_many_arguments)]
fn finish_composed(
    mut session: Session,
    to_h: Harness,
    explicit_to: bool,
    out: Option<PathBuf>,
    export: Option<String>,
    cwd: Option<PathBuf>,
    generate: bool,
    gen_model: Option<String>,
) -> Result<()> {
    // The generative half of looming: grow the composed branch with an LLM-generated continuation.
    if generate {
        match cv_llm::available_provider() {
            Some(prov) => eprintln!("✦ looming a continuation via {prov}…"),
            None => bail!(
                "--generate needs an LLM provider: set OPENROUTER_API_KEY, ANTHROPIC_API_KEY, or \
                 LMSTUDIO_API_BASE=local (free local LM Studio)"
            ),
        }
        let text = cv_llm::generate(
            &session,
            &cv_llm::GenerateOptions { model: gen_model, ..Default::default() },
        )?;
        let mut m = Message::new(Role::Assistant);
        m.content.push(Block::Text { text: text.into() });
        session.messages.push(m);
        eprintln!("  ↳ grew the branch by 1 generated turn ({} msg total)", session.messages.len());
    }

    // Emit path: an explicit --to or an --out directory means "materialize this for a harness".
    if explicit_to || out.is_some() {
        if let Some(dir) = &cwd {
            session.cwd = Some(dir.clone());
        }
        return emit_session(&session, to_h, out, EmitOptions { new_cwd: cwd, new_id: None });
    }

    // Otherwise: print a summary, and with --export, dump the composed session to stdout.
    println!(
        "✦ composed {} ({}) · {} msg",
        short_id(&session.id),
        session.harness.as_str(),
        session.messages.len()
    );
    if let Some(prov) = session.extra.get("loom") {
        if let Some(arr) = prov.as_array() {
            for p in arr {
                println!(
                    "  ↳ {} {}[{}..{}]",
                    p.get("harness").and_then(|v| v.as_str()).unwrap_or("?"),
                    p.get("source_id").and_then(|v| v.as_str()).map(short_id).unwrap_or_default(),
                    p.get("start").and_then(|v| v.as_u64()).unwrap_or(0),
                    p.get("end").and_then(|v| v.as_u64()).unwrap_or(0),
                );
            }
        }
    }
    match export.as_deref() {
        None => {
            println!("\n(use --export md|json to print it, or --to <harness> [--out <dir>] to emit it)");
        }
        Some("json") => println!("{}", serde_json::to_string_pretty(&session)?),
        Some("md") | Some("markdown") => print!("{}", cv_core::render::to_markdown(&session)),
        Some(other) => bail!("unknown export format {other:?} (use md or json)"),
    }
    Ok(())
}

// ---------- rendering helpers ----------

/// Everything a streamed renderer needs for a session's header — derived from the cheap
/// [`SessionRef`] (label/cwd) plus the model captured from the first assistant turn.
struct HeaderInfo {
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
fn stream_session_render<W: std::io::Write>(
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
    adapter.stream(r, &ParseOptions::bulk(), &mut sink)?;
    sink.flush_header(); // short sessions (no assistant turn / < HOLDBACK msgs) flush here
    sink.result?;
    Ok(())
}

/// Header for `cv show` (mirrors the old eager header exactly).
fn show_header(h: &HeaderInfo) -> String {
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
fn show_message(m: &Message) -> String {
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

#[cfg(test)]
mod tests {
    use super::parse_msg_range;

    #[test]
    fn msg_range_forms() {
        assert_eq!(parse_msg_range("10-13").unwrap(), (10, Some(13)));
        assert_eq!(parse_msg_range("10-").unwrap(), (10, None));
        assert_eq!(parse_msg_range("-13").unwrap(), (0, Some(13)));
        assert_eq!(parse_msg_range("5").unwrap(), (5, Some(6))); // single message
        assert_eq!(parse_msg_range(" 2 - 4 ").unwrap(), (2, Some(4))); // whitespace-tolerant
    }

    #[test]
    fn msg_range_rejects_inverted_and_garbage() {
        assert!(parse_msg_range("9-3").is_err()); // end before start
        assert!(parse_msg_range("abc").is_err());
        assert!(parse_msg_range("1-x").is_err());
    }
}
