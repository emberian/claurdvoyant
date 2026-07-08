//! `cv workflow` / `cv tools` / `cv compaction` — the deep Claude-harness surfaces:
//! first-class workflow runs (phase tree → agents → outcomes + script), cross-agent tool
//! analytics, and compaction-boundary detection/retrieval.

use crate::util::short_id;
use anyhow::{bail, Context, Result};
use cv_core::ir::truncate;
use cv_core::tools::{ForestTools, ToolHistogram};

// ===================== cv workflow =====================

/// `cv workflow <session> [run]`: render one workflow run richly — its phases, the agents grouped
/// under each phase with their outcomes, the run totals, the aggregated result, and (optionally)
/// the driving script. Without a `run`, lists every workflow the session launched. Both arguments
/// accept **names**: `run` matches a workflow name (exact, else unique prefix) as well as a run id,
/// and when `<session>` matches no session id it's resolved as a workflow name across the whole
/// catalog — session titles are auto-generated and rarely mention the workflow you remember.
pub(crate) fn cmd_workflow(
    id: &str,
    run_id: Option<String>,
    harness: Option<String>,
    json: bool,
    script: bool,
) -> Result<()> {
    let want = crate::util::parse_harness(&harness)?;
    let Some((r, _adapter)) = cv_core::find(id, want)? else {
        // Not a session id → maybe it's a workflow name ("the stark-kill session" problem).
        return workflow_by_name_fleetwide(id, json, script);
    };

    // No run id → list the session's workflows (a directory of runs).
    let Some(run_id) = run_id else {
        return list_workflows(&r, json);
    };

    let wf = cv_core::workflow_of(&r, &run_id).with_context(|| {
        let runs = cv_core::workflows_of(&r);
        let names: Vec<&str> = runs.iter().filter_map(|w| w.name.as_deref()).collect();
        format!(
            "no workflow {run_id:?} in {} — {} run(s): {}",
            short_id(&r.id),
            runs.len(),
            if names.is_empty() {
                "(unnamed)".into()
            } else {
                names.join(", ")
            },
        )
    })?;

    if json {
        println!("{}", serde_json::to_string_pretty(&wf)?);
        return Ok(());
    }

    render_workflow(&wf, script);
    Ok(())
}

/// Resolve a bare `cv workflow <name>` against every session's workflow runs. One hit renders it;
/// several list themselves with ready-to-paste commands. A miss falls through to a ghost-launch
/// scan — a name with no recorded run anywhere may still be a launch whose state was never
/// persisted (crash/power loss), and that is precisely when someone hunts it by name.
fn workflow_by_name_fleetwide(name: &str, json: bool, script: bool) -> Result<()> {
    let hits = cv_core::find_workflows_by_name(name);
    if hits.is_empty() {
        let ghosts = cv_core::find_ghost_launches_by_name(name);
        if !ghosts.is_empty() {
            println!(
                "no recorded run named {name:?} — but {} GHOST launch(es) match (state never persisted; crash/kill before write?):\n",
                ghosts.len()
            );
            for (r, g) in &ghosts {
                let when =
                    g.ts.map(|t| crate::util::fmt_local(t, "%Y-%m-%d %H:%M"))
                        .unwrap_or_else(|| "(no timestamp)".into());
                println!(
                    "   {}  launched {when} in session {} ({})",
                    g.name.as_deref().unwrap_or("(unnamed)"),
                    short_id(&r.id),
                    r.title.as_deref().unwrap_or("untitled"),
                );
            }
            println!("\n→ any sub-agent debris sits under the session dir's subagents/workflows/");
            return Ok(());
        }
        bail!("no session and no workflow name matching {name:?} (try `cv ls -q 'workflow:{name}'`)");
    }
    if hits.len() == 1 {
        let (r, wf) = &hits[0];
        if json {
            println!("{}", serde_json::to_string_pretty(wf)?);
            return Ok(());
        }
        println!(
            "(in session {} — {})\n",
            short_id(&r.id),
            r.title.as_deref().unwrap_or("untitled")
        );
        render_workflow(wf, script);
        return Ok(());
    }
    println!("# {} workflow run(s) matching {name:?}:\n", hits.len());
    for (r, w) in &hits {
        println!(
            "cv workflow {} {}   # {} · {} · {} agent(s)",
            short_id(&r.id),
            w.run_id,
            w.name.as_deref().unwrap_or("(unnamed)"),
            w.status.as_deref().unwrap_or("?"),
            w.agent_count,
        );
    }
    Ok(())
}

