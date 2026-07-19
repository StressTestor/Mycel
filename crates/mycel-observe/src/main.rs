//! mycel-observe: PostToolUseFailure hook binary, the capture half of the antibody loop.
//!
//! Reads one PostToolUseFailure hook JSON object on stdin and appends one SentinelAuditEvent-shaped
//! JSONL line to the substrate audit log, so `mycel-substrate ingest` can later turn accumulated
//! harness failures into antibody candidates.
//!
//! This is passive, observation-only learning. PostToolUseFailure is not blockable, so mycel-observe
//! ALWAYS exits 0 and must never disrupt the harness: any write failure or malformed input becomes a
//! one-line stderr note and a clean exit. No input may cause a panic.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::Utc;
use serde_json::json;

fn main() -> ExitCode {
    // Fail-safe by construction: every path returns ExitCode::SUCCESS. A failure only ever
    // downgrades to a stderr note, never a nonzero exit that could stall the harness.
    if let Err(cause) = run() {
        eprintln!("mycel-observe: {cause}");
    }
    ExitCode::SUCCESS
}

fn run() -> Result<(), String> {
    let audit_path = resolve_audit_path().map_err(|e| e.to_string())?;

    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .map_err(|e| format!("failed to read stdin: {e}"))?;

    // Malformed stdin: write nothing, note it, exit 0.
    let payload: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("stdin is not valid JSON: {e}"))?;

    let tool_name = payload
        .get("tool_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let reason = extract_reason(&payload);

    let line = json!({
        "timestamp": Utc::now().to_rfc3339(),
        "tool_name": tool_name,
        "action": "block",
        "mode": "observe",
        "reason": reason,
        "matched_rule": serde_json::Value::Null,
    });

    append_line(&audit_path, &line.to_string())
        .map_err(|e| format!("cannot write audit log {}: {e}", audit_path.display()))
}

/// Pull the failure reason out of the payload's `error` field. It may be a plain string, an object
/// carrying a `message` or `text` field, or absent; anything unrecognized falls back to "tool failed".
fn extract_reason(payload: &serde_json::Value) -> String {
    match payload.get("error") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Object(obj)) => obj
            .get("message")
            .or_else(|| obj.get("text"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "tool failed".to_string()),
        _ => "tool failed".to_string(),
    }
}

/// Append one newline-terminated line, creating the parent dir and file if missing. UNLIKE the gate
/// (which must never create its db), the audit log is append-only observation data and is safe to
/// create.
fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

/// Resolve the audit log path: `--audit <path>` wins, then `$MYCEL_HOME/substrate/audit.jsonl`,
/// then `$HOME/.mycel/substrate/audit.jsonl`.
fn resolve_audit_path() -> Result<PathBuf, String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--audit" {
            return args
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| "--audit given without a path".to_string());
        }
        if let Some(path) = arg.strip_prefix("--audit=") {
            return Ok(PathBuf::from(path));
        }
    }

    if let Some(home) = std::env::var_os("MYCEL_HOME") {
        return Ok(Path::new(&home).join("substrate").join("audit.jsonl"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(Path::new(&home)
            .join(".mycel")
            .join("substrate")
            .join("audit.jsonl"));
    }

    Err("cannot resolve audit log path: set MYCEL_HOME or HOME, or pass --audit <path>".to_string())
}
