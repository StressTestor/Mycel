use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use mycel_agent_protocol::{
    ExecutableToolOutput, ExecutableToolResult, ToolUpdate, ToolUpdateKind,
};
use mycel_agent_runtime::{
    register_local_builtins, AgentId, BashTool, CancellationToken, EditTool, ExecutableTool,
    GlobTool, GrepTool, LocalToolConfig, ReadTool, SessionId, ToolCallId, ToolInvocation,
    ToolPrepareContext, ToolRegistry, ToolUpdateSink, WriteTool,
};
use serde_json::json;

struct TempWorkspace(PathBuf);

impl TempWorkspace {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "mycel-runtime-{label}-{}",
            mycel_agent_runtime::RequestId::generate()
        ));
        fs::create_dir(&path).expect("create temporary workspace");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn prepare_context() -> ToolPrepareContext {
    ToolPrepareContext {
        session_id: SessionId::new("session-test").expect("session id"),
        agent_id: AgentId::main(),
        turn_id: 1,
        tool_call_id: ToolCallId::new("call-test").expect("call id"),
    }
}

#[derive(Default)]
struct RecordingUpdates(Mutex<Vec<ToolUpdate>>);

impl ToolUpdateSink for RecordingUpdates {
    fn emit(&self, update: ToolUpdate) {
        self.0.lock().expect("updates lock").push(update);
    }
}

async fn invoke(
    tool: &dyn ExecutableTool,
    arguments: serde_json::Value,
    cancellation: CancellationToken,
    updates: Arc<RecordingUpdates>,
) -> ExecutableToolResult {
    tool.validate_arguments(&arguments)
        .expect("valid arguments");
    tool.prepare(&arguments, &prepare_context())
        .expect("prepare tool");
    tool.execute(ToolInvocation {
        context: prepare_context(),
        arguments,
        cancellation,
        updates,
    })
    .await
    .expect("execute tool")
}

fn output_text(result: &ExecutableToolResult) -> &str {
    match &result.output {
        ExecutableToolOutput::Text(text) => text,
        ExecutableToolOutput::Parts(_) => panic!("expected text output"),
    }
}

#[test]
fn registry_exposes_six_strict_local_tool_schemas() {
    let workspace = TempWorkspace::new("schemas");
    let config = LocalToolConfig::new(workspace.path(), Vec::<PathBuf>::new()).expect("config");
    let registry = ToolRegistry::new();
    register_local_builtins(&registry, config).expect("register");

    let snapshot = registry.snapshot();
    assert_eq!(
        snapshot
            .definitions()
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>(),
        ["Bash", "Edit", "Glob", "Grep", "Read", "Write"]
    );
    for definition in snapshot.definitions() {
        assert_eq!(definition.parameters["additionalProperties"], false);
    }

    let read = snapshot.get("Read").expect("Read");
    assert!(read.validate_arguments(&json!({"path":"a.txt"})).is_ok());
    assert!(read
        .validate_arguments(&json!({"path":"a.txt","line_offset":0}))
        .is_err());
    assert!(read
        .validate_arguments(&json!({"path":"a.txt","unknown":true}))
        .is_err());

    let bash = snapshot.get("Bash").expect("Bash");
    assert!(bash
        .validate_arguments(&json!({"command":"printf ok","timeout":300}))
        .is_ok());
    assert!(bash
        .validate_arguments(&json!({"command":"printf ok","timeout":301}))
        .is_err());
    assert!(bash.validate_arguments(&json!({"command":""})).is_err());
}

