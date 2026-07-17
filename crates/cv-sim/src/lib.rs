//! Synthetic fleet generator for the cv task substrate — real-shaped data for the day-old
//! sensors (`FleetStats`, the debt/inbox views) and the replay-cost bench.
//!
//! A [`FleetScenario`] deterministically expands (seed → same bytes, forever; no wall clock,
//! no ambient randomness) into a `Vec<TaskEvent>` that the **real** reducer accepts:
//! [`FleetScenario::generate`] validates by running [`TaskReducer::reduce`] over the emitted
//! events before returning them. The sequences exercised are the ones live fleets produce:
//!
//! ```text
//! open → claim → (note?) → propose → (reroute?) → pass | refute
//!                                       refute → re-propose → pass
//!                                       pass → [merged_local?] → landed → done
//!                                       abandon (stranding a live revision)
//! ```
//!
//! [`Pathology`] dials the unhealthy behaviors in: refutes, abandons, rubber-stamp passes
//! (receipts with `saw_change: Some(false)`, or no receipts at all), same-family review, and
//! the latency envelopes for review verdicts and lands. Timestamps are synthetic and
//! monotonically non-decreasing in log order — tasks run sequentially on a simulated clock.
//!
//! This crate is a dev tool: `publish = false`, and per the dependency fence it may depend on
//! cv-core while nothing depends on it.

use chrono::{DateTime, Duration, Utc};
use cv_core::task::{
    FleetStats, IndependenceCheck, ReduceError, Revision, ReviewReceipts, TaskEvent,
    TaskEventKind, TaskReadModel, TaskReducer, VERIFIER_BY,
};
use uuid::Uuid;

/// Simulated epoch every scenario starts at: 2026-01-01T00:00:00Z. A constant, not `Utc::now()`
/// — determinism from seed is the whole contract.
const SIM_EPOCH_SECS: i64 = 1_767_225_600;

/// Probability a revision's review gets rerouted once before the verdict (only when the
/// scenario has a second reviewer to reroute to). A documented constant rather than a
/// [`Pathology`] dial: reroutes are lifecycle traffic, not a pathology under study.
const REROUTE_RATE: f64 = 0.05;

/// Probability a task carries a progress note between claim and propose.
const NOTE_RATE: f64 = 0.3;

/// Probability a landed revision passed through the verifier's `MergedLocal` observation
/// before `Landed` (the local-integration path) instead of landing directly from `Ready`
/// (the forge-PR path). Both are legal; live fleets show both.
const MERGED_LOCAL_RATE: f64 = 0.5;

/// Model families stamped on synthetic independence observations.
const FAMILIES: &[&str] = &["anthropic", "openai", "google", "xai"];

/// The unhealthy-behavior dials of a scenario. Rates are probabilities in `[0, 1]`; latency
/// envelopes are inclusive `(min, max)` ranges in **seconds**.
#[derive(Clone, Debug, PartialEq)]
pub struct Pathology {
    /// Probability a task's first revision is refuted (it is then cured by a re-proposal
    /// which passes).
    pub refute_rate: f64,
    /// Probability a task is abandoned after proposing, stranding a live revision on a
    /// terminal task (`abandoned_live` in the stats).
    pub abandon_rate: f64,
    /// Probability a pass is a rubber stamp: its receipts show `saw_change: Some(false)`,
    /// or no receipts were recorded at all (split evenly between the two shapes).
    pub rubber_stamp_rate: f64,
    /// Probability a pass's independence observation records same-family review
    /// (`independent: Some(false)`).
    pub same_family_rate: f64,
    /// Seconds from propose to the review verdict, drawn uniformly per verdict.
    pub review_latency: (u64, u64),
    /// Seconds from the pass verdict to the verifier's `Landed` observation.
    pub land_delay: (u64, u64),
}

impl Default for Pathology {
    /// A mostly healthy fleet: occasional refutes and abandons, some rubber stamps, review
    /// inside 10 minutes to 2 hours, lands inside 5 minutes to an hour.
    fn default() -> Self {
        Pathology {
            refute_rate: 0.10,
            abandon_rate: 0.05,
            rubber_stamp_rate: 0.10,
            same_family_rate: 0.20,
            review_latency: (600, 7_200),
            land_delay: (300, 3_600),
        }
    }
}

