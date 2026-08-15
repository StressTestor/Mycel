use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use mycel_agent_protocol::{
    ContentPart, ExecutableToolOutput, ExecutableToolResult, FinishReason, GenerateResult, Message,
    OptionalNullable, PermissionDecision, PermissionMode, PermissionRule, PermissionScope,
    ProviderError, ProviderErrorKind, ProviderRequest, TokenUsage, ToolCall, ToolCallKind,
    ToolUpdate,
};
use mycel_agent_runtime::{
    register_retained_builtins, AgentId, BackgroundBoard, BackgroundKind, BackgroundMode,
    BackgroundShutdown, BackgroundStatus, BackgroundTaskState, CancellationToken, CapabilitySet,
    Clock, ExecutableTool, FilesystemOrchestrationStore, HookRunner, LiveEventSink,
    NativeChildContext, NativeChildStatus, NativeOrchestrationBundle,
    NativeOrchestrationBundleConfig, NativeOrchestrationDependencies, NativeSessionOptionsFactory,
    NativeTurnEngineFactory, NativeTurnRuntime, OrchestrationEvent, OrchestrationRecord,
    OrchestrationRootConfig, OrchestrationStore, RequestId, RetainedBuiltinConfig, Runtime,
    SessionHandle, SessionId, SessionOptions, ToolCallId, ToolInvocation, ToolPrepareContext,
    ToolRegistry, ToolScheduler, ToolUpdateSink, TurnEngine, TurnEngineConfig, TurnInput,
    TurnOutcomeReason, TurnProvider, TurnProviderFuture, WorkerProfile, ORCHESTRATION_TOOL_NAMES,
};
use serde_json::{json, Value};
use tokio::sync::Notify;

static TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "mycel-orchestration-bundle-{label}-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create temporary root");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct NullEvents;

impl LiveEventSink for NullEvents {
    fn publish(&self, _event: OrchestrationEvent) {}
}

struct TestClock(AtomicU64);

impl TestClock {
    fn new(now_ms: u64) -> Self {
        Self(AtomicU64::new(now_ms))
    }

    fn set(&self, now_ms: u64) {
        self.0.store(now_ms, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct ChildProvider {
    pending: AtomicBool,
    calls: AtomicUsize,
    entered: Notify,
}

impl ChildProvider {
    fn immediate() -> Self {
        Self {
            pending: AtomicBool::new(false),
            calls: AtomicUsize::new(0),
            entered: Notify::new(),
        }
    }

    fn pending() -> Self {
        Self {
            pending: AtomicBool::new(true),
            calls: AtomicUsize::new(0),
            entered: Notify::new(),
        }
    }
}

impl TurnProvider for ChildProvider {
    fn name(&self) -> &str {
        "bundle-child"
    }

    fn model(&self) -> &str {
        "bundle-child-model"
    }

    fn complete<'a>(
        &'a self,
        _request: ProviderRequest,
        cancellation: CancellationToken,
    ) -> TurnProviderFuture<'a> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.entered.notify_one();
        let pending = self.pending.load(Ordering::Acquire);
        Box::pin(async move {
            if pending {
                cancellation.cancelled().await;
                return Err(ProviderError::new(
                    ProviderErrorKind::Cancelled,
                    "child cancelled by test",
                ));
            }
            Ok(completed_response("native child result"))
        })
    }
}

struct ChildSessions;

impl NativeSessionOptionsFactory for ChildSessions {
    fn build(&self, context: &NativeChildContext) -> Result<SessionOptions, String> {
        let mut options = SessionOptions::new(context.session_id.clone());
        options.initial_permission_mode = PermissionMode::Auto;
        Ok(options)
    }
}

struct ChildTurns {
    provider: Arc<ChildProvider>,
}

impl NativeTurnEngineFactory for ChildTurns {
    fn build(&self, context: &NativeChildContext) -> Result<NativeTurnRuntime, String> {
        let engine = TurnEngine::new(
            self.provider.clone(),
            ToolRegistry::new(),
            HookRunner::new(),
            ToolScheduler::new(),
            TurnEngineConfig {
                retry_delay: Duration::ZERO,
                ..TurnEngineConfig::default()
            },
        )
        .map_err(|error| error.to_string())?;
        Ok(NativeTurnRuntime {
            engine: Arc::new(engine),
            effective_capabilities: context.profile.capabilities.clone(),
            system_prompt: "bounded native child".to_owned(),
            thinking_effort: None,
            max_completion_tokens: Some(128),
            metadata: BTreeMap::new(),
        })
    }
}

struct ScriptedProvider {
    responses: Mutex<VecDeque<GenerateResult>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<GenerateResult>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
        }
    }
}

