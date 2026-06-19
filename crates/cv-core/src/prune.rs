//! `cv prune` — a *custom, lossless compaction* of a Claude Code session into a **new, resumable**
//! session. Instead of handing a full context window to the summarizer (which rewrites your history
//! into a lossy paragraph), prune snips the **bulky old tool payloads** — large file reads, command
//! logs, base64 screenshots — out of the conversation and into a sidecar, leaving a small `[PRUNED
//! id=…]` marker in their place. Your prompts and the chronological flow are preserved **verbatim**;
//! the most recent turns are left untouched so the model stays sharp on what it's doing right now.
//!
//! The approach (and its byte-faithfulness) is adapted from the validated `flatten-mcp` flattener:
//! we operate on the **raw JSONL lines**, never on our IR — so every Claude-specific field survives
//! a round-trip and the result resumes cleanly with `claude --resume <new-id>`. What we add on top:
//! a fresh session id stamped across every line (so it's a genuinely new session, not an in-place
//! rewrite), a copy of the session's sidecar resources (`subagents/`, `workflows/`) under the new
//! id, and a **keep-last** window so only *old* payloads are snipped.
//!
//! Two payload sites are handled, exactly as Claude Code records them:
//!  1. `tool_result` blocks inside a user message's `message.content` — the bytes the model re-reads
//!     every turn (this is what actually costs context tokens).
//!  2. the top-level `toolUseResult` mirror Claude keeps per result line — disk-only (the model never
//!     sees it), but snipping it shrinks the file and our parse.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Marker prefix stamped where a payload used to be; carries enough to retrieve the original.
const MARKER_PREFIX: &str = "[PRUNED id=";
/// Suffix distinguishing a flattened `toolUseResult` mirror entry from its `content` sibling.
const TUR_SUFFIX: &str = "#tur";
/// Token-estimate constants (Claude tokenizer ≈ 3.3–3.7 B/tok; a screenshot tile ≈ 1500 tok).
const TEXT_BYTES_PER_TOKEN: f64 = 3.5;
const IMAGE_TOKEN_EST: u64 = 1500;

/// Knobs for [`prune_session`].
#[derive(Debug, Clone)]
pub struct PruneOptions {
    /// Snip a tool payload only if it is larger than this many bytes.
    pub min_size: usize,
    /// Keep the last N conversational turns' payloads verbatim (recent context stays sharp).
    pub keep_last: usize,
    /// Hard-drop payloads (no sidecar, irreversible) instead of stashing them for retrieval.
    pub drop: bool,
    /// Also flatten assistant **thinking** blocks (extended reasoning). On a long session the
    /// chain-of-thought often dominates the loaded context, and on resume you rarely need the
    /// verbatim old reasoning — the conclusions are in the assistant's text. Lossless (stashed in the
    /// sidecar like any other payload). Off by default — it changes more than tool output.
    pub thinking: bool,
    /// Explicit new session id; otherwise a fresh UUIDv4.
    pub new_id: Option<String>,
    /// Also copy the session's `subagents/`/`workflows/` resource dir under the new id. Off by
    /// default — `claude --resume` doesn't need it, and for big sessions it can be hundreds of MB /
    /// thousands of files. Turn on if you want cv's forest features to work on the pruned session.
    pub copy_resources: bool,
    /// After pruning, rewrite the trailing `message.usage` records to an **honest** post-prune token
    /// count. Claude Code's resume gate reads the last turn's recorded `usage` (input + cache) as the
    /// session's current size *before it re-sends anything* — so an already-maxed session refuses to
    /// resume even when the (now pruned) content would fit. This recomputes the real size of the loaded
    /// window and corrects the stale number, so the gate lets you back in. Off by default — it edits
    /// recorded metadata, not just content. The source is still never touched (new id only).
    pub revive: bool,
    /// Sliding window: keep only the NEWEST conversational turns whose content sums to ≤ this many
    /// tokens, dropping older turns entirely (lossy — but the source session is never touched, so it
    /// stays the full backup). The kept tail is re-rooted into a standalone resumable session.
    pub window: Option<u64>,
    /// Explicit message subrange `[start, end)` (turn indices) to keep, dropping everything outside
    /// it. `end = None` ⇒ through the last turn. Like `window` but selected by index, not budget.
    pub keep_range: Option<(usize, Option<usize>)>,
    /// Compute and report without writing anything.
    pub dry_run: bool,
}

