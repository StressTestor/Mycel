use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use mycel_agent_protocol::{ExecutableToolOutput, ExecutableToolResult, ToolUpdate};
use mycel_agent_runtime::{
    native_delegate_arguments, register_orchestration_builtins, AgentId, BackgroundKind,
    BackgroundMode, BackgroundRegistry, BackgroundShutdown, CancellationToken, CapabilitySet,
    Clock, ExecutableTool, LiveEventSink, NativeAgentFuture, NativeAgentRequest, NativeAgentResult,
    NativeStopFuture, NativeSubagentHost, OrchestrationBuiltinConfig, OrchestrationDependencies,
    OrchestrationEvent, OrchestrationPorts, OrchestrationRecord, OrchestrationRootConfig,
    OrchestrationStore, RequestId, SessionId, ToolCallId, ToolInvocation, ToolPrepareContext,
    ToolRegistry, ToolUpdateSink, WorkerProfile, HYPHAE_TOOL_NAME, NATIVE_DELEGATE_TOOL,
    ORCHESTRATION_TOOL_NAMES,
};
use serde_json::{json, Value};

struct TempArtifacts(PathBuf);

impl TempArtifacts {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "mycel-orchestration-{label}-{}",
            RequestId::generate()
        ));
        fs::create_dir(&path).expect("create artifacts");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempArtifacts {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct TestStore(Mutex<Vec<OrchestrationRecord>>);

impl OrchestrationStore for TestStore {
    fn load(&self) -> Result<Vec<OrchestrationRecord>, String> {
        Ok(self.0.lock().expect("records").clone())
    }

    fn append(&self, records: &[OrchestrationRecord]) -> Result<(), String> {
        self.0.lock().expect("records").extend_from_slice(records);
        Ok(())
    }
}

#[derive(Default)]
struct TestEvents(Mutex<Vec<OrchestrationEvent>>);

impl LiveEventSink for TestEvents {
    fn publish(&self, event: OrchestrationEvent) {
        self.0.lock().expect("events").push(event);
    }
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

#[derive(Clone, Debug)]
struct SeenRequest {
    agent_id: String,
    prompt: String,
    native_spawn: bool,
}

#[derive(Default)]
struct FakeHost {
    mode: AtomicU8,
    stop_mode: AtomicU8,
    requests: Mutex<Vec<SeenRequest>>,
    stops: Mutex<Vec<String>>,
}

impl FakeHost {
    const PENDING: u8 = 1;

    fn pending(&self) {
        self.mode.store(Self::PENDING, Ordering::SeqCst);
    }

    fn reject_stop(&self) {
        self.stop_mode.store(1, Ordering::SeqCst);
    }

    fn timeout_first_stop(&self) {
        self.stop_mode.store(2, Ordering::SeqCst);
    }

    fn requests(&self) -> Vec<SeenRequest> {
        self.requests.lock().expect("requests").clone()
    }
}

impl NativeSubagentHost for FakeHost {
    fn execute(&self, request: NativeAgentRequest) -> NativeAgentFuture {
        self.requests.lock().expect("requests").push(SeenRequest {
            agent_id: request.agent_id.clone(),
            prompt: request.prompt.clone(),
            native_spawn: matches!(
                request.operation,
                mycel_agent_runtime::NativeAgentOperation::Spawn { .. }
            ),
        });
        let pending = self.mode.load(Ordering::SeqCst) == Self::PENDING;
        Box::pin(async move {
            if pending {
                request.cancellation.cancelled().await;
                return Err("cancelled by test host".to_owned());
            }
            request
                .output
                .append(&format!("progress:{}\n", request.prompt))?;
            Ok(NativeAgentResult {
                output: format!("native:{}", request.prompt),
            })
        })
    }

    fn stop(&self, agent_id: String, _reason: String) -> NativeStopFuture {
        self.stops.lock().expect("stops").push(agent_id);
        let reject = self.stop_mode.load(Ordering::SeqCst) == 1;
        let timeout_once = self
            .stop_mode
            .compare_exchange(2, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        Box::pin(async move {
            if reject {
                Err("stop rejected by test host".to_owned())
            } else if timeout_once {
                std::future::pending().await
            } else {
                Ok(())
            }
        })
    }
}

#[derive(Default)]
struct NoUpdates;

impl ToolUpdateSink for NoUpdates {
    fn emit(&self, _update: ToolUpdate) {}
}

fn context() -> ToolPrepareContext {
    ToolPrepareContext {
        session_id: SessionId::new("session-orchestration").expect("session"),
        agent_id: AgentId::main(),
        turn_id: 1,
        tool_call_id: ToolCallId::new("orchestration-call").expect("call"),
    }
}

async fn invoke(tool: &dyn ExecutableTool, arguments: Value) -> ExecutableToolResult {
    tool.validate_arguments(&arguments)
        .expect("valid arguments");
    tool.prepare(&arguments, &context()).expect("prepare");
    tool.execute(ToolInvocation {
        context: context(),
        arguments,
        cancellation: CancellationToken::new(),
        updates: Arc::new(NoUpdates),
    })
    .await
    .expect("tool execution")
}

fn text(result: &ExecutableToolResult) -> &str {
    match &result.output {
        ExecutableToolOutput::Text(text) => text,
        ExecutableToolOutput::Parts(_) => panic!("expected text"),
    }
}

fn output_json(result: &ExecutableToolResult) -> Value {
    serde_json::from_str(text(result)).expect("JSON output")
}

fn root_capabilities() -> CapabilitySet {
    CapabilitySet {
        tools: BTreeSet::from([
            "Read".to_owned(),
            "Write".to_owned(),
            "Edit".to_owned(),
            "Bash".to_owned(),
        ]),
        filesystem_roots: BTreeSet::from(["/workspace".to_owned()]),
        network: false,
        can_spawn_subagents: true,
        can_swarm: true,
        can_workflow: true,
    }
}

fn coder_profile() -> WorkerProfile {
    WorkerProfile {
        name: "coder".to_owned(),
        capabilities: CapabilitySet {
            tools: BTreeSet::from(["Read".to_owned(), "Edit".to_owned()]),
            filesystem_roots: BTreeSet::from(["/workspace".to_owned()]),
            network: false,
            can_spawn_subagents: false,
            can_swarm: false,
            can_workflow: false,
        },
        allow_delegation: false,
    }
}

struct Fixture {
    _artifacts: TempArtifacts,
    registry: ToolRegistry,
    builtins: mycel_agent_runtime::OrchestrationBuiltins,
    host: Arc<FakeHost>,
    clock: Arc<TestClock>,
}

fn fixture(label: &str, configure: impl FnOnce(&mut OrchestrationBuiltinConfig)) -> Fixture {
    let artifacts = TempArtifacts::new(label);
    let store = Arc::new(TestStore::default());
    let events = Arc::new(TestEvents::default());
    let clock = Arc::new(TestClock::new(0));
    let ports = OrchestrationPorts::new(store, events, clock.clone());
    let host = Arc::new(FakeHost::default());
    let profiles = BTreeMap::from([("coder".to_owned(), coder_profile())]);
    let dependencies = OrchestrationDependencies::new(ports, host.clone(), artifacts.path());
    let root = OrchestrationRootConfig::new("main", root_capabilities(), profiles, "coder");
    let mut config = OrchestrationBuiltinConfig::new(dependencies, root);
    configure(&mut config);
    let registry = ToolRegistry::new();
    let builtins = register_orchestration_builtins(&registry, config).expect("register");
    Fixture {
        _artifacts: artifacts,
        registry,
        builtins,
        host,
        clock,
    }
}

#[tokio::test]
async fn registry_is_strict_and_delegate_is_the_native_agent_path() {
    let fixture = fixture("delegate", |_| {});
    let snapshot = fixture.registry.snapshot();
    let names = snapshot
        .definitions()
        .iter()
        .map(|definition| definition.name.as_str())
        .collect::<BTreeSet<_>>();
    let expected = ORCHESTRATION_TOOL_NAMES
        .into_iter()
        .collect::<BTreeSet<_>>();
    assert_eq!(names, expected);
    assert!(snapshot
        .definitions()
        .iter()
        .all(|definition| definition.parameters["additionalProperties"] == false));

    let agent = snapshot.get(NATIVE_DELEGATE_TOOL).expect("Agent");
    assert!(agent
        .validate_arguments(&json!({
            "prompt":"work",
            "resume":"old",
            "subagent_type":"coder"
        }))
        .is_err());
    assert!(agent
        .validate_arguments(&json!({"prompt":"work","unknown":true}))
        .is_err());
    let create_goal = snapshot.get("CreateGoal").expect("CreateGoal");
    assert!(create_goal
        .validate_arguments(&json!({
            "objective":"conflict",
            "queue":true,
            "replace":true
        }))
        .is_err());
    let hyphae = snapshot.get(HYPHAE_TOOL_NAME).expect("Hyphae");
    assert!(hyphae
        .validate_arguments(&json!({"command":"on","finish_task":true}))
        .is_err());
    let result = invoke(agent.as_ref(), native_delegate_arguments("do the thing")).await;
    assert!(!result.is_error);
    assert_eq!(text(&result), "native:do the thing");
    let requests = fixture.host.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].native_spawn);
    assert_eq!(requests[0].prompt, "do the thing");
    assert!(requests[0].agent_id.starts_with("agent-"));
    let resumed = invoke(
        agent.as_ref(),
        json!({"prompt":"continue natively","resume":requests[0].agent_id.clone()}),
    )
    .await;
    assert!(!resumed.is_error, "{}", text(&resumed));
    let requests = fixture.host.requests();
    assert_eq!(requests.len(), 2);
    assert!(!requests[1].native_spawn);
}

#[tokio::test]
async fn swarm_rejects_fanout_and_recursive_profiles_before_host_execution() {
    let fixture = fixture("swarm-limits", |config| {
        config.max_swarm_fan_out = 2;
        config.max_swarm_concurrency = 1;
        config.profiles.insert(
            "recursive".to_owned(),
            WorkerProfile {
                name: "recursive".to_owned(),
                capabilities: CapabilitySet {
                    can_spawn_subagents: true,
                    can_swarm: true,
                    ..coder_profile().capabilities
                },
                allow_delegation: true,
            },
        );
    });
    let swarm = fixture
        .registry
        .snapshot()
        .get("AgentSwarm")
        .expect("swarm");
    let too_many = invoke(
        swarm.as_ref(),
        json!({
            "description":"too many",
            "subagent_type":"coder",
            "prompt_template":"review {{item}}",
            "items":["a","b","c"]
        }),
    )
    .await;
    assert!(too_many.is_error);
    let recursive = invoke(
        swarm.as_ref(),
        json!({
            "description":"recursive",
            "subagent_type":"recursive",
            "prompt_template":"review {{item}}",
            "items":["a","b"]
        }),
    )
    .await;
    assert!(recursive.is_error);
    assert!(fixture.host.requests().is_empty());

    let valid = invoke(
        swarm.as_ref(),
        json!({
            "description":"bounded",
            "subagent_type":"coder",
            "prompt_template":"review {{item}}",
            "items":["a","b"]
        }),
    )
    .await;
    assert!(!valid.is_error);
    let members = output_json(&valid)["members"]
        .as_array()
        .expect("members")
        .clone();
    assert_eq!(members.len(), 2);
    assert_eq!(members[0]["index"], 1);
    assert_eq!(members[1]["index"], 2);
}

#[tokio::test]
async fn background_stop_serializes_monitor_settlement_and_records_killed() {
    let fixture = fixture("background-stop", |config| {
        config.cancellation_grace = Duration::from_millis(100);
    });
    fixture.host.pending();
    let snapshot = fixture.registry.snapshot();
    let agent = snapshot.get("Agent").expect("Agent");
    let started = invoke(
        agent.as_ref(),
        json!({
            "prompt":"wait forever",
            "description":"pending child",
            "run_in_background":true
        }),
    )
    .await;
    let task_id = output_json(&started)["taskId"]
        .as_str()
        .expect("task id")
        .to_owned();

    let detach = snapshot.get("TaskDetach").expect("detach");
    let detached = invoke(
        detach.as_ref(),
        json!({"task_id":task_id,"keep_alive":true}),
    )
    .await;
    assert!(!detached.is_error);

    let stop = snapshot.get("TaskStop").expect("stop");
    let stopped = invoke(
        stop.as_ref(),
        json!({"task_id":task_id,"reason":"test cancellation"}),
    )
    .await;
    assert!(!stopped.is_error, "{}", text(&stopped));
    assert_eq!(output_json(&stopped)["status"], "killed");
    tokio::task::yield_now().await;
    assert!(!fixture.host.stops.lock().expect("stops").is_empty());

    let list = snapshot.get("TaskList").expect("list");
    let listed = invoke(list.as_ref(), json!({})).await;
    assert_eq!(output_json(&listed)[0]["status"], "killed");
}

#[tokio::test]
async fn rejected_stop_never_reports_killed_and_background_timeout_is_bounded() {
    let rejected = fixture("stop-rejected", |config| {
        config.cancellation_grace = Duration::from_millis(100);
    });
    rejected.host.pending();
    rejected.host.reject_stop();
    let snapshot = rejected.registry.snapshot();
    let started = invoke(
        snapshot.get("Agent").expect("Agent").as_ref(),
        json!({"prompt":"pending","run_in_background":true}),
    )
    .await;
    let task_id = output_json(&started)["taskId"]
        .as_str()
        .expect("task id")
        .to_owned();
    let stop = invoke(
        snapshot.get("TaskStop").expect("stop").as_ref(),
        json!({"task_id":task_id,"reason":"reject this"}),
    )
    .await;
    assert!(stop.is_error);
    assert!(text(&stop).contains("rejected"));
    let output = invoke(
        snapshot.get("TaskOutput").expect("output").as_ref(),
        json!({"task_id":task_id,"block":true,"wait_ms":500}),
    )
    .await;
    assert_eq!(output_json(&output)["task"]["status"], "failed");

    let timed = fixture("agent-timeout", |config| {
        config.agent_timeout = Duration::from_millis(5);
        config.cancellation_grace = Duration::from_millis(100);
    });
    timed.host.pending();
    let snapshot = timed.registry.snapshot();
    let started = invoke(
        snapshot.get("Agent").expect("Agent").as_ref(),
        json!({"prompt":"timeout","run_in_background":true}),
    )
    .await;
    let task_id = output_json(&started)["taskId"]
        .as_str()
        .expect("task id")
        .to_owned();
    let output = invoke(
        snapshot.get("TaskOutput").expect("output").as_ref(),
        json!({"task_id":task_id,"block":true,"wait_ms":500}),
    )
    .await;
    assert_eq!(output_json(&output)["task"]["status"], "timed_out");
}

#[tokio::test]
async fn shutdown_retries_one_dropped_idempotent_stop_before_settling_killed() {
    let fixture = fixture("shutdown-stop-retry", |config| {
        config.cancellation_grace = Duration::from_millis(5);
    });
    fixture.host.pending();
    fixture.host.timeout_first_stop();
    let snapshot = fixture.registry.snapshot();
    let list = snapshot.get("TaskList").expect("TaskList");
    let started = invoke(
        snapshot.get("Agent").expect("Agent").as_ref(),
        json!({"prompt":"pending","run_in_background":true}),
    )
    .await;
    let task_id = output_json(&started)["taskId"]
        .as_str()
        .expect("task id")
        .to_owned();

    let stopped = fixture
        .builtins
        .shutdown(BackgroundShutdown::StopAll)
        .await
        .expect("idempotent retry closes child");
    assert_eq!(stopped, vec![task_id.clone()]);
    assert_eq!(fixture.host.stops.lock().expect("stops").len(), 2);
    let listed = invoke(list.as_ref(), json!({})).await;
    let task = output_json(&listed)
        .as_array()
        .expect("task list")
        .iter()
        .find(|task| task["id"] == task_id)
        .expect("settled task")
        .clone();
    assert_eq!(task["status"], "killed");
}

#[tokio::test]
async fn workflow_is_native_interpolates_prior_results_and_caps_workers() {
    let fixture = fixture("workflow", |_| {});
    let workflow = fixture
        .registry
        .snapshot()
        .get("Workflow")
        .expect("workflow");
    let plan = json!({
        "version":1,
        "name":"native-flow",
        "description":"native workflow",
        "phases":[
            {"title":"first","tasks":[{
                "id":"a","description":"first task",
                "prompt":"alpha {{arg:value}}","worker_profile":"coder"
            }]},
            {"title":"second","tasks":[{
                "id":"b","description":"second task",
                "prompt":"use {{result:a}}","worker_profile":"coder"
            }]}
        ]
    });
    let started = invoke(
        workflow.as_ref(),
        json!({"plan":plan,"arguments":{"value":"X"}}),
    )
    .await;
    assert!(!started.is_error, "{}", text(&started));
    let task_id = output_json(&started)["taskId"]
        .as_str()
        .expect("task id")
        .to_owned();
    let output_tool = fixture
        .registry
        .snapshot()
        .get("TaskOutput")
        .expect("output");
    let output = invoke(
        output_tool.as_ref(),
        json!({"task_id":task_id,"block":true,"wait_ms":2000}),
    )
    .await;
    assert!(!output.is_error, "{}", text(&output));
    assert_eq!(output_json(&output)["task"]["status"], "completed");
    let requests = fixture.host.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].prompt, "alpha X");
    assert_eq!(requests[1].prompt, "use native:alpha X");

    let four = json!({
        "version":1,
        "name":"too-wide",
        "description":"four workers",
        "phases":[{"title":"phase","tasks":[
            {"id":"a","description":"a","prompt":"a","worker_profile":"coder"},
            {"id":"b","description":"b","prompt":"b","worker_profile":"coder"},
            {"id":"c","description":"c","prompt":"c","worker_profile":"coder"},
            {"id":"d","description":"d","prompt":"d","worker_profile":"coder"}
        ]}]
    });
    let rejected = invoke(workflow.as_ref(), json!({"plan":four})).await;
    assert!(rejected.is_error);
}