/// List every workflow run a session launched, newest first — name, status, phase/agent counts,
/// and the run summary — plus any **ghost launches**: `Workflow` invocations visible in the
/// transcript whose run state was never persisted (a crash / power loss / hard kill before the
/// harness wrote `workflows/wf_*.json`). Without the ghost check, a run that died at launch is
/// simply invisible here — exactly the run whose debris most needs finding.
fn list_workflows(r: &cv_core::SessionRef, json: bool) -> Result<()> {
    let runs = cv_core::workflows_of(r);
    let ghosts = cv_core::workflow_ghosts_of(r);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"runs": runs, "ghost_launches": ghosts}))?
        );
        return Ok(());
    }
    if runs.is_empty() && ghosts.is_empty() {
        println!("no workflows launched by {}", short_id(&r.id));
        return Ok(());
    }
    if runs.is_empty() {
        println!("# 0 recorded workflow(s) in {}", short_id(&r.id));
    } else {
        println!("# {} workflow(s) in {}\n", runs.len(), short_id(&r.id));
    }
    for w in &runs {
        let status = w.status.as_deref().unwrap_or("?");
        let states = w.state_counts();
        let state_str = if states.is_empty() {
            String::new()
        } else {
            let parts: Vec<String> = states.iter().map(|(k, n)| format!("{n} {k}")).collect();
            format!("  [{}]", parts.join(", "))
        };
        println!(
            "{}  {}  {} · {} phase(s) · {} agent(s){}",
            w.run_id,
            w.name.as_deref().unwrap_or("(unnamed)"),
            status,
            w.phases.len(),
            w.agent_count,
            state_str,
        );
        if let Some(s) = &w.summary {
            println!("    {}", truncate(s, 120));
        }
    }
    if !ghosts.is_empty() {
        println!(
            "\n⚠ {} launch(es) with NO recorded run — state never persisted (crash/kill before write?):",
            ghosts.len()
        );
        for g in &ghosts {
            let when =
                g.ts.map(|t| crate::util::fmt_local(t, "%Y-%m-%d %H:%M"))
                    .unwrap_or_else(|| "(no timestamp)".into());
            println!("   {}  launched {}", g.name.as_deref().unwrap_or("(unnamed)"), when);
        }
        println!("   → any sub-agent debris sits under the session dir's subagents/workflows/");
    }
    println!("\n→ `cv workflow {} <runId>` for one run's phase tree", short_id(&r.id));
    Ok(())
}

