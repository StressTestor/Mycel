//! Fixture-driven sclerotia harness for the v0.4 dormant-work milestone.
//!
//! Three metric blocks:
//!   1. wake conditions  — `tests/fixtures/wake_conditions.jsonl`, asserts each
//!      typed condition evaluates deterministically (>= 30 fixtures).
//!   2. serialize/restore — `tests/fixtures/sclerotia_records.jsonl`, asserts each
//!      dormant record round-trips through `SclerotiumStore` and references a
//!      self-spec-compatible task identity (>= 10 records).
//!   3. resume gate       — proves no dormant record resumes without passing
//!      antibody evaluation, and that resume is never auto-executed.

use chrono::Utc;
use mycel_core::{
    evaluate_resume, Antibody, AntibodySource, AntibodyStore, Confidence, RefusalMode,
    ResumeDecision, Sclerotium, SclerotiumStore, Severity, Signature, SignatureScope, TaskIdentity,
    WakeCondition, WakeWorld,
};
use mycel_tests::{load_jsonl, test_db};
use serde::Deserialize;
use uuid::Uuid;

// ── wake-condition fixtures ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct WakeCase {
    name: String,
    condition: WakeCondition,
    world: WakeWorld,
    expect_met: bool,
}

#[test]
fn wake_conditions_evaluate_deterministically() {
    let cases: Vec<WakeCase> = load_jsonl("tests/fixtures/wake_conditions.jsonl");

    let mut met = 0usize;
    let mut unmet = 0usize;
    // per-variant tallies: (met, unmet)
    let mut time = (0usize, 0usize);
    let mut fexist = (0usize, 0usize);
    let mut fabsent = (0usize, 0usize);
    let mut dep = (0usize, 0usize);
    let mut sig = (0usize, 0usize);
    let mut manual = (0usize, 0usize);

    for case in &cases {
        let actual = case.condition.is_met(&case.world);
        assert_eq!(
            actual, case.expect_met,
            "[{}] is_met mismatch: expected {}, got {}",
            case.name, case.expect_met, actual
        );

        let bucket = match &case.condition {
            WakeCondition::TimeReached { .. } => &mut time,
            WakeCondition::FileExists { .. } => &mut fexist,
            WakeCondition::FileAbsent { .. } => &mut fabsent,
            WakeCondition::DependencyResolved { .. } => &mut dep,
            WakeCondition::SignalRaised { .. } => &mut sig,
            WakeCondition::Manual => &mut manual,
        };
        if actual {
            met += 1;
            bucket.0 += 1;
        } else {
            unmet += 1;
            bucket.1 += 1;
        }
    }

    let total = cases.len();
    println!(
        "\n=== wake-condition harness metrics ===\n\
         total:               {total}\n\
         met / unmet:         {met} / {unmet}\n\
         time_reached:        met {} unmet {}\n\
         file_exists:         met {} unmet {}\n\
         file_absent:         met {} unmet {}\n\
         dependency_resolved: met {} unmet {}\n\
         signal_raised:       met {} unmet {}\n\
         manual:              met {} unmet {}\n\
         ======================================\n",
        time.0,
        time.1,
        fexist.0,
        fexist.1,
        fabsent.0,
        fabsent.1,
        dep.0,
        dep.1,
        sig.0,
        sig.1,
        manual.0,
        manual.1
    );

    assert!(
        total >= 30,
        "wake-condition corpus must have >= 30 fixtures (got {total})"
    );
    // every variant must be exercised in both directions (Manual is always unmet).
    assert!(
        time.0 > 0 && time.1 > 0,
        "time_reached needs met+unmet cases"
    );
    assert!(fexist.0 > 0 && fexist.1 > 0, "file_exists needs met+unmet");
    assert!(
        fabsent.0 > 0 && fabsent.1 > 0,
        "file_absent needs met+unmet"
    );
    assert!(
        dep.0 > 0 && dep.1 > 0,
        "dependency_resolved needs met+unmet"
    );
    assert!(sig.0 > 0 && sig.1 > 0, "signal_raised needs met+unmet");
    assert!(
        manual.0 == 0 && manual.1 > 0,
        "manual must never be met (got met {})",
        manual.0
    );
}

// ── serialize / restore + task-identity reuse ───────────────────────────────────

