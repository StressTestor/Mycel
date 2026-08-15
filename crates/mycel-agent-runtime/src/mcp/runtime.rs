use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use mycel_agent_protocol::{
    AgentEvent, ExecutableToolOutput, ExecutableToolResult, McpCommonConfig, McpServerConfig,
    McpServerStatus, McpServerStatusPayload, McpTransport, ToolDefinition, ToolInputDisplay,
    ToolListUpdatedReason, ToolUpdate, ToolUpdateKind,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::{
    CancellationToken, ExecutableTool, SessionHandle, ToolError, ToolExecutionSpec, ToolFuture,
    ToolInvocation, ToolPrepareContext, ToolRegistry, ToolRegistryError,
};

use super::{
    bounded_mcp_tool_result, qualify_mcp_tool_name, stable_hash_8, McpConnectedTransport,
    McpConnectionPurpose, McpEnvironment, McpFuture, McpHttpConnectRequest, McpPeer,
    McpProtocolEra, McpRequest, McpRequestError, McpStdioConnectRequest, McpTransportConnector,
    McpTransportError, McpTransportEvent, McpTransportEvents, SystemMcpEnvironment,
};

const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(60);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_TOOLS: usize = 1_024;
const MAX_TOOL_PAGES: usize = 32;
const MAX_DISCOVERY_BYTES: usize = 8 * 1024 * 1024;
const MAX_DIAGNOSTIC_CHARS: usize = 4_096;
const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";
const LEGACY_PROTOCOL_VERSION: &str = "2025-11-25";
const SUPPORTED_LEGACY_PROTOCOL_VERSIONS: [&str; 4] = [
    LEGACY_PROTOCOL_VERSION,
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];

const MCP_STATUS_RECORD: &str = "runtime.mcp_server_status";
const MCP_TOOL_LIST_RECORD: &str = "runtime.mcp_tool_list_updated";
const MCP_DISCOVERY_RECORD: &str = "mcp.tools_discovered";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolDefinition {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(skip)]
    http_header_bindings: Vec<McpHeaderBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct McpHeaderBinding {
    path: Vec<String>,
    header_name: String,
    value_kind: McpHeaderValueKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum McpHeaderValueKind {
    String,
    Number,
    Boolean,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpToolCollisionWith {
    SameServer { tool_name: String },
    OtherServer { server_name: String },
    RegistryTool { name: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolCollision {
    pub qualified: String,
    pub tool_name: String,
    pub collides_with: McpToolCollisionWith,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpDiscovery {
    pub server_name: String,
    pub hash: String,
    pub tools: Vec<McpToolDefinition>,
    pub enabled_names: Vec<String>,
    pub registered_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_scope: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collisions: Vec<McpToolCollision>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerEntry {
    pub name: String,
    pub transport: McpTransport,
    pub status: McpServerStatus,
    pub era: Option<McpProtocolEra>,
    pub protocol_version: Option<String>,
    pub tool_count: usize,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpRuntimeRecord {
    pub record_type: &'static str,
    pub payload: Value,
}

/// Persistence/live projection seam. The runtime always awaits `persist`
/// before calling `publish`; a persistence failure suppresses the live event.
pub trait McpEventSink: Send + Sync {
    fn persist<'a>(&'a self, record: McpRuntimeRecord) -> McpFuture<'a, Result<(), String>>;
    fn publish(&self, event: AgentEvent);
}

#[derive(Default)]
pub struct NoopMcpEventSink;

impl McpEventSink for NoopMcpEventSink {
    fn persist<'a>(&'a self, _record: McpRuntimeRecord) -> McpFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn publish(&self, _event: AgentEvent) {}
}

/// Production observer that writes into the session record log and publishes
/// through the session event bus only after the append succeeds.
pub struct SessionMcpEventSink {
    session: SessionHandle,
}

impl SessionMcpEventSink {
    pub fn new(session: SessionHandle) -> Self {
        Self { session }
    }
}

impl McpEventSink for SessionMcpEventSink {
    fn persist<'a>(&'a self, record: McpRuntimeRecord) -> McpFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.session
                .append_observation_record(record.record_type, record.payload)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn publish(&self, event: AgentEvent) {
        self.session.publish_live(event);
    }
}

pub struct McpRuntimeOptions {
    pub connector: Arc<dyn McpTransportConnector>,
    pub event_sink: Arc<dyn McpEventSink>,
    pub environment: Arc<dyn McpEnvironment>,
    pub default_stdio_cwd: Option<PathBuf>,
    pub default_startup_timeout: Duration,
    pub default_tool_timeout: Duration,
}

impl McpRuntimeOptions {
    pub fn new(connector: Arc<dyn McpTransportConnector>) -> Self {
        Self {
            connector,
            event_sink: Arc::new(NoopMcpEventSink),
            environment: Arc::new(SystemMcpEnvironment),
            default_stdio_cwd: None,
            default_startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            default_tool_timeout: DEFAULT_TOOL_TIMEOUT,
        }
    }

    pub fn with_event_sink(mut self, event_sink: Arc<dyn McpEventSink>) -> Self {
        self.event_sink = event_sink;
        self
    }

    pub fn with_environment(mut self, environment: Arc<dyn McpEnvironment>) -> Self {
        self.environment = environment;
        self
    }

    pub fn with_default_stdio_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.default_stdio_cwd = Some(cwd.into());
        self
    }
}

#[derive(Clone)]
pub struct McpRuntime {
    inner: Arc<McpRuntimeInner>,
}

struct McpRuntimeInner {
    registry: ToolRegistry,
    options: McpRuntimeOptions,
    entries: Mutex<BTreeMap<String, InternalEntry>>,
}

struct InternalEntry {
    public: McpServerEntry,
    config: McpServerConfig,
    generation: u64,
    peer: Option<Arc<dyn McpPeer>>,
    negotiated: Option<NegotiatedProtocol>,
    registered_names: BTreeSet<String>,
    redactor: Redactor,
    watcher: Option<tokio::task::AbortHandle>,
    watch_cancel: CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NegotiatedProtocol {
    era: McpProtocolEra,
    version: String,
    tools_list_changed: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct McpToolCatalog {
    tools: Vec<McpToolDefinition>,
    ttl_ms: Option<u64>,
    cache_scope: Option<String>,
}

struct ToolRegistration<'a> {
    server_name: &'a str,
    tools: &'a [McpToolDefinition],
    enabled_names: &'a BTreeSet<String>,
    previous_names: &'a BTreeSet<String>,
    peer: Arc<dyn McpPeer>,
    config: &'a McpServerConfig,
    negotiated: &'a NegotiatedProtocol,
}

impl McpRuntime {
    pub fn new(
        registry: ToolRegistry,
        options: McpRuntimeOptions,
    ) -> Result<Self, McpRuntimeError> {
        if options.default_startup_timeout.is_zero() {
            return Err(McpRuntimeError::InvalidTimeout("startup"));
        }
        if options.default_tool_timeout.is_zero() {
            return Err(McpRuntimeError::InvalidTimeout("tool"));
        }
        Ok(Self {
            inner: Arc::new(McpRuntimeInner {
                registry,
                options,
                entries: Mutex::new(BTreeMap::new()),
            }),
        })
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.inner.registry
    }

    pub async fn list(&self) -> Vec<McpServerEntry> {
        self.inner
            .entries
            .lock()
            .await
            .values()
            .map(|entry| entry.public.clone())
            .collect()
    }

    pub async fn get(&self, name: &str) -> Option<McpServerEntry> {
        self.inner
            .entries
            .lock()
            .await
            .get(name)
            .map(|entry| entry.public.clone())
    }

    /// Connects configured servers in sorted map order. Per-server startup
    /// failures are isolated and represented by a `failed` entry; durable
    /// event failures and caller cancellation abort the whole operation.
    pub async fn connect_all(
        &self,
        configs: &BTreeMap<String, McpServerConfig>,
        cancellation: &CancellationToken,
    ) -> Result<Vec<McpServerEntry>, McpRuntimeError> {
        let configured: BTreeSet<_> = configs.keys().cloned().collect();
        let existing: Vec<_> = self
            .inner
            .entries
            .lock()
            .await
            .keys()
            .filter(|name| !configured.contains(*name))
            .cloned()
            .collect();
        for name in existing {
            self.remove(&name).await?;
        }
        for (name, config) in configs {
            if cancellation.is_cancelled() {
                return Err(McpRuntimeError::Cancelled);
            }
            self.connect(name, config.clone(), cancellation).await?;
        }
        Ok(self.list().await)
    }

    pub async fn connect(
        &self,
        name: &str,
        config: McpServerConfig,
        cancellation: &CancellationToken,
    ) -> Result<McpServerEntry, McpRuntimeError> {
        validate_server_name(name)?;
        config
            .validate()
            .map_err(|error| McpRuntimeError::InvalidConfig(error.to_string()))?;
        let transport = transport_kind(&config);
        let redactor = build_redactor(&config, self.inner.options.environment.as_ref())?;
        let (generation, previous_peer, previous_tools, previous_watcher, previous_watch_cancel) = {
            let mut entries = self.inner.entries.lock().await;
            let previous = entries.remove(name);
            let generation = previous
                .as_ref()
                .map_or(1, |entry| entry.generation.wrapping_add(1));
            let previous_peer = previous.as_ref().and_then(|entry| entry.peer.clone());
            let previous_tools = previous
                .as_ref()
                .map(|entry| entry.registered_names.clone())
                .unwrap_or_default();
            let previous_watch_cancel = previous.as_ref().map(|entry| entry.watch_cancel.clone());
            let previous_watcher = previous.and_then(|entry| entry.watcher);
            entries.insert(
                name.to_owned(),
                InternalEntry {
                    public: McpServerEntry {
                        name: name.to_owned(),
                        transport,
                        status: McpServerStatus::Pending,
                        era: None,
                        protocol_version: None,
                        tool_count: 0,
                        error: None,
                    },
                    config: config.clone(),
                    generation,
                    peer: None,
                    negotiated: None,
                    registered_names: BTreeSet::new(),
                    redactor: redactor.clone(),
                    watcher: None,
                    watch_cancel: CancellationToken::new(),
                },
            );
            (
                generation,
                previous_peer,
                previous_tools,
                previous_watcher,
                previous_watch_cancel,
            )
        };
        if let Some(cancel) = previous_watch_cancel {
            cancel.cancel();
        }
        if let Some(watcher) = previous_watcher {
            watcher.abort();
        }
        if let Some(peer) = previous_peer {
            close_peer(&peer).await;
        }
        if !previous_tools.is_empty() {
            self.inner
                .registry
                .replace_batch(&previous_tools, Vec::new())?;
            self.commit_tool_list(name, ToolListUpdatedReason::Disconnected, None)
                .await?;
        }
        self.commit_status(name).await?;

        if common_config(&config).enabled == Some(false) {
            let public = self
                .set_status_if_current(name, generation, McpServerStatus::Disabled, None, 0)
                .await?;
            self.commit_status(name).await?;
            return Ok(public);
        }

        let result = self
            .connect_attempt(name, generation, &config, &redactor, cancellation)
            .await;
        match result {
            Ok(connected) => {
                self.install_connected(name, generation, config, redactor, connected)
                    .await
            }
            Err(error) => {
                let message = redactor.clean(&error.to_string());
                let public = self
                    .set_status_if_current(
                        name,
                        generation,
                        McpServerStatus::Failed,
                        Some(message),
                        0,
                    )
                    .await?;
                self.commit_status(name).await?;
                if matches!(error, McpRuntimeError::Cancelled) {
                    Err(error)
                } else {
                    Ok(public)
                }
            }
        }
    }

    pub async fn reconnect(
        &self,
        name: &str,
        cancellation: &CancellationToken,
    ) -> Result<McpServerEntry, McpRuntimeError> {
        let config = {
            let entries = self.inner.entries.lock().await;
            let entry = entries
                .get(name)
                .ok_or_else(|| McpRuntimeError::ServerNotFound(name.to_owned()))?;
            if common_config(&entry.config).enabled == Some(false) {
                return Err(McpRuntimeError::ServerDisabled(name.to_owned()));
            }
            entry.config.clone()
        };
        self.connect(name, config, cancellation).await
    }

    pub async fn refresh(
        &self,
        name: &str,
        cancellation: &CancellationToken,
    ) -> Result<McpServerEntry, McpRuntimeError> {
        let (generation, peer, config, redactor, negotiated, transport) = {
            let entries = self.inner.entries.lock().await;
            let entry = entries
                .get(name)
                .ok_or_else(|| McpRuntimeError::ServerNotFound(name.to_owned()))?;
            if entry.public.status != McpServerStatus::Connected {
                return Err(McpRuntimeError::NotConnected(name.to_owned()));
            }
            (
                entry.generation,
                entry.peer.clone().ok_or_else(|| {
                    McpRuntimeError::Protocol("connected MCP entry has no peer".to_owned())
                })?,
                entry.config.clone(),
                entry.redactor.clone(),
                entry.negotiated.clone().ok_or_else(|| {
                    McpRuntimeError::Protocol(
                        "connected MCP entry has no negotiated protocol".to_owned(),
                    )
                })?,
                entry.public.transport,
            )
        };
        let timeout = tool_timeout(&config, self.inner.options.default_tool_timeout);
        match discover_tools(&peer, timeout, cancellation, &negotiated, transport).await {
            Ok(catalog) => {
                self.register_discovery(name, generation, &config, peer, negotiated, catalog)
                    .await
            }
            Err(error) => {
                self.mark_failed(name, generation, &redactor.clean(&error.to_string()))
                    .await?;
                Err(error)
            }
        }
    }

    pub async fn remove(&self, name: &str) -> Result<bool, McpRuntimeError> {
        let Some(entry) = self.inner.entries.lock().await.remove(name) else {
            return Ok(false);
        };
        entry.watch_cancel.cancel();
        if let Some(watcher) = entry.watcher {
            watcher.abort();
        }
        if let Some(peer) = entry.peer {
            close_peer(&peer).await;
        }
        if !entry.registered_names.is_empty() {
            self.inner
                .registry
                .replace_batch(&entry.registered_names, Vec::new())?;
            self.commit_tool_list(name, ToolListUpdatedReason::Disconnected, None)
                .await?;
        }
        Ok(true)
    }

    pub async fn shutdown(&self) -> Result<(), McpRuntimeError> {
        let names: Vec<_> = self.inner.entries.lock().await.keys().cloned().collect();
        for name in names {
            self.remove(&name).await?;
        }
        Ok(())
    }

    async fn connect_attempt(
        &self,
        name: &str,
        _generation: u64,
        config: &McpServerConfig,
        redactor: &Redactor,
        cancellation: &CancellationToken,
    ) -> Result<ConnectedAttempt, McpRuntimeError> {
        let startup_timeout = startup_timeout(config, self.inner.options.default_startup_timeout);
        let deadline = tokio::time::Instant::now() + startup_timeout;
        let (connected, negotiated) = match config {
            McpServerConfig::Stdio {
                command,
                args,
                env,
                cwd,
                ..
            } => {
                let cwd = resolve_stdio_cwd(
                    cwd.as_deref(),
                    self.inner.options.default_stdio_cwd.as_deref(),
                );
                // A disposable probe process protects legacy servers that
                // exit (or wedge) when they receive a pre-initialize method.
                let probe_cancel = CancellationToken::new();
                let probe = run_transport_bounded(
                    self.inner.options.connector.connect_stdio(
                        McpStdioConnectRequest {
                            server_name: name.to_owned(),
                            purpose: McpConnectionPurpose::Probe,
                            command: command.clone(),
                            args: args.clone(),
                            env: env.clone(),
                            cwd: cwd.clone(),
                        },
                        &probe_cancel,
                    ),
                    cancellation,
                    &probe_cancel,
                    remaining(deadline)?,
                    "MCP startup",
                )
                .await?;
                let negotiated = probe_protocol(
                    &probe.peer,
                    McpTransport::Stdio,
                    cancellation,
                    remaining(deadline)?,
                )
                .await;
                close_peer(&probe.peer).await;
                let negotiated = negotiated?;

                let session_cancel = CancellationToken::new();
                let connected = run_transport_bounded(
                    self.inner.options.connector.connect_stdio(
                        McpStdioConnectRequest {
                            server_name: name.to_owned(),
                            purpose: McpConnectionPurpose::Session,
                            command: command.clone(),
                            args: args.clone(),
                            env: env.clone(),
                            cwd,
                        },
                        &session_cancel,
                    ),
                    cancellation,
                    &session_cancel,
                    remaining(deadline)?,
                    "MCP startup",
                )
                .await?;
                (connected, negotiated)
            }
            McpServerConfig::Http {
                url, headers, auth, ..
            } => {
                let headers = resolved_http_headers(
                    config,
                    headers,
                    self.inner.options.environment.as_ref(),
                )?;
                let connect_cancel = CancellationToken::new();
                let connected = run_transport_bounded(
                    self.inner.options.connector.connect_streamable_http(
                        McpHttpConnectRequest {
                            server_name: name.to_owned(),
                            url: url.clone(),
                            headers,
                            auth: *auth,
                        },
                        &connect_cancel,
                    ),
                    cancellation,
                    &connect_cancel,
                    remaining(deadline)?,
                    "MCP startup",
                )
                .await?;
                let negotiated = probe_protocol(
                    &connected.peer,
                    McpTransport::Http,
                    cancellation,
                    remaining(deadline)?,
                )
                .await?;
                (connected, negotiated)
            }
        };

        let negotiated = if negotiated.era == McpProtocolEra::Legacy {
            match initialize_legacy(
                &connected.peer,
                transport_kind(config),
                cancellation,
                deadline,
            )
            .await
            {
                Ok(negotiated) => negotiated,
                Err(error) => {
                    close_peer(&connected.peer).await;
                    return Err(redacted_error(error, redactor));
                }
            }
        } else {
            negotiated
        };
        let catalog = discover_tools_until(
            &connected.peer,
            deadline,
            cancellation,
            &negotiated,
            transport_kind(config),
        )
        .await?;
        Ok(ConnectedAttempt {
            connected,
            negotiated,
            catalog,
        })
    }

    async fn install_connected(
        &self,
        name: &str,
        generation: u64,
        config: McpServerConfig,
        redactor: Redactor,
        connected: ConnectedAttempt,
    ) -> Result<McpServerEntry, McpRuntimeError> {
        let peer = connected.connected.peer;
        let events = connected.connected.events;
        let public = self
            .register_discovery(
                name,
                generation,
                &config,
                peer.clone(),
                connected.negotiated.clone(),
                connected.catalog,
            )
            .await?;
        let watcher = spawn_watcher(WatcherContext {
            runtime: Arc::downgrade(&self.inner),
            server_name: name.to_owned(),
            generation,
            events,
            peer,
            negotiated: connected.negotiated.clone(),
            transport: transport_kind(&config),
            watch_cancel: {
                let entries = self.inner.entries.lock().await;
                entries
                    .get(name)
                    .filter(|entry| entry.generation == generation)
                    .ok_or_else(|| McpRuntimeError::StaleAttempt(name.to_owned()))?
                    .watch_cancel
                    .clone()
            },
        });
        {
            let mut entries = self.inner.entries.lock().await;
            let entry = entries
                .get_mut(name)
                .filter(|entry| entry.generation == generation)
                .ok_or_else(|| McpRuntimeError::StaleAttempt(name.to_owned()))?;
            entry.config = config;
            entry.redactor = redactor;
            entry.negotiated = Some(connected.negotiated.clone());
            entry.watcher = Some(watcher.abort_handle());
        }
        drop(watcher);
        Ok(public)
    }

    async fn register_discovery(
        &self,
        name: &str,
        generation: u64,
        config: &McpServerConfig,
        peer: Arc<dyn McpPeer>,
        negotiated: NegotiatedProtocol,
        catalog: McpToolCatalog,
    ) -> Result<McpServerEntry, McpRuntimeError> {
        let McpToolCatalog {
            tools,
            ttl_ms,
            cache_scope,
        } = catalog;
        let previous_names = {
            let entries = self.inner.entries.lock().await;
            entries
                .get(name)
                .filter(|entry| entry.generation == generation)
                .ok_or_else(|| McpRuntimeError::StaleAttempt(name.to_owned()))?
                .registered_names
                .clone()
        };
        let enabled_names = enabled_tool_names(config, &tools);
        let (wrapped, registered_names, collisions) = self
            .build_registered_tools(ToolRegistration {
                server_name: name,
                tools: &tools,
                enabled_names: &enabled_names,
                previous_names: &previous_names,
                peer: peer.clone(),
                config,
                negotiated: &negotiated,
            })
            .await;
        self.inner
            .registry
            .replace_batch(&previous_names, wrapped)?;

        let discovery = discovery_record(
            name,
            tools,
            enabled_names,
            registered_names.iter().cloned().collect(),
            collisions,
            ttl_ms,
            cache_scope,
        )?;
        {
            let mut entries = self.inner.entries.lock().await;
            let entry = entries
                .get_mut(name)
                .filter(|entry| entry.generation == generation)
                .ok_or_else(|| McpRuntimeError::StaleAttempt(name.to_owned()))?;
            entry.peer = Some(peer);
            entry.negotiated = Some(negotiated.clone());
            entry.registered_names = registered_names;
            entry.public.status = McpServerStatus::Connected;
            entry.public.era = Some(negotiated.era);
            entry.public.protocol_version = Some(negotiated.version);
            entry.public.tool_count = entry.registered_names.len();
            entry.public.error = None;
        }
        self.commit_tool_list(name, ToolListUpdatedReason::Connected, Some(discovery))
            .await?;
        self.commit_status(name).await?;
        self.get(name)
            .await
            .ok_or_else(|| McpRuntimeError::ServerNotFound(name.to_owned()))
    }

    async fn build_registered_tools(
        &self,
        registration: ToolRegistration<'_>,
    ) -> (
        Vec<Arc<dyn ExecutableTool>>,
        BTreeSet<String>,
        Vec<McpToolCollision>,
    ) {
        let ToolRegistration {
            server_name,
            tools,
            enabled_names,
            previous_names,
            peer,
            config,
            negotiated,
        } = registration;
        let snapshot = self.inner.registry.snapshot();
        let entries = self.inner.entries.lock().await;
        let mut owners = BTreeMap::new();
        for (name, entry) in entries.iter() {
            if name == server_name {
                continue;
            }
            for tool_name in &entry.registered_names {
                owners.insert(tool_name.clone(), name.clone());
            }
        }
        drop(entries);
        let existing: BTreeSet<_> = snapshot
            .definitions()
            .iter()
            .map(|definition| definition.name.clone())
            .filter(|name| !previous_names.contains(name))
            .collect();
        let redactor =
            build_redactor(config, self.inner.options.environment.as_ref()).unwrap_or_default();
        let timeout = tool_timeout(config, self.inner.options.default_tool_timeout);
        let mut seen: BTreeMap<String, String> = BTreeMap::new();
        let mut wrapped: Vec<Arc<dyn ExecutableTool>> = Vec::new();
        let mut registered = BTreeSet::new();
        let mut collisions = Vec::new();
        for tool in tools {
            if !enabled_names.contains(&tool.name) {
                continue;
            }
            let qualified = qualify_mcp_tool_name(server_name, &tool.name);
            if let Some(first) = seen.get(&qualified) {
                collisions.push(McpToolCollision {
                    qualified,
                    tool_name: tool.name.clone(),
                    collides_with: McpToolCollisionWith::SameServer {
                        tool_name: first.clone(),
                    },
                });
                continue;
            }
            if existing.contains(&qualified) {
                let collides_with = owners.get(&qualified).map_or_else(
                    || McpToolCollisionWith::RegistryTool {
                        name: qualified.clone(),
                    },
                    |owner| McpToolCollisionWith::OtherServer {
                        server_name: owner.clone(),
                    },
                );
                collisions.push(McpToolCollision {
                    qualified,
                    tool_name: tool.name.clone(),
                    collides_with,
                });
                continue;
            }
            seen.insert(qualified.clone(), tool.name.clone());
            registered.insert(qualified.clone());
            wrapped.push(Arc::new(McpExecutableTool {
                qualified_name: qualified,
                remote_name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.input_schema.clone(),
                peer: peer.clone(),
                negotiated: negotiated.clone(),
                transport: transport_kind(config),
                http_header_bindings: tool.http_header_bindings.clone(),
                timeout,
                redactor: redactor.clone(),
            }));
        }
        (wrapped, registered, collisions)
    }

    async fn set_status_if_current(
        &self,
        name: &str,
        generation: u64,
        status: McpServerStatus,
        error: Option<String>,
        tool_count: usize,
    ) -> Result<McpServerEntry, McpRuntimeError> {
        let mut entries = self.inner.entries.lock().await;
        let entry = entries
            .get_mut(name)
            .filter(|entry| entry.generation == generation)
            .ok_or_else(|| McpRuntimeError::StaleAttempt(name.to_owned()))?;
        entry.public.status = status;
        entry.public.error = error;
        entry.public.tool_count = tool_count;
        Ok(entry.public.clone())
    }

    async fn mark_failed(
        &self,
        name: &str,
        generation: u64,
        error: &str,
    ) -> Result<(), McpRuntimeError> {
        let (peer, names, watcher, watch_cancel) = {
            let mut entries = self.inner.entries.lock().await;
            let Some(entry) = entries
                .get_mut(name)
                .filter(|entry| entry.generation == generation)
            else {
                return Ok(());
            };
            entry.public.status = McpServerStatus::Failed;
            entry.public.tool_count = 0;
            entry.public.error = Some(entry.redactor.clean(error));
            (
                entry.peer.take(),
                std::mem::take(&mut entry.registered_names),
                entry.watcher.take(),
                entry.watch_cancel.clone(),
            )
        };
        watch_cancel.cancel();
        // `mark_failed` may be called by the watcher itself. Cancelling its
        // token lets an external watcher exit without aborting the task that
        // still has to durably publish the failed status.
        drop(watcher);
        if let Some(peer) = peer {
            close_peer(&peer).await;
        }
        if !names.is_empty() {
            self.inner.registry.replace_batch(&names, Vec::new())?;
            self.commit_tool_list(name, ToolListUpdatedReason::Failed, None)
                .await?;
        }
        self.commit_status(name).await
    }

    async fn commit_status(&self, name: &str) -> Result<(), McpRuntimeError> {
        let entry = self
            .get(name)
            .await
            .ok_or_else(|| McpRuntimeError::ServerNotFound(name.to_owned()))?;
        let payload = status_payload(&entry);
        self.commit_event(
            McpRuntimeRecord {
                record_type: MCP_STATUS_RECORD,
                payload: json!({
                    "server": payload,
                    "era": entry.era,
                    "protocolVersion": entry.protocol_version,
                }),
            },
            AgentEvent::McpServerStatus { server: payload },
        )
        .await
    }

    async fn commit_tool_list(
        &self,
        server_name: &str,
        reason: ToolListUpdatedReason,
        discovery: Option<McpDiscovery>,
    ) -> Result<(), McpRuntimeError> {
        let record = if let Some(discovery) = discovery {
            McpRuntimeRecord {
                record_type: MCP_DISCOVERY_RECORD,
                payload: serde_json::to_value(discovery)
                    .map_err(|error| McpRuntimeError::Protocol(error.to_string()))?,
            }
        } else {
            McpRuntimeRecord {
                record_type: MCP_TOOL_LIST_RECORD,
                payload: json!({"serverName":server_name, "reason":reason}),
            }
        };
        self.commit_event(
            record,
            AgentEvent::ToolListUpdated {
                reason,
                server_name: server_name.to_owned(),
            },
        )
        .await
    }

    async fn commit_event(
        &self,
        record: McpRuntimeRecord,
        event: AgentEvent,
    ) -> Result<(), McpRuntimeError> {
        self.inner
            .options
            .event_sink
            .persist(record)
            .await
            .map_err(McpRuntimeError::EventSink)?;
        self.inner.options.event_sink.publish(event);
        Ok(())
    }
}

/// Explicit typed decoding seam for hosts that accept standalone MCP server
/// JSON. The removed HTTP+SSE transport fails with a dedicated error instead
/// of being treated as Streamable HTTP.
pub fn parse_mcp_server_config(value: Value) -> Result<McpServerConfig, McpRuntimeError> {
    if value.get("transport").and_then(Value::as_str) == Some("sse") {
        return Err(McpRuntimeError::LegacySseUnsupported);
    }
    let config: McpServerConfig = serde_json::from_value(value)
        .map_err(|error| McpRuntimeError::InvalidConfig(error.to_string()))?;
    config
        .validate()
        .map_err(|error| McpRuntimeError::InvalidConfig(error.to_string()))?;
    Ok(config)
}

struct ConnectedAttempt {
    connected: McpConnectedTransport,
    negotiated: NegotiatedProtocol,
    catalog: McpToolCatalog,
}

struct WatcherContext {
    runtime: Weak<McpRuntimeInner>,
    server_name: String,
    generation: u64,
    events: Box<dyn McpTransportEvents>,
    peer: Arc<dyn McpPeer>,
    negotiated: NegotiatedProtocol,
    transport: McpTransport,
    watch_cancel: CancellationToken,
}

fn spawn_watcher(context: WatcherContext) -> tokio::task::JoinHandle<()> {
    let WatcherContext {
        runtime,
        server_name,
        generation,
        mut events,
        peer,
        negotiated,
        transport,
        watch_cancel,
    } = context;
    tokio::spawn(async move {
        let subscription_cancel = watch_cancel.clone();
        let subscription = async {
            if negotiated.era == McpProtocolEra::Modern && negotiated.tools_list_changed {
                let request = build_request(
                    &negotiated,
                    transport,
                    "subscriptions/listen",
                    json!({"toolsListChanged":true}),
                    BTreeMap::new(),
                )?;
                request_bounded(
                    &peer,
                    request,
                    &subscription_cancel,
                    Duration::from_secs(365 * 24 * 60 * 60),
                    "MCP subscription",
                )
                .await
                .map(Some)
            } else {
                std::future::pending::<Result<Option<Value>, McpRuntimeError>>().await
            }
        };
        tokio::pin!(subscription);
        loop {
            let event = tokio::select! {
                () = watch_cancel.cancelled() => return,
                subscription_result = &mut subscription => {
                    if watch_cancel.is_cancelled() {
                        return;
                    }
                    if let Some(inner) = runtime.upgrade() {
                        let runtime = McpRuntime { inner };
                        let message = subscription_result.map_or_else(
                            |error| error.to_string(),
                            |_| "MCP subscription ended".to_owned(),
                        );
                        let _ = runtime.mark_failed(&server_name, generation, &message).await;
                    }
                    return;
                }
                event = events.next() => event,
            };
            let Some(event) = event else {
                break;
            };
            let Some(inner) = runtime.upgrade() else {
                return;
            };
            let runtime = McpRuntime { inner };
            match event {
                McpTransportEvent::ToolsListChanged => {
                    let current = runtime
                        .inner
                        .entries
                        .lock()
                        .await
                        .get(&server_name)
                        .is_some_and(|entry| entry.generation == generation);
                    if !current {
                        return;
                    }
                    let _ = runtime
                        .refresh(&server_name, &CancellationToken::new())
                        .await;
                }
                McpTransportEvent::Closed { error } => {
                    let message =
                        error.unwrap_or_else(|| "transport closed unexpectedly".to_owned());
                    let _ = runtime
                        .mark_failed(&server_name, generation, &message)
                        .await;
                    return;
                }
            }
        }
        if let Some(inner) = runtime.upgrade() {
            let runtime = McpRuntime { inner };
            let _ = runtime
                .mark_failed(&server_name, generation, "transport event stream ended")
                .await;
        }
    })
}

struct McpExecutableTool {
    qualified_name: String,
    remote_name: String,
    description: String,
    parameters: Value,
    peer: Arc<dyn McpPeer>,
    negotiated: NegotiatedProtocol,
    transport: McpTransport,
    http_header_bindings: Vec<McpHeaderBinding>,
    timeout: Duration,
    redactor: Redactor,
}

impl ExecutableTool for McpExecutableTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.qualified_name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
            deferred: false,
        }
    }

    fn prepare(
        &self,
        _arguments: &Value,
        _context: &ToolPrepareContext,
    ) -> Result<ToolExecutionSpec, ToolError> {
        let mut spec = ToolExecutionSpec::new(
            ToolInputDisplay::Generic {
                summary: format!("Calling {}", self.qualified_name),
                detail: None,
            },
            self.qualified_name.clone(),
        );
        spec.approval_rule = Some(self.qualified_name.clone());
        spec.description = Some(format!("Calling {}", self.qualified_name));
        Ok(spec)
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation) -> ToolFuture<'a> {
        Box::pin(async move {
            self.validate_arguments(&invocation.arguments)?;
            let arguments = invocation.arguments.as_object().cloned().ok_or_else(|| {
                ToolError::InvalidArguments {
                    path: "$".to_owned(),
                    message: "MCP tool arguments must be an object".to_owned(),
                }
            })?;
            let param_headers =
                mcp_param_headers(&self.http_header_bindings, &arguments).map_err(|message| {
                    ToolError::InvalidArguments {
                        path: "$".to_owned(),
                        message,
                    }
                })?;
            invocation.updates.emit(ToolUpdate {
                kind: ToolUpdateKind::Status,
                text: Some(format!("calling {}", self.qualified_name)),
                percent: Some(0.0),
                custom_kind: None,
                custom_data: None,
            });
            let request = build_request(
                &self.negotiated,
                self.transport,
                "tools/call",
                json!({"name":self.remote_name, "arguments":arguments}),
                param_headers,
            )
            .map_err(|error| ToolError::Execute(error.to_string()))?;
            let result = request_bounded(
                &self.peer,
                request,
                &invocation.cancellation,
                self.timeout,
                "MCP tool call",
            )
            .await;
            let result = match result {
                Ok(value) => match bounded_mcp_tool_result(&value, &self.qualified_name) {
                    Ok(result) => result,
                    Err(error) => error_tool_result(format!(
                        "MCP tool returned a malformed result: {}",
                        self.redactor.clean(&error.to_string())
                    )),
                },
                Err(error) => error_tool_result(self.redactor.clean(&error.to_string())),
            };
            invocation.updates.emit(ToolUpdate {
                kind: ToolUpdateKind::Status,
                text: Some(if result.is_error {
                    format!("{} failed", self.qualified_name)
                } else {
                    format!("{} completed", self.qualified_name)
                }),
                percent: Some(100.0),
                custom_kind: None,
                custom_data: None,
            });
            Ok(result)
        })
    }
}

async fn discover_tools(
    peer: &Arc<dyn McpPeer>,
    timeout: Duration,
    cancellation: &CancellationToken,
    negotiated: &NegotiatedProtocol,
    transport: McpTransport,
) -> Result<McpToolCatalog, McpRuntimeError> {
    discover_tools_until(
        peer,
        tokio::time::Instant::now() + timeout,
        cancellation,
        negotiated,
        transport,
    )
    .await
}

async fn discover_tools_until(
    peer: &Arc<dyn McpPeer>,
    deadline: tokio::time::Instant,
    cancellation: &CancellationToken,
    negotiated: &NegotiatedProtocol,
    transport: McpTransport,
) -> Result<McpToolCatalog, McpRuntimeError> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors = BTreeSet::new();
    let mut encoded_bytes = 0_usize;
    let mut ttl_ms = None;
    let mut cache_scope = None;
    for _ in 0..MAX_TOOL_PAGES {
        let params = cursor
            .as_ref()
            .map_or_else(|| json!({}), |cursor| json!({"cursor":cursor}));
        let request = build_request(negotiated, transport, "tools/list", params, BTreeMap::new())?;
        let response = request_bounded(
            peer,
            request,
            cancellation,
            remaining(deadline)?,
            "MCP tools/list",
        )
        .await?;
        encoded_bytes = encoded_bytes.saturating_add(
            serde_json::to_vec(&response)
                .map_err(|error| McpRuntimeError::Protocol(error.to_string()))?
                .len(),
        );
        if encoded_bytes > MAX_DISCOVERY_BYTES {
            return Err(McpRuntimeError::Protocol(format!(
                "MCP tool discovery exceeds {MAX_DISCOVERY_BYTES} bytes"
            )));
        }
        let object = response.as_object().ok_or_else(|| {
            McpRuntimeError::Protocol("tools/list result must be an object".to_owned())
        })?;
        let page = object
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                McpRuntimeError::Protocol("tools/list tools must be an array".to_owned())
            })?;
        if negotiated.era == McpProtocolEra::Modern {
            if let Some(value) = object.get("ttlMs") {
                let page_ttl = value.as_u64().ok_or_else(|| {
                    McpRuntimeError::Protocol(
                        "tools/list ttlMs must be an unsigned integer".to_owned(),
                    )
                })?;
                if ttl_ms.is_some_and(|seen| seen != page_ttl) {
                    return Err(McpRuntimeError::Protocol(
                        "tools/list changed ttlMs during pagination".to_owned(),
                    ));
                }
                ttl_ms = Some(page_ttl);
            }
            if let Some(value) = object.get("cacheScope") {
                let page_scope = value
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        McpRuntimeError::Protocol(
                            "tools/list cacheScope must be a non-empty string".to_owned(),
                        )
                    })?;
                if cache_scope
                    .as_deref()
                    .is_some_and(|seen| seen != page_scope)
                {
                    return Err(McpRuntimeError::Protocol(
                        "tools/list changed cacheScope during pagination".to_owned(),
                    ));
                }
                cache_scope = Some(page_scope.to_owned());
            }
        }
        for raw in page {
            let Some(tool) = parse_tool_definition(
                raw,
                negotiated.era == McpProtocolEra::Modern && transport == McpTransport::Http,
            )?
            else {
                continue;
            };
            tools.push(tool);
            if tools.len() > MAX_TOOLS {
                return Err(McpRuntimeError::Protocol(format!(
                    "MCP server advertised more than {MAX_TOOLS} tools"
                )));
            }
        }
        let next = object.get("nextCursor").and_then(Value::as_str);
        let Some(next) = next.filter(|next| !next.is_empty()) else {
            return Ok(McpToolCatalog {
                tools,
                ttl_ms,
                cache_scope,
            });
        };
        if !seen_cursors.insert(next.to_owned()) {
            return Err(McpRuntimeError::Protocol(
                "tools/list repeated a pagination cursor".to_owned(),
            ));
        }
        cursor = Some(next.to_owned());
    }
    Err(McpRuntimeError::Protocol(format!(
        "tools/list exceeded {MAX_TOOL_PAGES} pages"
    )))
}