#[cfg(unix)]
#[tokio::test]
async fn workflow_manifests_and_task_logs_are_private() {
    let fixture = fixture("permissions", |_| {});
    let workflow = fixture
        .registry
        .snapshot()
        .get("Workflow")
        .expect("workflow");
    let plan = json!({
        "version":1,"name":"private-flow","description":"private",
        "phases":[{"title":"one","tasks":[{
            "id":"a","description":"a","prompt":"a","worker_profile":"coder"
        }]}]
    });
    let started = invoke(workflow.as_ref(), json!({"plan":plan})).await;
    let task_id = output_json(&started)["taskId"]
        .as_str()
        .expect("task")
        .to_owned();
    let output_tool = fixture
        .registry
        .snapshot()
        .get("TaskOutput")
        .expect("output");
    let _ = invoke(
        output_tool.as_ref(),
        json!({"task_id":task_id,"block":true,"wait_ms":2000}),
    )
    .await;

    for directory in ["tasks", "workflows"] {
        let path = fixture._artifacts.path().join(directory);
        assert_eq!(
            fs::metadata(&path).expect("directory").permissions().mode() & 0o777,
            0o700
        );
        for entry in fs::read_dir(path).expect("entries") {
            let path = entry.expect("entry").path();
            assert_eq!(
                fs::metadata(path).expect("file").permissions().mode() & 0o777,
                0o600
            );
        }
    }
}

