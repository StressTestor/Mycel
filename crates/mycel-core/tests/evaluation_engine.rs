use chrono::{Duration, TimeZone, Utc};
use mycel_core::{
    Antibody, AntibodySource, AntibodyStore, Confidence, EvaluationOutcome, ProposedRun,
    RefusalMode, Severity, Signature, SignatureScope,
};
use uuid::Uuid;

fn temp_store() -> (tempfile::TempDir, AntibodyStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = AntibodyStore::open(dir.path().join("mycel.sqlite")).expect("open store");
    (dir, store)
}

fn antibody(
    tool_pattern: Option<&str>,
    error_class: Option<&str>,
    severity: Severity,
    refusal_mode: RefusalMode,
    remediation: &str,
) -> Antibody {
    Antibody {
        id: Uuid::new_v4(),
        signature: Signature {
            error_class: error_class.map(str::to_string),
            file_pattern: None,
            agent_role: None,
            tool_pattern: tool_pattern.map(str::to_string),
            command_pattern: None,
            scope: SignatureScope::Project,
        },
        source: AntibodySource::Manual,
        severity,
        confidence: Confidence::Solid,
        refusal_mode,
        remediation: remediation.to_string(),
        examples: vec!["fixture example".to_string()],
        created_at: Utc.with_ymd_and_hms(2026, 5, 28, 8, 0, 0).unwrap(),
        expires_at: None,
        hit_count: 0,
    }
}

fn run(tool_name: &str, error_class: Option<&str>) -> ProposedRun {
    ProposedRun {
        error_class: error_class.map(str::to_string),
        file_path: None,
        agent_role: None,
        tool_name: Some(tool_name.to_string()),
        command: None,
        scope: SignatureScope::Project,
    }
}

fn seed_registry(store: &AntibodyStore) {
    for antibody in [
        antibody(
            Some("shell"),
            None,
            Severity::Refuse,
            RefusalMode::Hard,
            "use a narrower command or ask before touching protected paths",
        ),
        antibody(
            Some("cargo"),
            None,
            Severity::Warn,
            RefusalMode::Soft,
            "inspect the prior cargo failure before rerunning",
        ),
        antibody(
            Some("git"),
            None,
            Severity::Info,
            RefusalMode::LogOnly,
            "record git lineage only",
        ),
        antibody(
            Some("python"),
            Some("secret_access"),
            Severity::Refuse,
            RefusalMode::Hard,
            "remove secret material from the attempted script",
        ),
    ] {
        store.insert_antibody(&antibody).expect("insert antibody");
    }
}

#[test]
fn evaluation_uses_severity_and_refusal_mode_for_outcomes() {
    let (_dir, store) = temp_store();
    seed_registry(&store);
    let now = Utc.with_ymd_and_hms(2026, 5, 28, 9, 0, 0).unwrap();

    assert_eq!(
        store
            .evaluate_run(&run("shell", None), now)
            .expect("shell")
            .outcome,
        EvaluationOutcome::Refuse
    );
    assert_eq!(
        store
            .evaluate_run(&run("cargo", None), now)
            .expect("cargo")
            .outcome,
        EvaluationOutcome::Warn
    );
    assert_eq!(
        store
            .evaluate_run(&run("git", None), now)
            .expect("git")
            .outcome,
        EvaluationOutcome::Allow
    );
    assert_eq!(
        store
            .evaluate_run(&run("apply_patch", None), now)
            .expect("apply_patch")
            .outcome,
        EvaluationOutcome::Allow
    );
}

#[test]
fn every_refusal_carries_remediation_and_source_pointer() {
    let (_dir, store) = temp_store();
    seed_registry(&store);
    let now = Utc.with_ymd_and_hms(2026, 5, 28, 9, 0, 0).unwrap();

    let evaluation = store
        .evaluate_run(&run("python", Some("secret_access")), now)
        .expect("evaluate run");

    assert_eq!(evaluation.outcome, EvaluationOutcome::Refuse);
    let refusal = evaluation.refusal().expect("refusal match");
    assert!(!refusal.remediation.is_empty());
    assert!(refusal.source_pointer.starts_with("antibody:"));
}

#[test]
fn fixture_corpus_keeps_safe_false_positives_under_twenty_percent() {
    let (_dir, store) = temp_store();
    seed_registry(&store);
    let now = Utc.with_ymd_and_hms(2026, 5, 28, 9, 0, 0).unwrap();
    let mut fixtures = Vec::new();

    for _ in 0..20 {
        fixtures.push((run("shell", None), false, EvaluationOutcome::Refuse));
    }
    for _ in 0..10 {
        fixtures.push((run("cargo", None), false, EvaluationOutcome::Warn));
    }
    for _ in 0..5 {
        fixtures.push((
            run("python", Some("secret_access")),
            false,
            EvaluationOutcome::Refuse,
        ));
    }
    for tool in ["read", "apply_patch", "git", "node", "python"] {
        for _ in 0..7 {
            fixtures.push((run(tool, None), true, EvaluationOutcome::Allow));
        }
    }

    assert!(fixtures.len() >= 50);

    let mut safe_total = 0usize;
    let mut false_positives = 0usize;
    for (proposed_run, safe, expected_outcome) in fixtures {
        let evaluation = store
            .evaluate_run(&proposed_run, now)
            .expect("evaluate fixture");
        assert_eq!(evaluation.outcome, expected_outcome);
        if safe {
            safe_total += 1;
            if evaluation.outcome != EvaluationOutcome::Allow {
                false_positives += 1;
            }
        }
    }

    let false_positive_rate = false_positives as f64 / safe_total as f64;
    assert!(false_positive_rate < 0.20);
}

#[test]
fn expiry_passes_time_shifted_fixtures() {
    let (_dir, store) = temp_store();
    let base = Utc.with_ymd_and_hms(2026, 5, 28, 9, 0, 0).unwrap();

    for minute in 0..12 {
        let mut antibody = antibody(
            Some(&format!("tool-{minute}")),
            None,
            Severity::Refuse,
            RefusalMode::Hard,
            "expired antibodies should stop gating proposed runs",
        );
        antibody.expires_at = Some(base + Duration::minutes(minute));
        store.insert_antibody(&antibody).expect("insert antibody");
    }

    for minute in 0..12 {
        let proposed_run = run(&format!("tool-{minute}"), None);
        let before_expiry = base + Duration::minutes(minute) - Duration::seconds(1);
        let after_expiry = base + Duration::minutes(minute) + Duration::seconds(1);

        assert_eq!(
            store
                .evaluate_run(&proposed_run, before_expiry)
                .expect("before expiry")
                .outcome,
            EvaluationOutcome::Refuse
        );
        assert_eq!(
            store
                .evaluate_run(&proposed_run, after_expiry)
                .expect("after expiry")
                .outcome,
            EvaluationOutcome::Allow
        );
    }
}
