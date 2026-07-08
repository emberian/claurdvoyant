//! Claude Code `Workflow`-tool runs, parsed as **first-class structured objects** — not merely a
//! tier of sub-agents.
//!
//! A `Workflow` tool invocation runs a JS *script* that fans out a phased tree of sub-agents and
//! aggregates their structured returns. Claude Code records each run three ways under the session's
//! sidecar dir (`<projects>/<encoded>/<sessionId>/`):
//!
//! 1. **`workflows/wf_<runId>.json`** — the run's *state*: `workflowName`, `status`
//!    (`completed`/`killed`), the inline `script` + its `scriptPath`, `phases[]` (`{title, detail}`),
//!    a `workflowProgress[]` interleaving `workflow_phase` + `workflow_agent` entries (the phase tree
//!    with every agent's `state`/`tokens`/`toolCalls`/`resultPreview`/`promptPreview` under its
//!    phase), `agentCount`/`totalTokens`/`totalToolCalls`/`durationMs`, and the aggregated `result`.
//! 2. **`workflows/scripts/<name>-wf_<runId>.js`** — the driving script on disk (same bytes as the
//!    inline `script`, but the file is the durable copy and the resume entry point).
//! 3. **`subagents/workflows/wf_<runId>/`** — each agent's full transcript (`agent-*.jsonl`) +
//!    `journal.jsonl` (the orchestrator's structured per-agent results). This tier is what
//!    [`super::claude::subagent_tree`] already walks; here we add the *state/phase/script* layer.
//!
//! This module reads (1) and (2) into a [`Workflow`] so `cv workflow <runId>` can show
//! phases → agents → outcomes + the script — making workflows queryable on their own terms.

use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// One run of the `Workflow` tool, read from `workflows/wf_<runId>.json` (+ its on-disk script).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Workflow {
    /// The run id (`wf_<…>`), matching the `subagents/workflows/<runId>/` dir.
    pub run_id: String,
    /// `workflowName` — the script's `meta.name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `status`: `completed`, `killed`, … (the orchestrator's terminal state for the run).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// The run's freeform `summary` (the orchestrator's own roll-up line).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// `error` text when the run did not complete (`status: killed`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `defaultModel` the workflow launched its agents with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Number of agents the run spawned (`agentCount`).
    pub agent_count: u64,
    /// Aggregate `totalTokens` across the run's agents.
    pub total_tokens: u64,
    /// Aggregate `totalToolCalls` across the run's agents.
    pub total_tool_calls: u64,
    /// Wall-clock `durationMs` of the whole run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Where the driving script lives on disk (`scriptPath`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_path: Option<PathBuf>,
    /// The driving script source (the inline `script` — same bytes as `script_path`'s file).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// The run this one resumed from (`resumeFromRunId`), for resumed/cached workflows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_from: Option<String>,
    /// The workflow script's aggregated **return value** (`result`) — what the script's final
    /// `return` produced. The harvest payload; a run without it either returned nothing or died.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The script's `log()` lines, in order — the run's own progress narration (includes per-agent
    /// failure notes like rate-limit hits).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<String>,
    /// The `args` value the workflow was invoked with (verbatim; may itself be a JSON-encoded
    /// string if the caller stringified).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    /// The background task id (`taskId`) — matches the "Task ID: …" in the launch tool_result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// When the run started (`startTime` epoch-ms, falling back to the ISO `timestamp`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The declared phases, in order, each with the agents that ran under it (from
    /// `workflowProgress`). The phase tree the brief asks to surface richly.
    pub phases: Vec<WorkflowPhase>,
    /// Agents whose `workflowProgress` entry named a `phaseIndex` with no matching phase header
    /// (defensive — empty in practice). Surfaced so no agent is silently dropped.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub orphan_agents: Vec<WorkflowAgent>,
}

/// One declared phase of a workflow (`phases[]` entry) with the agents that ran under it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkflowPhase {
    /// 1-based phase index (matches `workflow_agent.phaseIndex`).
    pub index: u64,
    /// The phase title (`phases[].title` / `workflow_phase.title`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The phase's `detail` blurb from the script's `meta.phases`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The agents that ran under this phase, in spawn order.
    pub agents: Vec<WorkflowAgent>,
}

/// One agent within a workflow run — a `workflow_agent` entry of `workflowProgress`. Carries the
/// agent's live telemetry (`state`/`tokens`/`toolCalls`) and the previews the orchestrator kept of
/// its prompt and structured result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkflowAgent {
    /// 1-based agent index across the run.
    pub index: u64,
    /// The agent's short label (`label`), e.g. `migrate:stark→p3`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The `agent-<id>` id (bare, no prefix) — the key into `subagents/workflows/<run>/` and the
    /// journal. `None` for a queued-but-never-started slot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// 1-based phase index this agent ran under.
    pub phase_index: u64,
    /// `state`: `done` / `error` / `progress` / `start` (live or terminal).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// The model this agent ran on (may differ from the workflow default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Tokens this agent consumed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    /// Tool calls this agent made.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u64>,
    /// This agent's wall-clock `durationMs`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// `error` text when the agent's `state` is `error`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Whether this agent's result was served from cache (a resumed run).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cached: bool,
    /// Head of the prompt the orchestrator handed this agent (`promptPreview`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_preview: Option<String>,
    /// Head of the agent's structured return value (`resultPreview`) — the real outcome.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_preview: Option<String>,
    /// Retry attempt number (`attempt`), 1-based; >1 means the agent was re-issued.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u64>,
    /// When the agent started running (`startedAt`, epoch-ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The agent's last recorded heartbeat (`lastProgressAt`, epoch-ms). For a non-terminal agent
    /// (crash/kill mid-run), this is when it was last alive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_progress_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The last tool the agent was seen using (`lastToolName`) — for a dead/interrupted agent,
    /// where it was when it stopped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tool_name: Option<String>,
    /// One-line summary of that last tool call (`lastToolSummary`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tool_summary: Option<String>,
    /// The agent's **full** journaled return value, from the run's `journal.jsonl` — the complete
    /// structured result, where `result_preview` is a ~400-char head. Populated by
    /// [`Workflow::attach_journal`], not by the state-file parse.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub journal_result: Option<Value>,
}

