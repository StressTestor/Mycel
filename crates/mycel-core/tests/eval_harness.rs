use chrono::{TimeZone, Utc};
use mycel_core::run_v0_1_harness;

#[test]
fn v0_1_harness_reports_roadmap_metrics_from_full_corpus() {
    let metrics =
        run_v0_1_harness(Utc.with_ymd_and_hms(2026, 5, 28, 9, 0, 0).unwrap()).expect("run harness");

    assert!(metrics.antibody_count >= 25);
    assert!(metrics.sentinel_event_count >= 10);
    assert!(metrics.eval_fixture_count >= 50);
    assert_eq!(metrics.pass_count, metrics.eval_fixture_count);
    assert_eq!(metrics.fail_count, 0);
    assert!(metrics.false_positive_rate < 0.20);
    assert!(metrics.expiry_fixture_count >= 10);
    assert_eq!(metrics.expiry_pass_count, metrics.expiry_fixture_count);
    assert!(metrics.refusals_missing_remediation == 0);
    assert!(metrics.refusals_missing_source_pointer == 0);
    assert!(metrics.gate_scope_counts.agent_launch >= 1);
    assert!(metrics.gate_scope_counts.tool_invocation >= 1);
    assert!(metrics.gate_scope_counts.substrate_mutation >= 1);
    assert!(metrics.interop_loss_matrix_shapes >= 4);
}