impl TurnProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "bundle-parent"
    }

    fn model(&self) -> &str {
        "bundle-parent-model"
    }

    fn complete<'a>(
        &'a self,
        _request: ProviderRequest,
        _cancellation: CancellationToken,
    ) -> TurnProviderFuture<'a> {
        let response = self
            .responses
            .lock()
            .expect("parent responses")
            .pop_front()
            .expect("scripted response");
        Box::pin(async move { Ok(response) })
    }
}

#[derive(Default)]
struct NoUpdates;

impl ToolUpdateSink for NoUpdates {
    fn emit(&self, _update: ToolUpdate) {}
}

fn completed_response(text: &str) -> GenerateResult {
    GenerateResult {
        id: Some(RequestId::generate().into_string()),
        message: Message::assistant(vec![ContentPart::text(text)], vec![]),
        usage: Some(TokenUsage {
            input_other: 1,
            output: 1,
            input_cache_read: 0,
            input_cache_creation: 0,
        }),
        finish_reason: Some(FinishReason::Completed),
        raw_finish_reason: None,
        trace_id: OptionalNullable::Missing,
    }
}

fn tool_response(name: &str, arguments: Value) -> GenerateResult {
    GenerateResult {
        id: Some(RequestId::generate().into_string()),
        message: Message::assistant(
            vec![],
            vec![ToolCall {
                kind: ToolCallKind::Function,
                id: ToolCallId::generate().into_string(),
                name: name.to_owned(),
                arguments: Some(arguments.to_string()),
                extras: BTreeMap::new(),
            }],
        ),
        usage: Some(TokenUsage {
            input_other: 1,
            output: 1,
            input_cache_read: 0,
            input_cache_creation: 0,
        }),
        finish_reason: Some(FinishReason::ToolCalls),
        raw_finish_reason: None,
        trace_id: OptionalNullable::Missing,
    }
}

fn bounded_capabilities() -> CapabilitySet {
    CapabilitySet {
        tools: BTreeSet::new(),
        filesystem_roots: BTreeSet::new(),
        network: false,
        can_spawn_subagents: false,
        can_swarm: false,
        can_workflow: false,
    }
}

fn root_config() -> OrchestrationRootConfig {
    let bounded = WorkerProfile {
        name: "bounded".to_owned(),
        capabilities: bounded_capabilities(),
        allow_delegation: false,
    };
    let recursive_capabilities = CapabilitySet {
        can_spawn_subagents: true,
        can_swarm: true,
        can_workflow: true,
        ..bounded_capabilities()
    };
    let recursive = WorkerProfile {
        name: "recursive".to_owned(),
        capabilities: recursive_capabilities.clone(),
        allow_delegation: true,
    };
    OrchestrationRootConfig::new(
        "main",
        recursive_capabilities,
        BTreeMap::from([
            ("bounded".to_owned(), bounded),
            ("recursive".to_owned(), recursive),
        ]),
        "bounded",
    )
}

async fn parent_session(runtime: &Runtime, id: &str) -> SessionHandle {
    let mut options = SessionOptions::new(SessionId::new(id).expect("session id"));
    options.permission_rules.push(PermissionRule {
        decision: PermissionDecision::Allow,
        scope: PermissionScope::TurnOverride,
        pattern: "*".to_owned(),
        reason: Some("deterministic bundle integration".to_owned()),
    });
    runtime
        .create_session(options)
        .await
        .expect("parent session")
}

fn open_bundle(
    root: &TempRoot,
    runtime: &Runtime,
    session: &SessionHandle,
    registry: &ToolRegistry,
    provider: Arc<ChildProvider>,
    clock: Arc<TestClock>,
) -> NativeOrchestrationBundle {
    NativeOrchestrationBundle::open(
        NativeOrchestrationDependencies::new(
            runtime.clone(),
            registry.clone(),
            Arc::new(NullEvents),
            Arc::new(ChildSessions),
            Arc::new(ChildTurns { provider }),
        )
        .with_clock(clock),
        NativeOrchestrationBundleConfig::new(
            session.clone(),
            root.path().join("orchestration"),
            root_config(),
        )
        .with_shutdown_policy(BackgroundShutdown::StopAll),
    )
    .expect("open bundle")
}

fn context(session: &SessionHandle, call: &str) -> ToolPrepareContext {
    ToolPrepareContext {
        session_id: session.id().clone(),
        agent_id: AgentId::main(),
        turn_id: 1,
        tool_call_id: ToolCallId::new(call).expect("call id"),
    }
}

