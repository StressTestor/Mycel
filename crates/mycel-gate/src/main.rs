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
use mycel_core::{
    protected_floor_check, AntibodyStore, EvaluationOutcome, PathContext, ProposedRun,
    SignatureScope,
};
use serde::Deserialize;

// Exit codes. 0 = decision emitted on stdout (allow/warn/deny all succeed). Nonzero blocks.
const EXIT_DB: u8 = 3; // db missing or unopenable
const EXIT_INPUT: u8 = 4; // malformed stdin JSON
const EXIT_EVAL: u8 = 5; // evaluation/store error

// Source pointer stamped on a structural DENY of a write-class tool whose target
// path we cannot resolve. Distinct from the protected-path floor pointer.
const UNEXTRACTABLE_SOURCE: &str = "mycel-gate:unextractable-mutator";

// Core file-mutating tools. A call to one of these with no resolvable string
// target is a structural DENY (fail-closed): we cannot prove it is safe.
const CORE_WRITE_TOOLS: &[&str] = &["Write", "Edit", "MultiEdit", "NotebookEdit"];

// tool_input keys that may carry a write target, tried in this order. `path` is
// the real agent-core-v2 Write/Edit field; the others are fallbacks.
const PATH_KEYS: &[&str] = &["path", "file_path", "notebook_path"];

#[derive(Debug, Deserialize)]
struct HookPayload {
    tool_name: Option<String>,
    tool_input: Option<serde_json::Value>,
    // Top-level snake_cased cwd from the hook payload: the AGENT's cwd, used to
    // resolve a relative write target. Never the gate process cwd.
    cwd: Option<String>,
}

/// Outcome of trying to pull a write target out of the payload.
enum PathExtraction {
    /// Not a write-class tool: no floor check, `file_path` stays None.
    NotWrite,
    /// A concrete string target to floor-check and pass to evaluation.
    Extracted(String),
    /// A write-class tool we cannot verify (absent tool_input, non-string path,
    /// or a core write tool with no target key): structural DENY.
    Unextractable,
}

fn main() -> ExitCode {
    // `--claude` emits the Claude Code hook dialect (exit 2 + stderr reason to
    // block) instead of the native kimi/mycel dialect (exit 0 + permissionDecision
    // JSON). Lets one gate govern a `claude -p` subagent as well as Mycel itself.
    let claude = std::env::args().any(|a| a == "--claude");
    match run(claude) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("mycel-gate error: {}: {}", err.cause, err.hint);
            // Under --claude, Claude Code only treats exit 2 as a block; any other
            // nonzero is a non-blocking error that lets the tool proceed. Keep the
            // fail-closed guarantee by blocking (exit 2) on every error.
            if claude {
                ExitCode::from(2)
            } else {
                ExitCode::from(err.code)
            }
        }
    }
}

/// A blocking error carrying a specific cause, an actionable fix hint, and an exit code.
struct GateError {
    cause: String,
    hint: String,
    code: u8,
}

fn run(claude: bool) -> Result<ExitCode, GateError> {
    let db_path = resolve_db_path()?;
    let mycel_home = resolve_mycel_home();

    // Read + parse stdin FIRST. With the catch-all matcher the gate governs every
    // tool, and the protected-path floor needs the tool name, the target path,
    // and the payload cwd before it can decide anything.
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

    let ctx = PathContext {
        home: std::env::var_os("HOME").map(PathBuf::from),
        cwd: payload.cwd.as_deref().map(PathBuf::from),
    };

    // Structural + protected-path floor checks run BEFORE the db is opened, so a
    // write that targets the gate's own binary/config/substrate is blocked even
    // when the db is missing or corrupt.
    let mut file_path: Option<String> = None;
    match extract_write_path(payload.tool_name.as_deref(), payload.tool_input.as_ref()) {
        PathExtraction::NotWrite => {}
        PathExtraction::Unextractable => {
            return Ok(emit_block(
                claude,
                "refusing a write-class tool call with no resolvable target path (fail-closed)",
                UNEXTRACTABLE_SOURCE,
            ));
        }
        PathExtraction::Extracted(path) => {
            if let Some(refusal) = protected_floor_check(&path, &ctx, &mycel_home) {
                return Ok(emit_block(
                    claude,
                    &refusal.remediation,
                    &refusal.source_pointer,
                ));
            }
            file_path = Some(path);
        }
    }

    // SECURITY: never create the db. rusqlite's Connection::open creates-by-default, which would
    // turn `rm mycel.db` into a full gate disarm (a fresh empty store matches nothing -> allows
    // everything). Guard by requiring the file to already exist; a missing db is fail-closed.
    // This block MUST stay on the non-protected fall-through: a missing/corrupt db now
    // fail-closed BLOCKS every tool routed here (Read/Grep included) - the whole-toolset
    // coupling is deliberate, and it is what keeps `rm mycel.db` from disarming plain writes.
    if !db_path.exists() {
        return Err(GateError {
            cause: format!("substrate db missing at {}", db_path.display()),
            hint: "run install.sh to initialize (a deleted db means the guard was disarmed - blocking)"
                .to_string(),
            code: EXIT_DB,
        });
    }

    let command = payload
        .tool_input
        .as_ref()
        .and_then(|v| v.get("command"))
        .and_then(|c| c.as_str())
        .map(str::to_string);

    let run = ProposedRun {
        error_class: None,
        file_path,
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
            return Ok(emit_block(
                claude,
                &matched.remediation,
                &matched.source_pointer,
            ));
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

/// Emit a block in the active dialect. Native: exit 0 with a `permissionDecision:
/// deny` JSON on stdout (the same shape a stored refusal uses). Under `--claude`:
/// exit 2 with the reason on stderr (Claude Code only blocks on exit 2).
fn emit_block(claude: bool, remediation: &str, source_pointer: &str) -> ExitCode {
    let reason = format!("{remediation} (source: {source_pointer})");
    if claude {
        eprintln!("{reason}");
        return ExitCode::from(2);
    }
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    });
    println!("{out}");
    ExitCode::SUCCESS
}

