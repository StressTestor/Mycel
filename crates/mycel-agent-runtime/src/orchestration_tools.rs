//! Executable orchestration built-ins backed by the durable runtime reducers.
//!
//! Provider/session adapters supply [`NativeSubagentHost`]. The built-ins own
//! capability checks, durable lifecycle state, cancellation and private task
//! artifacts; they never shell out to another agent CLI.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    future::Future,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

use mycel_agent_protocol::{
    ExecutableToolOutput, ExecutableToolResult, ThinkingEffort, ToolDefinition, ToolInputDisplay,
    ToolUpdate,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    validate_json_schema, BackgroundKind, BackgroundMode, BackgroundRegistry, BackgroundStatus,
    BackgroundTaskState, CancellationToken, CapabilitySet, CronFire, CronScheduler, ExclusiveTool,
    ForegroundProcessPort, ForegroundProcessTask, GoalBudgetPort, GoalOrchestrator, HyphaeReducer,
    ManifestPermissions, OrchestrationPorts, PlanPolicy, PromotionGate, ResolvedWorkflowPlan,
    SubagentRegistry, SubagentStatus, SwarmMemberKind, SwarmPlanner, ToolError, ToolExecutionSpec,
    ToolFuture, ToolInvocation, ToolPrepareContext, ToolRegistry, ToolRegistryError,
    ToolUpdateSink, WorkerProfile, WorkflowArgValue, WorkflowManifest, WorkflowManifestStatus,
    WorkflowManifestStore, WorkflowPlan, WorkflowRunRequest, WorkflowRunner,
    WorkflowWorkerExecutor, WorkflowWorkerFuture, WorkflowWorkerRequest, MAX_SWARM_FAN_OUT,
    PROGRAMMATIC_WORKFLOW_WORKER_CAP,
};

pub const NATIVE_DELEGATE_TOOL: &str = "Agent";
pub const HYPHAE_TOOL_NAME: &str = "Hyphae";
pub const ORCHESTRATION_TOOL_NAMES: [&str; 14] = [
    "Agent",
    "AgentSwarm",
    "Workflow",
    "CreateGoal",
    "GetGoal",
    "UpdateGoal",
    "TaskList",
    "TaskOutput",
    "TaskStop",
    "TaskDetach",
    "CronCreate",
    "CronList",
    "CronDelete",
    HYPHAE_TOOL_NAME,
];

const TASK_LOG_LIMIT: u64 = 8 * 1024 * 1024;
const TASK_OUTPUT_READ_LIMIT: usize = 128 * 1024;
const AGENT_RESULT_LIMIT: usize = 200_000;
const MANIFEST_LIMIT: u64 = 32 * 1024 * 1024;
const MANIFEST_COUNT_LIMIT: usize = 256;
const DEFAULT_AGENT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const DEFAULT_WORKFLOW_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const DEFAULT_CANCELLATION_GRACE: Duration = Duration::from_secs(2);

pub type NativeAgentFuture =
    Pin<Box<dyn Future<Output = Result<NativeAgentResult, String>> + Send + 'static>>;
pub type NativeStopFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'static>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeAgentOperation {
    Spawn { profile: WorkerProfile },
    Resume { agent_id: String },
}

#[derive(Clone)]
pub struct NativeAgentRequest {
    pub agent_id: String,
    pub parent_agent_id: String,
    pub description: String,
    pub prompt: String,
    pub operation: NativeAgentOperation,
    pub cancellation: CancellationToken,
    pub output: Arc<dyn NativeAgentOutputSink>,
}

impl std::fmt::Debug for NativeAgentRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeAgentRequest")
            .field("agent_id", &self.agent_id)
            .field("parent_agent_id", &self.parent_agent_id)
            .field("description", &self.description)
            .field("prompt_chars", &self.prompt.chars().count())
            .field("operation", &self.operation)
            .field("cancelled", &self.cancellation.is_cancelled())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeAgentResult {
    pub output: String,
}

pub trait NativeAgentOutputSink: Send + Sync {
    fn append(&self, text: &str) -> Result<(), String>;
}

pub trait NativeSubagentHost: Send + Sync {
    /// Execute a child through the host's native session/turn engine. The
    /// adapter must preserve the child profile's tool, hook and permission
    /// path; returning simulated output violates this contract.
    fn execute(&self, request: NativeAgentRequest) -> NativeAgentFuture;

    /// Request bounded shutdown of a running native child. Implementations
    /// must be idempotent because session shutdown can race a tool stop.
    fn stop(&self, agent_id: String, reason: String) -> NativeStopFuture;
}

pub trait TaskOutputStore: Send + Sync {
    fn prepare(&self, task_id: &str) -> Result<(), String>;
    fn append(&self, task_id: &str, text: &str) -> Result<(), String>;
    fn read(&self, task_id: &str, maximum_bytes: usize) -> Result<TaskOutput, String>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskOutput {
    pub task_id: String,
    pub text: String,
    pub truncated: bool,
    pub path: Option<PathBuf>,
}

/// Private on-disk task log store. IDs are reduced to a restricted basename,
/// logs are capped, and directory/file modes are reasserted on every open.
pub struct FilesystemTaskOutputStore {
    root: PathBuf,
    lock: Mutex<()>,
}

impl FilesystemTaskOutputStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, String> {
        let root = root.as_ref().to_path_buf();
        create_private_directory(&root)?;
        Ok(Self {
            root,
            lock: Mutex::new(()),
        })
    }

    fn path(&self, task_id: &str) -> Result<PathBuf, String> {
        validate_artifact_id(task_id)?;
        Ok(self.root.join(format!("{task_id}.log")))
    }
}

impl TaskOutputStore for FilesystemTaskOutputStore {
    fn prepare(&self, task_id: &str) -> Result<(), String> {
        let _guard = lock(&self.lock);
        let path = self.path(task_id)?;
        let _file = open_private_append(&path)?;
        Ok(())
    }

    fn append(&self, task_id: &str, text: &str) -> Result<(), String> {
        let _guard = lock(&self.lock);
        let path = self.path(task_id)?;
        let mut file = open_private_append(&path)?;
        let length = file.metadata().map_err(file_error)?.len();
        if length >= TASK_LOG_LIMIT {
            return Ok(());
        }
        let remaining = usize::try_from(TASK_LOG_LIMIT - length).unwrap_or(usize::MAX);
        let bytes = text.as_bytes();
        let selected = &bytes[..bytes.len().min(remaining)];
        file.write_all(selected).map_err(file_error)?;
        file.flush().map_err(file_error)
    }

    fn read(&self, task_id: &str, maximum_bytes: usize) -> Result<TaskOutput, String> {
        let _guard = lock(&self.lock);
        let path = self.path(task_id)?;
        if maximum_bytes == 0 || maximum_bytes > TASK_OUTPUT_READ_LIMIT {
            return Err(format!(
                "task output limit must be between 1 and {TASK_OUTPUT_READ_LIMIT} bytes"
            ));
        }
        let metadata = fs::symlink_metadata(&path).map_err(file_error)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err("task output is not a regular file".to_owned());
        }
        let truncated = metadata.len() > maximum_bytes as u64;
        let mut bytes = Vec::with_capacity(maximum_bytes);
        fs::File::open(&path)
            .map_err(file_error)?
            .take(maximum_bytes as u64)
            .read_to_end(&mut bytes)
            .map_err(file_error)?;
        Ok(TaskOutput {
            task_id: task_id.to_owned(),
            text: String::from_utf8_lossy(&bytes).into_owned(),
            truncated,
            path: Some(path),
        })
    }
}

/// JSON manifest store used by the executable Workflow built-in.
pub struct FilesystemWorkflowManifestStore {
    root: PathBuf,
    sequence: AtomicU64,
    lock: Mutex<()>,
}

impl FilesystemWorkflowManifestStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, String> {
        let root = root.as_ref().to_path_buf();
        create_private_directory(&root)?;
        Ok(Self {
            root,
            sequence: AtomicU64::new(0),
            lock: Mutex::new(()),
        })
    }

    fn path(&self, run_id: &str) -> Result<PathBuf, String> {
        validate_artifact_id(run_id)?;
        Ok(self.root.join(format!("{run_id}.json")))
    }
}

impl WorkflowManifestStore for FilesystemWorkflowManifestStore {
    fn load(&self) -> Result<Vec<WorkflowManifest>, String> {
        let _guard = lock(&self.lock);
        let mut paths = Vec::new();
        for entry in fs::read_dir(&self.root).map_err(file_error)? {
            let path = entry.map_err(file_error)?.path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                paths.push(path);
            }
        }
        paths.sort();
        if paths.len() > MANIFEST_COUNT_LIMIT {
            return Err("workflow manifest count exceeds its safety limit".to_owned());
        }
        let mut manifests = Vec::with_capacity(paths.len());
        for path in paths {
            let metadata = fs::symlink_metadata(&path).map_err(file_error)?;
            if !metadata.file_type().is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() > MANIFEST_LIMIT
            {
                return Err(format!("unsafe workflow manifest {}", path.display()));
            }
            let bytes = fs::read(&path).map_err(file_error)?;
            manifests.push(serde_json::from_slice(&bytes).map_err(|error| {
                format!("invalid workflow manifest {}: {error}", path.display())
            })?);
        }
        Ok(manifests)
    }

    fn write(
        &self,
        manifest: &WorkflowManifest,
        permissions: ManifestPermissions,
    ) -> Result<(), String> {
        if permissions != ManifestPermissions::private() {
            return Err("workflow manifests must use private permissions".to_owned());
        }
        let bytes = serde_json::to_vec_pretty(manifest).map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MANIFEST_LIMIT {
            return Err("workflow manifest exceeds its safety limit".to_owned());
        }
        let _guard = lock(&self.lock);
        create_private_directory(&self.root)?;
        let path = self.path(&manifest.run_id)?;
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let temporary = self
            .root
            .join(format!(".{}.{}.tmp", manifest.run_id, sequence));
        write_private_new(&temporary, &bytes)?;
        if let Err(error) = fs::rename(&temporary, &path) {
            let _ = fs::remove_file(&temporary);
            return Err(file_error(error));
        }
        set_private_file_mode(&path)?;
        sync_directory(&self.root)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct OrchestrationStartupState {
    /// Reserved for a future live-executor adoption seam. The current
    /// in-process composition accepts only an empty startup state and marks
    /// persisted running work lost; it never leaves phantom running tasks.
    pub active_task_ids: BTreeSet<String>,
    pub active_agent_ids: BTreeSet<String>,
    pub active_workflow_ids: BTreeSet<String>,
    pub task_agents: BTreeMap<String, String>,
}

pub struct OrchestrationBuiltinConfig {
    pub ports: OrchestrationPorts,
    pub host: Arc<dyn NativeSubagentHost>,
    pub artifact_root: PathBuf,
    pub root_agent_id: String,
    pub root_capabilities: CapabilitySet,
    pub profiles: BTreeMap<String, WorkerProfile>,
    pub default_profile: String,
    pub max_swarm_fan_out: usize,
    pub max_swarm_concurrency: usize,
    pub workflow_worker_cap: usize,
    pub agent_timeout: Duration,
    pub workflow_timeout: Duration,
    pub cancellation_grace: Duration,
    pub current_effort: Option<ThinkingEffort>,
    pub xhigh_supported: bool,
    pub startup: OrchestrationStartupState,
}

pub struct OrchestrationDependencies {
    pub ports: OrchestrationPorts,
    pub host: Arc<dyn NativeSubagentHost>,
    pub artifact_root: PathBuf,
}

impl OrchestrationDependencies {
    pub fn new(
        ports: OrchestrationPorts,
        host: Arc<dyn NativeSubagentHost>,
        artifact_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            ports,
            host,
            artifact_root: artifact_root.into(),
        }
    }
}