#[cfg(unix)]
#[test]
fn prepare_confines_paths_to_real_workspace_roots_and_rejects_symlink_escapes() {
    use std::os::unix::fs::symlink;

    let workspace = TempWorkspace::new("path-root");
    let added = TempWorkspace::new("path-added");
    let outside = TempWorkspace::new("path-outside");
    fs::write(workspace.path().join("inside.txt"), "inside").expect("inside");
    fs::write(added.path().join("added.txt"), "added").expect("added");
    fs::write(outside.path().join("secret.txt"), "secret").expect("secret");
    symlink(outside.path(), workspace.path().join("escape")).expect("symlink");
    fs::write(workspace.path().join(".env"), "SECRET=value").expect("sensitive");

    let config = LocalToolConfig::new(workspace.path(), [added.path()]).expect("config");
    let read = ReadTool::new(config.clone());
    let context = prepare_context();
    assert!(read
        .prepare(&json!({"path":"inside.txt"}), &context)
        .is_ok());
    let added_result = read.prepare(
        &json!({"path":added.path().join("added.txt").to_string_lossy()}),
        &context,
    );
    assert!(added_result.is_ok(), "{added_result:?}");
    assert!(read
        .prepare(&json!({"path":"../path-outside/secret.txt"}), &context)
        .is_err());
    assert!(read
        .prepare(
            &json!({"path":outside.path().join("secret.txt").to_string_lossy()}),
            &context,
        )
        .is_err());
    assert!(read
        .prepare(&json!({"path":"escape/secret.txt"}), &context)
        .is_err());
    assert!(read.prepare(&json!({"path":".env"}), &context).is_err());
    // A harmlessly named symlink pointing at a sensitive file must be rejected
    // too: the name check runs on the input, but the bytes come from the
    // canonical target, so the target has to pass the same policy.
    symlink(
        workspace.path().join(".env"),
        workspace.path().join("config-link"),
    )
    .expect("sensitive symlink");
    assert!(
        read.prepare(&json!({"path":"config-link"}), &context)
            .is_err(),
        "symlink to .env bypassed the sensitive-path policy"
    );
    assert!(
        GrepTool::new(config.clone())
            .prepare(&json!({"pattern":"SECRET","path":"config-link"}), &context)
            .is_err(),
        "grep of a symlink to .env bypassed the sensitive-path policy"
    );
    assert!(WriteTool::new(config.clone())
        .prepare(&json!({"path":"escape/new.txt","content":"no"}), &context)
        .is_err());
    assert!(BashTool::new(config)
        .prepare(&json!({"command":"pwd","cwd":"escape"}), &context)
        .is_err());
}

#[test]
fn exact_file_grant_allows_only_write_and_edit_of_the_selected_file() {
    let workspace = TempWorkspace::new("exact-workspace");
    let plans = TempWorkspace::new("exact-plans");
    let canonical_plans = fs::canonicalize(plans.path()).expect("canonical plans");
    let selected = canonical_plans.join("selected.md");
    let sibling = canonical_plans.join("sibling.md");
    fs::write(&sibling, "# private sibling").expect("sibling plan");
    let config = LocalToolConfig::new(workspace.path(), Vec::<PathBuf>::new())
        .expect("config")
        .with_allowed_files([&selected])
        .expect("exact file grant");
    let context = prepare_context();

    assert!(WriteTool::new(config.clone())
        .prepare(
            &json!({"path":selected.to_string_lossy(),"content":"# selected"}),
            &context,
        )
        .is_ok());
    assert!(WriteTool::new(config.clone())
        .prepare(
            &json!({"path":sibling.to_string_lossy(),"content":"overwrite"}),
            &context,
        )
        .is_err());
    fs::write(&selected, "# selected").expect("selected plan");
    assert!(EditTool::new(config.clone())
        .prepare(
            &json!({
                "path":selected.to_string_lossy(),
                "old_string":"selected",
                "new_string":"approved"
            }),
            &context,
        )
        .is_ok());
    assert!(ReadTool::new(config)
        .prepare(&json!({"path":selected.to_string_lossy()}), &context)
        .is_err());
}

