use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[cfg(unix)]
use std::os::unix::fs::symlink;

use mycel_agent_protocol::{ExecutableToolOutput, ExecutableToolResult, ToolUpdate};
use mycel_agent_runtime::{
    register_retained_builtins, AgentId, CancellationToken, Clock, DurableSessionBuiltinState,
    GoalBudgetLimits, GoalBudgetPort, GoalBudgetSnapshot, LiveEventSink, LocalToolConfig,
    MediaCapabilities, OrchestrationEvent, OrchestrationPorts, OrchestrationRecord,
    OrchestrationStore, PortFuture, QuestionAnswer, QuestionPort, QuestionRequest,
    QuestionResponse, ReadMediaConfig, RequestId, RetainedBuiltinConfig, Runtime,
    SessionBuiltinStatePort, SessionId, SessionOptions, SkillActivation, SkillActivationPort,
    SkillKind, SkillTrigger, TodoItem, TodoStatus, ToolCallId, ToolInvocation, ToolPrepareContext,
    ToolRegistry, ToolUpdateSink,
};
use serde_json::{json, Value};

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("mycel-retained-{label}-{}", RequestId::generate()));
        fs::create_dir_all(&path).expect("temporary root");
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
struct NoUpdates;

impl ToolUpdateSink for NoUpdates {
    fn emit(&self, _update: ToolUpdate) {}
}

fn context(session_id: &SessionId, call: &str) -> ToolPrepareContext {
    ToolPrepareContext {
        session_id: session_id.clone(),
        agent_id: AgentId::main(),
        turn_id: 1,
        tool_call_id: ToolCallId::new(call).expect("call id"),
    }
}

async fn invoke(
    registry: &ToolRegistry,
    session_id: &SessionId,
    name: &str,
    arguments: Value,
) -> ExecutableToolResult {
    let tool = registry.snapshot().get(name).expect("registered tool");
    tool.validate_arguments(&arguments)
        .expect("valid arguments");
    tool.prepare(&arguments, &context(session_id, name))
        .expect("prepare");
    tool.execute(ToolInvocation {
        context: context(session_id, name),
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
        ExecutableToolOutput::Parts(_) => panic!("expected text output"),
    }
}

struct AnswerPort;

impl QuestionPort for AnswerPort {
    fn ask<'a>(
        &'a self,
        request: QuestionRequest,
    ) -> PortFuture<'a, Result<QuestionResponse, mycel_agent_runtime::PortError>> {
        Box::pin(async move {
            Ok(QuestionResponse {
                answers: vec![QuestionAnswer {
                    question_id: request.questions[0].id.clone(),
                    selected_labels: vec![request.questions[0].options[0].label.clone()],
                    text: None,
                }],
            })
        })
    }
}

struct PendingQuestionPort;

impl QuestionPort for PendingQuestionPort {
    fn ask<'a>(
        &'a self,
        _request: QuestionRequest,
    ) -> PortFuture<'a, Result<QuestionResponse, mycel_agent_runtime::PortError>> {
        Box::pin(std::future::pending())
    }
}

struct FakeSkills;

impl SkillActivationPort for FakeSkills {
    fn activate(
        &self,
        id: &str,
        arguments: &[String],
        trigger: SkillTrigger,
        _session_id: &str,
    ) -> Result<SkillActivation, String> {
        Ok(SkillActivation {
            id: id.to_owned(),
            kind: SkillKind::Inline,
            trigger,
            prompt: format!("activated:{id}:{}", arguments.join("|")),
        })
    }
}

struct FakeGoal {
    snapshot: Mutex<GoalBudgetSnapshot>,
    order: Arc<Mutex<Vec<&'static str>>>,
}

impl FakeGoal {
    fn new(snapshot: GoalBudgetSnapshot, order: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            snapshot: Mutex::new(snapshot),
            order,
        }
    }
}

