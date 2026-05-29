use chrono::{TimeZone, Utc};
use mycel_core::{ProposedRun, SignatureScope};
use mycel_mcp::McpTools;

#[test]
fn mcp_tools_expose_ingest_evaluate_and_list_antibodies() {
    let tools = McpTools::open_in_memory().expect("open tools");
    let now = Utc.with_ymd_and_hms(2026, 5, 28, 9, 0, 0).unwrap();
    let jsonl = r#"{"timestamp":"2026-05-28T08:00:00Z","tool_name":"shell","action":"block","reason":"blocked ssh","matched_rule":"deny.paths: ~/.ssh/*","mode":"enforce"}"#;

    let candidates = tools
        .ingest_sentinel(jsonl.as_bytes(), now)
        .expect("ingest sentinel");
    tools
        .insert_antibodies(
            candidates
                .iter()
                .map(|candidate| candidate.antibody.clone()),
        )
        .expect("insert antibodies");

    let listed = tools.list_antibodies().expect("list antibodies");
    assert_eq!(listed.len(), 1);

    let evaluation = tools
        .evaluate(
            &ProposedRun {
                error_class: None,
                file_path: None,
                agent_role: None,
                tool_name: Some("shell".to_string()),
                scope: SignatureScope::Project,
            },
            now,
        )
        .expect("evaluate run");

    // A Sentinel block maps to a refuse candidate (locked), but Sentinel-derived
    // antibodies populate only `tool_pattern`, so the single-field signature is
    // demoted to a soft warn when persisted under the v0.1.1 specificity rule.
    assert_eq!(format!("{:?}", evaluation.outcome), "Warn");
}
