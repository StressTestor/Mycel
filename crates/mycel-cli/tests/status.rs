use assert_cmd::Command;
use mycel_core::AntibodyStore;
use predicates::prelude::*;

/// Create an initialized (migrated) substrate db at `path`, as install.sh would.
/// The status/list-candidates subcommands must never create the db themselves.
fn init_db(path: &std::path::Path) {
    AntibodyStore::open(path).expect("init db");
}

fn write_events(path: &std::path::Path) {
    std::fs::write(
        path,
        [
            r#"{"timestamp":"2026-05-28T08:00:00Z","tool_name":"shell","action":"block","reason":"rm denied","matched_rule":"deny.commands: rm -rf","mode":"enforce"}"#,
            r#"{"timestamp":"2026-05-28T08:01:00Z","tool_name":"shell","action":"warn","reason":"outside project","matched_rule":"allow.paths: src/**","mode":"audit"}"#,
            r#"{"timestamp":"2026-05-28T08:02:00Z","tool_name":"read","action":"allow","reason":null,"matched_rule":null,"mode":"audit"}"#,
        ]
        .join("\n"),
    )
    .expect("write events jsonl");
}

#[test]
fn status_on_empty_db_reports_zero_counts_and_no_maintenance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("mycel.db");
    init_db(&db);

    Command::cargo_bin("mycel-substrate")
        .expect("mycel binary")
        .args(["status", "--db", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""antibody_count": 0"#))
        .stdout(predicate::str::contains(r#""sentinel_event_count": 0"#))
        .stdout(predicate::str::contains(r#""audit_bytes": 0"#))
        .stdout(predicate::str::contains(r#""last_maintenance": null"#));
}

#[test]
fn status_missing_db_errors_and_creates_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("substrate").join("mycel.db");
    assert!(!missing.exists(), "precondition: db absent");

    Command::cargo_bin("mycel-substrate")
        .expect("mycel binary")
        .args(["status", "--db", missing.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("substrate db missing"));

    assert!(!missing.exists(), "status must not create the db");
}

#[test]
fn list_candidates_missing_db_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("substrate").join("mycel.db");

    Command::cargo_bin("mycel-substrate")
        .expect("mycel binary")
        .args(["list-candidates", "--db", missing.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("substrate db missing"));

    assert!(!missing.exists(), "list-candidates must not create the db");
}

#[test]
fn list_candidates_empty_db_returns_empty_array() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("mycel.db");
    init_db(&db);

    Command::cargo_bin("mycel-substrate")
        .expect("mycel binary")
        .args(["list-candidates", "--db", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("[]"));
}

#[test]
fn ingest_then_status_and_list_candidates_reflect_events() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("mycel.db");
    let events = dir.path().join("events.jsonl");
    init_db(&db);
    write_events(&events);

    Command::cargo_bin("mycel-substrate")
        .expect("mycel binary")
        .args([
            "ingest",
            "--db",
            db.to_str().unwrap(),
            "--jsonl",
            events.to_str().unwrap(),
        ])
        .assert()
        .success();

    // status now reports 3 sentinel events, still zero trusted antibodies.
    Command::cargo_bin("mycel-substrate")
        .expect("mycel binary")
        .args(["status", "--db", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""antibody_count": 0"#))
        .stdout(predicate::str::contains(r#""sentinel_event_count": 3"#));

    // list-candidates derives one candidate per stored event.
    Command::cargo_bin("mycel-substrate")
        .expect("mycel binary")
        .args(["list-candidates", "--db", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""tool_name": "shell""#))
        .stdout(predicate::str::contains(r#""action": "block""#));
}

#[test]
fn maintain_then_status_reports_last_maintenance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("mycel.db");
    let workspace = dir.path().join("workspace");
    init_db(&db);

    Command::cargo_bin("mycel-substrate")
        .expect("mycel binary")
        .args([
            "maintain",
            "--db",
            db.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
            "--now",
            "1000",
        ])
        .assert()
        .success();

    Command::cargo_bin("mycel-substrate")
        .expect("mycel binary")
        .args(["status", "--db", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""last_maintenance""#))
        .stdout(predicate::str::contains(r#""retained""#))
        .stdout(predicate::str::contains(r#""last_maintenance": null"#).not());
}
