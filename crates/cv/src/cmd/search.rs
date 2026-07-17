//! `cv search` / `cv recall` / `cv index` — full-text and semantic search.

use crate::util::{dirs_home, home_rel, parse_harness, short_id};
use anyhow::{Context, Result};
use cv_core::ir::{truncate, Harness};
use cv_core::sanitize::sanitize_line;
use std::path::{Path, PathBuf};

pub(crate) fn cmd_search(
    query: &str,
    harness: Option<String>,
    limit: usize,
    semantic: bool,
    json: bool, // emit the hits as one JSON array (camelCase fields, full ids) instead of the table
) -> Result<()> {
    let want = parse_harness(&harness)?;

    // Semantic search: embed the query and rank stored vectors. Requires `cv index --semantic`.
    if semantic {
        let hits = cv_search::semantic_search(None, query, limit.saturating_mul(4))
            .context("semantic search failed (run `cv index --semantic` first?)")?;
        render_search_hits(&hits, want, limit, query, "semantic", json)?;
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
                render_search_hits(&hits, want, limit, query, "index", json)?;
                return Ok(());
            }
            Err(e) => eprintln!("(tantivy index unavailable: {e:#}; scanning live)"),
        }
    } else {
        eprintln!("(no index yet — scanning live; run `cv index` for instant search)");
    }
    cmd_search_live(query, want, limit, json)
}

