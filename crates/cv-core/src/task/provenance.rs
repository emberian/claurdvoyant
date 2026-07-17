//! Provenance & freshness: every observed fact knows how it was learned and when.
//!
//! The task substrate's first law is *observed, not attested* — landing is read from git, never
//! from an agent's say-so. But an observation has a shelf life: "landed" really means "observed
//! landed by a verify pass at some time", and if the verifier has gone quiet that truth is only as
//! fresh as its last pass. This module names that idea as a small, reusable shape so every fact —
//! starting with landing and self-reported completion — can carry its own certainty instead of
//! reading as an eternal boolean.
//!
//! **Pure and wasm-safe by construction.** [`freshness_from_heartbeat`] takes the heartbeat's
//! primitive fields (its timestamp and cadence) as inputs — it never reads the heartbeat file or
//! the [`super::verify::VerifyHeartbeat`] type, so this module has no I/O and no dependency on the
//! non-wasm verifier. The projection/view layer extracts those fields and passes them in. This is
//! enforced by the purity fence (`tests/dependency_fence.rs`).
//!
//! Extension path (named, not built): the same shape generalizes to the other observed facts —
//! independence/receipts provenance (who checked, in which session), a git-fetch-freshness stamp
//! on the verifier itself (was the last pass working from a fetched remote or a stale local ref),
//! and per-task-type completion predicates that would let a `Done` graduate from self-report to a
//! real observation with its own `source`.

use chrono::{DateTime, Utc};
use serde::Serialize;

/// A fact the git verifier observed on upstream (landing, suspect re-observation).
pub const SOURCE_GIT_VERIFY: &str = "git-verify";
/// A fact taken from an agent's self-report, labeled as such and never git-verified (the base
/// lifecycle's `Done` carve-out, made structural: a self-reported completion is *marked* distinct
/// from a verified land in the projection, not silently equal to one).
pub const SOURCE_SELF_REPORT: &str = "self-report";
/// A completion cv itself observed by RUNNING an attached check at `done` time (a command exiting
/// 0, a file existing and non-empty, an HTTP 2xx). Extends law 1 to revision-less tasks: distinct
/// from self-report because a machine executed the completion predicate, not an agent's say-so.
pub const SOURCE_CHECKED: &str = "checked";

/// How fresh an observed fact is, relative to the verifier's last pass. `Unknown` is first-class:
/// a fact the verifier has never checked since it entered a verifiable state is *not* implicitly
/// fine — it says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Freshness {
    /// The verifier checked this fact's state recently (within its own cadence, or it is a
    /// one-shot verifier with no cadence to miss).
    Fresh,
    /// The verifier last checked `age_secs` ago — older than twice its declared interval, so the
    /// periodic verifier may be dead and this fact may be out of date.
    Stale { age_secs: i64 },
    /// The verifier has never checked this fact since it became verifiable — no heartbeat at all,
    /// or the last pass predates the fact. Certainty is genuinely unknown, and that is said out
    /// loud rather than defaulted to healthy.
    Unknown,
}

impl Freshness {
    pub fn is_unknown(self) -> bool {
        matches!(self, Freshness::Unknown)
    }
}

/// Derive the freshness of a fact that became verifiable at `verifiable_since` from the verifier
/// heartbeat's primitive fields, evaluated at `now`. **Pure**: the heartbeat is passed in by
/// value, never read here.
///
/// - `Unknown` when there is no heartbeat (`heartbeat_ts` is `None`), or the last pass ran *before*
///   the fact became verifiable (the verifier has never actually looked at this revision in its
///   current state — a fresh land is not retroactively confirmed by an older pass).
/// - `Stale { age_secs }` when the verifier declares a cadence (`heartbeat_interval_secs`) and its
///   last pass is older than twice it — the same threshold [`super::verify::heartbeat_warning`]
///   uses to call the periodic verifier dead.
/// - `Fresh` otherwise — a recent pass, or a one-shot verifier (no interval) whose age alone can't
///   condemn it.
pub fn freshness_from_heartbeat(
    verifiable_since: DateTime<Utc>,
    heartbeat_ts: Option<DateTime<Utc>>,
    heartbeat_interval_secs: Option<u64>,
    now: DateTime<Utc>,
) -> Freshness {
    let Some(ts) = heartbeat_ts else {
        return Freshness::Unknown;
    };
    if ts < verifiable_since {
        return Freshness::Unknown;
    }
    let age = now.signed_duration_since(ts).num_seconds().max(0);
    match heartbeat_interval_secs {
        Some(interval) if age as u64 > interval.saturating_mul(2) => Freshness::Stale { age_secs: age },
        _ => Freshness::Fresh,
    }
}

