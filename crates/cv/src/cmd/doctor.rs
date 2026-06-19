//! `cv doctor` — diagnose why a session's context window keeps filling (and compacting).
//!
//! Attributes context pressure to its SOURCES, measured from the transcript's typed blocks:
//! tool results (split MCP vs builtin, ranked per tool), thinking, assistant/user text, images.
//! The *fixed* system+tools overhead (base prompt, CLAUDE.md/rules, skills, MCP tool schemas) is
//! NOT carried in the transcript — Claude Code records the conversation, not the system block — so
//! it can't be itemized; instead it's *sized* indirectly from token `usage` (the context every
//! turn pays before any conversation) and reported as one bucket. Paired with compaction
//! frequency/triggers, this answers David's "why is my Claude compacting so much?".
//!
//! With no <id>, analyzes the most recent session(s) for the current directory.

use anyhow::{Context, Result};
use cv_core::{Block, Role, Session, SessionRef};
use std::collections::{BTreeMap, HashMap};

/// Claude tokenizer ≈ 3.3–3.7 bytes/token; matches cv_core::prune's text estimate.
const BYTES_PER_TOKEN: f64 = 3.5;
/// One image tile ≈ this many tokens (we never inline image bytes, so estimate per block).
const IMAGE_TOKENS: u64 = 1500;

fn toks(byte_len: usize) -> u64 {
    (byte_len as f64 / BYTES_PER_TOKEN).ceil() as u64
}

#[derive(Default)]
struct ToolStat {
    calls: u64,
    result_tokens: u64,
    is_mcp: bool,
}

#[derive(Default)]
struct Report {
    sessions: u64,
    messages: u64,
    // measured conversational content, by source
    user_text: u64,
    assistant_text: u64,
    thinking: u64,
    tool_call_args: u64,
    tool_results: u64,
    images: u64,
    system_text: u64,
    by_tool: BTreeMap<String, ToolStat>,
    // compaction
    compactions: u64,
    auto_compactions: u64,
    pre_tokens: Vec<u64>,
    // usage-derived context sizing
    startup_ctx: u64, // worst per-session first-turn total context (≈ fixed overhead)
    peak_ctx: u64,    // largest total context observed across turns
}

impl Report {
    fn conv_total(&self) -> u64 {
        self.user_text
            + self.assistant_text
            + self.thinking
            + self.tool_call_args
            + self.tool_results
            + self.images
            + self.system_text
    }
}

fn attribute(rep: &mut Report, session: &Session) {
    rep.sessions += 1;
    rep.messages += session.messages.len() as u64;

    // Claude Code's ToolResult blocks usually omit the tool name but carry `tool_use_id`; the
    // matching ToolUse block has {id, name}. Map id→name first so results attribute to real tools
    // (and MCP names) instead of all collapsing into "(unknown tool)".
    let mut id_to_name: HashMap<&str, &str> = HashMap::new();
    for m in &session.messages {
        for b in &m.content {
            if let Block::ToolUse { id, name, .. } = b {
                id_to_name.insert(id.as_str(), name.as_str());
            }
        }
    }

    let mut session_startup: Option<u64> = None;
    for m in &session.messages {
        if let Some(u) = &m.usage {
            let total = u.input_tokens.unwrap_or(0)
                + u.cache_read_tokens.unwrap_or(0)
                + u.cache_creation_tokens.unwrap_or(0);
            if total > 0 {
                rep.peak_ctx = rep.peak_ctx.max(total);
                if session_startup.is_none() {
                    session_startup = Some(total); // first turn ≈ system+tools+rules+first msg
                }
            }
        }
        for b in &m.content {
            match b {
                Block::Text { text } => {
                    let t = toks(text.len());
                    match m.role {
                        Role::Assistant => rep.assistant_text += t,
                        Role::System => rep.system_text += t,
                        _ => rep.user_text += t,
                    }
                }
                Block::Thinking { text, signature, encrypted, .. } => {
                    // Thinking costs context as its plaintext PLUS the signature/encrypted blob that
                    // rides with it — both are re-sent to the API. Claude Code frequently records
                    // signature-only thinking (plaintext stripped), so counting just `text` reported
                    // a misleading zero.
                    rep.thinking += toks(text.len())
                        + toks(signature.as_deref().map_or(0, str::len))
                        + toks(encrypted.as_deref().map_or(0, str::len));
                }
                Block::ToolUse { name, input, .. } => {
                    rep.tool_call_args += toks(input.to_string().len());
                    let e = rep.by_tool.entry(name.clone()).or_default();
                    e.calls += 1;
                    if name.starts_with("mcp__") {
                        e.is_mcp = true;
                    }
                }
                Block::ToolResult { content, tool_name, tool_use_id, .. } => {
                    let t = toks(content.len());
                    rep.tool_results += t;
                    let name = tool_name
                        .clone()
                        .or_else(|| id_to_name.get(tool_use_id.as_str()).map(|s| s.to_string()))
                        .unwrap_or_else(|| "(unknown tool)".to_string());
                    let mcp = name.starts_with("mcp__");
                    let e = rep.by_tool.entry(name).or_default();
                    e.result_tokens += t;
                    if mcp {
                        e.is_mcp = true;
                    }
                }
                Block::Image { .. } => rep.images += IMAGE_TOKENS,
                _ => {}
            }
        }
    }
    rep.startup_ctx = rep.startup_ctx.max(session_startup.unwrap_or(0));
}