fn parse_tool_definition(
    value: &Value,
    validate_http_headers: bool,
) -> Result<Option<McpToolDefinition>, McpRuntimeError> {
    let object = value.as_object().ok_or_else(|| {
        McpRuntimeError::Protocol("MCP tool definition must be an object".to_owned())
    })?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| McpRuntimeError::Protocol("MCP tool name must not be empty".to_owned()))?;
    let description = object
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let input_schema = object.get("inputSchema").cloned().ok_or_else(|| {
        McpRuntimeError::Protocol(format!("MCP tool {name:?} has no inputSchema"))
    })?;
    if !input_schema.is_object() {
        return Err(McpRuntimeError::Protocol(format!(
            "MCP tool {name:?} inputSchema must be an object"
        )));
    }
    let http_header_bindings = if validate_http_headers {
        let Some(bindings) = validate_http_header_bindings(&input_schema) else {
            return Ok(None);
        };
        bindings
    } else {
        Vec::new()
    };
    Ok(Some(McpToolDefinition {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema,
        http_header_bindings,
    }))
}

fn validate_http_header_bindings(schema: &Value) -> Option<Vec<McpHeaderBinding>> {
    let total_annotations = count_key(schema, "x-mcp-header");
    let properties = schema.get("properties").and_then(Value::as_object);
    let mut bindings = Vec::new();
    let mut seen_names = BTreeSet::new();
    if let Some(properties) = properties {
        collect_http_header_bindings(properties, &mut Vec::new(), &mut bindings, &mut seen_names)?;
    }
    (bindings.len() == total_annotations).then_some(bindings)
}