#[tokio::test]
async fn goal_promotion_is_single_winner_and_terminal_actions_stop_the_batch() {
    let fixture = fixture("goal-race", |_| {});
    let snapshot = fixture.registry.snapshot();
    let create = snapshot.get("CreateGoal").expect("create");
    let update = snapshot.get("UpdateGoal").expect("update");
    let get = snapshot.get("GetGoal").expect("get");
    invoke(create.as_ref(), json!({"id":"g1","objective":"first"})).await;
    invoke(
        create.as_ref(),
        json!({"id":"g2","objective":"second","queue":true}),
    )
    .await;
    let complete_args = json!({"action":"complete","reason":"done"});
    let spec = update
        .prepare(&complete_args, &context())
        .expect("complete spec");
    assert!(spec.stop_batch_after_this);
    let complete = invoke(update.as_ref(), complete_args).await;
    assert!(complete.stop_turn);

    let left = update.clone();
    let right = update.clone();
    let (first, second) = tokio::join!(
        invoke(left.as_ref(), json!({"action":"promote"})),
        invoke(right.as_ref(), json!({"action":"promote"}))
    );
    let promoted = [first, second]
        .iter()
        .filter(|result| output_json(result).is_object())
        .count();
    assert_eq!(promoted, 1);
    let board = output_json(&invoke(get.as_ref(), json!({})).await);
    assert_eq!(board["current"]["id"], "g2");
    assert!(board["queue"].as_array().expect("queue").is_empty());
}

