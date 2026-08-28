use std::{
    collections::{BTreeMap, VecDeque},
    future::pending,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use mycel_agent_protocol::{AgentEvent, McpCommonConfig, McpServerConfig, McpServerStatus};
use mycel_agent_protocol::{ExecutableToolOutput, ToolUpdate};
use mycel_agent_runtime::{
    AgentId, CancellationToken, McpConnectedTransport, McpConnectionPurpose, McpEventSink,
    McpFuture, McpHttpConnectRequest, McpPeer, McpProtocolEra, McpRequest, McpRequestError,
    McpRuntime, McpRuntimeError, McpRuntimeOptions, McpRuntimeRecord, McpStdioConnectRequest,
    McpTransportConnector, McpTransportError, McpTransportEvent, McpTransportEvents, SessionId,
    ToolCallId, ToolInvocation, ToolPrepareContext, ToolRegistry, ToolUpdateSink,
};
use serde_json::{json, Value};

const MODERN: &str = "2026-07-28";
const LEGACY: &str = "2025-11-25";

enum PeerStep {
    Ready(Result<Value, McpRequestError>),
    WaitForCancellation,
}

#[derive(Default)]
struct ScriptedPeer {
    steps: Mutex<VecDeque<PeerStep>>,
    requests: Mutex<Vec<McpRequest>>,
    request_cancellations: Mutex<Vec<CancellationToken>>,
    notifications: Mutex<Vec<McpRequest>>,
    close_count: Mutex<usize>,
}

impl ScriptedPeer {
    fn with_steps(steps: impl IntoIterator<Item = PeerStep>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into_iter().collect()),
            ..Self::default()
        })
    }

    fn requests(&self) -> Vec<McpRequest> {
        lock(&self.requests).clone()
    }
}

impl McpPeer for ScriptedPeer {
    fn request<'a>(
        &'a self,
        request: McpRequest,
        cancellation: &'a CancellationToken,
    ) -> McpFuture<'a, Result<Value, McpRequestError>> {
        lock(&self.requests).push(request);
        lock(&self.request_cancellations).push(cancellation.clone());
        let step = lock(&self.steps).pop_front().unwrap_or_else(|| {
            PeerStep::Ready(Err(McpRequestError::Transport(McpTransportError::Failed(
                "unexpected request".to_owned(),
            ))))
        });
        Box::pin(async move {
            match step {
                PeerStep::Ready(result) => result,
                PeerStep::WaitForCancellation => {
                    cancellation.cancelled().await;
                    Err(McpRequestError::Transport(McpTransportError::Cancelled))
                }
            }
        })
    }

    fn notify<'a>(&'a self, request: McpRequest) -> McpFuture<'a, Result<(), McpRequestError>> {
        lock(&self.notifications).push(request);
        Box::pin(async { Ok(()) })
    }

    fn close<'a>(&'a self) -> McpFuture<'a, Result<(), McpTransportError>> {
        *lock(&self.close_count) += 1;
        Box::pin(async { Ok(()) })
    }
}

struct PendingEvents;

impl McpTransportEvents for PendingEvents {
    fn next<'a>(&'a mut self) -> McpFuture<'a, Option<McpTransportEvent>> {
        Box::pin(pending())
    }
}

struct NullUpdates;

impl ToolUpdateSink for NullUpdates {
    fn emit(&self, _update: ToolUpdate) {}
}

struct ChannelEvents {
    receiver: tokio::sync::mpsc::UnboundedReceiver<McpTransportEvent>,
}

impl McpTransportEvents for ChannelEvents {
    fn next<'a>(&'a mut self) -> McpFuture<'a, Option<McpTransportEvent>> {
        Box::pin(async move { self.receiver.recv().await })
    }
}

struct Connection {
    peer: Arc<dyn McpPeer>,
    events: Box<dyn McpTransportEvents>,
}

#[derive(Default)]
struct ScriptedConnector {
    stdio: Mutex<VecDeque<Connection>>,
    http: Mutex<VecDeque<Connection>>,
    stdio_requests: Mutex<Vec<McpStdioConnectRequest>>,
    http_requests: Mutex<Vec<McpHttpConnectRequest>>,
}

impl ScriptedConnector {
    fn push_http(&self, peer: Arc<dyn McpPeer>, events: Box<dyn McpTransportEvents>) {
        lock(&self.http).push_back(Connection { peer, events });
    }

    fn push_stdio(&self, peer: Arc<dyn McpPeer>, events: Box<dyn McpTransportEvents>) {
        lock(&self.stdio).push_back(Connection { peer, events });
    }
}