/// Render one workflow run: header (name/status/totals) → each phase with its agents and outcomes
/// → optionally the driving script.
fn render_workflow(w: &cv_core::Workflow, show_script: bool) {
    println!(
        "# workflow {}  ({})",
        w.name.as_deref().unwrap_or("(unnamed)"),
        w.run_id
    );
    let dur = w
        .duration_ms
        .map(|ms| format!(" · {}", fmt_duration(ms)))
        .unwrap_or_default();
    let started = w
        .started_at
        .map(|t| format!(" · started {}", crate::util::fmt_local(t, "%Y-%m-%d %H:%M")))
        .unwrap_or_default();
    println!(
        "{} · {} agent(s) · {} phase(s) · {} tokens · {} tool calls{}{}",
        w.status.as_deref().unwrap_or("?"),
        w.agent_count,
        w.phases.len(),
        fmt_int(w.total_tokens),
        fmt_int(w.total_tool_calls),
        dur,
        started,
    );
    if let Some(m) = &w.default_model {
        println!("model: {m}");
    }
    if let Some(t) = &w.task_id {
        println!("task: {t}");
    }
    if let Some(a) = &w.args {
        println!("args: {}", truncate(&a.to_string(), 200));
    }
    if let Some(rf) = &w.resume_from {
        println!("resumed from: {rf}");
    }
    if let Some(e) = &w.error {
        println!("⚠ error: {}", truncate(e, 200));
    }
    if let Some(s) = &w.summary {
        println!("\n{}", truncate(s, 600));
    }

    // The phase tree: phases in order, agents under each with their state + outcome.
    for p in &w.phases {
        let title = p.title.as_deref().unwrap_or("(phase)");
        println!(
            "\n## phase {} · {}{}",
            p.index,
            title,
            p.detail
                .as_deref()
                .map(|d| format!("  — {}", truncate(d, 80)))
                .unwrap_or_default()
        );
        if p.agents.is_empty() {
            println!("   (no agents)");
        }
        for a in &p.agents {
            render_workflow_agent(a);
        }
    }
    if !w.orphan_agents.is_empty() {
        println!("\n## (agents with no matching phase)");
        for a in &w.orphan_agents {
            render_workflow_agent(a);
        }
    }

    // The run's own narration: log() lines. Tail-biased — the end is where a run explains how it
    // finished (or what was failing when it stopped).
    if !w.logs.is_empty() {
        const TAIL: usize = 8;
        println!("\n## log ({} line(s))", w.logs.len());
        if w.logs.len() > TAIL {
            println!("   … {} earlier (--json for all)", w.logs.len() - TAIL);
        }
        for l in w.logs.iter().rev().take(TAIL).rev() {
            println!("   {}", truncate(l, 160));
        }
    }

    // The aggregated return value — the harvest payload.
    match &w.result {
        Some(r) => {
            let pretty = serde_json::to_string_pretty(r).unwrap_or_else(|_| r.to_string());
            println!("\n## result");
            if pretty.len() > 2000 {
                println!(
                    "{}\n(truncated — `--json` for the full result)",
                    truncate(&pretty, 2000)
                );
            } else {
                println!("{pretty}");
            }
        }
        None => println!("\n(no result recorded — the run returned nothing or died before finishing)"),
    }

    if show_script {
        match &w.script {
            Some(src) => {
                println!(
                    "\n## script{}",
                    w.script_path
                        .as_ref()
                        .map(|p| format!(" ({})", p.display()))
                        .unwrap_or_default()
                );
                println!("```js\n{src}\n```");
            }
            None => println!("\n(no script recorded)"),
        }
    } else if w.script.is_some() {
        println!("\n→ `--script` to print the driving workflow script");
    }
}

/// One agent line inside a workflow's phase tree: state glyph, label, id, telemetry, and the head
/// of its result.
fn render_workflow_agent(a: &cv_core::WorkflowAgent) {
    let glyph = match a.state.as_deref() {
        Some("done") => "✓",
        Some("error") => "✗",
        Some("progress") => "…",
        Some("start") => "▸",
        _ => "•",
    };
    let id = a.agent_id.as_deref().map(short_id).unwrap_or_else(|| "—".into());
    let label = a.label.as_deref().unwrap_or("(agent)");
    let mut tele = Vec::new();
    if let Some(t) = a.tokens {
        tele.push(format!("{} tok", fmt_int(t)));
    }
    if let Some(tc) = a.tool_calls {
        tele.push(format!("{tc} calls"));
    }
    if let Some(ms) = a.duration_ms {
        tele.push(fmt_duration(ms));
    }
    if a.cached {
        tele.push("cached".into());
    }
    if let Some(n) = a.attempt.filter(|n| *n > 1) {
        tele.push(format!("attempt {n}"));
    }
    let tele = if tele.is_empty() {
        String::new()
    } else {
        format!("  ({})", tele.join(", "))
    };
    println!("   {glyph} {label}  {id}{tele}");
    if let Some(e) = &a.error {
        println!("       ✗ {}", truncate(e, 120));
    }
    // A non-terminal agent (crash/kill/interrupt) — show where it was when last alive.
    if !matches!(a.state.as_deref(), Some("done")) {
        if let Some(tool) = &a.last_tool_name {
            let when = a
                .last_progress_at
                .map(|t| format!(" @ {}", crate::util::fmt_local(t, "%m-%d %H:%M")))
                .unwrap_or_default();
            println!(
                "       ↪ last: {tool} · {}{when}",
                a.last_tool_summary
                    .as_deref()
                    .map(|s| truncate(s, 100))
                    .unwrap_or_default(),
            );
        }
    }
    if let Some(rp) = &a.result_preview {
        println!("       ↩ {}", truncate(rp, 200));
    }
}

