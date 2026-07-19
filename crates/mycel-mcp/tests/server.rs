//! Integration tests for the mycel-mcp-server stdio binary.
//!
//! Each test spawns the built binary and speaks newline-delimited JSON-RPC 2.0
//! over its stdin/stdout, exercising the MCP subset the server implements.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use chrono::Utc;
use mycel_core::{
    Antibody, AntibodySource, AntibodyStore, Confidence, RefusalMode, Severity, Signature,
    SignatureScope,
};
use serde_json::{json, Value};

/// A live server process wired for line-delimited JSON-RPC.
struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Server {
    fn spawn(db: Option<&Path>) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mycel-mcp-server"));
        if let Some(db) = db {
            cmd.arg("--db").arg(db);
        }
        // Isolate from any real environment so path resolution is deterministic.
        cmd.env_remove("MYCEL_HOME");
        cmd.env("HOME", "/nonexistent-mycel-home");
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn mycel-mcp-server");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
    }

    /// Send a request and return its response object.
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.send(&req);
        loop {
            let msg = self.read_message();
            // Skip anything that is not the matching response (e.g. notifications).
            if msg.get("id").and_then(Value::as_i64) == Some(id) {
                return msg;
            }
        }
    }

    /// Send a notification (no id, no response expected).
    fn notify(&mut self, method: &str, params: Value) {
        let req = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.send(&req);
    }

    fn send(&mut self, value: &Value) {
        let line = serde_json::to_string(value).expect("serialize request");
        self.stdin.write_all(line.as_bytes()).expect("write");
        self.stdin.write_all(b"\n").expect("write newline");
        self.stdin.flush().expect("flush");
    }

    fn read_message(&mut self) -> Value {
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line).expect("read line");
        assert!(n > 0, "server closed stdout unexpectedly");
        serde_json::from_str(line.trim()).unwrap_or_else(|e| panic!("bad json {line:?}: {e}"))
    }

    /// Run the initialize + initialized handshake.
    fn initialize(&mut self) -> Value {
        let resp = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" },
            }),
        );
        self.notify("notifications/initialized", json!({}));
        resp
    }

    /// Call a tool and return the `result` object.
    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let resp = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        resp
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Extract the concatenated text content from a tools/call result.
fn result_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .expect("content array")
        .iter()
        .filter_map(|c| c.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

fn hard_refuse_antibody(command_pattern: &str) -> Antibody {
    Antibody {
        id: uuid::Uuid::new_v4(),
        signature: Signature {
            error_class: None,
            file_pattern: None,
            agent_role: None,
            tool_pattern: None,
            command_pattern: Some(command_pattern.to_string()),
            scope: SignatureScope::Project,
        },
        source: AntibodySource::Manual,
        severity: Severity::Refuse,
        confidence: Confidence::Solid,
        refusal_mode: RefusalMode::Hard,
        remediation: "do not run destructive recursive deletes".to_string(),
        examples: vec!["rm -rf / wiped the workspace".to_string()],
        created_at: Utc::now(),
        expires_at: None,
        hit_count: 0,
    }
}

/// Seed a fresh db file with a single hard-refuse antibody and return its path.
fn seeded_db(dir: &Path) -> PathBuf {
    let path = dir.join("mycel.db");
    let store = AntibodyStore::open(&path).expect("open store");
    store
        .insert_antibody(&hard_refuse_antibody("rm -rf"))
        .expect("insert antibody");
    // Drop closes the connection so the server can open the same file.
    drop(store);
    path
}

#[test]
fn initialize_handshake_succeeds() {
    let mut server = Server::spawn(None);
    let resp = server.initialize();
    let result = &resp["result"];
    assert_eq!(result["protocolVersion"], "2025-06-18");
    assert!(
        result["capabilities"].get("tools").is_some(),
        "expected tools capability, got {result}"
    );
    assert!(result["serverInfo"]["name"].is_string());
}

#[test]
fn tools_list_returns_three_tools_with_schemas() {
    let mut server = Server::spawn(None);
    server.initialize();
    let resp = server.request("tools/list", json!({}));
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 3, "expected 3 tools, got {tools:?}");
    let mut names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().expect("tool name"))
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["evaluate_run", "list_antibodies", "propose_antibody"]
    );
    for tool in tools {
        assert!(
            tool["inputSchema"]["type"] == "object",
            "tool {} missing object inputSchema",
            tool["name"]
        );
    }
}

