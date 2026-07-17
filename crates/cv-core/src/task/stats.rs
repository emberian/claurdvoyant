//! Fleet batting averages: a **pure projection** over the replayed [`TaskReadModel`] (plus the
//! verify heartbeat, passed in by the caller, for a freshness annotation). Observation only —
//! this module computes and reports, it never gates, blocks, or actuates anything (law 2's
//! posture), and it does no I/O of its own (the purity fence pins that textually).
//!
//! Attribution is honest about what the model records:
//! - **author** of a revision is the `by` on its propose event ([`RevisionProjection::proposed_by`]);
//! - **claimed** counts tasks whose *current* assignee is the endpoint — a released claim leaves
//!   no per-endpoint residue in the model, so it isn't counted (the durable events know more, but
//!   this projection deliberately reads only the model);
//! - **landed** means the git verifier observed the reviewed patch on upstream — never an
//!   agent's claim;
//! - medians are `None`, not `0`, when there is nothing to take a median of.
//!
//! Small-n honesty is a rendering rule the row shapes are built for: every rate ships next to
//! the counts it came from, so a consumer can always print `2/3` instead of a bare `67%`.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::model::RevisionState;
use super::reduce::{RevisionProjection, TaskProjection, TaskReadModel};
use super::verify::VerifyHeartbeat;

/// One endpoint's row as author/assignee (`cv task stats`, MCP `task_stats`,
/// `GET /api/tasks/stats`). Field order is the emitted JSON key order (preserve_order).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EndpointRow {
    pub endpoint: String,
    /// Tasks currently assigned to this endpoint (see the module note on released claims).
    pub claimed: usize,
    /// Revisions this endpoint proposed (all revisions, all tasks — history counts).
    pub proposed: usize,
    /// Their revisions the verifier observed landed.
    pub landed: usize,
    /// Their revisions refuted by review (terminal).
    pub refuted: usize,
    /// Their revisions superseded by a re-proposal (terminal).
    pub superseded: usize,
    /// Their live revisions stranded on an abandoned task.
    pub abandoned_live: usize,
    /// Their currently-live revisions (awaiting review / ready / merged-local) on live tasks.
    pub unlanded: usize,
    /// `landed / (landed + refuted + superseded)` — landed share of terminal revision outcomes.
    /// `None` when no revision has reached a terminal state (never `0.0` for "no data").
    pub landed_rate: Option<f64>,
    /// Median seconds from propose to the verifier's Landed observation, over their landed
    /// revisions that carry both timestamps.
    pub median_secs_to_land: Option<i64>,
}

/// One reviewer's row: verdicts given and the rubber-stamp signals.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReviewerRow {
    pub reviewer: String,
    pub verdicts: usize,
    pub passes: usize,
    pub refutes: usize,
    /// Passes recorded with `independence.independent == false` (same-family review).
    pub same_family_passes: usize,
    /// Passes recorded with no receipts observation at all (no reviewer session given).
    pub no_receipts_passes: usize,
    /// Passes whose receipts show no observable contact with the change (`saw_change == false`).
    pub no_contact_passes: usize,
    /// Median seconds from propose to this reviewer's verdict (pass or refute).
    pub median_review_latency_secs: Option<i64>,
}

/// Per-model-family aggregate over pass verdicts **where independence data recorded a reviewer
/// family** — the cross-family review posture at a glance.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FamilyRow {
    pub family: String,
    pub reviews_given: usize,
    pub cross_family: usize,
    pub same_family: usize,
    /// Reviewer family known, author family not — independence undetermined.
    pub undetermined: usize,
}

/// The whole stats view, packaged once for all three surfaces (views.rs pattern).
#[derive(Clone, Debug, Serialize)]
pub struct FleetStats {
    pub endpoints: Vec<EndpointRow>,
    pub reviewers: Vec<ReviewerRow>,
    pub families: Vec<FamilyRow>,
    /// Freshness of the landed/unlanded distinction: the verifier's last heartbeat. Stats are
    /// only as current as the verifier is alive.
    pub verified_as_of: Option<DateTime<Utc>>,
    pub verify_warning: Option<String>,
}

