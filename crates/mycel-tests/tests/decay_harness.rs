//! Fixture-driven decay harness for the TTL-tiered decay engine.
//!
//! Loads `tests/fixtures/decay_cases.jsonl`, runs each case through a fresh
//! in-memory database, and asserts the per-row and aggregate metrics that
//! are the v0.2 roadmap success criteria.

use mycel_core::{Confidence, DecayEngine, RunKind, RunStatus, Substrate};
use mycel_tests::{load_jsonl, test_db};
use serde::Deserialize;

// ── fixture schema ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct DecayCase {
    name: String,
    now: i64,
    run: RunInput,
    expect_state: String,
}

#[derive(Debug, Deserialize)]
struct RunInput {
    summary: String,
    confidence: String,
    expires_at: Option<i64>,
    no_compost: bool,
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn parse_confidence(s: &str) -> Confidence {
    match s {
        "solid" => Confidence::Solid,
        "directional" => Confidence::Directional,
        "vibes" => Confidence::Vibes,
        _ => panic!("unknown confidence value in fixture: {s}"),
    }
}

// ── harness ───────────────────────────────────────────────────────────────────

#[test]
fn decay_harness_runs_all_fixtures_and_meets_roadmap_metrics() {
    let cases: Vec<DecayCase> = load_jsonl("tests/fixtures/decay_cases.jsonl");

    let mut retained_count = 0usize;
    let mut distilled_count = 0usize;
    let mut decayed_count = 0usize;
    let mut live_count = 0usize;
    let mut preserved_count = 0usize;

    // Track no_compost=true cases to verify 100% preservation.
    let nc_true_total = cases.iter().filter(|c| c.run.no_compost).count();
    let mut nc_true_preserved = 0usize;

    for case in &cases {
        let db = test_db();
        let substrate = Substrate::new(&db);

        let conf = parse_confidence(&case.run.confidence);
        let id = substrate
            .insert_with_decay(
                RunKind::Observation,
                RunStatus::Applied,
                &case.run.summary,
                conf,
                case.run.expires_at,
                case.run.no_compost,
            )
            .unwrap_or_else(|e| panic!("[{}] insert failed: {e}", case.name));

        DecayEngine::new(&db)
            .run(case.now)
            .unwrap_or_else(|e| panic!("[{}] engine.run failed: {e}", case.name));

        let run = substrate
            .get(&id)
            .unwrap_or_else(|e| panic!("[{}] get failed: {e}", case.name))
            .unwrap_or_else(|| panic!("[{}] run not found after insert", case.name));

        match case.expect_state.as_str() {
            "retained" => {
                assert_eq!(
                    run.decay_state.as_deref(),
                    Some("retained"),
                    "[{}] expected decay_state=retained, got {:?}",
                    case.name,
                    run.decay_state
                );
                retained_count += 1;
            }
            "distilled" => {
                assert_eq!(
                    run.decay_state.as_deref(),
                    Some("distilled"),
                    "[{}] expected decay_state=distilled, got {:?}",
                    case.name,
                    run.decay_state
                );
                assert!(
                    run.distilled_summary.is_some(),
                    "[{}] distilled rows must have distilled_summary set",
                    case.name
                );
                distilled_count += 1;
            }
            "decayed" => {
                assert_eq!(
                    run.decay_state.as_deref(),
                    Some("decayed"),
                    "[{}] expected decay_state=decayed, got {:?}",
                    case.name,
                    run.decay_state
                );
                assert!(
                    run.distilled_summary.is_none(),
                    "[{}] decayed rows must NOT have distilled_summary",
                    case.name
                );
                decayed_count += 1;
            }
            "live" => {
                assert!(
                    run.decay_state.is_none(),
                    "[{}] expected decay_state=None (live), got {:?}",
                    case.name,
                    run.decay_state
                );
                assert!(
                    !run.no_compost,
                    "[{}] live rows must have no_compost=false",
                    case.name
                );
                live_count += 1;
            }
            "preserved" => {
                assert!(
                    run.decay_state.is_none(),
                    "[{}] expected decay_state=None (preserved), got {:?}",
                    case.name,
                    run.decay_state
                );
                assert!(
                    run.no_compost,
                    "[{}] preserved rows must have no_compost=true",
                    case.name
                );
                preserved_count += 1;
                if case.run.no_compost {
                    nc_true_preserved += 1;
                }
            }
            other => panic!("[{}] unknown expect_state in fixture: {other}", case.name),
        }
    }

    let total = cases.len();

    // ── roadmap success metrics ───────────────────────────────────────────────

    println!(
        "\n=== decay harness metrics ===\n\
         retained:   {retained_count}\n\
         distilled:  {distilled_count}\n\
         decayed:    {decayed_count}\n\
         live:       {live_count}\n\
         preserved:  {preserved_count}\n\
         total:      {total}\n\
         ===========================\n"
    );

    assert!(
        total >= 40,
        "fixture corpus must have at least 40 cases (got {total})"
    );

    assert!(
        distilled_count >= 10,
        "must have >= 10 distilled cases to prove directional-tier decay (got {distilled_count})"
    );

    assert!(
        decayed_count >= 10,
        "must have >= 10 decayed cases to prove vibes-tier decay (got {decayed_count})"
    );

    assert_eq!(
        nc_true_preserved, nc_true_total,
        "100% no-compost preservation required: {nc_true_preserved}/{nc_true_total} preserved"
    );
}