impl Default for PruneOptions {
    fn default() -> Self {
        PruneOptions {
            min_size: 2048,
            keep_last: 25,
            drop: false,
            thinking: false,
            new_id: None,
            copy_resources: false,
            revive: false,
            window: None,
            keep_range: None,
            dry_run: false,
        }
    }
}

/// What a prune did (or, in `dry_run`, would do).
#[derive(Debug, Clone)]
pub struct PruneResult {
    pub source_id: String,
    pub new_id: String,
    pub new_path: PathBuf,
    pub sidecar_path: Option<PathBuf>,
    pub copied_resources: Option<PathBuf>,
    pub pruned_count: usize,
    pub image_blocks: usize,
    pub original_size: u64,
    pub new_size: u64,
    pub est_context_tokens_saved: u64,
    /// `--revive`: how many `message.usage` records were corrected.
    pub usage_rewritten: usize,
    /// `--window`/`--range`: conversational turns dropped from the head/tail by the selection (0 if
    /// neither was used). The kept turns form the new session; the source keeps everything.
    pub dropped_turns: usize,
    /// `--window`: the real content-token size of the kept tail, from Claude's recorded `usage`
    /// (the figure the API will report on resume, ± system overhead). `None` for `--range`, or when
    /// the session had no usage records and the byte-estimate fallback was used.
    pub window_real_tokens: Option<u64>,
    /// `--revive`: the honest post-prune context-token figure written (None if revive off / nothing to do).
    pub revive_tokens: Option<u64>,
    /// `--revive`: the stale figure it replaced — the largest pre-revive usage total in the loaded window.
    pub revive_old_tokens: Option<u64>,
    pub dry_run: bool,
}

impl PruneResult {
    pub fn bytes_saved(&self) -> u64 {
        self.original_size.saturating_sub(self.new_size)
    }
}

/// One stashed payload, written as a line of `<new-id>.flat.jsonl` and read back by [`retrieve`].
#[derive(serde::Serialize, serde::Deserialize)]
struct SidecarEntry {
    id: String,
    slot: String, // "content" | "toolUseResult"
    name: String,
    input: Value,
    content: Value, // the ORIGINAL value, verbatim — lossless
    size: usize,
    line_count: usize,
    kind: String, // "text" | "image" | "mixed"
}

/// Size a `--window` tail by Claude's OWN recorded token counts — no estimator. Returns the turn to
/// keep from and the real content-token size of that tail, or `None` if the session has no `usage`
/// records (then the caller falls back to a byte estimate).
///
/// Why this is exact (not a guess): within the last compaction segment, a turn's recorded `usage`
/// (input + cache) IS the real context size at that point, monotonically increasing. The standalone
/// size of the tail kept from turn S is therefore `usage_end - usage[S-1]` — the shared prefix
/// cancels — so we pick the largest tail whose real size ≤ budget. Capped at the last segment so the
/// cancellation holds (older segments were compacted away and aren't a clean prefix). This is the
/// same arithmetic that predicts a resume exactly: a 250k-byte-estimate tail that loads as 373k real
/// is just `usage_end - usage[cutoff]`, which no byte ratio can know but the recorded numbers do.
fn usage_window_cutoff(lines: &[&str], budget: u64) -> Option<(usize, u64)> {
    // Per assistant turn that carries a usage record: (turn index, cumulative usage, starts a new
    // compaction segment). Cumulative usage is monotonic WITHIN a segment but resets across a
    // compaction boundary, so we can't subtract across the whole session — we work in per-turn deltas.
    let mut ti = 0usize;
    let mut recs: Vec<(usize, u64, bool)> = Vec::new();
    let mut boundary_pending = false;
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("user") | Some("assistant") => {
                // Prefer `_cv_orig_ctx` (the real count a prior revive stashed) over the live usage,
                // which revive may have overwritten with a pinned figure.
                let tot = v
                    .get("_cv_orig_ctx")
                    .and_then(Value::as_u64)
                    .or_else(|| {
                        v.get("message")
                            .and_then(|m| m.get("usage"))
                            .and_then(Value::as_object)
                            .map(usage_total)
                    })
                    .unwrap_or(0);
                if tot > 0 {
                    recs.push((ti, tot, boundary_pending));
                    boundary_pending = false;
                }
                ti += 1;
            }
            Some("system")
                if v.get("subtype").and_then(Value::as_str) == Some("compact_boundary") =>
            {
                boundary_pending = true;
            }
            _ => {}
        }
    }
    if recs.is_empty() {
        return None;
    }
    // Real token cost contributed by each recorded turn: within a segment it's the usage delta from
    // the previous record; at a segment start it's the whole post-compaction context (summary + that
    // turn) — real content that a resumed tail must re-send.
    let mut costs: Vec<(usize, u64)> = Vec::with_capacity(recs.len());
    let mut prev = 0u64;
    for (t, u, seg_start) in &recs {
        let cost = if *seg_start { *u } else { u.saturating_sub(prev) };
        costs.push((*t, cost));
        prev = *u;
    }
    // Accumulate real cost from the newest turn backward until the budget is met — this spans
    // compaction boundaries correctly (each segment's content counted once, via its deltas).
    let mut acc = 0u64;
    let mut start = recs.last().unwrap().0 + 1;
    for (t, c) in costs.iter().rev() {
        acc += c;
        start = *t;
        if acc >= budget {
            break;
        }
    }
    Some((start, acc))
}

