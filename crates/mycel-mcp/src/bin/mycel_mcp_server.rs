//! `mycel-mcp-server`: a stdio MCP server exposing the mycel substrate to an
//! agent harness.
//!
//! It speaks newline-delimited JSON-RPC 2.0 over stdin/stdout and implements the
//! MCP subset a harness needs: `initialize`, `notifications/initialized`,
//! `tools/list`, and `tools/call`. Three tools are exposed:
//!
//! * `evaluate_run`  - score a proposed tool/command against the antibody store.
//! * `list_antibodies` - dump the full antibody store.
//! * `propose_antibody` - append an inert proposal to `proposals.jsonl`. This
//!   NEVER mutates the live store; a human or the CLI promotes proposals later.
//!
//! The antibody database must already exist (created by `install.sh`). The
//! server still starts and answers `initialize`/`tools/list` when the db is
//! missing so the harness sees actionable per-tool errors instead of a dead
//! server; `evaluate_run`/`list_antibodies` then return an error naming the
//! path, while `propose_antibody` keeps working since it only needs the parent
//! directory.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use chrono::Utc;
use mycel_core::{ProposedRun, SignatureScope};
use mycel_mcp::McpTools;
use serde_json::{json, Value};

const PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "mycel-mcp-server";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let db_path = resolve_db_path(std::env::args().skip(1));
    let server = ServerState { db_path };

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => {
                write_message(&mut out, &parse_error());
                continue;
            }
        };
        if let Some(response) = server.handle(&request) {
            write_message(&mut out, &response);
        }
    }
}

struct ServerState {
    db_path: Option<PathBuf>,
}

impl ServerState {
    /// Dispatch a single JSON-RPC message. Returns `None` for notifications.
    fn handle(&self, request: &Value) -> Option<Value> {
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let id = request.get("id").cloned();

        // No id => notification: act (if relevant) but never respond.
        let id = id?;

        match method {
            "initialize" => Some(ok(id, self.initialize())),
            "ping" => Some(ok(id, json!({}))),
            "tools/list" => Some(ok(id, self.tools_list())),
            "tools/call" => Some(self.tools_call(id, request.get("params"))),
            other => Some(error(id, -32601, &format!("method not found: {other}"))),
        }
    }

    fn initialize(&self) -> Value {
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
        })
    }

    fn tools_list(&self) -> Value {
        json!({ "tools": tool_schemas() })
    }

    fn tools_call(&self, id: Value, params: Option<&Value>) -> Value {
        let params = match params {
            Some(p) => p,
            None => return error(id, -32602, "missing params"),
        };
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let empty = json!({});
        let args = params.get("arguments").unwrap_or(&empty);

        let outcome = match name {
            "evaluate_run" => self.evaluate_run(args),
            "list_antibodies" => self.list_antibodies(),
            "propose_antibody" => self.propose_antibody(args),
            other => return error(id, -32602, &format!("unknown tool: {other}")),
        };

        match outcome {
            Ok(text) => ok(id, tool_text_result(&text, false)),
            Err(text) => ok(id, tool_text_result(&text, true)),
        }
    }

    /// Path to the antibody db, or an error string naming the missing path.
    fn require_existing_db(&self) -> std::result::Result<&Path, String> {
        match &self.db_path {
            Some(path) if path.exists() => Ok(path.as_path()),
            Some(path) => Err(missing_db_message(path)),
            None => Err(
                "no antibody database configured; pass --db <path> or set MYCEL_HOME (run install.sh to initialize)"
                    .to_string(),
            ),
        }
    }

    fn evaluate_run(&self, args: &Value) -> std::result::Result<String, String> {
        let db = self.require_existing_db()?;
        let tool_name = optional_string(args, "tool_name");
        let command = optional_string(args, "command");
        let run = ProposedRun {
            error_class: None,
            file_path: None,
            agent_role: None,
            tool_name,
            command,
            scope: SignatureScope::Project,
        };
        let tools =
            McpTools::open(db).map_err(|e| format!("failed to open db {}: {e}", db.display()))?;
        let evaluation = tools
            .evaluate(&run, Utc::now())
            .map_err(|e| format!("evaluation failed: {e}"))?;

        let matches: Vec<Value> = evaluation
            .matches
            .iter()
            .map(|m| {
                json!({
                    "antibody_id": m.antibody_id.to_string(),
                    "outcome": m.outcome,
                    "severity": m.severity,
                    "remediation": m.remediation,
                    "source_pointer": m.source_pointer,
                })
            })
            .collect();
        let payload = json!({
            "outcome": evaluation.outcome,
            "matches": matches,
        });
        Ok(serde_json::to_string(&payload).expect("serialize evaluation"))
    }

    fn list_antibodies(&self) -> std::result::Result<String, String> {
        let db = self.require_existing_db()?;
        let tools =
            McpTools::open(db).map_err(|e| format!("failed to open db {}: {e}", db.display()))?;
        let antibodies = tools
            .list_antibodies()
            .map_err(|e| format!("failed to list antibodies: {e}"))?;
        Ok(serde_json::to_string(&antibodies).expect("serialize antibodies"))
    }

    fn propose_antibody(&self, args: &Value) -> std::result::Result<String, String> {
        // Proposals are inert: they land in proposals.jsonl next to the db and
        // are never written to the live antibody store.
        let dir = self
            .db_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
            .ok_or_else(|| "cannot determine substrate directory for proposals".to_string())?;
        let path = dir.join("proposals.jsonl");

        let signature = args.get("signature").cloned().unwrap_or_else(|| json!({}));
        let remediation = args.get("remediation").cloned().unwrap_or(Value::Null);
        let rationale = args.get("rationale").cloned().unwrap_or(Value::Null);

        let proposed_id = uuid::Uuid::new_v4().to_string();
        let record = json!({
            "id": proposed_id,
            "created_at": Utc::now().to_rfc3339(),
            "signature": signature,
            "remediation": remediation,
            "rationale": rationale,
        });

        let mut line = serde_json::to_string(&record).expect("serialize proposal");
        line.push('\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("failed to open proposals file {}: {e}", path.display()))?;
        file.write_all(line.as_bytes())
            .map_err(|e| format!("failed to write proposal to {}: {e}", path.display()))?;

        let response = json!({ "proposed_id": proposed_id, "path": path.to_string_lossy() });
        Ok(serde_json::to_string(&response).expect("serialize proposal response"))
    }
}