impl GoalBudgetPort for FakeGoal {
    fn snapshot(&self) -> Result<GoalBudgetSnapshot, String> {
        Ok(*self.snapshot.lock().expect("goal"))
    }

    fn set_budget<'a>(
        &'a self,
        limits: GoalBudgetLimits,
    ) -> mycel_agent_runtime::BuiltinPortFuture<'a, Result<GoalBudgetSnapshot, String>> {
        Box::pin(async move {
            self.order.lock().expect("order").push("goal.durable");
            let mut snapshot = self.snapshot.lock().expect("goal");
            snapshot.limits.turn_budget = limits.turn_budget.or(snapshot.limits.turn_budget);
            snapshot.limits.token_budget = limits.token_budget.or(snapshot.limits.token_budget);
            snapshot.limits.wall_clock_budget_ms = limits
                .wall_clock_budget_ms
                .or(snapshot.limits.wall_clock_budget_ms);
            snapshot.over_budget = snapshot
                .limits
                .turn_budget
                .is_some_and(|limit| snapshot.turns_used >= limit);
            self.order.lock().expect("order").push("goal.live");
            Ok(*snapshot)
        })
    }
}

async fn session(
    root: &TempRoot,
    label: &str,
    question: Arc<dyn QuestionPort>,
) -> (Runtime, mycel_agent_runtime::SessionHandle, SessionId) {
    let runtime = Runtime::new(root.path().join("sessions"));
    let id = SessionId::new(format!("retained-{label}")).expect("session id");
    let mut options = SessionOptions::new(id.clone());
    options.question_port = Some(question);
    let session = runtime
        .create_session(options)
        .await
        .expect("create session");
    (runtime, session, id)
}

fn registry(
    session: mycel_agent_runtime::SessionHandle,
    local: LocalToolConfig,
    goal: Arc<dyn GoalBudgetPort>,
    depth: u8,
) -> ToolRegistry {
    let registry = ToolRegistry::new();
    let state: Arc<dyn SessionBuiltinStatePort> = Arc::new(session.clone());
    let media = ReadMediaConfig::new(local.clone(), MediaCapabilities::images_and_video())
        .expect("media config");
    let config = RetainedBuiltinConfig::new(session, state)
        .with_local_tools(local.clone())
        .with_plan_file(local.cwd().join("plan.md"))
        .with_skills(Arc::new(FakeSkills), depth)
        .with_goal_budget(goal)
        .with_media(media);
    register_retained_builtins(&registry, config).expect("register tools");
    registry
}

#[tokio::test]
async fn registry_is_strict_and_question_uses_the_real_session_port() {
    let root = TempRoot::new("question");
    let (_runtime, session, id) = session(&root, "question", Arc::new(AnswerPort)).await;
    let order = Arc::new(Mutex::new(Vec::new()));
    let goal = Arc::new(FakeGoal::new(GoalBudgetSnapshot::default(), order));
    let local = LocalToolConfig::new(root.path(), Vec::<PathBuf>::new()).expect("local");
    let registry = registry(session, local, goal, 0);
    let tool_snapshot = registry.snapshot();
    let names = tool_snapshot
        .definitions()
        .iter()
        .map(|definition| definition.name.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        names,
        [
            "AskUserQuestion",
            "EnterPlanMode",
            "ExitPlanMode",
            "ReadMediaFile",
            "SetGoalBudget",
            "TodoList",
            "Skill",
        ]
        .into_iter()
        .collect()
    );
    assert!(registry
        .snapshot()
        .definitions()
        .iter()
        .all(|definition| definition.parameters["additionalProperties"] == false));

    let result = invoke(
        &registry,
        &id,
        "AskUserQuestion",
        json!({"questions":[{
            "question":"Choose?",
            "header":"Choice",
            "options":[{"label":"A"},{"label":"B"}]
        }]}),
    )
    .await;
    assert!(!result.is_error);
    assert_eq!(
        serde_json::from_str::<Value>(text(&result)).expect("JSON")["answers"]["Choose?"],
        "A"
    );

    let tool = registry.snapshot().get("AskUserQuestion").expect("tool");
    assert!(tool
        .validate_arguments(&json!({"questions":[{
            "question":"same?", "options":[{"label":"A"},{"label":"A"}]
        }]}))
        .is_err());
    assert!(tool
        .validate_arguments(&json!({"questions":[],"background":true}))
        .is_err());
}