fn collect_http_header_bindings(
    properties: &serde_json::Map<String, Value>,
    path: &mut Vec<String>,
    bindings: &mut Vec<McpHeaderBinding>,
    seen_names: &mut BTreeSet<String>,
) -> Option<()> {
    for (property_name, raw_schema) in properties {
        let property = raw_schema.as_object()?;
        path.push(property_name.clone());
        if let Some(raw_name) = property.get("x-mcp-header") {
            let header_name = raw_name.as_str()?;
            if !valid_mcp_header_name(header_name)
                || !seen_names.insert(header_name.to_ascii_lowercase())
            {
                return None;
            }
            let value_kind = match property.get("type").and_then(Value::as_str) {
                Some("string") => McpHeaderValueKind::String,
                Some("number") | Some("integer") => McpHeaderValueKind::Number,
                Some("boolean") => McpHeaderValueKind::Boolean,
                _ => return None,
            };
            bindings.push(McpHeaderBinding {
                path: path.clone(),
                header_name: header_name.to_owned(),
                value_kind,
            });
        }
        if let Some(children) = property.get("properties") {
            collect_http_header_bindings(children.as_object()?, path, bindings, seen_names)?;
        }
        path.pop();
    }
    Some(())
}

fn count_key(value: &Value, key: &str) -> usize {
    match value {
        Value::Object(object) => {
            usize::from(object.contains_key(key))
                + object
                    .values()
                    .map(|value| count_key(value, key))
                    .sum::<usize>()
        }
        Value::Array(values) => values.iter().map(|value| count_key(value, key)).sum(),
        _ => 0,
    }
}