// ===================== cv tools =====================

/// `cv tools <id>`: cross-agent tool analytics. Default = the aggregate histogram across the whole
/// forest (orchestrator + every sub-agent). Filters:
/// * `--agent <id>` — one agent's histogram ("which tools did agent X use")
/// * `--tool <name>` — which agents used tool T (across the forest)
/// * `--workflow <run>` — restrict to one workflow's agents
/// * `--across` — one row per agent (the per-agent breakdown), instead of the aggregate
/// * `--timeline` — the time-ordered tool-call timeline
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_tools(
    id: &str,
    harness: Option<String>,
    agent: Option<String>,
    tool: Option<String>,
    workflow: Option<String>,
    across: bool,
    timeline: bool,
    json: bool,
) -> Result<()> {
    let want = crate::util::parse_harness(&harness)?;
    let (r, _adapter) = cv_core::find(id, want)?.with_context(|| format!("no session matching {id:?}"))?;

    if timeline {
        return tools_timeline(&r, agent.as_deref(), json);
    }

    let mut forest = cv_core::tools::forest_tools(&r)?;
    if let Some(run) = &workflow {
        // Resolve a run-id *prefix* to the full id (the forest tags agents with the full run id),
        // so `--workflow wf_08eb66fa` matches `wf_08eb66fa-4b1`.
        let full = cv_core::workflow_of(&r, run)
            .map(|w| w.run_id)
            .with_context(|| format!("no workflow {run:?} in {}", short_id(&r.id)))?;
        // The orchestrator isn't part of any one workflow; restrict to the named run's agents.
        forest = forest.for_workflow(&full);
        if forest.agents.is_empty() {
            bail!("no agents found for workflow {full:?} in {}", short_id(&r.id));
        }
    }

    // `--tool T`: which agents used it.
    if let Some(t) = &tool {
        return tools_which_agents(&forest, t, json);
    }
    // `--agent X`: one agent's histogram.
    if let Some(a) = &agent {
        let at = forest.agent(a).with_context(|| {
            format!(
                "no agent matching {a:?} in {} (try `cv tools {} --across`)",
                short_id(&r.id),
                short_id(&r.id)
            )
        })?;
        if json {
            println!("{}", serde_json::to_string_pretty(at)?);
            return Ok(());
        }
        println!("# tools used by {} ({})", at.agent, at.agent_type);
        print_histogram(&at.histogram);
        return Ok(());
    }
    // `--across`: per-agent breakdown.
    if across {
        return tools_across(&forest, json);
    }

    // Default: the aggregate across the whole forest.
    let agg = forest.aggregate();
    if json {
        println!("{}", serde_json::to_string_pretty(&agg)?);
        return Ok(());
    }
    println!(
        "# tool usage across {} ({} agent(s): orchestrator + forest)",
        short_id(&r.id),
        forest.agents.len()
    );
    print_histogram(&agg);
    println!("\n→ `--across` for per-agent · `--agent <id>` · `--tool <name>` · `--timeline`");
    Ok(())
}