impl McpTransportConnector for ScriptedConnector {
    fn connect_stdio<'a>(
        &'a self,
        request: McpStdioConnectRequest,
        _cancellation: &'a CancellationToken,
    ) -> McpFuture<'a, Result<McpConnectedTransport, McpTransportError>> {
        lock(&self.stdio_requests).push(request);
        let connection = lock(&self.stdio).pop_front();
        Box::pin(async move { connection.map(connected).ok_or(McpTransportError::Closed) })
    }

    fn connect_streamable_http<'a>(
        &'a self,
        request: McpHttpConnectRequest,
        _cancellation: &'a CancellationToken,
    ) -> McpFuture<'a, Result<McpConnectedTransport, McpTransportError>> {
        lock(&self.http_requests).push(request);
        let connection = lock(&self.http).pop_front();
        Box::pin(async move { connection.map(connected).ok_or(McpTransportError::Closed) })
    }
}

fn connected(connection: Connection) -> McpConnectedTransport {
    McpConnectedTransport {
        peer: connection.peer,
        events: connection.events,
    }
}

#[derive(Default)]
struct TraceSink {
    trace: Mutex<Vec<String>>,
    records: Mutex<Vec<McpRuntimeRecord>>,
}

impl TraceSink {
    fn trace(&self) -> Vec<String> {
        lock(&self.trace).clone()
    }

    fn records(&self) -> Vec<McpRuntimeRecord> {
        lock(&self.records).clone()
    }
}

impl McpEventSink for TraceSink {
    fn persist<'a>(&'a self, record: McpRuntimeRecord) -> McpFuture<'a, Result<(), String>> {
        Box::pin(async move {
            lock(&self.trace).push(format!("persist:{}", record.record_type));
            lock(&self.records).push(record);
            Ok(())
        })
    }

    fn publish(&self, event: AgentEvent) {
        let kind = match event {
            AgentEvent::McpServerStatus { .. } => "status",
            AgentEvent::ToolListUpdated { .. } => "tools",
            _ => "other",
        };
        lock(&self.trace).push(format!("publish:{kind}"));
    }
}

#[tokio::test]
async fn modern_http_connect_registers_tools_and_persists_before_live() {
    let connector = Arc::new(ScriptedConnector::default());
    let peer = ScriptedPeer::with_steps([
        ready(discover(true)),
        ready(json!({
            "tools":[tool("weather")],
            "ttlMs":1500,
            "cacheScope":"private"
        })),
        PeerStep::WaitForCancellation,
    ]);
    connector.push_http(peer.clone(), Box::new(PendingEvents));
    let sink = Arc::new(TraceSink::default());
    let runtime = runtime(connector, sink.clone(), Duration::from_secs(1));

    let entry = runtime
        .connect("weather-server", http_config(), &CancellationToken::new())
        .await
        .expect("connect");

    assert_eq!(entry.status, McpServerStatus::Connected);
    assert_eq!(entry.era, Some(McpProtocolEra::Modern));
    assert_eq!(entry.protocol_version.as_deref(), Some(MODERN));
    assert_eq!(entry.tool_count, 1);
    assert_eq!(
        runtime.registry().snapshot().definitions()[0].name,
        "mcp__weather-server__weather"
    );
    let requests = peer.requests();
    assert_eq!(requests[0].method, "server/discover");
    assert_eq!(requests[1].method, "tools/list");
    assert!(requests[0].params.get("_meta").is_some());
    assert_eq!(
        requests[0].http_headers.get("MCP-Protocol-Version"),
        Some(&MODERN.to_owned())
    );
    assert!(!requests[0].http_headers.contains_key("Mcp-Session-Id"));
    assert!(lock(&peer.notifications).is_empty());

    let records = sink.records();
    let discovery = records
        .iter()
        .find(|record| record.record_type == "mcp.tools_discovered")
        .expect("durable discovery");
    assert_eq!(discovery.payload.get("ttlMs"), Some(&json!(1500)));
    assert_eq!(discovery.payload.get("cacheScope"), Some(&json!("private")));
    let trace = sink.trace();
    for pair in trace.as_chunks::<2>().0 {
        assert!(pair[0].starts_with("persist:"), "{trace:?}");
        assert!(pair[1].starts_with("publish:"), "{trace:?}");
    }

    eventually(|| {
        peer.requests()
            .iter()
            .any(|request| request.method == "subscriptions/listen")
    })
    .await;
}