async fn invoke(
    session: &SessionHandle,
    tool: &dyn ExecutableTool,
    call: &str,
    arguments: Value,
) -> ExecutableToolResult {
    tool.validate_arguments(&arguments)
        .expect("valid arguments");
    tool.prepare(&arguments, &context(session, call))
        .expect("prepare");
    tool.execute(ToolInvocation {
        context: context(session, call),
        arguments,
        cancellation: CancellationToken::new(),
        updates: Arc::new(NoUpdates),
    })
    .await
    .expect("execute")
}

fn output_json(result: &ExecutableToolResult) -> Value {
    let ExecutableToolOutput::Text(text) = &result.output else {
        panic!("expected text output");
    };
    serde_json::from_str(text).expect("JSON output")
}

#[tokio::test]
async fn bundle_registers_after_parent_engine_and_runs_a_real_native_child_with_goal_budget() {
    let root = TempRoot::new("parent");
    let runtime = Runtime::new(root.path().join("sessions"));
    let session = parent_session(&runtime, "parent").await;
    let registry = ToolRegistry::new();
    let parent_provider = Arc::new(ScriptedProvider::new(vec![
        tool_response(
            "Agent",
            json!({"prompt":"delegate in process","subagent_type":"bounded"}),
        ),
        completed_response("parent finished"),
    ]));
    let parent_engine = TurnEngine::new(
        parent_provider,
        registry.clone(),
        HookRunner::new(),
        ToolScheduler::new(),
        TurnEngineConfig {
            retry_delay: Duration::ZERO,
            ..TurnEngineConfig::default()
        },
    )
    .expect("parent engine");
    let child_provider = Arc::new(ChildProvider::immediate());
    let bundle = open_bundle(
        &root,
        &runtime,
        &session,
        &registry,
        child_provider.clone(),
        Arc::new(TestClock::new(0)),
    );

    let delegate = bundle
        .native_delegate_invocation("same native path")
        .expect("delegate invocation");
    assert_eq!(delegate.tool.definition().name, "Agent");
    assert_eq!(delegate.arguments["prompt"], "same native path");
    assert!(registry.snapshot().get("Agent").is_some());

    let outcome = parent_engine
        .run_turn(
            &session,
            TurnInput::user("delegate", "parent system"),
            CancellationToken::new(),
        )
        .await
        .expect("parent turn");
    assert_eq!(outcome.reason, TurnOutcomeReason::Completed);
    assert_eq!(child_provider.calls.load(Ordering::Relaxed), 1);
    let board = bundle.native_host().snapshot();
    let child = board.children.values().next().expect("native child");
    assert_eq!(child.status, NativeChildStatus::Completed);
    assert!(runtime.get_session(&child.session_id).await.is_none());

    let state: Arc<dyn mycel_agent_runtime::SessionBuiltinStatePort> = Arc::new(session.clone());
    register_retained_builtins(
        &registry,
        RetainedBuiltinConfig::new(session.clone(), state)
            .with_goal_budget(bundle.goal_budget_port()),
    )
    .expect("retained tools");
    let tools = registry.snapshot();
    let created = invoke(
        &session,
        tools.get("CreateGoal").expect("CreateGoal").as_ref(),
        "create-goal",
        json!({"id":"budgeted","objective":"stay bounded"}),
    )
    .await;
    assert!(!created.is_error);
    let budget = invoke(
        &session,
        tools.get("SetGoalBudget").expect("SetGoalBudget").as_ref(),
        "set-budget",
        json!({"value":1,"unit":"turns"}),
    )
    .await;
    assert!(!budget.is_error);
    let exhausted = bundle.record_goal_turn_usage(7).expect("record goal usage");
    assert!(exhausted.over_budget);
    assert_eq!(exhausted.turns_used, 1);
    assert!(bundle.enforce_goal_budget().expect("enforce").over_budget);
    assert!(bundle.record_goal_turn_usage(1).is_err());

    bundle
        .shutdown(BackgroundShutdown::StopAll)
        .await
        .expect("shutdown");
    assert!(bundle.is_shutdown());
    assert!(ORCHESTRATION_TOOL_NAMES
        .into_iter()
        .all(|name| registry.snapshot().get(name).is_none()));
    session.close().await.expect("close parent");
}

