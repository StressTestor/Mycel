use chrono::{TimeZone, Utc};
use mycel_core::{AntibodyStore, RefusalMode, SentinelAction, Severity, SignatureScope};

fn fixture_jsonl() -> String {
    [
        r#"{"timestamp":"2026-05-28T08:00:00Z","tool_name":"shell","action":"block","reason":"blocked ssh key access","matched_rule":"deny.paths: ~/.ssh/*","mode":"enforce"}"#,
        r#"{"timestamp":"2026-05-28T08:01:00Z","tool_name":"shell","action":"warn","reason":"outside project","matched_rule":"allow.paths: src/**","mode":"audit"}"#,
        r#"{"timestamp":"2026-05-28T08:02:00Z","tool_name":"apply_patch","action":"allow","reason":null,"matched_rule":null,"mode":"audit"}"#,
        r#"{"timestamp":"2026-05-28T08:03:00Z","tool_name":"read","action":"allow","reason":"read allowed","matched_rule":"allow.tools: read","mode":"enforce"}"#,
        r#"{"timestamp":"2026-05-28T08:04:00Z","tool_name":"write","action":"block","reason":"blocked env write","matched_rule":"deny.paths: .env","mode":"enforce"}"#,
        r#"{"timestamp":"2026-05-28T08:05:00Z","tool_name":"network","action":"warn","reason":"network audit","matched_rule":"warn.tools: network","mode":"audit"}"#,
        r#"{"timestamp":"2026-05-28T08:06:00Z","tool_name":"shell","action":"block","reason":"rm denied","matched_rule":"deny.commands: rm -rf","mode":"enforce"}"#,
        r#"{"timestamp":"2026-05-28T08:07:00Z","tool_name":"git","action":"allow","reason":"status allowed","matched_rule":"allow.commands: git status","mode":"audit"}"#,
        r#"{"timestamp":"2026-05-28T08:08:00Z","tool_name":"python","action":"warn","reason":null,"matched_rule":"warn.tools: python","mode":"audit"}"#,
        r#"{"timestamp":"2026-05-28T08:09:00Z","tool_name":"shell","action":"block","reason":"secret pattern","matched_rule":"deny.secrets: OPENAI_API_KEY","mode":"enforce"}"#,
    ]
    .join("\n")
}

#[test]
fn sentinel_jsonl_ingestion_normalizes_ten_events_to_candidates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = AntibodyStore::open(dir.path().join("mycel.sqlite")).expect("open store");
    let now = Utc.with_ymd_and_hms(2026, 5, 28, 9, 0, 0).unwrap();

    let candidates = store
        .ingest_sentinel_audit_jsonl(fixture_jsonl().as_bytes(), now)
        .expect("ingest sentinel jsonl");

    assert_eq!(candidates.len(), 10);
    assert_eq!(store.sentinel_event_count().expect("event count"), 10);

    let first = &candidates[0];
    assert_eq!(first.source.tool_name, "shell");
    assert_eq!(first.source.action, SentinelAction::Block);
    assert_eq!(
        first.source.timestamp,
        Utc.with_ymd_and_hms(2026, 5, 28, 8, 0, 0).unwrap()
    );
    assert_eq!(first.source.mode, "enforce");
    assert_eq!(
        first.metadata.reason.as_deref(),
        Some("blocked ssh key access")
    );
    assert_eq!(
        first.metadata.matched_rule.as_deref(),
        Some("deny.paths: ~/.ssh/*")
    );
    assert_eq!(
        first.antibody.signature.tool_pattern.as_deref(),
        Some("shell")
    );
    assert_eq!(first.antibody.signature.error_class, None);
    assert_eq!(
        first.antibody.signature.file_pattern.as_deref(),
        Some("~/.ssh/*")
    );
    assert_eq!(first.antibody.signature.agent_role, None);
    assert_eq!(first.antibody.signature.scope, SignatureScope::Project);
}

#[test]
fn sentinel_actions_map_to_severity_and_refusal_mode() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let store = AntibodyStore::open(dir.path().join("mycel.sqlite")).expect("open store");
    let now = Utc.with_ymd_and_hms(2026, 5, 28, 9, 0, 0).unwrap();

    let candidates = store
        .ingest_sentinel_audit_jsonl(fixture_jsonl().as_bytes(), now)
        .expect("ingest sentinel jsonl");

    let block = candidates
        .iter()
        .find(|candidate| candidate.source.action == SentinelAction::Block)
        .expect("block candidate");
    assert_eq!(block.antibody.severity, Severity::Refuse);
    assert_eq!(block.antibody.refusal_mode, RefusalMode::Hard);

    let warn = candidates
        .iter()
        .find(|candidate| candidate.source.action == SentinelAction::Warn)
        .expect("warn candidate");
    assert_eq!(warn.antibody.severity, Severity::Warn);
    assert_eq!(warn.antibody.refusal_mode, RefusalMode::Soft);

    let allow = candidates
        .iter()
        .find(|candidate| candidate.source.action == SentinelAction::Allow)
        .expect("allow candidate");
    assert_eq!(allow.antibody.severity, Severity::Info);
    assert_eq!(allow.antibody.refusal_mode, RefusalMode::LogOnly);
}

#[test]
fn matched_rule_metadata_is_queryable_from_an_indexed_column() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let store = AntibodyStore::open(dir.path().join("mycel.sqlite")).expect("open store");
    let now = Utc.with_ymd_and_hms(2026, 5, 28, 9, 0, 0).unwrap();

    store
        .ingest_sentinel_audit_jsonl(fixture_jsonl().as_bytes(), now)
        .expect("ingest sentinel jsonl");

    let events = store
        .sentinel_events_for_matched_rule("deny.paths: ~/.ssh/*")
        .expect("events for matched rule");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].tool_name, "shell");
    assert!(store
        .has_sqlite_index(
            "sentinel_audit_events",
            "idx_sentinel_audit_events_matched_rule"
        )
        .expect("index exists"));
}
