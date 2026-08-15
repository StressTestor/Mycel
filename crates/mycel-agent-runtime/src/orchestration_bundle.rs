//! Session-scoped production composition for native orchestration.
//!
//! Provider construction remains injected through [`NativeTurnEngineFactory`]
//! so this crate never depends on a concrete provider implementation.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use mycel_agent_protocol::ThinkingEffort;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::{
    native_delegate_arguments, register_orchestration_builtins, BackgroundShutdown, Clock,
    ExecutableTool, FilesystemOrchestrationStore, GoalBudgetPort, GoalBudgetSnapshot, GoalError,
    GoalOrchestrator, LiveEventSink, NativeChildAgentHost, NativeChildHostDependencies,
    NativeChildHostError, NativeChildHostOptions, NativeSessionOptionsFactory,
    NativeTurnEngineFactory, OrchestrationBuiltinConfig, OrchestrationBuiltins,
    OrchestrationDependencies, OrchestrationPorts, OrchestrationRootConfig, OrchestrationToolError,
    Runtime, SessionHandle, SystemClock, ToolRegistry, ToolRegistryError, ORCHESTRATION_TOOL_NAMES,
};

/// Concrete runtime dependencies a CLI supplies after its parent runtime and
/// shared tool registry exist. Provider-specific child construction is kept
/// behind the two factory traits.
pub struct NativeOrchestrationDependencies {
    pub runtime: Runtime,
    pub registry: ToolRegistry,
    pub live_events: Arc<dyn LiveEventSink>,
    pub clock: Arc<dyn Clock>,
    pub sessions: Arc<dyn NativeSessionOptionsFactory>,
    pub turns: Arc<dyn NativeTurnEngineFactory>,
}

impl NativeOrchestrationDependencies {
    pub fn new(
        runtime: Runtime,
        registry: ToolRegistry,
        live_events: Arc<dyn LiveEventSink>,
        sessions: Arc<dyn NativeSessionOptionsFactory>,
        turns: Arc<dyn NativeTurnEngineFactory>,
    ) -> Self {
        Self {
            runtime,
            registry,
            live_events,
            clock: Arc::new(SystemClock),
            sessions,
            turns,
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }
}

/// Session identity, storage, and root capability policy for one bundle.
pub struct NativeOrchestrationBundleConfig {
    pub parent_session: SessionHandle,
    pub storage_root: PathBuf,
    pub root: OrchestrationRootConfig,
    pub current_effort: Option<ThinkingEffort>,
    pub xhigh_supported: bool,
    pub shutdown_policy: BackgroundShutdown,
}

impl NativeOrchestrationBundleConfig {
    pub fn new(
        parent_session: SessionHandle,
        storage_root: impl Into<PathBuf>,
        root: OrchestrationRootConfig,
    ) -> Self {
        Self {
            parent_session,
            storage_root: storage_root.into(),
            root,
            current_effort: None,
            xhigh_supported: false,
            shutdown_policy: BackgroundShutdown::StopUnlessKeepAlive,
        }
    }

    pub fn with_hyphae(
        mut self,
        current_effort: Option<ThinkingEffort>,
        xhigh_supported: bool,
    ) -> Self {
        self.current_effort = current_effort;
        self.xhigh_supported = xhigh_supported;
        self
    }

