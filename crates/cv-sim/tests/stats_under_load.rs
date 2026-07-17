//! FleetStats under synthetic load: generate real-shaped fleets (validated by the real
//! reducer) and assert the stats projection's invariants hold at volume — rates are
//! probabilities, medians respect the scenario's latency envelopes, rubber-stamp and
//! same-family counts track their configured rates, and small-n rows still carry raw counts.

use cv_sim::{render_stats_table, FleetScenario, Pathology};

/// The load scenario: 12 endpoints, 4 reviewers, 300 tasks — big enough for the configured
/// rates to show, small enough to run in a test.
fn load_scenario(seed: u64) -> FleetScenario {
    FleetScenario {
        endpoints: 12,
        reviewers: 4,
        tasks: 300,
        seed,
        pathology: Pathology {
            refute_rate: 0.15,
            abandon_rate: 0.08,
            rubber_stamp_rate: 0.25,
            same_family_rate: 0.30,
            review_latency: (600, 7_200),
            land_delay: (300, 3_600),
        },
    }
}

const SEEDS: [u64; 3] = [1, 42, 20260716];

#[test]
fn generation_is_deterministic_from_seed() {
    let a = load_scenario(42).generate().unwrap();
    let b = load_scenario(42).generate().unwrap();
    assert_eq!(a, b, "same seed must generate byte-identical events");
    let c = load_scenario(43).generate().unwrap();
    assert_ne!(a, c, "a different seed must generate a different fleet");
}

#[test]
fn rates_are_probabilities_and_counts_are_consistent() {
    for seed in SEEDS {
        let stats = load_scenario(seed).stats().unwrap();
        assert!(!stats.endpoints.is_empty() && !stats.reviewers.is_empty());
        for e in &stats.endpoints {
            if let Some(rate) = e.landed_rate {
                assert!(
                    (0.0..=1.0).contains(&rate),
                    "seed {seed}: landed_rate {rate} not in [0,1]"
                );
            }
            assert_eq!(
                e.landed + e.refuted + e.superseded + e.abandoned_live + e.unlanded,
                e.proposed,
                "seed {seed}: endpoint {} outcome counts must partition proposed",
                e.endpoint
            );
        }
        for r in &stats.reviewers {
            assert_eq!(
                r.passes + r.refutes,
                r.verdicts,
                "seed {seed}: reviewer {} verdicts must equal passes + refutes",
                r.reviewer
            );
            assert!(r.same_family_passes <= r.passes);
            assert!(r.no_receipts_passes + r.no_contact_passes <= r.passes);
        }
    }
}

#[test]
fn medians_stay_within_scenario_latency_bounds() {
    for seed in SEEDS {
        let sc = load_scenario(seed);
        let (lat_min, lat_max) = sc.pathology.review_latency;
        let (land_min, land_max) = sc.pathology.land_delay;
        let stats = sc.stats().unwrap();

        for r in &stats.reviewers {
            let m = r
                .median_review_latency_secs
                .expect("every reviewer has verdicts under load");
            assert!(
                (lat_min as i64..=lat_max as i64).contains(&m),
                "seed {seed}: reviewer {} median latency {m}s outside [{lat_min}, {lat_max}]",
                r.reviewer
            );
        }
        // Time-to-land is (propose→verdict) + (verdict→landed), both uniform draws.
        let (min, max) = ((lat_min + land_min) as i64, (lat_max + land_max) as i64);
        for e in &stats.endpoints {
            if let Some(m) = e.median_secs_to_land {
                assert!(
                    (min..=max).contains(&m),
                    "seed {seed}: endpoint {} median time-to-land {m}s outside [{min}, {max}]",
                    e.endpoint
                );
            }
        }
    }
}

#[test]
fn rubber_stamp_passes_track_the_configured_rate() {
    for seed in SEEDS {
        let sc = load_scenario(seed);
        let stats = sc.stats().unwrap();
        let passes: usize = stats.reviewers.iter().map(|r| r.passes).sum();
        let rubber: usize = stats
            .reviewers
            .iter()
            .map(|r| r.no_receipts_passes + r.no_contact_passes)
            .sum();
        assert!(passes > 200, "seed {seed}: load scenario should produce many passes");
        let observed = rubber as f64 / passes as f64;
        let configured = sc.pathology.rubber_stamp_rate;
        assert!(
            (observed - configured).abs() < 0.07,
            "seed {seed}: rubber-stamp rate {observed:.3} strays from configured {configured} \
             ({rubber}/{passes} passes)"
        );
    }
}

#[test]
fn same_family_passes_track_the_configured_rate() {
    for seed in SEEDS {
        let sc = load_scenario(seed);
        let stats = sc.stats().unwrap();
        let passes: usize = stats.reviewers.iter().map(|r| r.passes).sum();
        let same: usize = stats.reviewers.iter().map(|r| r.same_family_passes).sum();
        let observed = same as f64 / passes as f64;
        let configured = sc.pathology.same_family_rate;
        assert!(
            (observed - configured).abs() < 0.07,
            "seed {seed}: same-family rate {observed:.3} strays from configured {configured} \
             ({same}/{passes} passes)"
        );
        // The family rows aggregate the same observations.
        let family_total: usize = stats.families.iter().map(|f| f.reviews_given).sum();
        assert_eq!(family_total, passes, "every sim pass records a reviewer family");
    }
}

#[test]
fn small_n_rows_still_carry_raw_counts() {
    // Two tasks, both abandoned: revisions stranded live, no terminal revision outcomes.
    let sc = FleetScenario {
        endpoints: 1,
        reviewers: 1,
        tasks: 2,
        seed: 7,
        pathology: Pathology {
            refute_rate: 0.0,
            abandon_rate: 1.0,
            rubber_stamp_rate: 0.0,
            same_family_rate: 0.0,
            review_latency: (600, 7_200),
            land_delay: (300, 3_600),
        },
    };
    let stats = sc.stats().unwrap();
    assert_eq!(stats.endpoints.len(), 1);
    let e = &stats.endpoints[0];
    assert_eq!((e.proposed, e.abandoned_live), (2, 2), "raw counts survive at small n");
    assert_eq!(e.landed_rate, None, "no terminal outcome → no rate, never 0.0");
    assert_eq!(e.median_secs_to_land, None, "no landed revision → no median, never 0");
    assert!(stats.reviewers.is_empty(), "no verdicts were given");
}

/// Human-eyeball artifact: the rendered stats table for a fixed fixture scenario, committed
/// as a snapshot. Regenerate deliberately with `CV_SIM_BLESS=1 cargo test -p cv-sim`.
#[test]
fn stats_table_snapshot_matches_committed_fixture() {
    let sc = FleetScenario {
        endpoints: 4,
        reviewers: 2,
        tasks: 40,
        seed: 7,
        pathology: Pathology {
            refute_rate: 0.15,
            abandon_rate: 0.10,
            rubber_stamp_rate: 0.25,
            same_family_rate: 0.30,
            review_latency: (600, 7_200),
            land_delay: (300, 3_600),
        },
    };
    let rendered = render_stats_table(&sc.stats().unwrap());
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/stats_table.txt");
    if std::env::var_os("CV_SIM_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &rendered).unwrap();
    }
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading snapshot {}: {e} (bless with CV_SIM_BLESS=1)", path.display()));
    assert_eq!(
        rendered, committed,
        "rendered stats table diverged from the committed snapshot; if the change is \
         intentional, re-bless with CV_SIM_BLESS=1 cargo test -p cv-sim"
    );
}