impl Workflow {
    /// Total agents across all phases (+ any orphans) — the phase-tree count, which matches
    /// `agent_count` for a well-formed run.
    pub fn agents_in_phases(&self) -> usize {
        self.phases.iter().map(|p| p.agents.len()).sum::<usize>() + self.orphan_agents.len()
    }

    /// A histogram of agent terminal states (`done`/`error`/…) across the run, for a one-glance
    /// "how did this workflow go" summary.
    pub fn state_counts(&self) -> BTreeMap<String, usize> {
        let mut m = BTreeMap::new();
        for a in self.phases.iter().flat_map(|p| &p.agents).chain(&self.orphan_agents) {
            if let Some(s) = &a.state {
                *m.entry(s.clone()).or_insert(0) += 1;
            }
        }
        m
    }

    /// Enrich every agent with its **full** journaled return value from the run's
    /// `subagents/workflows/<runId>/journal.jsonl` (the state file only keeps a ~400-char
    /// `resultPreview`). No-op when the journal is absent (e.g. a crashed run).
    pub fn attach_journal(&mut self, parent_path: &Path) {
        let results = journal_results(parent_path, &self.run_id);
        if results.is_empty() {
            return;
        }
        for a in self
            .phases
            .iter_mut()
            .flat_map(|p| &mut p.agents)
            .chain(&mut self.orphan_agents)
        {
            if let Some(id) = &a.agent_id {
                a.journal_result = results.get(id).cloned();
            }
        }
    }
}

/// A run's `journal.jsonl` reduced to `agentId → full result Value` (the LAST result per agent —
/// a re-issued agent journals more than once). Empty when the journal is absent or unreadable.
pub fn journal_results(parent_path: &Path, run_id: &str) -> BTreeMap<String, Value> {
    let mut map = BTreeMap::new();
    let Some(stem) = parent_path.file_stem().and_then(|s| s.to_str()) else {
        return map;
    };
    let Some(journal) = parent_path.parent().map(|d| {
        d.join(stem)
            .join("subagents")
            .join("workflows")
            .join(run_id)
            .join("journal.jsonl")
    }) else {
        return map;
    };
    let Ok(txt) = fs::read_to_string(&journal) else {
        return map;
    };
    for line in txt.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("result") {
            continue;
        }
        if let (Some(id), Some(r)) = (v.get("agentId").and_then(Value::as_str), v.get("result")) {
            map.insert(id.to_string(), r.clone());
        }
    }
    map
}

/// The directory holding a session's workflow *state* files: `<sessionId>/workflows/`. (Distinct
/// from `<sessionId>/subagents/workflows/`, which holds the per-run agent transcripts.)
fn workflows_dir(parent_path: &Path) -> Option<PathBuf> {
    let stem = parent_path.file_stem()?.to_str()?;
    parent_path.parent().map(|d| d.join(stem).join("workflows"))
}

/// Every workflow run a session launched, read from its `workflows/wf_*.json` state files (newest
/// first). Each is a fully-structured [`Workflow`] (phases → agents → outcomes + script). Returns
/// empty when the session spawned no workflows (or its sidecar dir is absent).
pub fn workflows(parent_path: &Path) -> Vec<Workflow> {
    let Some(dir) = workflows_dir(parent_path) else {
        return vec![];
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            let is_state = p.extension().and_then(|x| x.to_str()) == Some("json")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("wf_"));
            if is_state {
                paths.push(p);
            }
        }
    }
    let mut out = crate::par_filter_map(paths, |p| parse_state_file(&p));
    // Newest first, by file mtime (the JSON carries an ISO `timestamp` too, but mtime needs no parse).
    out.sort_by_key(|w| {
        std::cmp::Reverse(
            workflows_dir(parent_path)
                .map(|d| d.join(format!("{}.json", w.run_id)))
                .and_then(|p| fs::metadata(p).ok())
                .and_then(|m| m.modified().ok()),
        )
    });
    out
}

/// One workflow run by its `runId` **or its workflow name**, or `None` if nothing matches
/// unambiguously. Run ids accept the bare/`wf_`-prefixed form and unique prefixes; names accept an
/// exact match (newest run wins when a name was re-run) and a unique name-prefix.
pub fn workflow(parent_path: &Path, key: &str) -> Option<Workflow> {
    let want = key.strip_prefix("wf_").unwrap_or(key);
    let all = workflows(parent_path);

    let mut hits: Vec<Workflow> = all
        .iter()
        .filter(|w| {
            let id = w.run_id.strip_prefix("wf_").unwrap_or(&w.run_id);
            id == want || w.run_id == key || id.starts_with(want)
        })
        .cloned()
        .collect();
    // Exact id match wins over a prefix collision.
    if let Some(exact) = hits.iter().position(|w| {
        let id = w.run_id.strip_prefix("wf_").unwrap_or(&w.run_id);
        id == want || w.run_id == key
    }) {
        return Some(hits.swap_remove(exact));
    }
    if hits.len() == 1 {
        return hits.pop();
    }

    // No id match → try the workflow NAME. `all` is newest-first, so the first exact-name hit is
    // the newest run of a re-run name — the one you want when addressing by name.
    if let Some(w) = all.iter().find(|w| w.name.as_deref() == Some(key)) {
        return Some(w.clone());
    }
    let name_hits: Vec<&Workflow> = all
        .iter()
        .filter(|w| w.name.as_deref().is_some_and(|n| n.starts_with(key)))
        .collect();
    // A unique name-prefix — unique as a NAME, not as a run (re-runs of one name still resolve;
    // `all` is newest-first so the first hit is the newest run of that name).
    let distinct: std::collections::HashSet<&str> = name_hits.iter().filter_map(|w| w.name.as_deref()).collect();
    if distinct.len() == 1 {
        return name_hits.first().map(|w| (*w).clone());
    }
    None
}

/// The `(name, run_id)` pairs readable from `workflows/scripts/*.js` **filenames** alone — the
/// zero-parse half of the run-name index (the persisted basename is `<name>-wf_<runId>.js`).
fn scripts_index(parent_path: &Path) -> Vec<(String, String)> {
    let Some(dir) = workflows_dir(parent_path).map(|d| d.join("scripts")) else {
        return vec![];
    };
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.flatten() {
            if let Some(stem) = e.path().file_stem().and_then(|s| s.to_str()) {
                if let (Some(name), Some(rid)) = script_path_identity(stem) {
                    if rid.len() > "wf_".len() {
                        out.push((name, rid));
                    }
                }
            }
        }
    }
    out
}