#[tokio::test]
async fn question_cancellation_drops_the_pending_port_future() {
    let root = TempRoot::new("question-cancel");
    let (_runtime, session, id) =
        session(&root, "question-cancel", Arc::new(PendingQuestionPort)).await;
    let order = Arc::new(Mutex::new(Vec::new()));
    let local = LocalToolConfig::new(root.path(), Vec::<PathBuf>::new()).expect("local");
    let registry = registry(
        session,
        local,
        Arc::new(FakeGoal::new(GoalBudgetSnapshot::default(), order)),
        0,
    );
    let tool = registry.snapshot().get("AskUserQuestion").expect("tool");
    let token = CancellationToken::new();
    let invocation = ToolInvocation {
        context: context(&id, "cancel-question"),
        arguments: json!({"questions":[{
            "question":"Wait?", "options":[{"label":"Yes"},{"label":"No"}]
        }]}),
        cancellation: token.clone(),
        updates: Arc::new(NoUpdates),
    };
    let future = tool.execute(invocation);
    tokio::pin!(future);
    tokio::task::yield_now().await;
    token.cancel();
    let error = future.await.expect_err("cancelled question");
    assert!(error.to_string().contains("cancelled"));
}

#[tokio::test]
async fn canonical_todos_and_plan_mode_replay_without_parallel_state() {
    let root = TempRoot::new("canonical");
    fs::write(root.path().join("plan.md"), "# plan\n\nship it\n").expect("plan");
    let (runtime, session, id) = session(&root, "canonical", Arc::new(AnswerPort)).await;
    let order = Arc::new(Mutex::new(Vec::new()));
    let local = LocalToolConfig::new(root.path(), Vec::<PathBuf>::new()).expect("local");
    let registry = registry(
        session.clone(),
        local,
        Arc::new(FakeGoal::new(GoalBudgetSnapshot::default(), order)),
        0,
    );
    let result = invoke(
        &registry,
        &id,
        "TodoList",
        json!({"todos":[{"title":"port it","status":"in_progress"}]}),
    )
    .await;
    assert!(!result.is_error);
    assert!(
        !invoke(&registry, &id, "EnterPlanMode", json!({}))
            .await
            .is_error
    );
    assert!(session.snapshot().await.state.plan_mode);
    assert!(
        !invoke(&registry, &id, "ExitPlanMode", json!({}))
            .await
            .is_error
    );
    assert!(!session.snapshot().await.state.plan_mode);
    session.close().await.expect("close");

    let resumed = runtime
        .resume_session(SessionOptions::new(id.clone()))
        .await
        .expect("resume");
    let snapshot = SessionBuiltinStatePort::snapshot(&resumed)
        .await
        .expect("builtin snapshot");
    assert_eq!(snapshot.todos[0].title, "port it");
    assert!(!snapshot.plan_mode);
    assert_eq!(
        snapshot.plan_file,
        Some(fs::canonicalize(root.path().join("plan.md")).expect("canonical plan"))
    );
}

#[derive(Default)]
struct OrderedStore {
    records: Mutex<Vec<OrchestrationRecord>>,
    order: Arc<Mutex<Vec<&'static str>>>,
}

impl OrchestrationStore for OrderedStore {
    fn load(&self) -> Result<Vec<OrchestrationRecord>, String> {
        Ok(self.records.lock().expect("records").clone())
    }

    fn append(&self, records: &[OrchestrationRecord]) -> Result<(), String> {
        self.order.lock().expect("order").push("durable");
        self.records
            .lock()
            .expect("records")
            .extend_from_slice(records);
        Ok(())
    }
}

