//! `cv` — the claurdvoyant CLI.
//!
//! This file holds only the clap surface (`Cli`/`Cmd`) and the dispatch into the per-command
//! modules under `cmd/`; shared helpers live in `util.rs` and `cv blame` in `blame.rs`.

mod blame;
mod cmd;
mod util;

// Crate-root re-exports for `blame.rs`, which renders conversation windows via the same
// streaming renderer `cv show` uses.
pub(crate) use cmd::view::{show_header, show_message, stream_session_render};
pub(crate) use util::short_id;

use anyhow::Result;
use clap::{Parser, Subcommand};
use cmd::live::BoardCmd;
use cmd::{browse, compose, convert, live, pack, provenance, query, search, share, view};
use std::path::PathBuf;

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
        /// Query calculus to filter rows, e.g. `model:fable`, `harness:codex msgs>=50`,
        /// `after:2026-06-01 widget` (implicit AND; see `cv query`). A `model:` term forces a
        /// parse of each candidate (the catalog doesn't store the model), so pair it with cheap
        /// terms like `harness:`/`cwd:` to keep it fast.
        #[arg(long, short = 'q')]
        query: Option<String>,
        /// Max rows to show.
        #[arg(long, default_value_t = 40)]
        limit: usize,
        /// Sort key: updated (default), created, or messages.
        #[arg(long = "sort-by", default_value = "updated", value_parser = ["updated", "created", "messages"])]
        sort_by: String,
        /// Force a full fleet re-discovery instead of trusting the catalog's staleness probe.
        #[arg(long)]
        fresh: bool,
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
        // allow_hyphen_values: the documented `-<end>` form starts with a dash, which clap
        // would otherwise reject as an unknown flag (`cv show id --range -5`).
        #[arg(long, allow_hyphen_values = true)]
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
        /// Query calculus selecting which sessions to include, e.g. `model:fable`,
        /// `harness:claude msgs>=20`, `after:2026-01-01 cwd:/pug` (implicit AND; see `cv query`).
        #[arg(long, short = 'q')]
        query: Option<String>,
        /// Also emit each Claude session's sub-agent forest (directly- and `Workflow`-spawned
        /// transcripts in sidecar files). These are first-class training data — and a parent on one
        /// model often spawns sub-agents on another, so `model:` queries need this to catch them.
        #[arg(long)]
        subagents: bool,
        /// Stop after N emitted records.
        #[arg(long)]
        limit: Option<usize>,
        /// Skip sessions with fewer than this many messages (drops trivial/empty ones).
        #[arg(long, default_value_t = 2)]
        min_messages: usize,
        /// Scrub secrets/PII from every session before emitting (cv_core::redact).
        #[arg(long)]
        redact: bool,
        /// Scrub only these secret classes (comma list: `private_key`, `api_key`, `jwt`, `email`,
        /// `blob`, `assignment`), leaving everything else intact — e.g. `--redact-only private_key`
        /// to strip PEM blocks but keep identities. Implies redaction; overrides `--redact`.
        #[arg(long, value_name = "CLASSES")]
        redact_only: Option<String>,
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
    /// Redact a session and emit a single self-contained HTML artifact anyone can open.
    Share {
        id: String,
        #[arg(long)]
        harness: Option<String>,
        /// Write here instead of `./<id>.html`.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Skip the redaction pass (default is to scrub secrets/PII before sharing).
        #[arg(long)]
        no_redact: bool,
    },
    /// Compile a context bundle for a new task from your whole session corpus.
    Pack {
        /// What you're about to work on — recall finds the relevant past spans.
        task: String,
        /// Output shape: `md` (CLAUDE.md-style digest, default), `prompt` (system prompt),
        /// or `session` (a synthetic resumable session — see `--to`).
        #[arg(long, default_value = "md")]
        format: String,
        /// Target harness when `--format session`.
        #[arg(long)]
        to: Option<String>,
        /// Max past sessions to draw from.
        #[arg(long, default_value_t = 8)]
        limit: usize,
        #[arg(long)]
        out: Option<PathBuf>,
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
    Stats {
        /// Restrict the analytics to sessions matching this query (see `cv query`).
        #[arg(long, short = 'q')]
        query: Option<String>,
    },
    /// The query-calculus reference: every field, operator, and example. `--json` emits the machine
    /// schema. The `-q` flag on ls/dataset/timeline/stats speaks this language.
    Query {
        /// Emit the machine-readable schema (fields, types, operators) as JSON instead of the
        /// human reference.
        #[arg(long)]
        json: bool,
    },
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
        /// Restrict the feed to sessions matching this query (see `cv query`).
        #[arg(long, short = 'q')]
        query: Option<String>,
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Ls { harness, cwd, query, limit, sort_by, fresh } => browse::cmd_ls(harness, cwd, query, limit, &sort_by, fresh),
        Cmd::Search { query, harness, limit, semantic } => search::cmd_search(&query, harness, limit, semantic),
        Cmd::Show { id, harness, json, range } => view::cmd_show(&id, harness, json, range),
        Cmd::Export { id, format, harness } => view::cmd_export(&id, &format, harness),
        Cmd::Dataset { format, harness, query, subagents, limit, min_messages, redact, redact_only, out } => {
            compose::cmd_dataset(&format, harness, query, subagents, limit, min_messages, redact, redact_only, out)
        }
        Cmd::Convert { id, to, from, out, cwd } => convert::cmd_convert(&id, &to, from, out, cwd),
        Cmd::Port { id, to, from, to_dir, out, no_context } => convert::cmd_port(&id, to, from, to_dir, out, no_context),
        Cmd::Scry { harness, cwd, interval, existing } => live::cmd_scry(harness, cwd, interval, existing),
        Cmd::Index { semantic, rebuild } => search::cmd_index(semantic, rebuild),
        Cmd::Events { id, harness, kind } => provenance::cmd_events(&id, harness, kind),
        Cmd::Touched { path, edits_only } => provenance::cmd_touched(&path, edits_only),
        Cmd::Blame { file, lines, show } => blame::cmd_blame(&file, lines.as_deref(), show),
        Cmd::Share { id, harness, out, no_redact } => share::cmd_share(&id, harness, out, no_redact),
        Cmd::Pack { task, format, to, limit, out } => pack::cmd_pack(&task, &format, to, limit, out),
        Cmd::Stats { query } => browse::cmd_stats(query),
        Cmd::Query { json } => query::cmd_query(json),
        Cmd::Resume { id, harness, launch } => convert::cmd_resume(&id, harness, launch),
        Cmd::Tree { id, harness } => view::cmd_tree(&id, harness),
        Cmd::Board { action } => live::cmd_board(action),
        Cmd::Timeline { harness, cwd, query, limit } => browse::cmd_timeline(harness, cwd, query, limit),
        Cmd::Diff { a, b, harness } => view::cmd_diff(&a, &b, harness),
        Cmd::Splice { specs, to, out, export, cwd, generate, gen_model } => compose::cmd_splice(&specs, to, out, export, cwd, generate, gen_model),
        Cmd::Distill { id, harness, model, project, out, append } => {
            compose::cmd_distill(&id, harness, model, project, out, append)
        }
        Cmd::Recall { query, k, harness } => search::cmd_recall(&query, k, harness),
        Cmd::Redact { id, harness, format, stats } => view::cmd_redact(&id, harness, &format, stats),
        Cmd::Loom { base, at, graft, from, to, out, export, cwd, generate, gen_model } => {
            compose::cmd_loom(&base, at, &graft, from, to, out, export, cwd, generate, gen_model)
        }
    }
}
