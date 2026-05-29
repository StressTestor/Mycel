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

// v0.1.1 (Cluster 1, signature specificity): a single-field signature can no
// longer carry a hard refuse. The broad `shell`-only refuse that previously
// turned every shell run into a tripwire is demoted to an advisory warn at
// insertion, while a two-field signature still refuses. AND matching across
// populated fields continues to keep the `python` false positive away.
//
// before: tool-only `shell` refuse -> Refuse on any shell run (gap-found).
// after:  tool-only `shell` refuse demoted -> Warn; refuse reserved for
//         signatures specific enough (>= 2 fields) to justify it.
#[test]
fn false_positive_bait_demotes_broad_refuse_and_preserves_and_matching() {
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

    // Broad single-field refuse is demoted to a soft warn rather than hard
    // refusing all shell usage.
    assert_eq!(
        outcome(&store, &run(None, None, None, Some("shell"))),
        EvaluationOutcome::Warn
    );
    // A single-field warn is already advisory and stays a warn.
    assert_eq!(
        outcome(&store, &run(None, Some("README.md"), None, None)),
        EvaluationOutcome::Warn
    );
    // Two populated signature fields are an AND match, so a python run without
    // the secret_access error class is still allowed.
    assert_eq!(
        outcome(&store, &run(None, None, None, Some("python"))),
        EvaluationOutcome::Allow
    );
    // The same two-field signature still refuses when both fields match.
    assert_eq!(
        outcome(&store, &run(Some("secret_access"), None, None, Some("python"))),
        EvaluationOutcome::Refuse
    );
}

// v0.1.1 (Cluster 3, surface-variant normalization, partial): a deterministic
// normalization pass runs before matching. It case-folds tool and error fields
// (splitting separators and camelCase), canonicalizes file paths, and collapses
// whitespace / sorts arguments in command patterns. This closes the variants
// normalization can reach. Variants that need semantic similarity stay a
// documented v0.2 sqlite-vec item and are asserted here as still-allowed so the
// boundary is explicit.
#[test]
fn surface_variant_normalization_closes_reachable_variants_and_defers_semantic_ones() {
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

    // Closed gap: `permission_denied` and `PermissionDenied` normalize to the
    // same identifier, so the case/separator variant now matches and refuses.
    assert_eq!(
        outcome(
            &store,
            &run(Some("PermissionDenied"), None, None, Some("shell"))
        ),
        EvaluationOutcome::Refuse
    );
    // Deferred to v0.2 (semantic similarity): `src/config.rs` moving to
    // `src/settings/config.rs` is a rename, not a surface variant that path
    // canonicalization can reach, so it stays allowed under deterministic
    // matching.
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
    // Deferred to v0.2 (semantic similarity): `bash -lc cargo test` wraps
    // `cargo test`; recognizing the wrapper needs command understanding, not
    // whitespace/argument normalization, so it stays allowed.
    assert_eq!(
        outcome(&store, &run(None, None, None, Some("cargo test"))),
        EvaluationOutcome::Allow
    );
}