    pub fn with_shutdown_policy(mut self, policy: BackgroundShutdown) -> Self {
        self.shutdown_policy = policy;
        self
    }
}

/// Canonical `/delegate` invocation. Pass this through the same tool execution
/// path as provider-originated calls so authorization and hooks remain active.
pub struct NativeDelegateInvocation {
    pub tool: Arc<dyn ExecutableTool>,
    pub arguments: Value,
}

/// Owns all native orchestration state associated with one parent session.
///
/// Construction performs restart reconciliation before registering tools in
/// the shared registry. Call [`Self::shutdown`] before closing the parent
/// session; `Drop` can only request cooperative cancellation and cannot report
/// persistence or host-shutdown failures.
#[must_use = "keep the bundle alive for the parent session and await shutdown"]
pub struct NativeOrchestrationBundle {
    parent_session: SessionHandle,
    registry: ToolRegistry,
    ports: OrchestrationPorts,
    host: NativeChildAgentHost,
    builtins: OrchestrationBuiltins,
    registered_tools: Vec<Arc<dyn ExecutableTool>>,
    state_path: PathBuf,
    artifact_root: PathBuf,
    shutdown_policy: BackgroundShutdown,
    closed: AtomicBool,
    shutdown_gate: Mutex<()>,
}

impl NativeOrchestrationBundle {
    pub fn open(
        dependencies: NativeOrchestrationDependencies,
        config: NativeOrchestrationBundleConfig,
    ) -> Result<Self, NativeOrchestrationBundleError> {
        if config.root.agent_id != config.parent_session.main_agent_id().as_str() {
            return Err(NativeOrchestrationBundleError::Config(format!(
                "orchestration root agent {:?} does not match parent session agent {:?}",
                config.root.agent_id,
                config.parent_session.main_agent_id().as_str()
            )));
        }
        let existing = dependencies.registry.snapshot();
        if let Some(collision) = ORCHESTRATION_TOOL_NAMES
            .iter()
            .find(|name| existing.get(name).is_some())
        {
            return Err(
                OrchestrationToolError::Registry(ToolRegistryError::Duplicate(
                    (*collision).to_owned(),
                ))
                .into(),
            );
        }

        let store = Arc::new(FilesystemOrchestrationStore::open(
            &config.storage_root,
            config.parent_session.id(),
        )?);
        let state_path = store.path().to_path_buf();
        let artifact_root = state_path
            .parent()
            .ok_or_else(|| {
                NativeOrchestrationBundleError::Config(
                    "orchestration state path has no session directory".to_owned(),
                )
            })?
            .join("artifacts");
        let ports = OrchestrationPorts::new(store, dependencies.live_events, dependencies.clock);
        let host_options = NativeChildHostOptions::new(
            config.parent_session.id().as_str(),
            &config.root.agent_id,
            config.root.capabilities.clone(),
        );
        let host = NativeChildAgentHost::open(
            NativeChildHostDependencies {
                runtime: dependencies.runtime,
                ports: ports.clone(),
                sessions: dependencies.sessions,
                turns: dependencies.turns,
            },
            host_options,
        )?;
        let orchestration_dependencies =
            OrchestrationDependencies::new(ports.clone(), Arc::new(host.clone()), &artifact_root);
        let mut builtin_config =
            OrchestrationBuiltinConfig::new(orchestration_dependencies, config.root);
        builtin_config.current_effort = config.current_effort;
        builtin_config.xhigh_supported = config.xhigh_supported;
        let builtins = register_orchestration_builtins(&dependencies.registry, builtin_config)?;
        let registered = dependencies.registry.snapshot();
        let registered_tools = ORCHESTRATION_TOOL_NAMES
            .iter()
            .filter_map(|name| registered.get(name))
            .collect();

        Ok(Self {
            parent_session: config.parent_session,
            registry: dependencies.registry,
            ports,
            host,
            builtins,
            registered_tools,
            state_path,
            artifact_root,
            shutdown_policy: config.shutdown_policy,
            closed: AtomicBool::new(false),
            shutdown_gate: Mutex::new(()),
        })
    }

