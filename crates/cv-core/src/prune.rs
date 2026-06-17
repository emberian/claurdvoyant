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
    /// Explicit new session id; otherwise a fresh UUIDv4.
    pub new_id: Option<String>,
    /// Also copy the session's `subagents/`/`workflows/` resource dir under the new id. Off by
    /// default — `claude --resume` doesn't need it, and for big sessions it can be hundreds of MB /
    /// thousands of files. Turn on if you want cv's forest features to work on the pruned session.
    pub copy_resources: bool,
    /// Compute and report without writing anything.
    pub dry_run: bool,
}

impl Default for PruneOptions {
    fn default() -> Self {
        PruneOptions { min_size: 2048, keep_last: 25, drop: false, new_id: None, copy_resources: false, dry_run: false }
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

/// Prune `src_path` (a Claude `<id>.jsonl`) into a new session. Returns what happened.
pub fn prune_session(src_path: &Path, opts: &PruneOptions) -> Result<PruneResult> {
    let raw = std::fs::read_to_string(src_path)
        .with_context(|| format!("reading session {}", src_path.display()))?;
    let original_size = raw.len() as u64;
    let lines: Vec<&str> = raw.trim_end().split('\n').collect();

    let source_id = lines
        .iter()
        .find_map(|l| serde_json::from_str::<Value>(l).ok()?.get("sessionId")?.as_str().map(String::from))
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
                .and_then(|v| v.get("type").and_then(Value::as_str).map(|t| t == "user" || t == "assistant"))
                .unwrap_or(false)
        })
        .count();
    let keep_from = total_turns.saturating_sub(opts.keep_last);

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

        // Only old user messages carry snippable tool results.
        if is_old && ty == "user" {
            let mut modified = false;
            prune_user_line(&mut v, opts, &tool_names, &new_id, &mut sidecar, &mut pruned_count, &mut image_blocks, &mut est_tokens_saved, &mut modified);
            out_lines.push(if modified { v.to_string() } else { reserialize_with_id(line, &new_id) });
            continue;
        }

        // Untouched line — but still re-emit with the new session id when we changed it.
        out_lines.push(reserialize_with_id(line, &new_id));
    }

    let new_content = out_lines.join("\n") + "\n";
    let new_size = new_content.len() as u64;

    let sidecar_out = (!opts.drop && !sidecar.is_empty()).then(|| sidecar_path.clone());
    let copied = (opts.copy_resources && dir.join(src_stem).is_dir()).then(|| dir.join(&new_id));

    if !opts.dry_run {
        std::fs::write(&new_path, &new_content)
            .with_context(|| format!("writing new session {}", new_path.display()))?;
        if let Some(p) = &sidecar_out {
            let body: String = sidecar.iter().map(|e| serde_json::to_string(e).unwrap_or_default() + "\n").collect();
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
        dry_run: opts.dry_run,
    })
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
            let tuid = block.get("tool_use_id").and_then(Value::as_str).unwrap_or("unknown").to_string();
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
                        if kind == "none" { None } else { Some((c.clone(), size, kind, text, img)) }
                    }
                }
                _ => None,
            };
            let Some((content, size, kind, text, img)) = committed else { continue };
            let line_count = if text.is_empty() { 1 } else { text.lines().count() };
            let (name, input) = tool_names.get(&tuid).cloned().unwrap_or_else(|| ("unknown".into(), Value::Null));
            let marker = build_marker(&tuid, &name, &input, &kind, size, line_count, new_id);

            *tokens_saved += est_tokens(&content).saturating_sub(str_tokens(&marker));
            *images += img;
            if !opts.drop {
                sidecar.push(SidecarEntry {
                    id: tuid.clone(), slot: "content".into(), name, input,
                    content, size, line_count, kind,
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
                id, slot: "toolUseResult".into(), name, input,
                content: tur, size, line_count, kind: kind.into(),
            });
        }
        v["toolUseResult"] = Value::String(marker);
        *modified = true;
        *count += 1;
    }
}

/// Retrieve a stashed original from a prune sidecar by its `tool_use_id` (or `<id>#tur`). Returns the
/// verbatim value (string, content-block array incl. images, or the raw toolUseResult object).
pub fn retrieve(sidecar_path: &Path, id: &str) -> Result<Value> {
    let raw = std::fs::read_to_string(sidecar_path)
        .with_context(|| format!("reading sidecar {}", sidecar_path.display()))?;
    let mut available = Vec::new();
    let mut found = None;
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(entry) = serde_json::from_str::<SidecarEntry>(line) else { continue };
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
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) else { continue };
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
        if file.get("type").and_then(Value::as_str).is_some_and(|t| t.starts_with("image/")) {
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
    let Some(obj) = input.as_object() else { return String::new() };
    let key_args = ["file_path", "command", "pattern", "query", "url", "path", "description", "prompt"];
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
    let args = if args.is_empty() { String::new() } else { format!(" {args}") };
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
