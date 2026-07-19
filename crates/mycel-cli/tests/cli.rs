use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn cli_runs_v0_1_harness_through_mcp_surface() {
    let mut cmd = Command::cargo_bin("mycel-substrate").expect("mycel binary");

    cmd.arg("harness").assert().success().stdout(
        predicate::str::contains(r#""eval_fixture_count""#)
            .and(predicate::str::contains(r#""false_positive_rate""#))
            .and(predicate::str::contains(r#""sentinel_event_count""#)),
    );
}

#[test]
fn cli_import_promptpressure_then_maintain_produces_decay_projections() {
    use std::io::Write;

    // Set up temp db and workspace.
    let db_file = tempfile::NamedTempFile::new().expect("temp db file");
    let workspace = tempfile::tempdir().expect("temp workspace dir");

    // Write a small inline JSONL with three tiers:
    // - verified (TTL 365d, stays live after 40d)
    // - probable (TTL 30d, distilled after 40d)
    // - speculative (TTL 7d, decayed after 40d)
    let mut jsonl = tempfile::NamedTempFile::new().expect("temp jsonl file");
    writeln!(
        jsonl,
        r#"{{"source_id":"pp-cli-001","tier":"verified","summary":"verified finding for cli test"}}"#
    )
    .unwrap();
    writeln!(
        jsonl,
        r#"{{"source_id":"pp-cli-002","tier":"probable","summary":"probable finding for cli test"}}"#
    )
    .unwrap();
    writeln!(
        jsonl,
        r#"{{"source_id":"pp-cli-003","tier":"speculative","summary":"speculative-unique-xyz hypothesis for cli test"}}"#
    )
    .unwrap();
    jsonl.flush().unwrap();

    // Import at now=1_000_000.
    let import_now: i64 = 1_000_000;
    Command::cargo_bin("mycel-substrate")
        .expect("mycel binary")
        .args([
            "import-promptpressure",
            "--db",
            db_file.path().to_str().unwrap(),
            "--jsonl",
            jsonl.path().to_str().unwrap(),
            "--now",
            &import_now.to_string(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""imported": 3"#));

    // Run maintain at now = import_now + 40 days in seconds (> 30d TTL for probable, > 7d for speculative).
    let maintain_now: i64 = import_now + 40 * 24 * 60 * 60;
    Command::cargo_bin("mycel-substrate")
        .expect("mycel binary")
        .args([
            "maintain",
            "--db",
            db_file.path().to_str().unwrap(),
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--now",
            &maintain_now.to_string(),
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains(r#""distilled""#)
                .and(predicate::str::contains(r#""decayed""#)),
        );

    // Verify files exist.
    let substrate_path = workspace.path().join("SUBSTRATE.md");
    let compost_path = workspace.path().join("COMPOST.md");

    assert!(
        substrate_path.exists(),
        "SUBSTRATE.md must exist after maintain"
    );
    assert!(
        compost_path.exists(),
        "COMPOST.md must exist after maintain"
    );

    // Read and inspect content.
    let substrate_md = std::fs::read_to_string(&substrate_path).expect("read SUBSTRATE.md");
    let compost_md = std::fs::read_to_string(&compost_path).expect("read COMPOST.md");

    // SUBSTRATE.md structure.
    assert!(
        substrate_md.contains("# substrate"),
        "SUBSTRATE.md must contain # substrate heading"
    );
    // The verified finding is still live (365d TTL, only 40d elapsed).
    assert!(
        substrate_md.contains("verified finding for cli test"),
        "SUBSTRATE.md must contain the still-live verified finding"
    );

    // COMPOST.md structure.
    assert!(
        compost_md.contains("## distilled"),
        "COMPOST.md must have a ## distilled section"
    );
    assert!(
        compost_md.contains("## decayed"),
        "COMPOST.md must have a ## decayed section"
    );

    // The speculative summary must NOT appear in compost (tombstone only).
    assert!(
        !compost_md.contains("speculative-unique-xyz"),
        "original speculative summary must not appear in COMPOST.md (tombstone only)"
    );
}