#[tokio::test]
async fn foreground_agent_detaches_durably_and_releases_its_waiting_tool_call() {
    let root = TempRoot::new("foreground-detach");
    let runtime = Runtime::new(root.path().join("sessions"));
    let session = parent_session(&runtime, "foreground-detach").await;
    let registry = ToolRegistry::new();
    let child_provider = Arc::new(ChildProvider::pending());
    let bundle = open_bundle(
        &root,
        &runtime,
        &session,
        &registry,
        child_provider.clone(),
        Arc::new(TestClock::new(10)),
    );
    let agent = registry.snapshot().get("Agent").expect("Agent tool");
    let invoke_session = session.clone();
    let invocation = tokio::spawn(async move {
        invoke(
            &invoke_session,
            agent.as_ref(),
            "foreground-agent",
            json!({"prompt":"wait until detached","subagent_type":"bounded"}),
        )
        .await
    });

    tokio::time::timeout(Duration::from_secs(1), child_provider.entered.notified())
        .await
        .expect("foreground child entered provider");
    let detached = bundle
        .detach_foreground_tasks(false)
        .expect("detach foreground child");
    assert_eq!(detached.len(), 1);
    assert_eq!(detached[0].kind, BackgroundKind::Subagent);
    assert_eq!(
        detached[0].mode,
        BackgroundMode::Detached { keep_alive: false }
    );

    let released = tokio::time::timeout(Duration::from_secs(1), invocation)
        .await
        .expect("foreground tool released")
        .expect("foreground invocation joined");
    assert_eq!(output_json(&released)["taskId"], detached[0].id);
    assert_eq!(output_json(&released)["status"], "running");

    bundle
        .shutdown(BackgroundShutdown::StopAll)
        .await
        .expect("shutdown detached child");
    session.close().await.expect("close parent");
}