pub struct OrchestrationRootConfig {
    pub agent_id: String,
    pub capabilities: CapabilitySet,
    pub profiles: BTreeMap<String, WorkerProfile>,
    pub default_profile: String,
}

impl OrchestrationRootConfig {
    pub fn new(
        agent_id: impl Into<String>,
        capabilities: CapabilitySet,
        profiles: BTreeMap<String, WorkerProfile>,
        default_profile: impl Into<String>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            capabilities,
            profiles,
            default_profile: default_profile.into(),
        }
    }
}

impl OrchestrationBuiltinConfig {
    pub fn new(dependencies: OrchestrationDependencies, root: OrchestrationRootConfig) -> Self {
        Self {
            ports: dependencies.ports,
            host: dependencies.host,
            artifact_root: dependencies.artifact_root,
            root_agent_id: root.agent_id,
            root_capabilities: root.capabilities,
            profiles: root.profiles,
            default_profile: root.default_profile,
            max_swarm_fan_out: MAX_SWARM_FAN_OUT,
            max_swarm_concurrency: 8,
            workflow_worker_cap: PROGRAMMATIC_WORKFLOW_WORKER_CAP,
            agent_timeout: DEFAULT_AGENT_TIMEOUT,
            workflow_timeout: DEFAULT_WORKFLOW_TIMEOUT,
            cancellation_grace: DEFAULT_CANCELLATION_GRACE,
            current_effort: None,
            xhigh_supported: false,
            startup: OrchestrationStartupState::default(),
        }
    }
}

#[derive(Clone)]
pub struct OrchestrationBuiltins {
    core: Arc<OrchestrationCore>,
}

impl OrchestrationBuiltins {
    pub fn tick_cron(&self, idle: bool) -> Result<Vec<CronFire>, OrchestrationToolError> {
        self.core.cron.tick(idle).map_err(runtime_error)
    }

    pub fn native_delegate_tool(&self) -> Arc<dyn crate::ExecutableTool> {
        Arc::new(OrchestrationTool {
            kind: OrchestrationToolKind::Agent,
            core: Arc::clone(&self.core),
        })
    }

    pub fn finish_hyphae_task(&self) -> Result<crate::HyphaeState, OrchestrationToolError> {
        self.core.hyphae.finish_task().map_err(runtime_error)
    }

    /// Canonical durable goal driver for turn-usage accounting.
    pub fn goal_driver(&self) -> Arc<GoalOrchestrator> {
        Arc::clone(&self.core.goals)
    }

    /// Object-safe composition seam for the retained `SetGoalBudget` tool.
    pub fn goal_budget_port(&self) -> Arc<dyn GoalBudgetPort> {
        self.core.goals.clone()
    }

    pub fn foreground_process_port(&self) -> Arc<dyn ForegroundProcessPort> {
        Arc::new(OrchestrationProcessPort {
            core: Arc::clone(&self.core),
        })
    }

    /// Detach every currently running foreground process or subagent, newest
    /// first, and release its waiting tool call only after the durable mode
    /// transition succeeds.
    pub fn detach_foreground_tasks(
        &self,
        keep_alive: bool,
    ) -> Result<Vec<BackgroundTaskState>, OrchestrationToolError> {
        let mut tasks = self
            .core
            .background
            .list(true)
            .into_iter()
            .filter(|task| {
                task.mode == BackgroundMode::Foreground
                    && matches!(
                        task.kind,
                        BackgroundKind::Process | BackgroundKind::Subagent
                    )
            })
            .collect::<Vec<_>>();
        tasks.sort_by(|left, right| {
            right
                .started_at_ms
                .cmp(&left.started_at_ms)
                .then_with(|| right.id.cmp(&left.id))
        });
        let mut detached = Vec::with_capacity(tasks.len());
        for task in tasks {
            if detach_active_task(&self.core, &task.id, keep_alive)? {
                detached.push(
                    self.core
                        .background
                        .get(&task.id)
                        .ok_or_else(|| runtime_error("detached task disappeared from registry"))?,
                );
            }
        }
        Ok(detached)
    }

    /// Cancel and durably settle background work selected by the shutdown
    /// policy. Host cancellation is acknowledged before a task is marked
    /// killed; failures leave that task available for normal reconciliation.
    pub async fn shutdown(
        &self,
        policy: crate::BackgroundShutdown,
    ) -> Result<Vec<String>, OrchestrationToolError> {
        let tasks = self
            .core
            .background
            .list(true)
            .into_iter()
            .filter(|task| shutdown_selects(policy, task.mode))
            .collect::<Vec<_>>();
        let mut stopped = Vec::new();
        let mut failures = Vec::new();
        for task in tasks {
            let Some(active) = lock(&self.core.active_tasks).get(&task.id).cloned() else {
                failures.push(format!("task {:?} has no live executor", task.id));
                continue;
            };
            lock(&self.core.stopping_tasks).insert(task.id.clone());
            active.cancellation.cancel();
            let stop = if let Some(agent_id) = active.agent_id.as_deref() {
                stop_native_for_shutdown(
                    self.core.host.as_ref(),
                    self.core.cancellation_grace,
                    agent_id,
                )
                .await
            } else {
                Ok(())
            };
            if let Err(error) = stop {
                lock(&self.core.stopping_tasks).remove(&task.id);
                failures.push(format!("task {:?}: {error}", task.id));
                continue;
            }
            if let Err(error) = self.core.background.settle(
                &task.id,
                BackgroundStatus::Killed,
                Some("session shutdown"),
            ) {
                lock(&self.core.stopping_tasks).remove(&task.id);
                failures.push(format!("task {:?}: {error}", task.id));
                continue;
            }
            lock(&self.core.active_tasks).remove(&task.id);
            lock(&self.core.stopping_tasks).remove(&task.id);
            if let Some(agent_id) = active.agent_id {
                if let Err(error) = self.core.subagents.cancel(&agent_id, "session shutdown") {
                    let terminal = self
                        .core
                        .subagents
                        .get(&agent_id)
                        .is_some_and(|agent| agent.status.is_terminal());
                    if !terminal {
                        failures.push(format!("subagent {agent_id:?}: {error}"));
                        continue;
                    }
                }
            }
            stopped.push(task.id);
        }
        if failures.is_empty() {
            Ok(stopped)
        } else {
            Err(runtime_error(format!(
                "orchestration shutdown was incomplete: {}",
                failures.join("; ")
            )))
        }
    }

    /// Best-effort synchronous cancellation used by the owning bundle's Drop
    /// path. Call [`Self::shutdown`] to wait for durable settlement.
    pub fn request_shutdown(&self, policy: crate::BackgroundShutdown) -> Vec<String> {
        let selected = self
            .core
            .background
            .list(true)
            .into_iter()
            .filter(|task| shutdown_selects(policy, task.mode))
            .map(|task| task.id)
            .collect::<Vec<_>>();
        let active = lock(&self.core.active_tasks);
        for id in &selected {
            if let Some(task) = active.get(id) {
                task.cancellation.cancel();
            }
        }
        selected
    }
}

async fn stop_native_for_shutdown(
    host: &dyn NativeSubagentHost,
    cancellation_grace: Duration,
    agent_id: &str,
) -> Result<(), String> {
    // A timed-out stop future is dropped. The host contract is explicitly
    // idempotent, so retry once to finish cleanup if the first future was
    // descheduled after it had already claimed the child. A second timeout is
    // still a hard failure; shutdown must not report an unacknowledged child
    // as killed.
    for attempt in 0..2 {
        match tokio::time::timeout(
            cancellation_grace,
            host.stop(agent_id.to_owned(), "session shutdown".to_owned()),
        )
        .await
        {
            Ok(result) => return result,
            Err(_) if attempt == 0 => {}
            Err(_) => {
                return Err(
                    "native host shutdown acknowledgement timed out after one retry".to_owned(),
                )
            }
        }
    }
    unreachable!("bounded shutdown retry loop always returns")
}

fn shutdown_selects(policy: crate::BackgroundShutdown, mode: BackgroundMode) -> bool {
    match (policy, mode) {
        (crate::BackgroundShutdown::StopAll, _) | (_, BackgroundMode::Foreground) => true,
        (
            crate::BackgroundShutdown::StopUnlessKeepAlive,
            BackgroundMode::Detached { keep_alive },
        ) => !keep_alive,
    }
}

/// CLI slash-command adapters should transform `/delegate text` into this
/// native Agent call. No external Claude/Codex process is involved.
pub fn native_delegate_arguments(prompt: impl Into<String>) -> Value {
    let prompt = prompt.into();
    json!({
        "prompt": prompt,
        "description": "delegated task",
        "run_in_background": false
    })
}