fn valid_mcp_header_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| (0x21..=0x7e).contains(&byte) && byte != b':')
}

fn validate_initialize_result(value: &Value) -> Result<NegotiatedProtocol, McpRuntimeError> {
    let object = value.as_object().ok_or_else(|| {
        McpRuntimeError::Protocol("initialize result must be an object".to_owned())
    })?;
    let protocol = object
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            McpRuntimeError::Protocol("initialize result has no protocolVersion".to_owned())
        })?;
    if !SUPPORTED_LEGACY_PROTOCOL_VERSIONS.contains(&protocol) {
        return Err(McpRuntimeError::UnsupportedProtocol(protocol.to_owned()));
    }
    if !object.get("capabilities").is_some_and(Value::is_object) {
        return Err(McpRuntimeError::Protocol(
            "initialize capabilities must be an object".to_owned(),
        ));
    }
    if !object.get("serverInfo").is_some_and(Value::is_object) {
        return Err(McpRuntimeError::Protocol(
            "initialize serverInfo must be an object".to_owned(),
        ));
    }
    let tools_list_changed = object
        .get("capabilities")
        .and_then(|value| value.get("tools"))
        .and_then(|value| value.get("listChanged"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(NegotiatedProtocol {
        era: McpProtocolEra::Legacy,
        version: protocol.to_owned(),
        tools_list_changed,
    })
}

async fn request_bounded(
    peer: &Arc<dyn McpPeer>,
    request: McpRequest,
    cancellation: &CancellationToken,
    timeout: Duration,
    phase: &'static str,
) -> Result<Value, McpRuntimeError> {
    let local_cancel = CancellationToken::new();
    run_request_bounded(
        peer.request(request, &local_cancel),
        cancellation,
        &local_cancel,
        timeout,
        phase,
    )
    .await
}

async fn run_notify_bounded(
    peer: &Arc<dyn McpPeer>,
    request: McpRequest,
    cancellation: &CancellationToken,
    timeout: Duration,
) -> Result<(), McpRuntimeError> {
    let local_cancel = CancellationToken::new();
    run_request_bounded(
        peer.notify(request),
        cancellation,
        &local_cancel,
        timeout,
        "MCP notification",
    )
    .await
}

async fn run_request_bounded<T, F>(
    future: F,
    cancellation: &CancellationToken,
    operation_cancel: &CancellationToken,
    timeout: Duration,
    phase: &'static str,
) -> Result<T, McpRuntimeError>
where
    F: Future<Output = Result<T, McpRequestError>>,
{
    if cancellation.is_cancelled() {
        operation_cancel.cancel();
        return Err(McpRuntimeError::Cancelled);
    }
    tokio::pin!(future);
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    tokio::select! {
        result = &mut future => result.map_err(McpRuntimeError::Request),
        () = cancellation.cancelled() => {
            operation_cancel.cancel();
            Err(McpRuntimeError::Cancelled)
        },
        () = &mut deadline => {
            operation_cancel.cancel();
            Err(McpRuntimeError::Timeout { phase, timeout })
        }
    }
}

async fn run_transport_bounded<T, F>(
    future: F,
    cancellation: &CancellationToken,
    operation_cancel: &CancellationToken,
    timeout: Duration,
    phase: &'static str,
) -> Result<T, McpRuntimeError>
where
    F: Future<Output = Result<T, McpTransportError>>,
{
    if cancellation.is_cancelled() {
        operation_cancel.cancel();
        return Err(McpRuntimeError::Cancelled);
    }
    tokio::pin!(future);
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    tokio::select! {
        result = &mut future => result.map_err(McpRuntimeError::Transport),
        () = cancellation.cancelled() => {
            operation_cancel.cancel();
            Err(McpRuntimeError::Cancelled)
        },
        () = &mut deadline => {
            operation_cancel.cancel();
            Err(McpRuntimeError::Timeout { phase, timeout })
        }
    }
}

async fn probe_protocol(
    peer: &Arc<dyn McpPeer>,
    transport: McpTransport,
    cancellation: &CancellationToken,
    timeout: Duration,
) -> Result<NegotiatedProtocol, McpRuntimeError> {
    let preferred = NegotiatedProtocol {
        era: McpProtocolEra::Modern,
        version: MODERN_PROTOCOL_VERSION.to_owned(),
        tools_list_changed: false,
    };
    let request = build_request(
        &preferred,
        transport,
        "server/discover",
        json!({}),
        BTreeMap::new(),
    )?;
    match request_bounded(peer, request, cancellation, timeout, "MCP era probe").await {
        Ok(value) => parse_discover_result(&value),
        Err(error) => match modern_versions_from_error(&error) {
            Some(versions) => select_modern_version(&versions, None),
            None if legacy_probe_signal(&error, transport) => Ok(NegotiatedProtocol {
                era: McpProtocolEra::Legacy,
                version: LEGACY_PROTOCOL_VERSION.to_owned(),
                tools_list_changed: false,
            }),
            None => Err(error),
        },
    }
}

fn parse_discover_result(value: &Value) -> Result<NegotiatedProtocol, McpRuntimeError> {
    let object = value.as_object().ok_or_else(|| {
        McpRuntimeError::Protocol("server/discover result must be an object".to_owned())
    })?;
    let versions = object
        .get("supportedVersions")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            McpRuntimeError::Protocol(
                "server/discover supportedVersions must be an array".to_owned(),
            )
        })?
        .iter()
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                McpRuntimeError::Protocol(
                    "server/discover supportedVersions entries must be strings".to_owned(),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let capabilities = object
        .get("capabilities")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            McpRuntimeError::Protocol("server/discover capabilities must be an object".to_owned())
        })?;
    let tools_list_changed = capabilities
        .get("tools")
        .and_then(Value::as_object)
        .and_then(|tools| tools.get("listChanged"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    select_modern_version(&versions, Some(tools_list_changed))
}

fn modern_versions_from_error(error: &McpRuntimeError) -> Option<Vec<String>> {
    let McpRuntimeError::Request(McpRequestError::JsonRpc {
        code: -32022, data, ..
    }) = error
    else {
        return None;
    };
    data.as_ref()?
        .get("supported")?
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect()
}

fn select_modern_version(
    versions: &[String],
    tools_list_changed: Option<bool>,
) -> Result<NegotiatedProtocol, McpRuntimeError> {
    if versions
        .iter()
        .any(|version| version == MODERN_PROTOCOL_VERSION)
    {
        return Ok(NegotiatedProtocol {
            era: McpProtocolEra::Modern,
            version: MODERN_PROTOCOL_VERSION.to_owned(),
            tools_list_changed: tools_list_changed.unwrap_or(false),
        });
    }
    Err(McpRuntimeError::UnsupportedProtocol(versions.join(", ")))
}

fn legacy_probe_signal(error: &McpRuntimeError, transport: McpTransport) -> bool {
    match transport {
        // The stdio compatibility rule deliberately treats an unrecognized
        // JSON-RPC error or a silent probe as a legacy signal. The probe runs
        // in a disposable child so a wedged legacy server cannot poison the
        // session process.
        McpTransport::Stdio => matches!(
            error,
            McpRuntimeError::Request(_) | McpRuntimeError::Timeout { .. }
        ),
        // HTTP may fall back only on a non-authentication 4xx. Network errors,
        // timeouts, 401/403 and 5xx remain real failures.
        McpTransport::Http => match error {
            McpRuntimeError::Request(McpRequestError::Http { status, .. })
            | McpRuntimeError::Request(McpRequestError::JsonRpc {
                http_status: Some(status),
                ..
            }) => (400..500).contains(status) && !matches!(*status, 401 | 403),
            _ => false,
        },
    }
}

async fn initialize_legacy(
    peer: &Arc<dyn McpPeer>,
    transport: McpTransport,
    cancellation: &CancellationToken,
    deadline: tokio::time::Instant,
) -> Result<NegotiatedProtocol, McpRuntimeError> {
    let offered = NegotiatedProtocol {
        era: McpProtocolEra::Legacy,
        version: LEGACY_PROTOCOL_VERSION.to_owned(),
        tools_list_changed: false,
    };
    let request = build_request(
        &offered,
        transport,
        "initialize",
        json!({
            "protocolVersion": LEGACY_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": client_info(),
        }),
        BTreeMap::new(),
    )?;
    let response = request_bounded(
        peer,
        request,
        cancellation,
        remaining(deadline)?,
        "MCP initialize",
    )
    .await?;
    let negotiated = validate_initialize_result(&response)?;
    let initialized = build_request(
        &negotiated,
        transport,
        "notifications/initialized",
        json!({}),
        BTreeMap::new(),
    )?;
    run_notify_bounded(peer, initialized, cancellation, remaining(deadline)?).await?;
    Ok(negotiated)
}

fn build_request(
    negotiated: &NegotiatedProtocol,
    transport: McpTransport,
    method: &str,
    mut params: Value,
    extra_http_headers: BTreeMap<String, String>,
) -> Result<McpRequest, McpRuntimeError> {
    let object = params.as_object_mut().ok_or_else(|| {
        McpRuntimeError::Protocol("MCP request params must be an object".to_owned())
    })?;
    if negotiated.era == McpProtocolEra::Modern {
        let meta = object.entry("_meta").or_insert_with(|| json!({}));
        let meta = meta.as_object_mut().ok_or_else(|| {
            McpRuntimeError::Protocol("MCP request _meta must be an object".to_owned())
        })?;
        meta.insert(
            "io.modelcontextprotocol/protocolVersion".to_owned(),
            Value::String(negotiated.version.clone()),
        );
        meta.insert(
            "io.modelcontextprotocol/clientInfo".to_owned(),
            client_info(),
        );
        meta.insert(
            "io.modelcontextprotocol/clientCapabilities".to_owned(),
            json!({}),
        );
    }

    let mut http_headers = BTreeMap::new();
    if transport == McpTransport::Http {
        http_headers.insert(
            "MCP-Protocol-Version".to_owned(),
            negotiated.version.clone(),
        );
        if negotiated.era == McpProtocolEra::Modern {
            http_headers.insert("Mcp-Method".to_owned(), method.to_owned());
            if let Some(name) = object
                .get("name")
                .or_else(|| object.get("uri"))
                .and_then(Value::as_str)
            {
                if !plain_header_value(name) {
                    return Err(McpRuntimeError::Protocol(
                        "MCP name/URI cannot be represented safely in an HTTP header".to_owned(),
                    ));
                }
                http_headers.insert("Mcp-Name".to_owned(), name.to_owned());
            }
            for (name, value) in extra_http_headers {
                if http_headers
                    .keys()
                    .any(|reserved| reserved.eq_ignore_ascii_case(&name))
                {
                    return Err(McpRuntimeError::Protocol(format!(
                        "MCP per-request header {name:?} collides with a standard header"
                    )));
                }
                http_headers.insert(name, value);
            }
        }
    }

    Ok(McpRequest {
        method: method.to_owned(),
        params,
        era: negotiated.era,
        protocol_version: negotiated.version.clone(),
        http_headers,
    })
}

fn client_info() -> Value {
    json!({"name":"mycel","version":env!("CARGO_PKG_VERSION")})
}

fn mcp_param_headers(
    bindings: &[McpHeaderBinding],
    arguments: &serde_json::Map<String, Value>,
) -> Result<BTreeMap<String, String>, String> {
    let root = Value::Object(arguments.clone());
    let mut headers = BTreeMap::new();
    for binding in bindings {
        let mut value = &root;
        for component in &binding.path {
            let Some(next) = value.get(component) else {
                value = &Value::Null;
                break;
            };
            value = next;
        }
        if value.is_null() {
            continue;
        }
        let raw = match binding.value_kind {
            McpHeaderValueKind::String => value.as_str().map(str::to_owned),
            McpHeaderValueKind::Number => value.as_number().map(ToString::to_string),
            McpHeaderValueKind::Boolean => value.as_bool().map(|value| value.to_string()),
        }
        .ok_or_else(|| {
            format!(
                "argument {} does not match its x-mcp-header primitive type",
                binding.path.join(".")
            )
        })?;
        headers.insert(
            format!("Mcp-Param-{}", binding.header_name),
            encode_param_header_value(&raw),
        );
    }
    Ok(headers)
}

fn encode_param_header_value(value: &str) -> String {
    if plain_header_value(value) && !value.starts_with("=?base64?") {
        value.to_owned()
    } else {
        format!("=?base64?{}?=", BASE64_STANDARD.encode(value.as_bytes()))
    }
}

fn plain_header_value(value: &str) -> bool {
    !value.is_empty()
        && value.trim_matches([' ', '\t']) == value
        && value
            .bytes()
            .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
}

async fn close_peer(peer: &Arc<dyn McpPeer>) {
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, peer.close()).await;
}