/// Cheap, complete `(name, run_id)` index over a session's recorded runs — WITHOUT parsing the
/// (often multi-MB) state JSONs. Names come from the script filenames where a script was
/// persisted; the rest fall back to a byte-scan of the state file for its `"workflowName":"…"`
/// (the files are compact single-line JSON, so the pattern is stable). Sessions with no
/// `workflows/` dir return empty at the cost of one failed `read_dir`.
pub fn run_names(parent_path: &Path) -> Vec<(String, String)> {
    let Some(dir) = workflows_dir(parent_path) else {
        return vec![];
    };
    let by_script: BTreeMap<String, String> = scripts_index(parent_path).into_iter().map(|(n, r)| (r, n)).collect();
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            let Some(rid) = p
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|n| n.starts_with("wf_") && n.ends_with(".json"))
                .map(|n| n.trim_end_matches(".json").to_string())
            else {
                continue;
            };
            let name = by_script.get(&rid).cloned().or_else(|| {
                fs::read_to_string(&p)
                    .ok()
                    .and_then(|txt| scan_json_str_field(&txt, "workflowName"))
            });
            if let Some(name) = name {
                out.push((name, rid));
            }
        }
    }
    out
}

/// Extract a top-ish-level string field from compact JSON by byte-scan (`"key":"value"`), without
/// parsing. Used as a prefilter/index only — anything acting on the value re-parses properly.
fn scan_json_str_field(txt: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let start = txt.find(&pat)? + pat.len();
    let rest = &txt[start..];
    // Workflow names come from `meta.name` (plain identifiers); stop at the first unescaped quote.
    let mut end = 0;
    let bytes = rest.as_bytes();
    while end < bytes.len() {
        match bytes[end] {
            b'"' => return Some(rest[..end].to_string()),
            b'\\' => end += 2,
            _ => end += 1,
        }
    }
    None
}

/// Script files whose run has **no state file** — a launch that persisted its script (written at
/// launch) but whose state JSON never made it to disk. The on-disk half of ghost evidence, and
/// the only place a ghost's run id (→ its `subagents/workflows/<runId>/` debris dir) survives.
pub fn orphan_scripts(parent_path: &Path) -> Vec<(String, String)> {
    let Some(dir) = workflows_dir(parent_path) else {
        return vec![];
    };
    scripts_index(parent_path)
        .into_iter()
        .filter(|(_, rid)| !dir.join(format!("{rid}.json")).exists())
        .collect()
}

/// Whether a session recorded anything workflow-shaped at all (state files OR persisted scripts)
/// — the cheap bound for fleet-wide scans, replacing a full parse of every state file.
pub fn has_workflow_records(parent_path: &Path) -> bool {
    let Some(dir) = workflows_dir(parent_path) else {
        return false;
    };
    let any_state = fs::read_dir(&dir).into_iter().flatten().flatten().any(|e| {
        e.file_name()
            .to_str()
            .is_some_and(|n| n.starts_with("wf_") && n.ends_with(".json"))
    });
    any_state
        || fs::read_dir(dir.join("scripts"))
            .map(|mut rd| rd.next().is_some())
            .unwrap_or(false)
}

/// The runs whose name starts with `prefix`, fully parsed — but ONLY those: the cheap
/// [`run_names`] index picks the candidates, so non-matching (multi-MB) state files are never
/// parsed. The fleet-wide by-name search rides on this.
pub fn workflows_named(parent_path: &Path, prefix: &str) -> Vec<Workflow> {
    let Some(dir) = workflows_dir(parent_path) else {
        return vec![];
    };
    run_names(parent_path)
        .into_iter()
        .filter(|(n, _)| n.starts_with(prefix))
        .filter_map(|(_, rid)| parse_state_file(&dir.join(format!("{rid}.json"))))
        .collect()
}

/// Parse one `wf_<runId>.json` state file into a [`Workflow`]. Tolerant: a missing/odd field is an
/// absent option, never a failure (the catalog/forest views degrade gracefully).
fn parse_state_file(path: &Path) -> Option<Workflow> {
    let txt = fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&txt).ok()?;
    Some(parse_state(&v))
}

/// Parse a workflow state `Value` into a [`Workflow`] (pure — the testable core of
/// [`parse_state_file`]).
pub(crate) fn parse_state(v: &Value) -> Workflow {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let u = |k: &str| v.get(k).and_then(Value::as_u64);
    let run_id = s("runId").unwrap_or_default();

    // Build the phase tree from `workflowProgress`: `workflow_phase` entries declare the ordered
    // headers; `workflow_agent` entries slot under their `phaseIndex`. We also fold in the
    // `phases[]` array for the per-phase `detail` (which `workflowProgress` headers omit).
    let progress = v.get("workflowProgress").and_then(Value::as_array);
    let phase_details = phase_detail_map(v);

    // Ordered phase headers (index → title), preserving first-seen order.
    let mut phase_order: Vec<u64> = Vec::new();
    let mut phase_title: BTreeMap<u64, Option<String>> = BTreeMap::new();
    if let Some(arr) = progress {
        for e in arr {
            if e.get("type").and_then(Value::as_str) == Some("workflow_phase") {
                if let Some(idx) = e.get("index").and_then(Value::as_u64) {
                    if !phase_title.contains_key(&idx) {
                        phase_order.push(idx);
                    }
                    phase_title.insert(idx, e.get("title").and_then(Value::as_str).map(str::to_string));
                }
            }
        }
    }
    // Fall back to the declared `phases[]` (1-based) when `workflowProgress` had no phase headers.
    if phase_order.is_empty() {
        if let Some(arr) = v.get("phases").and_then(Value::as_array) {
            for (i, p) in arr.iter().enumerate() {
                let idx = (i + 1) as u64;
                phase_order.push(idx);
                phase_title.insert(idx, p.get("title").and_then(Value::as_str).map(str::to_string));
            }
        }
    }

    // Agents grouped by phaseIndex (in spawn order).
    let mut by_phase: BTreeMap<u64, Vec<WorkflowAgent>> = BTreeMap::new();
    let mut orphan_agents: Vec<WorkflowAgent> = Vec::new();
    if let Some(arr) = progress {
        for e in arr {
            if e.get("type").and_then(Value::as_str) == Some("workflow_agent") {
                let agent = parse_agent(e);
                let pidx = agent.phase_index;
                if phase_title.contains_key(&pidx) {
                    by_phase.entry(pidx).or_default().push(agent);
                } else if pidx == 0 {
                    // No phaseIndex recorded → treat as a phase-less agent only if there are no
                    // phases at all; otherwise it's an orphan worth surfacing.
                    if phase_order.is_empty() {
                        orphan_agents.push(agent);
                    } else {
                        by_phase.entry(0).or_default().push(agent);
                        if !phase_order.contains(&0) {
                            phase_order.insert(0, 0);
                        }
                    }
                } else {
                    orphan_agents.push(agent);
                }
            }
        }
    }

    let phases = phase_order
        .into_iter()
        .map(|idx| WorkflowPhase {
            index: idx,
            title: phase_title.get(&idx).cloned().flatten(),
            detail: phase_details.get(&idx).cloned().flatten(),
            agents: by_phase.remove(&idx).unwrap_or_default(),
        })
        .collect();

    Workflow {
        run_id,
        name: s("workflowName"),
        status: s("status"),
        summary: s("summary"),
        error: s("error"),
        default_model: s("defaultModel"),
        agent_count: u("agentCount").unwrap_or(0),
        total_tokens: u("totalTokens").unwrap_or(0),
        total_tool_calls: u("totalToolCalls").unwrap_or(0),
        duration_ms: u("durationMs"),
        script_path: s("scriptPath").map(PathBuf::from),
        script: s("script"),
        resume_from: s("resumeFromRunId"),
        result: v.get("result").filter(|r| !r.is_null()).cloned(),
        logs: v
            .get("logs")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
        args: v.get("args").filter(|a| !a.is_null()).cloned(),
        task_id: s("taskId"),
        started_at: u("startTime").and_then(ms_to_utc).or_else(|| {
            s("timestamp")
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
                .map(|d| d.with_timezone(&chrono::Utc))
        }),
        phases,
        orphan_agents,
    }
}

