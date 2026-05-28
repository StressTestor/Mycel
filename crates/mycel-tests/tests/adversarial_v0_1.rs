use chrono::{Duration, TimeZone, Utc};
use mycel_core::{
    Antibody, AntibodySource, AntibodyStore, Confidence, EvaluationOutcome, ProposedRun,
    RefusalMode, Severity, Signature, SignatureScope,
};
use mycel_mcp::McpTools;
use uuid::Uuid;

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 28, 9, 0, 0).unwrap()
}

fn store() -> AntibodyStore {
    AntibodyStore::open_in_memory().expect("open in-memory store")
}

fn antibody(
    signature: Signature,
    severity: Severity,
    refusal_mode: RefusalMode,
    remediation: &str,
) -> Antibody {
    Antibody {
        id: Uuid::new_v4(),
        signature,
        source: AntibodySource::Manual,
        severity,
        confidence: Confidence::Solid,
        refusal_mode,
        remediation: remediation.to_string(),
        examples: vec!["adversarial fixture".to_string()],
        created_at: now(),
        expires_at: None,
        hit_count: 0,
    }
}

fn signature(
    error_class: Option<&str>,
    file_pattern: Option<&str>,
    agent_role: Option<&str>,
    tool_pattern: Option<&str>,
) -> Signature {
    Signature {
        error_class: error_class.map(str::to_string),
        file_pattern: file_pattern.map(str::to_string),
        agent_role: agent_role.map(str::to_string),
        tool_pattern: tool_pattern.map(str::to_string),
        scope: SignatureScope::Project,
    }
}

fn run(
    error_class: Option<&str>,
    file_path: Option<&str>,
    agent_role: Option<&str>,
    tool_name: Option<&str>,
) -> ProposedRun {
    ProposedRun {
        error_class: error_class.map(str::to_string),
        file_path: file_path.map(str::to_string),
        agent_role: agent_role.map(str::to_string),
        tool_name: tool_name.map(str::to_string),
        scope: SignatureScope::Project,
    }
}

fn insert(store: &AntibodyStore, antibody: Antibody) -> Uuid {
    let id = antibody.id;
    store.insert_antibody(&antibody).expect("insert antibody");
    id
}

fn outcome(store: &AntibodyStore, proposed_run: &ProposedRun) -> EvaluationOutcome {
    store
        .evaluate_run(proposed_run, now())
        .expect("evaluate run")
        .outcome
}

#[test]
fn false_positive_bait_exposes_broad_exact_field_overmatching() {
    let store = store();
    insert(
        &store,
        antibody(
            signature(None, None, None, Some("shell")),
            Severity::Refuse,
            RefusalMode::Hard,
            "previous shell run touched a protected path",
        ),
    );
    insert(
        &store,
        antibody(
            signature(None, Some("README.md"), None, None),
            Severity::Warn,
            RefusalMode::Soft,
            "previous README change failed review",
        ),
    );
    insert(
        &store,
        antibody(
            signature(Some("secret_access"), None, None, Some("python")),
            Severity::Refuse,
            RefusalMode::Hard,
            "previous python run exposed a secret",
        ),
    );

    assert_eq!(
        outcome(&store, &run(None, None, None, Some("shell"))),
        EvaluationOutcome::Refuse
    );
    assert_eq!(
        outcome(&store, &run(None, Some("README.md"), None, None)),
        EvaluationOutcome::Warn
    );
    assert_eq!(
        outcome(&store, &run(None, None, None, Some("python"))),
        EvaluationOutcome::Allow
    );
}

#[test]
fn false_negative_bait_exposes_surface_variant_under_matching() {
    let store = store();
    insert(
        &store,
        antibody(
            signature(Some("permission_denied"), None, None, Some("shell")),
            Severity::Refuse,
            RefusalMode::Hard,
            "permission failure repeated",
        ),
    );
    insert(
        &store,
        antibody(
            signature(None, Some("src/config.rs"), None, Some("apply_patch")),
            Severity::Refuse,
            RefusalMode::Hard,
            "config mutation failed",
        ),
    );
    insert(
        &store,
        antibody(
            signature(None, None, None, Some("bash -lc cargo test")),
            Severity::Warn,
            RefusalMode::Soft,
            "cargo command failed",
        ),
    );

    assert_eq!(
        outcome(
            &store,
            &run(Some("PermissionDenied"), None, None, Some("shell"))
        ),
        EvaluationOutcome::Allow
    );
    assert_eq!(
        outcome(
            &store,
            &run(
                None,
                Some("src/settings/config.rs"),
                None,
                Some("apply_patch")
            )
        ),
        EvaluationOutcome::Allow
    );
    assert_eq!(
        outcome(&store, &run(None, None, None, Some("cargo test"))),
        EvaluationOutcome::Allow
    );
}