/// A synthetic fleet: how many author endpoints, reviewer endpoints, and tasks to simulate,
/// under which [`Pathology`], from which seed.
#[derive(Clone, Debug, PartialEq)]
pub struct FleetScenario {
    /// Author/assignee endpoints, named `agent:sim-e00 …`.
    pub endpoints: usize,
    /// Reviewer endpoints, named `agent:sim-r00 …`.
    pub reviewers: usize,
    /// Tasks to generate; each expands to one full lifecycle (5–12 events).
    pub tasks: usize,
    /// PRNG seed. Same scenario + same seed = byte-identical events, on any machine.
    pub seed: u64,
    pub pathology: Pathology,
}

/// Scenario construction/validation errors.
#[derive(Debug, thiserror::Error)]
pub enum SimError {
    #[error("invalid scenario: {0}")]
    InvalidScenario(String),
    #[error("generated events failed the real reducer: {0}")]
    Reduce(#[from] ReduceError),
}

impl FleetScenario {
    /// Expand the scenario into task events, then prove the expansion is real-shaped by
    /// replaying it through cv-core's [`TaskReducer`]. The returned events are exactly what
    /// was validated — deterministic in the seed.
    pub fn generate(&self) -> Result<Vec<TaskEvent>, SimError> {
        self.check()?;
        let events = Sim::new(self).run();
        TaskReducer::reduce(&events)?;
        Ok(events)
    }

    /// The reduced read model of the generated events.
    pub fn model(&self) -> Result<TaskReadModel, SimError> {
        Ok(TaskReducer::reduce(&self.generate()?)?)
    }

    /// The fleet stats projection over the generated events (no heartbeat, no repo filter).
    pub fn stats(&self) -> Result<FleetStats, SimError> {
        Ok(FleetStats::compute(&self.model()?, None, None))
    }

    fn check(&self) -> Result<(), SimError> {
        if self.endpoints == 0 || self.reviewers == 0 {
            return Err(SimError::InvalidScenario(
                "need at least one endpoint and one reviewer".into(),
            ));
        }
        let p = &self.pathology;
        for (name, rate) in [
            ("refute_rate", p.refute_rate),
            ("abandon_rate", p.abandon_rate),
            ("rubber_stamp_rate", p.rubber_stamp_rate),
            ("same_family_rate", p.same_family_rate),
        ] {
            if !(0.0..=1.0).contains(&rate) {
                return Err(SimError::InvalidScenario(format!("{name} {rate} not in [0,1]")));
            }
        }
        if p.refute_rate + p.abandon_rate > 1.0 {
            return Err(SimError::InvalidScenario(
                "refute_rate + abandon_rate exceeds 1.0 (they partition first-revision fates)"
                    .into(),
            ));
        }
        for (name, (min, max)) in
            [("review_latency", p.review_latency), ("land_delay", p.land_delay)]
        {
            if min > max {
                return Err(SimError::InvalidScenario(format!("{name} min {min} > max {max}")));
            }
        }
        Ok(())
    }
}

/// Render a `FleetStats` as the fixed-width table humans eyeball (the snapshot artifact).
/// Small-n honesty as in the CLI: rates print as the counts they came from, medians print as
/// `-` when absent, never `0`.
pub fn render_stats_table(stats: &FleetStats) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if !stats.endpoints.is_empty() {
        out.push_str("endpoints (as author/assignee):\n");
        let _ = writeln!(
            out,
            "  {:16} {:>7} {:>8} {:>7} {:>7} {:>5} {:>6} {:>9} {:>9}",
            "endpoint", "claimed", "proposed", "landed", "refuted", "live", "aband", "land-rate",
            "med-land"
        );
        for e in &stats.endpoints {
            let terminal = e.landed + e.refuted + e.superseded;
            let rate =
                if terminal == 0 { "-".to_string() } else { format!("{}/{terminal}", e.landed) };
            let _ = writeln!(
                out,
                "  {:16} {:>7} {:>8} {:>7} {:>7} {:>5} {:>6} {:>9} {:>9}",
                e.endpoint,
                e.claimed,
                e.proposed,
                e.landed,
                e.refuted,
                e.unlanded,
                e.abandoned_live,
                rate,
                median_cell(e.median_secs_to_land),
            );
        }
    }
    if !stats.reviewers.is_empty() {
        out.push_str("reviewers:\n");
        let _ = writeln!(
            out,
            "  {:16} {:>8} {:>11} {:>9} {:>8} {:>10} {:>11}",
            "reviewer", "verdicts", "pass/refute", "same-fam", "no-rcpt", "no-contact",
            "med-latency"
        );
        for r in &stats.reviewers {
            let _ = writeln!(
                out,
                "  {:16} {:>8} {:>11} {:>9} {:>8} {:>10} {:>11}",
                r.reviewer,
                r.verdicts,
                format!("{}/{}", r.passes, r.refutes),
                r.same_family_passes,
                r.no_receipts_passes,
                r.no_contact_passes,
                median_cell(r.median_review_latency_secs),
            );
        }
    }
    for f in &stats.families {
        let _ = writeln!(
            out,
            "family {}: {} reviews ({} cross-family, {} same-family, {} undetermined)",
            f.family, f.reviews_given, f.cross_family, f.same_family, f.undetermined
        );
    }
    out
}

