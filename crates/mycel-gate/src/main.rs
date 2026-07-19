//! mycel-gate: fail-closed PreToolUse hook binary.
//!
//! Reads one PreToolUse JSON object on stdin, evaluates the proposed run against the
//! antibody substrate, and emits a structured allow/warn/deny decision on stdout.
//!
//! This runs under `fail_mode = "closed"`: any nonzero exit BLOCKS the operation. Every
//! error path is therefore specific and actionable, and no user input may cause a panic.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::Utc;
use mycel_core::{AntibodyStore, EvaluationOutcome, ProposedRun, SignatureScope};
use serde::Deserialize;

// Exit codes. 0 = decision emitted on stdout (allow/warn/deny all succeed). Nonzero blocks.
const EXIT_DB: u8 = 3; // db missing or unopenable
const EXIT_INPUT: u8 = 4; // malformed stdin JSON
const EXIT_EVAL: u8 = 5; // evaluation/store error

#[derive(Debug, Deserialize)]
struct HookPayload {
    tool_name: Option<String>,
    tool_input: Option<serde_json::Value>,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("mycel-gate error: {}: {}", err.cause, err.hint);
            ExitCode::from(err.code)
        }
    }
}

/// A blocking error carrying a specific cause, an actionable fix hint, and an exit code.
struct GateError {
    cause: String,
    hint: String,
    code: u8,
}

fn run() -> Result<ExitCode, GateError> {
    let db_path = resolve_db_path()?;
    // SECURITY: never create the db. rusqlite's Connection::open creates-by-default, which would
    // turn `rm mycel.db` into a full gate disarm (a fresh empty store matches nothing -> allows
    // everything). Guard by requiring the file to already exist; a missing db is fail-closed.
    if !db_path.exists() {
        return Err(GateError {
            cause: format!("substrate db missing at {}", db_path.display()),
            hint: "run install.sh to initialize (a deleted db means the guard was disarmed - blocking)"
                .to_string(),
            code: EXIT_DB,
        });
    }

    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .map_err(|e| GateError {
            cause: format!("failed to read stdin: {e}"),
            hint: "pipe the PreToolUse JSON object into mycel-gate".to_string(),
            code: EXIT_INPUT,
        })?;

    let payload: HookPayload = serde_json::from_str(&raw).map_err(|e| GateError {
        cause: format!("stdin is not a valid PreToolUse JSON object: {e}"),
        hint: "expected {\"tool_name\":\"...\",\"tool_input\":{\"command\":\"...\"}}".to_string(),
        code: EXIT_INPUT,
    })?;

    let command = payload
        .tool_input
        .as_ref()
        .and_then(|v| v.get("command"))
        .and_then(|c| c.as_str())
        .map(str::to_string);

    let run = ProposedRun {
        error_class: None,
        file_path: None,
        agent_role: None,
        tool_name: payload.tool_name,
        command,
        scope: SignatureScope::Project,
    };

    let store = AntibodyStore::open(&db_path).map_err(|e| GateError {
        cause: format!("cannot open substrate db at {}: {e}", db_path.display()),
        hint: "the db may be corrupt or unreadable; check permissions or re-run install.sh"
            .to_string(),
        code: EXIT_DB,
    })?;

    let evaluation = store
        .evaluate_run(&run, Utc::now())
        .map_err(|e| GateError {
            cause: format!("evaluation failed: {e}"),
            hint: "the substrate db may be corrupt; re-run install.sh to rebuild it".to_string(),
            code: EXIT_EVAL,
        })?;

    match evaluation.outcome {
        EvaluationOutcome::Refuse => {
            // refusal() is guaranteed Some when outcome == Refuse.
            let matched = evaluation.refusal().ok_or_else(|| GateError {
                cause: "refuse outcome without a refusing match".to_string(),
                hint: "this is an internal invariant violation; file a bug".to_string(),
                code: EXIT_EVAL,
            })?;
            let reason = format!(
                "{} (source: {})",
                matched.remediation, matched.source_pointer
            );
            let out = serde_json::json!({
                "hookSpecificOutput": {
                    "permissionDecision": "deny",
                    "permissionDecisionReason": reason,
                }
            });
            println!("{out}");
        }
        EvaluationOutcome::Warn => {
            let matched = evaluation
                .matches
                .iter()
                .find(|m| m.outcome == EvaluationOutcome::Warn)
                .ok_or_else(|| GateError {
                    cause: "warn outcome without a warning match".to_string(),
                    hint: "this is an internal invariant violation; file a bug".to_string(),
                    code: EXIT_EVAL,
                })?;
            let message = format!(
                "mycel warn: {} (source: {})",
                matched.remediation, matched.source_pointer
            );
            let out = serde_json::json!({ "message": message });
            println!("{out}");
        }
        EvaluationOutcome::Allow => {
            println!("{{}}");
        }
    }

    Ok(ExitCode::SUCCESS)
}

/// Resolve the substrate db path: `--db <path>` wins, then `$MYCEL_HOME/substrate/mycel.db`,
/// then `$HOME/.mycel/substrate/mycel.db`.
fn resolve_db_path() -> Result<PathBuf, GateError> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--db" {
            let path = args.next().ok_or_else(|| GateError {
                cause: "--db given without a path".to_string(),
                hint: "pass a db path: mycel-gate --db /path/to/mycel.db".to_string(),
                code: EXIT_DB,
            })?;
            return Ok(PathBuf::from(path));
        }
        if let Some(path) = arg.strip_prefix("--db=") {
            return Ok(PathBuf::from(path));
        }
    }

    if let Some(home) = std::env::var_os("MYCEL_HOME") {
        return Ok(Path::new(&home).join("substrate").join("mycel.db"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(Path::new(&home)
            .join(".mycel")
            .join("substrate")
            .join("mycel.db"));
    }

    Err(GateError {
        cause: "cannot resolve substrate db path".to_string(),
        hint: "set MYCEL_HOME or HOME, or pass --db <path>".to_string(),
        code: EXIT_DB,
    })
}
