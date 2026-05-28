use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn cli_runs_v0_1_harness_through_mcp_surface() {
    let mut cmd = Command::cargo_bin("mycel").expect("mycel binary");

    cmd.arg("harness").assert().success().stdout(
        predicate::str::contains(r#""eval_fixture_count""#)
            .and(predicate::str::contains(r#""false_positive_rate""#))
            .and(predicate::str::contains(r#""sentinel_event_count""#)),
    );
}