#[tokio::test]
async fn stdio_uses_disposable_probe_then_legacy_initialize() {
    let connector = Arc::new(ScriptedConnector::default());
    let probe = ScriptedPeer::with_steps([PeerStep::Ready(Err(McpRequestError::JsonRpc {
        code: -32601,
        message: "method not found".to_owned(),
        data: None,
        http_status: None,
    }))]);
    let session = ScriptedPeer::with_steps([
        ready(json!({
            "protocolVersion":LEGACY,
            "capabilities":{"tools":{"listChanged":false}},
            "serverInfo":{"name":"legacy","version":"1"}
        })),
        ready(json!({"tools":[tool("legacy-tool")]})),
    ]);
    connector.push_stdio(probe.clone(), Box::new(PendingEvents));
    connector.push_stdio(session.clone(), Box::new(PendingEvents));
    let runtime = runtime(
        connector.clone(),
        Arc::new(TraceSink::default()),
        Duration::from_secs(1),
    );

    let entry = runtime
        .connect("legacy", stdio_config(), &CancellationToken::new())
        .await
        .expect("connect");

    assert_eq!(entry.era, Some(McpProtocolEra::Legacy));
    assert_eq!(entry.protocol_version.as_deref(), Some(LEGACY));
    let purposes: Vec<_> = lock(&connector.stdio_requests)
        .iter()
        .map(|request| request.purpose)
        .collect();
    assert_eq!(
        purposes,
        vec![McpConnectionPurpose::Probe, McpConnectionPurpose::Session]
    );
    assert_eq!(session.requests()[0].method, "initialize");
    assert_eq!(session.requests()[1].method, "tools/list");
    let notifications = lock(&session.notifications);
    assert_eq!(notifications.len(), 1);
    assert_eq!(notifications[0].method, "notifications/initialized");
}

#[tokio::test]
async fn authentication_and_server_errors_do_not_trigger_legacy_http() {
    for status in [401, 403, 500] {
        let connector = Arc::new(ScriptedConnector::default());
        let peer = ScriptedPeer::with_steps([PeerStep::Ready(Err(McpRequestError::Http {
            status,
            message: "denied".to_owned(),
        }))]);
        connector.push_http(peer.clone(), Box::new(PendingEvents));
        let runtime = runtime(
            connector,
            Arc::new(TraceSink::default()),
            Duration::from_secs(1),
        );
        let entry = runtime
            .connect("server", http_config(), &CancellationToken::new())
            .await
            .expect("isolated server failure");
        assert_eq!(entry.status, McpServerStatus::Failed);
        assert_eq!(
            peer.requests()
                .iter()
                .map(|request| request.method.as_str())
                .collect::<Vec<_>>(),
            vec!["server/discover"]
        );
        assert!(lock(&peer.notifications).is_empty());
    }
}

#[tokio::test]
async fn caller_cancellation_reaches_the_in_flight_transport_request() {
    let connector = Arc::new(ScriptedConnector::default());
    let peer = ScriptedPeer::with_steps([PeerStep::WaitForCancellation]);
    connector.push_http(peer.clone(), Box::new(PendingEvents));
    let runtime = runtime(
        connector,
        Arc::new(TraceSink::default()),
        Duration::from_secs(10),
    );
    let cancellation = CancellationToken::new();
    let task_cancel = cancellation.clone();
    let task =
        tokio::spawn(async move { runtime.connect("server", http_config(), &task_cancel).await });
    eventually(|| !lock(&peer.request_cancellations).is_empty()).await;
    cancellation.cancel();
    let error = task
        .await
        .expect("join")
        .expect_err("caller cancellation must escape isolation");
    assert!(matches!(error, McpRuntimeError::Cancelled));
    assert!(lock(&peer.request_cancellations)[0].is_cancelled());
}

#[tokio::test]
async fn refresh_atomically_replaces_the_server_tool_set() {
    let connector = Arc::new(ScriptedConnector::default());
    let peer = ScriptedPeer::with_steps([
        ready(discover(false)),
        ready(json!({"tools":[tool("old")]})),
        ready(json!({"tools":[tool("new")]})),
    ]);
    connector.push_http(peer, Box::new(PendingEvents));
    let runtime = runtime(
        connector,
        Arc::new(TraceSink::default()),
        Duration::from_secs(1),
    );
    runtime
        .connect("server", http_config(), &CancellationToken::new())
        .await
        .expect("connect");
    runtime
        .refresh("server", &CancellationToken::new())
        .await
        .expect("refresh");
    let names: Vec<_> = runtime
        .registry()
        .snapshot()
        .definitions()
        .iter()
        .map(|definition| definition.name.clone())
        .collect();
    assert_eq!(names, vec!["mcp__server__new".to_owned()]);
}