#[cfg(unix)]
#[test]
fn exact_file_grant_rejects_symlink_components_and_targets() {
    use std::os::unix::fs::symlink;

    let workspace = TempWorkspace::new("exact-symlink-workspace");
    let plans = TempWorkspace::new("exact-symlink-plans");
    let canonical_workspace = fs::canonicalize(workspace.path()).expect("canonical workspace");
    let canonical_plans = fs::canonicalize(plans.path()).expect("canonical plans");
    let linked_parent = canonical_workspace.join("linked-plans");
    symlink(plans.path(), &linked_parent).expect("parent symlink");
    assert!(
        LocalToolConfig::new(workspace.path(), Vec::<PathBuf>::new())
            .expect("config")
            .with_allowed_files([linked_parent.join("plan.md")])
            .expect_err("symlink parent rejected")
            .to_string()
            .contains("symlink")
    );

    let real = canonical_plans.join("real.md");
    fs::write(&real, "plan").expect("real plan");
    let linked_file = canonical_plans.join("linked.md");
    symlink(&real, &linked_file).expect("file symlink");
    assert!(
        LocalToolConfig::new(workspace.path(), Vec::<PathBuf>::new())
            .expect("config")
            .with_allowed_files([linked_file])
            .expect_err("symlink target rejected")
            .to_string()
            .contains("symlink")
    );
}

#[test]
fn sensitive_path_policy_blocks_credentials_without_blanket_key_file_false_positives() {
    let workspace = TempWorkspace::new("sensitive");
    for allowed in [
        ".env.example",
        "id_rsa.pub",
        "credentials.json",
        "server.pem",
        "keyboard.key",
    ] {
        fs::write(workspace.path().join(allowed), "public").expect("allowed fixture");
    }
    for blocked in [
        ".env.local",
        "id_ed25519_backup",
        "id_ecdsa.old",
        "credentials-copy",
    ] {
        fs::write(workspace.path().join(blocked), "secret").expect("blocked fixture");
    }
    fs::create_dir_all(workspace.path().join(".gcp/credentials")).expect("credential tree");
    fs::write(
        workspace.path().join(".gcp/credentials/token.txt"),
        "secret",
    )
    .expect("nested credential");

    let read = ReadTool::new(
        LocalToolConfig::new(workspace.path(), Vec::<PathBuf>::new()).expect("config"),
    );
    let context = prepare_context();
    for allowed in [
        ".env.example",
        "id_rsa.pub",
        "credentials.json",
        "server.pem",
        "keyboard.key",
    ] {
        assert!(
            read.prepare(&json!({"path":allowed}), &context).is_ok(),
            "expected {allowed} to remain readable"
        );
    }
    for blocked in [
        ".env.local",
        "id_ed25519_backup",
        "id_ecdsa.old",
        "credentials-copy",
        ".gcp/credentials/token.txt",
    ] {
        assert!(
            read.prepare(&json!({"path":blocked}), &context).is_err(),
            "expected {blocked} to be denied"
        );
    }
}

#[tokio::test]
async fn write_and_read_preserve_exact_content_with_ranges_and_caps() {
    let workspace = TempWorkspace::new("read-write");
    let config = LocalToolConfig::new(workspace.path(), Vec::<PathBuf>::new()).expect("config");
    let write = WriteTool::new(config.clone());
    let read = ReadTool::new(config);
    let updates = Arc::new(RecordingUpdates::default());

    let written = invoke(
        &write,
        json!({"path":"nested/file.txt","content":"one\ntwo\né","mode":"overwrite"}),
        CancellationToken::new(),
        Arc::clone(&updates),
    )
    .await;
    assert!(!written.is_error);
    assert!(output_text(&written).contains("10 bytes"));
    let appended = invoke(
        &write,
        json!({"path":"nested/file.txt","content":"\nfour","mode":"append"}),
        CancellationToken::new(),
        Arc::clone(&updates),
    )
    .await;
    assert!(!appended.is_error);
    assert_eq!(
        fs::read_to_string(workspace.path().join("nested/file.txt")).expect("written file"),
        "one\ntwo\né\nfour"
    );

    let ranged = invoke(
        &read,
        json!({"path":"nested/file.txt","line_offset":2,"n_lines":2}),
        CancellationToken::new(),
        Arc::clone(&updates),
    )
    .await;
    assert_eq!(output_text(&ranged), "2\ttwo\n3\té");
    let tail = invoke(
        &read,
        json!({"path":"nested/file.txt","line_offset":-2}),
        CancellationToken::new(),
        updates,
    )
    .await;
    assert_eq!(output_text(&tail), "3\té\n4\tfour");

    fs::write(workspace.path().join("long.txt"), "x".repeat(2_100)).expect("long line");
    let capped = invoke(
        &read,
        json!({"path":"long.txt"}),
        CancellationToken::new(),
        Arc::new(RecordingUpdates::default()),
    )
    .await;
    assert!(capped.truncated);
    assert!(output_text(&capped).contains("..."));
}

