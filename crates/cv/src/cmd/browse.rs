//! `cv ls` / `cv timeline` / `cv stats` — browsing the discovered corpus.
//!
//! These read [`cv_core::sessions`] (the probed catalog — milliseconds warm, transparently a full
//! discovery when cold/stale); `cv ls --fresh` forces [`cv_core::discover_all`]'s full scan.

use crate::util::{dim_cwd, home_rel, parse_harness, short_id};
use anyhow::Result;
use cv_core::ir::{truncate, SessionRef};

/// Apply a parsed query to a ref list in place: prune with the catalog-cheap prefilter, then — if
/// the query has any parse/index/events term — parse each survivor and keep only full matches. Used
/// by `ls`/`timeline`/`stats` so they all speak the same `-q`.
fn apply_query(refs: &mut Vec<SessionRef>, query: &Option<cv_core::SessionQuery>) {
    let Some(q) = query else { return };
    refs.retain(|r| q.prefilter(r));
    if q.needs_parse() || q.needs_index() || q.needs_events() {
        let text_sets = crate::cmd::query::TextSets::resolve(q);
        refs.retain(|r| {
            cv_core::harness::for_harness(r.harness)
                .and_then(|a| cv_core::stream::collect_with(a.as_ref(), r, &cv_core::ParseOptions::lazy()).ok())
                .is_some_and(|s| crate::cmd::query::matches_full(q, r, &s, &text_sets))
        });
    }
}

pub(crate) fn cmd_ls(
    harness: Option<String>,
    cwd: Option<String>,
    query: Option<String>,
    limit: usize,
    sort_by: &str,
    fresh: bool, // force a full re-discovery instead of trusting the probed catalog
) -> Result<()> {
    let want = parse_harness(&harness)?;
    let query = crate::cmd::query::build(query)?;
    let all = if fresh { cv_core::discover_all() } else { cv_core::sessions() };
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
    apply_query(&mut refs, &query);

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
    // The exists() check guards the catalog read path's one residual lie — a session deleted since
    // the probe last looked — and is bounded by `limit`, not the fleet (it stats only listed rows).
    for r in refs.iter().filter(|r| r.path.exists()).take(limit) {
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

// ---------- timeline ----------

/// A unified chronological feed across all harnesses (oldest → newest, like a feed). Grouped by day.
pub(crate) fn cmd_timeline(harness: Option<String>, cwd: Option<String>, query: Option<String>, limit: usize) -> Result<()> {
    let want = parse_harness(&harness)?;
    let query = crate::cmd::query::build(query)?;
    let mut refs: Vec<SessionRef> = cv_core::sessions()
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
    apply_query(&mut refs, &query);

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
    // exists(): same catalog-read guard as `cmd_ls` — O(shown window), not O(fleet).
    for r in shown.iter().filter(|r| r.path.exists()) {
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

// ---------- stats ----------

pub(crate) fn cmd_stats(query: Option<String>) -> Result<()> {
    use std::collections::HashMap;
    let query = crate::cmd::query::build(query)?;
    let mut refs = cv_core::sessions();
    apply_query(&mut refs, &query);
    let total = refs.len();
    if total == 0 {
        let scope = if query.is_some() { " match the query" } else { " discovered" };
        println!("no sessions{scope}.");
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