#[tokio::test]
async fn registered_tool_calls_the_remote_name_with_modern_parameter_headers() {
    let connector = Arc::new(ScriptedConnector::default());
    let peer = ScriptedPeer::with_steps([
        ready(discover(false)),
        ready(json!({
            "tools":[{
                "name":"weather",
                "description":"weather lookup",
                "inputSchema":{
                    "type":"object",
                    "properties":{
                        "region":{"type":"string","x-mcp-header":"Region"}
                    },
                    "required":["region"],
                    "additionalProperties":false
                }
            }]
        })),
        ready(json!({
            "resultType":"complete",
            "content":[{"type":"text","text":"sunny"}],
            "isError":false
        })),
    ]);
    connector.push_http(peer.clone(), Box::new(PendingEvents));
    let runtime = runtime(
        connector,
        Arc::new(TraceSink::default()),
        Duration::from_secs(1),
    );
    runtime
        .connect("server", http_config(), &CancellationToken::new())
        .await
        .expect("connect");
    let tool = runtime
        .registry()
        .snapshot()
        .get("mcp__server__weather")
        .expect("registered tool");
    let result = tool
        .execute(ToolInvocation {
            context: ToolPrepareContext {
                session_id: SessionId::generate(),
                agent_id: AgentId::main(),
                turn_id: 1,
                tool_call_id: ToolCallId::new("call-1").expect("call id"),
            },
            arguments: json!({"region":"us-west1"}),
            cancellation: CancellationToken::new(),
            updates: Arc::new(NullUpdates),
        })
        .await
        .expect("execute");
    assert_eq!(
        result.output,
        ExecutableToolOutput::Text("sunny".to_owned())
    );
    assert!(!result.is_error);
    let call = peer
        .requests()
        .into_iter()
        .find(|request| request.method == "tools/call")
        .expect("remote call");
    assert_eq!(call.params["name"], "weather");
    assert_eq!(call.params["arguments"]["region"], "us-west1");
    assert_eq!(call.http_headers["Mcp-Method"], "tools/call");
    assert_eq!(call.http_headers["Mcp-Name"], "weather");
    assert_eq!(call.http_headers["Mcp-Param-Region"], "us-west1");
}

#[tokio::test]
async fn stable_cross_server_collision_keeps_the_first_registration() {
    let connector = Arc::new(ScriptedConnector::default());
    let first = ScriptedPeer::with_steps([
        ready(discover(false)),
        ready(json!({"tools":[tool("search")]})),
    ]);
    let second = ScriptedPeer::with_steps([
        ready(discover(false)),
        ready(json!({"tools":[tool("search")]})),
    ]);
    connector.push_http(first, Box::new(PendingEvents));
    connector.push_http(second, Box::new(PendingEvents));
    let sink = Arc::new(TraceSink::default());
    let runtime = runtime(connector, sink.clone(), Duration::from_secs(1));
    runtime
        .connect("a b", http_config(), &CancellationToken::new())
        .await
        .expect("first");
    let second_entry = runtime
        .connect("a/b", http_config(), &CancellationToken::new())
        .await
        .expect("second");

    assert_eq!(second_entry.tool_count, 0);
    let names: Vec<_> = runtime
        .registry()
        .snapshot()
        .definitions()
        .iter()
        .map(|definition| definition.name.clone())
        .collect();
    assert_eq!(names, vec!["mcp__a_b__search"]);
    let collision = sink
        .records()
        .into_iter()
        .rfind(|record| record.record_type == "mcp.tools_discovered")
        .expect("second discovery");
    assert_eq!(
        collision.payload["collisions"][0]["qualified"],
        "mcp__a_b__search"
    );
}

#[tokio::test]
async fn transport_loss_removes_tools_and_durably_marks_failed() {
    let connector = Arc::new(ScriptedConnector::default());
    let peer = ScriptedPeer::with_steps([
        ready(discover(false)),
        ready(json!({"tools":[tool("search")]})),
    ]);
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    connector.push_http(peer, Box::new(ChannelEvents { receiver }));
    let sink = Arc::new(TraceSink::default());
    let runtime = runtime(connector, sink.clone(), Duration::from_secs(1));
    runtime
        .connect("server", http_config(), &CancellationToken::new())
        .await
        .expect("connect");
    sender
        .send(McpTransportEvent::Closed {
            error: Some("connection lost".to_owned()),
        })
        .expect("close event");
    eventually(|| {
        sink.records().iter().any(|record| {
            record.record_type == "runtime.mcp_server_status"
                && record.payload["server"]["status"] == "failed"
        })
    })
    .await;
    assert!(runtime.registry().snapshot().definitions().is_empty());
    assert_eq!(
        runtime.get("server").await.expect("entry").status,
        McpServerStatus::Failed
    );
}