pub fn register_orchestration_builtins(
    registry: &ToolRegistry,
    config: OrchestrationBuiltinConfig,
) -> Result<OrchestrationBuiltins, OrchestrationToolError> {
    validate_config(&config)?;
    let existing = registry
        .snapshot()
        .definitions()
        .iter()
        .map(|definition| definition.name.clone())
        .collect::<BTreeSet<_>>();
    if let Some(collision) = ORCHESTRATION_TOOL_NAMES
        .iter()
        .find(|name| existing.contains(**name))
    {
        return Err(ToolRegistryError::Duplicate((*collision).to_owned()).into());
    }
    let task_outputs: Arc<dyn TaskOutputStore> = Arc::new(FilesystemTaskOutputStore::open(
        config.artifact_root.join("tasks"),
    )?);
    let manifests: Arc<dyn WorkflowManifestStore> = Arc::new(
        FilesystemWorkflowManifestStore::open(config.artifact_root.join("workflows"))?,
    );

    let subagents = Arc::new(SubagentRegistry::open(config.ports.clone())?);
    match subagents.get(&config.root_agent_id) {
        None => {
            subagents.register_root(&config.root_agent_id, config.root_capabilities.clone())?;
        }
        Some(root)
            if root.parent_id.is_none()
                && root.status == SubagentStatus::Running
                && root.capabilities == config.root_capabilities => {}
        Some(_) => {
            return Err(OrchestrationToolError::Config(
                "persisted root agent does not match the configured running root".to_owned(),
            ));
        }
    }
    let background = Arc::new(BackgroundRegistry::open(config.ports.clone())?);
    let goals = Arc::new(GoalOrchestrator::open(config.ports.clone())?);
    let cron = Arc::new(CronScheduler::open(config.ports.clone())?);
    let hyphae = Arc::new(HyphaeReducer::open(
        config.ports.clone(),
        config.current_effort.clone(),
    )?);
    let sequence = Arc::new(AtomicU64::new(0));
    let workflow_executor = Arc::new(NativeWorkflowExecutor {
        host: Arc::clone(&config.host),
        subagents: Arc::clone(&subagents),
        root_agent_id: config.root_agent_id.clone(),
        output_store: Arc::clone(&task_outputs),
        cancellation_grace: config.cancellation_grace,
    });
    let workflow_runner = Arc::new(WorkflowRunner::new(
        config.ports.clone(),
        workflow_executor,
        Arc::clone(&manifests),
    ));

    // Every reducer persists its lost transition before publishing it.
    background.reconcile(&config.startup.active_task_ids)?;
    subagents.reconcile(&config.startup.active_agent_ids)?;
    workflow_runner.reconcile_lost(&config.startup.active_workflow_ids)?;

    let active_tasks = config
        .startup
        .task_agents
        .into_iter()
        .map(|(task_id, agent_id)| {
            (
                task_id,
                ActiveTask {
                    cancellation: CancellationToken::new(),
                    agent_id: Some(agent_id),
                    detach: CancellationToken::new(),
                },
            )
        })
        .collect();
    let core = Arc::new(OrchestrationCore {
        ports: config.ports,
        host: config.host,
        task_outputs,
        manifests,
        subagents,
        background,
        goals,
        cron,
        hyphae,
        workflow_runner,
        root_agent_id: config.root_agent_id,
        profiles: config.profiles,
        default_profile: config.default_profile,
        swarm: SwarmPlanner::new(config.max_swarm_fan_out, config.max_swarm_concurrency)?,
        workflow_worker_cap: config.workflow_worker_cap,
        agent_timeout: config.agent_timeout,
        workflow_timeout: config.workflow_timeout,
        cancellation_grace: config.cancellation_grace,
        xhigh_supported: config.xhigh_supported,
        sequence,
        active_tasks: Mutex::new(active_tasks),
        stopping_tasks: Mutex::new(BTreeSet::new()),
    });
    let tools = OrchestrationToolKind::ALL
        .into_iter()
        .map(|kind| {
            Arc::new(OrchestrationTool {
                kind,
                core: Arc::clone(&core),
            }) as Arc<dyn crate::ExecutableTool>
        })
        .collect();
    registry.replace_batch(&BTreeSet::new(), tools)?;
    Ok(OrchestrationBuiltins { core })
}

struct OrchestrationCore {
    ports: OrchestrationPorts,
    host: Arc<dyn NativeSubagentHost>,
    task_outputs: Arc<dyn TaskOutputStore>,
    manifests: Arc<dyn WorkflowManifestStore>,
    subagents: Arc<SubagentRegistry>,
    background: Arc<BackgroundRegistry>,
    goals: Arc<GoalOrchestrator>,
    cron: Arc<CronScheduler>,
    hyphae: Arc<HyphaeReducer>,
    workflow_runner: Arc<WorkflowRunner>,
    root_agent_id: String,
    profiles: BTreeMap<String, WorkerProfile>,
    default_profile: String,
    swarm: SwarmPlanner,
    workflow_worker_cap: usize,
    agent_timeout: Duration,
    workflow_timeout: Duration,
    cancellation_grace: Duration,
    xhigh_supported: bool,
    sequence: Arc<AtomicU64>,
    active_tasks: Mutex<BTreeMap<String, ActiveTask>>,
    stopping_tasks: Mutex<BTreeSet<String>>,
}

#[derive(Clone)]
struct ActiveTask {
    cancellation: CancellationToken,
    agent_id: Option<String>,
    detach: CancellationToken,
}

struct TaskLogSink {
    task_id: String,
    store: Arc<dyn TaskOutputStore>,
}

struct ProcessTaskUpdateSink {
    task_id: String,
    store: Arc<dyn TaskOutputStore>,
}

impl ToolUpdateSink for ProcessTaskUpdateSink {
    fn emit(&self, update: ToolUpdate) {
        if let Some(text) = update.text.filter(|text| !text.is_empty()) {
            let _ = self.store.append(&self.task_id, &text);
        }
    }
}

struct OrchestrationProcessPort {
    core: Arc<OrchestrationCore>,
}

impl ForegroundProcessPort for OrchestrationProcessPort {
    fn register(
        &self,
        description: &str,
        timeout: Duration,
    ) -> Result<ForegroundProcessTask, String> {
        let task_id =
            allocate_background_id(&self.core, "process").map_err(|error| error.to_string())?;
        self.core
            .task_outputs
            .prepare(&task_id)
            .map_err(|error| error.to_string())?;
        self.core
            .background
            .register(
                &task_id,
                BackgroundKind::Process,
                description,
                BackgroundMode::Foreground,
                duration_ms(timeout),
            )
            .map_err(|error| error.to_string())?;
        let cancellation = CancellationToken::new();
        let detach = CancellationToken::new();
        lock(&self.core.active_tasks).insert(
            task_id.clone(),
            ActiveTask {
                cancellation: cancellation.clone(),
                agent_id: None,
                detach: detach.clone(),
            },
        );
        Ok(ForegroundProcessTask {
            task_id: task_id.clone(),
            cancellation,
            detach,
            updates: Arc::new(ProcessTaskUpdateSink {
                task_id,
                store: Arc::clone(&self.core.task_outputs),
            }),
        })
    }