fn median_cell(secs: Option<i64>) -> String {
    match secs {
        None => "-".to_string(),
        Some(s) if s < 60 => format!("{s}s"),
        Some(s) if s < 3600 => format!("{}m", s / 60),
        Some(s) => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

// ── deterministic PRNG (splitmix64) ─────────────────────────────────────────

/// splitmix64: tiny, well-distributed, and dependency-free. Not cryptographic — this is a
/// simulator, and the point is byte-identical output from a seed.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn chance(&mut self, p: f64) -> bool {
        self.f64() < p
    }

    /// Uniform integer in the inclusive range `[min, max]`.
    fn range(&mut self, min: u64, max: u64) -> u64 {
        if max <= min {
            return min;
        }
        min + self.next_u64() % (max - min + 1)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// 40 lowercase hex chars — a synthetic git oid / patch-id.
    fn hex40(&mut self) -> String {
        format!("{:016x}{:016x}{:08x}", self.next_u64(), self.next_u64(), self.next_u64() as u32)
    }
}

// ── the simulator ───────────────────────────────────────────────────────────

struct Sim<'a> {
    sc: &'a FleetScenario,
    rng: Rng,
    clock: DateTime<Utc>,
    next_id: u64,
    events: Vec<TaskEvent>,
}

impl<'a> Sim<'a> {
    fn new(sc: &'a FleetScenario) -> Sim<'a> {
        Sim {
            sc,
            rng: Rng(sc.seed),
            clock: DateTime::from_timestamp(SIM_EPOCH_SECS, 0)
                .expect("the simulated epoch is a valid timestamp"),
            next_id: 0,
            events: Vec::new(),
        }
    }

    fn run(mut self) -> Vec<TaskEvent> {
        for i in 0..self.sc.tasks {
            self.task(i);
        }
        self.events
    }

    /// Fresh deterministic event id: a uuid whose 128 bits are `seed << 64 | counter`.
    /// Unique within a run, stable across runs. (Not a v7 — determinism beats time-sortable
    /// ids here; the log's order is the events vec itself.)
    fn fresh_id(&mut self) -> String {
        self.next_id += 1;
        Uuid::from_u128(((self.sc.seed as u128) << 64) | self.next_id as u128).to_string()
    }

    /// Advance the simulated clock by a uniform draw of seconds in `[min, max]`.
    fn advance(&mut self, min: u64, max: u64) {
        self.clock += Duration::seconds(self.rng.range(min, max) as i64);
    }

    fn push(&mut self, task_id: &str, by: &str, kind: TaskEventKind) {
        let id = self.fresh_id();
        self.events.push(TaskEvent {
            id,
            task_id: task_id.to_string(),
            ts: self.clock,
            by: by.to_string(),
            kind,
        });
    }

    fn open(&mut self, n: usize, assignee: &str) -> String {
        let id = self.fresh_id();
        self.events.push(TaskEvent {
            id: id.clone(),
            task_id: id.clone(),
            ts: self.clock,
            by: "human:sim".to_string(),
            kind: TaskEventKind::Opened {
                title: format!("sim task {n:04}"),
                body: String::new(),
                repo: Some("/sim/repo".into()),
                issue: None,
                channel: "tasks".to_string(),
                assignee: Some(assignee.to_string()),
            },
        });
        id
    }

    fn revision(&mut self, n: u32, task_no: usize, reviewer: &str) -> Revision {
        Revision {
            n,
            branch: format!("task/sim-{task_no:04}"),
            worktree: None,
            upstream: "origin/main".to_string(),
            base: self.rng.hex40(),
            review_sha: self.rng.hex40(),
            patch_id: self.rng.hex40(),
            reviewer: Some(reviewer.to_string()),
            session_ref: None,
        }
    }

    /// One full task lifecycle. Tasks run sequentially on the simulated clock, so log order
    /// is timestamp order (monotonic non-decreasing).
    fn task(&mut self, n: usize) {
        let p = self.sc.pathology.clone();
        let author = format!("agent:sim-e{:02}", self.rng.below(self.sc.endpoints));
        let reviewer_ix = self.rng.below(self.sc.reviewers);

        self.advance(60, 900); // task arrival spacing
        let task = self.open(n, &author);
        self.advance(5, 120);
        self.push(&task, &author, TaskEventKind::Claimed { assignee: author.clone() });

        if self.rng.chance(NOTE_RATE) {
            self.advance(60, 1_800);
            self.push(
                &task,
                &author,
                TaskEventKind::Noted {
                    text: format!("sim progress note on task {n:04}"),
                    session_ref: None,
                },
            );
        }

        self.advance(30, 900);
        let mut reviewer = format!("agent:sim-r{reviewer_ix:02}");
        let rev = self.revision(1, n, &reviewer);
        let mut patch_id = rev.patch_id.clone();
        let mut review_sha = rev.review_sha.clone();
        self.push(&task, &author, TaskEventKind::RevisionProposed { revision: rev });

        // Optional reroute while the first revision awaits review.
        if self.sc.reviewers > 1 && self.rng.chance(REROUTE_RATE) {
            let to_ix = (reviewer_ix + 1 + self.rng.below(self.sc.reviewers - 1)) % self.sc.reviewers;
            let to = format!("agent:sim-r{to_ix:02}");
            self.advance(60, 3_600);
            self.push(
                &task,
                &reviewer,
                TaskEventKind::ReviewRerouted { from: reviewer.clone(), to: to.clone() },
            );
            reviewer = to;
        }

        // First-revision fate: abandon | refute-then-cure | pass.
        let fate = self.rng.f64();
        if fate < p.abandon_rate {
            // Abandoned with the revision still live: `abandoned_live` in the stats.
            self.advance(600, 3_600);
            self.push(
                &task,
                &author,
                TaskEventKind::Abandoned { reason: "sim: path abandoned".to_string() },
            );
            return;
        }
        if fate < p.abandon_rate + p.refute_rate {
            // Refute (terminal for the revision), then the only cure: a re-proposal.
            self.advance(p.review_latency.0, p.review_latency.1);
            let refute_kind = TaskEventKind::ReviewRefuted {
                reviewer: reviewer.clone(),
                session_ref: None,
                receipts: Some(ReviewReceipts {
                    saw_change: Some(true),
                    ran_checks: Some(true),
                    turns: Some(self.rng.range(5, 40) as u32),
                }),
            };
            self.push(&task, &reviewer.clone(), refute_kind);
            self.advance(300, 3_600);
            let rev = self.revision(2, n, &reviewer);
            patch_id = rev.patch_id.clone();
            review_sha = rev.review_sha.clone();
            self.push(&task, &author, TaskEventKind::RevisionProposed { revision: rev });
        }

        // The (possibly re-proposed) revision passes review…
        self.advance(p.review_latency.0, p.review_latency.1);
        let rubber = self.rng.chance(p.rubber_stamp_rate);
        let receipts = if rubber {
            // Rubber stamps come in two shapes: contactless receipts, or none recorded at all.
            if self.rng.chance(0.5) {
                Some(ReviewReceipts {
                    saw_change: Some(false),
                    ran_checks: Some(false),
                    turns: Some(self.rng.range(1, 3) as u32),
                })
            } else {
                None
            }
        } else {
            Some(ReviewReceipts {
                saw_change: Some(true),
                ran_checks: Some(true),
                turns: Some(self.rng.range(5, 40) as u32),
            })
        };
        let same_family = self.rng.chance(p.same_family_rate);
        let author_family = FAMILIES[self.rng.below(FAMILIES.len())];
        let reviewer_family = if same_family {
            author_family
        } else {
            FAMILIES[(FAMILIES.iter().position(|f| *f == author_family).unwrap_or(0)
                + 1
                + self.rng.below(FAMILIES.len() - 1))
                % FAMILIES.len()]
        };
        let pass_kind = TaskEventKind::ReviewPassed {
            reviewer: reviewer.clone(),
            session_ref: None,
            independence: Some(IndependenceCheck {
                author_family: Some(author_family.to_string()),
                reviewer_family: Some(reviewer_family.to_string()),
                independent: Some(!same_family),
            }),
            receipts,
        };
        self.push(&task, &reviewer.clone(), pass_kind);

        // …and the verifier observes the land (sometimes via a local merge first).
        self.advance(p.land_delay.0, p.land_delay.1);
        if self.rng.chance(MERGED_LOCAL_RATE) {
            let from_sha = self.rng.hex40();
            self.push(
                &task,
                VERIFIER_BY,
                TaskEventKind::MergedLocal { from_sha, to_sha: review_sha },
            );
        }
        let upstream_head = self.rng.hex40();
        self.push(
            &task,
            VERIFIER_BY,
            TaskEventKind::Landed { upstream_head, observed_patch_id: patch_id },
        );

        self.advance(10, 300);
        self.push(
            &task,
            &author,
            TaskEventKind::Done { observed: Some("sim: revision landed".to_string()) },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny() -> FleetScenario {
        FleetScenario {
            endpoints: 3,
            reviewers: 2,
            tasks: 20,
            seed: 1,
            pathology: Pathology::default(),
        }
    }

    #[test]
    fn generate_validates_against_the_real_reducer() {
        let events = tiny().generate().unwrap();
        assert!(events.len() >= 20 * 4, "each task expands to several events");
        TaskReducer::reduce(&events).unwrap();
    }

    #[test]
    fn timestamps_are_monotonic_and_ids_unique() {
        let events = tiny().generate().unwrap();
        let mut seen = std::collections::HashSet::new();
        for pair in events.windows(2) {
            assert!(pair[0].ts <= pair[1].ts, "log order must be time order");
        }
        for ev in &events {
            assert!(seen.insert(ev.id.clone()), "duplicate event id {}", ev.id);
        }
    }

    #[test]
    fn verifier_events_carry_the_verifier_identity() {
        let events = tiny().generate().unwrap();
        for ev in &events {
            assert_eq!(
                ev.kind.is_verifier_only(),
                ev.by == VERIFIER_BY,
                "verifier-only kinds and the verifier identity must coincide: {} by {}",
                ev.kind.tag(),
                ev.by
            );
        }
    }

    #[test]
    fn invalid_scenarios_are_refused() {
        let mut sc = tiny();
        sc.reviewers = 0;
        assert!(matches!(sc.generate(), Err(SimError::InvalidScenario(_))));

        let mut sc = tiny();
        sc.pathology.refute_rate = 1.5;
        assert!(matches!(sc.generate(), Err(SimError::InvalidScenario(_))));

        let mut sc = tiny();
        sc.pathology.refute_rate = 0.6;
        sc.pathology.abandon_rate = 0.6;
        assert!(matches!(sc.generate(), Err(SimError::InvalidScenario(_))));

        let mut sc = tiny();
        sc.pathology.review_latency = (100, 10);
        assert!(matches!(sc.generate(), Err(SimError::InvalidScenario(_))));
    }
}