#[tokio::test]
async fn malformed_tool_list_fails_the_server_without_partial_registration() {
    let connector = Arc::new(ScriptedConnector::default());
    let peer = ScriptedPeer::with_steps([
        ready(discover(false)),
        ready(json!({"tools":"not-an-array"})),
    ]);
    connector.push_http(peer, Box::new(PendingEvents));
    let runtime = runtime(
        connector,
        Arc::new(TraceSink::default()),
        Duration::from_secs(1),
    );
    let entry = runtime
        .connect("server", http_config(), &CancellationToken::new())
        .await
        .expect("server failure is isolated");
    assert_eq!(entry.status, McpServerStatus::Failed);
    assert!(runtime.registry().snapshot().definitions().is_empty());
}

#[tokio::test]
async fn graceful_modern_subscription_end_marks_the_server_lost() {
    let connector = Arc::new(ScriptedConnector::default());
    let peer = ScriptedPeer::with_steps([
        ready(discover(true)),
        ready(json!({"tools":[]})),
        ready(json!({})),
    ]);
    connector.push_http(peer, Box::new(PendingEvents));
    let runtime = runtime(
        connector,
        Arc::new(TraceSink::default()),
        Duration::from_secs(1),
    );
    runtime
        .connect("server", http_config(), &CancellationToken::new())
        .await
        .expect("connect");
    eventually(|| runtime.registry().snapshot().definitions().is_empty()).await;
    eventually_async(|| {
        let runtime = runtime.clone();
        async move {
            runtime
                .get("server")
                .await
                .is_some_and(|entry| entry.status == McpServerStatus::Failed)
        }
    })
    .await;
}

#[tokio::test]
async fn rejected_tools_still_count_toward_the_discovery_byte_limit() {
    let connector = Arc::new(ScriptedConnector::default());
    let peer = ScriptedPeer::with_steps([
        ready(discover(false)),
        ready(json!({
            "tools":[{
                "name":"oversized-invalid",
                "description":"x".repeat(8 * 1024 * 1024 + 1),
                "inputSchema":{
                    "type":"object",
                    "properties":{
                        "region":{"type":"string","x-mcp-header":"Bad Name"}
                    }
                }
            }]
        })),
    ]);
    connector.push_http(peer, Box::new(PendingEvents));
    let runtime = runtime(
        connector,
        Arc::new(TraceSink::default()),
        Duration::from_secs(5),
    );
    let entry = runtime
        .connect("server", http_config(), &CancellationToken::new())
        .await
        .expect("server failure is isolated");
    assert_eq!(entry.status, McpServerStatus::Failed);
    assert!(entry
        .error
        .as_deref()
        .is_some_and(|error| error.contains("discovery exceeds")));
}

fn runtime(
    connector: Arc<ScriptedConnector>,
    sink: Arc<TraceSink>,
    startup_timeout: Duration,
) -> McpRuntime {
    let mut options = McpRuntimeOptions::new(connector).with_event_sink(sink);
    options.default_startup_timeout = startup_timeout;
    options.default_tool_timeout = Duration::from_secs(1);
    McpRuntime::new(ToolRegistry::new(), options).expect("runtime")
}

fn http_config() -> McpServerConfig {
    McpServerConfig::Http {
        url: "https://example.test/mcp".to_owned(),
        headers: BTreeMap::new(),
        bearer_token_env_var: None,
        auth: None,
        common: McpCommonConfig::default(),
    }
}

fn stdio_config() -> McpServerConfig {
    McpServerConfig::Stdio {
        command: "fake-mcp".to_owned(),
        args: Vec::new(),
        env: BTreeMap::new(),
        cwd: Some(PathBuf::from("workspace").to_string_lossy().into_owned()),
        common: McpCommonConfig::default(),
    }
}

fn discover(list_changed: bool) -> Value {
    json!({
        "resultType":"complete",
        "supportedVersions":[MODERN],
        "capabilities":{"tools":{"listChanged":list_changed}},
        "_meta":{"io.modelcontextprotocol/serverInfo":{"name":"fake","version":"1"}}
    })
}

fn tool(name: &str) -> Value {
    json!({
        "name":name,
        "description":"test tool",
        "inputSchema":{"type":"object","additionalProperties":false}
    })
}

fn ready(value: Value) -> PeerStep {
    PeerStep::Ready(Ok(value))
}

async fn eventually(mut predicate: impl FnMut() -> bool) {
    for _ in 0..100 {
        if predicate() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("condition did not become true");
}

async fn eventually_async<F, Fut>(mut predicate: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..100 {
        if predicate().await {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("async condition did not become true");
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