#[derive(Default)]
struct EndpointAcc {
    claimed: usize,
    proposed: usize,
    landed: usize,
    refuted: usize,
    superseded: usize,
    abandoned_live: usize,
    unlanded: usize,
    secs_to_land: Vec<i64>,
}

#[derive(Default)]
struct ReviewerAcc {
    passes: usize,
    refutes: usize,
    same_family_passes: usize,
    no_receipts_passes: usize,
    no_contact_passes: usize,
    latency_secs: Vec<i64>,
}

#[derive(Default)]
struct FamilyAcc {
    reviews_given: usize,
    cross_family: usize,
    same_family: usize,
    undetermined: usize,
}

impl FleetStats {
    /// Compute the fleet stats from a replayed model; `repo` restricts to tasks recorded against
    /// that repo path. Pass the heartbeat read from the tasks dir (or `None`, which is itself
    /// loud: the warning names the never-verified state).
    pub fn compute(
        model: &TaskReadModel,
        heartbeat: Option<&VerifyHeartbeat>,
        repo: Option<&Path>,
    ) -> FleetStats {
        let mut endpoints: BTreeMap<String, EndpointAcc> = BTreeMap::new();
        let mut reviewers: BTreeMap<String, ReviewerAcc> = BTreeMap::new();
        let mut families: BTreeMap<String, FamilyAcc> = BTreeMap::new();

        for task in model.tasks.values() {
            if let Some(want) = repo {
                if task.repo.as_deref() != Some(want) {
                    continue;
                }
            }
            if let Some(assignee) = &task.assignee {
                endpoints.entry(assignee.clone()).or_default().claimed += 1;
            }
            for rev in &task.revisions {
                fold_revision(task, rev, &mut endpoints);
                fold_verdicts(rev, &mut reviewers, &mut families);
            }
        }

        let mut endpoints: Vec<EndpointRow> = endpoints
            .into_iter()
            .map(|(endpoint, acc)| {
                let terminal = acc.landed + acc.refuted + acc.superseded;
                EndpointRow {
                    endpoint,
                    claimed: acc.claimed,
                    proposed: acc.proposed,
                    landed: acc.landed,
                    refuted: acc.refuted,
                    superseded: acc.superseded,
                    abandoned_live: acc.abandoned_live,
                    unlanded: acc.unlanded,
                    landed_rate: (terminal > 0).then(|| acc.landed as f64 / terminal as f64),
                    median_secs_to_land: median(acc.secs_to_land),
                }
            })
            .collect();
        // Most landed first; BTreeMap order breaks ties by endpoint name.
        endpoints.sort_by(|a, b| b.landed.cmp(&a.landed).then(b.proposed.cmp(&a.proposed)));

        let mut reviewers: Vec<ReviewerRow> = reviewers
            .into_iter()
            .map(|(reviewer, acc)| ReviewerRow {
                reviewer,
                verdicts: acc.passes + acc.refutes,
                passes: acc.passes,
                refutes: acc.refutes,
                same_family_passes: acc.same_family_passes,
                no_receipts_passes: acc.no_receipts_passes,
                no_contact_passes: acc.no_contact_passes,
                median_review_latency_secs: median(acc.latency_secs),
            })
            .collect();
        reviewers.sort_by_key(|r| std::cmp::Reverse(r.verdicts));

        let mut families: Vec<FamilyRow> = families
            .into_iter()
            .map(|(family, acc)| FamilyRow {
                family,
                reviews_given: acc.reviews_given,
                cross_family: acc.cross_family,
                same_family: acc.same_family,
                undetermined: acc.undetermined,
            })
            .collect();
        families.sort_by_key(|f| std::cmp::Reverse(f.reviews_given));

        FleetStats {
            endpoints,
            reviewers,
            families,
            verified_as_of: heartbeat.map(|h| h.ts),
            verify_warning: super::verify::heartbeat_warning(heartbeat),
        }
    }
}