/// Where a fact came from, when it was observed, and how fresh that observation is. The seed of
/// "every fact knows its own certainty" — attached to the highest-value facts first (landing,
/// completion) and meant to extend to the rest.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Provenance {
    /// When the fact was observed (the observing event's timestamp), if it was observed at all.
    /// `None` for an unlanded revision the verifier hasn't confirmed and for a self-report with no
    /// recorded moment.
    pub observed_at: Option<DateTime<Utc>>,
    /// How the fact was learned: [`SOURCE_GIT_VERIFY`] or [`SOURCE_SELF_REPORT`].
    pub source: &'static str,
    pub freshness: Freshness,
}

impl Provenance {
    /// A fact the git verifier observed positively on upstream (a landing). `observed_at` is the
    /// `Landed` event's timestamp; freshness measures how recently a pass has re-confirmed it.
    pub fn git_verified(
        observed_at: DateTime<Utc>,
        heartbeat_ts: Option<DateTime<Utc>>,
        heartbeat_interval_secs: Option<u64>,
        now: DateTime<Utc>,
    ) -> Provenance {
        Provenance {
            observed_at: Some(observed_at),
            source: SOURCE_GIT_VERIFY,
            freshness: freshness_from_heartbeat(observed_at, heartbeat_ts, heartbeat_interval_secs, now),
        }
    }

    /// A reviewed revision awaiting the verifier's landing observation (`Ready`/`MergedLocal`).
    /// `since` is when it became landable (its pass time). `observed_at` is the last time the
    /// verifier looked (the heartbeat), but only once a pass has actually covered it — an `Unknown`
    /// row has no observation to date.
    pub fn awaiting_land(
        since: DateTime<Utc>,
        heartbeat_ts: Option<DateTime<Utc>>,
        heartbeat_interval_secs: Option<u64>,
        now: DateTime<Utc>,
    ) -> Provenance {
        let freshness = freshness_from_heartbeat(since, heartbeat_ts, heartbeat_interval_secs, now);
        Provenance {
            observed_at: if freshness.is_unknown() { None } else { heartbeat_ts },
            source: SOURCE_GIT_VERIFY,
            freshness,
        }
    }

    /// A self-reported completion (the base lifecycle's `Done`): labeled as such and, by
    /// definition, never git-verified — its freshness is `Unknown` because no verifier can confirm
    /// or contradict a revision-less completion. This is the Done carve-out made structural.
    pub fn self_reported(observed_at: Option<DateTime<Utc>>) -> Provenance {
        Provenance {
            observed_at,
            source: SOURCE_SELF_REPORT,
            freshness: Freshness::Unknown,
        }
    }