struct OrderedEvents(Arc<Mutex<Vec<&'static str>>>);

impl LiveEventSink for OrderedEvents {
    fn publish(&self, _event: OrchestrationEvent) {
        self.0.lock().expect("order").push("live");
    }
}

struct FixedClock;

impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        7
    }
}

#[tokio::test]
async fn durable_state_records_before_live_and_rejects_invalid_todos() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let store = Arc::new(OrderedStore {
        records: Mutex::new(Vec::new()),
        order: Arc::clone(&order),
    });
    let ports = OrchestrationPorts::new(
        store.clone(),
        Arc::new(OrderedEvents(Arc::clone(&order))),
        Arc::new(FixedClock),
    );
    let state = DurableSessionBuiltinState::open("ordered", ports.clone()).expect("state");
    state
        .replace_todos(vec![TodoItem {
            title: "one".to_owned(),
            status: TodoStatus::InProgress,
        }])
        .await
        .expect("replace");
    assert_eq!(*order.lock().expect("order"), ["durable", "live"]);
    let restored = DurableSessionBuiltinState::open("ordered", ports).expect("restore");
    assert_eq!(restored.snapshot().await.expect("snapshot").todos.len(), 1);
    assert!(restored
        .replace_todos(vec![
            TodoItem {
                title: "one".to_owned(),
                status: TodoStatus::InProgress,
            },
            TodoItem {
                title: "two".to_owned(),
                status: TodoStatus::InProgress,
            },
        ])
        .await
        .is_err());
}

#[tokio::test]
async fn skill_activation_is_durable_context_and_recursion_is_bounded() {
    let root = TempRoot::new("skill");
    let (_runtime, session, id) = session(&root, "skill", Arc::new(AnswerPort)).await;
    let local = LocalToolConfig::new(root.path(), Vec::<PathBuf>::new()).expect("local");
    let skill_registry = registry(
        session.clone(),
        local.clone(),
        Arc::new(FakeGoal::new(
            GoalBudgetSnapshot::default(),
            Arc::new(Mutex::new(Vec::new())),
        )),
        0,
    );
    let result = invoke(
        &skill_registry,
        &id,
        "Skill",
        json!({"skill":"review","args":"one 'two three' \"\""}),
    )
    .await;
    assert!(!result.is_error);
    let snapshot = session.snapshot().await;
    let entry = snapshot
        .state
        .context
        .history()
        .last()
        .expect("skill context");
    assert_eq!(
        entry.message.content[0].as_text(),
        Some("activated:review:one|two three|")
    );
    assert!(matches!(
        entry.origin,
        Some(mycel_agent_protocol::PromptOrigin::SkillActivation { .. })
    ));

    let deep = registry(
        session,
        local,
        Arc::new(FakeGoal::new(
            GoalBudgetSnapshot::default(),
            Arc::new(Mutex::new(Vec::new())),
        )),
        3,
    );
    let result = invoke(&deep, &id, "Skill", json!({"skill":"review"})).await;
    assert!(result.is_error);
    assert!(text(&result).contains("depth limit"));
}

#[tokio::test]
async fn exhausted_goal_budget_stops_batch_and_turn_after_durable_update() {
    let root = TempRoot::new("goal");
    let (_runtime, session, id) = session(&root, "goal", Arc::new(AnswerPort)).await;
    let order = Arc::new(Mutex::new(Vec::new()));
    let goal = Arc::new(FakeGoal::new(
        GoalBudgetSnapshot {
            has_goal: true,
            turns_used: 1,
            ..GoalBudgetSnapshot::default()
        },
        Arc::clone(&order),
    ));
    let local = LocalToolConfig::new(root.path(), Vec::<PathBuf>::new()).expect("local");
    let registry = registry(session, local, goal, 0);
    let tool = registry
        .snapshot()
        .get("SetGoalBudget")
        .expect("budget tool");
    let arguments = json!({"value":0.6,"unit":"turns"});
    let spec = tool
        .prepare(&arguments, &context(&id, "budget"))
        .expect("prepare");
    assert!(spec.stop_batch_after_this);
    let result = invoke(&registry, &id, "SetGoalBudget", arguments).await;
    assert!(result.stop_turn);
    assert_eq!(*order.lock().expect("order"), ["goal.durable", "goal.live"]);
}