/// Fold one revision into its author's endpoint accumulator.
fn fold_revision(
    task: &TaskProjection,
    rev: &RevisionProjection,
    endpoints: &mut BTreeMap<String, EndpointAcc>,
) {
    // `proposed_by` is always stamped by the reducer; "(unknown)" only appears for models
    // deserialized from a pre-field snapshot, never for a fresh replay.
    let author = if rev.proposed_by.is_empty() { "(unknown)" } else { &rev.proposed_by };
    let acc = endpoints.entry(author.to_string()).or_default();
    acc.proposed += 1;
    match rev.state {
        RevisionState::Landed => {
            acc.landed += 1;
            if let Some(landed_at) = rev.landed_at {
                acc.secs_to_land
                    .push(landed_at.signed_duration_since(rev.proposed_at).num_seconds());
            }
        }
        RevisionState::Refuted => acc.refuted += 1,
        RevisionState::Superseded => acc.superseded += 1,
        RevisionState::AwaitingReview | RevisionState::Ready | RevisionState::MergedLocal => {
            if task.state.is_terminal() {
                // The only way a live revision rides a terminal task is an abandon (Done is
                // refused while a revision is live; supersede goes through Superseded state).
                acc.abandoned_live += 1;
            } else {
                acc.unlanded += 1;
            }
        }
    }
}

/// Fold one revision's verdicts into the reviewer and family accumulators.
fn fold_verdicts(
    rev: &RevisionProjection,
    reviewers: &mut BTreeMap<String, ReviewerAcc>,
    families: &mut BTreeMap<String, FamilyAcc>,
) {
    if let Some(pass) = &rev.pass {
        let acc = reviewers.entry(pass.reviewer.clone()).or_default();
        acc.passes += 1;
        acc.latency_secs
            .push(pass.ts.signed_duration_since(rev.proposed_at).num_seconds());
        match &pass.receipts {
            None => acc.no_receipts_passes += 1,
            Some(r) if r.saw_change == Some(false) => acc.no_contact_passes += 1,
            Some(_) => {}
        }
        if let Some(check) = &pass.independence {
            if check.independent == Some(false) {
                acc.same_family_passes += 1;
            }
            if let Some(family) = &check.reviewer_family {
                let fam = families.entry(family.clone()).or_default();
                fam.reviews_given += 1;
                match check.independent {
                    Some(true) => fam.cross_family += 1,
                    Some(false) => fam.same_family += 1,
                    None => fam.undetermined += 1,
                }
            }
        }
    }
    if let Some(refute) = &rev.refute {
        let acc = reviewers.entry(refute.reviewer.clone()).or_default();
        acc.refutes += 1;
        acc.latency_secs
            .push(refute.ts.signed_duration_since(rev.proposed_at).num_seconds());
    }
}