#[test]
fn sclerotia_records_serialize_restore_and_reference_task_identity() {
    let records: Vec<Sclerotium> = load_jsonl("tests/fixtures/sclerotia_records.jsonl");
    let total = records.len();
    assert!(
        total >= 10,
        "dormant-work corpus must have >= 10 records (got {total})"
    );

    let mut round_trips = 0usize;
    for (i, original) in records.iter().enumerate() {
        // Metric 3: every dormant record references a self-spec-compatible task
        // identity — its signature is exactly the canonicalization of its description.
        assert_eq!(
            original.task.signature,
            TaskIdentity::canonicalize(&original.task.description),
            "[record {i}] signature must equal TaskIdentity::canonicalize(description)"
        );
        assert!(
            original.validate().is_ok(),
            "[record {i}] fixture record must be valid: {:?}",
            original.validate().err()
        );

        // Metric 2: serialize through the store and restore — must be byte-equal.
        let db = test_db();
        let store = SclerotiumStore::new(&db);
        let id = store
            .insert(original, 1000 + i as i64)
            .unwrap_or_else(|e| panic!("[record {i}] insert failed: {e}"));
        let restored = store
            .get(&id)
            .unwrap_or_else(|e| panic!("[record {i}] get failed: {e}"))
            .unwrap_or_else(|| panic!("[record {i}] record missing after insert"));
        assert_eq!(
            &restored, original,
            "[record {i}] restored record must equal original"
        );
        round_trips += 1;
    }

    println!(
        "\n=== sclerotia serialize/restore metrics ===\n\
         records:        {total}\n\
         round-tripped:  {round_trips}\n\
         (every record's signature == canonicalize(description))\n\
         ==========================================\n"
    );

    assert!(
        round_trips >= 10,
        "must serialize/restore >= 10 blocked-work examples (got {round_trips})"
    );
}

// ── resume gate (antibody-gated, manual-confirm only) ───────────────────────────

/// Seed an antibody store that refuses any run whose tool name is `rm`.
fn refusing_store() -> AntibodyStore {
    let store = AntibodyStore::open_in_memory().expect("open antibody store");
    let antibody = Antibody {
        id: Uuid::new_v4(),
        signature: Signature {
            error_class: None,
            file_pattern: None,
            agent_role: None,
            tool_pattern: Some("rm".to_string()),
            command_pattern: None,
            scope: SignatureScope::Project,
        },
        source: AntibodySource::Manual,
        severity: Severity::Refuse,
        confidence: Confidence::Solid,
        refusal_mode: RefusalMode::Hard,
        remediation: "do not delete files without explicit approval".to_string(),
        examples: vec!["rm -rf /".to_string()],
        created_at: Utc::now(),
        expires_at: None,
        hit_count: 0,
    };
    store
        .insert_antibody(&antibody)
        .expect("insert refusing antibody");
    store
}

#[test]
fn resume_is_antibody_gated_and_never_auto_executes() {
    let records: Vec<Sclerotium> = load_jsonl("tests/fixtures/sclerotia_records.jsonl");
    let store = refusing_store();
    // now far in the future so `time_reached: 0` records are wakeable; the
    // sets stay empty so file/dependency/signal/manual records are NOT wakeable.
    let world = WakeWorld {
        now: 2_000_000_000,
        ..Default::default()
    };
    let now = Utc::now();

    // A wakeable record whose next_command is `rm ...` must be blocked.
    let rm_record = records
        .iter()
        .find(|r| r.is_wakeable(&world) && r.next_command.starts_with("rm"))
        .expect("fixture must contain a wakeable rm-command record");
    let rm_decision = evaluate_resume(rm_record, &world, &store, now).expect("evaluate rm");
    assert_eq!(
        rm_decision,
        ResumeDecision::BlockedByAntibody,
        "wakeable rm-command record must be blocked by the antibody"
    );

    // A wakeable record with a safe command is allowed to await manual confirmation.
    let safe_record = records
        .iter()
        .find(|r| r.is_wakeable(&world) && r.next_command.starts_with("cargo"))
        .expect("fixture must contain a wakeable cargo-command record");
    let safe_decision = evaluate_resume(safe_record, &world, &store, now).expect("evaluate safe");
    assert_eq!(
        safe_decision,
        ResumeDecision::ReadyForManualResume,
        "wakeable safe-command record must be ready for manual resume"
    );

    // A record that is not wakeable in this world must report NotWakeable.
    let dormant_record = records
        .iter()
        .find(|r| !r.is_wakeable(&world))
        .expect("fixture must contain a not-yet-wakeable record");
    let dormant_decision =
        evaluate_resume(dormant_record, &world, &store, now).expect("evaluate dormant");
    assert_eq!(
        dormant_decision,
        ResumeDecision::NotWakeable,
        "not-wakeable record must report NotWakeable"
    );

    println!(
        "\n=== sclerotia resume-gate metrics ===\n\
         rm-command (wakeable):   {rm_decision:?}\n\
         safe-command (wakeable): {safe_decision:?}\n\
         dormant (not wakeable):  {dormant_decision:?}\n\
         note: ReadyForManualResume still requires explicit human confirmation;\n\
         no path auto-executes next_command.\n\
         =====================================\n"
    );

    // The decision vocabulary itself proves no auto-execution: the most permissive
    // outcome is "ready for *manual* resume".
    for r in &records {
        let d = evaluate_resume(r, &world, &store, now).expect("evaluate");
        assert!(
            matches!(
                d,
                ResumeDecision::NotWakeable
                    | ResumeDecision::BlockedByAntibody
                    | ResumeDecision::ReadyForManualResume
            ),
            "resume decision must be one of the three non-executing variants"
        );
    }
}