/// "Which agents used tool T", ranked by count.
fn tools_which_agents(forest: &ForestTools, tool: &str, json: bool) -> Result<()> {
    let users = forest.agents_using(tool);
    if json {
        let rows: Vec<serde_json::Value> = users
            .iter()
            .map(|(a, c)| {
                serde_json::json!({
                    "agent": a.agent, "agent_type": a.agent_type, "workflow": a.workflow,
                    "calls": c.calls, "edits": c.edits, "reads": c.reads, "commands": c.commands, "errors": c.errors,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if users.is_empty() {
        println!("no agent used {tool:?}");
        return Ok(());
    }
    let total: usize = users.iter().map(|(_, c)| c.calls).sum();
    println!("# {} agent(s) used {tool:?} ({total} call(s) total)\n", users.len());
    for (a, c) in users {
        let wf = a.workflow.as_deref().map(|w| format!("  ⟐{w}")).unwrap_or_default();
        println!(
            "{:>5}  {:9}  {:18}{}",
            c.calls,
            a.agent_type,
            if a.agent == cv_core::tools::ORCHESTRATOR {
                a.agent.clone()
            } else {
                short_id(&a.agent)
            },
            wf,
        );
    }
    Ok(())
}

/// Per-agent breakdown: one block per agent, its histogram beneath.
fn tools_across(forest: &ForestTools, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(forest)?);
        return Ok(());
    }
    // Skip agents that used no tools (a chat-only sub-agent) to keep the view dense.
    let active: Vec<&cv_core::tools::AgentTools> =
        forest.agents.iter().filter(|a| a.histogram.total_calls > 0).collect();
    println!("# per-agent tool usage ({} active agent(s))\n", active.len());
    for a in active {
        let wf = a.workflow.as_deref().map(|w| format!("  ⟐{w}")).unwrap_or_default();
        let label = if a.agent == cv_core::tools::ORCHESTRATOR {
            a.agent.clone()
        } else {
            short_id(&a.agent)
        };
        let top: Vec<String> = a
            .histogram
            .ranked()
            .into_iter()
            .take(6)
            .map(|(name, c)| format!("{name}×{}", c.calls))
            .collect();
        println!(
            "{:18} {:11} {:>4} call(s){}  {}",
            label,
            a.agent_type,
            a.histogram.total_calls,
            wf,
            top.join(" "),
        );
    }
    Ok(())
}

/// The time-ordered tool-call timeline across the forest.
fn tools_timeline(r: &cv_core::SessionRef, agent: Option<&str>, json: bool) -> Result<()> {
    let mut events = cv_core::tools::forest_timeline(r)?;
    if let Some(key) = agent {
        // Match the orchestrator sentinel or a full/prefix agent id.
        events.retain(|e| e.agent == key || e.agent.starts_with(key));
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&events)?);
        return Ok(());
    }
    if events.is_empty() {
        println!(
            "no tool calls{}",
            agent.map(|a| format!(" for {a:?}")).unwrap_or_default()
        );
        return Ok(());
    }
    println!("# {} tool call(s) (chronological)\n", events.len());
    for e in &events {
        let time =
            e.ts.and_then(|t| crate::util::fmt_local_ts(t, "%m-%d %H:%M:%S"))
                .unwrap_or_else(|| "--------------".into());
        let who = if e.agent == cv_core::tools::ORCHESTRATOR {
            "orch".to_string()
        } else {
            short_id(&e.agent)
        };
        println!(
            "{}  {:8}  {:9}  {:14}  {}",
            time,
            who,
            e.kind,
            e.tool.as_deref().unwrap_or("-"),
            e.target.as_deref().map(|t| truncate(t, 70)).unwrap_or_default(),
        );
    }
    Ok(())
}

/// Print a [`ToolHistogram`] as a ranked table (tool · calls · kind breakdown · errors).
fn print_histogram(h: &ToolHistogram) {
    if h.tools.is_empty() {
        println!("  (no tool calls)");
        return;
    }
    println!(
        "  {:<22} {:>6}  {:>6} {:>6} {:>6} {:>6}",
        "tool", "calls", "edit", "read", "cmd", "err"
    );
    for (name, c) in h.ranked() {
        println!(
            "  {:<22} {:>6}  {:>6} {:>6} {:>6} {:>6}",
            truncate(name, 22),
            c.calls,
            kindcell(c.edits),
            kindcell(c.reads),
            kindcell(c.commands),
            kindcell(c.errors),
        );
    }
    println!(
        "  {:<22} {:>6}  {:>30}",
        "TOTAL",
        h.total_calls,
        format!("{} distinct · {} error(s)", h.distinct(), h.total_errors),
    );
}

/// A histogram kind-cell: the count, or a blank for zero (keeps the table readable).
fn kindcell(n: usize) -> String {
    if n == 0 {
        String::new()
    } else {
        n.to_string()
    }
}

// ===================== cv compaction =====================

/// `cv compaction <id>`: list every compaction boundary in a session — when it happened, why
/// (trigger), the pre-compaction context size, and the summary that seeded the next window. With
/// `--summaries`, print each summary's full text.
pub(crate) fn cmd_compaction(id: &str, harness: Option<String>, summaries: bool, json: bool) -> Result<()> {
    let want = crate::util::parse_harness(&harness)?;
    let (r, _adapter) = cv_core::find(id, want)?.with_context(|| format!("no session matching {id:?}"))?;

    let comps = cv_core::compaction::detect(&r, true)?;

    if json {
        // Attach each boundary's pre-compaction span for machine consumers.
        let rows: Vec<serde_json::Value> = comps
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut v = serde_json::to_value(c).unwrap_or(serde_json::Value::Null);
                if let Some(obj) = v.as_object_mut() {
                    if let Some((s, e)) = cv_core::compaction::pre_compaction_span(&comps, i) {
                        obj.insert("pre_compaction_span".into(), serde_json::json!([s, e]));
                    }
                }
                v
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if comps.is_empty() {
        println!("{} never compacted (0 boundaries)", short_id(&r.id));
        return Ok(());
    }

    println!("# {} compacted {} time(s)\n", short_id(&r.id), comps.len());
    for (i, c) in comps.iter().enumerate() {
        let trig = c.trigger.as_deref().unwrap_or("?");
        let pre = c
            .pre_tokens
            .map(|t| format!("{} tokens", fmt_int(t)))
            .unwrap_or_else(|| "? tokens".into());
        let dur = c
            .duration_ms
            .map(|ms| format!(" · took {}", fmt_duration(ms)))
            .unwrap_or_default();
        let span = cv_core::compaction::pre_compaction_span(&comps, i)
            .map(|(s, e)| format!("  · pre-span msgs {s}-{e}"))
            .unwrap_or_default();
        println!(
            "── #{} · {} · {} (pre) · @msg {}{}{}",
            i + 1,
            trig,
            pre,
            c.boundary_msg_idx,
            dur,
            span,
        );
        match (&c.summary, summaries) {
            (Some(s), true) => println!("\n{s}\n"),
            (Some(s), false) => println!("   summary: {}\n", truncate(s, 200)),
            (None, _) => println!("   (no summary recorded)\n"),
        }
    }
    if !summaries {
        println!(
            "→ `--summaries` for full summary text · `cv show {} --pre-compaction` to read the lost span",
            short_id(&r.id)
        );
    }
    Ok(())
}

// ===================== formatting helpers =====================

/// Group-separate a large integer (`589698` → `589,698`).
fn fmt_int(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Humanize a millisecond duration (`3144884` → `52m 24s`).
fn fmt_duration(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_and_duration_formatting() {
        assert_eq!(fmt_int(589698), "589,698");
        assert_eq!(fmt_int(0), "0");
        assert_eq!(fmt_int(42), "42");
        assert_eq!(fmt_int(1000), "1,000");
        assert_eq!(fmt_duration(3144884), "52m 24s");
        assert_eq!(fmt_duration(5_000), "5s");
        assert_eq!(fmt_duration(7_200_000), "2h 0m");
    }

    #[test]
    fn kindcell_blanks_zero() {
        assert_eq!(kindcell(0), "");
        assert_eq!(kindcell(3), "3");
    }
}