/// Epoch-milliseconds → UTC instant (the state files' `startTime`/`startedAt`/… shape).
fn ms_to_utc(ms: u64) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::from_timestamp_millis(ms as i64)
}

/// Map phase index (1-based) → `detail` from the state's declared `phases[]` array.
fn phase_detail_map(v: &Value) -> BTreeMap<u64, Option<String>> {
    let mut m = BTreeMap::new();
    if let Some(arr) = v.get("phases").and_then(Value::as_array) {
        for (i, p) in arr.iter().enumerate() {
            m.insert(
                (i + 1) as u64,
                p.get("detail").and_then(Value::as_str).map(str::to_string),
            );
        }
    }
    m
}

// ===================== ghost launches =====================

/// One `Workflow` tool invocation as recorded in the session **transcript** — the launch-side
/// record, independent of whether a `workflows/wf_*.json` state file ever got persisted.
///
/// The distinction matters for crash forensics: a run's state file is written *by the live
/// harness*, so a power loss / hard kill can leave a launch visible in the transcript with **no
/// run record at all** (its sub-agent work, if any, sits unindexed under
/// `subagents/workflows/`). [`ghost_launches`] surfaces exactly those.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct WorkflowLaunch {
    /// Transcript timestamp of the tool_use (UTC).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<chrono::DateTime<chrono::Utc>>,
    /// The workflow's name: the inline script's `meta.name`, the `name` input (a saved workflow),
    /// or the `<name>` half of a `scriptPath` basename (`<name>-wf_<runId>.js`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The `wf_<runId>` embedded in a `scriptPath` relaunch's basename — identifies the run family
    /// the relaunch belongs to (its state file, when the family completed any run).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_run_id: Option<String>,
    /// The `resumeFromRunId` input, when this launch resumed a prior run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_from: Option<String>,
    /// The launch's paired tool_result was an error (`<tool_use_error>…`) — the run never started.
    pub errored: bool,
    /// For a GHOST: the run id recovered from its orphaned script file (`scripts/<name>-wf_….js`
    /// is written at launch and survives the crash that ate the state file). The key into the
    /// debris dir.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// For a GHOST with a recovered run id: how many `agent-*.jsonl` transcripts sit in its
    /// `subagents/workflows/<runId>/` debris dir — the harvestable work.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debris_agents: Option<usize>,
    /// For a GHOST with a recovered run id: how many full agent results its `journal.jsonl`
    /// recorded before the crash.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub journal_result_count: Option<usize>,
}