#[test]
fn evaluate_run_refuses_matching_command() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = seeded_db(dir.path());

    let mut server = Server::spawn(Some(&db));
    server.initialize();
    let result = &server.call_tool("evaluate_run", json!({ "command": "rm -rf /" }))["result"];
    assert_ne!(result["isError"], json!(true), "unexpected error: {result}");
    let payload: Value = serde_json::from_str(&result_text(result)).expect("json payload");
    assert_eq!(payload["outcome"], "refuse", "payload was {payload}");
    let matches = payload["matches"].as_array().expect("matches array");
    assert_eq!(matches.len(), 1);
    let m = &matches[0];
    assert_eq!(m["outcome"], "refuse");
    assert_eq!(m["severity"], "refuse");
    assert!(m["antibody_id"].is_string());
    assert!(m["remediation"].is_string());
    assert!(m["source_pointer"].is_string());
}

#[test]
fn list_antibodies_returns_seeded_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = seeded_db(dir.path());

    let mut server = Server::spawn(Some(&db));
    server.initialize();
    let result = &server.call_tool("list_antibodies", json!({}))["result"];
    assert_ne!(result["isError"], json!(true), "unexpected error: {result}");
    let payload: Value = serde_json::from_str(&result_text(result)).expect("json payload");
    let list = payload.as_array().expect("antibody array");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["severity"], "refuse");
    assert_eq!(list[0]["refusal_mode"], "hard");
}

#[test]
fn propose_antibody_writes_one_jsonl_line_without_touching_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = seeded_db(dir.path());
    let proposals = dir.path().join("proposals.jsonl");

    let mut server = Server::spawn(Some(&db));
    server.initialize();

    let args = json!({
        "signature": { "command_pattern": "curl | sh" },
        "remediation": "pipe installers to a file and review first",
        "rationale": "curl-pipe-sh executes unreviewed remote code",
    });
    let result = &server.call_tool("propose_antibody", args)["result"];
    assert_ne!(result["isError"], json!(true), "unexpected error: {result}");
    let payload: Value = serde_json::from_str(&result_text(result)).expect("json payload");
    assert!(payload["proposed_id"].is_string());
    assert_eq!(payload["path"], proposals.to_string_lossy().as_ref());

    // Exactly one well-formed JSONL line.
    let contents = std::fs::read_to_string(&proposals).expect("read proposals");
    let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "expected one proposal line, got {lines:?}");
    let record: Value = serde_json::from_str(lines[0]).expect("proposal json");
    assert!(record["id"].is_string());
    assert!(record["created_at"].is_string());
    assert_eq!(
        record["remediation"],
        "pipe installers to a file and review first"
    );
    assert_eq!(
        record["rationale"],
        "curl-pipe-sh executes unreviewed remote code"
    );
    assert_eq!(record["signature"]["command_pattern"], "curl | sh");

    // The live antibody store is untouched: still exactly the one seeded entry.
    let store = AntibodyStore::open(&db).expect("reopen store");
    assert_eq!(store.list_antibodies().expect("list").len(), 1);
}

#[test]
fn missing_db_evaluate_errors_and_does_not_create_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("substrate").join("mycel.db");
    std::fs::create_dir_all(db.parent().unwrap()).expect("create parent dir");
    assert!(!db.exists());

    let mut server = Server::spawn(Some(&db));
    server.initialize();
    let result = &server.call_tool("evaluate_run", json!({ "command": "rm -rf /" }))["result"];
    assert_eq!(
        result["isError"],
        json!(true),
        "expected tool error: {result}"
    );
    let text = result_text(result);
    assert!(
        text.contains(&db.to_string_lossy().to_string()),
        "error should name the db path, got {text:?}"
    );
    assert!(
        text.contains("install.sh"),
        "error should mention install.sh, got {text:?}"
    );
    // The server must not have created the db file.
    assert!(!db.exists(), "server created the db file at {db:?}");
}
