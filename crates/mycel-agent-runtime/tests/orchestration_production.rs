use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use mycel_agent_protocol::{
    ContentPart, FinishReason, GenerateResult, Message, OptionalNullable, PermissionMode,
    ProviderError, ProviderErrorKind, ProviderRequest, TokenUsage,
};
use mycel_agent_runtime::{
    CancellationToken, CapabilitySet, Clock, FilesystemOrchestrationStore, HookRunner,
    LiveEventSink, NativeAgentOperation, NativeAgentOutputSink, NativeAgentRequest,
    NativeChildAgentHost, NativeChildBoard, NativeChildHostDependencies, NativeChildHostOptions,
    NativeChildState, NativeChildStatus, NativeSessionOptionsFactory, NativeSubagentHost,
    NativeTurnEngineFactory, NativeTurnRuntime, OrchestrationEvent, OrchestrationPorts,
    OrchestrationRecord, OrchestrationStore, Runtime, SessionId, SessionOptions, ToolRegistry,
    ToolScheduler, TurnEngine, TurnEngineConfig, TurnProvider, TurnProviderFuture, WorkerProfile,
};
use serde_json::json;
use tokio::sync::Notify;

static TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct TestClock(u64);

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0
    }
}

#[derive(Default)]
struct TestEvents;

impl LiveEventSink for TestEvents {
    fn publish(&self, _event: OrchestrationEvent) {}
}

struct OrderingEvents {
    store: Arc<FilesystemOrchestrationStore>,
    events: Mutex<Vec<OrchestrationEvent>>,
}

impl LiveEventSink for OrderingEvents {
    fn publish(&self, event: OrchestrationEvent) {
        let records = self.store.load().expect("read before live event");
        assert_eq!(
            records.last().map(|record| record.action.as_str()),
            Some(event.action.as_str())
        );
        self.events.lock().expect("events").push(event);
    }
}

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "mycel-orchestration-production-{name}-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temporary root");
        Self(path)
    }
}

impl AsRef<Path> for TempRoot {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn filesystem_store_is_private_session_scoped_and_repairs_only_a_truncated_tail() {
    let root = TempRoot::new("store");
    let first_id = SessionId::new("first").expect("first id");
    let second_id = SessionId::new("second").expect("second id");
    let first = FilesystemOrchestrationStore::open(&root, &first_id).expect("first store");
    let second = FilesystemOrchestrationStore::open(&root, &second_id).expect("second store");
    let record = OrchestrationRecord {
        scope: "goal".to_owned(),
        action: "created".to_owned(),
        entity_id: Some("goal-1".to_owned()),
        at_ms: 10,
        state: json!({"current": null}),
        detail: json!({}),
    };
    first.append(std::slice::from_ref(&record)).expect("append");
    assert_eq!(first.load().expect("load"), [record]);
    assert!(second.load().expect("other session").is_empty());