/// Resolve which conversational turns (`[start, end)`) a `--window`/`--range` prune keeps, plus the
/// real token size of the kept tail when known (from `usage`). `--range` is explicit; `--window`
/// sizes by Claude's recorded usage (falling back to a byte estimate only when usage is absent),
/// snapped back to a user turn so the tail opens on a prompt. Neither set ⇒ the whole session.
fn select_kept_turns(
    lines: &[&str],
    opts: &PruneOptions,
    total_turns: usize,
) -> (usize, usize, Option<u64>) {
    if let Some((s, e)) = opts.keep_range {
        let end = e.unwrap_or(total_turns).min(total_turns);
        return (s.min(end), end, None);
    }
    let Some(budget) = opts.window else {
        return (0, total_turns, None);
    };

    let is_user: Vec<bool> = lines
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| match v.get("type").and_then(Value::as_str) {
            Some(t @ ("user" | "assistant")) => Some(t == "user"),
            _ => None,
        })
        .collect();
    let snap = |mut start: usize| {
        // Snap back to a user turn so the tail opens on a prompt, not a bare assistant reply.
        while start > 0 && !is_user.get(start).copied().unwrap_or(true) {
            start -= 1;
        }
        start
    };

    // PRINCIPLED path: size by Claude's own recorded token counts.
    if let Some((start, real)) = usage_window_cutoff(lines, budget) {
        return (snap(start), total_turns, Some(real));
    }

    // Fallback (no usage records): byte estimate of message.content, newest-backward to budget.
    let per_turn: Vec<u64> = lines
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| matches!(v.get("type").and_then(Value::as_str), Some("user" | "assistant")))
        .map(|v| v.get("message").and_then(|m| m.get("content")).map(est_tokens).unwrap_or(0))
        .collect();
    let mut acc = 0u64;
    let mut start = per_turn.len();
    for i in (0..per_turn.len()).rev() {
        acc += per_turn[i];
        start = i;
        if acc >= budget {
            break;
        }
    }
    (snap(start), total_turns, None)
}