fn remaining(deadline: tokio::time::Instant) -> Result<Duration, McpRuntimeError> {
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        Err(McpRuntimeError::Timeout {
            phase: "MCP startup",
            timeout: Duration::ZERO,
        })
    } else {
        Ok(remaining)
    }
}

fn transport_kind(config: &McpServerConfig) -> McpTransport {
    match config {
        McpServerConfig::Stdio { .. } => McpTransport::Stdio,
        McpServerConfig::Http { .. } => McpTransport::Http,
    }
}

fn common_config(config: &McpServerConfig) -> &McpCommonConfig {
    match config {
        McpServerConfig::Stdio { common, .. } | McpServerConfig::Http { common, .. } => common,
    }
}

fn startup_timeout(config: &McpServerConfig, fallback: Duration) -> Duration {
    common_config(config)
        .startup_timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(fallback)
}

fn tool_timeout(config: &McpServerConfig, fallback: Duration) -> Duration {
    common_config(config)
        .tool_timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(fallback)
}

fn enabled_tool_names(config: &McpServerConfig, tools: &[McpToolDefinition]) -> BTreeSet<String> {
    let common = common_config(config);
    let enabled: BTreeSet<_> = common.enabled_tools.iter().cloned().collect();
    let disabled: BTreeSet<_> = common.disabled_tools.iter().cloned().collect();
    tools
        .iter()
        .map(|tool| tool.name.clone())
        .filter(|name| enabled.is_empty() || enabled.contains(name))
        .filter(|name| !disabled.contains(name))
        .collect()
}