#[tokio::test]
async fn read_rejects_binary_and_edit_is_literal_unique_and_crlf_aware() {
    let workspace = TempWorkspace::new("edit");
    fs::write(workspace.path().join("binary.bin"), [0, 1, 2, 0xff]).expect("binary");
    fs::write(workspace.path().join("duplicate.txt"), "same same").expect("duplicate");
    fs::write(workspace.path().join("crlf.txt"), "a\r\nb\r\n").expect("crlf");
    let config = LocalToolConfig::new(workspace.path(), Vec::<PathBuf>::new()).expect("config");
    let read = ReadTool::new(config.clone());
    let edit = EditTool::new(config);
    let updates = Arc::new(RecordingUpdates::default());

    let binary = invoke(
        &read,
        json!({"path":"binary.bin"}),
        CancellationToken::new(),
        Arc::clone(&updates),
    )
    .await;
    assert!(binary.is_error);
    let duplicate = invoke(
        &edit,
        json!({"path":"duplicate.txt","old_string":"same","new_string":"new"}),
        CancellationToken::new(),
        Arc::clone(&updates),
    )
    .await;
    assert!(duplicate.is_error);
    assert_eq!(
        fs::read_to_string(workspace.path().join("duplicate.txt")).expect("unchanged"),
        "same same"
    );
    let replace_all = invoke(
        &edit,
        json!({"path":"duplicate.txt","old_string":"same","new_string":"new","replace_all":true}),
        CancellationToken::new(),
        Arc::clone(&updates),
    )
    .await;
    assert!(!replace_all.is_error);
    assert_eq!(
        fs::read_to_string(workspace.path().join("duplicate.txt")).expect("edited"),
        "new new"
    );
    let crlf = invoke(
        &edit,
        json!({"path":"crlf.txt","old_string":"a\nb\n","new_string":"x\ny\n"}),
        CancellationToken::new(),
        updates,
    )
    .await;
    assert!(!crlf.is_error, "{}", output_text(&crlf));
    assert_eq!(
        fs::read(workspace.path().join("crlf.txt")).expect("crlf"),
        b"x\r\ny\r\n"
    );
}

#[cfg(unix)]
#[test]
fn write_and_edit_refuse_symlink_targets_even_when_the_target_is_inside_a_root() {
    use std::os::unix::fs::symlink;

    let workspace = TempWorkspace::new("write-symlink");
    fs::write(workspace.path().join("real.txt"), "safe").expect("real file");
    symlink("real.txt", workspace.path().join("link.txt")).expect("symlink");
    let config = LocalToolConfig::new(workspace.path(), Vec::<PathBuf>::new()).expect("config");
    let context = prepare_context();
    assert!(WriteTool::new(config.clone())
        .prepare(&json!({"path":"link.txt","content":"bad"}), &context)
        .is_err());
    assert!(EditTool::new(config)
        .prepare(
            &json!({"path":"link.txt","old_string":"safe","new_string":"bad"}),
            &context
        )
        .is_err());
}

