use assert_cmd::Command;
use chrono::Utc;
use mycel_core::{AntibodyStore, EvaluationOutcome, ProposedRun, SignatureScope};
use predicates::prelude::*;

/// Create an initialized (migrated) substrate db at `path`, as install.sh would. antibody-add
/// itself must never create the db, so tests must stand one up first.
fn init_db(path: &std::path::Path) {
    AntibodyStore::open(path).expect("init db");
}

#[test]
fn antibody_add_inserts_and_is_refused_on_matching_command() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("mycel.db");
    init_db(&db);

    Command::cargo_bin("mycel")
        .expect("mycel binary")
        .args([
            "antibody-add",
            "--db",
            db.to_str().unwrap(),
            "--command-pattern",
            "rm -rf /",
            "--remediation",
            "never run a recursive root delete; scope the path explicitly",
            "--severity",
            "refuse",
            "--refusal-mode",
            "hard",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains(r#""outcome_preview":"refuse""#)
                .or(predicate::str::contains(r#""outcome_preview": "refuse""#)),
        )
        .stdout(predicate::str::contains(r#""id""#));

    // list-antibodies shows the inserted antibody.
    Command::cargo_bin("mycel")
        .expect("mycel binary")
        .args(["list-antibodies", "--db", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "never run a recursive root delete",
        ));

    // A matching run is refused (evaluated directly through mycel-core).
    let store = AntibodyStore::open(&db).expect("reopen");
    let eval = store
        .evaluate_run(
            &ProposedRun {
                error_class: None,
                file_path: None,
                agent_role: None,
                tool_name: Some("Bash".to_string()),
                command: Some("sudo rm -rf / --no-preserve-root".to_string()),
                scope: SignatureScope::Project,
            },
            Utc::now(),
        )
        .expect("evaluate");
    assert_eq!(
        eval.outcome,
        EvaluationOutcome::Refuse,
        "seeded curated antibody must refuse a matching command"
    );
}

#[test]
fn antibody_add_missing_db_errors_and_creates_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("substrate").join("mycel.db");
    assert!(!missing.exists(), "precondition: db absent");

    Command::cargo_bin("mycel")
        .expect("mycel binary")
        .args([
            "antibody-add",
            "--db",
            missing.to_str().unwrap(),
            "--command-pattern",
            "x",
            "--remediation",
            "y",
            "--severity",
            "warn",
            "--refusal-mode",
            "soft",
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("substrate db missing")
                .and(predicate::str::contains("install.sh")),
        );

    assert!(
        !missing.exists(),
        "antibody-add must never create the db: {}",
        missing.display()
    );
}

#[test]
fn antibody_add_requires_a_signature_field() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("mycel.db");
    init_db(&db);

    Command::cargo_bin("mycel")
        .expect("mycel binary")
        .args([
            "antibody-add",
            "--db",
            db.to_str().unwrap(),
            "--remediation",
            "y",
            "--severity",
            "warn",
            "--refusal-mode",
            "soft",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "antibody-add error: at least one signature field required (--command-pattern, --tool-name, --error-class, --file-pattern)",
        ));
}

#[test]
fn antibody_add_warns_on_non_refusing_combo() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("mycel.db");
    init_db(&db);

    // severity=refuse but refusal-mode=soft can never hard-block: warn on stderr, still inserts,
    // outcome_preview degrades to warn.
    Command::cargo_bin("mycel")
        .expect("mycel binary")
        .args([
            "antibody-add",
            "--db",
            db.to_str().unwrap(),
            "--tool-name",
            "dangerctl",
            "--remediation",
            "do not use dangerctl in project scope",
            "--severity",
            "refuse",
            "--refusal-mode",
            "soft",
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains(r#""outcome_preview":"warn""#)
                .or(predicate::str::contains(r#""outcome_preview": "warn""#)),
        )
        .stderr(
            predicate::str::contains("antibody-add warning")
                .and(predicate::str::contains("will NOT hard-block")),
        );
}