/// Pull a write target out of the payload for the floor check. Only WRITE-class
/// tools are considered; Read/Grep/Glob return `NotWrite` so a `file_pattern`-only
/// antibody never over-blocks a read.
///
/// A core file tool (Write/Edit/MultiEdit/NotebookEdit) with absent tool_input, a
/// non-string path, or no target key at all is `Unextractable` -> structural
/// DENY. An MCP write tool is best-effort: a string path is checked, a non-string
/// path denies, but no recognizable path key falls through to normal evaluation.
///
/// RESIDUAL: an MCP write that carries its target in a non-`path` field (e.g.
/// Supabase apply_migration / execute_sql `query`) is NOT covered here.
fn extract_write_path(
    tool_name: Option<&str>,
    tool_input: Option<&serde_json::Value>,
) -> PathExtraction {
    let Some(name) = tool_name else {
        return PathExtraction::NotWrite;
    };
    let core = CORE_WRITE_TOOLS.contains(&name);
    let mcp = is_mcp_write_tool(name);
    if !core && !mcp {
        return PathExtraction::NotWrite;
    }

    let Some(input) = tool_input else {
        // a write-class tool with no input block: core denies, mcp cannot be
        // classified so it falls through unguarded (documented residual).
        return if core {
            PathExtraction::Unextractable
        } else {
            PathExtraction::NotWrite
        };
    };

    for key in PATH_KEYS {
        if let Some(value) = input.get(*key) {
            return match value.as_str() {
                Some(s) => PathExtraction::Extracted(s.to_string()),
                // key present but not a string (array/number/null/object): a
                // mutator whose target we cannot resolve -> fail closed.
                None => PathExtraction::Unextractable,
            };
        }
    }

    // No recognizable path key. A core write tool must have one -> deny; an MCP
    // write tool may legitimately carry the target elsewhere -> residual.
    if core {
        PathExtraction::Unextractable
    } else {
        PathExtraction::NotWrite
    }
}

/// Best-effort classifier for MCP write tools. MCP tools are namespaced
/// `mcp__<server>__<tool>`; treat those whose name mentions a mutation verb as
/// write-class so a `path`-bearing MCP write is floor-checked.
fn is_mcp_write_tool(name: &str) -> bool {
    if !name.starts_with("mcp__") {
        return false;
    }
    const VERBS: &[&str] = &[
        "write", "edit", "create", "update", "insert", "upsert", "apply", "patch", "put", "delete",
        "save", "append",
    ];
    let lower = name.to_ascii_lowercase();
    VERBS.iter().any(|verb| lower.contains(verb))
}

/// Resolve the mycel home whose bin/config/substrate the floor protects:
/// `--mycel-home <path>` wins, then `$MYCEL_HOME`, then the legacy
/// `$KIMI_CODE_HOME`, then `$HOME/.mycel`.
fn resolve_mycel_home() -> PathBuf {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--mycel-home" {
            if let Some(path) = args.next() {
                return PathBuf::from(path);
            }
        } else if let Some(path) = arg.strip_prefix("--mycel-home=") {
            return PathBuf::from(path);
        }
    }
    if let Some(home) = std::env::var_os("MYCEL_HOME") {
        return PathBuf::from(home);
    }
    if let Some(home) = std::env::var_os("KIMI_CODE_HOME") {
        return PathBuf::from(home);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Path::new(&home).join(".mycel");
    }
    PathBuf::from(".mycel")
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