    fn settle(
        &self,
        task_id: &str,
        status: BackgroundStatus,
        reason: Option<&str>,
    ) -> Result<(), String> {
        let owned = lock(&self.core.active_tasks).remove(task_id).is_some();
        let task = self
            .core
            .background
            .get(task_id)
            .ok_or_else(|| "foreground process disappeared from registry".to_owned())?;
        if !owned {
            return if task.status.is_terminal() {
                Ok(())
            } else {
                Err("foreground process lost its live executor".to_owned())
            };
        }
        if task.status == BackgroundStatus::Running {
            self.core
                .background
                .settle(task_id, status, reason)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

impl NativeAgentOutputSink for TaskLogSink {
    fn append(&self, text: &str) -> Result<(), String> {
        self.store.append(&self.task_id, text)
    }
}

#[derive(Default)]
struct MemoryOutputSink {
    text: Mutex<String>,
}

impl NativeAgentOutputSink for MemoryOutputSink {
    fn append(&self, text: &str) -> Result<(), String> {
        let mut current = lock(&self.text);
        append_bounded(&mut current, text, AGENT_RESULT_LIMIT);
        Ok(())
    }
}

struct NativeWorkflowExecutor {
    host: Arc<dyn NativeSubagentHost>,
    subagents: Arc<SubagentRegistry>,
    root_agent_id: String,
    output_store: Arc<dyn TaskOutputStore>,
    cancellation_grace: Duration,
}

impl WorkflowWorkerExecutor for NativeWorkflowExecutor {
    fn execute(&self, request: WorkflowWorkerRequest) -> WorkflowWorkerFuture {
        let host = Arc::clone(&self.host);
        let subagents = Arc::clone(&self.subagents);
        let parent_agent_id = self.root_agent_id.clone();
        let output_store = Arc::clone(&self.output_store);
        let grace = self.cancellation_grace;
        Box::pin(async move {
            let agent_id = format!("{}-{}", request.run_id, request.task_id);
            let task_id = workflow_task_log_id(&request.run_id, &request.task_id);
            output_store.prepare(&task_id)?;
            subagents
                .spawn(&agent_id, &parent_agent_id, request.profile.clone(), true)
                .map_err(|error| error.to_string())?;
            let sink: Arc<dyn NativeAgentOutputSink> = Arc::new(TaskLogSink {
                task_id: task_id.clone(),
                store: Arc::clone(&output_store),
            });
            let result = execute_native_bounded(
                Arc::clone(&host),
                NativeAgentRequest {
                    agent_id: agent_id.clone(),
                    parent_agent_id,
                    description: request.description,
                    prompt: request.prompt,
                    operation: NativeAgentOperation::Spawn {
                        profile: request.profile,
                    },
                    cancellation: request.cancellation.clone(),
                    output: sink,
                },
                None,
                grace,
                true,
            )
            .await;
            match &result {
                Ok(output) => {
                    let output = truncate_chars(&output.output, AGENT_RESULT_LIMIT);
                    output_store.append(&task_id, &output)?;
                    subagents
                        .finish(&agent_id, SubagentStatus::Completed, None)
                        .map_err(|error| error.to_string())?;
                    Ok(output)
                }
                Err(error) => {
                    if let Err(settle_error) =
                        subagents.finish(&agent_id, SubagentStatus::Failed, Some(error))
                    {
                        return Err(format!(
                            "{error}; failed to settle workflow child: {settle_error}"
                        ));
                    }
                    Err(error.clone())
                }
            }
        })
    }
}

#[derive(Clone, Copy)]
enum OrchestrationToolKind {
    Agent,
    AgentSwarm,
    Workflow,
    CreateGoal,
    GetGoal,
    UpdateGoal,
    TaskList,
    TaskOutput,
    TaskStop,
    TaskDetach,
    CronCreate,
    CronList,
    CronDelete,
    Hyphae,
}

impl OrchestrationToolKind {
    const ALL: [Self; 14] = [
        Self::Agent,
        Self::AgentSwarm,
        Self::Workflow,
        Self::CreateGoal,
        Self::GetGoal,
        Self::UpdateGoal,
        Self::TaskList,
        Self::TaskOutput,
        Self::TaskStop,
        Self::TaskDetach,
        Self::CronCreate,
        Self::CronList,
        Self::CronDelete,
        Self::Hyphae,
    ];
}

struct OrchestrationTool {
    kind: OrchestrationToolKind,
    core: Arc<OrchestrationCore>,
}

impl crate::ExecutableTool for OrchestrationTool {
    fn definition(&self) -> ToolDefinition {
        tool_definition(self.kind)
    }

    fn validate_arguments(&self, arguments: &Value) -> Result<(), ToolError> {
        validate_json_schema(&self.definition().parameters, arguments)?;
        match self.kind {
            OrchestrationToolKind::Agent => {
                let args: AgentArgs = parse_arguments(arguments)?;
                if args.resume.is_some() && args.subagent_type.is_some() {
                    return Err(invalid_arguments(
                        "resume and subagent_type are mutually exclusive",
                    ));
                }
            }
            OrchestrationToolKind::AgentSwarm => {
                let args: AgentSwarmArgs = parse_arguments(arguments)?;
                if args.items.is_empty() && args.resume_agent_ids.is_empty() {
                    return Err(invalid_arguments(
                        "items or resume_agent_ids must contain work",
                    ));
                }
            }
            OrchestrationToolKind::CreateGoal => {
                let args: CreateGoalArgs = parse_arguments(arguments)?;
                if args.queue && args.replace {
                    return Err(invalid_arguments(
                        "queue and replace are mutually exclusive",
                    ));
                }
            }
            OrchestrationToolKind::Hyphae => {
                let args: HyphaeArgs = parse_arguments(arguments)?;
                if args.finish_task && !args.command.trim().is_empty() {
                    return Err(invalid_arguments(
                        "finish_task cannot be combined with a command",
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn prepare(
        &self,
        arguments: &Value,
        _context: &ToolPrepareContext,
    ) -> Result<ToolExecutionSpec, ToolError> {
        prepare_spec(self.kind, arguments)
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation) -> ToolFuture<'a> {
        Box::pin(async move {
            let result = match self.kind {
                OrchestrationToolKind::Agent => self.execute_agent(&invocation).await,
                OrchestrationToolKind::AgentSwarm => self.execute_swarm(&invocation).await,
                OrchestrationToolKind::Workflow => self.execute_workflow(&invocation).await,
                OrchestrationToolKind::CreateGoal => self.create_goal(&invocation.arguments),
                OrchestrationToolKind::GetGoal => self.get_goal(),
                OrchestrationToolKind::UpdateGoal => self.update_goal(&invocation.arguments),
                OrchestrationToolKind::TaskList => self.task_list(&invocation.arguments),
                OrchestrationToolKind::TaskOutput => self.task_output(&invocation.arguments).await,
                OrchestrationToolKind::TaskStop => self.task_stop(&invocation.arguments).await,
                OrchestrationToolKind::TaskDetach => self.task_detach(&invocation.arguments),
                OrchestrationToolKind::CronCreate => self.cron_create(&invocation.arguments),
                OrchestrationToolKind::CronList => self.cron_list(),
                OrchestrationToolKind::CronDelete => self.cron_delete(&invocation.arguments),
                OrchestrationToolKind::Hyphae => self.hyphae(&invocation.arguments),
            };
            Ok(match result {
                Ok(result) => result,
                Err(error) => error_result(error.to_string()),
            })
        })
    }
}

impl OrchestrationTool {
    async fn execute_agent(
        &self,
        invocation: &ToolInvocation,
    ) -> Result<ExecutableToolResult, OrchestrationToolError> {
        let args: AgentArgs = parse_arguments(&invocation.arguments)?;
        let description = args
            .description
            .unwrap_or_else(|| summarize(&args.prompt, 120));
        let operation;
        let agent_id;
        if let Some(resume_id) = args.resume {
            self.core
                .subagents
                .get(&resume_id)
                .ok_or_else(|| runtime_error("agent to resume was not found"))?;
            agent_id = resume_id.clone();
            operation = NativeAgentOperation::Resume {
                agent_id: resume_id,
            };
        } else {
            let profile_name = args
                .subagent_type
                .as_deref()
                .unwrap_or(&self.core.default_profile);
            let profile = self.profile(profile_name)?.clone();
            agent_id = self.new_agent_id("agent")?;
            operation = NativeAgentOperation::Spawn {
                profile: profile.clone(),
            };
        }

        let task_id = self.new_task_id("agent")?;
        self.core.task_outputs.prepare(&task_id)?;
        let mode = if args.run_in_background {
            BackgroundMode::Detached {
                keep_alive: args.keep_alive,
            }
        } else {
            BackgroundMode::Foreground
        };
        self.core.background.register(
            &task_id,
            BackgroundKind::Subagent,
            &description,
            mode,
            duration_ms(self.core.agent_timeout),
        )?;
        if let Err(error) = self.activate_agent(&agent_id, &operation, args.run_in_background) {
            let reason = error.to_string();
            if let Err(settle_error) =
                self.core
                    .background
                    .settle(&task_id, BackgroundStatus::Failed, Some(&reason))
            {
                return Err(runtime_error(format!(
                    "{error}; failed to settle rejected agent task: {settle_error}"
                )));
            }
            return Err(error);
        }
        let cancellation = CancellationToken::new();
        let detach = CancellationToken::new();
        lock(&self.core.active_tasks).insert(
            task_id.clone(),
            ActiveTask {
                cancellation: cancellation.clone(),
                agent_id: Some(agent_id.clone()),
                detach: detach.clone(),
            },
        );
        let request = NativeAgentRequest {
            agent_id: agent_id.clone(),
            parent_agent_id: self.core.root_agent_id.clone(),
            description,
            prompt: args.prompt,
            operation,
            cancellation: cancellation.clone(),
            output: Arc::new(TaskLogSink {
                task_id: task_id.clone(),
                store: Arc::clone(&self.core.task_outputs),
            }),
        };

        if args.run_in_background {
            spawn_background_agent(
                Arc::clone(&self.core),
                task_id.clone(),
                request,
                None,
                false,
            );
            return text_json(json!({
                "taskId": task_id,
                "agentId": agent_id,
                "status": "running",
                "native": true
            }));
        }

        let (completion_sender, mut completion) = tokio::sync::oneshot::channel();
        spawn_background_agent(
            Arc::clone(&self.core),
            task_id.clone(),
            request,
            Some(completion_sender),
            true,
        );
        tokio::select! {
            biased;
            _ = detach.cancelled() => text_json(json!({
                "taskId": task_id,
                "agentId": agent_id,
                "status": "running",
                "native": true
            })),
            result = &mut completion => foreground_agent_result(result),
            _ = invocation.cancellation.cancelled() => {
                cancellation.cancel();
                let result = completion.await.map_err(|_| {
                    runtime_error("native child completion channel closed after cancellation")
                })?;
                match result {
                    Ok(_) => Err(runtime_error("native agent cancelled")),
                    Err(error) => Err(runtime_error(error)),
                }
            }
        }
    }

    async fn execute_swarm(
        &self,
        invocation: &ToolInvocation,
    ) -> Result<ExecutableToolResult, OrchestrationToolError> {
        let args: AgentSwarmArgs = parse_arguments(&invocation.arguments)?;
        let profile = self.profile(&args.subagent_type)?.clone();
        ensure_nonrecursive_profile(&profile)?;
        let plan = self.core.swarm.plan(
            &args.description,
            &args.subagent_type,
            &args.items,
            &args.prompt_template,
            &args.resume_agent_ids,
        )?;
        let mut ordered = Vec::with_capacity(plan.members.len());
        for wave in plan.waves() {
            let mut tasks = tokio::task::JoinSet::new();
            for member in wave {
                let host = Arc::clone(&self.core.host);
                let subagents = Arc::clone(&self.core.subagents);
                let parent_id = self.core.root_agent_id.clone();
                let default_profile = profile.clone();
                let cancellation = invocation.cancellation.clone();
                let grace = self.core.cancellation_grace;
                let timeout = self.core.agent_timeout;
                let generated_id = self.new_agent_id("swarm")?;
                tasks.spawn(async move {
                    let (agent_id, operation, profile) = match &member.kind {
                        SwarmMemberKind::Spawn { .. } => {
                            subagents
                                .spawn(&generated_id, &parent_id, default_profile.clone(), false)
                                .map_err(|error| error.to_string())?;
                            (
                                generated_id,
                                NativeAgentOperation::Spawn {
                                    profile: default_profile.clone(),
                                },
                                default_profile,
                            )
                        }
                        SwarmMemberKind::Resume { agent_id } => {
                            let state = subagents
                                .get(agent_id)
                                .ok_or_else(|| "swarm resume agent was not found".to_owned())?;
                            let resumed_profile = WorkerProfile {
                                name: state.profile_name,
                                capabilities: state.capabilities,
                                allow_delegation: state.allow_delegation,
                            };
                            ensure_nonrecursive_profile(&resumed_profile)
                                .map_err(|error| error.to_string())?;
                            subagents
                                .resume(agent_id)
                                .map_err(|error| error.to_string())?;
                            (
                                agent_id.clone(),
                                NativeAgentOperation::Resume {
                                    agent_id: agent_id.clone(),
                                },
                                resumed_profile,
                            )
                        }
                    };
                    let request = NativeAgentRequest {
                        agent_id: agent_id.clone(),
                        parent_agent_id: parent_id,
                        description: "swarm member".to_owned(),
                        prompt: member.prompt.clone(),
                        operation,
                        cancellation,
                        output: Arc::new(MemoryOutputSink::default()),
                    };
                    let result =
                        execute_native_bounded(host, request, Some(timeout), grace, true).await;
                    let result = match result {
                        Ok(output) => subagents
                            .finish(&agent_id, SubagentStatus::Completed, None)
                            .map(|_| output)
                            .map_err(|error| {
                                format!("failed to settle completed swarm child: {error}")
                            }),
                        Err(error) => {
                            match subagents.finish(&agent_id, SubagentStatus::Failed, Some(&error))
                            {
                                Ok(_) => Err(error),
                                Err(settle_error) => Err(format!(
                                    "{error}; failed to settle swarm child: {settle_error}"
                                )),
                            }
                        }
                    };
                    let rendered = match result {
                        Ok(result) => json!({
                            "index": member.index,
                            "agentId": agent_id,
                            "profile": profile.name,
                            "status": "completed",
                            "output": truncate_chars(&result.output, AGENT_RESULT_LIMIT)
                        }),
                        Err(error) => json!({
                            "index": member.index,
                            "agentId": agent_id,
                            "profile": profile.name,
                            "status": "failed",
                            "error": error
                        }),
                    };
                    Ok::<_, String>((member.index, rendered))
                });
            }
            while let Some(joined) = tasks.join_next().await {
                match joined {
                    Ok(Ok(result)) => ordered.push(result),
                    Ok(Err(error)) => return Err(runtime_error(error)),
                    Err(error) => return Err(runtime_error(error.to_string())),
                }
            }
            if invocation.cancellation.is_cancelled() {
                return Err(runtime_error("swarm cancelled"));
            }
        }
        ordered.sort_by_key(|(index, _)| *index);
        text_json(json!({
            "description": plan.description,
            "members": ordered.into_iter().map(|(_, value)| value).collect::<Vec<_>>()
        }))
    }

    async fn execute_workflow(
        &self,
        invocation: &ToolInvocation,
    ) -> Result<ExecutableToolResult, OrchestrationToolError> {
        let args: WorkflowArgs = parse_arguments(&invocation.arguments)?;
        let plan = WorkflowPlan::parse_json(&serde_json::to_string(&args.plan)?)?;
        let resolved = plan.resolve(
            &args.arguments,
            &self.core.profiles,
            self.core.workflow_worker_cap,
        )?;
        let run_id = self.new_workflow_run_id()?;
        let task_id = self.new_task_id("workflow")?;
        self.core.task_outputs.prepare(&task_id)?;
        self.core.background.register(
            &task_id,
            BackgroundKind::Workflow,
            &resolved.description,
            BackgroundMode::Detached {
                keep_alive: args.keep_alive,
            },
            duration_ms(self.core.workflow_timeout),
        )?;
        let cancellation = CancellationToken::new();
        lock(&self.core.active_tasks).insert(
            task_id.clone(),
            ActiveTask {
                cancellation: cancellation.clone(),
                agent_id: None,
                detach: CancellationToken::new(),
            },
        );
        spawn_background_workflow(
            Arc::clone(&self.core),
            task_id.clone(),
            run_id.clone(),
            resolved,
            cancellation,
        );
        text_json(json!({
            "taskId": task_id,
            "runId": run_id,
            "status": "running",
            "native": true
        }))
    }

    fn create_goal(
        &self,
        arguments: &Value,
    ) -> Result<ExecutableToolResult, OrchestrationToolError> {
        let args: CreateGoalArgs = parse_arguments(arguments)?;
        let id = args.id.unwrap_or_else(|| self.new_entity_id("goal"));
        let value = if args.queue {
            serde_json::to_value(self.core.goals.enqueue(&id, &args.objective)?)?
        } else {
            serde_json::to_value(self.core.goals.create(&id, &args.objective, args.replace)?)?
        };
        text_json(value)
    }

    fn get_goal(&self) -> Result<ExecutableToolResult, OrchestrationToolError> {
        text_json(serde_json::to_value(self.core.goals.snapshot())?)
    }

    fn update_goal(
        &self,
        arguments: &Value,
    ) -> Result<ExecutableToolResult, OrchestrationToolError> {
        let args: UpdateGoalArgs = parse_arguments(arguments)?;
        let (value, stop_turn) = match args.action.as_str() {
            "pause" => {
                self.core.goals.pause(args.reason.as_deref())?;
                (serde_json::to_value(self.core.goals.snapshot())?, false)
            }
            "resume" => {
                self.core.goals.resume()?;
                (serde_json::to_value(self.core.goals.snapshot())?, false)
            }
            "block" => {
                self.core.goals.block(required_reason(&args)?)?;
                (serde_json::to_value(self.core.goals.snapshot())?, true)
            }
            "complete" => (
                serde_json::to_value(self.core.goals.complete(required_reason(&args)?)?)?,
                true,
            ),
            "cancel" => (
                serde_json::to_value(self.core.goals.cancel(args.reason.as_deref())?)?,
                true,
            ),
            "next" => (serde_json::to_value(self.core.goals.next()?)?, true),
            "promote" => {
                let gate = args.promotion_gate.unwrap_or_default().into_gate();
                (
                    serde_json::to_value(self.core.goals.promote_next(gate)?)?,
                    false,
                )
            }
            _ => return Err(runtime_error("unknown goal action")),
        };
        let mut result = text_json(value)?;
        result.stop_turn = stop_turn;
        Ok(result)
    }

    fn task_list(&self, arguments: &Value) -> Result<ExecutableToolResult, OrchestrationToolError> {
        let args: TaskListArgs = parse_arguments(arguments)?;
        text_json(serde_json::to_value(
            self.core.background.list(args.active_only),
        )?)
    }

    async fn task_output(
        &self,
        arguments: &Value,
    ) -> Result<ExecutableToolResult, OrchestrationToolError> {
        let args: TaskOutputArgs = parse_arguments(arguments)?;
        if args.block {
            let wait = Duration::from_millis(args.wait_ms);
            let deadline = tokio::time::Instant::now() + wait;
            loop {
                let task = self
                    .core
                    .background
                    .get(&args.task_id)
                    .ok_or_else(|| runtime_error("background task not found"))?;
                if task.status.is_terminal() || tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10).min(wait)).await;
            }
        }
        let task = self
            .core
            .background
            .get(&args.task_id)
            .ok_or_else(|| runtime_error("background task not found"))?;
        let output = self.core.task_outputs.read(
            &args.task_id,
            args.max_bytes.unwrap_or(TASK_OUTPUT_READ_LIMIT),
        )?;
        text_json(json!({"task": task, "output": output}))
    }

    async fn task_stop(
        &self,
        arguments: &Value,
    ) -> Result<ExecutableToolResult, OrchestrationToolError> {
        let args: TaskStopArgs = parse_arguments(arguments)?;
        let task = self
            .core
            .background
            .get(&args.task_id)
            .ok_or_else(|| runtime_error("background task not found"))?;
        if task.status.is_terminal() {
            return Err(runtime_error("background task is already terminal"));
        }
        let active = lock(&self.core.active_tasks).get(&args.task_id).cloned();
        let active = match active {
            Some(active) => active,
            None => {
                let current = self
                    .core
                    .background
                    .get(&args.task_id)
                    .ok_or_else(|| runtime_error("background task not found"))?;
                return Err(if current.status.is_terminal() {
                    runtime_error("background task became terminal before it could be stopped")
                } else {
                    runtime_error("background task has no attached live executor")
                });
            }
        };
        lock(&self.core.stopping_tasks).insert(args.task_id.clone());
        active.cancellation.cancel();
        if let Some(agent_id) = active.agent_id {
            let stop = tokio::time::timeout(
                self.core.cancellation_grace,
                self.core.host.stop(agent_id.clone(), args.reason.clone()),
            )
            .await;
            match stop {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    lock(&self.core.stopping_tasks).remove(&args.task_id);
                    return Err(runtime_error(format!(
                        "the native host rejected task stop: {error}"
                    )));
                }
                Err(_) => {
                    lock(&self.core.stopping_tasks).remove(&args.task_id);
                    return Err(runtime_error(
                        "the native host did not acknowledge task stop within the cancellation grace",
                    ));
                }
            }
            let settled = match self.core.background.settle(
                &args.task_id,
                BackgroundStatus::Killed,
                Some(&args.reason),
            ) {
                Ok(settled) => settled,
                Err(error) => {
                    lock(&self.core.stopping_tasks).remove(&args.task_id);
                    return Err(error.into());
                }
            };
            lock(&self.core.active_tasks).remove(&args.task_id);
            lock(&self.core.stopping_tasks).remove(&args.task_id);
            if let Err(error) =
                self.core
                    .subagents
                    .finish(&agent_id, SubagentStatus::Failed, Some(&args.reason))
            {
                let terminal = self
                    .core
                    .subagents
                    .get(&agent_id)
                    .is_some_and(|agent| agent.status.is_terminal());
                if !terminal {
                    return Err(error.into());
                }
            }
            return text_json(serde_json::to_value(settled)?);
        }
        let settled = match self.core.background.settle(
            &args.task_id,
            BackgroundStatus::Killed,
            Some(&args.reason),
        ) {
            Ok(settled) => settled,
            Err(error) => {
                lock(&self.core.stopping_tasks).remove(&args.task_id);
                return Err(error.into());
            }
        };
        lock(&self.core.active_tasks).remove(&args.task_id);
        lock(&self.core.stopping_tasks).remove(&args.task_id);
        text_json(serde_json::to_value(settled)?)
    }

    fn task_detach(
        &self,
        arguments: &Value,
    ) -> Result<ExecutableToolResult, OrchestrationToolError> {
        let args: TaskDetachArgs = parse_arguments(arguments)?;
        let changed = detach_active_task(&self.core, &args.task_id, args.keep_alive)?;
        text_json(json!({"taskId": args.task_id, "changed": changed}))
    }

    fn cron_create(
        &self,
        arguments: &Value,
    ) -> Result<ExecutableToolResult, OrchestrationToolError> {
        let args: CronCreateArgs = parse_arguments(arguments)?;
        let id = self.new_cron_id()?;
        let task = self
            .core
            .cron
            .schedule(&id, &args.expression, &args.prompt, args.recurring)?;
        text_json(serde_json::to_value(task)?)
    }

    fn cron_list(&self) -> Result<ExecutableToolResult, OrchestrationToolError> {
        text_json(serde_json::to_value(self.core.cron.snapshot())?)
    }

    fn cron_delete(
        &self,
        arguments: &Value,
    ) -> Result<ExecutableToolResult, OrchestrationToolError> {
        let args: CronDeleteArgs = parse_arguments(arguments)?;
        let removed = self.core.cron.remove(&args.id)?;
        text_json(json!({"id": args.id, "removed": removed}))
    }

    fn hyphae(&self, arguments: &Value) -> Result<ExecutableToolResult, OrchestrationToolError> {
        let args: HyphaeArgs = parse_arguments(arguments)?;
        let value = if args.finish_task {
            json!({"state": self.core.hyphae.finish_task()?, "submitPrompt": null, "effortChanged": false})
        } else {
            let transition = self
                .core
                .hyphae
                .apply(&args.command, self.core.xhigh_supported)?;
            json!({
                "state": transition.state,
                "submitPrompt": transition.submit_prompt,
                "effortChanged": transition.effort_changed
            })
        };
        text_json(value)
    }

    fn profile(&self, name: &str) -> Result<&WorkerProfile, OrchestrationToolError> {
        self.core
            .profiles
            .get(name)
            .ok_or_else(|| runtime_error(format!("worker profile {name:?} was not configured")))
    }

    fn new_task_id(&self, prefix: &str) -> Result<String, OrchestrationToolError> {
        allocate_background_id(&self.core, prefix)
    }

    fn new_agent_id(&self, prefix: &str) -> Result<String, OrchestrationToolError> {
        for _ in 0..64 {
            let id = self.new_entity_id(prefix);
            if self.core.subagents.get(&id).is_none() {
                return Ok(id);
            }
        }
        Err(runtime_error("could not allocate a unique agent id"))
    }

    fn new_cron_id(&self) -> Result<String, OrchestrationToolError> {
        let existing = self
            .core
            .cron
            .snapshot()
            .tasks
            .into_iter()
            .map(|task| task.id)
            .collect::<BTreeSet<_>>();
        for _ in 0..64 {
            let id = format!("{:08x}", self.next_id_word() as u32);
            if !existing.contains(&id) {
                return Ok(id);
            }
        }
        Err(runtime_error("could not allocate a unique cron id"))
    }

    fn new_workflow_run_id(&self) -> Result<String, OrchestrationToolError> {
        let existing = self
            .core
            .manifests
            .load()?
            .into_iter()
            .map(|manifest| manifest.run_id)
            .collect::<BTreeSet<_>>();
        for _ in 0..64 {
            let high = self.next_id_word();
            let low = self.next_id_word();
            let value = (u128::from(high) << 64) | u128::from(low);
            let hex = format!("{value:032x}");
            let id = format!(
                "wf-{}-{}-{}-{}-{}",
                &hex[0..8],
                &hex[8..12],
                &hex[12..16],
                &hex[16..20],
                &hex[20..32]
            );
            if !existing.contains(&id) {
                return Ok(id);
            }
        }
        Err(runtime_error("could not allocate a unique workflow run id"))
    }

    fn new_entity_id(&self, prefix: &str) -> String {
        format!("{prefix}-{:08x}", self.next_id_word() as u32)
    }

    fn next_id_word(&self) -> u64 {
        let sequence = self.core.sequence.fetch_add(1, Ordering::Relaxed);
        mix64(self.core.ports.now_ms() ^ sequence.rotate_left(17))
    }

    fn activate_agent(
        &self,
        agent_id: &str,
        operation: &NativeAgentOperation,
        detached: bool,
    ) -> Result<(), OrchestrationToolError> {
        match operation {
            NativeAgentOperation::Spawn { profile } => {
                self.core.subagents.spawn(
                    agent_id,
                    &self.core.root_agent_id,
                    profile.clone(),
                    detached,
                )?;
            }
            NativeAgentOperation::Resume { agent_id } => {
                self.core.subagents.resume(agent_id)?;
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentArgs {
    prompt: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    subagent_type: Option<String>,
    #[serde(default)]
    resume: Option<String>,
    #[serde(default)]
    run_in_background: bool,
    #[serde(default)]
    keep_alive: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentSwarmArgs {
    description: String,
    subagent_type: String,
    prompt_template: String,
    #[serde(default)]
    items: Vec<String>,
    #[serde(default)]
    resume_agent_ids: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowArgs {
    plan: Value,
    #[serde(default)]
    arguments: BTreeMap<String, WorkflowArgValue>,
    #[serde(default)]
    keep_alive: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateGoalArgs {
    objective: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    queue: bool,
    #[serde(default)]
    replace: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateGoalArgs {
    action: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    promotion_gate: Option<PromotionGateArgs>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromotionGateArgs {
    #[serde(default = "yes")]
    session_matches: bool,
    #[serde(default = "yes")]
    idle: bool,
    #[serde(default = "yes")]
    user_queue_empty: bool,
    #[serde(default)]
    dispatch_pending: bool,
    #[serde(default)]
    compacting: bool,
}

impl Default for PromotionGateArgs {
    fn default() -> Self {
        Self {
            session_matches: true,
            idle: true,
            user_queue_empty: true,
            dispatch_pending: false,
            compacting: false,
        }
    }
}

impl PromotionGateArgs {
    const fn into_gate(self) -> PromotionGate {
        PromotionGate {
            session_matches: self.session_matches,
            idle: self.idle,
            user_queue_empty: self.user_queue_empty,
            dispatch_pending: self.dispatch_pending,
            compacting: self.compacting,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskListArgs {
    #[serde(default)]
    active_only: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskOutputArgs {
    task_id: String,
    #[serde(default)]
    block: bool,
    #[serde(default = "default_wait_ms")]
    wait_ms: u64,
    #[serde(default)]
    max_bytes: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskStopArgs {
    task_id: String,
    #[serde(default = "default_stop_reason")]
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskDetachArgs {
    task_id: String,
    #[serde(default)]
    keep_alive: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CronCreateArgs {
    expression: String,
    prompt: String,
    #[serde(default = "yes")]
    recurring: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CronDeleteArgs {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HyphaeArgs {
    #[serde(default)]
    command: String,
    #[serde(default)]
    finish_task: bool,
}

fn tool_definition(kind: OrchestrationToolKind) -> ToolDefinition {
    let (name, description, parameters) = match kind {
        OrchestrationToolKind::Agent => (
            "Agent",
            "Run a native foreground or background subagent with a configured capability profile.",
            json!({
                "type":"object",
                "properties":{
                    "prompt":{"type":"string","minLength":1,"maxLength":100000},
                    "description":{"type":"string","minLength":1,"maxLength":240},
                    "subagent_type":{"type":"string","minLength":1,"maxLength":64},
                    "resume":{"type":"string","minLength":1,"maxLength":160},
                    "run_in_background":{"type":"boolean"},
                    "keep_alive":{"type":"boolean"}
                },
                "required":["prompt"],
                "additionalProperties":false
            }),
        ),
        OrchestrationToolKind::AgentSwarm => (
            "AgentSwarm",
            "Run a bounded native fan-out using a nonrecursive worker profile.",
            json!({
                "type":"object",
                "properties":{
                    "description":{"type":"string","minLength":1,"maxLength":240},
                    "subagent_type":{"type":"string","minLength":1,"maxLength":64},
                    "prompt_template":{"type":"string","minLength":1,"maxLength":100000},
                    "items":{"type":"array","maxItems":MAX_SWARM_FAN_OUT,"items":{"type":"string","minLength":1,"maxLength":10000}},
                    "resume_agent_ids":{"type":"object","maxProperties":MAX_SWARM_FAN_OUT,"additionalProperties":{"type":"string","minLength":1,"maxLength":100000}}
                },
                "required":["description","subagent_type","prompt_template"],
                "additionalProperties":false
            }),
        ),
        OrchestrationToolKind::Workflow => (
            "Workflow",
            "Validate and start a declarative native workflow in the background.",
            json!({
                "type":"object",
                "properties":{
                    "plan":{"type":"object"},
                    "arguments":{"type":"object","additionalProperties":{"type":["string","number","boolean"]}},
                    "keep_alive":{"type":"boolean"}
                },
                "required":["plan"],
                "additionalProperties":false
            }),
        ),
        OrchestrationToolKind::CreateGoal => (
            "CreateGoal",
            "Create, replace, or queue a durable session goal.",
            json!({
                "type":"object",
                "properties":{
                    "objective":{"type":"string","minLength":1,"maxLength":4000},
                    "id":{"type":"string","minLength":1,"maxLength":160},
                    "queue":{"type":"boolean"},
                    "replace":{"type":"boolean"}
                },
                "required":["objective"],
                "additionalProperties":false
            }),
        ),
        OrchestrationToolKind::GetGoal => (
            "GetGoal",
            "Return the current durable goal and queue.",
            empty_schema(),
        ),
        OrchestrationToolKind::UpdateGoal => (
            "UpdateGoal",
            "Pause, resume, block, complete, cancel, replace with next, or promote the durable goal.",
            json!({
                "type":"object",
                "properties":{
                    "action":{"type":"string","enum":["pause","resume","block","complete","cancel","next","promote"]},
                    "reason":{"type":"string","minLength":1,"maxLength":4000},
                    "promotion_gate":{
                        "type":"object",
                        "properties":{
                            "session_matches":{"type":"boolean"},
                            "idle":{"type":"boolean"},
                            "user_queue_empty":{"type":"boolean"},
                            "dispatch_pending":{"type":"boolean"},
                            "compacting":{"type":"boolean"}
                        },
                        "additionalProperties":false
                    }
                },
                "required":["action"],
                "additionalProperties":false
            }),
        ),
        OrchestrationToolKind::TaskList => (
            "TaskList",
            "List durable background tasks.",
            json!({"type":"object","properties":{"active_only":{"type":"boolean"}},"additionalProperties":false}),
        ),
        OrchestrationToolKind::TaskOutput => (
            "TaskOutput",
            "Read bounded output and status for a background task.",
            json!({
                "type":"object",
                "properties":{
                    "task_id":{"type":"string","minLength":10,"maxLength":160},
                    "block":{"type":"boolean"},
                    "wait_ms":{"type":"integer","minimum":1,"maximum":30000},
                    "max_bytes":{"type":"integer","minimum":1,"maximum":TASK_OUTPUT_READ_LIMIT}
                },
                "required":["task_id"],
                "additionalProperties":false
            }),
        ),
        OrchestrationToolKind::TaskStop => (
            "TaskStop",
            "Cooperatively stop a running background task.",
            json!({
                "type":"object",
                "properties":{
                    "task_id":{"type":"string","minLength":10,"maxLength":160},
                    "reason":{"type":"string","minLength":1,"maxLength":4000}
                },
                "required":["task_id"],
                "additionalProperties":false
            }),
        ),
        OrchestrationToolKind::TaskDetach => (
            "TaskDetach",
            "Detach a running task and select whether it survives session shutdown.",
            json!({
                "type":"object",
                "properties":{
                    "task_id":{"type":"string","minLength":10,"maxLength":160},
                    "keep_alive":{"type":"boolean"}
                },
                "required":["task_id"],
                "additionalProperties":false
            }),
        ),
        OrchestrationToolKind::CronCreate => (
            "CronCreate",
            "Create a session-scoped five-field UTC cron task.",
            json!({
                "type":"object",
                "properties":{
                    "expression":{"type":"string","minLength":1,"maxLength":120},
                    "prompt":{"type":"string","minLength":1,"maxLength":100000},
                    "recurring":{"type":"boolean"}
                },
                "required":["expression","prompt"],
                "additionalProperties":false
            }),
        ),
        OrchestrationToolKind::CronList => (
            "CronList",
            "List session-scoped cron tasks.",
            empty_schema(),
        ),
        OrchestrationToolKind::CronDelete => (
            "CronDelete",
            "Delete a session-scoped cron task.",
            json!({
                "type":"object",
                "properties":{"id":{"type":"string","minLength":8,"maxLength":8}},
                "required":["id"],
                "additionalProperties":false
            }),
        ),
        OrchestrationToolKind::Hyphae => (
            "Hyphae",
            "Apply a session-only Hyphae xhigh/swarm mode transition.",
            json!({
                "type":"object",
                "properties":{
                    "command":{"type":"string","maxLength":100000},
                    "finish_task":{"type":"boolean"}
                },
                "additionalProperties":false
            }),
        ),
    };
    ToolDefinition {
        name: name.to_owned(),
        description: description.to_owned(),
        parameters,
        deferred: false,
    }
}

fn prepare_spec(
    kind: OrchestrationToolKind,
    arguments: &Value,
) -> Result<ToolExecutionSpec, ToolError> {
    let (summary, action) = match kind {
        OrchestrationToolKind::Agent => {
            let args: AgentArgs = parse_arguments(arguments)?;
            (
                ToolInputDisplay::AgentCall {
                    agent_name: args
                        .subagent_type
                        .or(args.resume)
                        .unwrap_or_else(|| "default".to_owned()),
                    prompt: args.prompt,
                    background: Some(args.run_in_background),
                },
                "delegate",
            )
        }
        OrchestrationToolKind::TaskStop => {
            let args: TaskStopArgs = parse_arguments(arguments)?;
            (
                ToolInputDisplay::TaskStop {
                    task_id: args.task_id,
                    task_description: args.reason,
                },
                "task_stop",
            )
        }
        OrchestrationToolKind::CreateGoal => {
            let args: CreateGoalArgs = parse_arguments(arguments)?;
            (
                ToolInputDisplay::GoalStart {
                    objective: args.objective,
                    completion_criterion: None,
                    mode: mycel_agent_protocol::GoalStartMode::Manual,
                },
                "goal_create",
            )
        }
        _ => (
            ToolInputDisplay::Generic {
                summary: tool_definition(kind).name,
                detail: Some(arguments.clone()),
            },
            "orchestrate",
        ),
    };
    let mut spec = ToolExecutionSpec::new(summary, action);
    spec.approval_rule = Some(tool_definition(kind).name);
    spec.plan_policy = PlanPolicy::NotInPlan;
    match kind {
        OrchestrationToolKind::AgentSwarm => {
            spec.exclusive_tool = Some(ExclusiveTool::AgentSwarm);
        }
        OrchestrationToolKind::Workflow => {
            spec.exclusive_tool = Some(ExclusiveTool::Workflow);
        }
        OrchestrationToolKind::CreateGoal => {
            spec.create_goal_review = true;
        }
        OrchestrationToolKind::UpdateGoal => {
            let args: UpdateGoalArgs = parse_arguments(arguments)?;
            spec.stop_batch_after_this = matches!(
                args.action.as_str(),
                "block" | "complete" | "cancel" | "next"
            );
        }
        _ => {}
    }
    Ok(spec)
}

fn empty_schema() -> Value {
    json!({"type":"object","properties":{},"additionalProperties":false})
}

async fn execute_native_bounded(
    host: Arc<dyn NativeSubagentHost>,
    request: NativeAgentRequest,
    timeout: Option<Duration>,
    cancellation_grace: Duration,
    stop_on_cancellation: bool,
) -> Result<NativeAgentResult, String> {
    let agent_id = request.agent_id.clone();
    let cancellation = request.cancellation.clone();
    let execution = host.execute(request);
    tokio::pin!(execution);
    let (outcome, stop_required) = if let Some(timeout) = timeout {
        tokio::select! {
            result = &mut execution => return result,
            _ = cancellation.cancelled() => (Err("native agent cancelled".to_owned()), stop_on_cancellation),
            _ = tokio::time::sleep(timeout) => {
                cancellation.cancel();
                (Err("native agent timed out".to_owned()), true)
            },
        }
    } else {
        tokio::select! {
            result = &mut execution => return result,
            _ = cancellation.cancelled() => (Err("native agent cancelled".to_owned()), stop_on_cancellation),
        }
    };
    cancellation.cancel();
    if !stop_required {
        return outcome;
    }
    match tokio::time::timeout(
        cancellation_grace,
        host.stop(agent_id, outcome.clone().unwrap_err()),
    )
    .await
    {
        Ok(Ok(())) | Err(_) => {}
        Ok(Err(error)) => return Err(format!("{}; stop failed: {error}", outcome.unwrap_err())),
    }
    outcome
}

fn detach_active_task(
    core: &OrchestrationCore,
    task_id: &str,
    keep_alive: bool,
) -> Result<bool, OrchestrationToolError> {
    let task = core
        .background
        .get(task_id)
        .ok_or(crate::BackgroundError::NotFound)?;
    let requested_mode = BackgroundMode::Detached { keep_alive };
    if task.mode == requested_mode {
        return Ok(false);
    }
    let active = lock(&core.active_tasks)
        .get(task_id)
        .cloned()
        .ok_or_else(|| runtime_error("background task has no attached live executor"))?;
    let changed = core.background.detach(task_id, keep_alive)?;
    let subagent_result = active
        .agent_id
        .as_deref()
        .map(|agent_id| core.subagents.detach(agent_id))
        .transpose();
    active.detach.cancel();
    subagent_result?;
    Ok(changed)
}

fn allocate_background_id(
    core: &OrchestrationCore,
    prefix: &str,
) -> Result<String, OrchestrationToolError> {
    for _ in 0..64 {
        let sequence = core.sequence.fetch_add(1, Ordering::Relaxed);
        let id = format!(
            "{prefix}-{:08x}",
            mix64(core.ports.now_ms() ^ sequence.rotate_left(17)) as u32
        );
        if core.background.get(&id).is_none() {
            return Ok(id);
        }
    }
    Err(runtime_error("could not allocate a unique background id"))
}

fn spawn_background_agent(
    core: Arc<OrchestrationCore>,
    task_id: String,
    request: NativeAgentRequest,
    completion: Option<tokio::sync::oneshot::Sender<Result<NativeAgentResult, String>>>,
    stop_on_cancellation: bool,
) {
    tokio::spawn(async move {
        let agent_id = request.agent_id.clone();
        let result = execute_native_bounded(
            Arc::clone(&core.host),
            request,
            Some(core.agent_timeout),
            core.cancellation_grace,
            stop_on_cancellation,
        )
        .await;
        let (agent_status, mut task_status, mut reason) = match &result {
            Ok(result) => {
                let output = truncate_chars(&result.output, AGENT_RESULT_LIMIT);
                match core.task_outputs.append(&task_id, &output) {
                    Ok(()) => (SubagentStatus::Completed, BackgroundStatus::Completed, None),
                    Err(error) => (
                        SubagentStatus::Failed,
                        BackgroundStatus::Failed,
                        Some(format!("failed to persist native task output: {error}")),
                    ),
                }
            }
            Err(error) if error.contains("timed out") => (
                SubagentStatus::Failed,
                BackgroundStatus::TimedOut,
                Some(error.clone()),
            ),
            Err(error) => (
                SubagentStatus::Failed,
                BackgroundStatus::Failed,
                Some(error.clone()),
            ),
        };
        if let Err(error) = core
            .subagents
            .finish(&agent_id, agent_status, reason.as_deref())
        {
            let terminal = core
                .subagents
                .get(&agent_id)
                .is_some_and(|agent| agent.status.is_terminal());
            if !terminal {
                let diagnostic = format!("failed to settle native child: {error}");
                let _ = core.task_outputs.append(&task_id, &diagnostic);
                task_status = BackgroundStatus::Failed;
                reason = Some(diagnostic);
            }
        }
        wait_for_stop_decision(&core, &task_id).await;
        let owned = lock(&core.active_tasks).remove(&task_id).is_some();
        if owned
            && core
                .background
                .get(&task_id)
                .is_some_and(|task| task.status == BackgroundStatus::Running)
        {
            if let Err(error) = core
                .background
                .settle(&task_id, task_status, reason.as_deref())
            {
                let _ = core.task_outputs.append(
                    &task_id,
                    &format!("failed to settle background task: {error}"),
                );
            }
        }
        if let Some(completion) = completion {
            let _ = completion.send(result);
        }
    });
}

fn foreground_agent_result(
    result: Result<Result<NativeAgentResult, String>, tokio::sync::oneshot::error::RecvError>,
) -> Result<ExecutableToolResult, OrchestrationToolError> {
    match result {
        Ok(Ok(result)) => Ok(text_result(truncate_chars(
            &result.output,
            AGENT_RESULT_LIMIT,
        ))),
        Ok(Err(error)) => Err(runtime_error(error)),
        Err(_) => Err(runtime_error(
            "native child completion channel closed unexpectedly",
        )),
    }
}

fn spawn_background_workflow(
    core: Arc<OrchestrationCore>,
    task_id: String,
    run_id: String,
    plan: ResolvedWorkflowPlan,
    cancellation: CancellationToken,
) {
    tokio::spawn(async move {
        let result = core
            .workflow_runner
            .run(WorkflowRunRequest {
                run_id,
                plan,
                timeout: core.workflow_timeout,
                cancellation,
            })
            .await;
        let (mut status, mut reason, rendered) = match result {
            Ok(manifest) => {
                let status = match manifest.status {
                    WorkflowManifestStatus::Completed => BackgroundStatus::Completed,
                    WorkflowManifestStatus::TimedOut => BackgroundStatus::TimedOut,
                    WorkflowManifestStatus::Aborted => BackgroundStatus::Killed,
                    WorkflowManifestStatus::Failed | WorkflowManifestStatus::Lost => {
                        BackgroundStatus::Failed
                    }
                    WorkflowManifestStatus::Running => BackgroundStatus::Failed,
                };
                let reason = manifest.error.clone();
                let rendered = serde_json::to_string_pretty(&manifest).unwrap_or_else(|error| {
                    format!("workflow result serialization failed: {error}")
                });
                (status, reason, rendered)
            }
            Err(error) => (
                BackgroundStatus::Failed,
                Some(error.to_string()),
                error.to_string(),
            ),
        };
        if let Err(error) = core.task_outputs.append(&task_id, &rendered) {
            status = BackgroundStatus::Failed;
            reason = Some(format!("failed to persist workflow output: {error}"));
        }
        wait_for_stop_decision(&core, &task_id).await;
        let owned = lock(&core.active_tasks).remove(&task_id).is_some();
        if owned
            && core
                .background
                .get(&task_id)
                .is_some_and(|task| task.status == BackgroundStatus::Running)
        {
            if let Err(error) = core.background.settle(&task_id, status, reason.as_deref()) {
                let _ = core.task_outputs.append(
                    &task_id,
                    &format!("failed to settle workflow background task: {error}"),
                );
            }
        }
    });
}

async fn wait_for_stop_decision(core: &OrchestrationCore, task_id: &str) {
    while lock(&core.stopping_tasks).contains(task_id) {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

fn validate_config(config: &OrchestrationBuiltinConfig) -> Result<(), OrchestrationToolError> {
    if config.root_agent_id.trim().is_empty() {
        return Err(config_error("root agent id must not be empty"));
    }
    if config.agent_timeout.is_zero()
        || config.workflow_timeout.is_zero()
        || config.cancellation_grace.is_zero()
    {
        return Err(config_error(
            "timeouts and cancellation grace must be positive",
        ));
    }
    if !(1..=128).contains(&config.workflow_worker_cap) {
        return Err(config_error(
            "workflow worker cap must be between 1 and 128",
        ));
    }
    if !config.root_capabilities.can_spawn_subagents {
        return Err(config_error(
            "root capabilities must permit native subagents",
        ));
    }
    if !config.profiles.contains_key(&config.default_profile) {
        return Err(config_error("default worker profile was not configured"));
    }
    for (name, profile) in &config.profiles {
        if name != &profile.name {
            return Err(config_error(format!(
                "worker profile key {name:?} does not match its name"
            )));
        }
        if !profile.capabilities.is_subset_of(&config.root_capabilities) {
            return Err(config_error(format!(
                "worker profile {name:?} exceeds root capabilities"
            )));
        }
    }
    if !config.startup.active_task_ids.is_empty()
        || !config.startup.active_agent_ids.is_empty()
        || !config.startup.active_workflow_ids.is_empty()
        || !config.startup.task_agents.is_empty()
    {
        return Err(config_error(
            "live executor adoption is not implemented; startup state must be empty so persisted running work is reconciled lost",
        ));
    }
    Ok(())
}

fn ensure_nonrecursive_profile(profile: &WorkerProfile) -> Result<(), OrchestrationToolError> {
    if profile.allow_delegation
        || profile.capabilities.can_spawn_subagents
        || profile.capabilities.can_swarm
        || profile.capabilities.can_workflow
    {
        return Err(runtime_error(format!(
            "worker profile {:?} permits recursive orchestration",
            profile.name
        )));
    }
    Ok(())
}

fn required_reason(args: &UpdateGoalArgs) -> Result<&str, OrchestrationToolError> {
    args.reason
        .as_deref()
        .ok_or_else(|| runtime_error(format!("goal action {:?} requires reason", args.action)))
}

fn workflow_task_log_id(run_id: &str, task_id: &str) -> String {
    format!(
        "workflow-{:016x}",
        stable_hash(&format!("{run_id}:{task_id}"))
    )
}

fn duration_ms(duration: Duration) -> Option<u64> {
    Some(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn stable_hash(value: &str) -> u64 {
    value.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn summarize(value: &str, maximum: usize) -> String {
    let trimmed = value.trim();
    let summary = truncate_chars(trimmed, maximum);
    if summary.is_empty() {
        "native subagent".to_owned()
    } else {
        summary
    }
}

fn append_bounded(output: &mut String, text: &str, maximum: usize) {
    let current = output.chars().count();
    if current >= maximum {
        return;
    }
    output.extend(text.chars().take(maximum - current));
}

fn truncate_chars(value: &str, maximum: usize) -> String {
    let mut output = value.chars().take(maximum).collect::<String>();
    if value.chars().count() > maximum {
        output.push_str("\n…[output truncated]");
    }
    output
}

fn text_result(text: impl Into<String>) -> ExecutableToolResult {
    ExecutableToolResult {
        output: ExecutableToolOutput::Text(text.into()),
        is_error: false,
        stop_turn: false,
        message: None,
        note: None,
        truncated: false,
    }
}

fn text_json(value: Value) -> Result<ExecutableToolResult, OrchestrationToolError> {
    Ok(text_result(serde_json::to_string_pretty(&value)?))
}

fn error_result(error: impl ToString) -> ExecutableToolResult {
    ExecutableToolResult {
        output: ExecutableToolOutput::Text(error.to_string()),
        is_error: true,
        stop_turn: false,
        message: None,
        note: None,
        truncated: false,
    }
}

fn parse_arguments<T: for<'de> Deserialize<'de>>(value: &Value) -> Result<T, ToolError> {
    serde_json::from_value(value.clone()).map_err(|error| ToolError::InvalidArguments {
        path: "$".to_owned(),
        message: error.to_string(),
    })
}

fn invalid_arguments(message: impl Into<String>) -> ToolError {
    ToolError::InvalidArguments {
        path: "$".to_owned(),
        message: message.into(),
    }
}

fn yes() -> bool {
    true
}

fn default_wait_ms() -> u64 {
    30_000
}

fn default_stop_reason() -> String {
    "stopped by user".to_owned()
}

fn validate_artifact_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 160
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
        || value.starts_with('.')
    {
        Err("invalid artifact id".to_owned())
    } else {
        Ok(())
    }
}

fn create_private_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(file_error)?;
    let metadata = fs::symlink_metadata(path).map_err(file_error)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(format!(
            "artifact root {} is not a directory",
            path.display()
        ));
    }
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(file_error)?;
    Ok(())
}

fn open_private_append(path: &Path) -> Result<fs::File, String> {
    if path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(format!("refusing symlink artifact {}", path.display()));
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(path).map_err(file_error)?;
    if !file.metadata().map_err(file_error)?.is_file() {
        return Err(format!("artifact {} is not a regular file", path.display()));
    }
    set_private_file_mode(path)?;
    Ok(file)
}

fn write_private_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).map_err(file_error)?;
    file.write_all(bytes).map_err(file_error)?;
    file.sync_all().map_err(file_error)
}

fn set_private_file_mode(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(file_error)?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    fs::File::open(path)
        .map_err(file_error)?
        .sync_all()
        .map_err(file_error)?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn file_error(error: std::io::Error) -> String {
    error.to_string()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn config_error(error: impl Into<String>) -> OrchestrationToolError {
    OrchestrationToolError::Config(error.into())
}

fn runtime_error(error: impl ToString) -> OrchestrationToolError {
    OrchestrationToolError::Runtime(error.to_string())
}

#[derive(Debug, thiserror::Error)]
pub enum OrchestrationToolError {
    #[error("invalid orchestration built-in configuration: {0}")]
    Config(String),
    #[error("orchestration execution failed: {0}")]
    Runtime(String),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error(transparent)]
    Registry(#[from] ToolRegistryError),
    #[error(transparent)]
    Background(#[from] crate::BackgroundError),
    #[error(transparent)]
    Goal(#[from] crate::GoalError),
    #[error(transparent)]
    Cron(#[from] crate::CronError),
    #[error(transparent)]
    Hyphae(#[from] crate::HyphaeError),
    #[error(transparent)]
    Subagent(#[from] crate::SubagentError),
    #[error(transparent)]
    Swarm(#[from] crate::SwarmError),
    #[error(transparent)]
    Workflow(#[from] crate::WorkflowError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("artifact store failed: {0}")]
    Artifact(String),
}

impl From<String> for OrchestrationToolError {
    fn from(error: String) -> Self {
        Self::Artifact(error)
    }
}
