use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{CancellationToken, OrchestrationError, OrchestrationPorts, WorkerProfile};

const WORKFLOW_SCOPE: &str = "workflow";
const MAX_PHASES: usize = 32;
const MAX_TASKS: usize = 128;
const MAX_TASKS_PER_PHASE: usize = 64;
const MAX_PROMPT_CHARS: usize = 100_000;
const MAX_EXPANDED_PROMPT_CHARS: usize = 200_000;
const MAX_ARGUMENTS: usize = 64;
const MAX_ARGUMENT_BYTES: usize = 100_000;
pub const PROGRAMMATIC_WORKFLOW_WORKER_CAP: usize = 3;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WorkflowArgValue {
    String(String),
    Number(serde_json::Number),
    Bool(bool),
}

impl std::fmt::Display for WorkflowArgValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::String(value) => formatter.write_str(value),
            Self::Number(value) => write!(formatter, "{value}"),
            Self::Bool(value) => write!(formatter, "{value}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPlanTask {
    pub id: String,
    pub description: String,
    pub prompt: String,
    #[serde(default = "default_worker_profile", alias = "subagent_type")]
    pub worker_profile: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPlanPhase {
    pub title: String,
    pub tasks: Vec<WorkflowPlanTask>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPlan {
    pub version: u8,
    pub name: String,
    pub description: String,
    pub phases: Vec<WorkflowPlanPhase>,
}

impl WorkflowPlan {
    pub fn parse_json(input: &str) -> Result<Self, WorkflowError> {
        let plan: Self = serde_json::from_str(input).map_err(WorkflowError::Parse)?;
        plan.validate_structure()?;
        Ok(plan)
    }

    pub fn resolve(
        &self,
        args: &BTreeMap<String, WorkflowArgValue>,
        profiles: &BTreeMap<String, WorkerProfile>,
        max_workers: usize,
    ) -> Result<ResolvedWorkflowPlan, WorkflowError> {
        self.validate_structure()?;
        if !(1..=MAX_TASKS).contains(&max_workers) {
            return Err(WorkflowError::InvalidWorkerCap);
        }
        let task_count = self.task_count();
        if task_count > max_workers {
            return Err(WorkflowError::WorkerCapExceeded {
                declared: task_count,
                limit: max_workers,
            });
        }
        validate_worker_profiles(self, profiles)?;
        if args.len() > MAX_ARGUMENTS {
            return Err(WorkflowError::TooManyArguments);
        }
        let argument_bytes = args.iter().fold(0usize, |total, (key, value)| {
            total
                .saturating_add(key.len())
                .saturating_add(value.to_string().len())
        });
        if argument_bytes > MAX_ARGUMENT_BYTES {
            return Err(WorkflowError::ArgumentsTooLarge);
        }
        for (key, value) in args {
            if matches!(value, WorkflowArgValue::String(text) if contains_reserved_start(text)) {
                return Err(WorkflowError::ArgumentContainsPlaceholder(key.clone()));
            }
        }
        let mut used = BTreeSet::new();
        let mut resolved = self.clone();
        for phase in &mut resolved.phases {
            for task in &mut phase.tasks {
                task.prompt = interpolate_args(&task.prompt, args, &mut used)?;
            }
        }
        let unused = args
            .keys()
            .filter(|key| !used.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        if !unused.is_empty() {
            return Err(WorkflowError::UnusedArguments(unused));
        }
        // Substitution cannot introduce a result reference; validate again as
        // a defence in depth if argument rules are changed later.
        resolved.validate_structure()?;
        Ok(ResolvedWorkflowPlan {
            plan: resolved,
            profiles: profiles.clone(),
        })
    }

    fn task_count(&self) -> usize {
        self.phases.iter().map(|phase| phase.tasks.len()).sum()
    }

    fn validate_structure(&self) -> Result<(), WorkflowError> {
        if self.version != 1 {
            return Err(WorkflowError::UnsupportedVersion(self.version));
        }
        if !valid_workflow_name(&self.name) {
            return Err(WorkflowError::InvalidName);
        }
        if self.description.trim().is_empty() || self.description.chars().count() > 240 {
            return Err(WorkflowError::InvalidDescription);
        }
        if self.phases.is_empty() || self.phases.len() > MAX_PHASES {
            return Err(WorkflowError::InvalidPhaseCount);
        }
        if self.task_count() > MAX_TASKS {
            return Err(WorkflowError::TooManyTasks);
        }
        let mut task_phases = BTreeMap::new();
        for (phase_index, phase) in self.phases.iter().enumerate() {
            if phase.title.trim().is_empty() || phase.title.chars().count() > 120 {
                return Err(WorkflowError::InvalidPhaseTitle);
            }
            if phase.tasks.is_empty() || phase.tasks.len() > MAX_TASKS_PER_PHASE {
                return Err(WorkflowError::InvalidPhaseTaskCount);
            }
            for task in &phase.tasks {
                if !valid_task_id(&task.id) {
                    return Err(WorkflowError::InvalidTaskId(task.id.clone()));
                }
                if task_phases.insert(task.id.clone(), phase_index).is_some() {
                    return Err(WorkflowError::DuplicateTaskId(task.id.clone()));
                }
                if task.description.trim().is_empty() || task.description.chars().count() > 160 {
                    return Err(WorkflowError::InvalidTaskDescription(task.id.clone()));
                }
                if task.prompt.trim().is_empty() || task.prompt.chars().count() > MAX_PROMPT_CHARS {
                    return Err(WorkflowError::InvalidTaskPrompt(task.id.clone()));
                }
                parse_placeholders(&task.prompt)?;
            }
        }
        for (phase_index, phase) in self.phases.iter().enumerate() {
            for task in &phase.tasks {
                for placeholder in parse_placeholders(&task.prompt)? {
                    if let Placeholder::Result(id) = placeholder {
                        let result_phase = task_phases
                            .get(&id)
                            .ok_or_else(|| WorkflowError::UnknownResult(id.clone()))?;
                        if *result_phase >= phase_index {
                            return Err(WorkflowError::ForwardResult {
                                task: task.id.clone(),
                                result: id,
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedWorkflowPlan {
    pub plan: WorkflowPlan,
    pub profiles: BTreeMap<String, WorkerProfile>,
}

impl std::ops::Deref for ResolvedWorkflowPlan {
    type Target = WorkflowPlan;

    fn deref(&self) -> &Self::Target {
        &self.plan
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManifestPermissions {
    pub directory_mode: u32,
    pub file_mode: u32,
}

impl ManifestPermissions {
    pub const fn private() -> Self {
        Self {
            directory_mode: 0o700,
            file_mode: 0o600,
        }
    }
}

pub trait WorkflowManifestStore: Send + Sync {
    fn load(&self) -> Result<Vec<WorkflowManifest>, String>;
    fn write(
        &self,
        manifest: &WorkflowManifest,
        permissions: ManifestPermissions,
    ) -> Result<(), String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowManifestStatus {
    Running,
    Completed,
    Failed,
    TimedOut,
    Aborted,
    Lost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowTaskStatus {
    Completed,
    Failed,
    Aborted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowTaskResult {
    pub task_id: String,
    pub phase_index: usize,
    pub status: WorkflowTaskStatus,
    pub result: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowManifest {
    pub version: u8,
    pub run_id: String,
    pub workflow_name: String,
    pub description: String,
    pub status: WorkflowManifestStatus,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    pub current_phase: Option<usize>,
    pub phase_titles: Vec<String>,
    pub results: Vec<WorkflowTaskResult>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct WorkflowWorkerRequest {
    pub run_id: String,
    pub phase_index: usize,
    pub task_id: String,
    pub description: String,
    pub prompt: String,
    pub profile: WorkerProfile,
    pub cancellation: CancellationToken,
}

pub type WorkflowWorkerFuture =
    Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'static>>;

pub trait WorkflowWorkerExecutor: Send + Sync {
    fn execute(&self, request: WorkflowWorkerRequest) -> WorkflowWorkerFuture;
}

pub struct WorkflowRunRequest {
    pub run_id: String,
    pub plan: ResolvedWorkflowPlan,
    pub timeout: Duration,
    pub cancellation: CancellationToken,
}

pub struct WorkflowRunner {
    ports: OrchestrationPorts,
    executor: Arc<dyn WorkflowWorkerExecutor>,
    manifests: Arc<dyn WorkflowManifestStore>,
}

impl WorkflowRunner {
    pub fn new(
        ports: OrchestrationPorts,
        executor: Arc<dyn WorkflowWorkerExecutor>,
        manifests: Arc<dyn WorkflowManifestStore>,
    ) -> Self {
        Self {
            ports,
            executor,
            manifests,
        }
    }

    pub async fn run(
        &self,
        request: WorkflowRunRequest,
    ) -> Result<WorkflowManifest, WorkflowError> {
        validate_run_id(&request.run_id)?;
        if request.timeout.is_zero() {
            return Err(WorkflowError::ZeroTimeout);
        }
        let manifest = WorkflowManifest {
            version: 1,
            run_id: request.run_id.clone(),
            workflow_name: request.plan.name.clone(),
            description: request.plan.description.clone(),
            status: WorkflowManifestStatus::Running,
            started_at_ms: self.ports.now_ms(),
            ended_at_ms: None,
            current_phase: None,
            phase_titles: request
                .plan
                .phases
                .iter()
                .map(|phase| phase.title.clone())
                .collect(),
            results: Vec::new(),
            error: None,
        };
        self.commit_manifest(&manifest, "started")?;
        let shared = Arc::new(Mutex::new(manifest));
        let execution_cancellation = CancellationToken::new();
        let phases = self.execute_phases(
            Arc::clone(&shared),
            request.plan,
            execution_cancellation.clone(),
        );
        tokio::pin!(phases);
        let terminal = tokio::select! {
            result = &mut phases => match result {
                Ok(()) => (WorkflowManifestStatus::Completed, None),
                Err(WorkflowError::Cancelled) => (WorkflowManifestStatus::Aborted, Some("workflow cancelled".to_owned())),
                Err(error) => (WorkflowManifestStatus::Failed, Some(error.to_string())),
            },
            _ = request.cancellation.cancelled() => {
                execution_cancellation.cancel();
                (WorkflowManifestStatus::Aborted, Some("workflow cancelled".to_owned()))
            },
            _ = tokio::time::sleep(request.timeout) => {
                execution_cancellation.cancel();
                (WorkflowManifestStatus::TimedOut, Some("workflow timed out".to_owned()))
            },
        };
        execution_cancellation.cancel();
        let mut final_manifest = lock(&shared).clone();
        final_manifest.status = terminal.0;
        final_manifest.error = terminal.1;
        final_manifest.ended_at_ms = Some(self.ports.now_ms());
        final_manifest.current_phase = None;
        self.commit_manifest(&final_manifest, "terminal")?;
        *lock(&shared) = final_manifest.clone();
        Ok(final_manifest)
    }

    pub fn reconcile_lost(
        &self,
        active_run_ids: &BTreeSet<String>,
    ) -> Result<Vec<WorkflowManifest>, WorkflowError> {
        let manifests = self
            .manifests
            .load()
            .map_err(WorkflowError::ManifestStore)?;
        let mut lost = Vec::new();
        for mut manifest in manifests {
            if manifest.status != WorkflowManifestStatus::Running
                || active_run_ids.contains(&manifest.run_id)
            {
                continue;
            }
            manifest.status = WorkflowManifestStatus::Lost;
            manifest.ended_at_ms = Some(self.ports.now_ms());
            manifest.current_phase = None;
            manifest.error = Some("workflow executor was not present after restart".to_owned());
            self.commit_manifest(&manifest, "reconciled_lost")?;
            lost.push(manifest);
        }
        Ok(lost)
    }

    async fn execute_phases(
        &self,
        manifest: Arc<Mutex<WorkflowManifest>>,
        resolved: ResolvedWorkflowPlan,
        cancellation: CancellationToken,
    ) -> Result<(), WorkflowError> {
        let mut results = BTreeMap::new();
        for (phase_index, phase) in resolved.phases.iter().enumerate() {
            if cancellation.is_cancelled() {
                return Err(WorkflowError::Cancelled);
            }
            {
                let mut current = lock(&manifest);
                current.current_phase = Some(phase_index);
                self.commit_manifest(&current, "phase_started")?;
            }
            let mut tasks = tokio::task::JoinSet::new();
            for (task_index, task) in phase.tasks.iter().enumerate() {
                let prompt = interpolate_results(&task.prompt, &results)?;
                let profile = resolved
                    .profiles
                    .get(&task.worker_profile)
                    .cloned()
                    .ok_or_else(|| WorkflowError::UnknownProfile(task.worker_profile.clone()))?;
                let request = WorkflowWorkerRequest {
                    run_id: lock(&manifest).run_id.clone(),
                    phase_index,
                    task_id: task.id.clone(),
                    description: task.description.clone(),
                    prompt,
                    profile,
                    cancellation: cancellation.clone(),
                };
                let task_id = task.id.clone();
                let future = self.executor.execute(request);
                tasks.spawn(async move { (task_index, task_id, future.await) });
            }
            let mut phase_results = Vec::with_capacity(phase.tasks.len());
            while let Some(joined) = tasks.join_next().await {
                match joined {
                    Ok((index, task_id, Ok(output))) => {
                        phase_results.push((
                            index,
                            WorkflowTaskResult {
                                task_id,
                                phase_index,
                                status: WorkflowTaskStatus::Completed,
                                result: Some(output),
                                error: None,
                            },
                        ));
                    }
                    Ok((index, task_id, Err(error))) => {
                        phase_results.push((
                            index,
                            WorkflowTaskResult {
                                task_id,
                                phase_index,
                                status: if cancellation.is_cancelled() {
                                    WorkflowTaskStatus::Aborted
                                } else {
                                    WorkflowTaskStatus::Failed
                                },
                                result: None,
                                error: Some(error),
                            },
                        ));
                    }
                    Err(error) => return Err(WorkflowError::WorkerJoin(error.to_string())),
                }
            }
            phase_results.sort_by_key(|(index, _)| *index);
            let mut failed = false;
            {
                let mut current = lock(&manifest);
                for (_, result) in phase_results {
                    failed |= result.status != WorkflowTaskStatus::Completed;
                    if let Some(output) = &result.result {
                        results.insert(result.task_id.clone(), output.clone());
                    }
                    current.results.push(result);
                }
                self.commit_manifest(&current, "phase_completed")?;
            }
            if failed {
                return Err(WorkflowError::PhaseFailed(phase_index));
            }
        }
        Ok(())
    }

    fn commit_manifest(
        &self,
        manifest: &WorkflowManifest,
        action: &str,
    ) -> Result<(), WorkflowError> {
        self.manifests
            .write(manifest, ManifestPermissions::private())
            .map_err(WorkflowError::ManifestStore)?;
        let event = self.ports.persist(
            WORKFLOW_SCOPE,
            action,
            Some(&manifest.run_id),
            manifest,
            json!({"status": manifest.status, "currentPhase": manifest.current_phase}),
        )?;
        self.ports.publish(event);
        Ok(())
    }
}

fn validate_worker_profiles(
    plan: &WorkflowPlan,
    profiles: &BTreeMap<String, WorkerProfile>,
) -> Result<(), WorkflowError> {
    for task in plan.phases.iter().flat_map(|phase| &phase.tasks) {
        let profile = profiles
            .get(&task.worker_profile)
            .ok_or_else(|| WorkflowError::UnknownProfile(task.worker_profile.clone()))?;
        if profile.allow_delegation
            || profile.capabilities.can_spawn_subagents
            || profile.capabilities.can_swarm
            || profile.capabilities.can_workflow
        {
            return Err(WorkflowError::RecursiveProfile(profile.name.clone()));
        }
    }
    Ok(())
}

fn interpolate_args(
    prompt: &str,
    args: &BTreeMap<String, WorkflowArgValue>,
    used: &mut BTreeSet<String>,
) -> Result<String, WorkflowError> {
    let mut output = prompt.to_owned();
    for placeholder in parse_placeholders(prompt)? {
        if let Placeholder::Arg(key) = placeholder {
            let value = args
                .get(&key)
                .ok_or_else(|| WorkflowError::MissingArgument(key.clone()))?;
            used.insert(key.clone());
            output = output.replace(&format!("{{{{arg:{key}}}}}"), &value.to_string());
        }
    }
    Ok(output)
}

fn interpolate_results(
    prompt: &str,
    results: &BTreeMap<String, String>,
) -> Result<String, WorkflowError> {
    let mut output = prompt.to_owned();
    for placeholder in parse_placeholders(prompt)? {
        if let Placeholder::Result(id) = placeholder {
            let value = results
                .get(&id)
                .ok_or_else(|| WorkflowError::UnavailableResult(id.clone()))?;
            output = output.replace(&format!("{{{{result:{id}}}}}"), value);
        }
    }
    if output.chars().count() > MAX_EXPANDED_PROMPT_CHARS {
        return Err(WorkflowError::ExpandedPromptTooLarge);
    }
    Ok(output)
}

#[derive(Clone, Debug)]
enum Placeholder {
    Arg(String),
    Result(String),
}

fn parse_placeholders(prompt: &str) -> Result<Vec<Placeholder>, WorkflowError> {
    let mut placeholders = Vec::new();
    let mut cursor = 0usize;
    while let Some(relative) = prompt[cursor..].find("{{") {
        let start = cursor + relative;
        let Some(close_relative) = prompt[start + 2..].find("}}") else {
            if contains_reserved_start(&prompt[start..]) {
                return Err(WorkflowError::MalformedPlaceholder);
            }
            break;
        };
        let end = start + 2 + close_relative;
        let body = &prompt[start + 2..end];
        if let Some(key) = body.strip_prefix("arg:") {
            if !valid_arg_key(key) {
                return Err(WorkflowError::MalformedPlaceholder);
            }
            placeholders.push(Placeholder::Arg(key.to_owned()));
        } else if let Some(id) = body.strip_prefix("result:") {
            if !valid_task_id(id) {
                return Err(WorkflowError::MalformedPlaceholder);
            }
            placeholders.push(Placeholder::Result(id.to_owned()));
        }
        cursor = end + 2;
    }
    if contains_reserved_start(&prompt[cursor..]) {
        return Err(WorkflowError::MalformedPlaceholder);
    }
    Ok(placeholders)
}

fn contains_reserved_start(value: &str) -> bool {
    value.contains("{{arg:") || value.contains("{{result:")
}

fn valid_workflow_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--")
}

fn valid_task_id(value: &str) -> bool {
    value.len() <= 64
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn valid_arg_key(value: &str) -> bool {
    value
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn validate_run_id(value: &str) -> Result<(), WorkflowError> {
    let Some(uuid) = value.strip_prefix("wf-") else {
        return Err(WorkflowError::InvalidRunId);
    };
    let valid = [8, 4, 4, 4, 12]
        .into_iter()
        .zip(uuid.split('-'))
        .all(|(length, part)| {
            part.len() == length
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
        && uuid.split('-').count() == 5;
    if !valid {
        return Err(WorkflowError::InvalidRunId);
    }
    Ok(())
}

fn default_worker_profile() -> String {
    "coder".to_owned()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("workflow JSON is invalid: {0}")]
    Parse(serde_json::Error),
    #[error("unsupported workflow version {0}")]
    UnsupportedVersion(u8),
    #[error("workflow name is invalid")]
    InvalidName,
    #[error("workflow description is invalid")]
    InvalidDescription,
    #[error("workflow phase count is invalid")]
    InvalidPhaseCount,
    #[error("workflow phase title is invalid")]
    InvalidPhaseTitle,
    #[error("workflow phase task count is invalid")]
    InvalidPhaseTaskCount,
    #[error("workflow declares too many tasks")]
    TooManyTasks,
    #[error("workflow task id {0:?} is invalid")]
    InvalidTaskId(String),
    #[error("workflow task id {0:?} is duplicated")]
    DuplicateTaskId(String),
    #[error("workflow task {0:?} description is invalid")]
    InvalidTaskDescription(String),
    #[error("workflow task {0:?} prompt is invalid")]
    InvalidTaskPrompt(String),
    #[error("workflow placeholder syntax is malformed")]
    MalformedPlaceholder,
    #[error("workflow references unknown result {0:?}")]
    UnknownResult(String),
    #[error("task {task:?} references non-prior result {result:?}")]
    ForwardResult { task: String, result: String },
    #[error("workflow worker cap is invalid")]
    InvalidWorkerCap,
    #[error("workflow declares {declared} workers but the limit is {limit}")]
    WorkerCapExceeded { declared: usize, limit: usize },
    #[error("workflow worker profile {0:?} does not exist")]
    UnknownProfile(String),
    #[error("workflow worker profile {0:?} permits recursive orchestration")]
    RecursiveProfile(String),
    #[error("workflow declares too many arguments")]
    TooManyArguments,
    #[error("workflow arguments are too large")]
    ArgumentsTooLarge,
    #[error("workflow argument {0:?} contains a reserved placeholder")]
    ArgumentContainsPlaceholder(String),
    #[error("workflow argument {0:?} is required")]
    MissingArgument(String),
    #[error("workflow arguments are unused: {0:?}")]
    UnusedArguments(Vec<String>),
    #[error("workflow result {0:?} is unavailable")]
    UnavailableResult(String),
    #[error("expanded workflow prompt is too large")]
    ExpandedPromptTooLarge,
    #[error("workflow run id is invalid")]
    InvalidRunId,
    #[error("workflow timeout must be positive")]
    ZeroTimeout,
    #[error("workflow phase {0} failed")]
    PhaseFailed(usize),
    #[error("workflow was cancelled")]
    Cancelled,
    #[error("workflow worker task failed: {0}")]
    WorkerJoin(String),
    #[error("workflow manifest store failed: {0}")]
    ManifestStore(String),
    #[error(transparent)]
    Orchestration(#[from] OrchestrationError),
}