    let mut file = OpenOptions::new()
        .append(true)
        .open(first.path())
        .expect("open log");
    file.write_all(br#"{"scope":"unfinished""#)
        .expect("write truncated tail");
    file.sync_all().expect("sync tail");
    drop(file);
    let reopened = FilesystemOrchestrationStore::open(&root, &first_id).expect("repair tail");
    assert_eq!(reopened.load().expect("repaired load").len(), 1);
    assert!(std::fs::read(reopened.path())
        .expect("read repaired log")
        .ends_with(b"\n"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(reopened.path())
                .expect("file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(reopened.path().parent().expect("session directory"))
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    let mut file = OpenOptions::new()
        .append(true)
        .open(reopened.path())
        .expect("open repaired log");
    file.write_all(b"not-json\n").expect("write corrupt record");
    file.sync_all().expect("sync corruption");
    drop(file);
    assert!(FilesystemOrchestrationStore::open(&root, &first_id).is_err());
}

struct TestSessions;

impl NativeSessionOptionsFactory for TestSessions {
    fn build(
        &self,
        context: &mycel_agent_runtime::NativeChildContext,
    ) -> Result<SessionOptions, String> {
        let mut options = SessionOptions::new(context.session_id.clone());
        options.initial_permission_mode = PermissionMode::Auto;
        Ok(options)
    }
}

struct PendingProvider {
    entered: Notify,
    calls: AtomicUsize,
}

impl PendingProvider {
    fn new() -> Self {
        Self {
            entered: Notify::new(),
            calls: AtomicUsize::new(0),
        }
    }
}

impl TurnProvider for PendingProvider {
    fn name(&self) -> &str {
        "pending"
    }

    fn model(&self) -> &str {
        "pending-model"
    }

    fn complete<'a>(
        &'a self,
        _request: ProviderRequest,
        cancellation: CancellationToken,
    ) -> TurnProviderFuture<'a> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.entered.notify_one();
        Box::pin(async move {
            cancellation.cancelled().await;
            Err(ProviderError::new(
                ProviderErrorKind::Cancelled,
                "cancelled by test",
            ))
        })
    }
}

struct ImmediateProvider {
    text: String,
    calls: AtomicUsize,
}

impl ImmediateProvider {
    fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            calls: AtomicUsize::new(0),
        }
    }
}

impl TurnProvider for ImmediateProvider {
    fn name(&self) -> &str {
        "immediate"
    }

    fn model(&self) -> &str {
        "immediate-model"
    }

    fn complete<'a>(
        &'a self,
        _request: ProviderRequest,
        cancellation: CancellationToken,
    ) -> TurnProviderFuture<'a> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let response = completed_response(&self.text);
        Box::pin(async move {
            if cancellation.is_cancelled() {
                Err(ProviderError::new(
                    ProviderErrorKind::Cancelled,
                    "cancelled by test",
                ))
            } else {
                Ok(response)
            }
        })
    }
}

struct TestTurns {
    provider: Arc<dyn TurnProvider>,
    capabilities: CapabilitySet,
}

impl NativeTurnEngineFactory for TestTurns {
    fn build(
        &self,
        _context: &mycel_agent_runtime::NativeChildContext,
    ) -> Result<NativeTurnRuntime, String> {
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
            effective_capabilities: self.capabilities.clone(),
            system_prompt: "You are a bounded native child.".to_owned(),
            thinking_effort: None,
            max_completion_tokens: Some(128),
            metadata: BTreeMap::new(),
        })
    }
}

#[derive(Default)]
struct NullOutput;

impl NativeAgentOutputSink for NullOutput {
    fn append(&self, _text: &str) -> Result<(), String> {
        Ok(())
    }
}

fn child_capabilities() -> CapabilitySet {
    CapabilitySet {
        tools: BTreeSet::new(),
        filesystem_roots: BTreeSet::new(),
        network: false,
        can_spawn_subagents: false,
        can_swarm: false,
        can_workflow: false,
    }
}

fn root_capabilities() -> CapabilitySet {
    CapabilitySet {
        can_spawn_subagents: true,
        ..child_capabilities()
    }
}

fn child_request(agent_id: &str) -> NativeAgentRequest {
    NativeAgentRequest {
        agent_id: agent_id.to_owned(),
        parent_agent_id: "root".to_owned(),
        description: "native child test".to_owned(),
        prompt: "perform bounded work".to_owned(),
        operation: NativeAgentOperation::Spawn {
            profile: WorkerProfile {
                name: "bounded".to_owned(),
                capabilities: child_capabilities(),
                allow_delegation: false,
            },
        },
        cancellation: CancellationToken::new(),
        output: Arc::new(NullOutput),
    }
}