#[tokio::test]
async fn glob_and_grep_use_argv_not_shell_and_filter_sensitive_matches() {
    let workspace = TempWorkspace::new("search");
    fs::create_dir(workspace.path().join("src")).expect("src");
    fs::write(workspace.path().join("src/a.rs"), "needle one\n").expect("a");
    fs::write(workspace.path().join("src/b.rs"), "needle two\n").expect("b");
    fs::write(workspace.path().join("src/no.txt"), "nothing\n").expect("text");
    fs::write(workspace.path().join(".env.local"), "needle do-not-leak\n").expect("secret");
    fs::write(workspace.path().join("server.pem"), "needle public\n").expect("pem");
    fs::write(workspace.path().join(".env.example"), "needle template\n").expect("template");
    let config = LocalToolConfig::new(workspace.path(), Vec::<PathBuf>::new()).expect("config");
    let updates = Arc::new(RecordingUpdates::default());

    let glob = invoke(
        &GlobTool::new(config.clone()),
        json!({"pattern":"*.rs","path":"src"}),
        CancellationToken::new(),
        Arc::clone(&updates),
    )
    .await;
    assert!(!glob.is_error, "{}", output_text(&glob));
    assert!(output_text(&glob).contains("src/a.rs"));
    assert!(output_text(&glob).contains("src/b.rs"));

    let files = invoke(
        &GrepTool::new(config.clone()),
        json!({"pattern":"needle","output_mode":"files_with_matches"}),
        CancellationToken::new(),
        Arc::clone(&updates),
    )
    .await;
    assert!(!files.is_error, "{}", output_text(&files));
    assert!(output_text(&files).contains("src/a.rs"));
    assert!(output_text(&files).contains("server.pem"));
    assert!(output_text(&files).contains(".env.example"));
    assert!(!output_text(&files).contains(".env.local"));
    assert!(!output_text(&files).contains("do-not-leak"));
    {
        let recorded = updates.0.lock().expect("updates");
        assert!(recorded.iter().all(|update| {
            let text = update.text.as_deref().unwrap_or_default();
            !text.contains(".env.local") && !text.contains("do-not-leak")
        }));
    }

    let content = invoke(
        &GrepTool::new(config),
        json!({"pattern":"needle (one|two)","output_mode":"content","-n":true}),
        CancellationToken::new(),
        updates,
    )
    .await;
    assert!(!content.is_error, "{}", output_text(&content));
    assert!(output_text(&content).contains("src/a.rs:1:needle one"));
}

#[tokio::test]
async fn grep_accepts_a_confined_regular_file_path() {
    let workspace = TempWorkspace::new("grep-file");
    fs::write(workspace.path().join("one.rs"), "needle\nother\n").expect("file");
    let config = LocalToolConfig::new(workspace.path(), Vec::<PathBuf>::new()).expect("config");
    let result = invoke(
        &GrepTool::new(config),
        json!({"pattern":"needle","path":"one.rs","output_mode":"content"}),
        CancellationToken::new(),
        Arc::new(RecordingUpdates::default()),
    )
    .await;
    assert!(!result.is_error, "{}", output_text(&result));
    assert!(output_text(&result).contains("one.rs:1:needle"));
}