/// Prune `src_path` (a Claude `<id>.jsonl`) into a new session. Returns what happened.
pub fn prune_session(src_path: &Path, opts: &PruneOptions) -> Result<PruneResult> {
    let raw = std::fs::read_to_string(src_path).with_context(|| format!("reading session {}", src_path.display()))?;
    let original_size = raw.len() as u64;
    let lines: Vec<&str> = raw.trim_end().split('\n').collect();

    let source_id = lines
        .iter()
        .find_map(|l| {
            serde_json::from_str::<Value>(l)
                .ok()?
                .get("sessionId")?
                .as_str()
                .map(String::from)
        })
        .unwrap_or_default();
    let new_id = opts.new_id.clone().unwrap_or_else(new_uuid);
    if new_id == source_id {
        bail!("new id must differ from the source id");
    }

    let dir = src_path.parent().unwrap_or(Path::new("."));
    let src_stem = src_path.file_stem().and_then(|s| s.to_str()).unwrap_or(&source_id);
    let new_path = dir.join(format!("{new_id}.jsonl"));
    let sidecar_path = dir.join(format!("{new_id}.flat.jsonl"));

    // Map tool_use_id -> (name, input) from assistant tool_use blocks (which use `id`, not
    // `tool_use_id`); tool_result blocks only carry the id, not the name.
    let tool_names = build_tool_name_map(&lines);

    // Conversational-turn index per line, so keep-last can spare the most recent turns. A line is
    // "old" (eligible to snip) iff its turn index is before the keep-last window.
    let total_turns = lines
        .iter()
        .filter(|l| {
            serde_json::from_str::<Value>(l)
                .ok()
                .and_then(|v| {
                    v.get("type")
                        .and_then(Value::as_str)
                        .map(|t| t == "user" || t == "assistant")
                })
                .unwrap_or(false)
        })
        .count();
    let keep_from = total_turns.saturating_sub(opts.keep_last);

    // `--window`/`--range`: select which conversational turns survive; the rest are dropped and the
    // first survivor is re-rooted after the loop. Defaults to the whole session (no-op).
    let (keep_start, keep_end, window_real) = select_kept_turns(&lines, opts, total_turns);
    let windowing = keep_start > 0 || keep_end < total_turns;
    let mut dropped_turns = 0usize;

    let mut out_lines: Vec<String> = Vec::with_capacity(lines.len());
    let mut sidecar: Vec<SidecarEntry> = Vec::new();
    let mut pruned_count = 0usize;
    let mut image_blocks = 0usize;
    let mut est_tokens_saved = 0u64;
    let mut turn = 0usize;

    for line in &lines {
        let Ok(mut v) = serde_json::from_str::<Value>(line) else {
            out_lines.push((*line).to_string()); // non-JSON line: verbatim
            continue;
        };

        // Stamp the new session id on every line that carries one.
        if v.get("sessionId").is_some() {
            v["sessionId"] = Value::String(new_id.clone());
        }

        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        let is_turn = ty == "user" || ty == "assistant";
        let this_turn = turn;
        if is_turn {
            turn += 1;
        }
        let is_old = is_turn && this_turn < keep_from;

        // `--window`/`--range`: drop turns (and the records between them) outside the kept range.
        if windowing && (this_turn < keep_start || this_turn >= keep_end) {
            if is_turn {
                dropped_turns += 1;
            }
            continue;
        }

        // Only old user messages carry snippable tool results.
        if is_old && ty == "user" {
            let mut modified = false;
            prune_user_line(
                &mut v,
                opts,
                &tool_names,
                &new_id,
                &mut sidecar,
                &mut pruned_count,
                &mut image_blocks,
                &mut est_tokens_saved,
                &mut modified,
            );
            out_lines.push(if modified {
                v.to_string()
            } else {
                reserialize_with_id(line, &new_id)
            });
            continue;
        }

        // `--thinking`: flatten the OLDEST assistant reasoning (recent thinking, within keep-last,
        // stays verbatim so the resumed agent keeps its live train of thought).
        if is_old && ty == "assistant" && opts.thinking {
            let mut modified = false;
            prune_assistant_thinking(
                &mut v,
                opts,
                &new_id,
                &mut sidecar,
                &mut pruned_count,
                &mut est_tokens_saved,
                &mut modified,
            );
            out_lines.push(if modified {
                v.to_string()
            } else {
                reserialize_with_id(line, &new_id)
            });
            continue;
        }

        // Untouched line — but still re-emit with the new session id when we changed it.
        out_lines.push(reserialize_with_id(line, &new_id));
    }

    // Re-root the first surviving turn: its parentUuid points at a now-dropped message, which would
    // break the resume chain. Null it so the kept tail is a valid standalone conversation.
    if windowing {
        for l in out_lines.iter_mut() {
            let Ok(mut v) = serde_json::from_str::<Value>(l) else {
                continue;
            };
            let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
            if ty == "user" || ty == "assistant" {
                if v.get("parentUuid").is_some_and(|p| !p.is_null()) {
                    v["parentUuid"] = Value::Null;
                    *l = v.to_string();
                }
                break;
            }
        }
    }

    // `--revive`: correct the stale recorded context size so the resume gate reads the honest
    // post-prune figure. Runs on the already-flattened lines, so the number reflects what's left.
    let (usage_rewritten, revive_tokens, revive_old_tokens) =
        if opts.revive { revive_usage(&mut out_lines, windowing, window_real) } else { (0, None, None) };

    let new_content = out_lines.join("\n") + "\n";
    let new_size = new_content.len() as u64;

    let sidecar_out = (!opts.drop && !sidecar.is_empty()).then(|| sidecar_path.clone());
    let copied = (opts.copy_resources && dir.join(src_stem).is_dir()).then(|| dir.join(&new_id));

    if !opts.dry_run {
        std::fs::write(&new_path, &new_content)
            .with_context(|| format!("writing new session {}", new_path.display()))?;
        if let Some(p) = &sidecar_out {
            let body: String = sidecar
                .iter()
                .map(|e| serde_json::to_string(e).unwrap_or_default() + "\n")
                .collect();
            std::fs::write(p, body).with_context(|| format!("writing sidecar {}", p.display()))?;
        }
        if let Some(dst) = &copied {
            copy_dir(&dir.join(src_stem), dst)
                .with_context(|| format!("copying session resources to {}", dst.display()))?;
        }
    }

    Ok(PruneResult {
        source_id,
        new_id,
        new_path,
        sidecar_path: sidecar_out,
        copied_resources: copied,
        pruned_count,
        image_blocks,
        original_size,
        new_size,
        est_context_tokens_saved: est_tokens_saved,
        usage_rewritten,
        dropped_turns,
        window_real_tokens: window_real,
        revive_tokens,
        revive_old_tokens,
        dry_run: opts.dry_run,
    })
}