fn discovery_record(
    server_name: &str,
    tools: Vec<McpToolDefinition>,
    enabled_names: BTreeSet<String>,
    registered_names: Vec<String>,
    collisions: Vec<McpToolCollision>,
    ttl_ms: Option<u64>,
    cache_scope: Option<String>,
) -> Result<McpDiscovery, McpRuntimeError> {
    let enabled_names: Vec<_> = enabled_names.into_iter().collect();
    let fingerprint = serde_json::to_vec(&json!({
        "tools":tools,
        "enabledNames":enabled_names,
        "registeredNames":registered_names,
        "collisions":collisions,
        "ttlMs":ttl_ms,
        "cacheScope":cache_scope,
    }))
    .map_err(|error| McpRuntimeError::Protocol(error.to_string()))?;
    Ok(McpDiscovery {
        server_name: server_name.to_owned(),
        hash: stable_hash_8(&fingerprint),
        tools,
        enabled_names,
        registered_names,
        ttl_ms,
        cache_scope,
        collisions,
    })
}

fn status_payload(entry: &McpServerEntry) -> McpServerStatusPayload {
    McpServerStatusPayload {
        name: entry.name.clone(),
        transport: entry.transport,
        status: entry.status,
        tool_count: u64::try_from(entry.tool_count).unwrap_or(u64::MAX),
        error: entry.error.clone(),
    }
}

