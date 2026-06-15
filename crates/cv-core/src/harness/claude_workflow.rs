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

/// One workflow run by its `runId`, or `None` if no `workflows/wf_<runId>.json` exists. Accepts the
/// id with or without the `wf_` prefix and as a unique prefix of a full run id.
pub fn workflow(parent_path: &Path, run_id: &str) -> Option<Workflow> {
    let want = run_id.strip_prefix("wf_").unwrap_or(run_id);
    let mut hits: Vec<Workflow> = workflows(parent_path)
        .into_iter()
        .filter(|w| {
            let id = w.run_id.strip_prefix("wf_").unwrap_or(&w.run_id);
            id == want || w.run_id == run_id || id.starts_with(want)
        })
        .collect();
    // Exact match wins over a prefix collision.
    if let Some(exact) = hits.iter().position(|w| {
        let id = w.run_id.strip_prefix("wf_").unwrap_or(&w.run_id);
        id == want || w.run_id == run_id
    }) {
        return Some(hits.swap_remove(exact));
    }
    match hits.len() {
        1 => Some(hits.pop().unwrap()),
        _ => None,
    }
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
        phases,
        orphan_agents,
    }
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