/// Every `Workflow` tool invocation in a session's transcript, in order. Reads the `.jsonl`
/// directly (cheap substring prefilter, then a JSON parse per candidate line).
pub fn launches(session_path: &Path) -> Vec<WorkflowLaunch> {
    let Ok(file) = fs::File::open(session_path) else {
        return vec![];
    };
    use std::io::BufRead;
    let reader = std::io::BufReader::new(file);
    // tool_use_id → index into `out`, so the (later) tool_result line can set `errored`.
    let mut by_use_id: BTreeMap<String, usize> = BTreeMap::new();
    let mut out: Vec<WorkflowLaunch> = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { break };
        // Prefilter, cheapest test first: a launch line names the tool; a result line is only
        // worth parsing when we still await a pending tool_use_id AND that id appears. Matching
        // on the bare "tool_use_id" literal would JSON-parse nearly every tool-result line of the
        // transcript; scanning for pending ids on lines without "tool_use_id" would rescan huge
        // result payloads. Pending ids are removed once resolved, so the set stays ≤ a few.
        let is_launch = line.contains("\"Workflow\"");
        if !is_launch
            && (by_use_id.is_empty()
                || !line.contains("tool_use_id")
                || !by_use_id.keys().any(|id| line.contains(id.as_str())))
        {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let ts = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&chrono::Utc));
        let Some(content) = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for c in content {
            match c.get("type").and_then(Value::as_str) {
                Some("tool_use") if c.get("name").and_then(Value::as_str) == Some("Workflow") => {
                    let input = c.get("input");
                    let g = |k: &str| input.and_then(|i| i.get(k)).and_then(Value::as_str);
                    let (path_name, path_run) = g("scriptPath").map(script_path_identity).unwrap_or((None, None));
                    let launch = WorkflowLaunch {
                        ts,
                        name: g("script")
                            .and_then(meta_name)
                            .or_else(|| g("name").map(str::to_string))
                            .or(path_name),
                        script_run_id: path_run,
                        resume_from: g("resumeFromRunId").map(str::to_string),
                        errored: false,
                        run_id: None,
                        debris_agents: None,
                        journal_result_count: None,
                    };
                    if let Some(id) = c.get("id").and_then(Value::as_str) {
                        by_use_id.insert(id.to_string(), out.len());
                    }
                    out.push(launch);
                }
                Some("tool_result") => {
                    // Resolve (and retire) the pending launch — keeping the pending set small is
                    // what keeps the prefilter above cheap.
                    if let Some(idx) = c
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .and_then(|id| by_use_id.remove(id))
                    {
                        if c.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
                            out[idx].errored = true;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Transcript launches with **no recorded run**: launched successfully per the transcript, but no
/// `workflows/wf_*.json` state file accounts for them — the signature of a crash / power loss /
/// hard kill before the harness persisted (or synced) the run's state.
///
/// Each ghost is enriched from what DID survive: its orphaned script file (written at launch)
/// recovers the run id, which keys the `subagents/workflows/<runId>/` debris dir — agent
/// transcripts and journaled results produced before the crash, i.e. the harvestable work.
pub fn ghost_launches(session_path: &Path) -> Vec<WorkflowLaunch> {
    let mut ghosts = compute_ghosts(launches(session_path), &workflows(session_path));
    let mut orphans = orphan_scripts(session_path);
    let stem_dir = session_path
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|stem| session_path.parent().map(|d| d.join(stem)));
    for g in &mut ghosts {
        let Some(name) = g.name.as_deref() else { continue };
        // Consume one same-named orphan script per ghost (each launch persists its own script).
        let Some(pos) = orphans.iter().position(|(n, _)| n == name) else {
            continue;
        };
        let (_, rid) = orphans.swap_remove(pos);
        if let Some(base) = &stem_dir {
            let debris = base.join("subagents").join("workflows").join(&rid);
            g.debris_agents = Some(
                fs::read_dir(&debris)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|e| {
                        e.file_name()
                            .to_str()
                            .is_some_and(|n| n.starts_with("agent-") && n.ends_with(".jsonl"))
                    })
                    .count(),
            );
            g.journal_result_count = Some(journal_results(session_path, &rid).len());
        }
        g.run_id = Some(rid);
    }
    ghosts
}

/// The pure matcher behind [`ghost_launches`]: each launch is accounted for by (a) having errored
/// at launch (no run to record), (b) a state file for its `scriptPath`-embedded run family, or
/// (c) consuming one same-named state file (multiset — N inline launches of one name need N
/// recorded runs). Unidentifiable launches (no name, no run id) are skipped rather than guessed at.
fn compute_ghosts(launches: Vec<WorkflowLaunch>, states: &[Workflow]) -> Vec<WorkflowLaunch> {
    use std::collections::HashMap;
    let ids: std::collections::HashSet<&str> = states.iter().map(|w| w.run_id.as_str()).collect();
    let mut name_budget: HashMap<&str, usize> = HashMap::new();
    for w in states {
        if let Some(n) = &w.name {
            *name_budget.entry(n.as_str()).or_default() += 1;
        }
    }
    launches
        .into_iter()
        .filter(|l| {
            if l.errored {
                return false;
            }
            if let Some(rid) = &l.script_run_id {
                if ids.contains(rid.as_str()) {
                    return false;
                }
            }
            if let Some(rf) = &l.resume_from {
                // A resume's new run may keep the resumed family's id in `resume_from`.
                if states.iter().any(|w| w.resume_from.as_deref() == Some(rf)) {
                    return false;
                }
            }
            match &l.name {
                Some(n) => match name_budget.get_mut(n.as_str()) {
                    Some(b) if *b > 0 => {
                        *b -= 1;
                        false
                    }
                    _ => true,
                },
                None => false, // unidentifiable — don't report a guess
            }
        })
        .collect()
}

/// `meta.name` from an inline workflow script: the first `name: '<…>'` / `name: "<…>"` after the
/// script head (the `meta` literal is required to open the script, so first occurrence is it).
fn meta_name(script: &str) -> Option<String> {
    let i = script.find("name")?;
    let rest = script[i + 4..].trim_start().strip_prefix(':')?.trim_start();
    let quote = rest.chars().next().filter(|q| *q == '\'' || *q == '"')?;
    let body = &rest[1..];
    Some(body[..body.find(quote)?].to_string())
}

/// Split a `scriptPath` basename `<name>-wf_<runId>.js` into `(name, wf_<runId>)`. Either half is
/// `None` when the basename doesn't carry it.
fn script_path_identity(p: &str) -> (Option<String>, Option<String>) {
    let stem = Path::new(p).file_stem().and_then(|s| s.to_str()).unwrap_or(p);
    match stem.rsplit_once("-wf_") {
        Some((name, rid)) => (Some(name.to_string()), Some(format!("wf_{rid}"))),
        None => (Some(stem.to_string()), None),
    }
}

/// Parse one `workflow_agent` progress entry into a [`WorkflowAgent`].
fn parse_agent(e: &Value) -> WorkflowAgent {
    let s = |k: &str| e.get(k).and_then(Value::as_str).map(str::to_string);
    let u = |k: &str| e.get(k).and_then(Value::as_u64);
    WorkflowAgent {
        index: u("index").unwrap_or(0),
        label: s("label"),
        agent_id: s("agentId"),
        phase_index: u("phaseIndex").unwrap_or(0),
        state: s("state"),
        model: s("model"),
        tokens: u("tokens"),
        tool_calls: u("toolCalls"),
        duration_ms: u("durationMs"),
        error: s("error"),
        cached: e.get("cached").and_then(Value::as_bool).unwrap_or(false),
        prompt_preview: s("promptPreview"),
        result_preview: s("resultPreview"),
        attempt: u("attempt"),
        started_at: u("startedAt").and_then(ms_to_utc),
        last_progress_at: u("lastProgressAt").and_then(ms_to_utc),
        last_tool_name: s("lastToolName"),
        last_tool_summary: s("lastToolSummary"),
        journal_result: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_state() -> Value {
        json!({
            "runId": "wf_abc-123",
            "workflowName": "titanium-critical-path",
            "status": "completed",
            "summary": "did the thing",
            "defaultModel": "claude-opus-4-8[1m]",
            "agentCount": 3,
            "totalTokens": 589698,
            "totalToolCalls": 198,
            "durationMs": 3144884,
            "scriptPath": "/x/workflows/scripts/titanium-wf_abc-123.js",
            "script": "export const meta = { name: 'titanium-critical-path' }",
            "phases": [
                {"title": "Coherence-0.1", "detail": "migrate off stark.rs"},
                {"title": "Verify-0.1", "detail": "adversarial check"}
            ],
            "workflowProgress": [
                {"type": "workflow_phase", "index": 1, "title": "Coherence-0.1"},
                {"type": "workflow_phase", "index": 2, "title": "Verify-0.1"},
                {"type": "workflow_agent", "index": 1, "label": "migrate", "phaseIndex": 1,
                 "agentId": "ac7eeb16dca41d194", "model": "claude-opus-4-8[1m]", "state": "done",
                 "tokens": 277133, "toolCalls": 95, "durationMs": 2142913,
                 "promptPreview": "You are doing the single most security-critical fix",
                 "resultPreview": "{\"done\":true,\"commit\":\"26bb171e\"}"},
                {"type": "workflow_agent", "index": 2, "label": "verify", "phaseIndex": 2,
                 "agentId": "a29d1de560fb4a1b7", "model": "claude-opus-4-8[1m]", "state": "error",
                 "tokens": 90642, "toolCalls": 39, "error": "broke anti-ghost",
                 "resultPreview": "{\"verdict\":\"PARTIAL\"}"}
            ]
        })
    }

    #[test]
    fn parses_phase_tree_with_agents_under_phases() {
        let w = parse_state(&sample_state());
        assert_eq!(w.run_id, "wf_abc-123");
        assert_eq!(w.name.as_deref(), Some("titanium-critical-path"));
        assert_eq!(w.status.as_deref(), Some("completed"));
        assert_eq!(w.agent_count, 3);
        assert_eq!(w.total_tokens, 589698);
        assert_eq!(w.total_tool_calls, 198);
        assert!(w.script.as_deref().unwrap().contains("titanium-critical-path"));
        assert!(w.script_path.is_some());

        // Two phases, each carrying its detail and its agent.
        assert_eq!(w.phases.len(), 2);
        assert_eq!(w.phases[0].index, 1);
        assert_eq!(w.phases[0].title.as_deref(), Some("Coherence-0.1"));
        assert_eq!(w.phases[0].detail.as_deref(), Some("migrate off stark.rs"));
        assert_eq!(w.phases[0].agents.len(), 1);
        let a = &w.phases[0].agents[0];
        assert_eq!(a.agent_id.as_deref(), Some("ac7eeb16dca41d194"));
        assert_eq!(a.state.as_deref(), Some("done"));
        assert_eq!(a.tokens, Some(277133));
        assert!(a.result_preview.as_deref().unwrap().contains("26bb171e"));

        // The error agent under phase 2.
        let e = &w.phases[1].agents[0];
        assert_eq!(e.state.as_deref(), Some("error"));
        assert_eq!(e.error.as_deref(), Some("broke anti-ghost"));

        // State histogram.
        let sc = w.state_counts();
        assert_eq!(sc.get("done"), Some(&1));
        assert_eq!(sc.get("error"), Some(&1));
        assert_eq!(w.agents_in_phases(), 2);
    }

    #[test]
    fn falls_back_to_phases_array_without_progress_headers() {
        // A state whose workflowProgress has agents but no workflow_phase headers: phase titles
        // come from `phases[]`, agents slot by phaseIndex.
        let v = json!({
            "runId": "wf_x",
            "phases": [{"title": "P1", "detail": "d1"}, {"title": "P2", "detail": "d2"}],
            "workflowProgress": [
                {"type": "workflow_agent", "index": 1, "phaseIndex": 1, "agentId": "a1", "state": "done"},
                {"type": "workflow_agent", "index": 2, "phaseIndex": 2, "agentId": "a2", "state": "done"}
            ]
        });
        let w = parse_state(&v);
        assert_eq!(w.phases.len(), 2);
        assert_eq!(w.phases[0].title.as_deref(), Some("P1"));
        assert_eq!(w.phases[0].agents[0].agent_id.as_deref(), Some("a1"));
        assert_eq!(w.phases[1].agents[0].agent_id.as_deref(), Some("a2"));
    }

    #[test]
    fn parses_result_logs_args_task_and_timing() {
        let v = json!({
            "runId": "wf_rich-1",
            "workflowName": "harvest-me",
            "status": "completed",
            "taskId": "w9mkonv48",
            "startTime": 1783423014394u64,
            "args": {"seeds_file": "/tmp/seeds.json"},
            "logs": ["20 families queued", "[refine:temporal] failed: session limit"],
            "result": {"confirmed": [1, 2], "notes": "ok"},
            "workflowProgress": [
                {"type": "workflow_phase", "index": 1, "title": "P"},
                {"type": "workflow_agent", "index": 1, "phaseIndex": 1, "agentId": "a1",
                 "state": "error", "attempt": 2, "startedAt": 1783389189922u64,
                 "lastProgressAt": 1783389200000u64, "lastToolName": "Bash",
                 "lastToolSummary": "cd /x && grep -n def"}
            ]
        });
        let w = parse_state(&v);
        assert_eq!(w.task_id.as_deref(), Some("w9mkonv48"));
        assert_eq!(w.started_at.unwrap().timestamp_millis(), 1_783_423_014_394);
        assert_eq!(w.args.as_ref().unwrap()["seeds_file"], "/tmp/seeds.json");
        assert_eq!(w.logs.len(), 2);
        assert!(w.logs[1].contains("session limit"));
        assert_eq!(w.result.as_ref().unwrap()["confirmed"][1], 2);
        let a = &w.phases[0].agents[0];
        assert_eq!(a.attempt, Some(2));
        assert_eq!(a.started_at.unwrap().timestamp_millis(), 1_783_389_189_922);
        assert_eq!(a.last_progress_at.unwrap().timestamp_millis(), 1_783_389_200_000);
        assert_eq!(a.last_tool_name.as_deref(), Some("Bash"));
        assert!(a.last_tool_summary.as_deref().unwrap().starts_with("cd /x"));
        // Null result stays absent, not Some(Null).
        let w2 = parse_state(&json!({"runId": "wf_x", "result": null}));
        assert!(w2.result.is_none());
    }

    #[test]
    fn run_names_orphans_and_ghost_debris() {
        let dir = std::env::temp_dir().join(format!("cv-wf-index-test-{}", std::process::id()));
        let wfdir = dir.join("sess").join("workflows");
        std::fs::create_dir_all(wfdir.join("scripts")).unwrap();
        let sess = dir.join("sess.jsonl");
        // A transcript with two successful launches: one recorded, one ghosted.
        let lines = [
            json!({"type":"assistant","timestamp":"2026-07-08T09:00:00Z","message":{"content":[
                {"type":"tool_use","id":"t1","name":"Workflow","input":{"script":"export const meta = { name: 'alive-swarm' }"}}]}})
            .to_string(),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"Workflow launched in background. Task ID: w1"}]}}).to_string(),
            json!({"type":"assistant","timestamp":"2026-07-08T09:02:00Z","message":{"content":[
                {"type":"tool_use","id":"t2","name":"Workflow","input":{"script":"export const meta = { name: 'doomed-swarm' }"}}]}})
            .to_string(),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t2","content":"Workflow launched in background. Task ID: w2"}]}}).to_string(),
        ];
        std::fs::write(&sess, lines.join("\n")).unwrap();
        // Recorded run: state file WITHOUT a script file (the byte-scan fallback path).
        std::fs::write(
            wfdir.join("wf_alive-1.json"),
            json!({"runId": "wf_alive-1", "workflowName": "alive-swarm"}).to_string(),
        )
        .unwrap();
        // Ghost: script file only (written at launch; the crash ate the state file) + debris.
        std::fs::write(
            wfdir.join("scripts").join("doomed-swarm-wf_dead-2.js"),
            "export const meta = {}",
        )
        .unwrap();
        let debris = dir.join("sess").join("subagents").join("workflows").join("wf_dead-2");
        std::fs::create_dir_all(&debris).unwrap();
        std::fs::write(debris.join("agent-a1.jsonl"), "").unwrap();
        std::fs::write(debris.join("agent-a2.jsonl"), "").unwrap();
        std::fs::write(
            debris.join("journal.jsonl"),
            json!({"type":"result","key":"v2:k","agentId":"a1","result":{"ok":true}}).to_string(),
        )
        .unwrap();

        // The cheap index sees both halves without parsing the recorded state fully.
        let mut names = run_names(&sess);
        names.sort();
        assert_eq!(names, vec![("alive-swarm".into(), "wf_alive-1".into())]);
        assert_eq!(orphan_scripts(&sess), vec![("doomed-swarm".into(), "wf_dead-2".into())]);
        assert!(has_workflow_records(&sess));
        assert_eq!(workflows_named(&sess, "alive").len(), 1);
        assert!(workflows_named(&sess, "nope").is_empty());

        // The ghost carries its recovered run id + debris counts.
        let ghosts = ghost_launches(&sess);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(ghosts.len(), 1);
        let g = &ghosts[0];
        assert_eq!(g.name.as_deref(), Some("doomed-swarm"));
        assert_eq!(g.run_id.as_deref(), Some("wf_dead-2"));
        assert_eq!(g.debris_agents, Some(2));
        assert_eq!(g.journal_result_count, Some(1));
    }

    #[test]
    fn scan_json_str_field_handles_escapes_and_misses() {
        assert_eq!(
            scan_json_str_field(
                r#"{"runId":"wf_x","workflowName":"my-swarm","status":"completed"}"#,
                "workflowName"
            ),
            Some("my-swarm".into())
        );
        assert_eq!(
            scan_json_str_field(r#"{"workflowName":"a\"b","x":1}"#, "workflowName"),
            Some(r#"a\"b"#.into())
        );
        assert_eq!(scan_json_str_field(r#"{"other":"x"}"#, "workflowName"), None);
    }

    #[test]
    fn attach_journal_fills_full_results_last_write_wins() {
        let dir = std::env::temp_dir().join(format!("cv-wf-journal-test-{}", std::process::id()));
        let run_dir = dir.join("sess").join("subagents").join("workflows").join("wf_j-1");
        std::fs::create_dir_all(&run_dir).unwrap();
        let sess = dir.join("sess.jsonl");
        std::fs::write(&sess, "").unwrap();
        let journal = [
            json!({"type":"started","key":"v2:k1","agentId":"a1"}).to_string(),
            json!({"type":"result","key":"v2:k1","agentId":"a1","result":{"status":"RED","try":1}}).to_string(),
            // A re-issued agent journals again — the LAST result must win.
            json!({"type":"result","key":"v2:k1","agentId":"a1","result":{"status":"GREEN","try":2,"detail":"x".repeat(600)}})
                .to_string(),
        ];
        std::fs::write(run_dir.join("journal.jsonl"), journal.join("\n")).unwrap();

        let mut w = parse_state(&json!({
            "runId": "wf_j-1",
            "phases": [{"title": "P"}],
            "workflowProgress": [
                {"type": "workflow_agent", "index": 1, "phaseIndex": 1, "agentId": "a1",
                 "state": "done", "resultPreview": "{\"status\":\"GREEN\"…"},
                {"type": "workflow_agent", "index": 2, "phaseIndex": 1, "agentId": "a2", "state": "done"}
            ]
        }));
        w.attach_journal(&sess);
        std::fs::remove_dir_all(&dir).ok();

        let a1 = &w.phases[0].agents[0];
        let jr = a1.journal_result.as_ref().unwrap();
        assert_eq!(jr["status"], "GREEN");
        assert_eq!(jr["try"], 2);
        assert_eq!(jr["detail"].as_str().unwrap().len(), 600); // FULL, beyond any preview cap
        assert!(w.phases[0].agents[1].journal_result.is_none()); // no journal entry → stays absent
    }

    #[test]
    fn workflow_resolves_by_run_id_and_by_name() {
        let dir = std::env::temp_dir().join(format!("cv-wf-byname-test-{}", std::process::id()));
        let wfdir = dir.join("sess").join("workflows");
        std::fs::create_dir_all(&wfdir).unwrap();
        let sess = dir.join("sess.jsonl");
        std::fs::write(&sess, "").unwrap();
        let mk = |rid: &str, name: &str| {
            std::fs::write(
                wfdir.join(format!("{rid}.json")),
                json!({"runId": rid, "workflowName": name}).to_string(),
            )
            .unwrap()
        };
        mk("wf_aaa-111", "stark-kill-emit-swarm");
        mk("wf_bbb-222", "stark-kill-assurance-audit");
        mk("wf_ccc-333", "stark-kill-assurance-audit"); // a re-run of the same name

        // By run id (prefix, with/without wf_).
        assert_eq!(workflow(&sess, "wf_aaa-111").unwrap().run_id, "wf_aaa-111");
        assert_eq!(workflow(&sess, "aaa").unwrap().run_id, "wf_aaa-111");
        // By exact name; a re-run name resolves (to one of its runs) instead of failing.
        assert_eq!(workflow(&sess, "stark-kill-emit-swarm").unwrap().run_id, "wf_aaa-111");
        let audit = workflow(&sess, "stark-kill-assurance-audit").unwrap();
        assert_eq!(audit.name.as_deref(), Some("stark-kill-assurance-audit"));
        // Unique name-prefix resolves; ambiguous prefix (two distinct names) does not.
        assert_eq!(workflow(&sess, "stark-kill-emit").unwrap().run_id, "wf_aaa-111");
        assert!(workflow(&sess, "stark-kill").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    fn launch(name: Option<&str>, script_run_id: Option<&str>, errored: bool) -> WorkflowLaunch {
        WorkflowLaunch {
            ts: None,
            name: name.map(str::to_string),
            script_run_id: script_run_id.map(str::to_string),
            resume_from: None,
            errored,
            run_id: None,
            debris_agents: None,
            journal_result_count: None,
        }
    }

    fn state(run_id: &str, name: &str) -> Workflow {
        parse_state(&json!({"runId": run_id, "workflowName": name}))
    }

    #[test]
    fn ghosts_are_launches_without_a_recorded_run() {
        // The real 2026-07-08 power-loss shape: census/audit runs recorded, the two final
        // launches' state files never persisted.
        let states = vec![state("wf_aaa-111", "census"), state("wf_bbb-222", "audit")];
        let launches = vec![
            launch(Some("census"), None, false),
            launch(Some("audit"), None, false),
            launch(Some("final-flips"), None, false),  // ghost
            launch(Some("bugfix-swarm"), None, false), // ghost
        ];
        let ghosts = compute_ghosts(launches, &states);
        assert_eq!(
            ghosts.iter().filter_map(|g| g.name.as_deref()).collect::<Vec<_>>(),
            vec!["final-flips", "bugfix-swarm"]
        );
    }

    #[test]
    fn errored_and_resume_launches_are_not_ghosts() {
        let states = vec![state("wf_aaa-111", "swarm")];
        let launches = vec![
            launch(Some("swarm"), None, false),               // consumes the state
            launch(Some("swarm"), Some("wf_aaa-111"), false), // scriptPath resume of a recorded family
            launch(Some("swarm"), Some("wf_aaa-111"), true),  // errored at launch — never ran
        ];
        assert!(compute_ghosts(launches, &states).is_empty());
        // But a second *inline* launch of the same name with only one recorded run IS a ghost.
        let launches = vec![launch(Some("swarm"), None, false), launch(Some("swarm"), None, false)];
        assert_eq!(compute_ghosts(launches, &states).len(), 1);
    }

    #[test]
    fn meta_name_and_script_path_identity() {
        assert_eq!(
            meta_name("export const meta = {\n  name: 'stark-kill-bugfix-swarm',\n  description: 'x'}"),
            Some("stark-kill-bugfix-swarm".into())
        );
        assert_eq!(
            meta_name(r#"export const meta = { name: "dq", phases: [] }"#),
            Some("dq".into())
        );
        assert_eq!(meta_name("no meta here"), None);
        assert_eq!(
            script_path_identity("/x/workflows/scripts/rung1-swarm-wf_1ee0382f-94a.js"),
            (Some("rung1-swarm".into()), Some("wf_1ee0382f-94a".into()))
        );
        assert_eq!(script_path_identity("/x/custom.js"), (Some("custom".into()), None));
    }

    #[test]
    fn launches_parses_a_transcript() {
        let dir = std::env::temp_dir().join(format!("cv-wf-launch-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("sess.jsonl");
        let lines = [
            json!({"type":"assistant","timestamp":"2026-07-08T09:02:20.652Z","message":{"content":[
                {"type":"tool_use","id":"tu_1","name":"Workflow","input":{"script":"export const meta = { name: 'doomed-swarm' }"}}]}})
            .to_string(),
            json!({"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"tu_1","content":"Workflow launched in background. Task ID: w123"}]}})
            .to_string(),
            json!({"type":"assistant","message":{"content":[
                {"type":"tool_use","id":"tu_2","name":"Workflow","input":{"scriptPath":"/x/scripts/doomed-swarm-wf_abc-123.js","resumeFromRunId":"wf_abc-123"}}]}})
            .to_string(),
            json!({"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"tu_2","is_error":true,"content":"<tool_use_error>still running</tool_use_error>"}]}})
            .to_string(),
        ];
        std::fs::write(&p, lines.join("\n")).unwrap();
        let ls = launches(&p);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(ls.len(), 2);
        assert_eq!(ls[0].name.as_deref(), Some("doomed-swarm"));
        assert!(!ls[0].errored);
        assert_eq!(ls[0].ts.unwrap().to_rfc3339(), "2026-07-08T09:02:20.652+00:00");
        assert_eq!(ls[1].script_run_id.as_deref(), Some("wf_abc-123"));
        assert_eq!(ls[1].resume_from.as_deref(), Some("wf_abc-123"));
        assert!(ls[1].errored);
    }

    #[test]
    fn killed_run_keeps_error_and_partial_agents() {
        let v = json!({
            "runId": "wf_k",
            "workflowName": "doomed",
            "status": "killed",
            "error": "session limit",
            "agentCount": 1,
            "phases": [{"title": "only"}],
            "workflowProgress": [
                {"type": "workflow_phase", "index": 1, "title": "only"},
                {"type": "workflow_agent", "index": 1, "phaseIndex": 1, "agentId": "a1",
                 "state": "progress", "tokens": 1000}
            ]
        });
        let w = parse_state(&v);
        assert_eq!(w.status.as_deref(), Some("killed"));
        assert_eq!(w.error.as_deref(), Some("session limit"));
        assert_eq!(w.phases[0].agents[0].state.as_deref(), Some("progress"));
    }
}
