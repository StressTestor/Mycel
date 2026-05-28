use chrono::{Duration, Utc};
use mycel_core::{
    Antibody, AntibodySource, AntibodyStore, Confidence, ProposedRun, RefusalMode, Severity,
    Signature, SignatureScope,
};

fn temp_store() -> (tempfile::TempDir, AntibodyStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = AntibodyStore::open(dir.path().join("mycel.sqlite")).expect("open store");
    (dir, store)
}

fn antibody(signature: Signature) -> Antibody {
    Antibody {
        id: uuid::Uuid::new_v4(),
        signature,
        source: AntibodySource::Manual,
        severity: Severity::Warn,
        confidence: Confidence::Solid,
        refusal_mode: RefusalMode::Soft,
        remediation: "check the repeat failure before continuing".to_string(),
        examples: vec!["cargo test failed with the same tool".to_string()],
        created_at: Utc::now(),
        expires_at: None,
        hit_count: 0,
    }
}

#[test]
fn migration_applies_on_fresh_database() {
    let (_dir, store) = temp_store();

    assert!(store.schema_version().expect("schema version") >= 1);
}

#[test]
fn antibody_round_trips_through_sqlite() {
    let (_dir, store) = temp_store();
    let mut saved = antibody(Signature {
        error_class: Some("test_failure".to_string()),
        file_pattern: Some("crates/mycel-core/src/lib.rs".to_string()),
        agent_role: Some("builder".to_string()),
        tool_pattern: Some("cargo".to_string()),
        scope: SignatureScope::Project,
    });
    saved.expires_at = Some(Utc::now() + Duration::days(7));

    store.insert_antibody(&saved).expect("insert antibody");
    let loaded = store
        .get_antibody(saved.id)
        .expect("get antibody")
        .expect("stored antibody");

    assert_eq!(loaded, saved);
}

#[test]
fn antibody_can_be_updated_and_deleted() {
    let (_dir, store) = temp_store();
    let mut saved = antibody(Signature {
        error_class: None,
        file_pattern: None,
        agent_role: None,
        tool_pattern: Some("shell".to_string()),
        scope: SignatureScope::Project,
    });
    store.insert_antibody(&saved).expect("insert antibody");

    saved.severity = Severity::Refuse;
    saved.refusal_mode = RefusalMode::Hard;
    saved.hit_count = 3;
    store.update_antibody(&saved).expect("update antibody");

    let loaded = store
        .get_antibody(saved.id)
        .expect("get antibody")
        .expect("stored antibody");
    assert_eq!(loaded.severity, Severity::Refuse);
    assert_eq!(loaded.refusal_mode, RefusalMode::Hard);
    assert_eq!(loaded.hit_count, 3);

    store.delete_antibody(saved.id).expect("delete antibody");
    assert!(store
        .get_antibody(saved.id)
        .expect("get after delete")
        .is_none());
}

#[test]
fn populated_signature_fields_are_and_matched_and_empty_fields_are_wildcards() {
    let (_dir, store) = temp_store();
    let broad = antibody(Signature {
        error_class: None,
        file_pattern: None,
        agent_role: None,
        tool_pattern: Some("cargo".to_string()),
        scope: SignatureScope::Project,
    });
    let narrow = antibody(Signature {
        error_class: Some("test_failure".to_string()),
        file_pattern: Some("crates/mycel-core/src/lib.rs".to_string()),
        agent_role: Some("builder".to_string()),
        tool_pattern: Some("cargo".to_string()),
        scope: SignatureScope::Project,
    });
    let other_tool = antibody(Signature {
        error_class: None,
        file_pattern: None,
        agent_role: None,
        tool_pattern: Some("git".to_string()),
        scope: SignatureScope::Project,
    });
    store.insert_antibody(&broad).expect("insert broad");
    store.insert_antibody(&narrow).expect("insert narrow");
    store.insert_antibody(&other_tool).expect("insert other");

    let matches = store
        .matching_antibodies(&ProposedRun {
            error_class: Some("test_failure".to_string()),
            file_path: Some("crates/mycel-core/src/lib.rs".to_string()),
            agent_role: Some("builder".to_string()),
            tool_name: Some("cargo".to_string()),
            scope: SignatureScope::Project,
        })
        .expect("matching antibodies");

    let ids: Vec<_> = matches.into_iter().map(|a| a.id).collect();
    assert!(ids.contains(&broad.id));
    assert!(ids.contains(&narrow.id));
    assert!(!ids.contains(&other_tool.id));
}

#[test]
fn empty_signatures_are_rejected() {
    let (_dir, store) = temp_store();
    let saved = antibody(Signature {
        error_class: None,
        file_pattern: None,
        agent_role: None,
        tool_pattern: None,
        scope: SignatureScope::Project,
    });

    let err = store.insert_antibody(&saved).expect_err("empty signature");

    assert!(err.to_string().contains("at least one signature field"));
}