#[tokio::test]
async fn cron_tool_coalesces_missed_ticks_and_hyphae_is_session_only() {
    let fixture = fixture("cron-hyphae", |config| {
        config.xhigh_supported = true;
    });
    let snapshot = fixture.registry.snapshot();
    let create = snapshot.get("CronCreate").expect("cron");
    let created = invoke(
        create.as_ref(),
        json!({"expression":"* * * * *","prompt":"check","recurring":true}),
    )
    .await;
    assert!(!created.is_error);
    fixture.clock.set(5 * 60_000);
    let fires = fixture.builtins.tick_cron(true).expect("tick");
    assert_eq!(fires.len(), 1);
    assert_eq!(fires[0].coalesced_count, 5);
    assert!(fixture.builtins.tick_cron(false).expect("busy").is_empty());

    let hyphae = snapshot.get(HYPHAE_TOOL_NAME).expect("Hyphae");
    let enabled = invoke(hyphae.as_ref(), json!({"command":"review this"})).await;
    let enabled = output_json(&enabled);
    assert_eq!(enabled["state"]["thinkingEffort"], "xhigh");
    assert_eq!(enabled["state"]["swarmMode"], "task");
    assert_eq!(enabled["submitPrompt"], "review this");
    let finished = invoke(hyphae.as_ref(), json!({"finish_task":true})).await;
    assert_eq!(output_json(&finished)["state"]["swarmMode"], "off");
}