/// Rewrite the trailing `message.usage` token counts to an honest post-prune figure (`--revive`).
///
/// Claude Code's resume gate reads the **last turn's recorded `usage`** (input + cache tokens) as the
/// session's current context size — and it does so *before* re-sending anything to the API. After a
/// prune the real content is small, but that recorded number is still the pre-prune total, so the gate
/// refuses to resume a session that would actually fit (the "979.7k / context limit reached" wall).
///
/// We recompute the honest size of the **loaded window** — everything after the last compaction
/// boundary, which is what Claude actually re-sends on resume — and rewrite every usage record in that
/// window that still over-reports, pinning it to the honest figure with the cache counters zeroed (a
/// fresh resume has no live cache anyway). Returns `(records rewritten, honest tokens, stale max)`.
fn revive_usage(
    out_lines: &mut [String],
    windowed: bool,
    real_override: Option<u64>,
) -> (usize, Option<u64>, Option<u64>) {
    let parsed: Vec<Option<Value>> = out_lines.iter().map(|l| serde_json::from_str::<Value>(l).ok()).collect();

    // Claude re-sends only what follows the last compaction boundary; before the first, the whole file.
    // BUT a `--window`/`--range` tail IS the whole resumable session — and it can still contain OLDER
    // compaction boundaries whose turns kept their original near-wall usage numbers. Honoring "last
    // boundary" there would leave those stale highs in place (Claude reads one ⇒ "0% remaining"). So
    // when windowed, treat the entire tail as the loaded window and pin EVERY over-reporting record.
    let window = if windowed {
        0
    } else {
        parsed
            .iter()
            .rposition(|p| {
                p.as_ref().is_some_and(|v| {
                    v.get("type").and_then(Value::as_str) == Some("system")
                        && v.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
                })
            })
            .map(|i| i + 1)
            .unwrap_or(0)
    };

    // Honest size of the loaded window: the tokens Claude will actually re-send. Prefer the REAL
    // figure derived from recorded `usage` (passed for `--window`); otherwise estimate from bytes.
    let honest: u64 = real_override.unwrap_or_else(|| {
        parsed[window..]
            .iter()
            .flatten()
            .filter_map(|v| v.get("message").and_then(|m| m.get("content")))
            .map(est_tokens)
            .sum()
    });

    let mut rewritten = 0usize;
    let mut stale_max = 0u64;
    for i in window..out_lines.len() {
        let Some(v) = &parsed[i] else { continue };
        let Some(usage) = v.pointer("/message/usage").and_then(Value::as_object) else {
            continue;
        };
        let total = usage_total(usage);
        if total <= honest {
            continue; // already honest (or smaller) — leave it
        }
        stale_max = stale_max.max(total);
        let mut v = v.clone();
        // Stash the ORIGINAL recorded context size before overwriting it, so a future `--window`
        // can still size this (now-revived) session by Claude's real counts instead of falling back
        // to a byte estimate. (Revive otherwise destroys the ground truth that windowing needs.)
        if v.get("_cv_orig_ctx").is_none() {
            v["_cv_orig_ctx"] = Value::from(total);
        }
        if let Some(u) = v.pointer_mut("/message/usage").and_then(Value::as_object_mut) {
            u.insert("input_tokens".into(), Value::from(honest));
            u.insert("cache_read_input_tokens".into(), Value::from(0u64));
            u.insert("cache_creation_input_tokens".into(), Value::from(0u64));
            if let Some(cc) = u.get_mut("cache_creation").and_then(Value::as_object_mut) {
                cc.insert("ephemeral_1h_input_tokens".into(), Value::from(0u64));
                cc.insert("ephemeral_5m_input_tokens".into(), Value::from(0u64));
            }
        }
        out_lines[i] = v.to_string();
        rewritten += 1;
    }

    if rewritten == 0 {
        (0, None, None)
    } else {
        (rewritten, Some(honest), Some(stale_max))
    }
}