#[tokio::test]
async fn media_is_sniffed_bounded_and_confined_without_secret_leaks() {
    let root = TempRoot::new("media");
    fs::write(root.path().join("pixel.bin"), b"\x89PNG\r\n\x1a\nrest").expect("png");
    fs::write(root.path().join("too-big.png"), b"\x89PNG\r\n\x1a\nX").expect("large png");
    fs::write(root.path().join("not-video.bin"), b"\0\0\0\x18ftypavifrest").expect("avif marker");
    fs::write(root.path().join(".env.local"), b"SECRET_IMAGE_BYTES").expect("secret");
    let outside = TempRoot::new("media-outside");
    fs::write(
        outside.path().join("outside.png"),
        b"\x89PNG\r\n\x1a\nsecret",
    )
    .expect("outside");
    #[cfg(unix)]
    symlink(
        outside.path().join("outside.png"),
        root.path().join("escape.png"),
    )
    .expect("symlink");
    let (_runtime, session, id) = session(&root, "media", Arc::new(AnswerPort)).await;
    let local = LocalToolConfig::new(root.path(), Vec::<PathBuf>::new()).expect("local");
    let registry = registry(
        session.clone(),
        local.clone(),
        Arc::new(FakeGoal::new(
            GoalBudgetSnapshot::default(),
            Arc::new(Mutex::new(Vec::new())),
        )),
        0,
    );
    let result = invoke(&registry, &id, "ReadMediaFile", json!({"path":"pixel.bin"})).await;
    let ExecutableToolOutput::Parts(parts) = result.output else {
        panic!("expected media parts");
    };
    assert!(matches!(
        parts[1],
        mycel_agent_protocol::ContentPart::ImageUrl { .. }
    ));

    let tool = registry
        .snapshot()
        .get("ReadMediaFile")
        .expect("media tool");
    let secret = tool.prepare(&json!({"path":".env.local"}), &context(&id, "secret"));
    let message = secret.expect_err("sensitive path").to_string();
    assert!(!message.contains("SECRET_IMAGE_BYTES"));
    #[cfg(unix)]
    {
        let escaped = tool.prepare(&json!({"path":"escape.png"}), &context(&id, "escape"));
        assert!(escaped
            .expect_err("symlink escape")
            .to_string()
            .contains("symlink"));
    }
    let outside_result = tool.prepare(
        &json!({"path":outside.path().join("outside.png").to_string_lossy()}),
        &context(&id, "outside"),
    );
    assert!(outside_result
        .expect_err("outside root")
        .to_string()
        .contains("outside"));

    let avif = invoke(
        &registry,
        &id,
        "ReadMediaFile",
        json!({"path":"not-video.bin"}),
    )
    .await;
    assert!(avif.is_error);
    assert!(text(&avif).contains("unsupported"));

    let limited_registry = ToolRegistry::new();
    let limited_media = ReadMediaConfig::new(local, MediaCapabilities::images())
        .expect("media")
        .with_max_bytes(8)
        .expect("limit");
    let state: Arc<dyn SessionBuiltinStatePort> = Arc::new(session.clone());
    register_retained_builtins(
        &limited_registry,
        RetainedBuiltinConfig::new(session, state).with_media(limited_media),
    )
    .expect("limited registry");
    let oversized = invoke(
        &limited_registry,
        &id,
        "ReadMediaFile",
        json!({"path":"too-big.png"}),
    )
    .await;
    assert!(oversized.is_error);
    assert!(text(&oversized).contains("byte limit"));
}