// v0.1.1 (Cluster 2, clock-skew expiry): an antibody is active only while
// `created_at <= now < expires_at`. Guarding the lower bound means a
// future-created antibody (clock skew) no longer gates runs before its creation
// instant.
//
// before: an antibody created one hour in the future still refused (gap-found).
// after:  it does not gate until `now` reaches its `created_at`.
#[test]
fn expiry_and_clock_skew_edges_are_handled_correctly() {
    let store = store();
    let base = now();

    let mut boundary = antibody(
        signature(Some("expiry_check"), None, None, Some("boundary-tool")),
        Severity::Refuse,
        RefusalMode::Hard,
        "boundary fixture",
    );
    boundary.expires_at = Some(base);
    insert(&store, boundary);

    let mut before = antibody(
        signature(Some("expiry_check"), None, None, Some("before-tool")),
        Severity::Refuse,
        RefusalMode::Hard,
        "before-expiry fixture",
    );
    before.expires_at = Some(base + Duration::milliseconds(1));
    insert(&store, before);

    let mut expired_hit = antibody(
        signature(Some("expiry_check"), None, None, Some("expired-hit-tool")),
        Severity::Refuse,
        RefusalMode::Hard,
        "expired-hit fixture",
    );
    expired_hit.expires_at = Some(base - Duration::seconds(1));
    expired_hit.hit_count = 99;
    insert(&store, expired_hit);

    let mut future_created = antibody(
        signature(Some("expiry_check"), None, None, Some("future-created-tool")),
        Severity::Refuse,
        RefusalMode::Hard,
        "clock-skew fixture",
    );
    future_created.created_at = base + Duration::hours(1);
    future_created.expires_at = Some(base + Duration::hours(2));
    insert(&store, future_created);

    // Lower-bound boundary equality: created_at == now is active (inclusive).
    let mut created_now = antibody(
        signature(Some("expiry_check"), None, None, Some("created-now-tool")),
        Severity::Refuse,
        RefusalMode::Hard,
        "created-at-boundary fixture",
    );
    created_now.created_at = base;
    insert(&store, created_now);

    // One millisecond into the future is not yet active.
    let mut created_just_after = antibody(
        signature(Some("expiry_check"), None, None, Some("created-future-tool")),
        Severity::Refuse,
        RefusalMode::Hard,
        "created-just-after-now fixture",
    );
    created_just_after.created_at = base + Duration::milliseconds(1);
    insert(&store, created_just_after);

    assert_eq!(
        outcome(
            &store,
            &run(Some("expiry_check"), None, None, Some("boundary-tool"))
        ),
        EvaluationOutcome::Allow
    );
    assert_eq!(
        outcome(
            &store,
            &run(Some("expiry_check"), None, None, Some("before-tool"))
        ),
        EvaluationOutcome::Refuse
    );
    assert_eq!(
        outcome(
            &store,
            &run(Some("expiry_check"), None, None, Some("expired-hit-tool"))
        ),
        EvaluationOutcome::Allow
    );
    // Closed gap: a future-created antibody no longer gates before its creation
    // instant.
    assert_eq!(
        outcome(
            &store,
            &run(Some("expiry_check"), None, None, Some("future-created-tool"))
        ),
        EvaluationOutcome::Allow
    );
    // created_at == now is active and still gates.
    assert_eq!(
        outcome(
            &store,
            &run(Some("expiry_check"), None, None, Some("created-now-tool"))
        ),
        EvaluationOutcome::Refuse
    );
    // created_at one millisecond after now is not yet active.
    assert_eq!(
        outcome(
            &store,
            &run(Some("expiry_check"), None, None, Some("created-future-tool"))
        ),
        EvaluationOutcome::Allow
    );
}