/// Sum the context-bearing token counts of a Claude `usage` object (input + both cache buckets).
fn usage_total(usage: &serde_json::Map<String, Value>) -> u64 {
    ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"]
        .iter()
        .map(|k| usage.get(*k).and_then(Value::as_u64).unwrap_or(0))
        .sum()
}

/// Snip the eligible payloads on one (old) user line, in place.
#[allow(clippy::too_many_arguments)]
fn prune_user_line(
    v: &mut Value,
    opts: &PruneOptions,
    tool_names: &HashMap<String, (String, Value)>,
    new_id: &str,
    sidecar: &mut Vec<SidecarEntry>,
    count: &mut usize,
    images: &mut usize,
    tokens_saved: &mut u64,
    modified: &mut bool,
) {
    // (1) tool_result blocks in message.content (the context-bearing payloads).
    let mut line_tool_id: Option<String> = None;
    if let Some(blocks) = v.pointer_mut("/message/content").and_then(Value::as_array_mut) {
        for block in blocks.iter_mut() {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let tuid = block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            if line_tool_id.is_none() {
                line_tool_id = Some(tuid.clone());
            }
            // Decide using a borrow; commit by cloning the payload out (releasing the borrow), then
            // overwrite the block — so we never hold a read of `block` across the mutation.
            let committed = match block.get("content") {
                Some(c) if !c.is_null() && !c.as_str().is_some_and(|s| s.starts_with(MARKER_PREFIX)) => {
                    let size = value_byte_size(c);
                    if size <= opts.min_size {
                        None
                    } else {
                        let (kind, text, img) = classify(c);
                        if kind == "none" {
                            None
                        } else {
                            Some((c.clone(), size, kind, text, img))
                        }
                    }
                }
                _ => None,
            };
            let Some((content, size, kind, text, img)) = committed else {
                continue;
            };
            let line_count = if text.is_empty() { 1 } else { text.lines().count() };
            let (name, input) = tool_names
                .get(&tuid)
                .cloned()
                .unwrap_or_else(|| ("unknown".into(), Value::Null));
            let marker = build_marker(&tuid, &name, &input, &kind, size, line_count, new_id);

            *tokens_saved += est_tokens(&content).saturating_sub(str_tokens(&marker));
            *images += img;
            if !opts.drop {
                sidecar.push(SidecarEntry {
                    id: tuid.clone(),
                    slot: "content".into(),
                    name,
                    input,
                    content,
                    size,
                    line_count,
                    kind,
                });
            }
            block["content"] = Value::String(marker);
            *modified = true;
            *count += 1;
        }
    }

    // (2) the top-level toolUseResult mirror (disk-only; shrinks the file, not context). Clone it out
    // first so the borrow is released before we overwrite the field.
    let tur_committed = match v.get("toolUseResult") {
        Some(t) if !t.is_null() && !t.as_str().is_some_and(|s| s.starts_with(MARKER_PREFIX)) => {
            let size = value_byte_size(t);
            (size > opts.min_size).then(|| (t.clone(), size))
        }
        _ => None,
    };
    if let Some((tur, size)) = tur_committed {
        let base = line_tool_id.clone().unwrap_or_else(|| "line".into());
        let id = format!("{base}{TUR_SUFFIX}");
        let kind = if tur_is_image(&tur) { "image" } else { "text" };
        let (name, input) = line_tool_id
            .as_ref()
            .and_then(|t| tool_names.get(t).cloned())
            .unwrap_or_else(|| ("unknown".into(), Value::Null));
        let line_count = tur.as_str().map(|s| s.lines().count()).unwrap_or(1);
        let marker = build_marker(&id, &name, &input, kind, size, line_count, new_id);
        if !opts.drop {
            sidecar.push(SidecarEntry {
                id,
                slot: "toolUseResult".into(),
                name,
                input,
                content: tur,
                size,
                line_count,
                kind: kind.into(),
            });
        }
        v["toolUseResult"] = Value::String(marker);
        *modified = true;
        *count += 1;
    }
}