fn fmt_tok(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

fn bar(frac: f64) -> String {
    "█".repeat((frac * 24.0).round() as usize)
}

pub(crate) fn cmd_doctor(
    id: Option<String>,
    harness: Option<String>,
    recent: usize,
    json: bool,
) -> Result<()> {
    let want = crate::util::parse_harness(&harness)?;

    // Resolve the target session(s).
    let targets: Vec<SessionRef> = if let Some(id) = &id {
        let (r, _ad) =
            cv_core::find(id, want)?.with_context(|| format!("no session matching {id:?}"))?;
        vec![r]
    } else {
        let cwd = std::env::current_dir().ok();
        let mut refs: Vec<SessionRef> = cv_core::sessions()
            .into_iter()
            .filter(|r| want.is_none_or(|h| r.harness == h))
            .filter(|r| {
                cwd.as_ref()
                    .is_none_or(|c| r.cwd.as_deref() == Some(c.as_path()))
            })
            .collect();
        refs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        refs.truncate(recent.max(1));
        if refs.is_empty() {
            anyhow::bail!(
                "no sessions found for {} — pass an explicit <id>, or run from a project dir",
                cwd.map(|c| c.display().to_string()).unwrap_or_default()
            );
        }
        refs
    };

    let mut rep = Report::default();
    let mut label = String::new();
    for r in &targets {
        let (rr, adapter) = cv_core::find(&r.id, Some(r.harness))?
            .with_context(|| format!("could not load session {}", r.id))?;
        let session = adapter.parse(&rr)?;
        if label.is_empty() {
            label = crate::short_id(&rr.id);
        }
        attribute(&mut rep, &session);
        for c in cv_core::compaction::detect_in_session(&session, false) {
            rep.compactions += 1;
            if c.trigger.as_deref() == Some("auto") {
                rep.auto_compactions += 1;
            }
            if let Some(p) = c.pre_tokens {
                rep.pre_tokens.push(p);
            }
        }
    }

    if json {
        return print_json(&rep);
    }
    print_report(&rep, &label, targets.len());
    Ok(())
}

fn print_report(rep: &Report, label: &str, n_targets: usize) {
    let scope = if rep.sessions > 1 {
        format!("{} sessions (this dir)", rep.sessions)
    } else {
        format!("session {label}")
    };
    println!("# compaction doctor — {scope}\n");

    // Compaction summary.
    if rep.compactions == 0 {
        println!("Compaction:  never compacted across {} message(s) — healthy ✅", rep.messages);
    } else {
        let avg = rep.pre_tokens.iter().sum::<u64>() / rep.pre_tokens.len().max(1) as u64;
        let worst = rep.pre_tokens.iter().copied().max().unwrap_or(0);
        println!(
            "Compaction:  {} time(s) ({} auto) · avg pre-compaction {} · worst {}",
            rep.compactions,
            rep.auto_compactions,
            fmt_tok(avg),
            fmt_tok(worst)
        );
    }

    // Fixed overhead + peak, from usage.
    if rep.startup_ctx > 0 {
        println!(
            "Overhead:    ~{} of fixed context every turn before any conversation\n             \
             (base prompt + CLAUDE.md/rules + skills + MCP tool schemas — sized from token usage,\n             \
             not itemizable: the transcript records the conversation, not the system block)",
            fmt_tok(rep.startup_ctx)
        );
    }
    if rep.peak_ctx > 0 {
        println!("Peak window: {} observed", fmt_tok(rep.peak_ctx));
    }

    // Where the conversational growth goes (measured).
    let total = rep.conv_total().max(1);
    println!("\nConversational context by source ({} measured):", fmt_tok(rep.conv_total()));
    let mut rows: Vec<(&str, u64)> = vec![
        ("tool results", rep.tool_results),
        ("thinking", rep.thinking),
        ("assistant text", rep.assistant_text),
        ("user text", rep.user_text),
        ("tool-call args", rep.tool_call_args),
        ("images", rep.images),
        ("system msgs", rep.system_text),
    ];
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    for (name, v) in rows {
        if v == 0 {
            continue;
        }
        let frac = v as f64 / total as f64;
        println!("  {:<15} {:>4.0}%  {:<24} {}", name, frac * 100.0, bar(frac), fmt_tok(v));
    }

    // Top tools by result tokens (the usual variable bloat).
    let mut tools: Vec<(&String, &ToolStat)> = rep.by_tool.iter().collect();
    tools.sort_by(|a, b| b.1.result_tokens.cmp(&a.1.result_tokens));
    let shown: Vec<_> = tools.iter().filter(|(_, s)| s.result_tokens > 0).take(8).collect();
    if !shown.is_empty() {
        println!("\nTop context consumers (tool results):");
        for (name, s) in &shown {
            let tag = if s.is_mcp { " [MCP]" } else { "" };
            println!(
                "  {:<28}{:<6} {:>7}  ({} call{})",
                name,
                tag,
                fmt_tok(s.result_tokens),
                s.calls,
                if s.calls == 1 { "" } else { "s" }
            );
        }
        let mcp: u64 = rep.by_tool.values().filter(|s| s.is_mcp).map(|s| s.result_tokens).sum();
        let builtin = rep.tool_results.saturating_sub(mcp);
        if mcp > 0 {
            let share = 100.0 * mcp as f64 / rep.tool_results.max(1) as f64;
            println!(
                "\nMCP vs builtin tool results:  MCP {} ({:.0}%) · builtin {}",
                fmt_tok(mcp),
                share,
                fmt_tok(builtin)
            );
        }
    }

    println!("\nVerdict: {}", verdict(rep));
    if n_targets == 1 && rep.sessions == 1 {
        println!("  (analyze your whole project: `cv doctor --recent 20`)");
    }
}

fn verdict(rep: &Report) -> String {
    let total = rep.conv_total().max(1);
    let mcp: u64 = rep.by_tool.values().filter(|s| s.is_mcp).map(|s| s.result_tokens).sum();
    let mut parts: Vec<String> = Vec::new();

    // Tool output is the lever to pull when it's the single largest source (even below 50%) or
    // when tool results + call args together dominate the window.
    let tool_activity = rep.tool_results + rep.tool_call_args;
    let tool_results_is_top = rep.tool_results >= rep.thinking
        && rep.tool_results >= rep.assistant_text
        && rep.tool_results >= rep.user_text;
    if rep.tool_results > 0 && (tool_results_is_top || tool_activity * 2 >= total) {
        let mut t: Vec<_> = rep.by_tool.iter().filter(|(_, s)| s.result_tokens > 0).collect();
        t.sort_by(|a, b| b.1.result_tokens.cmp(&a.1.result_tokens));
        let top: Vec<String> = t.iter().take(2).map(|(n, _)| (*n).clone()).collect();
        parts.push(format!(
            "tool output is the biggest lever ({:.0}% of context, {:.0}% with tool-call args), led by {} — \
             trim verbose output, scope reads/greps, or `cv prune` old tool payloads",
            100.0 * rep.tool_results as f64 / total as f64,
            100.0 * tool_activity as f64 / total as f64,
            top.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(" + ")
        ));
    }
    if mcp > 0 && mcp * 2 >= rep.tool_results {
        parts.push(format!(
            "MCP tools are {:.0}% of tool output — disabling unused MCP servers would free real room",
            100.0 * mcp as f64 / rep.tool_results.max(1) as f64
        ));
    }
    // ~25% of a 200k window is a heavy constant tax.
    if rep.startup_ctx >= 50_000 {
        parts.push(format!(
            "fixed overhead is ~{} before any conversation (leaner CLAUDE.md / fewer MCP servers \
             cuts this every turn)",
            fmt_tok(rep.startup_ctx)
        ));
    }
    if rep.thinking * 4 >= total && rep.thinking > 0 {
        parts.push("extended thinking is a large share — lower the thinking budget if you don't need it".into());
    }
    if parts.is_empty() {
        if rep.compactions == 0 {
            return "nothing alarming — context use looks balanced and the session hasn't compacted.".into();
        }
        return "context use is fairly balanced; compaction is driven by overall conversation length more than any one source.".into();
    }
    parts.join("; ")
}

fn print_json(rep: &Report) -> Result<()> {
    let tools: Vec<_> = rep
        .by_tool
        .iter()
        .map(|(name, s)| {
            serde_json::json!({
                "tool": name, "mcp": s.is_mcp,
                "calls": s.calls, "result_tokens": s.result_tokens
            })
        })
        .collect();
    let mcp: u64 = rep.by_tool.values().filter(|s| s.is_mcp).map(|s| s.result_tokens).sum();
    let out = serde_json::json!({
        "sessions": rep.sessions,
        "messages": rep.messages,
        "compactions": rep.compactions,
        "auto_compactions": rep.auto_compactions,
        "pre_tokens": rep.pre_tokens,
        "fixed_overhead_est": rep.startup_ctx,
        "peak_context": rep.peak_ctx,
        "conversational": {
            "total": rep.conv_total(),
            "tool_results": rep.tool_results,
            "thinking": rep.thinking,
            "assistant_text": rep.assistant_text,
            "user_text": rep.user_text,
            "tool_call_args": rep.tool_call_args,
            "images": rep.images,
            "system_msgs": rep.system_text,
        },
        "tool_results_mcp": mcp,
        "tool_results_builtin": rep.tool_results.saturating_sub(mcp),
        "by_tool": tools,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