#[test]
fn signature_collision_resolution_is_deterministic_and_most_severe_wins() {
    for order in 0..6 {
        let store = store();
        let hard = antibody(
            signature(Some("collision_err"), None, None, Some("collision-tool")),
            Severity::Refuse,
            RefusalMode::Hard,
            "hard collision",
        );
        let soft = antibody(
            signature(Some("collision_err"), None, None, Some("collision-tool")),
            Severity::Warn,
            RefusalMode::Soft,
            "soft collision",
        );
        let log = antibody(
            signature(Some("collision_err"), None, None, Some("collision-tool")),
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
                outcome(
                    &store,
                    &run(Some("collision_err"), None, None, Some("collision-tool"))
                ),
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

    // v0.1.1 (Cluster 1): a present-but-empty stable field is rejected at
    // ingestion rather than normalized into a refuse-capable antibody with an
    // empty (wildcard) tool_pattern.
    // before: accepted, candidate with tool_pattern == Some("") (gap-found).
    // after:  rejected with an error.
    let empty_tool_name = r#"{"timestamp":"2026-05-28T08:00:00Z","tool_name":"","action":"block","reason":"x","matched_rule":"deny.paths: ~/.ssh/*","mode":"enforce"}"#;
    let result = tools.ingest_sentinel(empty_tool_name.as_bytes(), now());
    assert!(result.is_err());

    // A whitespace-only tool_name is the same present-but-empty case.
    let blank_tool_name = r#"{"timestamp":"2026-05-28T08:00:00Z","tool_name":"  ","action":"block","reason":"x","matched_rule":"deny.paths: ~/.ssh/*","mode":"enforce"}"#;
    assert!(tools.ingest_sentinel(blank_tool_name.as_bytes(), now()).is_err());
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

// --- v0.1.1 hardening fixtures: Cluster 1, signature specificity ---

#[test]
fn all_empty_string_signature_is_rejected_like_a_wildcard() {
    // The wildcard guard now treats present-but-empty (and whitespace-only)
    // fields as unpopulated, so a signature whose fields are all empty strings
    // is rejected exactly like an all-None signature.
    let store = store();
    let empty_strings = antibody(
        signature(Some(""), Some(""), Some("  "), Some("")),
        Severity::Refuse,
        RefusalMode::Hard,
        "would refuse everything if persisted",
    );
    assert!(store.insert_antibody(&empty_strings).is_err());

    let tools = McpTools::open_in_memory().expect("open MCP tools");
    assert!(tools.insert_antibodies([empty_strings]).is_err());
}

#[test]
fn single_field_refuse_is_demoted_to_warn_on_all_insertion_paths() {
    // before: a tool-only refuse hard-blocked every matching run (gap-found via
    //         the shell false-positive bait).
    // after:  it is demoted to a soft warn at insertion, on both the direct
    //         store path and the MCP insert path.
    let direct = store();
    insert(
        &direct,
        antibody(
            signature(None, None, None, Some("broad-tool")),
            Severity::Refuse,
            RefusalMode::Hard,
            "single field cannot justify refuse",
        ),
    );
    assert_eq!(
        outcome(&direct, &run(None, None, None, Some("broad-tool"))),
        EvaluationOutcome::Warn
    );

    let tools = McpTools::open_in_memory().expect("open MCP tools");
    tools
        .insert_antibodies([antibody(
            signature(None, None, None, Some("broad-tool")),
            Severity::Refuse,
            RefusalMode::Hard,
            "single field cannot justify refuse",
        )])
        .expect("single-field refuse is demoted, not rejected");
    assert_eq!(
        tools
            .evaluate(&run(None, None, None, Some("broad-tool")), now())
            .expect("evaluate")
            .outcome,
        EvaluationOutcome::Warn
    );
}

#[test]
fn two_field_refuse_is_specific_enough_to_persist() {
    let store = store();
    insert(
        &store,
        antibody(
            signature(Some("secret_access"), None, None, Some("python")),
            Severity::Refuse,
            RefusalMode::Hard,
            "two fields justify a hard refusal",
        ),
    );
    assert_eq!(
        outcome(
            &store,
            &run(Some("secret_access"), None, None, Some("python"))
        ),
        EvaluationOutcome::Refuse
    );
}

#[test]
fn sentinel_block_with_empty_tool_name_is_rejected_not_normalized() {
    let tools = McpTools::open_in_memory().expect("open MCP tools");
    let empty = r#"{"timestamp":"2026-05-28T08:00:00Z","tool_name":"","action":"block","reason":"x","matched_rule":"deny.paths: ~/.ssh/*","mode":"enforce"}"#;
    assert!(tools.ingest_sentinel(empty.as_bytes(), now()).is_err());

    // A non-empty tool_name still ingests under the locked block -> refuse mapping.
    let populated = r#"{"timestamp":"2026-05-28T08:00:00Z","tool_name":"shell","action":"block","reason":"x","matched_rule":"deny.paths: ~/.ssh/*","mode":"enforce"}"#;
    let candidates = tools
        .ingest_sentinel(populated.as_bytes(), now())
        .expect("non-empty tool_name ingests");
    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0].antibody.signature.tool_pattern.as_deref(),
        Some("shell")
    );
}

// --- v0.1.1 hardening fixtures: Cluster 3, surface-variant normalization ---

#[test]
fn case_and_separator_variants_of_tool_and_error_match() {
    let store = store();
    insert(
        &store,
        antibody(
            signature(Some("disk_full"), None, None, Some("apply_patch")),
            Severity::Refuse,
            RefusalMode::Hard,
            "case and separator normalization",
        ),
    );
    // Upper-/camel-cased tool and error fields normalize to the antibody's form.
    assert_eq!(
        outcome(
            &store,
            &run(Some("DiskFull"), None, None, Some("Apply_Patch"))
        ),
        EvaluationOutcome::Refuse
    );
}

#[test]
fn canonicalized_file_paths_match() {
    let store = store();
    insert(
        &store,
        antibody(
            signature(None, Some("src/config.rs"), None, Some("apply_patch")),
            Severity::Refuse,
            RefusalMode::Hard,
            "path canonicalization",
        ),
    );
    // A `./` prefix and a `..` round-trip canonicalize to the stored path.
    assert_eq!(
        outcome(
            &store,
            &run(
                None,
                Some("./src/foo/../config.rs"),
                None,
                Some("apply_patch")
            )
        ),
        EvaluationOutcome::Refuse
    );
}

#[test]
fn command_whitespace_and_argument_order_are_normalized() {
    let store = store();
    insert(
        &store,
        antibody(
            signature(None, None, None, Some("cargo test --all --release")),
            Severity::Warn,
            RefusalMode::Soft,
            "argument order normalization",
        ),
    );
    // Collapsed whitespace and reordered arguments match the same command.
    assert_eq!(
        outcome(
            &store,
            &run(None, None, None, Some("cargo   test  --release --all"))
        ),
        EvaluationOutcome::Warn
    );
}