fn validate_server_name(name: &str) -> Result<(), McpRuntimeError> {
    if name.trim().is_empty() {
        return Err(McpRuntimeError::InvalidConfig(
            "MCP server name must not be empty".to_owned(),
        ));
    }
    Ok(())
}

fn resolve_stdio_cwd(config: Option<&str>, default: Option<&Path>) -> Option<PathBuf> {
    let config = config.map(PathBuf::from);
    match (config, default) {
        (Some(config), Some(default)) if config.is_relative() => Some(default.join(config)),
        (Some(config), _) => Some(config),
        (None, Some(default)) => Some(default.to_owned()),
        (None, None) => None,
    }
}

fn resolved_http_headers(
    config: &McpServerConfig,
    configured: &BTreeMap<String, String>,
    environment: &dyn McpEnvironment,
) -> Result<BTreeMap<String, String>, McpRuntimeError> {
    let mut headers = configured.clone();
    let McpServerConfig::Http {
        bearer_token_env_var,
        ..
    } = config
    else {
        return Ok(headers);
    };
    if let Some(variable) = bearer_token_env_var {
        let token = environment
            .get(variable)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| McpRuntimeError::MissingBearerToken(variable.clone()))?;
        let authorization_keys: Vec<_> = headers
            .keys()
            .filter(|name| name.eq_ignore_ascii_case("authorization"))
            .cloned()
            .collect();
        for key in authorization_keys {
            headers.remove(&key);
        }
        headers.insert("Authorization".to_owned(), format!("Bearer {token}"));
    }
    Ok(headers)
}

#[derive(Clone, Default)]
struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    fn clean(&self, value: &str) -> String {
        let mut output = value.to_owned();
        for secret in &self.secrets {
            if !secret.is_empty() {
                output = output.replace(secret, "[REDACTED]");
            }
        }
        take_chars(&output, MAX_DIAGNOSTIC_CHARS)
    }
}

fn build_redactor(
    config: &McpServerConfig,
    environment: &dyn McpEnvironment,
) -> Result<Redactor, McpRuntimeError> {
    let mut secrets = Vec::new();
    match config {
        McpServerConfig::Stdio { env, .. } => {
            for (name, value) in env {
                if looks_sensitive_name(name) && !value.is_empty() {
                    secrets.push(value.clone());
                }
            }
        }
        McpServerConfig::Http {
            headers,
            bearer_token_env_var,
            ..
        } => {
            secrets.extend(headers.values().filter(|value| !value.is_empty()).cloned());
            if let Some(variable) = bearer_token_env_var {
                let token = environment
                    .get(variable)
                    .filter(|token| !token.is_empty())
                    .ok_or_else(|| McpRuntimeError::MissingBearerToken(variable.clone()))?;
                secrets.push(token.clone());
                secrets.push(format!("Bearer {token}"));
            }
        }
    }
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets.dedup();
    Ok(Redactor { secrets })
}

fn looks_sensitive_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "token", "secret", "password", "passwd", "api_key", "apikey", "auth", "cookie",
    ]
    .iter()
    .any(|needle| name.contains(needle))
}

fn take_chars(value: &str, maximum: usize) -> String {
    let mut output: String = value.chars().take(maximum).collect();
    if value.chars().count() > maximum {
        output.push_str("…[diagnostic truncated]");
    }
    output
}

fn redacted_error(error: McpRuntimeError, redactor: &Redactor) -> McpRuntimeError {
    match error {
        McpRuntimeError::Transport(source) => McpRuntimeError::Transport(
            McpTransportError::Failed(redactor.clean(&source.to_string())),
        ),
        McpRuntimeError::Request(source) => McpRuntimeError::Request(match source {
            McpRequestError::JsonRpc {
                code,
                message,
                data: _,
                http_status,
            } => McpRequestError::JsonRpc {
                code,
                message: redactor.clean(&message),
                data: None,
                http_status,
            },
            McpRequestError::Http { status, message } => McpRequestError::Http {
                status,
                message: redactor.clean(&message),
            },
            McpRequestError::Transport(source) => McpRequestError::Transport(
                McpTransportError::Failed(redactor.clean(&source.to_string())),
            ),
        }),
        other => other,
    }
}