/// Resolve the db path from `--db <path>`, else `$MYCEL_HOME/substrate/mycel.db`,
/// else `$HOME/.mycel/substrate/mycel.db`.
fn resolve_db_path(args: impl Iterator<Item = String>) -> Option<PathBuf> {
    let mut args = args;
    while let Some(arg) = args.next() {
        if arg == "--db" {
            return args.next().map(PathBuf::from);
        }
        if let Some(rest) = arg.strip_prefix("--db=") {
            return Some(PathBuf::from(rest));
        }
    }
    if let Some(home) = std::env::var_os("MYCEL_HOME") {
        return Some(PathBuf::from(home).join("substrate").join("mycel.db"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Some(
            PathBuf::from(home)
                .join(".mycel")
                .join("substrate")
                .join("mycel.db"),
        );
    }
    None
}

fn missing_db_message(path: &Path) -> String {
    format!(
        "antibody database not found at {}; run install.sh to initialize",
        path.display()
    )
}

fn optional_string(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn tool_schemas() -> Value {
    json!([
        {
            "name": "evaluate_run",
            "description": "Evaluate a proposed tool invocation or command against the project antibody store. Returns an outcome (refuse/warn/allow) and the matching antibodies.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tool_name": { "type": "string", "description": "Name of the tool about to run." },
                    "command": { "type": "string", "description": "The command line about to be executed." }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "list_antibodies",
            "description": "List every antibody in the project store as JSON.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "propose_antibody",
            "description": "Append an inert antibody proposal to proposals.jsonl. Proposals never mutate the live store; a human or the CLI promotes them later.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "signature": {
                        "type": "object",
                        "properties": {
                            "command_pattern": { "type": "string" },
                            "error_class": { "type": "string" },
                            "file_pattern": { "type": "string" },
                            "tool_name": { "type": "string" }
                        },
                        "additionalProperties": false
                    },
                    "remediation": { "type": "string" },
                    "rationale": { "type": "string" }
                },
                "required": ["signature", "remediation", "rationale"],
                "additionalProperties": false
            }
        }
    ])
}

fn tool_text_result(text: &str, is_error: bool) -> Value {
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": is_error,
    })
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn parse_error() -> Value {
    json!({ "jsonrpc": "2.0", "id": Value::Null, "error": { "code": -32700, "message": "parse error" } })
}

fn write_message(out: &mut impl Write, message: &Value) {
    let line = serde_json::to_string(message).expect("serialize response");
    let _ = out.write_all(line.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}