/// Flatten large `thinking` blocks on one (old) assistant line. Each is replaced by a small **text**
/// block carrying the marker (not a thinking block — keeping the original `signature` against altered
/// text would fail Claude's validation on resume), with the original block stashed verbatim in the
/// sidecar under `<uuid>#think<i>`.
#[allow(clippy::too_many_arguments)]
fn prune_assistant_thinking(
    v: &mut Value,
    opts: &PruneOptions,
    new_id: &str,
    sidecar: &mut Vec<SidecarEntry>,
    count: &mut usize,
    tokens_saved: &mut u64,
    modified: &mut bool,
) {
    let uuid = v.get("uuid").and_then(Value::as_str).unwrap_or("line").to_string();
    let Some(blocks) = v.pointer_mut("/message/content").and_then(Value::as_array_mut) else {
        return;
    };
    for (i, block) in blocks.iter_mut().enumerate() {
        if block.get("type").and_then(Value::as_str) != Some("thinking") {
            continue;
        }
        // Gate on the WHOLE block size, not the thinking text: Claude usually omits the reasoning
        // text in the transcript but keeps a ~600-byte cryptographic `signature` — that signature
        // (×thousands of turns) is what actually bloats the resumed context, so that's what we lift
        // out. Commit by cloning the block out (lossless), releasing the borrow before overwriting.
        let size = value_byte_size(block);
        if size <= opts.min_size {
            continue;
        }
        let line_count = block
            .get("thinking")
            .and_then(Value::as_str)
            .map(|t| t.lines().count().max(1))
            .unwrap_or(1);
        let original = block.clone();
        let id = format!("{uuid}#think{i}");
        let marker = build_marker(&id, "thinking", &Value::Null, "text", size, line_count, new_id);
        *tokens_saved += str_tokens(&original.to_string()).saturating_sub(str_tokens(&marker));
        if !opts.drop {
            sidecar.push(SidecarEntry {
                id,
                slot: "thinking".into(),
                name: "thinking".into(),
                input: Value::Null,
                content: original,
                size,
                line_count,
                kind: "text".into(),
            });
        }
        *block = serde_json::json!({ "type": "text", "text": marker });
        *modified = true;
        *count += 1;
    }
}

/// Retrieve a stashed original from a prune sidecar by its `tool_use_id` (or `<id>#tur`). Returns the
/// verbatim value (string, content-block array incl. images, or the raw toolUseResult object).
pub fn retrieve(sidecar_path: &Path, id: &str) -> Result<Value> {
    let raw =
        std::fs::read_to_string(sidecar_path).with_context(|| format!("reading sidecar {}", sidecar_path.display()))?;
    let mut available = Vec::new();
    let mut found = None;
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(entry) = serde_json::from_str::<SidecarEntry>(line) else {
            continue;
        };
        available.push(entry.id.clone());
        if entry.id == id {
            found = Some(entry.content); // last-wins
        }
    }
    found.ok_or_else(|| {
        let shown: Vec<_> = available.iter().take(20).cloned().collect();
        anyhow::anyhow!("id {id:?} not found in sidecar. available: {}", shown.join(", "))
    })
}

// ── helpers ───────────────────────────────────────────────────────────────

fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Re-serialize a line only to swap its `sessionId`; if it has none (or isn't JSON), return as-is so
/// untouched lines stay byte-identical.
fn reserialize_with_id(line: &str, new_id: &str) -> String {
    match serde_json::from_str::<Value>(line) {
        Ok(mut v) if v.get("sessionId").is_some() => {
            v["sessionId"] = Value::String(new_id.to_string());
            v.to_string()
        }
        _ => line.to_string(),
    }
}

fn build_tool_name_map(lines: &[&str]) -> HashMap<String, (String, Value)> {
    let mut map = HashMap::new();
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) else {
            continue;
        };
        for b in blocks {
            if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                if let Some(id) = b.get("id").and_then(Value::as_str) {
                    let name = b.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                    let input = b.get("input").cloned().unwrap_or(Value::Null);
                    map.insert(id.to_string(), (name, input));
                }
            }
        }
    }
    map
}

/// Classify a tool_result content value → (kind, projected text, image count).
fn classify(content: &Value) -> (String, String, usize) {
    match content {
        Value::String(s) => ("text".into(), s.clone(), 0),
        Value::Array(arr) => {
            let mut text = String::new();
            let mut images = 0;
            for b in arr {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(t);
                        }
                    }
                    Some("image") => images += 1,
                    _ => {}
                }
            }
            let kind = match (images > 0, !text.is_empty()) {
                (true, true) => "mixed",
                (true, false) => "image",
                (false, true) => "text",
                // a non-empty array of other block types is still stored verbatim (lossless)
                (false, false) if !arr.is_empty() => "text",
                _ => "none",
            };
            (kind.into(), text, images)
        }
        _ => ("none".into(), String::new(), 0),
    }
}

fn tur_is_image(tur: &Value) -> bool {
    if tur.get("type").and_then(Value::as_str) == Some("image") {
        return true;
    }
    if let Some(file) = tur.get("file") {
        if file.get("base64").is_some() {
            return true;
        }
        if file
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| t.starts_with("image/"))
        {
            return true;
        }
    }
    false
}

fn value_byte_size(v: &Value) -> usize {
    match v {
        Value::String(s) => s.len(),
        other => other.to_string().len(),
    }
}

fn str_tokens(s: &str) -> u64 {
    (s.len() as f64 / TEXT_BYTES_PER_TOKEN).ceil() as u64
}

/// Estimate the context-token cost of a content value (text + image tiles).
fn est_tokens(content: &Value) -> u64 {
    match content {
        Value::String(s) => str_tokens(s),
        Value::Array(arr) => arr
            .iter()
            .map(|b| match b.get("type").and_then(Value::as_str) {
                Some("image") => IMAGE_TOKEN_EST,
                Some("text") => str_tokens(b.get("text").and_then(Value::as_str).unwrap_or("")),
                _ => str_tokens(&b.to_string()),
            })
            .sum(),
        other => str_tokens(&other.to_string()),
    }
}

/// A compact key=value summary of the tool's most telling arguments, for the marker.
fn summarize_args(input: &Value) -> String {
    let Some(obj) = input.as_object() else {
        return String::new();
    };
    let key_args = [
        "file_path",
        "command",
        "pattern",
        "query",
        "url",
        "path",
        "description",
        "prompt",
    ];
    let mut picked: Vec<String> = obj
        .iter()
        .filter(|(k, _)| key_args.contains(&k.as_str()))
        .take(2)
        .map(|(k, v)| fmt_arg(k, v))
        .collect();
    if picked.is_empty() {
        if let Some((k, v)) = obj.iter().next() {
            picked.push(fmt_arg(k, v));
        }
    }
    picked.join(", ")
}

fn fmt_arg(k: &str, v: &Value) -> String {
    let s = v.as_str().map(String::from).unwrap_or_else(|| v.to_string());
    // Truncate on a char boundary (values can contain multibyte UTF-8).
    let s = if s.chars().count() > 80 {
        format!("{}...", s.chars().take(77).collect::<String>())
    } else {
        s
    };
    format!("{k}={s}")
}

fn build_marker(id: &str, name: &str, input: &Value, kind: &str, size: usize, lines: usize, session: &str) -> String {
    let args = summarize_args(input);
    let args = if args.is_empty() {
        String::new()
    } else {
        format!(" {args}")
    };
    let label = match kind {
        "image" => "image",
        "mixed" => "text+image",
        _ => "text",
    };
    format!("{MARKER_PREFIX}{id} tool={name}{args} | {label} {size}B/{lines}L | session={session} | retrieve: cv prune {session} --retrieve {id}]")
}

/// Recursively copy a directory tree (used to carry `subagents/`/`workflows/` into the new session).
fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