/// Where the retired sqlite FTS index used to live: `$CLUSTERVISION_HOME/index.sqlite` or
/// `~/.clustervision/index.sqlite`. Only used to nudge cleanup of a stale file.
fn legacy_sqlite_index_path() -> Option<PathBuf> {
    std::env::var_os("CLUSTERVISION_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs_home().map(|h| h.join(".clustervision")))
        .map(|d| d.join("index.sqlite"))
}

/// Render a slice of cv-search [`cv_search::Hit`]s with harness/short-id/date/title/snippet,
/// applying the `--harness` filter and `--limit`. `source` labels the empty-result hint.
/// With `json`, the SAME hits in the same order go to stdout as one JSON array instead —
/// no hint lines, an empty result is `[]` — so the output pipes cleanly.
fn render_search_hits(
    hits: &[cv_search::Hit],
    want: Option<Harness>,
    limit: usize,
    query: &str,
    source: &str,
    json: bool,
) -> Result<()> {
    let rows: Vec<&cv_search::Hit> = hits
        .iter()
        .filter(|h| want.is_none_or(|w| h.harness == w.as_str()))
        .take(limit)
        .collect();
    if json {
        // Machine-readable hits: camelCase fields, the FULL session id (the table shows an
        // 8-char prefix), the untruncated snippet, and the index's relevance score (BM25 for
        // FTS, cosine similarity for --semantic). Timestamps are ISO-8601 UTC, null when the
        // index carries none (e.g. semantic hits from a pre-dates embedding store).
        let vals: Vec<serde_json::Value> = rows.iter().map(|h| hit_json(h)).collect();
        println!("{}", serde_json::to_string_pretty(&vals)?);
        return Ok(());
    }
    if rows.is_empty() {
        let hint = if source == "semantic" {
            "(semantic; run `cv index --semantic` to (re)build embeddings)"
        } else {
            "(index; try `cv index` to refresh)"
        };
        println!("no matches for {query:?} {hint}");
        // The likely reason a recent conversation isn't findable: the index has fallen behind.
        if source == "index" {
            if let Some(days) = index_days_behind() {
                println!("(note: the index is ~{days} day(s) behind the newest session — run `cv index`)");
            }
            // The other common miss: the wanted text lives in a *sub-agent* transcript, which the
            // default index doesn't fold in. Say so — a search that came back empty for something an
            // Agent/Workflow lane discussed is exactly this case.
            if !cv_search::fts::has_subagent_docs(&cv_search::default_tantivy_dir()) {
                println!(
                    "(note: sub-agent transcripts aren't searched yet — rebuild with \
                     `cv index --subagents` to include the Agent/Workflow lanes)"
                );
            }
        }
        return Ok(());
    }
    for h in rows {
        // Dates ride on the hit straight from the index (FTS); semantic hits carry none.
        let date = h
            .updated_at
            .or(h.created_at)
            .and_then(|t| crate::util::fmt_local_ts(t, "%Y-%m-%d"))
            .unwrap_or_else(|| "----------".into());
        // A folded-in sub-agent hit (`cv index --subagents`): its own id is `agent-<hex>`, so a
        // bare `short_id` would render the shared `agent-` prefix, useless for drill-in. Show the
        // bare agentId (what `cv show <id>` resolves) and tag the row with its parent + workflow so
        // the reader sees it's a lane, not a top-level session.
        let (id_disp, provenance) = match h.agent_id.as_deref() {
            Some(aid) => {
                let parent = h.parent_id.as_deref().map(short_id).unwrap_or_default();
                let wf = h.workflow.as_deref().map(|w| format!(" · ⟐{w}")).unwrap_or_default();
                (short_id(aid), format!("   ⤷ sub-agent of {parent}{wf}"))
            }
            None => (short_id(&h.id), String::new()),
        };
        // Titles and snippets are transcript-derived (untrusted) — sanitize at the terminal
        // seam (G5); JSON surfaces stay raw.
        println!(
            "{:8}  {:8}  {:10}  {}{}",
            h.harness,
            id_disp,
            date,
            sanitize_line(h.title.as_deref().unwrap_or_default()),
            provenance,
        );
        if !h.snippet.trim().is_empty() {
            println!("          … {}", truncate(&sanitize_line(&h.snippet), 120));
        }
    }
    Ok(())
}

/// One `--json` object for an index/semantic hit: what the table row shows, machine-readably —
/// plus the full id, cwd, and score the table drops or truncates.
fn hit_json(h: &cv_search::Hit) -> serde_json::Value {
    serde_json::json!({
        "harness": h.harness,
        "id": h.id,
        "cwd": h.cwd,
        "title": h.title,
        "score": h.score,
        "snippet": h.snippet,
        "createdAt": h.created_at.and_then(|t| chrono::DateTime::from_timestamp(t, 0)).map(|d| d.to_rfc3339()),
        "updatedAt": h.updated_at.and_then(|t| chrono::DateTime::from_timestamp(t, 0)).map(|d| d.to_rfc3339()),
    })
}

/// Whole days the FTS index lags the newest session file on disk, when ≥ 1. Cheap enough for the
/// no-matches path it decorates: one stored-field scan of the index (the newest indexed mtime
/// stamp) plus a metadata-only discovery sweep (a stat per session, no parsing).
fn index_days_behind() -> Option<u64> {
    const DAY_NS: i64 = 86_400 * 1_000_000_000;
    let indexed = cv_search::fts::newest_indexed_mtime(&cv_search::default_tantivy_dir())?;
    let newest_on_disk = cv_core::discover_all()
        .iter()
        .map(|r| cv_core::offsets::file_sig(&r.path).0)
        .max()?;
    let days = newest_on_disk.saturating_sub(indexed) / DAY_NS;
    (days >= 1).then_some(days as u64)
}

fn cmd_search_live(query: &str, want: Option<Harness>, limit: usize, json: bool) -> Result<()> {
    use cv_core::ParseOptions;
    let needle = query.to_lowercase();
    let mut hits = 0;
    // --json: accumulate the SAME hits the table would print (same order, same snippet), emitted
    // as one array at the end. No live-scan score exists, so `score` is an explicit null; ids are
    // full (the table truncates to 8 chars).
    let mut rows: Vec<serde_json::Value> = Vec::new();

    // Streams each session into pack's head-capped `CapSink` under a lazy parse: peak per session
    // is O(LIVE_HAY_BYTES), never O(session) — this is exactly the first-run (no index yet) path,
    // where the old bulk parse materialized every message plus a lowercased copy of the whole
    // transcript (multi-GB RSS on a big corpus). Trade-off: a match beyond the capped head is
    // missed here; finding those is the index's job (`cv index`).
    for adapter in cv_core::harness::all() {
        if want.is_some_and(|h| adapter.harness() != h) || adapter.storage_root().is_none() {
            continue;
        }
        for r in adapter.discover()? {
            let mut sink = super::pack::CapSink::new(&r.path);
            let meta = match adapter.stream(&r, &ParseOptions::lazy(), &mut sink) {
                Ok(m) => m,
                Err(_) => continue,
            };
            // Lowercase only the bounded head (the match check), windowing the snippet from the
            // original-case haystack.
            let low = sink.hay.to_lowercase();
            if let Some(pos) = low.find(&needle) {
                hits += 1;
                let label = cv_core::label_from(meta.title.as_deref(), sink.first_user.as_deref());
                let snip = snippet(&sink.hay, pos.min(sink.hay.len()), needle.len());
                if json {
                    rows.push(serde_json::json!({
                        "harness": r.harness.as_str(),
                        "id": r.id,
                        "cwd": r.cwd.as_ref().map(|p| p.to_string_lossy()),
                        "title": label,
                        "score": serde_json::Value::Null,
                        "snippet": snip,
                        "createdAt": r.created_at.map(|t| t.to_rfc3339()),
                        "updatedAt": r.updated_at.map(|t| t.to_rfc3339()),
                    }));
                } else {
                    println!(
                        "{:8}  {:8}  {:10}  {}",
                        r.harness.as_str(),
                        short_id(&r.id),
                        r.updated_at
                            .map(|d| crate::util::fmt_local(d, "%Y-%m-%d"))
                            .unwrap_or_else(|| "----------".into()),
                        sanitize_line(&label),
                    );
                    println!("          … {}", sanitize_line(&snip));
                }
                if hits >= limit {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&rows)?);
                        eprintln!("(stopped at {limit} hits; use --limit)");
                    } else {
                        println!("\n(stopped at {limit} hits; use --limit)");
                    }
                    return Ok(());
                }
            }
        }
    }
    if json {
        // Pure JSON on stdout even for a miss: an empty array, no prose.
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if hits == 0 {
        println!("no matches for {query:?}");
    }
    Ok(())
}

pub(crate) fn cmd_recall(query: &str, k: usize, harness: Option<String>) -> Result<()> {
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
            truncate(&sanitize_line(hit.title.as_deref().unwrap_or_default()), 50),
            truncate(&sanitize_line(&cwd), 40),
        );
        let excerpt = recall_excerpt(hit, query);
        for line in excerpt.lines() {
            println!("      {}", sanitize_line(line));
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
        let score = words.iter().filter(|w| t.contains(**w)).count() as i64 + if t.contains(&needle) { 5 } else { 0 };
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

pub(crate) fn cmd_index(semantic: bool, rebuild: bool, subagents: bool) -> Result<()> {
    eprintln!(
        "✦ {} full-text index{}…",
        if rebuild { "rebuilding" } else { "updating" },
        if subagents { " (+ sub-agent forest)" } else { "" }
    );
    let n = cv_search::index_all(None, rebuild, subagents)?;
    println!(
        "indexed {n} top-level session(s) → {}{}",
        cv_search::default_tantivy_dir().display(),
        if subagents {
            " (sub-agent transcripts folded in)"
        } else {
            ""
        }
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