    pub fn parent_session(&self) -> &SessionHandle {
        &self.parent_session
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    pub fn ports(&self) -> OrchestrationPorts {
        self.ports.clone()
    }

    pub fn native_host(&self) -> NativeChildAgentHost {
        self.host.clone()
    }

    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    pub fn artifact_root(&self) -> &Path {
        &self.artifact_root
    }

    pub fn goal_driver(&self) -> Arc<GoalOrchestrator> {
        self.builtins.goal_driver()
    }

    pub fn goal_budget_port(&self) -> Arc<dyn GoalBudgetPort> {
        self.builtins.goal_budget_port()
    }

    pub fn foreground_process_port(&self) -> Arc<dyn crate::ForegroundProcessPort> {
        self.builtins.foreground_process_port()
    }

    pub fn enforce_goal_budget(
        &self,
    ) -> Result<GoalBudgetSnapshot, NativeOrchestrationBundleError> {
        Ok(self.builtins.goal_driver().enforce_budget()?)
    }

    pub fn record_goal_turn_usage(
        &self,
        tokens: u64,
    ) -> Result<GoalBudgetSnapshot, NativeOrchestrationBundleError> {
        Ok(self.builtins.goal_driver().record_turn_usage(tokens)?)
    }

    pub fn native_delegate_invocation(
        &self,
        prompt: impl Into<String>,
    ) -> Result<NativeDelegateInvocation, NativeOrchestrationBundleError> {
        self.ensure_open()?;
        let tool = self.builtins.native_delegate_tool();
        let arguments = native_delegate_arguments(prompt);
        tool.validate_arguments(&arguments)?;
        Ok(NativeDelegateInvocation { tool, arguments })
    }

    pub fn finish_hyphae_task(&self) -> Result<crate::HyphaeState, NativeOrchestrationBundleError> {
        self.ensure_open()?;
        Ok(self.builtins.finish_hyphae_task()?)
    }

    pub fn detach_foreground_tasks(
        &self,
        keep_alive: bool,
    ) -> Result<Vec<crate::BackgroundTaskState>, NativeOrchestrationBundleError> {
        self.ensure_open()?;
        Ok(self.builtins.detach_foreground_tasks(keep_alive)?)
    }

    pub fn tick_cron(
        &self,
        idle: bool,
    ) -> Result<Vec<crate::CronFire>, NativeOrchestrationBundleError> {
        self.ensure_open()?;
        Ok(self.builtins.tick_cron(idle)?)
    }

    /// Stop selected work and wait for durable settlement. Failures are
    /// returned and the bundle remains open so callers may retry or diagnose.
    pub async fn shutdown(
        &self,
        policy: BackgroundShutdown,
    ) -> Result<Vec<String>, NativeOrchestrationBundleError> {
        let _guard = self.shutdown_gate.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Ok(Vec::new());
        }
        let stopped = self.builtins.shutdown(policy).await?;
        self.closed.store(true, Ordering::Release);
        self.unregister_tools();
        Ok(stopped)
    }

    pub fn is_shutdown(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn ensure_open(&self) -> Result<(), NativeOrchestrationBundleError> {
        if self.is_shutdown() {
            Err(NativeOrchestrationBundleError::Closed)
        } else {
            Ok(())
        }
    }

    fn unregister_tools(&self) {
        for tool in &self.registered_tools {
            self.registry.unregister_if_same(tool);
        }
    }
}

impl Drop for NativeOrchestrationBundle {
    fn drop(&mut self) {
        if !self.closed.load(Ordering::Acquire) {
            self.builtins.request_shutdown(self.shutdown_policy);
        }
        self.unregister_tools();
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NativeOrchestrationBundleError {
    #[error("native orchestration bundle configuration failed: {0}")]
    Config(String),
    #[error("native orchestration bundle is shut down")]
    Closed,
    #[error("orchestration store failed: {0}")]
    Store(String),
    #[error(transparent)]
    Host(#[from] NativeChildHostError),
    #[error(transparent)]
    Tools(#[from] OrchestrationToolError),
    #[error(transparent)]
    Goal(#[from] GoalError),
    #[error(transparent)]
    Tool(#[from] crate::ToolError),
}

impl From<String> for NativeOrchestrationBundleError {
    fn from(error: String) -> Self {
        Self::Store(error)
    }
}