#[tokio::test]
async fn native_host_execute_future_is_send_and_stop_cancels_without_holding_state_locks() {
    let root = TempRoot::new("native-cancel");
    let runtime = Runtime::new(root.as_ref().join("sessions"));
    let store_root = root.as_ref().join("orchestration");
    let session_id = SessionId::new("root-session").expect("root session id");
    let store = Arc::new(
        FilesystemOrchestrationStore::open(&store_root, &session_id).expect("orchestration store"),
    );
    let events = Arc::new(TestEvents);
    let ports = OrchestrationPorts::new(store, events, Arc::new(TestClock(100)));
    let provider = Arc::new(PendingProvider::new());
    let host = NativeChildAgentHost::open(
        NativeChildHostDependencies {
            runtime,
            ports,
            sessions: Arc::new(TestSessions),
            turns: Arc::new(TestTurns {
                provider: provider.clone(),
                capabilities: child_capabilities(),
            }),
        },
        NativeChildHostOptions::new("root-session", "root", root_capabilities()),
    )
    .expect("host");
    let request = child_request("child-1");

    // `tokio::spawn` is the compile-time Send regression for the boxed host future.
    let running_host = host.clone();
    let execution = tokio::spawn(async move { running_host.execute(request).await });
    tokio::time::timeout(Duration::from_secs(2), provider.entered.notified())
        .await
        .expect("provider entered");
    tokio::time::timeout(
        Duration::from_secs(2),
        host.stop("child-1".to_owned(), "test stop".to_owned()),
    )
    .await
    .expect("stop timeout")
    .expect("stop");
    let result = tokio::time::timeout(Duration::from_secs(2), execution)
        .await
        .expect("execution timeout")
        .expect("execution task");
    assert!(result.is_err());
    assert_eq!(provider.calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        host.snapshot().children["child-1"].status,
        NativeChildStatus::Cancelled
    );
    assert!(host.active_agent_ids().is_empty());

    let request = child_request("child-2");
    let cancellation = request.cancellation.clone();
    let running_host = host.clone();
    let execution = tokio::spawn(async move { running_host.execute(request).await });
    tokio::time::timeout(Duration::from_secs(2), provider.entered.notified())
        .await
        .expect("second provider entered");
    cancellation.cancel();
    assert!(tokio::time::timeout(Duration::from_secs(2), execution)
        .await
        .expect("external cancellation timeout")
        .expect("external cancellation task")
        .is_err());
    assert_eq!(
        host.snapshot().children["child-2"].status,
        NativeChildStatus::Cancelled
    );
}

#[tokio::test]
async fn native_host_runs_and_resumes_real_sessions_with_bounded_output() {
    let root = TempRoot::new("native-complete");
    let session_id = SessionId::new("root-session").expect("root session id");
    let store = Arc::new(
        FilesystemOrchestrationStore::open(root.as_ref().join("orchestration"), &session_id)
            .expect("orchestration store"),
    );
    let events = Arc::new(OrderingEvents {
        store: store.clone(),
        events: Mutex::new(Vec::new()),
    });
    let provider = Arc::new(ImmediateProvider::new("0123456789abcdef"));
    let mut options = NativeChildHostOptions::new("root-session", "root", root_capabilities());
    options.max_output_chars = 8;
    let host = NativeChildAgentHost::open(
        NativeChildHostDependencies {
            runtime: Runtime::new(root.as_ref().join("sessions")),
            ports: OrchestrationPorts::new(store.clone(), events.clone(), Arc::new(TestClock(10))),
            sessions: Arc::new(TestSessions),
            turns: Arc::new(TestTurns {
                provider: provider.clone(),
                capabilities: child_capabilities(),
            }),
        },
        options,
    )
    .expect("host");

    let first = host
        .execute(child_request("child-complete"))
        .await
        .expect("execute child");
    assert_eq!(first.output, "01234567");
    assert_eq!(
        host.snapshot().children["child-complete"].status,
        NativeChildStatus::Completed
    );

    let mut resume = child_request("child-complete");
    resume.operation = NativeAgentOperation::Resume {
        agent_id: "child-complete".to_owned(),
    };
    let second = host.execute(resume).await.expect("resume child");
    assert_eq!(second.output, "01234567");
    assert_eq!(provider.calls.load(Ordering::Relaxed), 2);
    assert_eq!(
        store
            .load()
            .expect("records")
            .iter()
            .map(|record| record.action.as_str())
            .collect::<Vec<_>>(),
        ["spawned", "completed", "resumed", "completed"]
    );
    assert_eq!(events.events.lock().expect("events").len(), 4);
}