/// Median of a sample; the mean of the middle two for even counts, `None` for an empty sample
/// (a median of nothing is not `0`).
fn median(mut sample: Vec<i64>) -> Option<i64> {
    if sample.is_empty() {
        return None;
    }
    sample.sort_unstable();
    let mid = sample.len() / 2;
    Some(if sample.len() % 2 == 1 {
        sample[mid]
    } else {
        (sample[mid - 1] + sample[mid]) / 2
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::model::{
        IndependenceCheck, ReviewReceipts, Revision, TaskEvent, TaskEventKind,
    };
    use crate::task::reduce::TaskReducer;
    use std::path::PathBuf;

    fn sha(c: char) -> String {
        std::iter::repeat_n(c, 40).collect()
    }

    struct Log {
        events: Vec<TaskEvent>,
        n: u64,
    }

    impl Log {
        fn new() -> Self {
            Log { events: Vec::new(), n: 0 }
        }
        fn next_id(&mut self) -> String {
            self.n += 1;
            format!("00000000-0000-7000-8000-{:012}", self.n)
        }
        fn push_at(&mut self, task_id: &str, by: &str, ts: &str, kind: TaskEventKind) {
            let id = self.next_id();
            self.events.push(TaskEvent {
                id,
                task_id: task_id.to_string(),
                ts: ts.parse().unwrap(),
                by: by.to_string(),
                kind,
            });
        }
        fn open(&mut self, by: &str, repo: &str) -> String {
            let id = self.next_id();
            self.events.push(TaskEvent {
                id: id.clone(),
                task_id: id.clone(),
                ts: "2026-07-01T00:00:00Z".parse().unwrap(),
                by: by.to_string(),
                kind: TaskEventKind::Opened {
                    title: "t".into(),
                    body: String::new(),
                    repo: Some(PathBuf::from(repo)),
                    issue: None,
                    channel: "tasks".into(),
                    assignee: None,
                },
            });
            id
        }
        fn model(&self) -> TaskReadModel {
            TaskReducer::reduce(&self.events).unwrap()
        }
    }

    fn revision(n: u32, reviewer: &str) -> Revision {
        Revision {
            n,
            branch: format!("task/x{n}"),
            worktree: None,
            upstream: "origin/main".into(),
            base: sha('0'),
            review_sha: sha(char::from_digit(n % 10, 10).unwrap()),
            patch_id: sha('b'),
            reviewer: Some(reviewer.into()),
            session_ref: None,
        }
    }

    fn pass_kind(
        reviewer: &str,
        independent: Option<bool>,
        receipts: Option<ReviewReceipts>,
    ) -> TaskEventKind {
        TaskEventKind::ReviewPassed {
            reviewer: reviewer.into(),
            session_ref: None,
            independence: independent.map(|i| IndependenceCheck {
                author_family: Some(if i { "anthropic" } else { "openai" }.into()),
                reviewer_family: Some("openai".into()),
                independent: Some(i),
            }),
            receipts,
        }
    }

    /// Three endpoints, mixed outcomes:
    /// - agent:a — two landed revisions (2h and 6h to land → median 4h), one live (unlanded),
    ///   one claimed task, reviewed by agent:r (one no-receipts pass, one same-family
    ///   with-contact pass, one no-contact pass on b's task).
    /// - agent:b — one refuted then superseding re-proposal refuted again? No: one revision
    ///   passed-no-contact then abandoned live? Keep it simple: one refuted revision and one
    ///   passed-then-abandoned (abandoned_live).
    /// - agent:c — claimed a task, proposed nothing.
    fn fixture() -> TaskReadModel {
        let mut log = Log::new();

        // Task 1 (agent:a): propose 00:00 → pass (no receipts) 01:00 → landed 02:00.
        let t1 = log.open("h", "/tmp/r1");
        log.push_at(&t1, "agent:a", "2026-07-01T00:00:00Z", TaskEventKind::Claimed { assignee: "agent:a".into() });
        log.push_at(&t1, "agent:a", "2026-07-02T00:00:00Z", TaskEventKind::RevisionProposed { revision: revision(1, "agent:r") });
        log.push_at(&t1, "agent:r", "2026-07-02T01:00:00Z", pass_kind("agent:r", None, None));
        log.push_at(&t1, "cv-verify", "2026-07-02T02:00:00Z", TaskEventKind::Landed {
            upstream_head: sha('f'),
            observed_patch_id: sha('b'),
        });

        // Task 2 (agent:a): propose 00:00 → same-family pass with contact 03:00 → landed 06:00.
        let t2 = log.open("h", "/tmp/r1");
        log.push_at(&t2, "agent:a", "2026-07-03T00:00:00Z", TaskEventKind::RevisionProposed { revision: revision(1, "agent:r") });
        log.push_at(
            &t2,
            "agent:r",
            "2026-07-03T03:00:00Z",
            pass_kind(
                "agent:r",
                Some(false),
                Some(ReviewReceipts { saw_change: Some(true), ran_checks: Some(true), turns: Some(9) }),
            ),
        );
        log.push_at(&t2, "cv-verify", "2026-07-03T06:00:00Z", TaskEventKind::Landed {
            upstream_head: sha('f'),
            observed_patch_id: sha('b'),
        });

        // Task 3 (agent:a): live revision, still awaiting review — unlanded.
        let t3 = log.open("h", "/tmp/r2");
        log.push_at(&t3, "agent:a", "2026-07-04T00:00:00Z", TaskEventKind::RevisionProposed { revision: revision(1, "agent:r") });

        // Task 4 (agent:b): refuted 30m after propose.
        let t4 = log.open("h", "/tmp/r1");
        log.push_at(&t4, "agent:b", "2026-07-05T00:00:00Z", TaskEventKind::RevisionProposed { revision: revision(1, "agent:r") });
        log.push_at(&t4, "agent:r", "2026-07-05T00:30:00Z", TaskEventKind::ReviewRefuted {
            reviewer: "agent:r".into(),
            session_ref: None,
            receipts: None,
        });

        // Task 5 (agent:b): no-contact pass, then the task is abandoned with the revision live.
        let t5 = log.open("h", "/tmp/r1");
        log.push_at(&t5, "agent:b", "2026-07-06T00:00:00Z", TaskEventKind::RevisionProposed { revision: revision(1, "agent:r") });
        log.push_at(
            &t5,
            "agent:r",
            "2026-07-06T02:00:00Z",
            pass_kind(
                "agent:r",
                Some(true),
                Some(ReviewReceipts { saw_change: Some(false), ran_checks: Some(false), turns: Some(1) }),
            ),
        );
        log.push_at(&t5, "h", "2026-07-07T00:00:00Z", TaskEventKind::Abandoned { reason: "path abandoned".into() });

        // Task 6 (agent:c): claimed, nothing proposed.
        let t6 = log.open("h", "/tmp/r2");
        log.push_at(&t6, "agent:c", "2026-07-08T00:00:00Z", TaskEventKind::Claimed { assignee: "agent:c".into() });

        log.model()
    }

    #[test]
    fn endpoint_rows_count_outcomes_and_median_time_to_land() {
        let stats = FleetStats::compute(&fixture(), None, None);

        assert_eq!(stats.endpoints.len(), 3, "{:?}", stats.endpoints);
        // Sorted by landed count: agent:a first.
        let a = &stats.endpoints[0];
        assert_eq!(a.endpoint, "agent:a");
        assert_eq!((a.claimed, a.proposed, a.landed, a.refuted), (1, 3, 2, 0));
        assert_eq!(a.unlanded, 1, "task 3 is live and unlanded");
        assert_eq!(a.abandoned_live, 0);
        assert_eq!(a.landed_rate, Some(1.0), "2 landed / 2 terminal outcomes");
        assert_eq!(
            a.median_secs_to_land,
            Some(4 * 3600),
            "median of 2h and 6h propose→landed is 4h"
        );

        let b = stats.endpoints.iter().find(|r| r.endpoint == "agent:b").unwrap();
        assert_eq!((b.proposed, b.landed, b.refuted), (2, 0, 1));
        assert_eq!(b.abandoned_live, 1, "task 5's live revision rides an abandoned task");
        assert_eq!(b.unlanded, 0);
        assert_eq!(b.landed_rate, Some(0.0), "0 landed / 1 terminal outcome — printed as counts");
        assert_eq!(b.median_secs_to_land, None, "no landed revision → no median, never 0");

        // Small-n honesty: agent:c has no revision history at all — no rate, no medians.
        let c = stats.endpoints.iter().find(|r| r.endpoint == "agent:c").unwrap();
        assert_eq!((c.claimed, c.proposed), (1, 0));
        assert_eq!(c.landed_rate, None);
        assert_eq!(c.median_secs_to_land, None);
    }

    #[test]
    fn reviewer_rows_carry_rubber_stamp_signals_and_latency() {
        let stats = FleetStats::compute(&fixture(), None, None);
        assert_eq!(stats.reviewers.len(), 1);
        let r = &stats.reviewers[0];
        assert_eq!(r.reviewer, "agent:r");
        assert_eq!((r.verdicts, r.passes, r.refutes), (4, 3, 1));
        assert_eq!(r.same_family_passes, 1, "task 2's pass recorded independent=false");
        assert_eq!(r.no_receipts_passes, 1, "task 1's pass had no reviewer session");
        assert_eq!(r.no_contact_passes, 1, "task 5's receipts show saw_change=false");
        // Latencies: 1h, 3h, 30m, 2h → sorted 30m,1h,2h,3h → median (1h+2h)/2 = 90m.
        assert_eq!(r.median_review_latency_secs, Some(90 * 60));
    }

    #[test]
    fn family_rows_aggregate_recorded_independence_only() {
        let stats = FleetStats::compute(&fixture(), None, None);
        assert_eq!(stats.families.len(), 1, "only passes with a recorded reviewer family");
        let f = &stats.families[0];
        assert_eq!(f.family, "openai");
        assert_eq!((f.reviews_given, f.cross_family, f.same_family, f.undetermined), (2, 1, 1, 0));
    }

    #[test]
    fn repo_filter_and_heartbeat_freshness() {
        let model = fixture();
        let stats = FleetStats::compute(&model, None, Some(Path::new("/tmp/r2")));
        // Only tasks 3 and 6 live in /tmp/r2: one unlanded revision by a, one claim by c.
        let a = stats.endpoints.iter().find(|r| r.endpoint == "agent:a").unwrap();
        assert_eq!((a.proposed, a.landed, a.unlanded, a.claimed), (1, 0, 1, 0));
        assert!(stats.reviewers.is_empty(), "no verdicts in /tmp/r2 yet");

        // No heartbeat: freshness is loudly absent.
        assert_eq!(stats.verified_as_of, None);
        assert!(stats.verify_warning.as_deref().unwrap_or("").contains("NEVER"));

        // A fresh one-shot heartbeat annotates and stops warning.
        let hb = VerifyHeartbeat {
            ts: Utc::now(),
            tasks_checked: 1,
            events_appended: 0,
            interval_secs: None,
            suspect_landed: Vec::new(),
        };
        let stats = FleetStats::compute(&model, Some(&hb), None);
        assert_eq!(stats.verified_as_of, Some(hb.ts));
        assert_eq!(stats.verify_warning, None);
    }

    #[test]
    fn median_edges() {
        assert_eq!(median(vec![]), None);
        assert_eq!(median(vec![7]), Some(7));
        assert_eq!(median(vec![3, 1]), Some(2));
        assert_eq!(median(vec![5, 1, 9]), Some(5));
        assert_eq!(median(vec![4, 1, 9, 5]), Some(4), "(4+5)/2 truncating");
    }

    /// The stats wire shape: key names and order, pinned per row kind (this is a NEW surface —
    /// the shape below is what ships).
    #[test]
    fn row_serialization_shape() {
        let stats = FleetStats::compute(&fixture(), None, None);
        let v = serde_json::to_value(&stats).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["endpoints", "reviewers", "families", "verified_as_of", "verify_warning"]
        );
        let keys: Vec<&str> = v["endpoints"][0].as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "endpoint",
                "claimed",
                "proposed",
                "landed",
                "refuted",
                "superseded",
                "abandoned_live",
                "unlanded",
                "landed_rate",
                "median_secs_to_land",
            ]
        );
        let keys: Vec<&str> = v["reviewers"][0].as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "reviewer",
                "verdicts",
                "passes",
                "refutes",
                "same_family_passes",
                "no_receipts_passes",
                "no_contact_passes",
                "median_review_latency_secs",
            ]
        );
        let keys: Vec<&str> = v["families"][0].as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["family", "reviews_given", "cross_family", "same_family", "undetermined"]);
    }
}