#[derive(Default)]
struct OrderedLog(Mutex<Vec<String>>);

struct OrderedStore {
    records: Mutex<Vec<OrchestrationRecord>>,
    order: Arc<OrderedLog>,
}

impl OrchestrationStore for OrderedStore {
    fn load(&self) -> Result<Vec<OrchestrationRecord>, String> {
        Ok(self.records.lock().expect("records").clone())
    }

    fn append(&self, records: &[OrchestrationRecord]) -> Result<(), String> {
        self.records
            .lock()
            .expect("records")
            .extend_from_slice(records);
        self.order.0.lock().expect("order").extend(
            records
                .iter()
                .map(|record| format!("record:{}:{}", record.scope, record.action)),
        );
        Ok(())
    }
}

struct OrderedEvents(Arc<OrderedLog>);

impl LiveEventSink for OrderedEvents {
    fn publish(&self, event: OrchestrationEvent) {
        self.0
             .0
            .lock()
            .expect("order")
            .push(format!("event:{}:{}", event.scope, event.action));
    }
}

#[test]
fn restart_marks_missing_executors_lost_durable_before_live() {
    let artifacts = TempArtifacts::new("restart");
    let order = Arc::new(OrderedLog::default());
    let store = Arc::new(OrderedStore {
        records: Mutex::new(Vec::new()),
        order: order.clone(),
    });
    let events = Arc::new(OrderedEvents(order.clone()));
    let clock = Arc::new(TestClock::new(10));
    let ports = OrchestrationPorts::new(store, events, clock);
    let background = BackgroundRegistry::open(ports.clone()).expect("background");
    background
        .register(
            "agent-deadbeef",
            BackgroundKind::Subagent,
            "lost child",
            BackgroundMode::Detached { keep_alive: true },
            None,
        )
        .expect("register");
    order.0.lock().expect("order").clear();

    let registry = ToolRegistry::new();
    let dependencies =
        OrchestrationDependencies::new(ports, Arc::new(FakeHost::default()), artifacts.path());
    let root = OrchestrationRootConfig::new(
        "main",
        root_capabilities(),
        BTreeMap::from([("coder".to_owned(), coder_profile())]),
        "coder",
    );
    let config = OrchestrationBuiltinConfig::new(dependencies, root);
    register_orchestration_builtins(&registry, config).expect("reopen");
    let order = order.0.lock().expect("order");
    let record = order
        .iter()
        .position(|entry| entry == "record:background:reconciled_lost")
        .expect("record");
    let event = order
        .iter()
        .position(|entry| entry == "event:background:reconciled_lost")
        .expect("event");
    assert!(record < event, "{order:?}");
}