#[tokio::test]
async fn native_host_rejects_factory_capability_escalation_before_provider_dispatch() {
    let root = TempRoot::new("native-escalation");
    let session_id = SessionId::new("root-session").expect("root session id");
    let store = Arc::new(
        FilesystemOrchestrationStore::open(root.as_ref().join("orchestration"), &session_id)
            .expect("orchestration store"),
    );
    let provider = Arc::new(ImmediateProvider::new("must not run"));
    let host = NativeChildAgentHost::open(
        NativeChildHostDependencies {
            runtime: Runtime::new(root.as_ref().join("sessions")),
            ports: OrchestrationPorts::new(store, Arc::new(TestEvents), Arc::new(TestClock(10))),
            sessions: Arc::new(TestSessions),
            turns: Arc::new(TestTurns {
                provider: provider.clone(),
                capabilities: root_capabilities(),
            }),
        },
        NativeChildHostOptions::new("root-session", "root", root_capabilities()),
    )
    .expect("host");

    let error = host
        .execute(child_request("child-escalation"))
        .await
        .expect_err("reject escalation");
    assert!(error.contains("escalated capabilities"));
    assert_eq!(provider.calls.load(Ordering::Relaxed), 0);
    assert_eq!(
        host.snapshot().children["child-escalation"].status,
        NativeChildStatus::Failed
    );
}

#[test]
fn native_host_startup_reconciliation_marks_persisted_running_children_lost_before_live() {
    let root = TempRoot::new("native-lost");
    let session_id = SessionId::new("root-session").expect("root session id");
    let store = Arc::new(
        FilesystemOrchestrationStore::open(root.as_ref().join("orchestration"), &session_id)
            .expect("orchestration store"),
    );
    let child_session = SessionId::new("child-persisted").expect("child session id");
    let profile = WorkerProfile {
        name: "bounded".to_owned(),
        capabilities: child_capabilities(),
        allow_delegation: false,
    };
    let board = NativeChildBoard {
        children: BTreeMap::from([(
            "orphan".to_owned(),
            NativeChildState {
                agent_id: "orphan".to_owned(),
                parent_agent_id: "root".to_owned(),
                session_id: child_session,
                profile,
                depth: 1,
                status: NativeChildStatus::Running,
                started_at_ms: 10,
                ended_at_ms: None,
                reason: None,
            },
        )]),
    };
    store
        .append(&[OrchestrationRecord {
            scope: "native-child-host".to_owned(),
            action: "spawned".to_owned(),
            entity_id: Some("orphan".to_owned()),
            at_ms: 10,
            state: serde_json::to_value(board).expect("serialize board"),
            detail: json!({}),
        }])
        .expect("seed running child");
    let events = Arc::new(OrderingEvents {
        store: store.clone(),
        events: Mutex::new(Vec::new()),
    });
    let provider = Arc::new(ImmediateProvider::new("unused"));
    let host = NativeChildAgentHost::open(
        NativeChildHostDependencies {
            runtime: Runtime::new(root.as_ref().join("sessions")),
            ports: OrchestrationPorts::new(store.clone(), events.clone(), Arc::new(TestClock(20))),
            sessions: Arc::new(TestSessions),
            turns: Arc::new(TestTurns {
                provider,
                capabilities: child_capabilities(),
            }),
        },
        NativeChildHostOptions::new("root-session", "root", root_capabilities()),
    )
    .expect("reconcile host");

    assert_eq!(
        host.snapshot().children["orphan"].status,
        NativeChildStatus::Lost
    );
    assert_eq!(
        store.load().expect("records").last().expect("last").action,
        "reconciled_lost"
    );
    assert_eq!(events.events.lock().expect("events").len(), 1);
}

fn completed_response(text: &str) -> GenerateResult {
    GenerateResult {
        id: Some("native-test".to_owned()),
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