#[tokio::test]
async fn bundle_produces_swarm_workflow_background_and_cron_artifacts_without_recursion() {
    let root = TempRoot::new("artifacts");
    let runtime = Runtime::new(root.path().join("sessions"));
    let session = parent_session(&runtime, "artifacts").await;
    let registry = ToolRegistry::new();
    let clock = Arc::new(TestClock::new(0));
    let bundle = open_bundle(
        &root,
        &runtime,
        &session,
        &registry,
        Arc::new(ChildProvider::immediate()),
        clock.clone(),
    );
    let tools = registry.snapshot();

    let before = bundle.native_host().snapshot().children.len();
    let recursive = invoke(
        &session,
        tools.get("AgentSwarm").expect("AgentSwarm").as_ref(),
        "recursive",
        json!({
            "description":"must fail",
            "subagent_type":"recursive",
            "prompt_template":"review {{item}}",
            "items":["one"]
        }),
    )
    .await;
    assert!(recursive.is_error);
    assert_eq!(bundle.native_host().snapshot().children.len(), before);

    let swarm = invoke(
        &session,
        tools.get("AgentSwarm").expect("AgentSwarm").as_ref(),
        "swarm",
        json!({
            "description":"bounded fanout",
            "subagent_type":"bounded",
            "prompt_template":"review {{item}}",
            "items":["one","two"]
        }),
    )
    .await;
    assert!(!swarm.is_error);
    assert_eq!(
        output_json(&swarm)["members"]
            .as_array()
            .expect("members")
            .len(),
        2
    );

    let workflow = invoke(
        &session,
        tools.get("Workflow").expect("Workflow").as_ref(),
        "workflow",
        json!({"plan":{
            "version":1,
            "name":"bundle-flow",
            "description":"bundle workflow",
            "phases":[{"title":"one","tasks":[{
                "id":"work",
                "description":"work",
                "prompt":"use {{arg:value}}",
                "worker_profile":"bounded"
            }]}]
        },"arguments":{"value":"native"}}),
    )
    .await;
    assert!(!workflow.is_error);
    let workflow_task = output_json(&workflow)["taskId"]
        .as_str()
        .expect("workflow task")
        .to_owned();
    let workflow_output = invoke(
        &session,
        tools.get("TaskOutput").expect("TaskOutput").as_ref(),
        "workflow-output",
        json!({"task_id":workflow_task,"block":true,"wait_ms":2000}),
    )
    .await;
    assert_eq!(output_json(&workflow_output)["task"]["status"], "completed");

    let background = invoke(
        &session,
        tools.get("Agent").expect("Agent").as_ref(),
        "background",
        json!({"prompt":"background native","run_in_background":true}),
    )
    .await;
    let background_task = output_json(&background)["taskId"]
        .as_str()
        .expect("background task")
        .to_owned();
    let background_output = invoke(
        &session,
        tools.get("TaskOutput").expect("TaskOutput").as_ref(),
        "background-output",
        json!({"task_id":background_task,"block":true,"wait_ms":2000}),
    )
    .await;
    assert_eq!(
        output_json(&background_output)["task"]["status"],
        "completed"
    );

    let cron = invoke(
        &session,
        tools.get("CronCreate").expect("CronCreate").as_ref(),
        "cron",
        json!({"expression":"* * * * *","prompt":"tick","recurring":true}),
    )
    .await;
    assert!(!cron.is_error);
    clock.set(3 * 60_000);
    let fired = bundle.tick_cron(true).expect("cron tick");
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].coalesced_count, 3);

    for directory in ["tasks", "workflows"] {
        let directory = bundle.artifact_root().join(directory);
        let entries = fs::read_dir(&directory)
            .expect("artifact directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("artifact entries");
        assert!(!entries.is_empty());
        #[cfg(unix)]
        {
            assert_eq!(
                fs::metadata(&directory)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert!(entries.into_iter().all(|entry| {
                fs::metadata(entry.path())
                    .expect("artifact metadata")
                    .permissions()
                    .mode()
                    & 0o777
                    == 0o600
            }));
        }
    }

    bundle
        .shutdown(BackgroundShutdown::StopAll)
        .await
        .expect("shutdown");
    session.close().await.expect("close parent");
}

#[tokio::test]
async fn bundle_shutdown_cancels_native_background_and_restart_marks_orphans_lost() {
    let root = TempRoot::new("cancel-restart");
    let runtime = Runtime::new(root.path().join("sessions"));
    let session = parent_session(&runtime, "cancel-restart").await;
    let registry = ToolRegistry::new();
    let provider = Arc::new(ChildProvider::pending());
    let bundle = open_bundle(
        &root,
        &runtime,
        &session,
        &registry,
        provider.clone(),
        Arc::new(TestClock::new(10)),
    );
    let tools = registry.snapshot();
    let task_list = tools.get("TaskList").expect("TaskList");
    let started = invoke(
        &session,
        tools.get("Agent").expect("Agent").as_ref(),
        "pending",
        json!({"prompt":"wait","run_in_background":true}),
    )
    .await;
    let task_id = output_json(&started)["taskId"]
        .as_str()
        .expect("task id")
        .to_owned();
    tokio::time::timeout(Duration::from_secs(2), provider.entered.notified())
        .await
        .expect("child entered");
    let host = bundle.native_host();
    let stopped = bundle
        .shutdown(BackgroundShutdown::StopAll)
        .await
        .expect("bounded shutdown");
    assert_eq!(stopped.as_slice(), std::slice::from_ref(&task_id));
    let listed = invoke(&session, task_list.as_ref(), "list-killed", json!({})).await;
    let tasks = output_json(&listed);
    assert_eq!(tasks[0]["status"], "killed");
    assert!(host
        .snapshot()
        .children
        .values()
        .all(|child| child.status == NativeChildStatus::Cancelled));
    drop(bundle);

    let store_root = root.path().join("orchestration");
    let store = FilesystemOrchestrationStore::open(&store_root, session.id()).expect("store");
    let orphan = BackgroundTaskState {
        id: "orphan-task".to_owned(),
        kind: BackgroundKind::Subagent,
        description: "orphan".to_owned(),
        mode: BackgroundMode::Detached { keep_alive: false },
        status: BackgroundStatus::Running,
        started_at_ms: 10,
        ended_at_ms: None,
        timeout_ms: None,
        stop_reason: None,
    };
    store
        .append(&[OrchestrationRecord {
            scope: "background".to_owned(),
            action: "registered".to_owned(),
            entity_id: Some(orphan.id.clone()),
            at_ms: 10,
            state: serde_json::to_value(BackgroundBoard {
                tasks: BTreeMap::from([(orphan.id.clone(), orphan)]),
            })
            .expect("serialize orphan"),
            detail: json!({}),
        }])
        .expect("seed orphan");

    let reopened = open_bundle(
        &root,
        &runtime,
        &session,
        &registry,
        Arc::new(ChildProvider::immediate()),
        Arc::new(TestClock::new(20)),
    );
    let list = registry.snapshot().get("TaskList").expect("reopened list");
    let listed = invoke(&session, list.as_ref(), "list-lost", json!({})).await;
    let tasks = output_json(&listed);
    assert_eq!(tasks[0]["id"], "orphan-task");
    assert_eq!(tasks[0]["status"], "lost");
    assert_eq!(
        FilesystemOrchestrationStore::open(&store_root, session.id())
            .expect("reopen store")
            .load()
            .expect("records")
            .last()
            .expect("last record")
            .action,
        "reconciled_lost"
    );

    reopened
        .shutdown(BackgroundShutdown::StopAll)
        .await
        .expect("reopened shutdown");
    session.close().await.expect("close parent");
}