fn error_tool_result(message: String) -> ExecutableToolResult {
    ExecutableToolResult {
        output: ExecutableToolOutput::Text(message),
        is_error: true,
        stop_turn: false,
        message: None,
        note: None,
        truncated: false,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum McpRuntimeError {
    #[error("invalid MCP configuration: {0}")]
    InvalidConfig(String),
    #[error("legacy standalone HTTP+SSE MCP transport is not supported; use Streamable HTTP")]
    LegacySseUnsupported,
    #[error("MCP server {0:?} was not found")]
    ServerNotFound(String),
    #[error("MCP server {0:?} is disabled")]
    ServerDisabled(String),
    #[error("MCP server {0:?} is not connected")]
    NotConnected(String),
    #[error("MCP connection attempt for {0:?} became stale")]
    StaleAttempt(String),
    #[error("MCP bearer token environment variable {0:?} is not set or is empty")]
    MissingBearerToken(String),
    #[error("MCP server selected unsupported protocol version {0:?}")]
    UnsupportedProtocol(String),
    #[error("MCP protocol error: {0}")]
    Protocol(String),
    #[error("{phase} timed out after {timeout:?}")]
    Timeout {
        phase: &'static str,
        timeout: Duration,
    },
    #[error("MCP operation was cancelled")]
    Cancelled,
    #[error(transparent)]
    Transport(#[from] McpTransportError),
    #[error(transparent)]
    Request(#[from] McpRequestError),
    #[error(transparent)]
    Registry(#[from] ToolRegistryError),
    #[error("MCP event persistence failed: {0}")]
    EventSink(String),
    #[error("default {0} timeout must be positive")]
    InvalidTimeout(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_sse_is_rejected_explicitly() {
        let error = parse_mcp_server_config(json!({
            "transport":"sse",
            "url":"https://example.test/sse"
        }))
        .expect_err("legacy SSE must fail");
        assert!(matches!(error, McpRuntimeError::LegacySseUnsupported));
    }

    #[test]
    fn bearer_replaces_case_variant_authorization() {
        struct Env;
        impl McpEnvironment for Env {
            fn get(&self, _name: &str) -> Option<String> {
                Some("token".to_owned())
            }
        }
        let config = McpServerConfig::Http {
            url: "https://example.test/mcp".to_owned(),
            headers: BTreeMap::from([("authorization".to_owned(), "old".to_owned())]),
            bearer_token_env_var: Some("TOKEN".to_owned()),
            auth: None,
            common: McpCommonConfig::default(),
        };
        let McpServerConfig::Http { headers, .. } = &config else {
            unreachable!()
        };
        assert_eq!(
            resolved_http_headers(&config, headers, &Env).expect("headers"),
            BTreeMap::from([("Authorization".to_owned(), "Bearer token".to_owned())])
        );
    }

    #[test]
    fn modern_request_carries_stateless_metadata_and_http_headers() {
        let negotiated = NegotiatedProtocol {
            era: McpProtocolEra::Modern,
            version: MODERN_PROTOCOL_VERSION.to_owned(),
            tools_list_changed: false,
        };
        let request = build_request(
            &negotiated,
            McpTransport::Http,
            "tools/call",
            json!({"name":"weather", "arguments":{"city":"Denver"}}),
            BTreeMap::from([("Mcp-Param-City".to_owned(), "Denver".to_owned())]),
        )
        .expect("request");
        assert_eq!(
            request.http_headers.get("MCP-Protocol-Version"),
            Some(&MODERN_PROTOCOL_VERSION.to_owned())
        );
        assert_eq!(
            request.http_headers.get("Mcp-Method"),
            Some(&"tools/call".to_owned())
        );
        assert_eq!(
            request.http_headers.get("Mcp-Name"),
            Some(&"weather".to_owned())
        );
        assert_eq!(
            request.http_headers.get("Mcp-Param-City"),
            Some(&"Denver".to_owned())
        );
        let meta = request.params.get("_meta").expect("modern meta");
        assert_eq!(
            meta.get("io.modelcontextprotocol/protocolVersion"),
            Some(&json!(MODERN_PROTOCOL_VERSION))
        );
        assert!(meta.get("io.modelcontextprotocol/clientInfo").is_some());
        assert!(meta
            .get("io.modelcontextprotocol/clientCapabilities")
            .is_some());
        assert!(!request
            .http_headers
            .keys()
            .any(|name| name.eq_ignore_ascii_case("Mcp-Session-Id")));
    }

    #[test]
    fn legacy_request_has_no_modern_envelope_or_method_header() {
        let negotiated = NegotiatedProtocol {
            era: McpProtocolEra::Legacy,
            version: LEGACY_PROTOCOL_VERSION.to_owned(),
            tools_list_changed: false,
        };
        let request = build_request(
            &negotiated,
            McpTransport::Http,
            "initialize",
            json!({}),
            BTreeMap::new(),
        )
        .expect("request");
        assert!(request.params.get("_meta").is_none());
        assert!(!request.http_headers.contains_key("Mcp-Method"));
        assert_eq!(
            request.http_headers.get("MCP-Protocol-Version"),
            Some(&LEGACY_PROTOCOL_VERSION.to_owned())
        );
    }

    #[test]
    fn http_probe_fallback_is_limited_to_non_authentication_4xx() {
        let http_error = |status| {
            McpRuntimeError::Request(McpRequestError::Http {
                status,
                message: "failed".to_owned(),
            })
        };
        assert!(legacy_probe_signal(&http_error(400), McpTransport::Http));
        assert!(legacy_probe_signal(&http_error(404), McpTransport::Http));
        assert!(!legacy_probe_signal(&http_error(401), McpTransport::Http));
        assert!(!legacy_probe_signal(&http_error(403), McpTransport::Http));
        assert!(!legacy_probe_signal(&http_error(500), McpTransport::Http));
        assert!(!legacy_probe_signal(
            &McpRuntimeError::Timeout {
                phase: "probe",
                timeout: Duration::from_millis(1),
            },
            McpTransport::Http,
        ));
    }

    #[test]
    fn stdio_disposable_probe_falls_back_on_any_unrecognized_failure() {
        assert!(legacy_probe_signal(
            &McpRuntimeError::Request(McpRequestError::JsonRpc {
                code: -32601,
                message: "method not found".to_owned(),
                data: None,
                http_status: None,
            }),
            McpTransport::Stdio,
        ));
        assert!(legacy_probe_signal(
            &McpRuntimeError::Request(McpRequestError::Transport(McpTransportError::Closed)),
            McpTransport::Stdio,
        ));
    }

    #[test]
    fn modern_version_error_never_downgrades_to_legacy() {
        let error = McpRuntimeError::Request(McpRequestError::JsonRpc {
            code: -32022,
            message: "unsupported".to_owned(),
            data: Some(json!({"supported":[MODERN_PROTOCOL_VERSION, LEGACY_PROTOCOL_VERSION]})),
            http_status: Some(400),
        });
        let versions = modern_versions_from_error(&error).expect("recognized modern error");
        let selected = select_modern_version(&versions, None).expect("modern version");
        assert_eq!(selected.era, McpProtocolEra::Modern);
        assert_eq!(selected.version, MODERN_PROTOCOL_VERSION);
    }

    #[test]
    fn x_mcp_headers_reject_bad_tools_and_encode_unsafe_values() {
        let invalid = json!({
            "name":"bad",
            "inputSchema":{
                "type":"object",
                "properties":{"region":{"type":"string","x-mcp-header":"Bad Name"}}
            }
        });
        assert!(parse_tool_definition(&invalid, true)
            .expect("invalid annotation excludes only this tool")
            .is_none());

        let valid = json!({
            "name":"good",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "region":{"type":"string","x-mcp-header":"Region"},
                    "load":{"type":"number","x-mcp-header":"Load"}
                }
            }
        });
        let tool = parse_tool_definition(&valid, true)
            .expect("definition")
            .expect("valid tool");
        let headers = mcp_param_headers(
            &tool.http_header_bindings,
            json!({"region":" Hello, 世界 ","load":42.5})
                .as_object()
                .expect("arguments"),
        )
        .expect("headers");
        assert_eq!(headers.get("Mcp-Param-Load"), Some(&"42.5".to_owned()));
        assert_eq!(
            headers.get("Mcp-Param-Region"),
            Some(&format!(
                "=?base64?{}?=",
                BASE64_STANDARD.encode(" Hello, 世界 ".as_bytes())
            ))
        );
    }

    #[test]
    fn x_mcp_header_outside_properties_excludes_the_tool() {
        let invalid = json!({
            "name":"bad-placement",
            "inputSchema":{
                "type":"object",
                "allOf":[{"x-mcp-header":"Region","type":"string"}]
            }
        });
        assert!(parse_tool_definition(&invalid, true)
            .expect("definition parsing")
            .is_none());
    }

    #[test]
    fn request_debug_never_exposes_params_or_header_values() {
        let request = McpRequest {
            method: "tools/call".to_owned(),
            params: json!({"arguments":{"token":"super-secret-argument"}}),
            era: McpProtocolEra::Modern,
            protocol_version: MODERN_PROTOCOL_VERSION.to_owned(),
            http_headers: BTreeMap::from([(
                "Authorization".to_owned(),
                "Bearer super-secret-header".to_owned(),
            )]),
        };
        let rendered = format!("{request:?}");
        assert!(rendered.contains("Authorization"));
        assert!(!rendered.contains("super-secret-argument"));
        assert!(!rendered.contains("super-secret-header"));
    }

    #[test]
    fn malformed_discover_response_is_fatal_not_legacy() {
        let error = parse_discover_result(&json!({
            "supportedVersions":"not-an-array",
            "capabilities":{}
        }))
        .expect_err("malformed modern response");
        assert!(matches!(error, McpRuntimeError::Protocol(_)));
    }
}