#[tokio::test]
async fn bash_uses_shell_c_semantics_confined_cwd_progress_timeout_and_cancellation() {
    let workspace = TempWorkspace::new("bash");
    fs::create_dir(workspace.path().join("sub")).expect("sub");
    let config = LocalToolConfig::new(workspace.path(), Vec::<PathBuf>::new()).expect("config");
    let bash = BashTool::new(config.clone());
    let updates = Arc::new(RecordingUpdates::default());
    let result = invoke(
        &bash,
        json!({"command":"printf 'quoted %s' \"$TERM\"; printf err >&2","cwd":"sub"}),
        CancellationToken::new(),
        Arc::clone(&updates),
    )
    .await;
    assert!(!result.is_error, "{}", output_text(&result));
    assert!(output_text(&result).contains("quoted dumb"));
    assert!(output_text(&result).contains("err"));
    assert!(updates
        .0
        .lock()
        .expect("updates")
        .iter()
        .any(|update| matches!(update.kind, ToolUpdateKind::Stdout | ToolUpdateKind::Stderr)));

    let timed_out = invoke(
        &BashTool::new(config.clone()),
        json!({"command":"sleep 5","timeout":1}),
        CancellationToken::new(),
        Arc::new(RecordingUpdates::default()),
    )
    .await;
    assert!(timed_out.is_error);
    assert!(output_text(&timed_out).contains("timed out"));

    let token = CancellationToken::new();
    let task_token = token.clone();
    let task = tokio::spawn(async move {
        invoke(
            &BashTool::new(config),
            json!({"command":"sleep 5"}),
            task_token,
            Arc::new(RecordingUpdates::default()),
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    token.cancel();
    let cancelled = task.await.expect("cancelled task");
    assert!(cancelled.is_error);
    assert!(output_text(&cancelled).contains("Interrupted"));
}

/// An update sink that is slow per streamed chunk - the real TUI/headless
/// sinks render or write to a socket, so a consumer that lags the producer is
/// the production condition, not a contrivance.
struct SlowUpdates;

impl ToolUpdateSink for SlowUpdates {
    fn emit(&self, update: ToolUpdate) {
        if matches!(update.kind, ToolUpdateKind::Stdout | ToolUpdateKind::Stderr) {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
}

/// A child that bursts far more than the 32-chunk channel can hold and then
/// exits must still return. Regression: the runner awaited both drain tasks
/// BEFORE draining the receiver, so a producer parked in `sender.send` on a
/// full channel after the child exited was joined by a consumer that never
/// read - a circular wait, i.e. a hang. Needs a lagging consumer to surface.
#[tokio::test]
async fn bash_burst_then_exit_returns_instead_of_deadlocking() {
    let workspace = TempWorkspace::new("bash-burst");
    let blob = workspace.path().join("blob");
    std::fs::write(&blob, vec![b'x'; 1024 * 1024]).expect("blob");
    let config = LocalToolConfig::new(workspace.path(), Vec::<PathBuf>::new()).expect("config");
    let tool = BashTool::new(config);
    let arguments = json!({"command":"cat blob; cat blob >&2","timeout":10});
    tool.validate_arguments(&arguments)
        .expect("valid arguments");
    tool.prepare(&arguments, &prepare_context())
        .expect("prepare tool");
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tool.execute(ToolInvocation {
            context: prepare_context(),
            arguments,
            cancellation: CancellationToken::new(),
            updates: Arc::new(SlowUpdates),
        }),
    )
    .await
    .expect("bash burst-then-exit deadlocked (drain tasks joined before the channel was drained)")
    .expect("execute tool");
    assert!(!result.is_error, "{}", output_text(&result));
    assert!(
        output_text(&result).contains('x'),
        "burst output was lost: {}",
        output_text(&result)
    );
}

#[tokio::test]
async fn bash_flood_caps_final_output_and_live_stream_updates_while_draining_the_child() {
    let workspace = TempWorkspace::new("bash-flood");
    let config = LocalToolConfig::new(workspace.path(), Vec::<PathBuf>::new()).expect("config");
    let updates = Arc::new(RecordingUpdates::default());
    let result = invoke(
        &BashTool::new(config),
        json!({"command":"yes x | head -c 11000000","timeout":10}),
        CancellationToken::new(),
        Arc::clone(&updates),
    )
    .await;
    assert!(!result.is_error, "{}", output_text(&result));
    assert!(result.truncated);
    assert!(output_text(&result).chars().count() <= 50_100);
    let updates = updates.0.lock().expect("updates");
    let streamed_bytes: usize = updates
        .iter()
        .filter(|update| matches!(update.kind, ToolUpdateKind::Stdout | ToolUpdateKind::Stderr))
        .filter_map(|update| update.text.as_ref())
        .map(String::len)
        .sum();
    assert!(streamed_bytes <= 10 * 1024 * 1024);
    assert_eq!(
        updates
            .iter()
            .filter(|update| {
                update
                    .text
                    .as_deref()
                    .is_some_and(|text| text.contains("remaining output is being drained"))
            })
            .count(),
        1
    );
}