    /// A completion cv observed by running an attached check that PASSED. `observed_at` is when the
    /// check ran (the `Done` event's timestamp). Freshness is `Fresh`: the check is a one-shot
    /// observation with no re-check cadence to go stale against (same rule as a one-shot verifier
    /// pass) — cv looked, at that moment, and the predicate held.
    pub fn checked(observed_at: Option<DateTime<Utc>>) -> Provenance {
        Provenance {
            observed_at,
            source: SOURCE_CHECKED,
            freshness: Freshness::Fresh,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn no_heartbeat_is_unknown() {
        let since = ts("2026-07-16T12:00:00Z");
        let now = ts("2026-07-16T12:05:00Z");
        assert_eq!(
            freshness_from_heartbeat(since, None, Some(300), now),
            Freshness::Unknown
        );
    }

    #[test]
    fn heartbeat_before_the_fact_is_unknown() {
        // The last pass ran at 11:59, but the fact became verifiable at 12:00: never actually
        // checked in this state.
        let since = ts("2026-07-16T12:00:00Z");
        let hb = ts("2026-07-16T11:59:00Z");
        let now = ts("2026-07-16T12:05:00Z");
        assert_eq!(
            freshness_from_heartbeat(since, Some(hb), Some(300), now),
            Freshness::Unknown
        );
    }

    #[test]
    fn recent_pass_after_the_fact_is_fresh() {
        let since = ts("2026-07-16T12:00:00Z");
        let hb = ts("2026-07-16T12:04:00Z");
        let now = ts("2026-07-16T12:05:00Z"); // 60s after the pass, interval 300s → fresh
        assert_eq!(
            freshness_from_heartbeat(since, Some(hb), Some(300), now),
            Freshness::Fresh
        );
    }

    #[test]
    fn pass_older_than_twice_the_interval_is_stale() {
        let since = ts("2026-07-16T12:00:00Z");
        let hb = ts("2026-07-16T12:04:00Z");
        // 700s after a 300s-cadence pass: 700 > 600 → stale, carrying the age.
        let now = ts("2026-07-16T12:15:40Z");
        assert_eq!(
            freshness_from_heartbeat(since, Some(hb), Some(300), now),
            Freshness::Stale { age_secs: 700 }
        );
    }

    #[test]
    fn exactly_twice_the_interval_is_still_fresh() {
        // Boundary: age == 2*interval is NOT yet stale (strict `>`), matching heartbeat_warning.
        let since = ts("2026-07-16T12:00:00Z");
        let hb = ts("2026-07-16T12:00:00Z");
        let now = ts("2026-07-16T12:10:00Z"); // age 600 == 2*300
        assert_eq!(
            freshness_from_heartbeat(since, Some(hb), Some(300), now),
            Freshness::Fresh
        );
    }

    #[test]
    fn oneshot_pass_has_no_cadence_to_miss() {
        // No interval: however old, age alone can't call it stale (same rule as heartbeat_warning).
        let since = ts("2026-07-16T12:00:00Z");
        let hb = ts("2026-07-16T12:01:00Z");
        let now = ts("2026-07-18T12:00:00Z");
        assert_eq!(freshness_from_heartbeat(since, Some(hb), None, now), Freshness::Fresh);
    }

    #[test]
    fn landed_is_git_verified_done_is_self_reported() {
        let observed = ts("2026-07-16T12:00:00Z");
        let hb = ts("2026-07-16T12:01:00Z");
        let now = ts("2026-07-16T12:02:00Z");
        let landed = Provenance::git_verified(observed, Some(hb), Some(300), now);
        assert_eq!(landed.source, SOURCE_GIT_VERIFY);
        assert_eq!(landed.observed_at, Some(observed));
        assert_eq!(landed.freshness, Freshness::Fresh);

        let done = Provenance::self_reported(Some(observed));
        assert_eq!(done.source, SOURCE_SELF_REPORT);
        assert!(done.freshness.is_unknown(), "a self-report is never git-verified");

        // A checked completion: cv ran the predicate, so it is observed (not self-report) and fresh
        // at the moment it ran — distinct source, distinct freshness from a bare Done.
        let checked = Provenance::checked(Some(observed));
        assert_eq!(checked.source, SOURCE_CHECKED);
        assert_eq!(checked.observed_at, Some(observed));
        assert_eq!(checked.freshness, Freshness::Fresh);
        assert_ne!(checked.source, SOURCE_SELF_REPORT, "a checked done is not self-report");
    }

    #[test]
    fn awaiting_land_hides_observed_at_until_a_pass_covers_it() {
        let since = ts("2026-07-16T12:00:00Z");
        let now = ts("2026-07-16T12:05:00Z");
        // Never verified: unknown, no observation to date.
        let never = Provenance::awaiting_land(since, None, Some(300), now);
        assert!(never.freshness.is_unknown());
        assert_eq!(never.observed_at, None);
        // A pass after it became landable: fresh, dated by the pass.
        let hb = ts("2026-07-16T12:04:00Z");
        let checked = Provenance::awaiting_land(since, Some(hb), Some(300), now);
        assert_eq!(checked.freshness, Freshness::Fresh);
        assert_eq!(checked.observed_at, Some(hb));
    }
}