#[test]
fn expiry_edge_cases_expose_boundary_and_clock_skew_behavior() {
    let store = store();
    let base = now();

    let mut boundary = antibody(
        signature(None, None, None, Some("boundary-tool")),
        Severity::Refuse,
        RefusalMode::Hard,
        "boundary fixture",
    );
    boundary.expires_at = Some(base);
    insert(&store, boundary);

    let mut before = antibody(
        signature(None, None, None, Some("before-tool")),
        Severity::Refuse,
        RefusalMode::Hard,
        "before-expiry fixture",
    );
    before.expires_at = Some(base + Duration::milliseconds(1));
    insert(&store, before);

    let mut expired_hit = antibody(
        signature(None, None, None, Some("expired-hit-tool")),
        Severity::Refuse,
        RefusalMode::Hard,
        "expired-hit fixture",
    );
    expired_hit.expires_at = Some(base - Duration::seconds(1));
    expired_hit.hit_count = 99;
    insert(&store, expired_hit);

    let mut future_created = antibody(
        signature(None, None, None, Some("future-created-tool")),
        Severity::Refuse,
        RefusalMode::Hard,
        "clock-skew fixture",
    );
    future_created.created_at = base + Duration::hours(1);
    future_created.expires_at = Some(base + Duration::hours(2));
    insert(&store, future_created);

    assert_eq!(
        outcome(&store, &run(None, None, None, Some("boundary-tool"))),
        EvaluationOutcome::Allow
    );
    assert_eq!(
        outcome(&store, &run(None, None, None, Some("before-tool"))),
        EvaluationOutcome::Refuse
    );
    assert_eq!(
        outcome(&store, &run(None, None, None, Some("expired-hit-tool"))),
        EvaluationOutcome::Allow
    );
    assert_eq!(
        outcome(&store, &run(None, None, None, Some("future-created-tool"))),
        EvaluationOutcome::Refuse
    );
}

#[test]
fn signature_collision_resolution_is_deterministic_and_most_severe_wins() {
    for order in 0..6 {
        let store = store();
        let hard = antibody(
            signature(None, None, None, Some("collision-tool")),
            Severity::Refuse,
            RefusalMode::Hard,
            "hard collision",
        );
        let soft = antibody(
            signature(None, None, None, Some("collision-tool")),
            Severity::Warn,
            RefusalMode::Soft,
            "soft collision",
        );
        let log = antibody(
            signature(None, None, None, Some("collision-tool")),
            Severity::Info,
            RefusalMode::LogOnly,
            "log collision",
        );

        match order % 3 {
            0 => {
                insert(&store, hard);
                insert(&store, soft);
                insert(&store, log);
            }
            1 => {
                insert(&store, soft);
                insert(&store, log);
                insert(&store, hard);
            }
            _ => {
                insert(&store, log);
                insert(&store, hard);
                insert(&store, soft);
            }
        }

        for _ in 0..5 {
            assert_eq!(
                outcome(&store, &run(None, None, None, Some("collision-tool"))),
                EvaluationOutcome::Refuse
            );
        }
    }
}

#[test]
fn malformed_sentinel_input_degrades_without_panicking() {
    let tools = McpTools::open_in_memory().expect("open MCP tools");

    let malformed = [
        (
            "truncated",
            r#"{"timestamp":"2026-05-28T08:00:00Z","tool_name":"shell""#,
        ),
        (
            "missing_timestamp",
            r#"{"tool_name":"shell","action":"block","reason":"x","matched_rule":"deny.paths: ~/.ssh/*","mode":"enforce"}"#,
        ),
        (
            "unknown_action",
            r#"{"timestamp":"2026-05-28T08:00:00Z","tool_name":"shell","action":"quarantine","reason":"x","matched_rule":"deny.paths: ~/.ssh/*","mode":"enforce"}"#,
        ),
        (
            "null_tool_name",
            r#"{"timestamp":"2026-05-28T08:00:00Z","tool_name":null,"action":"block","reason":"x","matched_rule":"deny.paths: ~/.ssh/*","mode":"enforce"}"#,
        ),
    ];

    for (_name, jsonl) in malformed {
        let result = tools.ingest_sentinel(jsonl.as_bytes(), now());
        assert!(result.is_err());
    }

    let empty_tool_name = r#"{"timestamp":"2026-05-28T08:00:00Z","tool_name":"","action":"block","reason":"x","matched_rule":"deny.paths: ~/.ssh/*","mode":"enforce"}"#;
    let candidates = tools
        .ingest_sentinel(empty_tool_name.as_bytes(), now())
        .expect("empty stable field is currently accepted");
    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0].antibody.signature.tool_pattern.as_deref(),
        Some("")
    );
}

#[test]
fn wildcard_explosion_is_blocked_by_public_persistence_paths() {
    let store = store();
    let wildcard = antibody(
        signature(None, None, None, None),
        Severity::Refuse,
        RefusalMode::Hard,
        "would refuse everything if persisted",
    );
    let direct_result = store.insert_antibody(&wildcard);
    assert!(direct_result.is_err());

    let tools = McpTools::open_in_memory().expect("open MCP tools");
    let mcp_result = tools.insert_antibodies([wildcard]);
    assert!(mcp_result.is_err());

    assert_eq!(
        outcome(&store, &run(None, None, None, Some("safe-tool"))),
        EvaluationOutcome::Allow
    );
}
