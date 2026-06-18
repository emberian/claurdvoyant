//! Demo CLI for cv-search:
//!
//!   cv-search index              # (re)build the full-text index over all sessions
//!   cv-search text  <query>      # full-text query (BM25 + highlighted snippets)
//!   cv-search embed              # embed all sessions for semantic search  (feature = semantic)
//!   cv-search semantic <query>   # nearest-neighbour query by meaning      (feature = semantic)

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cv-search", about = "Full-text + semantic search over claurdvoyant sessions")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Discover, parse, and (re)build the full-text index.
    Index {
        /// Also fold each session's sub-agent forest into the index (can add ~900MB).
        #[arg(long)]
        subagents: bool,
    },
    /// Full-text query. Supports `harness:claude`, "phrases", AND/OR.
    Text {
        query: Vec<String>,
        #[arg(short, long, default_value_t = 10)]
        limit: usize,
    },
    /// Embed every session for semantic search.
    #[cfg(feature = "semantic")]
    Embed,
    /// Semantic (vector) query.
    #[cfg(feature = "semantic")]
    Semantic {
        query: Vec<String>,
        #[arg(short, long, default_value_t = 10)]
        k: usize,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Index { subagents } => {
            let n = cv_search::index_all(None, false, subagents)?;
            println!(
                "indexed {n} sessions into {}",
                cv_search::default_tantivy_dir().display()
            );
        }
        Cmd::Text { query, limit } => {
            let q = query.join(" ");
            let hits = cv_search::text_search(None, &q, limit)?;
            print_hits(&hits);
        }
        #[cfg(feature = "semantic")]
        Cmd::Embed => {
            let n = cv_search::embed_all(None)?;
            println!(
                "embedded {n} sessions into {}",
                cv_search::default_embeddings_path().display()
            );
        }
        #[cfg(feature = "semantic")]
        Cmd::Semantic { query, k } => {
            let q = query.join(" ");
            let hits = cv_search::semantic_search(None, &q, k)?;
            print_hits(&hits);
        }
    }
    Ok(())
}

fn print_hits(hits: &[cv_search::Hit]) {
    if hits.is_empty() {
        println!("(no matches)");
        return;
    }
    for h in hits {
        println!(
            "[{:.3}] {} {} — {}",
            h.score,
            h.harness,
            &h.id[..h.id.len().min(12)],
            h.title.as_deref().unwrap_or("(untitled)")
        );
        if let Some(cwd) = &h.cwd {
            println!("        cwd: {cwd}");
        }
        if !h.snippet.is_empty() {
            println!("        {}", h.snippet.replace('\n', " "));
        }
    }
}
