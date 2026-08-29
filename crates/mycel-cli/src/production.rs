use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    io::{self, Write as _},
    path::{Component, Path, PathBuf},
    process::Command as ProcessCommand,
    sync::{mpsc, Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use chrono::Utc;
use futures_util::StreamExt;
use mycel_agent_protocol::{
    AgentEvent, ApprovalDecision as ProtocolApprovalDecision, ApprovalRequest, ApprovalResponse,
    ApprovalScope, ContentPart, GoalStartMode, GoalStatus as ProtocolGoalStatus, HookEvent,
    HookFailMode, McpConfigFile, Message, ModelConfig, ModelProtocol, MycelConfig,
    PermissionMode as ProtocolPermissionMode, PermissionRule, PromptOrigin, ProviderEntryConfig,
    ProviderError, ProviderErrorKind, ProviderRequest, Role, SecretString, StreamAssembler,
    ThinkingEffort, ToolInputDisplay, TurnEndReason,
};
use mycel_agent_runtime::{
    register_local_builtins_with_process_port, register_retained_builtins, ApprovalPort,
    AutoCompactionConfig, BackgroundShutdown, BackgroundStatus, CancellationToken, CapabilitySet,
    CommandHookFailMode, CompactionEngine, CompactionRequest, ContextEntry, ForegroundProcessPort,
    ForegroundProcessTask, GoalBudgetPort, HookMatcher, HookRegistration, HookRunner,
    LiveEventSink, LocalPluginRegistry, LocalToolConfig, McpEnvironment, McpRuntime,
    McpTransportConnector, MediaCapabilities, NativeChildContext, NativeOrchestrationBundle,
    NativeOrchestrationBundleConfig, NativeOrchestrationDependencies, NativeSessionOptionsFactory,
    NativeTurnEngineFactory, NativeTurnRuntime, OrchestrationEvent, OrchestrationRootConfig,
    PermissionVerdict, PlanPolicy, PluginCommandTool, PluginContributionPlan, PluginInfo,
    PluginLimits, PortError, PortFuture, PromotionGate, QuestionAnswer, QuestionPort,
    QuestionRequest, QuestionResponse, ReadMediaConfig, RequestId, RetainedBuiltinConfig, Runtime,
    SessionBuiltinStatePort, SessionHandle, SessionId, SessionIndex, SessionIndexError,
    SessionOptions, SkillActivationPort, SkillDiagnosticLevel, SkillRegistry,
    SkillRegistryActivationPort, SkillRoot, SkillScanLimits, SystemMcpEnvironment, ToolCallId,
    ToolHookEvent, ToolPermissionRequest, ToolRegistry, ToolScheduler, TurnEngine,
    TurnEngineConfig, TurnInput, TurnOutcome, TurnOutcomeReason, TurnProvider, TurnProviderFuture,
    TurnProviderStreamFuture, TurnProviderStreamSink, WorkerProfile, ORCHESTRATION_TOOL_NAMES,
};
use mycel_providers::{
    managed_kimi_defaults, ApiKeyCredentialConfig, GoogleServiceAccountCredentialSource,
    HttpTransport, ProviderAdapterConfig, ProviderConfig, ProviderCredentialConfig,
    ProviderFactory, ProviderModelConfig, ProviderRegistry, ProviderRegistryConfig,
    ReqwestTransport, CODEX_SUBSCRIPTION_BASE_URL,
};
use serde_json::Value;

use crate::{
    cli::{
        validate_provider_command, Command, ExportArgs, GoalCreateRequest, InteractiveRequest,
        PermissionMode, PromptRequest, ProviderArgs, ProviderAuthTarget, ProviderCommand,
        SessionSelection,
    },
    clipboard::{read_clipboard_image, PastedImageStore},
    doctor::run_doctor,
    ecology::{
        parse_ecology_submission, EcologyDispatch, EcologyService, GateStatus, SubstrateStatus,
    },
    exit::{GoalStatus, TerminationSignal},
    export::{
        run_export, ExportConfirmation, FilesystemSessionExportStore, ProcessExportConfirmation,
        SessionExportStore,
    },
    headless::{HeadlessEvent, HeadlessEventSink, RetryMetadata},
    markdown_export::{
        build_export_markdown, default_markdown_export_path, write_markdown_export, MarkdownExport,
    },
    mcp_service::{parse_mcp_config, start_session_mcp, SessionMcpContext},
    mcp_transport::ProcessMcpConnector,
    plugin_store::{
        install_local_plugin, load_plugin_registrations, remove_installed_plugin,
        set_installed_plugin_enabled, set_installed_plugin_mcp_enabled,
    },
    provider_command_runner::{
        ProcessProviderCommandStderr, ProviderCommandRunner, ProviderCommandRunnerDependencies,
    },
    provider_commands::{
        AtomicTomlConfigStore, NoProviderCommandInput, ProcessProviderEnvironment,
        TokioProviderCommandClock,
    },
    runtime::{
        AdapterOutput, RuntimeAdapter, RuntimeAdapterError, RuntimeCompletion, RuntimeRequest,
    },
    session_management::{resume_command, ProcessSessionPicker, SessionPickerPort},
    system_prompt::{
        build_system_prompt, PreparedSystemPrompt, SystemPromptContext, SystemSkillSummary,
        INIT_PROMPT,
    },
    terminal::{
        style::truecolor_enabled, visible_width, wrap_text, DifferentialRenderer, InputDecoder,
        InputEvent, KeyCode, ProcessTerminalBackend, TerminalBackend, TerminalDriver,
        TerminalEvent, TerminalSession, TerminalSignal, TerminalSink, TerminalSize,
    },
    tui::{
        components::header::{header_card, GateDisplay, HeaderData, SubstrateSummary},
        components::inspector::{AntibodyDetail, InspectorData},
        components::session_rail::RailData,
        components::transcript::{transcript_frame_lines, FrameCtx},
        theme::Theme,
        ApprovalChoice, ApprovalDecision as DialogApprovalDecision, ApprovalDialogAction,
        ApprovalDialogReducer, FrameKind, GateLog, LogicalAction, QuestionDialogAction,
        QuestionDialogReducer, QuestionItem, QuestionOption as DialogQuestionOption, SessionPhase,
        SessionReducer, SubmissionMode, TranscriptEvent, TranscriptReducer,
    },
    tui_config::{
        active_theme, load_tui_config, save_tui_config, ThemeName, TuiConfig, LIGHT_THEME_WARNING,
    },
    workspace_config::{
        load_workspace_local_config, remember_workspace_additional_dir, resolve_workspace_directory,
    },
};

const CONFIG_FILE: &str = "config.toml";
const MCP_CONFIG_FILE: &str = "mcp.json";
const SESSIONS_DIR: &str = "sessions";
const PLANS_DIR: &str = "plans";
const CODEX_FLAG: &str = "codex_subscription_auth";
const CODEX_FLAG_ENV: &str = "MYCEL_EXPERIMENTAL_CODEX_SUBSCRIPTION_AUTH";
const GOOGLE_APPLICATION_CREDENTIALS: &str = "GOOGLE_APPLICATION_CREDENTIALS";
const INTERACTIVE_POLL: Duration = Duration::from_millis(25);
/// How long each braille spinner frame is held on running tool rows.
const SPINNER_INTERVAL_MS: u64 = 90;
/// After an exit is requested while a turn is in flight (Ctrl-D once, then
/// stdin closes), how long the session waits for that turn to finish on its
/// own before cancelling it and exiting anyway. Bounded on purpose: a stalled
/// provider must never make the session unkillable.
const EXIT_TURN_GRACE: Duration = Duration::from_secs(5);
/// After cancelling the in-flight turn on shutdown, how long to wait for the
/// task to honor cancellation before aborting it. A turn stuck in something
/// non-cancellable must not block the process from exiting.
const SHUTDOWN_JOIN_BOUND: Duration = Duration::from_secs(5);
const SIDE_QUESTION_SYSTEM_REMINDER: &str = "This is a side-channel conversation with the user. Answer questions directly from the conversation you already have. The main agent continues independently. Do not call tools; this side channel has no tools. Follow-up turns remain in this side channel. If you do not know the answer, say so directly.";

#[derive(Clone, Copy)]
struct PermissionToggleSpec {
    mode: ProtocolPermissionMode,
    label: &'static str,
    enabled_detail: &'static str,
}

/// Injectable persisted-config boundary. Implementations must return the
/// complete UTF-8 TOML document or a concrete I/O error.
pub trait ConfigSource: Send + Sync {
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FileConfigSource;

impl ConfigSource for FileConfigSource {
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        fs::read_to_string(path)
    }
}

/// Injectable `MYCEL_HOME` resolution boundary.
pub trait HomeLocator: Send + Sync {
    fn mycel_home(&self) -> Result<PathBuf, String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessHomeLocator;

impl HomeLocator for ProcessHomeLocator {
    fn mycel_home(&self) -> Result<PathBuf, String> {
        if let Some(path) = nonempty_os_path(std::env::var_os("MYCEL_HOME")) {
            return Ok(path);
        }
        let home = nonempty_os_path(std::env::var_os("HOME"))
            .ok_or_else(|| "neither MYCEL_HOME nor HOME is set".to_owned())?;
        Ok(home.join(".mycel"))
    }
}

/// Injectable provider-environment boundary. Configured values remain
/// authoritative; this source supplies only documented environment fallbacks.
pub trait RuntimeEnvironment: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnvironmentSource;

impl RuntimeEnvironment for ProcessEnvironmentSource {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// Builds the runtime tool surface after CLI paths have been parsed and
/// validated, but before the terminal is mutated. The local-builtins owner can
/// supply its registry here without coupling terminal control to tool setup.
pub trait ToolRegistryBuilder: Send + Sync {
    fn build(
        &self,
        working_dir: &Path,
        additional_dirs: &[PathBuf],
        allowed_files: &[PathBuf],
        foreground_processes: Option<Arc<dyn ForegroundProcessPort>>,
    ) -> Result<ToolRegistry, String>;
}

/// Deliberately tool-less registry used until the production local-builtins
/// composition is supplied. Additional workspace roots fail loudly because an
/// empty registry cannot honor them.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyToolRegistryBuilder;

impl ToolRegistryBuilder for EmptyToolRegistryBuilder {
    fn build(
        &self,
        _working_dir: &Path,
        additional_dirs: &[PathBuf],
        allowed_files: &[PathBuf],
        _foreground_processes: Option<Arc<dyn ForegroundProcessPort>>,
    ) -> Result<ToolRegistry, String> {
        if !additional_dirs.is_empty() || !allowed_files.is_empty() {
            return Err(
                "additional workspace paths require a configured tool registry builder".to_owned(),
            );
        }
        Ok(ToolRegistry::new())
    }
}

/// Production local filesystem/shell tool composition. Path canonicalization
/// and containment are delegated to the runtime's retained local tool config.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalToolRegistryBuilder;

impl ToolRegistryBuilder for LocalToolRegistryBuilder {
    fn build(
        &self,
        working_dir: &Path,
        additional_dirs: &[PathBuf],
        allowed_files: &[PathBuf],
        foreground_processes: Option<Arc<dyn ForegroundProcessPort>>,
    ) -> Result<ToolRegistry, String> {
        let config = LocalToolConfig::new(working_dir, additional_dirs.iter())
            .map_err(|error| format!("invalid local tool roots: {error}"))?
            .with_allowed_files(allowed_files.iter())
            .map_err(|error| format!("invalid exact local file grant: {error}"))?;
        let registry = ToolRegistry::new();
        register_local_builtins_with_process_port(&registry, config, foreground_processes)
            .map_err(|error| format!("could not register local tools: {error}"))?;
        Ok(registry)
    }
}

#[derive(Default)]
struct DeferredForegroundProcessPort {
    bound: RwLock<Option<Arc<dyn ForegroundProcessPort>>>,
}

impl DeferredForegroundProcessPort {
    fn bind(&self, port: Arc<dyn ForegroundProcessPort>) -> Result<(), String> {
        let mut bound = self
            .bound
            .write()
            .map_err(|_| "foreground process port lock was poisoned".to_owned())?;
        if bound.is_some() {
            return Err("foreground process port was already bound".to_owned());
        }
        *bound = Some(port);
        Ok(())
    }

    fn port(&self) -> Result<Arc<dyn ForegroundProcessPort>, String> {
        self.bound
            .read()
            .map_err(|_| "foreground process port lock was poisoned".to_owned())?
            .clone()
            .ok_or_else(|| "foreground process port is not initialized".to_owned())
    }
}

impl ForegroundProcessPort for DeferredForegroundProcessPort {
    fn register(
        &self,
        description: &str,
        timeout: Duration,
    ) -> Result<ForegroundProcessTask, String> {
        self.port()?.register(description, timeout)
    }

    fn settle(
        &self,
        task_id: &str,
        status: BackgroundStatus,
        reason: Option<&str>,
    ) -> Result<(), String> {
        self.port()?.settle(task_id, status, reason)
    }
}

fn register_canonical_session_builtins(
    registry: &ToolRegistry,
    session: &SessionHandle,
    local: LocalToolConfig,
    plan_file: PathBuf,
    skills: Option<Arc<dyn SkillActivationPort>>,
    media: Option<ReadMediaConfig>,
    goal_budget: Option<Arc<dyn GoalBudgetPort>>,
) -> Result<(), String> {
    let state: Arc<dyn SessionBuiltinStatePort> = Arc::new(session.clone());
    let mut config = RetainedBuiltinConfig::new(session.clone(), state)
        .with_plan_file(plan_file)
        .with_local_tools(local);
    if let Some(skills) = skills {
        config = config.with_skills(skills, 0);
    }
    if let Some(media) = media {
        config = config.with_media(media);
    }
    if let Some(goal_budget) = goal_budget {
        config = config.with_goal_budget(goal_budget);
    }
    register_retained_builtins(registry, config)
        .map_err(|error| format!("could not register retained session tools: {error}"))
}

struct SkillComposition {
    activation: Option<Arc<dyn SkillActivationPort>>,
    catalog: Vec<SystemSkillSummary>,
    warnings: Vec<String>,
}

#[derive(Clone, Default)]
struct PluginComposition {
    plan: PluginContributionPlan,
    infos: Vec<PluginInfo>,
    warnings: Vec<String>,
    command_names: BTreeSet<String>,
}

fn compose_plugins(home: &Path) -> Result<PluginComposition, String> {
    let registrations = load_plugin_registrations(home)?;
    if registrations.is_empty() {
        return Ok(PluginComposition::default());
    }
    let mut registry = LocalPluginRegistry::local(registrations, PluginLimits::default());
    let reload = registry.reload();
    let warnings = reload
        .diagnostics
        .into_iter()
        .map(|diagnostic| {
            format!(
                "plugin scan {:?} at {}: {}",
                diagnostic.code,
                diagnostic.path.display(),
                diagnostic.message
            )
        })
        .collect::<Vec<_>>();
    let infos = registry.list();
    let plan = registry.contribution_plan();
    let command_names = plan
        .commands
        .iter()
        .map(|command| command.runtime_name.clone())
        .collect();
    Ok(PluginComposition {
        plan,
        infos,
        warnings,
        command_names,
    })
}

fn merge_plugin_mcp(
    services: &mut SessionMcpServices,
    plugins: &PluginComposition,
) -> Result<(), String> {
    for (name, config) in plugins.plan.runtime_mcp_configs() {
        if services.config.mcp_servers.contains_key(&name) {
            return Err(format!(
                "plugin MCP server {name:?} collides with an explicitly configured server"
            ));
        }
        services.config.mcp_servers.insert(name, config);
    }
    Ok(())
}

fn register_plugin_commands(
    registry: &ToolRegistry,
    plugins: &PluginComposition,
) -> Result<(), String> {
    if plugins.plan.commands.is_empty() {
        return Ok(());
    }
    let tool = PluginCommandTool::new(plugins.plan.commands.clone())
        .map_err(|error| format!("could not compose plugin commands: {error}"))?;
    registry
        .register(Arc::new(tool))
        .map_err(|error| format!("could not register plugin commands: {error}"))
}

fn compose_skills(
    config: &MycelConfig,
    cli_roots: &[PathBuf],
    home: &Path,
    user_home: Option<&Path>,
    working_dir: &Path,
    plugin_roots: &[SkillRoot],
) -> Result<SkillComposition, String> {
    let project_root = find_project_root(working_dir);
    let mut roots = Vec::new();
    if cli_roots.is_empty() {
        roots.push(SkillRoot::project(project_root.join(".mycel/skills")));
        roots.push(SkillRoot::project(project_root.join(".agents/skills")));
        roots.push(SkillRoot::user(home.join("skills")));
        if let Some(user_home) = user_home {
            roots.push(SkillRoot::user(user_home.join(".agents/skills")));
        }
    } else {
        for root in cli_roots {
            roots.push(SkillRoot::user(resolve_skill_root(
                root,
                &project_root,
                user_home,
            )?));
        }
    }
    for root in &config.extra_skill_dirs {
        roots.push(SkillRoot::extra(resolve_skill_root(
            Path::new(root),
            &project_root,
            user_home,
        )?));
    }
    roots.extend(plugin_roots.iter().cloned());

    let mut registry = SkillRegistry::local(roots, SkillScanLimits::default());
    let reload = registry.reload();
    let warnings = reload
        .diagnostics
        .into_iter()
        .filter(|diagnostic| diagnostic.level != SkillDiagnosticLevel::Info)
        .map(|diagnostic| {
            format!(
                "skill scan {:?} at {}: {}",
                diagnostic.code,
                diagnostic.path.display(),
                diagnostic.message
            )
        })
        .collect::<Vec<_>>();
    let catalog = registry
        .catalog()
        .iter()
        .map(|(id, skill)| SystemSkillSummary {
            id: id.to_owned(),
            description: skill.metadata.description.clone(),
        })
        .collect();
    let activation = if reload.loaded == 0 {
        None
    } else {
        let registry = Arc::new(RwLock::new(registry));
        let port: Arc<dyn SkillActivationPort> =
            Arc::new(SkillRegistryActivationPort::new(registry));
        Some(port)
    };
    Ok(SkillComposition {
        activation,
        catalog,
        warnings,
    })
}

fn find_project_root(working_dir: &Path) -> PathBuf {
    let start = working_dir.to_path_buf();
    let mut current = start.clone();
    loop {
        if fs::symlink_metadata(current.join(".git")).is_ok() {
            return current;
        }
        let Some(parent) = current.parent() else {
            return start;
        };
        if parent == current {
            return start;
        }
        current = parent.to_path_buf();
    }
}

fn resolve_skill_root(
    configured: &Path,
    project_root: &Path,
    user_home: Option<&Path>,
) -> Result<PathBuf, String> {
    let configured_text = configured.to_string_lossy();
    if configured_text == "~" {
        return user_home
            .map(Path::to_path_buf)
            .ok_or_else(|| "cannot resolve skill root '~' because HOME is not set".to_owned());
    }
    if let Some(suffix) = configured_text.strip_prefix("~/") {
        return user_home.map(|home| home.join(suffix)).ok_or_else(|| {
            format!(
                "cannot resolve skill root {:?} because HOME is not set",
                configured.display()
            )
        });
    }
    if configured.is_absolute() {
        Ok(configured.to_path_buf())
    } else {
        Ok(project_root.join(configured))
    }
}

fn ensure_plan_directory(home: &Path) -> Result<PathBuf, String> {
    let home = canonical_path_with_missing_tail(home)?;
    let plans = home.join(PLANS_DIR);
    reject_existing_symlink_components(&plans)?;
    fs::create_dir_all(&plans).map_err(|error| {
        format!(
            "could not create plan directory {}: {error}",
            plans.display()
        )
    })?;
    reject_existing_symlink_components(&plans)?;
    let canonical = fs::canonicalize(&plans).map_err(|error| {
        format!(
            "could not resolve plan directory {}: {error}",
            plans.display()
        )
    })?;
    if !canonical.is_dir() {
        return Err(format!(
            "plan directory {} is not a directory",
            plans.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&canonical, fs::Permissions::from_mode(0o700)).map_err(|error| {
            format!(
                "could not secure plan directory {}: {error}",
                canonical.display()
            )
        })?;
    }
    Ok(canonical)
}

fn canonical_path_with_missing_tail(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("Mycel home {} is not absolute", path.display()));
    }
    let mut ancestor = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(&ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let leaf = ancestor
                    .file_name()
                    .ok_or_else(|| format!("could not resolve Mycel home {}", path.display()))?
                    .to_os_string();
                missing.push(leaf);
                if !ancestor.pop() {
                    return Err(format!("could not resolve Mycel home {}", path.display()));
                }
            }
            Err(error) => {
                return Err(format!(
                    "could not inspect Mycel home ancestor {}: {error}",
                    ancestor.display()
                ))
            }
        }
    }
    let mut canonical = fs::canonicalize(&ancestor).map_err(|error| {
        format!(
            "could not resolve Mycel home ancestor {}: {error}",
            ancestor.display()
        )
    })?;
    for component in missing.into_iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

fn reject_existing_symlink_components(path: &Path) -> Result<(), String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(std::path::MAIN_SEPARATOR.to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!(
                    "plan directory {} contains a parent traversal",
                    path.display()
                ))
            }
            Component::Normal(value) => current.push(value),
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "plan directory {} traverses symlink {}",
                    path.display(),
                    current.display()
                ))
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(format!(
                    "could not inspect plan directory component {}: {error}",
                    current.display()
                ))
            }
        }
    }
    Ok(())
}

fn new_plan_file(plans: &Path) -> PathBuf {
    plans.join(format!("{}.md", RequestId::generate()))
}

fn validate_replayed_plan_file(plans: &Path, path: &Path) -> Result<PathBuf, String> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err("persisted plan path has no UTF-8 file name".to_owned());
    };
    let Some(id) = name.strip_suffix(".md") else {
        return Err("persisted plan path must end in .md".to_owned());
    };
    if id.is_empty()
        || id.len() > 200
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("persisted plan id is not a safe opaque path component".to_owned());
    }
    let candidate = plans.join(name);
    if path != candidate {
        return Err(format!(
            "persisted plan path {} is outside {}",
            path.display(),
            plans.display()
        ));
    }
    Ok(candidate)
}

async fn resolve_plan_file(home: &Path, session: &SessionHandle) -> Result<PathBuf, String> {
    let plans = ensure_plan_directory(home)?;
    let snapshot = <SessionHandle as SessionBuiltinStatePort>::snapshot(session).await?;
    if !snapshot.plan_mode {
        return Ok(new_plan_file(&plans));
    }
    match snapshot.plan_file {
        Some(path) => validate_replayed_plan_file(&plans, &path),
        None => {
            let path = new_plan_file(&plans);
            session
                .set_tool_store_value(
                    "plan_file",
                    Value::String(path.to_string_lossy().into_owned()),
                )
                .await
                .map_err(|error| format!("could not repair active plan path: {error}"))?;
            Ok(path)
        }
    }
}

fn clear_plan_file(home: &Path, path: &Path) -> Result<(), String> {
    let plans = ensure_plan_directory(home)?;
    let path = validate_replayed_plan_file(&plans, path)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_file() => {
            return Err(format!(
                "refusing to clear non-regular plan file {}",
                path.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "could not inspect plan file {}: {error}",
                path.display()
            ));
        }
    }

    let temporary = plans.join(format!(".plan-clear-{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&temporary).map_err(|error| {
            format!(
                "could not create temporary plan file {}: {error}",
                temporary.display()
            )
        })?;
        file.sync_all().map_err(|error| {
            format!(
                "could not sync temporary plan file {}: {error}",
                temporary.display()
            )
        })?;
        drop(file);
        fs::rename(&temporary, &path).map_err(|error| {
            format!(
                "could not atomically clear plan file {}: {error}",
                path.display()
            )
        })?;
        fs::File::open(&plans)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("could not sync plan directory {}: {error}", plans.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn media_config(
    working_dir: &Path,
    additional_dirs: &[PathBuf],
    resolved: &ResolvedModel,
    detected: mycel_agent_protocol::ModelCapability,
) -> Result<Option<ReadMediaConfig>, String> {
    let capabilities = MediaCapabilities {
        image_input: resolved.image_input || detected.image_in,
        video_input: resolved.video_input || detected.video_in,
    };
    if !capabilities.image_input && !capabilities.video_input {
        return Ok(None);
    }
    let local = LocalToolConfig::new(working_dir, additional_dirs.iter())
        .map_err(|error| format!("invalid media workspace roots: {error}"))?;
    ReadMediaConfig::new(local, capabilities)
        .map(Some)
        .map_err(|error| format!("could not configure media reader: {error}"))
}

async fn close_after_setup_error(session: &SessionHandle, error: String) -> String {
    match session.close().await {
        Ok(()) => error,
        Err(close) => format!("{error}; additionally session cleanup failed: {close}"),
    }
}

async fn shutdown_mcp(mcp: Option<&McpRuntime>) -> Result<(), String> {
    match mcp {
        Some(runtime) => runtime
            .shutdown()
            .await
            .map_err(|error| format!("could not shut down MCP runtime: {error}")),
        None => Ok(()),
    }
}

async fn restore_permission(
    session: &SessionHandle,
    permission: ProtocolPermissionMode,
) -> Result<(), String> {
    if permission == ProtocolPermissionMode::Auto {
        return Ok(());
    }
    session
        .set_permission_mode(permission)
        .await
        .map_err(|error| format!("could not restore session permission mode: {error}"))
}

/// Production prompt adapter. The Tokio executor, home/config sources, and
/// HTTP transport are owned explicitly so tests can exercise the real runtime
/// and provider stack without network or process-global filesystem state.
pub struct ProductionRuntimeAdapter {
    executor: tokio::runtime::Runtime,
    home: Arc<dyn HomeLocator>,
    config: Arc<dyn ConfigSource>,
    environment: Arc<dyn RuntimeEnvironment>,
    transport: Arc<dyn HttpTransport>,
    tool_registry: Arc<dyn ToolRegistryBuilder>,
    session_exports: Arc<dyn SessionExportStore>,
    export_confirmation: Arc<dyn ExportConfirmation>,
    session_picker: Arc<dyn SessionPickerPort>,
    mcp_connector_factory: Arc<dyn McpConnectorFactory>,
    mcp_environment: Arc<dyn McpEnvironment>,
    version: String,
}

/// Creates a session-scoped MCP connector after `MYCEL_HOME` has been
/// resolved. Empty MCP configuration bypasses this boundary entirely.
pub trait McpConnectorFactory: Send + Sync {
    fn create(&self, mycel_home: &Path) -> Result<Arc<dyn McpTransportConnector>, String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessMcpConnectorFactory;

impl McpConnectorFactory for ProcessMcpConnectorFactory {
    fn create(&self, mycel_home: &Path) -> Result<Arc<dyn McpTransportConnector>, String> {
        ProcessMcpConnector::new(mycel_home)
            .map(|connector| Arc::new(connector) as Arc<dyn McpTransportConnector>)
            .map_err(|error| format!("could not initialize MCP transport: {error}"))
    }
}

/// Injectable non-provider services used by the production adapter. Grouping
/// these ports keeps construction stable as session management grows without
/// hiding dependencies in process globals.
pub struct ProductionRuntimeServices {
    pub tool_registry: Arc<dyn ToolRegistryBuilder>,
    pub session_exports: Arc<dyn SessionExportStore>,
    pub export_confirmation: Arc<dyn ExportConfirmation>,
    pub session_picker: Arc<dyn SessionPickerPort>,
    pub mcp_connector_factory: Arc<dyn McpConnectorFactory>,
    pub mcp_environment: Arc<dyn McpEnvironment>,
}

impl ProductionRuntimeServices {
    pub fn new(tool_registry: Arc<dyn ToolRegistryBuilder>) -> Self {
        Self {
            tool_registry,
            session_exports: Arc::new(FilesystemSessionExportStore),
            export_confirmation: Arc::new(ProcessExportConfirmation),
            session_picker: Arc::new(ProcessSessionPicker),
            mcp_connector_factory: Arc::new(ProcessMcpConnectorFactory),
            mcp_environment: Arc::new(SystemMcpEnvironment),
        }
    }

    pub fn with_session_services(
        tool_registry: Arc<dyn ToolRegistryBuilder>,
        session_exports: Arc<dyn SessionExportStore>,
        export_confirmation: Arc<dyn ExportConfirmation>,
        session_picker: Arc<dyn SessionPickerPort>,
    ) -> Self {
        Self {
            tool_registry,
            session_exports,
            export_confirmation,
            session_picker,
            mcp_connector_factory: Arc::new(ProcessMcpConnectorFactory),
            mcp_environment: Arc::new(SystemMcpEnvironment),
        }
    }

    pub fn with_mcp_services(
        mut self,
        connector_factory: Arc<dyn McpConnectorFactory>,
        environment: Arc<dyn McpEnvironment>,
    ) -> Self {
        self.mcp_connector_factory = connector_factory;
        self.mcp_environment = environment;
        self
    }
}

impl ProductionRuntimeAdapter {
    pub fn from_process() -> Result<Self, RuntimeAdapterError> {
        let transport = ReqwestTransport::without_redirects().map_err(|error| {
            RuntimeAdapterError::failed(
                "runtime initialization",
                format!("could not construct provider transport: {error}"),
            )
        })?;
        Self::with_components_and_tools(
            Arc::new(ProcessHomeLocator),
            Arc::new(FileConfigSource),
            Arc::new(ProcessEnvironmentSource),
            Arc::new(transport),
            Arc::new(LocalToolRegistryBuilder),
        )
    }

    pub fn with_components(
        home: Arc<dyn HomeLocator>,
        config: Arc<dyn ConfigSource>,
        environment: Arc<dyn RuntimeEnvironment>,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, RuntimeAdapterError> {
        Self::with_components_and_tools(
            home,
            config,
            environment,
            transport,
            Arc::new(LocalToolRegistryBuilder),
        )
    }

    pub fn with_components_and_tools(
        home: Arc<dyn HomeLocator>,
        config: Arc<dyn ConfigSource>,
        environment: Arc<dyn RuntimeEnvironment>,
        transport: Arc<dyn HttpTransport>,
        tool_registry: Arc<dyn ToolRegistryBuilder>,
    ) -> Result<Self, RuntimeAdapterError> {
        Self::with_components_and_services(
            home,
            config,
            environment,
            transport,
            ProductionRuntimeServices::new(tool_registry),
        )
    }

    pub fn with_components_and_services(
        home: Arc<dyn HomeLocator>,
        config: Arc<dyn ConfigSource>,
        environment: Arc<dyn RuntimeEnvironment>,
        transport: Arc<dyn HttpTransport>,
        services: ProductionRuntimeServices,
    ) -> Result<Self, RuntimeAdapterError> {
        let ProductionRuntimeServices {
            tool_registry,
            session_exports,
            export_confirmation,
            session_picker,
            mcp_connector_factory,
            mcp_environment,
        } = services;
        let executor = tokio::runtime::Runtime::new().map_err(|error| {
            RuntimeAdapterError::failed(
                "runtime initialization",
                format!("could not construct Tokio executor: {error}"),
            )
        })?;
        Ok(Self {
            executor,
            home,
            config,
            environment,
            transport,
            tool_registry,
            session_exports,
            export_confirmation,
            session_picker,
            mcp_connector_factory,
            mcp_environment,
            version: env!("CARGO_PKG_VERSION").to_owned(),
        })
    }

    fn load_config(
        &self,
        home: &Path,
        operation: &'static str,
    ) -> Result<MycelConfig, RuntimeAdapterError> {
        let path = home.join(CONFIG_FILE);
        let source = self.config.read_to_string(&path).map_err(|error| {
            RuntimeAdapterError::failed(
                operation,
                format!("could not read {}: {error}", path.display()),
            )
        })?;
        parse_config(&source).map_err(|message| {
            RuntimeAdapterError::failed(operation, format!("invalid {}: {message}", path.display()))
        })
    }

    fn load_mcp_config(
        &self,
        home: &Path,
        operation: &'static str,
    ) -> Result<McpConfigFile, RuntimeAdapterError> {
        let path = home.join(MCP_CONFIG_FILE);
        let source = match fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(McpConfigFile::default());
            }
            Err(error) => {
                return Err(RuntimeAdapterError::failed(
                    operation,
                    format!(
                        "could not read MCP configuration {}: {error}",
                        path.display()
                    ),
                ));
            }
        };
        parse_mcp_config(&source).map_err(|_| {
            // Header values and environment-derived bearer tokens are secret
            // material. Do not include parser excerpts or source lines.
            RuntimeAdapterError::failed(
                operation,
                format!("invalid MCP configuration {}", path.display()),
            )
        })
    }

    fn session_mcp_services(
        &self,
        home: &Path,
        operation: &'static str,
    ) -> Result<SessionMcpServices, RuntimeAdapterError> {
        Ok(SessionMcpServices {
            config: self.load_mcp_config(home, operation)?,
            connector_factory: Arc::clone(&self.mcp_connector_factory),
            environment: Arc::clone(&self.mcp_environment),
        })
    }

    fn run_provider_command(
        &self,
        command: &Command,
    ) -> Result<AdapterOutput, RuntimeAdapterError> {
        if let Command::Provider(arguments) = command {
            validate_provider_command(&arguments.command).map_err(|error| {
                RuntimeAdapterError::failed("provider command", error.to_string())
            })?;
        }
        let home = self.home.mycel_home().map_err(|message| {
            RuntimeAdapterError::failed(
                "provider command",
                format!("could not resolve home: {message}"),
            )
        })?;
        let runner = ProviderCommandRunner::new(
            ProviderCommandRunnerDependencies {
                transport: Arc::clone(&self.transport),
                config_store: Arc::new(AtomicTomlConfigStore::new(home.join(CONFIG_FILE))),
                environment: Arc::new(ProcessProviderEnvironment),
                input: Arc::new(NoProviderCommandInput),
                clock: Arc::new(TokioProviderCommandClock),
                stderr: Arc::new(ProcessProviderCommandStderr),
            },
            home,
            self.version.clone(),
        );
        self.executor
            .block_on(runner.run_with_process_sigint(command))
            .map_err(|error| RuntimeAdapterError::failed(error.operation(), error.to_string()))
    }

    #[cfg(test)]
    fn prepare_interactive(
        &self,
        request: &InteractiveRequest,
    ) -> Result<PreparedInteractive, RuntimeAdapterError> {
        let request = self.resolve_interactive_request(request)?.ok_or_else(|| {
            RuntimeAdapterError::failed("interactive execution", "session selection was cancelled")
        })?;
        self.prepare_resolved_interactive(&request)
    }

    fn prepare_resolved_interactive(
        &self,
        request: &InteractiveRequest,
    ) -> Result<PreparedInteractive, RuntimeAdapterError> {
        let operation = "interactive execution";
        let home = self.home.mycel_home().map_err(|message| {
            RuntimeAdapterError::failed(operation, format!("could not resolve home: {message}"))
        })?;
        let config = self.load_config(&home, operation)?;
        let resolved = resolve_model(
            request.model.as_deref(),
            &config,
            self.environment.as_ref(),
            operation,
        )?;
        let working_dir = std::env::current_dir().map_err(|error| {
            RuntimeAdapterError::failed(
                operation,
                format!("could not resolve working directory: {error}"),
            )
        })?;
        let plugins = compose_plugins(&home)
            .map_err(|message| RuntimeAdapterError::failed(operation, message))?;
        let mut mcp = self.session_mcp_services(&home, operation)?;
        merge_plugin_mcp(&mut mcp, &plugins)
            .map_err(|message| RuntimeAdapterError::failed(operation, message))?;
        let context = InteractiveRunContext {
            home,
            working_dir,
            config,
            resolved,
            transport: Arc::clone(&self.transport),
            version: self.version.clone(),
            tool_registry: Arc::clone(&self.tool_registry),
            session: request.session.clone(),
            permission: protocol_permission(request.permission),
            additional_dirs: request.add_dirs.clone(),
            skill_dirs: request.skills_dirs.clone(),
            user_home: self
                .environment
                .get("HOME")
                .and_then(|value| nonempty(&value).map(PathBuf::from)),
            shell: self.environment.get("SHELL"),
            external_editor: self
                .environment
                .get("VISUAL")
                .or_else(|| self.environment.get("EDITOR")),
            startup_plan: request.plan,
            mcp,
            plugins,
        };
        self.executor
            .block_on(prepare_interactive(context))
            .map_err(|message| RuntimeAdapterError::failed(operation, message))
    }

    fn resolve_interactive_request(
        &self,
        request: &InteractiveRequest,
    ) -> Result<Option<InteractiveRequest>, RuntimeAdapterError> {
        let operation = "interactive execution";
        let home = self.home.mycel_home().map_err(|message| {
            RuntimeAdapterError::failed(operation, format!("could not resolve home: {message}"))
        })?;
        let working_dir = std::env::current_dir().map_err(|error| {
            RuntimeAdapterError::failed(
                operation,
                format!("could not resolve working directory: {error}"),
            )
        })?;
        let local = load_workspace_local_config(&working_dir)
            .map_err(|message| RuntimeAdapterError::failed(operation, message))?;
        let mut requested_dirs = local.additional_dirs;
        requested_dirs.extend(request.add_dirs.iter().cloned());
        let index = SessionIndex::new(home);
        let Some(resolved) = resolve_session_selection(
            &index,
            &working_dir,
            &request.session,
            &requested_dirs,
            Some(self.session_picker.as_ref()),
            operation,
        )?
        else {
            return Ok(None);
        };
        let mut request = request.clone();
        request.session = resolved.session;
        request.add_dirs = resolved.additional_dirs;
        Ok(Some(request))
    }

    fn resolve_prompt_request(
        &self,
        home: &Path,
        working_dir: &Path,
        request: &PromptRequest,
    ) -> Result<PromptRequest, RuntimeAdapterError> {
        let operation = "prompt execution";
        let local = load_workspace_local_config(working_dir)
            .map_err(|message| RuntimeAdapterError::failed(operation, message))?;
        let mut requested_dirs = local.additional_dirs;
        requested_dirs.extend(request.add_dirs.iter().cloned());
        let resolved = resolve_session_selection(
            &SessionIndex::new(home),
            working_dir,
            &request.session,
            &requested_dirs,
            None,
            operation,
        )?
        .ok_or_else(|| RuntimeAdapterError::failed(operation, "session selection was cancelled"))?;
        let mut request = request.clone();
        request.session = resolved.session;
        request.add_dirs = resolved.additional_dirs;
        Ok(request)
    }

    #[cfg(test)]
    fn run_prepared_interactive<B: TerminalBackend>(
        &self,
        prepared: PreparedInteractive,
        terminal: &mut TerminalDriver<B>,
    ) -> Result<RuntimeCompletion, RuntimeAdapterError> {
        match self.run_prepared_interactive_outcome(prepared, terminal)? {
            PreparedInteractiveOutcome::Completion(completion) => Ok(completion),
            PreparedInteractiveOutcome::Transition(_) => Err(RuntimeAdapterError::failed(
                "interactive execution",
                "session transition requires the production interactive lifecycle",
            )),
        }
    }

    fn run_prepared_interactive_outcome<B: TerminalBackend>(
        &self,
        prepared: PreparedInteractive,
        terminal: &mut TerminalDriver<B>,
    ) -> Result<PreparedInteractiveOutcome, RuntimeAdapterError> {
        let operation = "interactive execution";
        let session_id = prepared.session.id().as_str().to_owned();
        let result = run_interactive_terminal(&self.executor, &prepared, terminal)
            .map(|outcome| match outcome {
                InteractiveTerminalOutcome::Completion(completion) => {
                    PreparedInteractiveOutcome::Completion(with_session(
                        completion,
                        session_id.clone(),
                    ))
                }
                InteractiveTerminalOutcome::Transition(action) => {
                    let snapshot = self.executor.block_on(prepared.session.snapshot());
                    PreparedInteractiveOutcome::Transition(PreparedSessionTransition {
                        action,
                        session_id: session_id.clone(),
                        permission: cli_permission(snapshot.state.permission_mode),
                        plan: snapshot.state.plan_mode,
                    })
                }
            })
            .map_err(|message| RuntimeAdapterError::failed(operation, message));
        let orchestration_shutdown = self
            .executor
            .block_on(shutdown_orchestration(Some(
                prepared.orchestration.as_ref(),
            )))
            .map_err(|error| RuntimeAdapterError::failed(operation, error));
        let mcp_shutdown = self
            .executor
            .block_on(shutdown_mcp(prepared.mcp.as_ref()))
            .map_err(|error| RuntimeAdapterError::failed(operation, error));
        let close = self
            .executor
            .block_on(prepared.session.close())
            .map_err(|error| RuntimeAdapterError::failed(operation, error.to_string()));
        let refresh = prepared
            .session_index
            .refresh(&session_id)
            .map(|_| ())
            .map_err(|error| RuntimeAdapterError::failed(operation, error.to_string()));
        let services_cleanup =
            combine_cleanup_results(orchestration_shutdown, mcp_shutdown, operation);
        let session_cleanup = combine_cleanup_results(services_cleanup, close, operation);
        let cleanup = combine_cleanup_results(session_cleanup, refresh, operation);
        match (result, cleanup) {
            (Ok(completion), Ok(())) => Ok(completion),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(cleanup)) => Err(RuntimeAdapterError::failed(
                operation,
                format!("{error}; additionally session cleanup failed: {cleanup}"),
            )),
        }
    }

    fn run_interactive_with_terminal<B: TerminalBackend>(
        &self,
        request: &InteractiveRequest,
        terminal: &mut TerminalDriver<B>,
    ) -> Result<RuntimeCompletion, RuntimeAdapterError> {
        let Some(mut request) = self.resolve_interactive_request(request)? else {
            return Ok(RuntimeCompletion::success());
        };
        let mut pending_notice = None;
        let mut pending_draft = None;
        loop {
            let mut prepared = self.prepare_resolved_interactive(&request)?;
            prepared.initial_draft = pending_draft.take();
            if let Some(notice) = pending_notice.take() {
                prepared.warning = Some(match prepared.warning.take() {
                    Some(warning) => format!("{notice}; {warning}"),
                    None => notice,
                });
            }
            match self.run_prepared_interactive_outcome(prepared, terminal)? {
                PreparedInteractiveOutcome::Completion(completion) => return Ok(completion),
                PreparedInteractiveOutcome::Transition(transition) => {
                    request.permission = transition.permission;
                    request.plan = transition.plan;
                    request.session = match transition.action {
                        InteractiveSessionTransition::New => SessionSelection::New,
                        InteractiveSessionTransition::Resume(id) => {
                            request.add_dirs.clear();
                            SessionSelection::Resume(id)
                        }
                        InteractiveSessionTransition::Reload => {
                            SessionSelection::Resume(transition.session_id)
                        }
                        InteractiveSessionTransition::Model(alias) => {
                            request.model = Some(alias);
                            SessionSelection::Resume(transition.session_id)
                        }
                        InteractiveSessionTransition::AddDir { path, notice } => {
                            if !request.add_dirs.contains(&path) {
                                request.add_dirs.push(path);
                            }
                            pending_notice = Some(notice);
                            SessionSelection::Resume(transition.session_id)
                        }
                        InteractiveSessionTransition::Provider {
                            command,
                            close_after,
                        } => {
                            let login = matches!(&command, Command::Login)
                                || matches!(
                                    &command,
                                    Command::Provider(ProviderArgs {
                                        command: ProviderCommand::Login { .. }
                                    })
                                );
                            let output = self.run_provider_command(&command)?;
                            let notice = provider_transition_notice(&output);
                            if close_after {
                                terminal
                                    .backend_mut()
                                    .write_output(notice.as_bytes())
                                    .and_then(|()| terminal.backend_mut().flush_output())
                                    .map_err(|error| {
                                        RuntimeAdapterError::failed(
                                            "interactive provider command",
                                            format!("could not write command result: {error}"),
                                        )
                                    })?;
                                return Ok(output.completion);
                            }
                            if login
                                && matches!(output.completion, RuntimeCompletion::Success { .. })
                            {
                                request.model = None;
                            }
                            pending_notice = Some(notice.trim().to_owned());
                            SessionSelection::Resume(transition.session_id)
                        }
                        InteractiveSessionTransition::ExternalEditor { command, draft } => {
                            match edit_in_external_editor(&command, &draft) {
                                Ok(Some(edited)) => pending_draft = Some(edited),
                                Ok(None) => {
                                    pending_draft = Some(draft);
                                    pending_notice = Some(
                                        "External editor exited without applying changes."
                                            .to_owned(),
                                    );
                                }
                                Err(error) => {
                                    pending_draft = Some(draft);
                                    pending_notice =
                                        Some(format!("External editor failed: {error}"));
                                }
                            }
                            SessionSelection::Resume(transition.session_id)
                        }
                        InteractiveSessionTransition::Fork => {
                            let home = self.home.mycel_home().map_err(|message| {
                                RuntimeAdapterError::failed(
                                    "interactive execution",
                                    format!("could not resolve home: {message}"),
                                )
                            })?;
                            let index = SessionIndex::new(&home);
                            let source = index
                                .get(&transition.session_id)
                                .map_err(|error| {
                                    RuntimeAdapterError::failed(
                                        "interactive execution",
                                        format!("could not read fork source: {error}"),
                                    )
                                })?
                                .ok_or_else(|| {
                                    RuntimeAdapterError::failed(
                                        "interactive execution",
                                        "fork source disappeared during session cleanup",
                                    )
                                })?;
                            let source_id =
                                SessionId::new(transition.session_id.clone()).map_err(|error| {
                                    RuntimeAdapterError::failed(
                                        "interactive execution",
                                        format!("invalid fork source: {error}"),
                                    )
                                })?;
                            let target_id = SessionId::generate();
                            let runtime = Runtime::new(home.join(SESSIONS_DIR));
                            self.executor
                                .block_on(runtime.fork_session_records(&source_id, &target_id))
                                .map_err(|error| {
                                    RuntimeAdapterError::failed(
                                        "interactive execution",
                                        format!("could not fork session: {error}"),
                                    )
                                })?;
                            let additional_dirs = source
                                .additional_dirs
                                .iter()
                                .map(PathBuf::from)
                                .collect::<Vec<_>>();
                            let title =
                                format!("Fork: {}", source.title.as_deref().unwrap_or(&source.id))
                                    .chars()
                                    .take(200)
                                    .collect::<String>();
                            if let Err(error) = index.register_fork(
                                &source.id,
                                target_id.as_str(),
                                Path::new(&source.work_dir),
                                &additional_dirs,
                                Some(&title),
                            ) {
                                let target_dir = index.session_dir(target_id.as_str()).map_err(
                                    |cleanup| {
                                        RuntimeAdapterError::failed(
                                            "interactive execution",
                                            format!(
                                                "could not index forked session: {error}; could not resolve incomplete fork for cleanup: {cleanup}"
                                            ),
                                        )
                                    },
                                )?;
                                let cleanup = fs::remove_dir_all(&target_dir);
                                return Err(RuntimeAdapterError::failed(
                                    "interactive execution",
                                    match cleanup {
                                        Ok(()) => format!("could not index forked session: {error}"),
                                        Err(cleanup) => format!(
                                            "could not index forked session: {error}; additionally could not remove incomplete fork {}: {cleanup}",
                                            target_dir.display()
                                        ),
                                    },
                                ));
                            }
                            request.add_dirs.clear();
                            SessionSelection::Resume(target_id.into_string())
                        }
                    };
                    let Some(resolved) = self.resolve_interactive_request(&request)? else {
                        return Ok(RuntimeCompletion::success());
                    };
                    request = resolved;
                }
            }
        }
    }
}

impl RuntimeAdapter for ProductionRuntimeAdapter {
    fn run_interactive(
        &mut self,
        request: &InteractiveRequest,
    ) -> Result<RuntimeCompletion, RuntimeAdapterError> {
        let mut terminal = TerminalDriver::new(ProcessTerminalBackend::new());
        self.run_interactive_with_terminal(request, &mut terminal)
    }

    fn run_prompt(
        &mut self,
        request: &PromptRequest,
        events: &mut dyn HeadlessEventSink,
    ) -> Result<RuntimeCompletion, RuntimeAdapterError> {
        validate_supported_prompt(request)?;
        let home = self.home.mycel_home().map_err(|message| {
            RuntimeAdapterError::failed(
                "prompt execution",
                format!("could not resolve home: {message}"),
            )
        })?;
        let config = self.load_config(&home, "prompt execution")?;
        let resolved = resolve_model(
            request.model.as_deref(),
            &config,
            self.environment.as_ref(),
            "prompt execution",
        )?;
        let working_dir = std::env::current_dir().map_err(|error| {
            RuntimeAdapterError::failed(
                "prompt execution",
                format!("could not resolve working directory: {error}"),
            )
        })?;
        let request = self.resolve_prompt_request(&home, &working_dir, request)?;
        let plugins = compose_plugins(&home)
            .map_err(|message| RuntimeAdapterError::failed("prompt execution", message))?;
        let mut mcp = self.session_mcp_services(&home, "prompt execution")?;
        merge_plugin_mcp(&mut mcp, &plugins)
            .map_err(|message| RuntimeAdapterError::failed("prompt execution", message))?;
        let transport = Arc::clone(&self.transport);
        let version = self.version.clone();
        self.executor
            .block_on(run_headless(
                HeadlessRunContext {
                    home,
                    working_dir,
                    config,
                    resolved,
                    transport,
                    version,
                    tool_registry: Arc::clone(&self.tool_registry),
                    user_home: self
                        .environment
                        .get("HOME")
                        .and_then(|value| nonempty(&value).map(PathBuf::from)),
                    shell: self.environment.get("SHELL"),
                    mcp,
                    plugins,
                },
                &request,
                events,
            ))
            .map_err(|message| RuntimeAdapterError::failed("prompt execution", message))
    }

    fn run_command(
        &mut self,
        request: RuntimeRequest,
    ) -> Result<AdapterOutput, RuntimeAdapterError> {
        let RuntimeRequest::Command(command) = request;
        match command {
            Command::Doctor(args) => {
                let home = self.home.mycel_home().map_err(|message| {
                    RuntimeAdapterError::failed(
                        "doctor",
                        format!("could not resolve home: {message}"),
                    )
                })?;
                let cwd = std::env::current_dir().map_err(|error| {
                    RuntimeAdapterError::failed(
                        "doctor",
                        format!("could not resolve working directory: {error}"),
                    )
                })?;
                Ok(run_doctor(&args, &home, &cwd, self.config.as_ref()))
            }
            Command::Export(args) => {
                let home = self.home.mycel_home().map_err(|message| {
                    RuntimeAdapterError::failed(
                        "session export",
                        format!("could not resolve home: {message}"),
                    )
                })?;
                let cwd = std::env::current_dir().map_err(|error| {
                    RuntimeAdapterError::failed(
                        "session export",
                        format!("could not resolve working directory: {error}"),
                    )
                })?;
                Ok(run_export(
                    &args,
                    &home,
                    &cwd,
                    self.session_exports.as_ref(),
                    self.export_confirmation.as_ref(),
                    &self.version,
                ))
            }
            command @ (Command::Login | Command::Provider(_)) => {
                self.run_provider_command(&command)
            }
        }
    }
}

fn validate_supported_prompt(request: &PromptRequest) -> Result<(), RuntimeAdapterError> {
    match &request.session {
        SessionSelection::New | SessionSelection::Resume(_) | SessionSelection::Continue => Ok(()),
        SessionSelection::Pick => Err(RuntimeAdapterError::failed(
            "prompt execution",
            "session picker cannot be used in prompt mode",
        )),
    }
}

#[derive(Debug)]
struct ResolvedSessionSelection {
    session: SessionSelection,
    additional_dirs: Vec<PathBuf>,
}

fn resolve_session_selection(
    index: &SessionIndex,
    working_dir: &Path,
    selection: &SessionSelection,
    requested_additional_dirs: &[PathBuf],
    picker: Option<&dyn SessionPickerPort>,
    operation: &'static str,
) -> Result<Option<ResolvedSessionSelection>, RuntimeAdapterError> {
    let summary = match selection {
        SessionSelection::New => {
            return Ok(Some(ResolvedSessionSelection {
                session: SessionSelection::New,
                additional_dirs: requested_additional_dirs.to_vec(),
            }));
        }
        SessionSelection::Resume(id) => Some(
            index
                .validate_resume(id, working_dir)
                .map_err(|error| session_index_runtime_error(operation, error))?,
        ),
        SessionSelection::Continue => index
            .newest_for_cwd(working_dir)
            .map_err(|error| session_index_runtime_error(operation, error))?,
        SessionSelection::Pick => {
            let picker = picker.ok_or_else(|| {
                RuntimeAdapterError::failed(operation, "session picker port is missing")
            })?;
            let discovery = index
                .list(None)
                .map_err(|error| session_index_runtime_error(operation, error))?;
            let picker_cwd = fs::canonicalize(working_dir).map_err(|error| {
                RuntimeAdapterError::failed(
                    operation,
                    format!(
                        "could not canonicalize working directory {}: {error}",
                        working_dir.display()
                    ),
                )
            })?;
            let Some(id) = picker
                .choose(&discovery.sessions, &picker_cwd)
                .map_err(|message| RuntimeAdapterError::failed(operation, message))?
            else {
                return Ok(None);
            };
            Some(
                index
                    .validate_resume(&id, working_dir)
                    .map_err(|error| session_index_runtime_error(operation, error))?,
            )
        }
    };

    let Some(summary) = summary else {
        return Err(RuntimeAdapterError::failed(
            operation,
            format!(
                "No previous session was found for {}.",
                working_dir.display()
            ),
        ));
    };
    let mut additional_dirs = summary
        .additional_dirs
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    for path in requested_additional_dirs {
        if !additional_dirs.contains(path) {
            additional_dirs.push(path.clone());
        }
    }
    Ok(Some(ResolvedSessionSelection {
        session: SessionSelection::Resume(summary.id),
        additional_dirs,
    }))
}

fn session_index_runtime_error(
    operation: &'static str,
    error: SessionIndexError,
) -> RuntimeAdapterError {
    match error {
        SessionIndexError::CrossWorkingDirectory { id, expected, .. } => {
            RuntimeAdapterError::failed(
                operation,
                format!(
                    "Session {id:?} was created under a different directory.\n  To resume, run: {}",
                    resume_command(&expected.to_string_lossy(), &id)
                ),
            )
        }
        error => RuntimeAdapterError::failed(operation, error.to_string()),
    }
}

fn combine_cleanup_results(
    first: Result<(), RuntimeAdapterError>,
    second: Result<(), RuntimeAdapterError>,
    operation: &'static str,
) -> Result<(), RuntimeAdapterError> {
    match (first, second) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(first), Err(second)) => Err(RuntimeAdapterError::failed(
            operation,
            format!("{first}; additionally cleanup failed: {second}"),
        )),
    }
}

fn combine_string_cleanup_results(
    shutdown_mcp: Result<(), String>,
    restore: Result<(), String>,
    close: Result<(), String>,
    refresh: Result<(), String>,
) -> Result<(), String> {
    let errors = [
        shutdown_mcp.err(),
        restore.err(),
        close.err(),
        refresh.err(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; additionally cleanup failed: "))
    }
}

#[derive(Clone)]
struct ResolvedModel {
    alias: String,
    provider_id: String,
    model_id: String,
    registry: ProviderRegistryConfig,
    thinking_effort: Option<ThinkingEffort>,
    max_completion_tokens: Option<u64>,
    max_context_tokens: u64,
    google_application_credentials: Option<PathBuf>,
    image_input: bool,
    video_input: bool,
    xhigh_supported: bool,
    effort_options: Vec<String>,
    allow_unknown_effort: bool,
}

fn resolve_model(
    requested_model: Option<&str>,
    config: &MycelConfig,
    environment: &dyn RuntimeEnvironment,
    operation: &'static str,
) -> Result<ResolvedModel, RuntimeAdapterError> {
    let alias = requested_model
        .and_then(nonempty)
        .or_else(|| config.default_model.as_deref().and_then(nonempty))
        .ok_or_else(|| {
            RuntimeAdapterError::failed(
                operation,
                "No default_model configured. Set default_model in config.toml or pass --model.",
            )
        })?;
    let model = config.models.get(alias).ok_or_else(|| {
        RuntimeAdapterError::failed(
            operation,
            format!(
                "Model {alias:?} is not configured in config.toml. Add a [models.{alias:?}] entry with max_context_size."
            ),
        )
    })?;
    let provider = config.providers.get(&model.provider).ok_or_else(|| {
        RuntimeAdapterError::failed(
            operation,
            format!(
                "Provider {:?} for model {alias:?} is not configured.",
                model.provider
            ),
        )
    })?;
    let registry = provider_registry_config(&model.provider, provider, model, config, environment)
        .map_err(|message| RuntimeAdapterError::failed(operation, message))?;
    let effort = if config
        .thinking
        .as_ref()
        .and_then(|thinking| thinking.enabled)
        == Some(false)
    {
        None
    } else {
        config
            .thinking
            .as_ref()
            .and_then(|thinking| thinking.effort.as_deref())
            .or_else(|| effective_default_effort(model))
            .and_then(nonempty)
            .map(|value| ThinkingEffort::new(value.to_owned()))
            .transpose()
            .map_err(|error| {
                RuntimeAdapterError::failed(
                    operation,
                    format!("invalid thinking effort for model {alias:?}: {error}"),
                )
            })?
    };
    let capabilities = effective_capabilities(model);
    let always_thinking = capabilities
        .iter()
        .any(|capability| capability.eq_ignore_ascii_case("always_thinking"));
    let thinking_supported = always_thinking
        || effective_adaptive_thinking(model) == Some(true)
        || capabilities
            .iter()
            .any(|capability| capability.eq_ignore_ascii_case("thinking"));
    let mut effort_options = effective_support_efforts(model).to_vec();
    if effort_options.is_empty() {
        effort_options = if always_thinking {
            vec!["on".to_owned()]
        } else if thinking_supported {
            vec!["on".to_owned(), "off".to_owned()]
        } else {
            vec!["off".to_owned()]
        };
    } else if !always_thinking {
        effort_options.insert(0, "off".to_owned());
    }
    Ok(ResolvedModel {
        alias: alias.to_owned(),
        provider_id: model.provider.clone(),
        model_id: model.model.clone(),
        registry,
        thinking_effort: effort,
        max_completion_tokens: effective_max_output(model),
        max_context_tokens: model.max_context_size,
        google_application_credentials: environment
            .get(GOOGLE_APPLICATION_CREDENTIALS)
            .and_then(|value| nonempty(&value).map(PathBuf::from)),
        image_input: effective_capabilities(model)
            .iter()
            .any(|capability| capability.trim().eq_ignore_ascii_case("image_in")),
        video_input: effective_capabilities(model)
            .iter()
            .any(|capability| capability.trim().eq_ignore_ascii_case("video_in")),
        xhigh_supported: effective_support_efforts(model)
            .iter()
            .any(|effort| effort.trim().eq_ignore_ascii_case("xhigh")),
        effort_options,
        allow_unknown_effort: model.protocol == Some(ModelProtocol::Anthropic)
            || provider.provider_type == mycel_agent_protocol::ProviderType::Anthropic,
    })
}

fn provider_registry_config(
    provider_id: &str,
    provider: &ProviderEntryConfig,
    model: &ModelConfig,
    config: &MycelConfig,
    environment: &dyn RuntimeEnvironment,
) -> Result<ProviderRegistryConfig, String> {
    let api_key = provider_api_key(provider, environment);
    if provider.oauth.is_some() && api_key.is_some() {
        return Err(format!(
            "Provider {provider_id:?} has both api_key and oauth configured; remove one."
        ));
    }

    let managed_kimi = provider_id == "managed:kimi-code"
        && provider.provider_type == mycel_agent_protocol::ProviderType::Kimi
        && api_key.is_none();
    let (adapter, credential, models) = if managed_kimi {
        let mut adapter = managed_kimi_defaults();
        if let ProviderAdapterConfig::ManagedKimi { api_base_url, .. } = &mut adapter {
            if let Some(configured) = provider_base_url(provider, environment, "KIMI_BASE_URL") {
                *api_base_url = configured;
            }
        }
        (adapter, ProviderCredentialConfig::ManagedKimi, Vec::new())
    } else if provider.oauth.is_some() {
        if provider.provider_type != mycel_agent_protocol::ProviderType::OpenAiResponses {
            return Err(
                "Codex subscription authentication requires an openai_responses provider."
                    .to_owned(),
            );
        }
        let base_url = provider
            .base_url
            .as_deref()
            .and_then(nonempty)
            .ok_or_else(|| {
                format!(
                    "Codex subscription authentication requires base_url = {CODEX_SUBSCRIPTION_BASE_URL:?}."
                )
            })?;
        if base_url.trim_end_matches('/') != CODEX_SUBSCRIPTION_BASE_URL {
            return Err(format!(
                "Codex subscription authentication requires base_url = {CODEX_SUBSCRIPTION_BASE_URL:?}."
            ));
        }
        if !codex_enabled(config, environment)? {
            return Err(format!(
                "Codex subscription authentication is experimental. Enable {CODEX_FLAG:?} under [experimental] or set {CODEX_FLAG_ENV}=1."
            ));
        }
        (
            ProviderAdapterConfig::CodexSubscription,
            ProviderCredentialConfig::CodexSubscription,
            vec![provider_model(model)],
        )
    } else if let Some((adapter, credential)) = vertex_service_account_config(provider, environment)
    {
        (adapter, credential, vec![provider_model(model)])
    } else {
        let api_key = api_key.ok_or_else(|| {
            format!(
                "Provider {provider_id:?} has no API key. Configure api_key or its documented provider environment variable."
            )
        })?;
        let adapter = static_adapter(provider, model, environment)?;
        (
            adapter,
            ProviderCredentialConfig::ApiKey(ApiKeyCredentialConfig {
                configured: Some(SecretString::new(api_key)),
                environment: None,
                headers: BTreeMap::new(),
            }),
            vec![provider_model(model)],
        )
    };

    Ok(ProviderRegistryConfig {
        providers: vec![ProviderConfig {
            id: provider_id.to_owned(),
            adapter,
            credential,
            headers: provider.custom_headers.clone(),
            models,
        }],
    })
}

fn vertex_service_account_config(
    provider: &ProviderEntryConfig,
    environment: &dyn RuntimeEnvironment,
) -> Option<(ProviderAdapterConfig, ProviderCredentialConfig)> {
    if provider.provider_type != mycel_agent_protocol::ProviderType::VertexAi {
        return None;
    }
    let base_url = provider_base_url(provider, environment, "GOOGLE_VERTEX_BASE_URL");
    let project = provider_value(provider, environment, "GOOGLE_CLOUD_PROJECT")?;
    let location = provider_value(provider, environment, "GOOGLE_CLOUD_LOCATION")
        .or_else(|| base_url.as_deref().and_then(vertex_location_from_base_url))?;
    let credential = provider_value(provider, environment, GOOGLE_APPLICATION_CREDENTIALS)
        .map(PathBuf::from)
        .map(GoogleServiceAccountCredentialSource::File)
        .unwrap_or(GoogleServiceAccountCredentialSource::ApplicationDefault);
    Some((
        ProviderAdapterConfig::VertexServiceAccount {
            base_url,
            project,
            location,
        },
        ProviderCredentialConfig::GoogleServiceAccount(credential),
    ))
}

fn vertex_location_from_base_url(base_url: &str) -> Option<String> {
    let authority = base_url
        .trim()
        .strip_prefix("https://")
        .or_else(|| base_url.trim().strip_prefix("http://"))?
        .split(['/', '?', '#'])
        .next()?;
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    host.strip_suffix("-aiplatform.googleapis.com")
        .and_then(nonempty)
        .map(str::to_owned)
}

fn static_adapter(
    provider: &ProviderEntryConfig,
    model: &ModelConfig,
    environment: &dyn RuntimeEnvironment,
) -> Result<ProviderAdapterConfig, String> {
    if model.protocol == Some(ModelProtocol::Anthropic) {
        let base_url =
            provider_base_url(provider, environment, "ANTHROPIC_BASE_URL").map(|value| {
                value
                    .trim_end_matches('/')
                    .trim_end_matches("/v1")
                    .to_owned()
            });
        return Ok(ProviderAdapterConfig::Anthropic {
            base_url,
            beta_api: model.beta_api.unwrap_or(false),
            beta_features: Vec::new(),
            adaptive_thinking: effective_adaptive_thinking(model),
        });
    }
    use mycel_agent_protocol::ProviderType;
    Ok(match provider.provider_type {
        ProviderType::Anthropic => ProviderAdapterConfig::Anthropic {
            base_url: provider_base_url(provider, environment, "ANTHROPIC_BASE_URL"),
            beta_api: model.beta_api.unwrap_or(false),
            beta_features: Vec::new(),
            adaptive_thinking: effective_adaptive_thinking(model),
        },
        ProviderType::OpenAi => ProviderAdapterConfig::OpenAiChat {
            base_url: provider_base_url(provider, environment, "OPENAI_BASE_URL"),
        },
        ProviderType::OpenAiResponses => ProviderAdapterConfig::OpenAiResponses {
            base_url: provider_base_url(provider, environment, "OPENAI_BASE_URL"),
        },
        ProviderType::Kimi => ProviderAdapterConfig::Kimi {
            base_url: provider_base_url(provider, environment, "KIMI_BASE_URL"),
        },
        ProviderType::GoogleGenAi => ProviderAdapterConfig::Gemini {
            base_url: provider_base_url(provider, environment, "GOOGLE_GEMINI_BASE_URL"),
        },
        ProviderType::VertexAi => ProviderAdapterConfig::VertexApiKey {
            base_url: provider_base_url(provider, environment, "GOOGLE_VERTEX_BASE_URL"),
        },
    })
}

fn provider_model(model: &ModelConfig) -> ProviderModelConfig {
    ProviderModelConfig {
        id: model.model.clone(),
        display_name: model
            .overrides
            .as_ref()
            .and_then(|overrides| overrides.display_name.clone())
            .or_else(|| model.display_name.clone()),
        capability: None,
    }
}

fn provider_api_key(
    provider: &ProviderEntryConfig,
    environment: &dyn RuntimeEnvironment,
) -> Option<String> {
    provider
        .api_key
        .as_deref()
        .and_then(nonempty)
        .map(str::to_owned)
        .or_else(|| {
            let names: &[&str] = match provider.provider_type {
                mycel_agent_protocol::ProviderType::Anthropic => &["ANTHROPIC_API_KEY"],
                mycel_agent_protocol::ProviderType::OpenAi
                | mycel_agent_protocol::ProviderType::OpenAiResponses => &["OPENAI_API_KEY"],
                mycel_agent_protocol::ProviderType::Kimi => &["KIMI_API_KEY"],
                mycel_agent_protocol::ProviderType::GoogleGenAi => &["GOOGLE_API_KEY"],
                mycel_agent_protocol::ProviderType::VertexAi => {
                    &["VERTEXAI_API_KEY", "GOOGLE_API_KEY"]
                }
            };
            names
                .iter()
                .find_map(|name| provider_value(provider, environment, name))
        })
}

fn provider_base_url(
    provider: &ProviderEntryConfig,
    environment: &dyn RuntimeEnvironment,
    name: &str,
) -> Option<String> {
    provider
        .base_url
        .as_deref()
        .and_then(nonempty)
        .map(str::to_owned)
        .or_else(|| provider_value(provider, environment, name))
}

fn provider_value(
    provider: &ProviderEntryConfig,
    environment: &dyn RuntimeEnvironment,
    name: &str,
) -> Option<String> {
    provider
        .env
        .get(name)
        .and_then(|value| nonempty(value))
        .map(str::to_owned)
        .or_else(|| {
            environment
                .get(name)
                .and_then(|value| nonempty(&value).map(str::to_owned))
        })
}

fn codex_enabled(
    config: &MycelConfig,
    environment: &dyn RuntimeEnvironment,
) -> Result<bool, String> {
    if config.experimental.get(CODEX_FLAG).copied() == Some(true) {
        return Ok(true);
    }
    let Some(value) = environment.get(CODEX_FLAG_ENV) else {
        return Ok(false);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" | "" => Ok(false),
        _ => Err(format!(
            "{CODEX_FLAG_ENV} must be a boolean (true/false/1/0/yes/no/on/off), got {value:?}."
        )),
    }
}

fn effective_max_output(model: &ModelConfig) -> Option<u64> {
    model
        .overrides
        .as_ref()
        .and_then(|overrides| overrides.max_output_size)
        .or(model.max_output_size)
}

fn effective_default_effort(model: &ModelConfig) -> Option<&str> {
    model
        .overrides
        .as_ref()
        .and_then(|overrides| overrides.default_effort.as_deref())
        .or(model.default_effort.as_deref())
}

fn effective_adaptive_thinking(model: &ModelConfig) -> Option<bool> {
    model
        .overrides
        .as_ref()
        .and_then(|overrides| overrides.adaptive_thinking)
        .or(model.adaptive_thinking)
}

fn effective_support_efforts(model: &ModelConfig) -> &[String] {
    model
        .overrides
        .as_ref()
        .and_then(|overrides| overrides.support_efforts.as_deref())
        .unwrap_or(&model.support_efforts)
}

fn effective_capabilities(model: &ModelConfig) -> &[String] {
    model
        .overrides
        .as_ref()
        .and_then(|overrides| overrides.capabilities.as_deref())
        .unwrap_or(&model.capabilities)
}

fn configured_hook_runner(config: &MycelConfig, cwd: &Path) -> Result<HookRunner, String> {
    let runner = HookRunner::new();
    for hook in &config.hooks {
        let event = runtime_hook_event(hook.event);
        let matcher = match hook.matcher.as_deref().and_then(nonempty) {
            Some(pattern) => HookMatcher::tool_name_regex(pattern.to_owned())
                .map_err(|error| format!("invalid {:?} hook matcher: {error}", hook.event))?,
            None => HookMatcher::Any,
        };
        runner
            .register(HookRegistration {
                event,
                matcher,
                command: hook.command.clone(),
                cwd: cwd.to_owned(),
                timeout: hook.timeout.map(std::time::Duration::from_secs),
                fail_mode: match hook.fail_mode {
                    Some(HookFailMode::Closed) => CommandHookFailMode::Closed,
                    Some(HookFailMode::Open) | None => CommandHookFailMode::Open,
                },
            })
            .map_err(|error| format!("could not register {:?} hook: {error}", hook.event))?;
    }
    Ok(runner)
}

fn runtime_hook_event(event: HookEvent) -> ToolHookEvent {
    match event {
        HookEvent::PreToolUse => ToolHookEvent::PreToolUse,
        HookEvent::PostToolUse => ToolHookEvent::PostToolUse,
        HookEvent::PostToolUseFailure => ToolHookEvent::PostToolUseFailure,
        HookEvent::PermissionRequest => ToolHookEvent::PermissionRequest,
        HookEvent::PermissionResult => ToolHookEvent::PermissionResult,
        HookEvent::UserPromptSubmit => ToolHookEvent::UserPromptSubmit,
        HookEvent::Stop => ToolHookEvent::Stop,
        HookEvent::StopFailure => ToolHookEvent::StopFailure,
        HookEvent::Interrupt => ToolHookEvent::Interrupt,
        HookEvent::SessionStart => ToolHookEvent::SessionStart,
        HookEvent::SessionEnd => ToolHookEvent::SessionEnd,
        HookEvent::SubagentStart => ToolHookEvent::SubagentStart,
        HookEvent::SubagentStop => ToolHookEvent::SubagentStop,
        HookEvent::PreCompact => ToolHookEvent::PreCompact,
        HookEvent::PostCompact => ToolHookEvent::PostCompact,
        HookEvent::Notification => ToolHookEvent::Notification,
    }
}

enum DialogRpc {
    Approval {
        request: ApprovalRequest,
        reply: tokio::sync::oneshot::Sender<Result<ApprovalResponse, PortError>>,
    },
    Question {
        request: QuestionRequest,
        reply: tokio::sync::oneshot::Sender<Result<QuestionResponse, PortError>>,
    },
}

impl DialogRpc {
    fn cancel(self, reason: &str) {
        match self {
            Self::Approval { reply, .. } => {
                let _ = reply.send(Err(PortError::new(reason)));
            }
            Self::Question { reply, .. } => {
                let _ = reply.send(Err(PortError::new(reason)));
            }
        }
    }
}

struct InteractiveDialogPort {
    sender: mpsc::Sender<DialogRpc>,
    closed: Mutex<bool>,
}

impl InteractiveDialogPort {
    fn close(&self) {
        *self
            .closed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
    }

    fn enqueue(&self, request: DialogRpc) -> Result<(), PortError> {
        let closed = self
            .closed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *closed {
            return Err(PortError::new("interactive dialog host is closed"));
        }
        self.sender
            .send(request)
            .map_err(|_| PortError::new("interactive dialog host is unavailable"))
    }
}

impl ApprovalPort for InteractiveDialogPort {
    fn request_approval<'a>(
        &'a self,
        request: ApprovalRequest,
    ) -> PortFuture<'a, Result<ApprovalResponse, PortError>> {
        let (reply, response) = tokio::sync::oneshot::channel();
        let enqueued = self.enqueue(DialogRpc::Approval { request, reply });
        Box::pin(async move {
            enqueued?;
            response
                .await
                .map_err(|_| PortError::new("interactive approval was cancelled"))?
        })
    }
}

impl QuestionPort for InteractiveDialogPort {
    fn ask<'a>(
        &'a self,
        request: QuestionRequest,
    ) -> PortFuture<'a, Result<QuestionResponse, PortError>> {
        let (reply, response) = tokio::sync::oneshot::channel();
        let enqueued = self.enqueue(DialogRpc::Question { request, reply });
        Box::pin(async move {
            enqueued?;
            response
                .await
                .map_err(|_| PortError::new("interactive question was cancelled"))?
        })
    }
}

fn interactive_dialog_channel() -> (Arc<InteractiveDialogPort>, mpsc::Receiver<DialogRpc>) {
    let (sender, receiver) = mpsc::channel();
    (
        Arc::new(InteractiveDialogPort {
            sender,
            closed: Mutex::new(false),
        }),
        receiver,
    )
}

enum ActiveDialog {
    Approval {
        request: ApprovalRequest,
        reducer: ApprovalDialogReducer,
        reply: tokio::sync::oneshot::Sender<Result<ApprovalResponse, PortError>>,
    },
    Question {
        request: QuestionRequest,
        reducer: QuestionDialogReducer,
        reply: tokio::sync::oneshot::Sender<Result<QuestionResponse, PortError>>,
    },
}

impl ActiveDialog {
    fn from_rpc(request: DialogRpc) -> Self {
        match request {
            DialogRpc::Approval { request, reply } => {
                let choices = approval_dialog_choices(&request);
                let preview_available =
                    matches!(request.display, ToolInputDisplay::PlanReview { .. });
                Self::Approval {
                    request,
                    reducer: ApprovalDialogReducer::new(choices, preview_available),
                    reply,
                }
            }
            DialogRpc::Question { request, reply } => {
                let questions = request
                    .questions
                    .iter()
                    .map(|question| QuestionItem {
                        question: question.prompt.clone(),
                        header: None,
                        multi_select: question.multiple,
                        other_label: Some("Other".to_owned()),
                        options: question
                            .options
                            .iter()
                            .map(|option| DialogQuestionOption {
                                label: option.label.clone(),
                                description: option.description.clone(),
                            })
                            .collect(),
                    })
                    .collect();
                Self::Question {
                    request,
                    reducer: QuestionDialogReducer::new(questions),
                    reply,
                }
            }
        }
    }

    fn cancel(self, reason: &str) {
        match self {
            Self::Approval { reply, .. } => {
                let _ = reply.send(Err(PortError::new(reason)));
            }
            Self::Question { reply, .. } => {
                let _ = reply.send(Err(PortError::new(reason)));
            }
        }
    }
}

struct DialogHost {
    port: Arc<InteractiveDialogPort>,
    receiver: mpsc::Receiver<DialogRpc>,
    queue: VecDeque<DialogRpc>,
    active: Option<ActiveDialog>,
    show_detail: bool,
}

impl DialogHost {
    fn new(port: Arc<InteractiveDialogPort>, receiver: mpsc::Receiver<DialogRpc>) -> Self {
        Self {
            port,
            receiver,
            queue: VecDeque::new(),
            active: None,
            show_detail: true,
        }
    }

    fn poll(&mut self) {
        while let Ok(request) = self.receiver.try_recv() {
            self.queue.push_back(request);
        }
        self.activate_next();
    }

    fn is_active(&self) -> bool {
        self.active.is_some()
    }

    fn activate_next(&mut self) {
        if self.active.is_none() {
            self.active = self.queue.pop_front().map(ActiveDialog::from_rpc);
            self.show_detail = true;
        }
    }

    fn apply(&mut self, event: InputEvent) {
        let mut outcome = None;
        match self.active.as_mut() {
            Some(ActiveDialog::Approval {
                request: _,
                reducer,
                reply: _,
            }) => {
                reducer.apply(event);
                for action in std::mem::take(&mut reducer.actions) {
                    match action {
                        ApprovalDialogAction::Respond {
                            decision,
                            feedback,
                            selected_label,
                        } => {
                            outcome = Some(DialogOutcome::Approval(ApprovalResponse {
                                decision: match decision {
                                    DialogApprovalDecision::Approved
                                    | DialogApprovalDecision::ApprovedForSession => {
                                        ProtocolApprovalDecision::Approved
                                    }
                                    DialogApprovalDecision::Rejected => {
                                        ProtocolApprovalDecision::Rejected
                                    }
                                    DialogApprovalDecision::Cancelled => {
                                        ProtocolApprovalDecision::Cancelled
                                    }
                                },
                                scope: (decision == DialogApprovalDecision::ApprovedForSession)
                                    .then_some(ApprovalScope::Session),
                                feedback,
                                selected_label,
                            }));
                            break;
                        }
                        ApprovalDialogAction::OpenPreview => self.show_detail = true,
                        ApprovalDialogAction::ToggleToolOutput => {
                            self.show_detail = !self.show_detail;
                        }
                    }
                }
            }
            Some(ActiveDialog::Question {
                request,
                reducer,
                reply: _,
            }) => {
                reducer.apply(event);
                let resolved = reducer.resolved_answers();
                for action in std::mem::take(&mut reducer.actions) {
                    match action {
                        QuestionDialogAction::Answer { answers, .. } => {
                            let answers = if answers.is_empty() {
                                Vec::new()
                            } else {
                                request
                                    .questions
                                    .iter()
                                    .zip(resolved.iter())
                                    .filter_map(|(question, answer)| {
                                        answer.as_ref().map(|answer| QuestionAnswer {
                                            question_id: question.id.clone(),
                                            selected_labels: answer.selected_labels.clone(),
                                            text: answer.text.clone(),
                                        })
                                    })
                                    .collect()
                            };
                            outcome = Some(DialogOutcome::Question(QuestionResponse { answers }));
                            break;
                        }
                        QuestionDialogAction::ToggleToolOutput => {
                            self.show_detail = !self.show_detail;
                        }
                    }
                }
            }
            None => {}
        }
        if let Some(outcome) = outcome {
            let active = self
                .active
                .take()
                .expect("dialog outcome requires active dialog");
            match (active, outcome) {
                (ActiveDialog::Approval { reply, .. }, DialogOutcome::Approval(response)) => {
                    let _ = reply.send(Ok(response));
                }
                (ActiveDialog::Question { reply, .. }, DialogOutcome::Question(response)) => {
                    let _ = reply.send(Ok(response));
                }
                (active, _) => active.cancel("interactive dialog response type mismatch"),
            }
            self.activate_next();
        }
    }

    fn cancel_all(&mut self, reason: &str) {
        self.port.close();
        if let Some(active) = self.active.take() {
            active.cancel(reason);
        }
        while let Some(request) = self.queue.pop_front() {
            request.cancel(reason);
        }
        while let Ok(request) = self.receiver.try_recv() {
            request.cancel(reason);
        }
    }
}

impl Drop for DialogHost {
    fn drop(&mut self) {
        self.cancel_all("interactive dialog host closed");
    }
}

enum DialogOutcome {
    Approval(ApprovalResponse),
    Question(QuestionResponse),
}

fn approval_dialog_choices(request: &ApprovalRequest) -> Vec<ApprovalChoice> {
    match &request.display {
        ToolInputDisplay::PlanReview { options, .. } => {
            let mut choices = match options {
                Some(options) if options.len() >= 2 => options
                    .iter()
                    .map(|option| ApprovalChoice {
                        label: option.label.clone(),
                        decision: DialogApprovalDecision::Approved,
                        selected_label: Some(option.label.clone()),
                        requires_feedback: false,
                    })
                    .collect::<Vec<_>>(),
                _ => vec![ApprovalChoice {
                    label: "Approve".to_owned(),
                    decision: DialogApprovalDecision::Approved,
                    selected_label: Some("Approve".to_owned()),
                    requires_feedback: false,
                }],
            };
            choices.extend([
                ApprovalChoice {
                    label: "Reject".to_owned(),
                    decision: DialogApprovalDecision::Rejected,
                    selected_label: Some("Reject".to_owned()),
                    requires_feedback: false,
                },
                ApprovalChoice {
                    label: "Revise".to_owned(),
                    decision: DialogApprovalDecision::Rejected,
                    selected_label: Some("Revise".to_owned()),
                    requires_feedback: true,
                },
            ]);
            choices
        }
        ToolInputDisplay::GoalStart { mode, .. } => goal_start_choices(*mode),
        _ => vec![
            ApprovalChoice {
                label: "Approve once".to_owned(),
                decision: DialogApprovalDecision::Approved,
                selected_label: None,
                requires_feedback: false,
            },
            ApprovalChoice {
                label: "Approve for this session".to_owned(),
                decision: DialogApprovalDecision::ApprovedForSession,
                selected_label: None,
                requires_feedback: false,
            },
            ApprovalChoice {
                label: "Reject".to_owned(),
                decision: DialogApprovalDecision::Rejected,
                selected_label: None,
                requires_feedback: false,
            },
            ApprovalChoice {
                label: "Reject with feedback".to_owned(),
                decision: DialogApprovalDecision::Rejected,
                selected_label: None,
                requires_feedback: true,
            },
        ],
    }
}

fn goal_start_choices(mode: GoalStartMode) -> Vec<ApprovalChoice> {
    let mut choices = vec![
        ("Switch to Auto and start", "auto"),
        (
            if mode == GoalStartMode::Yolo {
                "Keep YOLO and start"
            } else {
                "Switch to YOLO and start"
            },
            "yolo",
        ),
    ];
    if mode == GoalStartMode::Manual {
        choices.push(("Start in Manual", "manual"));
    }
    let mut choices = choices
        .into_iter()
        .map(|(label, selected)| ApprovalChoice {
            label: label.to_owned(),
            decision: DialogApprovalDecision::Approved,
            selected_label: Some(selected.to_owned()),
            requires_feedback: false,
        })
        .collect::<Vec<_>>();
    choices.push(ApprovalChoice {
        label: "Do not start".to_owned(),
        decision: DialogApprovalDecision::Cancelled,
        selected_label: Some("cancel".to_owned()),
        requires_feedback: false,
    });
    choices
}

struct InteractiveRunContext {
    home: PathBuf,
    working_dir: PathBuf,
    config: MycelConfig,
    resolved: ResolvedModel,
    transport: Arc<dyn HttpTransport>,
    version: String,
    tool_registry: Arc<dyn ToolRegistryBuilder>,
    session: SessionSelection,
    permission: ProtocolPermissionMode,
    additional_dirs: Vec<PathBuf>,
    skill_dirs: Vec<PathBuf>,
    user_home: Option<PathBuf>,
    shell: Option<String>,
    external_editor: Option<String>,
    startup_plan: bool,
    mcp: SessionMcpServices,
    plugins: PluginComposition,
}

struct SessionMcpServices {
    config: McpConfigFile,
    connector_factory: Arc<dyn McpConnectorFactory>,
    environment: Arc<dyn McpEnvironment>,
}

async fn start_configured_session_mcp(
    services: &SessionMcpServices,
    home: &Path,
    registry: &ToolRegistry,
    session: &SessionHandle,
    working_dir: &Path,
) -> Result<Option<McpRuntime>, String> {
    if services.config.mcp_servers.is_empty() {
        return Ok(None);
    }
    let connector = services.connector_factory.create(home)?;
    start_session_mcp(
        SessionMcpContext {
            registry: registry.clone(),
            config: services.config.clone(),
            connector,
            environment: Arc::clone(&services.environment),
            session: session.clone(),
            working_dir: working_dir.to_path_buf(),
        },
        &CancellationToken::new(),
    )
    .await
    .map(Some)
}

#[derive(Default)]
struct ProductionOrchestrationEvents {
    pending: Mutex<VecDeque<OrchestrationEvent>>,
}

impl ProductionOrchestrationEvents {
    fn drain(&self) -> Vec<OrchestrationEvent> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
            .collect()
    }
}

impl LiveEventSink for ProductionOrchestrationEvents {
    fn publish(&self, event: OrchestrationEvent) {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push_back(event);
    }
}

struct ProductionNativeSessionFactory {
    permission: ProtocolPermissionMode,
    permission_rules: Vec<PermissionRule>,
    approval_port: Option<Arc<dyn ApprovalPort>>,
    question_port: Option<Arc<dyn QuestionPort>>,
    hooks: HookRunner,
}

impl NativeSessionOptionsFactory for ProductionNativeSessionFactory {
    fn build(&self, context: &NativeChildContext) -> Result<SessionOptions, String> {
        let mut options = SessionOptions::new(context.session_id.clone());
        options.initial_permission_mode = self.permission;
        options.permission_rules = self.permission_rules.clone();
        options.approval_port = self.approval_port.clone();
        options.question_port = self.question_port.clone();
        options.hooks = self.hooks.clone();
        Ok(options)
    }
}

struct ProductionNativeTurnFactory {
    provider: Arc<dyn TurnProvider>,
    registry: ToolRegistry,
    hooks: HookRunner,
    engine_config: TurnEngineConfig,
    system_prompt: Arc<str>,
    thinking_effort: Option<ThinkingEffort>,
    max_completion_tokens: Option<u64>,
}

impl NativeTurnEngineFactory for ProductionNativeTurnFactory {
    fn build(&self, context: &NativeChildContext) -> Result<NativeTurnRuntime, String> {
        let shared = self.registry.snapshot();
        let child_tools = ToolRegistry::new();
        for name in &context.profile.capabilities.tools {
            let tool = shared
                .get(name)
                .ok_or_else(|| format!("native child tool {name:?} is not registered"))?;
            child_tools.register(tool).map_err(|error| {
                format!("could not register native child tool {name:?}: {error}")
            })?;
        }
        let engine = TurnEngine::new(
            Arc::clone(&self.provider),
            child_tools,
            self.hooks.clone(),
            ToolScheduler::new(),
            self.engine_config.clone(),
        )
        .map_err(|error| format!("could not build native child turn engine: {error}"))?;
        Ok(NativeTurnRuntime {
            engine: Arc::new(engine),
            effective_capabilities: context.profile.capabilities.clone(),
            system_prompt: format!(
                "{}\n\n# Subagent role\n\nYou are a bounded Mycel subagent using the {:?} worker profile. Complete the delegated task and return a concise result. Do not attempt to delegate, swarm, or start workflows unless those tools are explicitly present in your capability set.",
                self.system_prompt,
                context.profile.name
            ),
            thinking_effort: self.thinking_effort.clone(),
            max_completion_tokens: self.max_completion_tokens,
            metadata: BTreeMap::from([
                ("agent_id".to_owned(), Value::String(context.agent_id.clone())),
                (
                    "parent_agent_id".to_owned(),
                    Value::String(context.parent_agent_id.clone()),
                ),
            ]),
        })
    }
}

struct NativeOrchestrationContext {
    runtime: Runtime,
    registry: ToolRegistry,
    session: SessionHandle,
    home: PathBuf,
    working_dir: PathBuf,
    additional_dirs: Vec<PathBuf>,
    provider: Arc<dyn TurnProvider>,
    hooks: HookRunner,
    engine_config: TurnEngineConfig,
    system_prompt: Arc<str>,
    permission: ProtocolPermissionMode,
    permission_rules: Vec<PermissionRule>,
    approval_port: Option<Arc<dyn ApprovalPort>>,
    question_port: Option<Arc<dyn QuestionPort>>,
    thinking_effort: Option<ThinkingEffort>,
    max_completion_tokens: Option<u64>,
    xhigh_supported: bool,
    live_events: Arc<ProductionOrchestrationEvents>,
}

fn open_native_orchestration(
    context: NativeOrchestrationContext,
) -> Result<Arc<NativeOrchestrationBundle>, String> {
    let snapshot = context.registry.snapshot();
    let child_tool_names = snapshot
        .definitions()
        .iter()
        .map(|definition| definition.name.clone())
        .collect::<BTreeSet<_>>();
    let mut root_tool_names = child_tool_names.clone();
    root_tool_names.extend(
        ORCHESTRATION_TOOL_NAMES
            .iter()
            .map(|name| (*name).to_owned()),
    );
    let filesystem_roots = std::iter::once(&context.working_dir)
        .chain(context.additional_dirs.iter())
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<BTreeSet<_>>();
    let child_capabilities = CapabilitySet {
        tools: child_tool_names,
        filesystem_roots: filesystem_roots.clone(),
        network: false,
        can_spawn_subagents: false,
        can_swarm: false,
        can_workflow: false,
    };
    let root_capabilities = CapabilitySet {
        tools: root_tool_names,
        filesystem_roots,
        network: false,
        can_spawn_subagents: true,
        can_swarm: true,
        can_workflow: true,
    };
    let profile = WorkerProfile {
        name: "general".to_owned(),
        capabilities: child_capabilities,
        allow_delegation: false,
    };
    let root = OrchestrationRootConfig::new(
        context.session.main_agent_id().as_str(),
        root_capabilities,
        BTreeMap::from([("general".to_owned(), profile)]),
        "general",
    );
    let sessions = Arc::new(ProductionNativeSessionFactory {
        permission: context.permission,
        permission_rules: context.permission_rules,
        approval_port: context.approval_port,
        question_port: context.question_port,
        hooks: context.hooks.clone(),
    });
    let turns = Arc::new(ProductionNativeTurnFactory {
        provider: context.provider,
        registry: context.registry.clone(),
        hooks: context.hooks,
        engine_config: context.engine_config,
        system_prompt: context.system_prompt,
        thinking_effort: context.thinking_effort.clone(),
        max_completion_tokens: context.max_completion_tokens,
    });
    let dependencies = NativeOrchestrationDependencies::new(
        context.runtime,
        context.registry,
        context.live_events,
        sessions,
        turns,
    );
    let config = NativeOrchestrationBundleConfig::new(
        context.session,
        context.home.join("orchestration"),
        root,
    )
    .with_hyphae(context.thinking_effort, context.xhigh_supported)
    .with_shutdown_policy(BackgroundShutdown::StopAll);
    NativeOrchestrationBundle::open(dependencies, config)
        .map(Arc::new)
        .map_err(|error| format!("could not initialize native orchestration: {error}"))
}

async fn shutdown_orchestration(
    orchestration: Option<&NativeOrchestrationBundle>,
) -> Result<(), String> {
    match orchestration {
        Some(orchestration) => orchestration
            .shutdown(BackgroundShutdown::StopAll)
            .await
            .map(|_| ())
            .map_err(|error| format!("could not shut down native orchestration: {error}")),
        None => Ok(()),
    }
}

struct PreparedInteractive {
    // SessionHandle stores a weak runtime reference. Keep the owner alive for
    // the whole terminal session so close() can unregister it cleanly.
    _runtime: Runtime,
    home: PathBuf,
    working_dir: PathBuf,
    additional_dirs: Vec<PathBuf>,
    user_home: Option<PathBuf>,
    editor_fallback: Option<String>,
    initial_draft: Option<String>,
    session: SessionHandle,
    session_index: SessionIndex,
    dialog_port: Arc<InteractiveDialogPort>,
    dialog_receiver: Mutex<Option<mpsc::Receiver<DialogRpc>>>,
    engine: Arc<TurnEngine>,
    btw_provider: Arc<dyn TurnProvider>,
    btw_hooks: HookRunner,
    btw_engine_config: TurnEngineConfig,
    compaction: Arc<CompactionEngine>,
    system_prompt: Arc<str>,
    model_alias: String,
    model_aliases: Vec<String>,
    provider: String,
    context_window: u64,
    thinking_effort: Option<ThinkingEffort>,
    effort_options: Vec<String>,
    allow_unknown_effort: bool,
    max_completion_tokens: Option<u64>,
    plan_file: PathBuf,
    plan_mode: bool,
    swarm_mode: bool,
    /// Welcome-card recent-session titles, captured from the discovery the
    /// startup register/refresh already produced.
    recent_sessions: Vec<String>,
    /// The current session's title (or short id) for the session rail, from
    /// the same discovery.
    session_name: String,
    warning: Option<String>,
    tui_config: TuiConfig,
    mcp: Option<McpRuntime>,
    orchestration: Arc<NativeOrchestrationBundle>,
    orchestration_events: Arc<ProductionOrchestrationEvents>,
    ecology: EcologyService,
    /// Substrate snapshot taken once during preparation; the loop state
    /// refreshes it after ecology-mutating events, never per-tick.
    substrate: SubstrateStatus,
    plugins: PluginComposition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InteractiveSessionTransition {
    New,
    Resume(String),
    Reload,
    Fork,
    Model(String),
    AddDir { path: PathBuf, notice: String },
    Provider { command: Command, close_after: bool },
    ExternalEditor { command: String, draft: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedSessionTransition {
    action: InteractiveSessionTransition,
    session_id: String,
    permission: PermissionMode,
    plan: bool,
}

enum PreparedInteractiveOutcome {
    Completion(RuntimeCompletion),
    Transition(PreparedSessionTransition),
}

enum InteractiveTerminalOutcome {
    Completion(RuntimeCompletion),
    Transition(InteractiveSessionTransition),
}

async fn prepare_interactive(
    context: InteractiveRunContext,
) -> Result<PreparedInteractive, String> {
    let InteractiveRunContext {
        home,
        working_dir,
        config,
        resolved,
        transport,
        version,
        tool_registry,
        session,
        permission,
        additional_dirs,
        skill_dirs,
        user_home,
        shell,
        external_editor,
        startup_plan,
        mcp,
        plugins,
    } = context;
    let (tui_config, tui_config_warning) = load_tui_config(&home);
    // The rebuilt TUI has no light palette; `light` resolves to amanita (see
    // `active_theme`), and pretending otherwise would be silent.
    let light_theme_warning =
        (tui_config.theme == ThemeName::Light).then(|| LIGHT_THEME_WARNING.to_owned());
    let mut factory = ProviderFactory::new(transport, home.clone(), version);
    if let Some(path) = resolved.google_application_credentials.clone() {
        factory = factory.with_google_application_credentials(path);
    }
    let registry = factory
        .build(resolved.registry.clone())
        .await
        .map_err(|error| format!("could not initialize provider registry: {error}"))?;
    let detected_capability = registry
        .model(&resolved.provider_id, &resolved.model_id)
        .map(|model| model.capability)
        .ok_or_else(|| {
            format!(
                "Model {:?} resolved to {}/{} but that provider model is unavailable.",
                resolved.alias, resolved.provider_id, resolved.model_id
            )
        })?;
    let provider: Arc<dyn TurnProvider> = Arc::new(RegistryTurnProvider {
        registry: Arc::new(registry),
        provider_id: resolved.provider_id.clone(),
        model_id: resolved.model_id.clone(),
    });
    let hooks = configured_hook_runner(&config, &working_dir)?;

    let runtime = Runtime::new(home.join(SESSIONS_DIR));
    let id = match &session {
        SessionSelection::New => SessionId::generate(),
        SessionSelection::Resume(id) => SessionId::new(id.clone())
            .map_err(|error| format!("invalid session id {id:?}: {error}"))?,
        SessionSelection::Pick | SessionSelection::Continue => {
            return Err("unsupported interactive session selection reached runtime".to_owned());
        }
    };
    let mut options = SessionOptions::new(id);
    let (dialog_port, dialog_receiver) = interactive_dialog_channel();
    options.initial_permission_mode = permission;
    options.permission_rules = config
        .permission
        .as_ref()
        .map(|configured| configured.rules.clone())
        .unwrap_or_default();
    options.hooks = hooks.clone();
    options.approval_port = Some(dialog_port.clone());
    options.question_port = Some(dialog_port.clone());
    let is_new = matches!(session, SessionSelection::New);
    let session_handle = match session {
        SessionSelection::New => runtime.create_session(options).await,
        SessionSelection::Resume(_) => runtime.resume_session(options).await,
        SessionSelection::Pick | SessionSelection::Continue => unreachable!("validated above"),
    }
    .map_err(|error| error.to_string())?;
    let plan_file = match resolve_plan_file(&home, &session_handle).await {
        Ok(path) => path,
        Err(error) => return Err(close_after_setup_error(&session_handle, error).await),
    };
    let plan_local = match LocalToolConfig::new(&working_dir, additional_dirs.iter())
        .map_err(|error| format!("invalid plan workspace roots: {error}"))
        .and_then(|local| {
            local
                .with_allowed_files([&plan_file])
                .map_err(|error| format!("invalid plan-file grant: {error}"))
        }) {
        Ok(local) => local,
        Err(error) => return Err(close_after_setup_error(&session_handle, error).await),
    };
    let foreground_processes = Arc::new(DeferredForegroundProcessPort::default());
    let tools = match tool_registry.build(
        &working_dir,
        &additional_dirs,
        std::slice::from_ref(&plan_file),
        Some(foreground_processes.clone()),
    ) {
        Ok(tools) => tools,
        Err(error) => return Err(close_after_setup_error(&session_handle, error).await),
    };
    if let Err(error) = register_plugin_commands(&tools, &plugins) {
        return Err(close_after_setup_error(&session_handle, error).await);
    }
    let skills = match compose_skills(
        &config,
        &skill_dirs,
        &home,
        user_home.as_deref(),
        &working_dir,
        &plugins.plan.skill_roots,
    ) {
        Ok(skills) => skills,
        Err(error) => return Err(close_after_setup_error(&session_handle, error).await),
    };
    let media = match media_config(
        &working_dir,
        &additional_dirs,
        &resolved,
        detected_capability,
    ) {
        Ok(media) => media,
        Err(error) => return Err(close_after_setup_error(&session_handle, error).await),
    };
    let PreparedSystemPrompt {
        text: system_prompt,
        warnings: system_prompt_warnings,
    } = build_system_prompt(SystemPromptContext {
        cwd: &working_dir,
        additional_dirs: &additional_dirs,
        mycel_home: &home,
        user_home: user_home.as_deref(),
        shell: shell.as_deref(),
        now: Utc::now(),
        skills: &skills.catalog,
    });
    let system_prompt: Arc<str> = Arc::from(system_prompt);
    let should_start_plan = startup_plan || is_new && config.default_plan_mode == Some(true);
    if should_start_plan {
        let snapshot =
            match <SessionHandle as SessionBuiltinStatePort>::snapshot(&session_handle).await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return Err(close_after_setup_error(&session_handle, error).await);
                }
            };
        if !snapshot.plan_mode {
            if let Err(error) = session_handle
                .enter_plan_mode(Some(plan_file.to_string_lossy().into_owned()))
                .await
            {
                return Err(close_after_setup_error(
                    &session_handle,
                    format!("could not enable startup plan mode: {error}"),
                )
                .await);
            }
        }
    }
    let session_index = SessionIndex::new(&home);
    let session_id = session_handle.id().as_str().to_owned();
    let indexed = if is_new {
        session_index.register_session_discovering(&session_id, &working_dir, &additional_dirs)
    } else {
        session_index.refresh_discovering(&session_id)
    };
    // The register/refresh already ran the full locked repair scan; keep its
    // discovery for the welcome card's recent-sessions list so startup never
    // pays a second index lock and repair (review item 5, option (a)).
    let (session_name, recent_sessions) = match indexed {
        Ok((_, discovery)) => {
            // The rail's session name comes from the same discovery: the
            // current session's title when one is set, else its short id.
            let name = discovery
                .sessions
                .iter()
                .find(|summary| summary.id == session_id)
                .and_then(|summary| summary.title.clone())
                .unwrap_or_else(|| crate::util::short_id(&session_id));
            let recent = discovery
                .sessions
                .into_iter()
                .take(3)
                .map(|summary| {
                    summary
                        .title
                        .unwrap_or_else(|| crate::util::short_id(&summary.id))
                })
                .collect();
            (name, recent)
        }
        Err(error) => {
            let close = session_handle.close().await;
            return Err(match close {
                Ok(()) => format!("could not update session index: {error}"),
                Err(close) => format!(
                    "could not update session index: {error}; additionally session cleanup failed: {close}"
                ),
            });
        }
    };
    let current_permission = session_handle.snapshot().await.state.permission_mode;
    if current_permission != permission {
        if let Err(error) = session_handle.set_permission_mode(permission).await {
            let close = session_handle
                .close()
                .await
                .map_err(|close| format!("could not close session: {close}"));
            let refresh = session_index
                .refresh(&session_id)
                .map(|_| ())
                .map_err(|refresh| format!("could not refresh session index: {refresh}"));
            let cleanup = combine_string_cleanup_results(Ok(()), Ok(()), close, refresh);
            return Err(match cleanup {
                Ok(()) => format!("could not apply requested permission mode: {error}"),
                Err(cleanup) => format!(
                    "could not apply requested permission mode: {error}; additionally cleanup failed: {cleanup}"
                ),
            });
        }
    }
    let mcp_runtime = match start_configured_session_mcp(
        &mcp,
        &home,
        &tools,
        &session_handle,
        &working_dir,
    )
    .await
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let close = session_handle
                .close()
                .await
                .map_err(|close| format!("could not close session: {close}"));
            let refresh = session_index
                .refresh(&session_id)
                .map(|_| ())
                .map_err(|refresh| format!("could not refresh session index: {refresh}"));
            let cleanup = combine_string_cleanup_results(Ok(()), Ok(()), close, refresh);
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; additionally cleanup failed: {cleanup}"),
            });
        }
    };
    let engine_config = turn_engine_config(&config, resolved.max_context_tokens);
    let orchestration_events = Arc::new(ProductionOrchestrationEvents::default());
    let orchestration = match open_native_orchestration(NativeOrchestrationContext {
        runtime: runtime.clone(),
        registry: tools.clone(),
        session: session_handle.clone(),
        home: home.clone(),
        working_dir: working_dir.clone(),
        additional_dirs: additional_dirs.clone(),
        provider: Arc::clone(&provider),
        hooks: hooks.clone(),
        engine_config: engine_config.clone(),
        system_prompt: Arc::clone(&system_prompt),
        permission,
        permission_rules: config
            .permission
            .as_ref()
            .map(|configured| configured.rules.clone())
            .unwrap_or_default(),
        approval_port: Some(dialog_port.clone()),
        question_port: Some(dialog_port.clone()),
        thinking_effort: resolved.thinking_effort.clone(),
        max_completion_tokens: resolved.max_completion_tokens,
        xhigh_supported: resolved.xhigh_supported,
        live_events: Arc::clone(&orchestration_events),
    }) {
        Ok(orchestration) => orchestration,
        Err(error) => {
            let shutdown = shutdown_mcp(mcp_runtime.as_ref()).await;
            let close = session_handle
                .close()
                .await
                .map_err(|close| format!("could not close session: {close}"));
            let refresh = session_index
                .refresh(&session_id)
                .map(|_| ())
                .map_err(|refresh| format!("could not refresh session index: {refresh}"));
            let cleanup = combine_string_cleanup_results(shutdown, Ok(()), close, refresh);
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; additionally cleanup failed: {cleanup}"),
            });
        }
    };
    if let Err(error) = foreground_processes.bind(orchestration.foreground_process_port()) {
        let orchestration_shutdown = shutdown_orchestration(Some(orchestration.as_ref())).await;
        let mcp_shutdown = shutdown_mcp(mcp_runtime.as_ref()).await;
        let services_shutdown =
            combine_string_cleanup_results(orchestration_shutdown, mcp_shutdown, Ok(()), Ok(()));
        let close = session_handle
            .close()
            .await
            .map_err(|close| format!("could not close session: {close}"));
        let refresh = session_index
            .refresh(&session_id)
            .map(|_| ())
            .map_err(|refresh| format!("could not refresh session index: {refresh}"));
        let cleanup = combine_string_cleanup_results(services_shutdown, Ok(()), close, refresh);
        return Err(match cleanup {
            Ok(()) => error,
            Err(cleanup) => format!("{error}; additionally cleanup failed: {cleanup}"),
        });
    }
    if let Err(error) = register_canonical_session_builtins(
        &tools,
        &session_handle,
        plan_local,
        plan_file.clone(),
        skills.activation,
        media,
        Some(orchestration.goal_budget_port()),
    ) {
        let orchestration_shutdown = shutdown_orchestration(Some(orchestration.as_ref())).await;
        let mcp_shutdown = shutdown_mcp(mcp_runtime.as_ref()).await;
        let services_shutdown =
            combine_string_cleanup_results(orchestration_shutdown, mcp_shutdown, Ok(()), Ok(()));
        let close = session_handle
            .close()
            .await
            .map_err(|close| format!("could not close session: {close}"));
        let refresh = session_index
            .refresh(&session_id)
            .map(|_| ())
            .map_err(|refresh| format!("could not refresh session index: {refresh}"));
        let cleanup = combine_string_cleanup_results(services_shutdown, Ok(()), close, refresh);
        return Err(match cleanup {
            Ok(()) => error,
            Err(cleanup) => format!("{error}; additionally cleanup failed: {cleanup}"),
        });
    }
    let compaction = Arc::new(CompactionEngine::standard(
        Arc::clone(&provider),
        tools.clone(),
    ));
    let engine = match TurnEngine::new(
        Arc::clone(&provider),
        tools.clone(),
        hooks.clone(),
        ToolScheduler::new(),
        engine_config.clone(),
    )
    .map(Arc::new)
    {
        Ok(engine) => engine,
        Err(error) => {
            let orchestration_shutdown = shutdown_orchestration(Some(orchestration.as_ref())).await;
            let mcp_shutdown = shutdown_mcp(mcp_runtime.as_ref()).await;
            let services_shutdown = combine_string_cleanup_results(
                orchestration_shutdown,
                mcp_shutdown,
                Ok(()),
                Ok(()),
            );
            let close = session_handle
                .close()
                .await
                .map_err(|close| format!("could not close session: {close}"));
            let refresh = session_index
                .refresh(&session_id)
                .map(|_| ())
                .map_err(|refresh| format!("could not refresh session index: {refresh}"));
            let cleanup = combine_string_cleanup_results(services_shutdown, Ok(()), close, refresh);
            return Err(match cleanup {
                Ok(()) => error.to_string(),
                Err(cleanup) => {
                    format!("{error}; additionally cleanup failed: {cleanup}")
                }
            });
        }
    };
    let state = session_handle.snapshot().await.state;
    let plan_mode = state.plan_mode;
    let swarm_mode = state.swarm_mode;
    let warning = std::iter::once(session_handle.warning().map(str::to_owned))
        .chain(std::iter::once(tui_config_warning))
        .chain(std::iter::once(light_theme_warning))
        .chain(skills.warnings.into_iter().map(Some))
        .chain(system_prompt_warnings.into_iter().map(Some))
        .chain(plugins.warnings.iter().cloned().map(Some))
        .flatten()
        .reduce(|mut combined, warning| {
            combined.push_str("; ");
            combined.push_str(&warning);
            combined
        });
    let model_aliases = config.models.keys().cloned().collect::<Vec<_>>();
    let ecology = EcologyService::new(home.clone());

    Ok(PreparedInteractive {
        _runtime: runtime,
        home: home.clone(),
        working_dir,
        additional_dirs,
        user_home,
        editor_fallback: external_editor,
        initial_draft: None,
        session: session_handle,
        session_index,
        dialog_port,
        dialog_receiver: Mutex::new(Some(dialog_receiver)),
        engine,
        btw_provider: provider,
        btw_hooks: hooks,
        btw_engine_config: engine_config,
        compaction,
        system_prompt,
        provider: resolved.provider_id.clone(),
        context_window: resolved.max_context_tokens,
        model_alias: resolved.alias,
        model_aliases,
        thinking_effort: resolved.thinking_effort,
        effort_options: resolved.effort_options,
        allow_unknown_effort: resolved.allow_unknown_effort,
        max_completion_tokens: resolved.max_completion_tokens,
        plan_file,
        plan_mode,
        swarm_mode,
        recent_sessions,
        session_name,
        warning,
        tui_config,
        mcp: mcp_runtime,
        orchestration,
        orchestration_events,
        substrate: ecology.summary(Utc::now()),
        ecology,
        plugins,
    })
}

fn turn_engine_config(config: &MycelConfig, max_context_tokens: u64) -> TurnEngineConfig {
    let mut engine = TurnEngineConfig::default();
    if let Some(loop_control) = &config.loop_control {
        if let Some(max_steps) = loop_control.max_steps_per_turn {
            engine.max_steps = max_steps;
        }
        if let Some(max_retries) = loop_control.max_retries_per_step {
            engine.max_retries_per_step = max_retries;
        }
    }
    let loop_control = config.loop_control.as_ref();
    engine.auto_compaction = Some(AutoCompactionConfig {
        max_context_tokens,
        trigger_ratio: loop_control
            .and_then(|control| control.compaction_trigger_ratio)
            .unwrap_or(0.85),
        reserved_context_tokens: loop_control
            .and_then(|control| control.reserved_context_size)
            .unwrap_or(50_000),
    });
    engine
}

fn protocol_permission(permission: PermissionMode) -> ProtocolPermissionMode {
    match permission {
        PermissionMode::Manual => ProtocolPermissionMode::Manual,
        PermissionMode::Yolo => ProtocolPermissionMode::Yolo,
        PermissionMode::Auto => ProtocolPermissionMode::Auto,
    }
}

fn cli_permission(permission: ProtocolPermissionMode) -> PermissionMode {
    match permission {
        ProtocolPermissionMode::Manual => PermissionMode::Manual,
        ProtocolPermissionMode::Yolo => PermissionMode::Yolo,
        ProtocolPermissionMode::Auto => PermissionMode::Auto,
    }
}

enum InteractiveRuntimeMessage {
    Event(Box<AgentEvent>),
    EventLagged(u64),
    EventClosed,
    TurnFinished(Result<InteractiveTurnResult, String>),
    BtwEvent(Box<AgentEvent>),
    BtwEventLagged(u64),
    BtwFinished(Result<TurnOutcomeReason, String>),
}

struct InteractiveTurnResult {
    event_barrier_turn_id: Option<u64>,
    reason: TurnOutcomeReason,
    tokens: u64,
    advances_goal: bool,
    status: Option<String>,
    refresh_plugins: bool,
    hyphae: Option<HyphaeCompletion>,
}

#[derive(Clone)]
struct HyphaeCompletion {
    thinking_effort: Option<ThinkingEffort>,
    swarm_mode: String,
    submit_prompt: Option<String>,
}

struct ActiveTurn {
    cancellation: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    steerable: bool,
}

struct BtwPanelState {
    _runtime: Runtime,
    root: PathBuf,
    session: SessionHandle,
    engine: Arc<TurnEngine>,
    transcript: TranscriptReducer,
    active: Option<ActiveTurn>,
    event_pump: tokio::task::JoinHandle<()>,
}

#[derive(Clone, Debug)]
enum PluginStoreOperation {
    Install {
        source: PathBuf,
    },
    Remove {
        id: String,
    },
    SetEnabled {
        id: String,
        enabled: bool,
    },
    SetMcpEnabled {
        id: String,
        server: String,
        enabled: bool,
    },
}

impl PluginStoreOperation {
    fn display(&self) -> String {
        match self {
            Self::Install { source } => format!("/plugins install {}", source.display()),
            Self::Remove { id } => format!("/plugins remove {id}"),
            Self::SetEnabled { id, enabled } => format!(
                "/plugins {} {id}",
                if *enabled { "enable" } else { "disable" }
            ),
            Self::SetMcpEnabled {
                id,
                server,
                enabled,
            } => format!(
                "/plugins mcp {} {id} {server}",
                if *enabled { "enable" } else { "disable" }
            ),
        }
    }

    fn approval_subject(&self) -> String {
        match self {
            Self::Install { .. } => "install".to_owned(),
            Self::Remove { .. } => "remove".to_owned(),
            Self::SetEnabled { .. } => "toggle".to_owned(),
            Self::SetMcpEnabled { .. } => "mcp-toggle".to_owned(),
        }
    }

    fn requires_approval(&self) -> bool {
        matches!(self, Self::Install { .. } | Self::Remove { .. })
    }

    fn run(self, home: &Path) -> Result<String, String> {
        match self {
            Self::Install { source } => {
                let installed = install_local_plugin(home, &source, Utc::now())?;
                Ok(format!(
                    "{} local plugin {} {}. Start a new session to apply plugin changes.",
                    if installed.replaced {
                        "updated"
                    } else {
                        "installed"
                    },
                    installed.id,
                    installed.version
                ))
            }
            Self::Remove { id } => {
                remove_installed_plugin(home, &id)?;
                Ok(format!(
                    "removed local plugin {id}. Start a new session to apply plugin changes."
                ))
            }
            Self::SetEnabled { id, enabled } => {
                set_installed_plugin_enabled(home, &id, enabled, Utc::now())?;
                Ok(format!(
                    "{} local plugin {id}. Start a new session to apply plugin changes.",
                    if enabled { "enabled" } else { "disabled" }
                ))
            }
            Self::SetMcpEnabled {
                id,
                server,
                enabled,
            } => {
                set_installed_plugin_mcp_enabled(home, &id, &server, enabled, Utc::now())?;
                Ok(format!(
                    "{} MCP server {server} for {id}. Start a new session to apply plugin changes.",
                    if enabled { "enabled" } else { "disabled" }
                ))
            }
        }
    }
}

struct InteractiveLoopState {
    reducer: SessionReducer,
    transcript: TranscriptReducer,
    decoder: InputDecoder,
    renderer: DifferentialRenderer,
    size: TerminalSize,
    active: Option<ActiveTurn>,
    btw: Option<BtwPanelState>,
    exit_after_turn: bool,
    dialogs: DialogHost,
    messages: mpsc::Receiver<InteractiveRuntimeMessage>,
    message_sender: mpsc::Sender<InteractiveRuntimeMessage>,
    turn_finished_sender: tokio::sync::mpsc::UnboundedSender<Result<InteractiveTurnResult, String>>,
    event_pump: tokio::task::JoinHandle<()>,
    system_queue: VecDeque<(String, String)>,
    thinking_effort: Option<ThinkingEffort>,
    tui_config: TuiConfig,
    /// Theme and truecolor support resolved once at construction and
    /// re-resolved by `refresh_render_caches` when the TUI config changes;
    /// the ~40Hz render loop must not re-read config or the environment.
    theme: Theme,
    truecolor: bool,
    /// The header card rendered at a given width; the card's data changes only
    /// on substrate refreshes, so this invalidates on resize, theme change,
    /// or `refresh_substrate`.
    header_cache: Option<(usize, Vec<String>)>,
    header: HeaderData,
    /// Live substrate snapshot for the header and rails. Refreshed by
    /// `refresh_substrate` on ecology-mutating events only.
    substrate: SubstrateStatus,
    /// Bounded ring of observed gate decisions feeding the inspector; fed from
    /// the main session's event stream (BTW side-channel events stay out).
    gate_log: GateLog,
    /// Substrate record behind the most recent denial, resolved at deny time
    /// from the reason's `(source: antibody:<id>)` pointer. `None` when the
    /// deny came from the protected-path floor or the pointer did not resolve.
    last_deny_antibody: Option<AntibodyDetail>,
    swarm_mode: bool,
    hyphae_task_active: bool,
    pasted_images: PastedImageStore,
    plugin_view: PluginComposition,
    session_transition: Option<InteractiveSessionTransition>,
    terminal_sequences: VecDeque<Vec<u8>>,
    last_view: Vec<String>,
    last_cursor: Option<(usize, usize)>,
    /// Braille spinner frame index for running tool rows; advanced from
    /// `now_ms` on every loop tick so it is deterministic per wall-clock time.
    spinner_phase: usize,
}

impl InteractiveLoopState {
    fn new(
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        size: TerminalSize,
    ) -> Self {
        let (sender, messages) = mpsc::channel();
        let (turn_finished_sender, mut turn_finished_receiver) =
            tokio::sync::mpsc::unbounded_channel::<Result<InteractiveTurnResult, String>>();
        let mut receiver = prepared.session.subscribe();
        let event_sender = sender.clone();
        let event_pump = executor.spawn(async move {
            let mut last_ended_turn = None;
            let mut pending_completion = None;
            loop {
                tokio::select! {
                    // Provider turns publish their completion on a task-local channel while
                    // transcript events arrive over the session broadcast channel. The channels
                    // have no shared ordering guarantee, so hold completion until its durable
                    // `turn.ended` event has been forwarded to the renderer.
                    biased;
                    event = receiver.recv() => {
                        let (message, ended_turn) = match event {
                            Ok(event) => {
                                let ended_turn = match &event.event {
                                    AgentEvent::TurnEnded { turn_id, .. } => Some(*turn_id),
                                    _ => None,
                                };
                                (
                                    InteractiveRuntimeMessage::Event(Box::new(event.event)),
                                    ended_turn,
                                )
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                                (InteractiveRuntimeMessage::EventLagged(count), None)
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                let _ = event_sender.send(InteractiveRuntimeMessage::EventClosed);
                                break;
                            }
                        };
                        if event_sender.send(message).is_err() {
                            break;
                        }
                        if let Some(turn_id) = ended_turn {
                            last_ended_turn = Some(turn_id);
                            let barrier_satisfied = pending_completion
                                .as_ref()
                                .and_then(|result: &Result<InteractiveTurnResult, String>| {
                                    result.as_ref().ok()
                                })
                                .and_then(|result| result.event_barrier_turn_id)
                                == Some(turn_id);
                            if barrier_satisfied
                                && event_sender
                                    .send(InteractiveRuntimeMessage::TurnFinished(
                                        pending_completion.take().expect(
                                            "a satisfied interactive turn barrier has a completion",
                                        ),
                                    ))
                                    .is_err()
                            {
                                break;
                            }
                        }
                    }
                    result = turn_finished_receiver.recv() => {
                        let Some(result) = result else {
                            break;
                        };
                        let barrier = result
                            .as_ref()
                            .ok()
                            .and_then(|result| result.event_barrier_turn_id);
                        if barrier.is_none() || barrier == last_ended_turn {
                            if event_sender
                                .send(InteractiveRuntimeMessage::TurnFinished(result))
                                .is_err()
                            {
                                break;
                            }
                        } else {
                            debug_assert!(pending_completion.is_none());
                            pending_completion = Some(result);
                        }
                    }
                }
            }
        });
        let transcript = seed_transcript(
            format!(
                "session {} · model {}",
                prepared.session.id(),
                prepared.model_alias
            ),
            prepared.warning.as_deref(),
        );
        let dialog_receiver = prepared
            .dialog_receiver
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .expect("interactive dialog receiver can only be attached once");
        let mut reducer = SessionReducer {
            plan: prepared.plan_mode,
            ..SessionReducer::default()
        };
        if let Some(draft) = &prepared.initial_draft {
            reducer.editor.replace_without_undo(draft.clone());
        }
        Self {
            reducer,
            transcript,
            decoder: InputDecoder::default(),
            renderer: DifferentialRenderer::default(),
            size,
            active: None,
            btw: None,
            exit_after_turn: false,
            dialogs: DialogHost::new(Arc::clone(&prepared.dialog_port), dialog_receiver),
            messages,
            message_sender: sender,
            turn_finished_sender,
            event_pump,
            system_queue: VecDeque::new(),
            thinking_effort: prepared.thinking_effort.clone(),
            theme: active_theme(&prepared.tui_config.theme),
            truecolor: truecolor_enabled(),
            header_cache: None,
            tui_config: prepared.tui_config.clone(),
            header: build_header(prepared),
            substrate: prepared.substrate,
            gate_log: GateLog::default(),
            last_deny_antibody: None,
            swarm_mode: prepared.swarm_mode,
            hyphae_task_active: false,
            pasted_images: PastedImageStore::default(),
            plugin_view: prepared.plugins.clone(),
            session_transition: None,
            terminal_sequences: VecDeque::new(),
            last_view: Vec::new(),
            last_cursor: None,
            spinner_phase: 0,
        }
    }

    /// Wall-clock unix epoch milliseconds. Transcript frames stamp this so
    /// the gutter can render local `HH:MM:SS`; the reducer only ever compares
    /// differences, so the epoch base does not change coalescing.
    fn now_ms(&self) -> u64 {
        epoch_now_ms()
    }

    /// Re-resolve the cached theme and truecolor support from the current TUI
    /// config and drop the cached header render. Every path that replaces
    /// `tui_config` with a possibly different theme (`/theme`, `/reload-tui`)
    /// must call this, or the view keeps rendering the stale theme forever.
    fn refresh_render_caches(&mut self) {
        self.theme = active_theme(&self.tui_config.theme);
        self.truecolor = truecolor_enabled();
        self.header_cache = None;
    }

    // Consumed by the body-band composition in this PR's final task; the
    // allow goes with it.
    #[allow(dead_code)]
    /// Snapshot the inspector's data: the decision ring, the resolved
    /// last-deny antibody, and the cached candidate count. Pure reads only.
    fn inspector_data(&self) -> InspectorData {
        InspectorData {
            activity: self.gate_log.decisions().cloned().collect(),
            antibody: self.last_deny_antibody.clone(),
            candidates_pending: self.substrate.candidates_pending,
        }
    }

    // Consumed by the body-band composition in this PR's final task; the
    // allow goes with it.
    #[allow(dead_code)]
    /// Snapshot the session rail's data from state the loop already holds.
    /// Pure reads only: the substrate snapshot is the event-driven cache, and
    /// the hyphae stats come from the transcript's Subagent frames — the only
    /// hyphae state reachable without an async orchestration read (`/hyphae`
    /// goes through a host-tool turn, `handle_hyphae_command`).
    fn rail_data(&self, prepared: &PreparedInteractive) -> RailData {
        let now = self.now_ms();
        let mut hyphae_active = 0usize;
        let mut hyphae_last = None;
        for frame in self.transcript.frames() {
            if frame.kind != FrameKind::Subagent {
                continue;
            }
            if frame.streaming {
                hyphae_active += 1;
            }
            let name = frame.text.lines().next().unwrap_or_default();
            let state = frame.state.as_deref().unwrap_or("unknown");
            hyphae_last = Some(format!(
                "{name} · {state} · {}",
                format_age(now.saturating_sub(frame.at_ms))
            ));
        }
        RailData {
            name: prepared.session_name.clone(),
            model: self.header.model.clone(),
            provider: self.header.provider.clone(),
            cwd: self.header.cwd.clone(),
            shell_mode: self.reducer.input_mode == crate::tui::InputMode::Shell,
            plan: self.reducer.plan,
            // See `build_header`: occupancy is not carried by the event
            // stream, so the rail renders the window alone.
            ctx_used: None,
            ctx_window: self.header.ctx_window,
            substrate: self.substrate,
            hyphae_active,
            hyphae_last,
        }
    }

    /// Re-read the substrate summary and invalidate the header render.
    /// Event-driven only: called after `/promote`, `/deny`, and projected gate
    /// denials — never on the render tick, which must not gain I/O.
    fn refresh_substrate(&mut self, prepared: &PreparedInteractive) {
        self.substrate = prepared.ecology.summary(Utc::now());
        self.header.substrate = substrate_summary_display(&self.substrate);
        self.header_cache = None;
    }

    fn open_btw(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        prompt: String,
    ) {
        if prompt.trim().is_empty() {
            self.status("usage: /btw <question>");
            return;
        }
        self.close_btw(executor, None);
        let root = prepared
            .home
            .join("run")
            .join("btw")
            .join(uuid::Uuid::new_v4().to_string());
        let runtime = Runtime::new(root.clone());
        let id = SessionId::generate();
        let mut options = SessionOptions::new(id);
        options.initial_permission_mode = ProtocolPermissionMode::Auto;
        options.hooks = prepared.btw_hooks.clone();
        let parent_history = executor
            .block_on(prepared.session.snapshot())
            .state
            .context
            .history()
            .to_vec();
        let session = match executor.block_on(runtime.create_session(options)) {
            Ok(session) => session,
            Err(error) => {
                self.status(format!("Failed to start /btw: {error}"));
                return;
            }
        };
        let initialization = executor.block_on(async {
            for entry in projected_side_context(&parent_history) {
                session
                    .append_context(entry)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            session
                .append_context(ContextEntry {
                    message: Message {
                        role: Role::System,
                        name: None,
                        content: vec![ContentPart::text(SIDE_QUESTION_SYSTEM_REMINDER)],
                        tool_calls: Vec::new(),
                        tool_call_id: None,
                        partial: false,
                        tools: Vec::new(),
                    },
                    origin: Some(PromptOrigin::SystemTrigger {
                        name: "btw".to_owned(),
                    }),
                    is_error: false,
                    tool_call_displays: BTreeMap::new(),
                    note: None,
                })
                .await
                .map_err(|error| error.to_string())
        });
        if let Err(error) = initialization {
            let _ = executor.block_on(session.close());
            let _ = fs::remove_dir_all(&root);
            self.status(format!("Failed to start /btw: {error}"));
            return;
        }
        let engine = match TurnEngine::new(
            Arc::clone(&prepared.btw_provider),
            ToolRegistry::new(),
            prepared.btw_hooks.clone(),
            ToolScheduler::new(),
            prepared.btw_engine_config.clone(),
        ) {
            Ok(engine) => Arc::new(engine),
            Err(error) => {
                let _ = executor.block_on(session.close());
                let _ = fs::remove_dir_all(&root);
                self.status(format!("Failed to start /btw: {error}"));
                return;
            }
        };
        let mut receiver = session.subscribe();
        let sender = self.message_sender.clone();
        let event_pump = executor.spawn(async move {
            loop {
                match receiver.recv().await {
                    Ok(event) => {
                        if sender
                            .send(InteractiveRuntimeMessage::BtwEvent(Box::new(event.event)))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                        if sender
                            .send(InteractiveRuntimeMessage::BtwEventLagged(count))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        let mut transcript = TranscriptReducer::default();
        transcript.push(
            TranscriptEvent::Status("BTW · side channel · tools disabled".to_owned()),
            self.now_ms(),
        );
        self.btw = Some(BtwPanelState {
            _runtime: runtime,
            root,
            session,
            engine,
            transcript,
            active: None,
            event_pump,
        });
        self.start_btw_turn(executor, prepared, prompt);
    }

    fn start_btw_turn(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        prompt: String,
    ) {
        let now = self.now_ms();
        let content = self.pasted_images.expand(&prompt);
        let Some(panel) = self.btw.as_mut() else {
            return;
        };
        if panel.active.is_some() {
            self.reducer.editor.replace_without_undo(prompt);
            panel.transcript.push(
                TranscriptEvent::Status(
                    "Wait for /btw to finish before sending another question.".to_owned(),
                ),
                now,
            );
            return;
        }
        panel
            .transcript
            .push(TranscriptEvent::UserMessage(prompt.clone()), now);
        let session = panel.session.clone();
        let engine = Arc::clone(&panel.engine);
        let cancellation = CancellationToken::new();
        let turn_cancellation = cancellation.clone();
        let sender = self.message_sender.clone();
        let system_prompt = prepared.system_prompt.to_string();
        let thinking_effort = self.thinking_effort.clone();
        let max_completion_tokens = prepared.max_completion_tokens;
        let task = executor.spawn(async move {
            let mut input = TurnInput::user(prompt, system_prompt);
            input.content = content;
            input.origin = PromptOrigin::User;
            input.thinking_effort = thinking_effort;
            input.max_completion_tokens = max_completion_tokens;
            let result = engine
                .run_turn(&session, input, turn_cancellation)
                .await
                .map(|outcome| outcome.reason)
                .map_err(|error| error.to_string());
            let _ = sender.send(InteractiveRuntimeMessage::BtwFinished(result));
        });
        panel.active = Some(ActiveTurn {
            cancellation,
            task,
            steerable: false,
        });
    }

    fn close_btw(&mut self, executor: &tokio::runtime::Runtime, notice: Option<&str>) -> bool {
        let Some(mut panel) = self.btw.take() else {
            return false;
        };
        let cleanup = executor.block_on(async {
            if let Some(mut active) = panel.active.take() {
                active.cancellation.cancel();
                if tokio::time::timeout(Duration::from_secs(2), &mut active.task)
                    .await
                    .is_err()
                {
                    active.task.abort();
                    let _ = active.task.await;
                }
            }
            panel.event_pump.abort();
            let _ = panel.event_pump.await;
            panel.session.cancel();
            panel
                .session
                .close()
                .await
                .map_err(|error| error.to_string())
        });
        let removal = match fs::remove_dir_all(&panel.root) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
        if removal.is_ok() {
            if let Some(btw_root) = panel.root.parent() {
                let _ = fs::remove_dir(btw_root);
                if let Some(run_root) = btw_root.parent() {
                    let _ = fs::remove_dir(run_root);
                }
            }
        }
        match (cleanup, removal) {
            (Ok(()), Ok(())) => {
                if let Some(notice) = notice {
                    self.status(notice);
                }
            }
            (Ok(()), Err(error)) => {
                self.status(format!("BTW closed, but temporary cleanup failed: {error}"));
            }
            (Err(error), _) => self.status(format!("Failed to close /btw cleanly: {error}")),
        }
        true
    }

    fn cancel_or_close_btw(&mut self, executor: &tokio::runtime::Runtime) -> bool {
        let now = self.now_ms();
        if let Some(panel) = self.btw.as_mut() {
            if let Some(active) = &panel.active {
                active.cancellation.cancel();
                panel
                    .transcript
                    .push(TranscriptEvent::Status("Cancelling /btw…".to_owned()), now);
                return true;
            }
        }
        self.close_btw(executor, Some("BTW closed."))
    }

    fn request_external_editor(&mut self, prepared: &PreparedInteractive) -> bool {
        if self.active.is_some()
            || self
                .btw
                .as_ref()
                .is_some_and(|panel| panel.active.is_some())
        {
            self.status("Wait for active turns to finish before opening the external editor.");
            return false;
        }
        let command = self
            .tui_config
            .editor_command
            .clone()
            .or_else(|| prepared.editor_fallback.clone());
        let Some(command) = command.and_then(|command| nonempty(&command).map(str::to_owned))
        else {
            self.status("No editor configured. Set $VISUAL / $EDITOR, or run /editor <command>.");
            return false;
        };
        if command.len() > 4096 || command.chars().any(char::is_control) {
            self.status("External editor command is invalid; use /editor to replace it.");
            return false;
        }
        self.session_transition = Some(InteractiveSessionTransition::ExternalEditor {
            command,
            draft: self.reducer.editor.text().to_owned(),
        });
        true
    }

    fn start_turn(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        prompt: String,
    ) {
        let content = self.pasted_images.expand(&prompt);
        self.start_turn_with_content(
            executor,
            prepared,
            Some(prompt.clone()),
            content,
            PromptOrigin::User,
        );
    }

    fn start_turn_with_origin(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        display: Option<String>,
        prompt: String,
        origin: PromptOrigin,
    ) {
        self.start_turn_with_content(
            executor,
            prepared,
            display,
            vec![ContentPart::text(prompt)],
            origin,
        );
    }

    fn start_turn_with_content(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        display: Option<String>,
        content: Vec<ContentPart>,
        origin: PromptOrigin,
    ) {
        if let Some(display) = display {
            let now = self.now_ms();
            self.transcript
                .push(TranscriptEvent::UserMessage(display), now);
        }
        self.reducer.phase = SessionPhase::Busy;
        let system_prompt = if self.swarm_mode {
            format!(
                "{}\n\n# Swarm mode\n\nSwarm mode is active for this session. For work that can be split safely, use the bounded `AgentSwarm` tool and synthesize the workers' results. Do not fan out trivial or tightly coupled work.",
                prepared.system_prompt
            )
        } else {
            prepared.system_prompt.to_string()
        };
        let mut input = TurnInput::user("", system_prompt);
        input.content = content;
        input.origin = origin;
        input.thinking_effort = self.thinking_effort.clone();
        input.max_completion_tokens = prepared.max_completion_tokens;
        let engine = Arc::clone(&prepared.engine);
        let session = prepared.session.clone();
        let cancellation = CancellationToken::new();
        let turn_cancellation = cancellation.clone();
        let turn_finished_sender = self.turn_finished_sender.clone();
        let task = executor.spawn(async move {
            let result = engine
                .run_turn(&session, input, turn_cancellation)
                .await
                .map(|outcome| InteractiveTurnResult {
                    event_barrier_turn_id: Some(outcome.turn_id),
                    reason: outcome.reason,
                    tokens: outcome.usage.grand_total(),
                    advances_goal: true,
                    status: None,
                    refresh_plugins: false,
                    hyphae: None,
                })
                .map_err(|error| error.to_string());
            let _ = turn_finished_sender.send(result);
        });
        self.active = Some(ActiveTurn {
            cancellation,
            task,
            steerable: true,
        });
    }

    fn start_delegate(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        prompt: String,
        task_description: String,
    ) {
        let invocation = match prepared
            .orchestration
            .native_delegate_invocation(task_description)
        {
            Ok(invocation) => invocation,
            Err(error) => {
                self.status(format!("could not prepare native delegation: {error}"));
                return;
            }
        };
        self.start_host_tool(
            executor,
            prepared,
            prompt,
            invocation.tool.definition().name,
            invocation.arguments,
            SessionPhase::Busy,
        );
    }

    fn start_shell(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        command: String,
    ) {
        self.start_host_tool(
            executor,
            prepared,
            format!("!{command}"),
            "Bash".to_owned(),
            serde_json::json!({ "command": command }),
            SessionPhase::Shell,
        );
    }

    fn start_plugin_command(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        prompt: String,
        command: String,
        arguments: String,
    ) {
        self.start_host_tool(
            executor,
            prepared,
            prompt,
            "PluginCommand".to_owned(),
            serde_json::json!({
                "command": command,
                "arguments": arguments,
            }),
            SessionPhase::Busy,
        );
    }

    fn start_plugin_store_operation(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        operation: PluginStoreOperation,
    ) {
        let display = operation.display();
        let now = self.now_ms();
        self.transcript
            .push(TranscriptEvent::UserMessage(display.clone()), now);
        self.reducer.phase = SessionPhase::Busy;
        let cancellation = CancellationToken::new();
        let turn_cancellation = cancellation.clone();
        let session = prepared.session.clone();
        let home = prepared.home.clone();
        let turn_finished_sender = self.turn_finished_sender.clone();
        let requires_approval = operation.requires_approval();
        let approval_subject = operation.approval_subject();
        let turn_id = executor.block_on(session.snapshot()).state.turn_sequence;
        let task = executor.spawn(async move {
            let result = async {
                if requires_approval {
                    let tool_call_id = ToolCallId::new(RequestId::generate().into_string())
                        .map_err(|error| format!("could not allocate plugin approval: {error}"))?;
                    let authorization = session
                        .authorize_tool(&ToolPermissionRequest {
                            turn_id,
                            tool_call_id,
                            tool_name: "PluginManage".to_owned(),
                            action: "Modify explicitly installed local plugins".to_owned(),
                            display: ToolInputDisplay::Command {
                                command: display,
                                cwd: None,
                                description: Some(
                                    "local plugin code can contribute commands, skills, and MCP servers"
                                        .to_owned(),
                                ),
                                language: None,
                            },
                            approval_rule: Some("PluginManage".to_owned()),
                            rule_subject: Some(approval_subject),
                            exclusive_tool: None,
                            plan_policy: PlanPolicy::NotInPlan,
                            create_goal_review: false,
                            sensitive_file: false,
                            git_control: false,
                            git_cwd_write: false,
                        })
                        .await
                        .map_err(|error| format!("plugin change approval failed: {error}"))?;
                    if authorization.verdict != PermissionVerdict::Allow {
                        return Err(authorization
                            .reason
                            .unwrap_or_else(|| "plugin change was rejected".to_owned()));
                    }
                }
                if turn_cancellation.is_cancelled() {
                    return Err("plugin change was cancelled".to_owned());
                }
                tokio::task::spawn_blocking(move || operation.run(&home))
                    .await
                    .map_err(|error| format!("plugin change task failed: {error}"))?
            }
            .await
            .map(|status| InteractiveTurnResult {
                event_barrier_turn_id: None,
                reason: TurnOutcomeReason::Completed,
                tokens: 0,
                advances_goal: false,
                status: Some(status),
                refresh_plugins: true,
                hyphae: None,
            });
            let _ = turn_finished_sender.send(result);
        });
        self.active = Some(ActiveTurn {
            cancellation,
            task,
            steerable: false,
        });
    }

    fn start_host_tool(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        prompt: String,
        tool_name: String,
        arguments: Value,
        phase: SessionPhase,
    ) {
        let now = self.now_ms();
        self.transcript
            .push(TranscriptEvent::UserMessage(prompt.clone()), now);
        self.reducer.phase = phase;
        let engine = Arc::clone(&prepared.engine);
        let session = prepared.session.clone();
        let cancellation = CancellationToken::new();
        let turn_cancellation = cancellation.clone();
        let turn_finished_sender = self.turn_finished_sender.clone();
        let task = executor.spawn(async move {
            let result = match engine
                .invoke_host_tool(&session, prompt, tool_name, arguments, turn_cancellation)
                .await
            {
                Ok(result) if result.is_error => {
                    let output = match result.output {
                        mycel_agent_protocol::ExecutableToolOutput::Text(text) => text,
                        mycel_agent_protocol::ExecutableToolOutput::Parts(parts) => {
                            serde_json::to_string(&parts)
                                .unwrap_or_else(|_| "native delegation failed".to_owned())
                        }
                    };
                    Err(result.message.or(result.note).unwrap_or(output))
                }
                Ok(_) => Ok(InteractiveTurnResult {
                    event_barrier_turn_id: None,
                    reason: TurnOutcomeReason::Completed,
                    tokens: 0,
                    advances_goal: false,
                    status: None,
                    refresh_plugins: false,
                    hyphae: None,
                }),
                Err(error) => Err(error.to_string()),
            };
            let _ = turn_finished_sender.send(result);
        });
        self.active = Some(ActiveTurn {
            cancellation,
            task,
            steerable: false,
        });
    }

    fn start_task_command(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        prompt: String,
        tool_name: &'static str,
        arguments: Value,
    ) {
        let now = self.now_ms();
        self.transcript
            .push(TranscriptEvent::UserMessage(prompt.clone()), now);
        self.reducer.phase = SessionPhase::Busy;
        let engine = Arc::clone(&prepared.engine);
        let session = prepared.session.clone();
        let cancellation = CancellationToken::new();
        let turn_cancellation = cancellation.clone();
        let turn_finished_sender = self.turn_finished_sender.clone();
        let task = executor.spawn(async move {
            let result = match engine
                .invoke_host_tool(
                    &session,
                    prompt,
                    tool_name.to_owned(),
                    arguments,
                    turn_cancellation,
                )
                .await
            {
                Ok(result) if result.is_error => {
                    let output = match result.output {
                        mycel_agent_protocol::ExecutableToolOutput::Text(text) => text,
                        mycel_agent_protocol::ExecutableToolOutput::Parts(parts) => {
                            serde_json::to_string(&parts)
                                .unwrap_or_else(|_| "task operation failed".to_owned())
                        }
                    };
                    Err(result.message.or(result.note).unwrap_or(output))
                }
                Ok(result) => render_task_tool_output(tool_name, result.output).map(|status| {
                    InteractiveTurnResult {
                        event_barrier_turn_id: None,
                        reason: TurnOutcomeReason::Completed,
                        tokens: 0,
                        advances_goal: false,
                        status: Some(status),
                        refresh_plugins: false,
                        hyphae: None,
                    }
                }),
                Err(error) => Err(error.to_string()),
            };
            let _ = turn_finished_sender.send(result);
        });
        self.active = Some(ActiveTurn {
            cancellation,
            task,
            steerable: false,
        });
    }

    fn process_actions(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
    ) -> bool {
        let actions = std::mem::take(&mut self.reducer.actions);
        let mut exit = false;
        for action in actions {
            match action {
                LogicalAction::Submit(input) => match input.mode {
                    SubmissionMode::Prompt => {
                        let handled_command = input.text.starts_with('/')
                            && self.handle_session_command(executor, prepared, &input.text);
                        if handled_command {
                            if self.active.is_none() {
                                self.reducer.phase = SessionPhase::Idle;
                            }
                            if self.session_transition.is_some() || self.exit_after_turn {
                                exit = true;
                            }
                        } else if self.btw.is_some() {
                            self.start_btw_turn(executor, prepared, input.text);
                        } else if self.handle_session_command(executor, prepared, &input.text) {
                            if self.active.is_none() {
                                self.reducer.phase = SessionPhase::Idle;
                            }
                        } else {
                            self.start_turn(executor, prepared, input.text);
                        }
                    }
                    SubmissionMode::Shell => {
                        self.start_shell(executor, prepared, input.text);
                    }
                },
                LogicalAction::Cancel => {
                    if let Some(active) = &self.active {
                        active.cancellation.cancel();
                        self.reducer.phase = SessionPhase::Busy;
                        self.status("cancelling current turn");
                    }
                }
                LogicalAction::ExitArmed => exit = true,
                LogicalAction::TogglePlan(enabled) => {
                    let transition = if enabled {
                        executor.block_on(prepared.session.enter_plan_mode(Some(
                            prepared.plan_file.to_string_lossy().into_owned(),
                        )))
                    } else {
                        executor.block_on(prepared.session.exit_plan_mode())
                    };
                    match transition {
                        Ok(()) => self.status(if enabled {
                            format!("plan mode enabled · {}", prepared.plan_file.display())
                        } else {
                            "plan mode disabled".to_owned()
                        }),
                        Err(error) => {
                            self.reducer.plan = !enabled;
                            self.status(format!(
                                "could not {} plan mode: {error}",
                                if enabled { "enable" } else { "disable" }
                            ));
                        }
                    }
                }
                LogicalAction::PasteMedia => self.paste_clipboard_image(),
                LogicalAction::Steer(messages) => {
                    if !self.active.as_ref().is_some_and(|active| active.steerable) {
                        self.reducer.queue.extend(messages.into_iter().map(|text| {
                            crate::tui::QueuedInput {
                                text,
                                mode: SubmissionMode::Prompt,
                            }
                        }));
                        self.status("current operation cannot be steered; input remains queued");
                        continue;
                    }
                    let total = messages.len();
                    let mut messages = messages.into_iter();
                    let mut steered = 0usize;
                    while let Some(message) = messages.next() {
                        let content = self.pasted_images.expand(&message);
                        match executor.block_on(prepared.session.steer(content, PromptOrigin::User))
                        {
                            Ok(()) => steered = steered.saturating_add(1),
                            Err(error) => {
                                self.reducer.queue.push(crate::tui::QueuedInput {
                                    text: message,
                                    mode: SubmissionMode::Prompt,
                                });
                                self.reducer.queue.extend(messages.map(|text| {
                                    crate::tui::QueuedInput {
                                        text,
                                        mode: SubmissionMode::Prompt,
                                    }
                                }));
                                self.status(format!(
                                    "could not steer current turn; input remains queued: {error}"
                                ));
                                break;
                            }
                        }
                    }
                    if steered == total {
                        self.status(format!(
                            "steered {steered} message{} into the current turn",
                            if steered == 1 { "" } else { "s" }
                        ));
                    }
                }
                LogicalAction::Detach => {
                    match prepared.orchestration.detach_foreground_tasks(false) {
                        Ok(tasks) if tasks.is_empty() => {
                            self.status("No foreground task running.");
                        }
                        Ok(tasks) => self.status(format!(
                            "Moved {} task{} to background. /tasks to view.",
                            tasks.len(),
                            if tasks.len() == 1 { "" } else { "s" }
                        )),
                        Err(error) => {
                            self.status(format!("Failed to move task to background: {error}"));
                        }
                    }
                }
                LogicalAction::Queue(input)
                    if input.mode == SubmissionMode::Prompt
                        && (self.btw.is_some()
                            || slash_arguments(&input.text, "/btw").is_some()) =>
                {
                    if self.reducer.queue.last() == Some(&input) {
                        self.reducer.queue.pop();
                    } else if let Some(index) = self
                        .reducer
                        .queue
                        .iter()
                        .rposition(|queued| queued == &input)
                    {
                        self.reducer.queue.remove(index);
                    }
                    if let Some(arguments) = slash_arguments(&input.text, "/btw") {
                        self.open_btw(executor, prepared, arguments.to_owned());
                    } else {
                        self.start_btw_turn(executor, prepared, input.text);
                    }
                }
                LogicalAction::Queue(_) | LogicalAction::Newline | LogicalAction::Clear => {}
            }
        }
        exit
    }

    fn handle_session_command(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        input: &str,
    ) -> bool {
        let input = input.trim();
        for command in ["/new", "/clear"] {
            if let Some(arguments) = slash_arguments(input, command) {
                if arguments.is_empty() {
                    self.session_transition = Some(InteractiveSessionTransition::New);
                } else {
                    self.status(format!("usage: {command}"));
                }
                return true;
            }
        }
        if let Some(arguments) = slash_arguments(input, "/reload") {
            if arguments.is_empty() {
                self.session_transition = Some(InteractiveSessionTransition::Reload);
            } else {
                self.status("usage: /reload");
            }
            return true;
        }
        if let Some(arguments) = slash_arguments(input, "/btw") {
            self.open_btw(executor, prepared, arguments.to_owned());
            return true;
        }
        if let Some(arguments) = slash_arguments(input, "/reload-tui") {
            if !arguments.is_empty() {
                self.status("usage: /reload-tui");
                return true;
            }
            let (config, warning) = load_tui_config(&prepared.home);
            self.tui_config = config;
            self.refresh_render_caches();
            self.renderer.reset();
            self.last_view.clear();
            self.status(warning.unwrap_or_else(|| {
                format!(
                    "TUI settings reloaded · theme {}",
                    self.tui_config.theme.as_str()
                )
            }));
            return true;
        }
        for command in ["/settings", "/config"] {
            let Some(arguments) = slash_arguments(input, command) else {
                continue;
            };
            if arguments.is_empty() {
                self.status(format_tui_settings(&self.tui_config));
            } else {
                self.status(format!(
                    "usage: {command}\nchange values with /theme and /editor, or edit {}",
                    prepared.home.join("tui.toml").display()
                ));
            }
            return true;
        }
        if let Some(arguments) = slash_arguments(input, "/theme") {
            if arguments.is_empty() {
                self.status(format!("theme: {}", self.tui_config.theme.as_str()));
                return true;
            }
            let theme = match ThemeName::parse(arguments) {
                Ok(theme) => theme,
                Err(error) => {
                    self.status(format!("invalid theme: {error}"));
                    return true;
                }
            };
            let mut config = self.tui_config.clone();
            config.theme = theme;
            match save_tui_config(&prepared.home, &config) {
                Ok(path) => {
                    self.tui_config = config;
                    self.refresh_render_caches();
                    self.renderer.reset();
                    self.last_view.clear();
                    self.status(format!(
                        "theme set to {} · {}",
                        self.tui_config.theme.as_str(),
                        path.display()
                    ));
                }
                Err(error) => self.status(format!("could not save theme: {error}")),
            }
            return true;
        }
        if let Some(arguments) = slash_arguments(input, "/editor") {
            if arguments.is_empty() {
                self.status(format!(
                    "editor: {}",
                    self.tui_config.editor_command.as_deref().unwrap_or("auto")
                ));
                return true;
            }
            let command = if arguments.eq_ignore_ascii_case("auto") {
                None
            } else if arguments.len() > 4096 || arguments.chars().any(char::is_control) {
                self.status("editor command must be at most 4096 bytes with no controls");
                return true;
            } else {
                Some(arguments.to_owned())
            };
            let mut config = self.tui_config.clone();
            config.editor_command = command;
            match save_tui_config(&prepared.home, &config) {
                Ok(path) => {
                    self.tui_config = config;
                    self.status(format!(
                        "editor set to {} · {}",
                        self.tui_config.editor_command.as_deref().unwrap_or("auto"),
                        path.display()
                    ));
                }
                Err(error) => self.status(format!("could not save editor setting: {error}")),
            }
            return true;
        }
        for command in ["/experiments", "/experimental"] {
            let Some(arguments) = slash_arguments(input, command) else {
                continue;
            };
            if arguments.is_empty() {
                self.status(
                    "No runtime experiments are registered. Provider-specific preview features are configured explicitly in config.toml.",
                );
            } else {
                self.status(format!("usage: {command}"));
            }
            return true;
        }
        if let Some(arguments) = slash_arguments(input, "/fork") {
            if arguments.is_empty() {
                self.session_transition = Some(InteractiveSessionTransition::Fork);
            } else {
                self.status("usage: /fork");
            }
            return true;
        }
        for command in ["/sessions", "/resume"] {
            let Some(arguments) = slash_arguments(input, command) else {
                continue;
            };
            if arguments.is_empty() || arguments == "all" {
                let scope = if arguments == "all" {
                    None
                } else {
                    Some(prepared.working_dir.as_path())
                };
                match prepared.session_index.list(scope) {
                    Ok(discovery) if discovery.sessions.is_empty() => {
                        self.status("No sessions found to resume.");
                    }
                    Ok(discovery) => {
                        let mut lines = discovery
                            .sessions
                            .iter()
                            .take(50)
                            .map(|session| {
                                let current = if session.id == prepared.session.id().as_str() {
                                    "*"
                                } else {
                                    " "
                                };
                                format!(
                                    "{current} {} · {} · {}",
                                    session.id,
                                    session.title.as_deref().unwrap_or("New Session"),
                                    session.work_dir
                                )
                            })
                            .collect::<Vec<_>>();
                        if discovery.sessions.len() > 50 {
                            lines.push(format!(
                                "… {} more sessions omitted",
                                discovery.sessions.len() - 50
                            ));
                        }
                        lines.push(
                            "use /sessions <id> to switch; /sessions all lists every cwd"
                                .to_owned(),
                        );
                        for warning in discovery.warnings {
                            lines.push(format!("warning: {warning}"));
                        }
                        self.status(lines.join("\n"));
                    }
                    Err(error) => self.status(format!("could not list sessions: {error}")),
                }
            } else {
                match prepared
                    .session_index
                    .validate_resume(arguments, &prepared.working_dir)
                {
                    Ok(_) if arguments == prepared.session.id().as_str() => {
                        self.status("session is already active");
                    }
                    Ok(_) => {
                        self.session_transition =
                            Some(InteractiveSessionTransition::Resume(arguments.to_owned()));
                    }
                    Err(SessionIndexError::CrossWorkingDirectory { expected, .. }) => {
                        self.status(format!(
                            "session is in another working directory; resume with: {}",
                            resume_command(&expected.to_string_lossy(), arguments)
                        ));
                    }
                    Err(error) => self.status(format!("could not resume session: {error}")),
                }
            }
            return true;
        }
        if let Some(arguments) = slash_arguments(input, "/model") {
            if arguments.is_empty() {
                self.status(format!(
                    "model: {}\navailable: {}",
                    prepared.model_alias,
                    prepared.model_aliases.join(", ")
                ));
            } else if arguments == prepared.model_alias {
                self.status(format!("already using model {}", prepared.model_alias));
            } else if prepared
                .model_aliases
                .iter()
                .any(|alias| alias == arguments)
            {
                self.session_transition =
                    Some(InteractiveSessionTransition::Model(arguments.to_owned()));
            } else {
                self.status(format!("unknown model alias: {arguments}"));
            }
            return true;
        }
        for command in ["/effort", "/thinking"] {
            let Some(arguments) = slash_arguments(input, command) else {
                continue;
            };
            if arguments.is_empty() {
                self.status(format!(
                    "thinking effort: {}\navailable: {}",
                    self.thinking_effort
                        .as_ref()
                        .map(ThinkingEffort::as_str)
                        .unwrap_or("off"),
                    prepared.effort_options.join(", ")
                ));
                return true;
            }
            let effort = arguments.to_ascii_lowercase();
            let declared = prepared.effort_options.iter().any(|known| known == &effort);
            if !declared && !prepared.allow_unknown_effort {
                self.status(format!(
                    "unsupported thinking effort {effort:?} for {}; available: {}",
                    prepared.model_alias,
                    prepared.effort_options.join(", ")
                ));
                return true;
            }
            if !declared {
                self.status(format!(
                    "thinking effort {effort:?} is not declared for {}; the provider will validate it",
                    prepared.model_alias
                ));
            }
            self.thinking_effort = if effort == "off" {
                None
            } else {
                match ThinkingEffort::new(effort.clone()) {
                    Ok(effort) => Some(effort),
                    Err(error) => {
                        self.status(format!("invalid thinking effort: {error}"));
                        return true;
                    }
                }
            };
            self.status(format!(
                "thinking set to {} for this session",
                self.thinking_effort
                    .as_ref()
                    .map(ThinkingEffort::as_str)
                    .unwrap_or("off")
            ));
            return true;
        }
        if let Some(arguments) = slash_arguments(input, "/add-dir") {
            if arguments.is_empty() || arguments.eq_ignore_ascii_case("list") {
                if prepared.additional_dirs.is_empty() {
                    self.status("No additional directories configured.");
                } else {
                    self.status(format!(
                        "Additional directories:\n{}",
                        prepared
                            .additional_dirs
                            .iter()
                            .map(|path| format!("  {}", path.display()))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ));
                }
                return true;
            }
            let (remember, path) = arguments
                .strip_prefix("remember ")
                .map_or((false, arguments), |path| (true, path.trim()));
            let path = match resolve_workspace_directory(
                &prepared.working_dir,
                path,
                prepared.user_home.as_deref(),
            ) {
                Ok(path) => path,
                Err(error) => {
                    self.status(error);
                    return true;
                }
            };
            let mut additional_dirs = prepared.additional_dirs.clone();
            if !additional_dirs.contains(&path) {
                additional_dirs.push(path.clone());
            } else if !remember {
                self.status(format!(
                    "workspace directory is already active: {}",
                    path.display()
                ));
                return true;
            }
            let remembered = if remember {
                match remember_workspace_additional_dir(&prepared.working_dir, &path) {
                    Ok(config) => Some(config.config_path),
                    Err(error) => {
                        self.status(format!("could not remember workspace directory: {error}"));
                        return true;
                    }
                }
            } else {
                None
            };
            match prepared
                .session_index
                .set_additional_dirs(prepared.session.id().as_str(), &additional_dirs)
            {
                Ok(_) => {
                    let notice = remembered.map_or_else(
                        || {
                            format!(
                                "Added workspace directory for this session: {}",
                                path.display()
                            )
                        },
                        |config_path| {
                            format!(
                                "Added workspace directory {} and saved it to {}",
                                path.display(),
                                config_path.display()
                            )
                        },
                    );
                    self.session_transition =
                        Some(InteractiveSessionTransition::AddDir { path, notice });
                }
                Err(error) => self.status(format!("could not add workspace directory: {error}")),
            }
            return true;
        }
        if let Some(arguments) = input.strip_prefix("/goal") {
            if arguments.is_empty() || arguments.starts_with(char::is_whitespace) {
                self.handle_goal_command(executor, prepared, input, arguments.trim());
                return true;
            }
        }
        if let Some(arguments) = input.strip_prefix("/swarm") {
            if arguments.is_empty() || arguments.starts_with(char::is_whitespace) {
                self.handle_swarm_command(executor, prepared, arguments.trim());
                return true;
            }
        }
        if let Some(arguments) = input.strip_prefix("/hyphae") {
            if arguments.is_empty() || arguments.starts_with(char::is_whitespace) {
                self.handle_hyphae_command(executor, prepared, input, arguments.trim());
                return true;
            }
        }
        for command in ["/tasks", "/task"] {
            if let Some(arguments) = input.strip_prefix(command) {
                if arguments.is_empty() || arguments.starts_with(char::is_whitespace) {
                    self.handle_tasks_command(executor, prepared, input, arguments.trim());
                    return true;
                }
            }
        }
        if let Some(arguments) = input.strip_prefix("/undo") {
            if arguments.is_empty() || arguments.starts_with(char::is_whitespace) {
                self.handle_undo_command(executor, prepared, arguments.trim());
                return true;
            }
        }
        if let Some(arguments) = input.strip_prefix("/compact") {
            if arguments.is_empty() || arguments.starts_with(char::is_whitespace) {
                self.start_compaction(executor, prepared, input, arguments.trim());
                return true;
            }
        }
        for command in ["/export-md", "/export"] {
            if let Some(arguments) = slash_arguments(input, command) {
                self.export_markdown(executor, prepared, arguments);
                return true;
            }
        }
        if let Some(arguments) = slash_arguments(input, "/export-debug-zip") {
            if arguments.is_empty() {
                self.export_debug_zip(prepared);
            } else {
                self.status("usage: /export-debug-zip");
            }
            return true;
        }
        for command in ["/provider", "/providers"] {
            if let Some(arguments) = slash_arguments(input, command) {
                match parse_interactive_provider_command(arguments) {
                    Ok((command, close_after)) => {
                        self.session_transition = Some(InteractiveSessionTransition::Provider {
                            command,
                            close_after,
                        });
                    }
                    Err(error) => self.status(error),
                }
                return true;
            }
        }
        if let Some(arguments) = input.strip_prefix("/plugins") {
            if arguments.is_empty() || arguments.starts_with(char::is_whitespace) {
                self.handle_plugins_command(executor, prepared, arguments.trim());
                return true;
            }
        }
        if let Some((command, arguments)) =
            parse_plugin_submission(input, &prepared.plugins.command_names)
        {
            self.start_plugin_command(executor, prepared, input.to_owned(), command, arguments);
            return true;
        }
        if let Some((command, arguments)) = parse_ecology_submission(input) {
            let mutates = matches!(
                command,
                crate::ecology::EcologyCommand::Promote | crate::ecology::EcologyCommand::Deny
            );
            match prepared.ecology.run(command, arguments, Utc::now()) {
                EcologyDispatch::Panel { title, lines } => {
                    self.status(format!("{title}\n{}", lines.join("\n")));
                    if mutates {
                        self.refresh_substrate(prepared);
                    }
                }
                EcologyDispatch::Error(error) => self.status(format!("ecology error: {error}")),
                EcologyDispatch::Status(status) => self.status(status),
                EcologyDispatch::Delegate { task } => {
                    self.start_delegate(executor, prepared, input.to_owned(), task);
                }
            }
            return true;
        }
        match input {
            "/help" | "/h" | "/?" => {
                self.status(
                    "commands: /help /status /usage /new /sessions /reload /reload-tui /settings /theme /editor /experiments /btw /fork /title /model /effort /provider /login /logout /permission /plan /swarm /hyphae /goal /tasks /undo /compact /mcp /plugins /add-dir /init /export-md /export-debug-zip /copy /immunity /gate /substrate /candidates /promote /deny /delegate /exit",
                );
                return true;
            }
            "/version" => {
                self.status(format!("mycel {}", env!("CARGO_PKG_VERSION")));
                return true;
            }
            "/status" => {
                let snapshot = executor.block_on(prepared.session.snapshot());
                let goal = prepared.orchestration.goal_driver().snapshot();
                self.status(format!(
                    "session: {}\nmodel: {}\ncwd: {}\npermission: {}\nplan: {}\nswarm: {}\neffort: {}\ngoal: {}",
                    prepared.session.id(),
                    prepared.model_alias,
                    prepared.working_dir.display(),
                    permission_name(snapshot.state.permission_mode),
                    if snapshot.state.plan_mode { "on" } else { "off" },
                    if snapshot.state.swarm_mode { "on" } else { "off" },
                    self.thinking_effort
                        .as_ref()
                        .map(ThinkingEffort::as_str)
                        .unwrap_or("default"),
                    goal.current
                        .as_ref()
                        .map(|goal| goal.objective.as_str())
                        .unwrap_or("none"),
                ));
                return true;
            }
            "/usage" => {
                let snapshot = executor.block_on(prepared.session.snapshot());
                if snapshot.state.usage_by_model.is_empty() {
                    self.status("no token usage recorded");
                } else {
                    self.status(
                        snapshot
                            .state
                            .usage_by_model
                            .iter()
                            .map(|(model, usage)| {
                                format!(
                                    "{model}: {} input · {} output · {} total",
                                    usage.input_total(),
                                    usage.output,
                                    usage.grand_total()
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
                return true;
            }
            "/mcp" => {
                let entries = prepared
                    .mcp
                    .as_ref()
                    .map(|runtime| executor.block_on(runtime.list()))
                    .unwrap_or_default();
                if entries.is_empty() {
                    self.status("no MCP servers configured");
                } else {
                    self.status(
                        entries
                            .iter()
                            .map(|entry| {
                                let mut line = format!(
                                    "{} · {:?} · {:?} · {} tools",
                                    entry.name, entry.transport, entry.status, entry.tool_count
                                );
                                if let Some(error) = &entry.error {
                                    line.push_str(" · ");
                                    line.push_str(error);
                                }
                                line
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
                return true;
            }
            "/init" => {
                self.start_host_tool(
                    executor,
                    prepared,
                    "/init".to_owned(),
                    "Agent".to_owned(),
                    serde_json::json!({
                        "prompt": INIT_PROMPT,
                        "description": "Initialize AGENTS.md",
                        "subagent_type": "general",
                        "run_in_background": false,
                    }),
                    SessionPhase::Busy,
                );
                return true;
            }
            "/copy" => {
                self.copy_last_assistant_message();
                return true;
            }
            "/login" => {
                self.session_transition = Some(InteractiveSessionTransition::Provider {
                    command: Command::Login,
                    close_after: false,
                });
                return true;
            }
            "/logout" | "/disconnect" => {
                self.session_transition = Some(InteractiveSessionTransition::Provider {
                    command: Command::Provider(ProviderArgs {
                        command: ProviderCommand::Logout {
                            provider: ProviderAuthTarget::Kimi,
                        },
                    }),
                    close_after: true,
                });
                return true;
            }
            "/exit" | "/quit" | "/q" => {
                self.exit_after_turn = true;
                return true;
            }
            _ => {}
        }
        for (command, spec) in [
            (
                "/yolo",
                PermissionToggleSpec {
                    mode: ProtocolPermissionMode::Yolo,
                    label: "YOLO",
                    enabled_detail:
                        "Tool actions auto-approved; the agent may still ask you questions.",
                },
            ),
            (
                "/yes",
                PermissionToggleSpec {
                    mode: ProtocolPermissionMode::Yolo,
                    label: "YOLO",
                    enabled_detail:
                        "Tool actions auto-approved; the agent may still ask you questions.",
                },
            ),
            (
                "/auto",
                PermissionToggleSpec {
                    mode: ProtocolPermissionMode::Auto,
                    label: "Auto",
                    enabled_detail:
                        "All actions auto-approved; the agent will not ask you questions.",
                },
            ),
        ] {
            let Some(arguments) = slash_arguments(input, command) else {
                continue;
            };
            self.toggle_interactive_permission(executor, prepared, command, arguments, spec);
            return true;
        }
        if let Some(arguments) = slash_arguments(input, "/permission") {
            if arguments.is_empty() {
                let snapshot = executor.block_on(prepared.session.snapshot());
                self.status(format!(
                    "permission mode: {}",
                    permission_name(snapshot.state.permission_mode)
                ));
            } else {
                match arguments {
                    "manual" => self.set_interactive_permission(
                        executor,
                        prepared,
                        ProtocolPermissionMode::Manual,
                    ),
                    "yolo" => self.set_interactive_permission(
                        executor,
                        prepared,
                        ProtocolPermissionMode::Yolo,
                    ),
                    "auto" => self.set_interactive_permission(
                        executor,
                        prepared,
                        ProtocolPermissionMode::Auto,
                    ),
                    _ => self.status("usage: /permission [manual|yolo|auto]"),
                }
            }
            return true;
        }
        if let Some(arguments) = slash_arguments(input, "/plan") {
            if arguments.eq_ignore_ascii_case("clear") {
                self.clear_interactive_plan(executor, prepared);
                return true;
            }
            let enabled = match arguments {
                "" => !self.reducer.plan,
                "on" => true,
                "off" => false,
                _ => {
                    self.status("usage: /plan [on|off|clear]");
                    return true;
                }
            };
            self.set_interactive_plan(executor, prepared, enabled);
            return true;
        }
        let rest = ["/title", "/rename"]
            .into_iter()
            .find_map(|command| input.strip_prefix(command));
        let Some(rest) = rest else {
            return false;
        };
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
            return false;
        }
        let title = rest.trim();
        if title.is_empty() {
            match prepared.session_index.get(prepared.session.id().as_str()) {
                Ok(Some(summary)) => self.status(format!(
                    "session title: {}",
                    summary.title.as_deref().unwrap_or("New Session")
                )),
                Ok(None) => self.status("session metadata is unavailable"),
                Err(error) => self.status(format!("could not read session title: {error}")),
            }
        } else {
            let title = title.chars().take(200).collect::<String>();
            match prepared
                .session_index
                .set_title(prepared.session.id().as_str(), &title)
            {
                Ok(_) => self.status(format!("session title set to {title}")),
                Err(error) => self.status(format!("could not set session title: {error}")),
            }
        }
        true
    }

    fn export_markdown(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        arguments: &str,
    ) {
        let snapshot = executor.block_on(prepared.session.snapshot());
        let history = snapshot.state.context.history();
        if history.is_empty() {
            self.status("No messages to export.");
            return;
        }
        let now = Utc::now();
        let output = if arguments.is_empty() {
            default_markdown_export_path(&prepared.working_dir, prepared.session.id().as_str(), now)
        } else {
            let requested = Path::new(arguments);
            if requested.is_absolute() {
                requested.to_path_buf()
            } else {
                prepared.working_dir.join(requested)
            }
        };
        let markdown = build_export_markdown(&MarkdownExport {
            session_id: prepared.session.id().as_str(),
            work_dir: &prepared.working_dir,
            history,
            token_count: snapshot.state.context.token_count(),
            now,
        });
        match write_markdown_export(&output, &markdown) {
            Ok(()) => self.status(format!(
                "Exported {} messages\n{}",
                history.len(),
                output.display()
            )),
            Err(error) => self.status(format!("Failed to export session: {error}")),
        }
    }

    fn export_debug_zip(&mut self, prepared: &PreparedInteractive) {
        let output = run_export(
            &ExportArgs {
                session_id: Some(prepared.session.id().as_str().to_owned()),
                output: None,
                yes: true,
                include_global_log: true,
            },
            &prepared.home,
            &prepared.working_dir,
            &FilesystemSessionExportStore,
            &ProcessExportConfirmation,
            env!("CARGO_PKG_VERSION"),
        );
        match output.completion {
            RuntimeCompletion::Success { .. } => {
                self.status(format!("Export complete\n{}", output.stdout.trim()))
            }
            _ => self.status(format!(
                "Failed to export session: {}",
                output.stderr.trim()
            )),
        }
    }

    fn copy_last_assistant_message(&mut self) {
        const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
        // Streamed deltas coalesce for a short window before they become a
        // frame. The user can see the answer and type /copy inside that
        // window; without this flush we would report "no assistant message"
        // for text that is already on screen.
        self.transcript.flush_now();
        let Some(text) = self
            .transcript
            .frames()
            .iter()
            .rev()
            .find(|frame| frame.kind == FrameKind::Assistant && !frame.text.trim().is_empty())
            .map(|frame| frame.text.clone())
        else {
            self.status("No assistant message to copy.");
            return;
        };
        if text.len() > MAX_CLIPBOARD_BYTES {
            self.status(format!(
                "assistant message is too large to copy safely ({} bytes; limit {})",
                text.len(),
                MAX_CLIPBOARD_BYTES
            ));
            return;
        }
        let encoded = BASE64_STANDARD.encode(text.as_bytes());
        self.terminal_sequences
            .push_back(format!("\u{1b}]52;c;{encoded}\u{7}").into_bytes());
        self.status(format!(
            "Copied via terminal escape sequence (unverified, {} characters).",
            text.chars().count()
        ));
    }

    fn paste_clipboard_image(&mut self) {
        let image = match read_clipboard_image() {
            Ok(Some(image)) => image,
            Ok(None) => {
                self.status("No supported image found in the clipboard.");
                return;
            }
            Err(error) => {
                self.status(format!("Could not paste clipboard image: {error}"));
                return;
            }
        };
        let placeholder = match self.pasted_images.add(image) {
            Ok(placeholder) => placeholder,
            Err(error) => {
                self.status(format!("Could not attach clipboard image: {error}"));
                return;
            }
        };
        let prefix = if self.reducer.editor.text().is_empty()
            || self.reducer.editor.text().ends_with(char::is_whitespace)
        {
            ""
        } else {
            " "
        };
        self.reducer
            .editor
            .insert_paste(&format!("{prefix}{placeholder} "));
        self.status(format!("Attached {placeholder}"));
    }

    fn set_interactive_permission(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        mode: ProtocolPermissionMode,
    ) {
        match executor.block_on(prepared.session.set_permission_mode(mode)) {
            Ok(()) => self.status(format!("permission mode: {}", permission_name(mode))),
            Err(error) => self.status(format!("could not set permission mode: {error}")),
        }
    }

    fn toggle_interactive_permission(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        command: &str,
        arguments: &str,
        spec: PermissionToggleSpec,
    ) {
        let current = executor
            .block_on(prepared.session.snapshot())
            .state
            .permission_mode;
        let target = match arguments.to_ascii_lowercase().as_str() {
            "" => {
                if current == spec.mode {
                    ProtocolPermissionMode::Manual
                } else {
                    spec.mode
                }
            }
            "on" => spec.mode,
            "off" => {
                if current == spec.mode {
                    ProtocolPermissionMode::Manual
                } else {
                    self.status(format!("{} mode is already off", spec.label));
                    return;
                }
            }
            _ => {
                self.status(format!("usage: {command} [on|off]"));
                return;
            }
        };
        if target == current {
            self.status(format!("{} mode is already on", spec.label));
            return;
        }
        match executor.block_on(prepared.session.set_permission_mode(target)) {
            Ok(()) if target == spec.mode => {
                self.status(format!("{} mode: ON\n{}", spec.label, spec.enabled_detail));
            }
            Ok(()) => self.status(format!("{} mode: OFF", spec.label)),
            Err(error) => self.status(format!("could not set permission mode: {error}")),
        }
    }

    fn clear_interactive_plan(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
    ) {
        let snapshot = match executor.block_on(
            <SessionHandle as SessionBuiltinStatePort>::snapshot(&prepared.session),
        ) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.status(format!("could not inspect plan mode: {error}"));
                return;
            }
        };
        let result = if snapshot.plan_mode {
            snapshot
                .plan_file
                .as_deref()
                .ok_or_else(|| "active plan has no file path".to_owned())
                .and_then(|path| clear_plan_file(&prepared.home, path))
        } else {
            Ok(())
        };
        match result {
            Ok(()) => self.status("Plan cleared"),
            Err(error) => self.status(format!("could not clear plan: {error}")),
        }
    }

    fn set_interactive_plan(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        enabled: bool,
    ) {
        let transition = if enabled {
            executor.block_on(
                prepared
                    .session
                    .enter_plan_mode(Some(prepared.plan_file.to_string_lossy().into_owned())),
            )
        } else {
            executor.block_on(prepared.session.exit_plan_mode())
        };
        match transition {
            Ok(()) => {
                self.reducer.plan = enabled;
                self.status(if enabled {
                    format!("plan mode enabled · {}", prepared.plan_file.display())
                } else {
                    "plan mode disabled".to_owned()
                });
            }
            Err(error) => self.status(format!(
                "could not {} plan mode: {error}",
                if enabled { "enable" } else { "disable" }
            )),
        }
    }

    fn handle_swarm_command(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        arguments: &str,
    ) {
        match arguments.to_ascii_lowercase().as_str() {
            "on" => {
                self.set_interactive_swarm(executor, prepared, true, "manual");
            }
            "off" => {
                self.set_interactive_swarm(executor, prepared, false, "manual");
            }
            "" => {
                self.set_interactive_swarm(executor, prepared, !self.swarm_mode, "manual");
            }
            _ => {
                if !self.swarm_mode && !self.set_interactive_swarm(executor, prepared, true, "task")
                {
                    return;
                }
                self.start_turn(executor, prepared, arguments.to_owned());
            }
        }
    }

    fn set_interactive_swarm(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        enabled: bool,
        trigger: &str,
    ) -> bool {
        if self.swarm_mode == enabled {
            self.status(if enabled {
                "swarm mode is already on"
            } else {
                "swarm mode is already off"
            });
            return true;
        }
        match executor.block_on(prepared.session.set_swarm_mode(enabled, trigger)) {
            Ok(()) => {
                self.swarm_mode = enabled;
                self.status(if enabled {
                    "swarm mode enabled"
                } else {
                    "swarm mode disabled"
                });
                true
            }
            Err(error) => {
                self.status(format!(
                    "could not {} swarm mode: {error}",
                    if enabled { "enable" } else { "disable" }
                ));
                false
            }
        }
    }

    fn handle_hyphae_command(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        display: &str,
        arguments: &str,
    ) {
        let now = self.now_ms();
        self.transcript
            .push(TranscriptEvent::UserMessage(display.to_owned()), now);
        self.reducer.phase = SessionPhase::Busy;
        let engine = Arc::clone(&prepared.engine);
        let session = prepared.session.clone();
        let cancellation = CancellationToken::new();
        let turn_cancellation = cancellation.clone();
        let turn_finished_sender = self.turn_finished_sender.clone();
        let prompt = display.to_owned();
        let arguments = arguments.to_owned();
        let task = executor.spawn(async move {
            let result = match engine
                .invoke_host_tool(
                    &session,
                    prompt,
                    "Hyphae".to_owned(),
                    serde_json::json!({"command": arguments}),
                    turn_cancellation,
                )
                .await
            {
                Ok(result) if result.is_error => Err(result
                    .message
                    .or(result.note)
                    .unwrap_or_else(|| "hyphae transition failed".to_owned())),
                Ok(result) => {
                    parse_hyphae_completion(result.output).map(|hyphae| InteractiveTurnResult {
                        event_barrier_turn_id: None,
                        reason: TurnOutcomeReason::Completed,
                        tokens: 0,
                        advances_goal: false,
                        status: None,
                        refresh_plugins: false,
                        hyphae: Some(hyphae),
                    })
                }
                Err(error) => Err(error.to_string()),
            };
            let _ = turn_finished_sender.send(result);
        });
        self.active = Some(ActiveTurn {
            cancellation,
            task,
            steerable: false,
        });
    }

    fn handle_tasks_command(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        display: &str,
        arguments: &str,
    ) {
        let parts = arguments.split_whitespace().collect::<Vec<_>>();
        let (tool_name, arguments) = match parts.as_slice() {
            [] | ["list"] => ("TaskList", serde_json::json!({"active_only": false})),
            ["active"] => ("TaskList", serde_json::json!({"active_only": true})),
            ["output", task_id] => (
                "TaskOutput",
                serde_json::json!({"task_id": task_id, "block": false}),
            ),
            ["stop", task_id] => (
                "TaskStop",
                serde_json::json!({
                    "task_id": task_id,
                    "reason": "stopped from /tasks"
                }),
            ),
            _ => {
                self.status("usage: /tasks [list|active|output <task-id>|stop <task-id>]");
                return;
            }
        };
        self.start_task_command(executor, prepared, display.to_owned(), tool_name, arguments);
    }

    fn handle_undo_command(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        arguments: &str,
    ) {
        let count = if arguments.is_empty() {
            1
        } else {
            match arguments.parse::<usize>() {
                Ok(count) if (1..=10_000).contains(&count) => count,
                _ => {
                    self.status("usage: /undo [1-10000]");
                    return;
                }
            }
        };
        match executor.block_on(prepared.session.undo_context(count)) {
            Ok(0) => self.status("nothing to undo"),
            Ok(removed) => self.status(format!(
                "undid {count} user message{} · removed {removed} context entr{}",
                if count == 1 { "" } else { "s" },
                if removed == 1 { "y" } else { "ies" }
            )),
            Err(error) => self.status(format!("could not undo context: {error}")),
        }
    }

    fn start_compaction(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        display: &str,
        arguments: &str,
    ) {
        let now = self.now_ms();
        self.transcript
            .push(TranscriptEvent::UserMessage(display.to_owned()), now);
        self.reducer.phase = SessionPhase::Compacting;
        let compaction = Arc::clone(&prepared.compaction);
        let session = prepared.session.clone();
        let cancellation = CancellationToken::new();
        let compaction_cancellation = cancellation.clone();
        let turn_finished_sender = self.turn_finished_sender.clone();
        let mut request = CompactionRequest::manual(
            prepared.system_prompt.as_ref(),
            (!arguments.is_empty()).then(|| arguments.to_owned()),
        );
        request.thinking_effort = self.thinking_effort.clone();
        request.max_completion_tokens = prepared.max_completion_tokens;
        let task = executor.spawn(async move {
            let result = compaction
                .compact_manual(&session, request, compaction_cancellation)
                .await
                .map(|_| InteractiveTurnResult {
                    event_barrier_turn_id: None,
                    reason: TurnOutcomeReason::Completed,
                    tokens: 0,
                    advances_goal: false,
                    status: None,
                    refresh_plugins: false,
                    hyphae: None,
                })
                .map_err(|error| error.to_string());
            let _ = turn_finished_sender.send(result);
        });
        self.active = Some(ActiveTurn {
            cancellation,
            task,
            steerable: false,
        });
    }

    fn handle_plugins_command(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        arguments: &str,
    ) {
        if arguments.is_empty() || arguments == "list" {
            if self.plugin_view.infos.is_empty() {
                self.status("no local plugins installed");
                return;
            }
            self.status(
                self.plugin_view
                    .infos
                    .iter()
                    .map(|plugin| {
                        format!(
                            "{} {} · {} skills · {} MCP · {} commands",
                            plugin.id,
                            plugin.version,
                            plugin.skill_roots,
                            plugin.mcp_servers.len(),
                            plugin.commands.len()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            return;
        }
        if let Some(id) = arguments.strip_prefix("info ").map(str::trim) {
            match self.plugin_view.infos.iter().find(|plugin| plugin.id == id) {
                Some(plugin) => self.status(format!(
                    "{} {}\n{}\nroot: {}\nskills: {}\nMCP: {}\ncommands: {}",
                    plugin.id,
                    plugin.version,
                    plugin.description.as_deref().unwrap_or("local plugin"),
                    plugin.root.display(),
                    plugin.skill_roots,
                    join_or_none(&plugin.mcp_servers),
                    join_or_none(&plugin.commands),
                )),
                None => self.status(format!("plugin {id:?} is not installed")),
            }
            return;
        }
        if let Some(source) = arguments.strip_prefix("install ").map(str::trim) {
            if source.is_empty() {
                self.status("usage: /plugins install <local-directory>");
                return;
            }
            match resolve_plugin_install_source(
                source,
                &prepared.working_dir,
                prepared.user_home.as_deref(),
            ) {
                Ok(source) => self.start_plugin_store_operation(
                    executor,
                    prepared,
                    PluginStoreOperation::Install { source },
                ),
                Err(error) => self.status(format!("could not install plugin: {error}")),
            }
            return;
        }
        let parts = arguments.split_whitespace().collect::<Vec<_>>();
        match parts.as_slice() {
            ["enable", id] | ["disable", id] => self.start_plugin_store_operation(
                executor,
                prepared,
                PluginStoreOperation::SetEnabled {
                    id: (*id).to_owned(),
                    enabled: parts[0] == "enable",
                },
            ),
            ["mcp", action @ ("enable" | "disable"), id, server] => {
                self.start_plugin_store_operation(
                    executor,
                    prepared,
                    PluginStoreOperation::SetMcpEnabled {
                        id: (*id).to_owned(),
                        server: (*server).to_owned(),
                        enabled: *action == "enable",
                    },
                );
            }
            ["remove", id] => self.start_plugin_store_operation(
                executor,
                prepared,
                PluginStoreOperation::Remove {
                    id: (*id).to_owned(),
                },
            ),
            ["reload"] => {
                let previous = self
                    .plugin_view
                    .infos
                    .iter()
                    .map(|plugin| plugin.id.clone())
                    .collect::<BTreeSet<_>>();
                match compose_plugins(&prepared.home) {
                    Ok(plugins) => {
                        let current = plugins
                            .infos
                            .iter()
                            .map(|plugin| plugin.id.clone())
                            .collect::<BTreeSet<_>>();
                        let added = current.difference(&previous).count();
                        let removed = previous.difference(&current).count();
                        let errors = plugins.warnings.len();
                        self.plugin_view = plugins;
                        self.status(format!(
                            "plugin reload: +{added} -{removed} · {errors} diagnostics. Start a new session to apply runtime changes."
                        ));
                    }
                    Err(error) => self.status(format!("plugin reload failed: {error}")),
                }
            }
            [id] if self.plugin_view.infos.iter().any(|plugin| plugin.id == *id) => {
                let plugin = self
                    .plugin_view
                    .infos
                    .iter()
                    .find(|plugin| plugin.id == *id)
                    .cloned()
                    .expect("guarded plugin lookup");
                self.status(format!(
                    "{} {}\n{}\nroot: {}\nskills: {}\nMCP: {}\ncommands: {}",
                    plugin.id,
                    plugin.version,
                    plugin.description.as_deref().unwrap_or("local plugin"),
                    plugin.root.display(),
                    plugin.skill_roots,
                    join_or_none(&plugin.mcp_servers),
                    join_or_none(&plugin.commands),
                ));
            }
            _ => self.status(
                "usage: /plugins [list|info <id>|install <local-directory>|enable|disable <id>|mcp enable|disable <id> <server>|remove <id>|reload]",
            ),
        }
    }

    fn handle_goal_command(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        display: &str,
        arguments: &str,
    ) {
        let driver = prepared.orchestration.goal_driver();
        match arguments {
            "" | "status" => {
                let board = driver.snapshot();
                let mut lines = Vec::new();
                if let Some(goal) = board.current {
                    lines.push(format!(
                        "active goal [{}]: {}",
                        format!("{:?}", goal.status).to_ascii_lowercase(),
                        goal.objective
                    ));
                    if let Some(reason) = goal.reason {
                        lines.push(format!("reason: {reason}"));
                    }
                    lines.push(format!(
                        "usage: {} turns · {} tokens",
                        goal.budget.turns_used, goal.budget.tokens_used
                    ));
                } else {
                    lines.push("no active goal".to_owned());
                }
                if !board.queue.is_empty() {
                    lines.push(format!("queued: {}", board.queue.len()));
                    lines.extend(
                        board
                            .queue
                            .iter()
                            .map(|goal| format!("  {} · {}", goal.id, goal.objective)),
                    );
                }
                self.status(lines.join("\n"));
            }
            "pause" => match driver.pause(None) {
                Ok(()) => self.status("goal paused"),
                Err(error) => self.status(format!("could not pause goal: {error}")),
            },
            "resume" => match driver.resume() {
                Ok(()) => {
                    let board = driver.snapshot();
                    if let Some(goal) = board.current {
                        self.status(format!("goal resumed: {}", goal.objective));
                        self.start_turn_with_origin(
                            executor,
                            prepared,
                            Some(display.to_owned()),
                            format!("Resume the active goal. Objective: {}", goal.objective),
                            PromptOrigin::SystemTrigger {
                                name: "goal_resume".to_owned(),
                            },
                        );
                    }
                }
                Err(error) => self.status(format!("could not resume goal: {error}")),
            },
            "cancel" => match driver.cancel(Some("cancelled by user")) {
                Ok(goal) => self.status(format!("goal cancelled: {}", goal.objective)),
                Err(error) => self.status(format!("could not cancel goal: {error}")),
            },
            "next" | "next manage" => {
                let board = driver.snapshot();
                if board.queue.is_empty() {
                    self.status("goal queue is empty");
                } else {
                    self.status(
                        board
                            .queue
                            .iter()
                            .enumerate()
                            .map(|(index, goal)| {
                                format!("{}. {} · {}", index + 1, goal.id, goal.objective)
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
            }
            _ => {
                let (mode, raw_objective) = if let Some(rest) = arguments.strip_prefix("replace") {
                    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                        ("replace", rest.trim())
                    } else {
                        ("create", arguments)
                    }
                } else if let Some(rest) = arguments.strip_prefix("next") {
                    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                        ("next", rest.trim())
                    } else {
                        ("create", arguments)
                    }
                } else {
                    ("create", arguments)
                };
                let objective = raw_objective
                    .strip_prefix("--")
                    .map_or(raw_objective, str::trim_start)
                    .trim();
                if objective.is_empty() {
                    self.status("usage: /goal [replace|next] [--] <objective>");
                    return;
                }
                if objective.chars().count() > 4_000 {
                    self.status("goal objective must be 4000 characters or fewer");
                    return;
                }
                let id = RequestId::generate().into_string();
                if mode == "next" && driver.snapshot().current.is_some() {
                    match driver.enqueue(&id, objective) {
                        Ok(goal) => self.status(format!("goal queued: {}", goal.objective)),
                        Err(error) => self.status(format!("could not queue goal: {error}")),
                    }
                    return;
                }
                match driver.create(&id, objective, mode == "replace") {
                    Ok(goal) => {
                        self.start_turn_with_origin(
                            executor,
                            prepared,
                            Some(display.to_owned()),
                            goal.objective,
                            PromptOrigin::User,
                        );
                    }
                    Err(error) => self.status(format!("could not create goal: {error}")),
                }
            }
        }
    }

    fn status(&mut self, message: impl Into<String>) {
        let now = self.now_ms();
        self.transcript
            .push(TranscriptEvent::Status(message.into()), now);
    }

    fn process_runtime_messages(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
    ) -> Result<(), String> {
        loop {
            match self.messages.try_recv() {
                Ok(InteractiveRuntimeMessage::Event(event)) => {
                    let now = self.now_ms();
                    if let AgentEvent::AgentStatusUpdated {
                        plan_mode: Some(plan_mode),
                        ..
                    } = event.as_ref()
                    {
                        self.reducer.plan = *plan_mode;
                    }
                    // A recorded denial means the gate captured a sentinel
                    // event into the substrate: re-read the summary so the
                    // candidate count moves with it, and resolve the denial's
                    // antibody pointer for the inspector while still at the
                    // event boundary (the render tick must not gain I/O).
                    if self.gate_log.observe(event.as_ref(), now) {
                        self.refresh_substrate(prepared);
                        self.last_deny_antibody = self
                            .gate_log
                            .last()
                            .and_then(|decision| parse_antibody_source(&decision.detail))
                            .and_then(|id| prepared.ecology.find_antibody(id))
                            .map(|antibody| antibody_detail(&antibody));
                    }
                    project_interactive_event(*event, &mut self.transcript, now);
                }
                Ok(InteractiveRuntimeMessage::EventLagged(count)) => {
                    return Err(format!("interactive event stream lagged by {count} events"));
                }
                Ok(InteractiveRuntimeMessage::EventClosed) => {
                    return Err("interactive event stream closed unexpectedly".to_owned());
                }
                Ok(InteractiveRuntimeMessage::TurnFinished(result)) => {
                    let cancelled = self
                        .active
                        .as_ref()
                        .is_some_and(|active| active.cancellation.is_cancelled());
                    self.active.take();
                    self.reducer.phase = SessionPhase::Idle;
                    if let Ok(turn) = &result {
                        if turn.refresh_plugins {
                            match compose_plugins(&prepared.home) {
                                Ok(plugins) => self.plugin_view = plugins,
                                Err(error) => {
                                    self.status(format!("could not refresh plugin view: {error}"));
                                }
                            }
                        }
                        if let Some(status) = &turn.status {
                            self.status(status.clone());
                        }
                    }
                    if std::mem::take(&mut self.hyphae_task_active) {
                        match prepared.orchestration.finish_hyphae_task() {
                            Ok(state) => {
                                self.thinking_effort = state.thinking_effort;
                                self.status("hyphae one-shot finished · swarm disabled");
                            }
                            Err(error) => {
                                self.status(format!("could not finish hyphae one-shot: {error}"));
                            }
                        }
                    }
                    let mut goal_started = false;
                    match result {
                        Ok(turn)
                            if matches!(
                                turn.reason,
                                TurnOutcomeReason::Completed
                                    | TurnOutcomeReason::MaxTokens
                                    | TurnOutcomeReason::Filtered
                                    | TurnOutcomeReason::ToolStopped
                            ) =>
                        {
                            if let Some(hyphae) = turn.hyphae {
                                self.thinking_effort = hyphae.thinking_effort;
                                let effort = self
                                    .thinking_effort
                                    .as_ref()
                                    .map(ThinkingEffort::as_str)
                                    .unwrap_or("default");
                                if let Some(prompt) = hyphae.submit_prompt {
                                    self.hyphae_task_active = true;
                                    self.status(format!(
                                        "hyphae one-shot enabled · effort {effort}"
                                    ));
                                    self.start_turn_with_origin(
                                        executor,
                                        prepared,
                                        None,
                                        prompt,
                                        PromptOrigin::User,
                                    );
                                    goal_started = true;
                                } else {
                                    self.status(format!(
                                        "hyphae {} · effort {effort}",
                                        display_hyphae_mode(&hyphae.swarm_mode)
                                    ));
                                }
                            } else if turn.advances_goal {
                                goal_started =
                                    self.advance_goal_after_turn(executor, prepared, turn.tokens);
                            }
                        }
                        Ok(turn) if turn.reason == TurnOutcomeReason::Paused => {
                            self.status("turn paused")
                        }
                        Ok(turn) if turn.reason == TurnOutcomeReason::Aborted && cancelled => {
                            self.status("turn cancelled")
                        }
                        Ok(_) => self.status("turn aborted"),
                        Err(_) if cancelled => self.status("turn cancelled"),
                        Err(error) => self.status(format!("turn failed: {error}")),
                    }
                    if !goal_started && (!self.exit_after_turn || !self.reducer.queue.is_empty()) {
                        self.start_next_queued(executor, prepared);
                    }
                }
                Ok(InteractiveRuntimeMessage::BtwEvent(event)) => {
                    let now = self.now_ms();
                    if let Some(panel) = self.btw.as_mut() {
                        project_interactive_event(*event, &mut panel.transcript, now);
                    }
                }
                Ok(InteractiveRuntimeMessage::BtwEventLagged(count)) => {
                    let now = self.now_ms();
                    if let Some(panel) = self.btw.as_mut() {
                        panel.transcript.push(
                            TranscriptEvent::Status(format!(
                                "BTW event stream lagged by {count} events"
                            )),
                            now,
                        );
                    }
                }
                Ok(InteractiveRuntimeMessage::BtwFinished(result)) => {
                    let now = self.now_ms();
                    if let Some(panel) = self.btw.as_mut() {
                        panel.active.take();
                        let status = match result {
                            Ok(TurnOutcomeReason::Completed | TurnOutcomeReason::ToolStopped) => {
                                "BTW ready for a follow-up.".to_owned()
                            }
                            Ok(TurnOutcomeReason::Aborted) => "BTW interrupted by user.".to_owned(),
                            Ok(reason) => format!("BTW turn ended with reason: {reason:?}"),
                            Err(error) => format!("BTW turn failed: {error}"),
                        };
                        panel.transcript.push(TranscriptEvent::Status(status), now);
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    let now = self.now_ms();
                    for event in prepared.orchestration_events.drain() {
                        project_orchestration_event(event, prepared, &mut self.transcript, now);
                    }
                    return Ok(());
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err("interactive runtime channel closed unexpectedly".to_owned());
                }
            }
        }
    }

    fn advance_goal_after_turn(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
        tokens: u64,
    ) -> bool {
        let driver = prepared.orchestration.goal_driver();
        let before = driver.snapshot();
        if before
            .current
            .as_ref()
            .is_some_and(|goal| goal.status == mycel_agent_runtime::GoalStatus::Active)
        {
            if let Err(error) = prepared.orchestration.record_goal_turn_usage(tokens) {
                self.status(format!("could not record goal usage: {error}"));
                return false;
            }
        }
        let board = driver.snapshot();
        if let Some(goal) = board.current {
            if goal.status == mycel_agent_runtime::GoalStatus::Active
                && self.reducer.queue.is_empty()
                && !self.exit_after_turn
            {
                self.status(format!("continuing goal: {}", goal.objective));
                self.start_turn_with_origin(
                    executor,
                    prepared,
                    None,
                    format!(
                        "Continue working on the active goal. Objective: {}",
                        goal.objective
                    ),
                    PromptOrigin::SystemTrigger {
                        name: "goal_continuation".to_owned(),
                    },
                );
                return true;
            }
            return false;
        }
        if board.promotion_pending && self.reducer.queue.is_empty() && !self.exit_after_turn {
            match driver.promote_next(PromotionGate {
                session_matches: true,
                idle: true,
                user_queue_empty: true,
                dispatch_pending: false,
                compacting: false,
            }) {
                Ok(Some(goal)) => {
                    self.status(format!("starting queued goal: {}", goal.objective));
                    self.start_turn_with_origin(
                        executor,
                        prepared,
                        None,
                        goal.objective,
                        PromptOrigin::SystemTrigger {
                            name: "goal_queue".to_owned(),
                        },
                    );
                    true
                }
                Ok(None) => false,
                Err(error) => {
                    self.status(format!("could not promote queued goal: {error}"));
                    false
                }
            }
        } else {
            false
        }
    }

    fn start_next_queued(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
    ) {
        while self.active.is_none() && !self.reducer.queue.is_empty() {
            let queued = self.reducer.queue.remove(0);
            match queued.mode {
                SubmissionMode::Prompt => {
                    if self.handle_session_command(executor, prepared, &queued.text) {
                        if self.active.is_none() {
                            self.reducer.phase = SessionPhase::Idle;
                        }
                    } else {
                        self.start_turn(executor, prepared, queued.text);
                    }
                }
                SubmissionMode::Shell => self.start_shell(executor, prepared, queued.text),
            }
        }
    }

    fn poll_cron(
        &mut self,
        executor: &tokio::runtime::Runtime,
        prepared: &PreparedInteractive,
    ) -> Result<(), String> {
        if self.active.is_some() || self.dialogs.is_active() || !self.reducer.queue.is_empty() {
            return Ok(());
        }
        if self.system_queue.is_empty() {
            for fire in prepared
                .orchestration
                .tick_cron(true)
                .map_err(|error| format!("could not tick cron scheduler: {error}"))?
            {
                self.system_queue.push_back((fire.task_id, fire.prompt));
            }
        }
        if let Some((task_id, prompt)) = self.system_queue.pop_front() {
            self.status(format!("cron fired: {task_id}"));
            self.start_turn_with_origin(
                executor,
                prepared,
                None,
                prompt,
                PromptOrigin::SystemTrigger {
                    name: format!("cron:{task_id}"),
                },
            );
        }
        Ok(())
    }

    fn shutdown(&mut self, executor: &tokio::runtime::Runtime) {
        self.dialogs
            .cancel_all("interactive session closed while waiting for input");
        if let Some(active) = self.active.take() {
            active.cancellation.cancel();
            // Bounded: give the turn a moment to honor cancellation, then abort
            // it. A turn stuck in something non-cancellable must not block the
            // process from exiting.
            let mut task = active.task;
            let joined = executor.block_on(async {
                tokio::time::timeout(SHUTDOWN_JOIN_BOUND, &mut task)
                    .await
                    .is_ok()
            });
            if !joined {
                task.abort();
                let _ = executor.block_on(task);
            }
        }
        self.close_btw(executor, None);
        self.event_pump.abort();
        let _ = executor.block_on(&mut self.event_pump);
    }
}

fn parse_interactive_provider_command(input: &str) -> Result<(Command, bool), String> {
    let words = input.split_whitespace().collect::<Vec<_>>();
    match words.as_slice() {
        [] | ["list"] => Ok((
            Command::Provider(ProviderArgs {
                command: ProviderCommand::List { json: false },
            }),
            false,
        )),
        ["list", "--json"] => Ok((
            Command::Provider(ProviderArgs {
                command: ProviderCommand::List { json: true },
            }),
            false,
        )),
        ["login", "kimi"] => Ok((
            Command::Provider(ProviderArgs {
                command: ProviderCommand::Login {
                    provider: ProviderAuthTarget::Kimi,
                },
            }),
            false,
        )),
        ["logout", "kimi"] => Ok((
            Command::Provider(ProviderArgs {
                command: ProviderCommand::Logout {
                    provider: ProviderAuthTarget::Kimi,
                },
            }),
            true,
        )),
        ["remove", provider_id] if valid_provider_id(provider_id) => Ok((
            Command::Provider(ProviderArgs {
                command: ProviderCommand::Remove {
                    provider_id: (*provider_id).to_owned(),
                },
            }),
            true,
        )),
        ["remove", _] => Err(
            "provider id must use lowercase ASCII letters, digits, dashes, or underscores"
                .to_owned(),
        ),
        _ => Err(
            "usage: /provider [list [--json]|login kimi|logout kimi|remove <provider-id>]"
                .to_owned(),
        ),
    }
}

#[cfg(unix)]
fn edit_in_external_editor(command: &str, draft: &str) -> Result<Option<String>, String> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    const EDIT_LIMIT: u64 = 4 * 1024 * 1024;
    let directory = std::env::temp_dir().join(format!("mycel-edit-{}", uuid::Uuid::new_v4()));
    let file_path = directory.join("prompt.md");
    let result = (|| {
        let mut directory_options = fs::DirBuilder::new();
        directory_options.mode(0o700);
        directory_options
            .create(&directory)
            .map_err(|error| format!("could not create editor directory: {error}"))?;
        let mut file_options = fs::OpenOptions::new();
        file_options.write(true).create_new(true).mode(0o600);
        let mut file = file_options
            .open(&file_path)
            .map_err(|error| format!("could not create editor draft: {error}"))?;
        file.write_all(draft.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("could not seed editor draft: {error}"))?;
        drop(file);
        let shell_command = format!("{command} \"$1\"");
        let status = ProcessCommand::new("/bin/sh")
            .arg("-c")
            .arg(shell_command)
            .arg("mycel-editor")
            .arg(&file_path)
            .status()
            .map_err(|error| format!("could not start editor: {error}"))?;
        if !status.success() {
            return Ok(None);
        }
        let metadata = fs::symlink_metadata(&file_path)
            .map_err(|error| format!("could not inspect edited draft: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err("editor replaced the draft with a symlink or special file".to_owned());
        }
        if metadata.len() > EDIT_LIMIT {
            return Err("edited draft exceeds the 4 MiB limit".to_owned());
        }
        let edited = fs::read_to_string(&file_path)
            .map_err(|error| format!("could not read edited draft: {error}"))?;
        let mut edited = edited.replace("\r\n", "\n");
        if edited.ends_with('\n') {
            edited.pop();
        }
        Ok(Some(edited))
    })();
    let cleanup = fs::remove_dir_all(&directory);
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup)) => Err(format!("could not remove editor draft: {cleanup}")),
        (Err(error), Err(cleanup)) => Err(format!(
            "{error}; additionally could not remove editor draft: {cleanup}"
        )),
    }
}

#[cfg(not(unix))]
fn edit_in_external_editor(_command: &str, _draft: &str) -> Result<Option<String>, String> {
    Err("external editor integration is not available on this platform".to_owned())
}

fn valid_provider_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().enumerate().all(|(index, byte)| match byte {
            b'a'..=b'z' | b'0'..=b'9' => true,
            b'_' | b'-' => index > 0,
            _ => false,
        })
}

fn provider_transition_notice(output: &AdapterOutput) -> String {
    let mut text = String::new();
    if !output.stdout.trim().is_empty() {
        text.push_str(output.stdout.trim());
    }
    if !output.stderr.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(output.stderr.trim());
    }
    if text.is_empty() {
        text.push_str(
            if matches!(output.completion, RuntimeCompletion::Success { .. }) {
                "Provider command completed."
            } else {
                "Provider command failed."
            },
        );
    }
    text.push('\n');
    text
}

fn parse_plugin_submission(input: &str, installed: &BTreeSet<String>) -> Option<(String, String)> {
    let input = input.strip_prefix('/')?;
    let (name, arguments) = input
        .split_once(char::is_whitespace)
        .map_or((input, ""), |(name, arguments)| (name, arguments.trim()));
    let (plugin, command) = name.split_once(':')?;
    if plugin.is_empty() || command.is_empty() || command.contains(':') {
        return None;
    }
    let runtime_name = format!("{plugin}.{command}");
    installed
        .contains(&runtime_name)
        .then(|| (runtime_name, arguments.to_owned()))
}

fn slash_arguments<'a>(input: &'a str, command: &str) -> Option<&'a str> {
    let rest = input.strip_prefix(command)?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim())
}

fn permission_name(mode: ProtocolPermissionMode) -> &'static str {
    match mode {
        ProtocolPermissionMode::Manual => "manual",
        ProtocolPermissionMode::Yolo => "yolo",
        ProtocolPermissionMode::Auto => "auto",
    }
}

fn projected_side_context(history: &[ContextEntry]) -> Vec<ContextEntry> {
    let mut outstanding = BTreeSet::new();
    let mut safe_end = 0usize;
    for (index, entry) in history.iter().enumerate() {
        for call in &entry.message.tool_calls {
            outstanding.insert(call.id.clone());
        }
        if let Some(tool_call_id) = &entry.message.tool_call_id {
            outstanding.remove(tool_call_id);
        }
        if outstanding.is_empty() {
            safe_end = index.saturating_add(1);
        }
    }
    history[..safe_end].to_vec()
}

fn resolve_plugin_install_source(
    source: &str,
    working_dir: &Path,
    user_home: Option<&Path>,
) -> Result<PathBuf, String> {
    let source = source.trim();
    if source.is_empty() || source.chars().any(char::is_control) {
        return Err("plugin source must be a non-empty local path".to_owned());
    }
    if source.starts_with("http://") || source.starts_with("https://") {
        return Err("remote plugin installation is not supported".to_owned());
    }
    if source == "~" || source.starts_with("~/") {
        let home = user_home.ok_or_else(|| "HOME is unavailable for ~ expansion".to_owned())?;
        return Ok(if source == "~" {
            home.to_path_buf()
        } else {
            home.join(&source[2..])
        });
    }
    let source = Path::new(source);
    Ok(if source.is_absolute() {
        source.to_path_buf()
    } else {
        working_dir.join(source)
    })
}

fn parse_hyphae_completion(
    output: mycel_agent_protocol::ExecutableToolOutput,
) -> Result<HyphaeCompletion, String> {
    let mycel_agent_protocol::ExecutableToolOutput::Text(text) = output else {
        return Err("hyphae transition returned a non-text result".to_owned());
    };
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| "hyphae transition returned invalid JSON".to_owned())?;
    let state = value
        .get("state")
        .and_then(Value::as_object)
        .ok_or_else(|| "hyphae transition omitted state".to_owned())?;
    let thinking_effort = match state.get("thinkingEffort") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            serde_json::from_value(value.clone())
                .map_err(|_| "hyphae transition returned an invalid thinking effort".to_owned())?,
        ),
    };
    let swarm_mode = state
        .get("swarmMode")
        .and_then(Value::as_str)
        .ok_or_else(|| "hyphae transition omitted swarm mode".to_owned())?
        .to_owned();
    let submit_prompt = match value.get("submitPrompt") {
        None | Some(Value::Null) => None,
        Some(Value::String(prompt)) if !prompt.trim().is_empty() => Some(prompt.clone()),
        Some(_) => return Err("hyphae transition returned an invalid one-shot prompt".to_owned()),
    };
    Ok(HyphaeCompletion {
        thinking_effort,
        swarm_mode,
        submit_prompt,
    })
}

fn render_task_tool_output(
    tool_name: &str,
    output: mycel_agent_protocol::ExecutableToolOutput,
) -> Result<String, String> {
    let mycel_agent_protocol::ExecutableToolOutput::Text(text) = output else {
        return Err("task operation returned a non-text result".to_owned());
    };
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| "task operation returned invalid JSON".to_owned())?;
    match tool_name {
        "TaskList" => {
            let tasks = value
                .as_array()
                .ok_or_else(|| "task list returned an invalid result".to_owned())?;
            if tasks.is_empty() {
                return Ok("no background tasks".to_owned());
            }
            tasks
                .iter()
                .map(|task| {
                    let task = task
                        .as_object()
                        .ok_or_else(|| "task list contained an invalid task".to_owned())?;
                    let id = task_field(task, "id")?;
                    let kind = task_field(task, "kind")?;
                    let status = task_field(task, "status")?;
                    let description = task_field(task, "description")?;
                    Ok(format!("{id} · {kind} · {status} · {description}"))
                })
                .collect::<Result<Vec<_>, String>>()
                .map(|lines| lines.join("\n"))
        }
        "TaskOutput" => {
            let result = value
                .as_object()
                .ok_or_else(|| "task output returned an invalid result".to_owned())?;
            let task = result
                .get("task")
                .and_then(Value::as_object)
                .ok_or_else(|| "task output omitted task state".to_owned())?;
            let output = result
                .get("output")
                .and_then(Value::as_object)
                .ok_or_else(|| "task output omitted output state".to_owned())?;
            let id = task_field(task, "id")?;
            let status = task_field(task, "status")?;
            let body = output
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| "task output omitted text".to_owned())?;
            let body = if body.is_empty() { "(no output)" } else { body };
            let truncated = output
                .get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(format!(
                "task {id} · {status}\n{body}{}",
                if truncated {
                    "\n[output truncated]"
                } else {
                    ""
                }
            ))
        }
        "TaskStop" => {
            let task = value
                .as_object()
                .ok_or_else(|| "task stop returned an invalid result".to_owned())?;
            Ok(format!(
                "task {} · {}",
                task_field(task, "id")?,
                task_field(task, "status")?
            ))
        }
        _ => Err(format!("unsupported task operation {tool_name:?}")),
    }
}

fn task_field<'a>(
    task: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Result<&'a str, String> {
    task.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("task result omitted {field}"))
}

fn display_hyphae_mode(mode: &str) -> &str {
    match mode {
        "off" => "Off",
        "standing" => "Standing",
        "task" => "Task",
        mode => mode,
    }
}

fn join_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "none".to_owned()
    } else {
        values.join(", ")
    }
}

fn run_interactive_terminal<B: TerminalBackend>(
    executor: &tokio::runtime::Runtime,
    prepared: &PreparedInteractive,
    driver: &mut TerminalDriver<B>,
) -> Result<InteractiveTerminalOutcome, String> {
    let mut terminal = driver
        .start()
        .map_err(|error| format!("could not start terminal session: {error}"))?;
    let body = interactive_terminal_body(executor, prepared, &mut terminal);
    let finish = terminal
        .finish()
        .map_err(|error| format!("could not restore terminal session: {error}"));
    match (body, finish) {
        (Ok(completion), Ok(())) => Ok(completion),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(format!(
            "{error}; additionally terminal cleanup failed: {cleanup}"
        )),
    }
}

fn interactive_terminal_body<B: TerminalBackend>(
    executor: &tokio::runtime::Runtime,
    prepared: &PreparedInteractive,
    terminal: &mut TerminalSession<'_, B>,
) -> Result<InteractiveTerminalOutcome, String> {
    let size = terminal
        .size()
        .map_err(|error| format!("could not read terminal size: {error}"))?;
    let mut state = InteractiveLoopState::new(executor, prepared, size);
    // When stdin closes with an exit already requested and a turn still in
    // flight, this marks the start of the bounded grace wait for that turn.
    let mut exit_grace_started: Option<Instant> = None;
    let result = (|| loop {
        state.process_runtime_messages(executor, prepared)?;
        if let Some(transition) = state.session_transition.take() {
            break Ok(InteractiveTerminalOutcome::Transition(transition));
        }
        state.dialogs.poll();
        state.poll_cron(executor, prepared)?;
        let now = state.now_ms();
        state.spinner_phase = usize::try_from(now / SPINNER_INTERVAL_MS).unwrap_or(0);
        state.transcript.tick(now);
        if let Some(panel) = state.btw.as_mut() {
            panel.transcript.tick(now);
        }
        while let Some(sequence) = state.terminal_sequences.pop_front() {
            terminal
                .write(&sequence)
                .map_err(|error| format!("could not write terminal control sequence: {error}"))?;
        }
        render_interactive(&mut state, terminal)?;
        if state.exit_after_turn && state.active.is_none() {
            break Ok(InteractiveTerminalOutcome::Completion(
                RuntimeCompletion::success(),
            ));
        }

        match terminal
            .read_event(Some(INTERACTIVE_POLL))
            .map_err(|error| format!("could not read terminal input: {error}"))?
        {
            TerminalEvent::Input(bytes) => {
                let mut exit_requested = false;
                for input in state.decoder.feed(&bytes) {
                    if state.dialogs.is_active() {
                        state.dialogs.apply(input);
                        continue;
                    }
                    if state.btw.is_some() && is_escape(&input) {
                        state.close_btw(executor, Some("BTW closed."));
                        continue;
                    }
                    if state.btw.is_some() && is_control_c(&input) {
                        state.cancel_or_close_btw(executor);
                        continue;
                    }
                    if is_control_g(&input) && state.request_external_editor(prepared) {
                        exit_requested = true;
                        break;
                    }
                    if is_control_d(&input) && state.reducer.editor.text().is_empty() {
                        if let Some(active) = &state.active {
                            if state.exit_after_turn {
                                active.cancellation.cancel();
                                exit_requested = true;
                            } else {
                                state.exit_after_turn = true;
                                state.status(
                                    "exit requested; waiting for current turn (Ctrl-D again to cancel)",
                                );
                            }
                        } else {
                            exit_requested = true;
                        }
                        break;
                    }
                    state.reducer.apply(input);
                    if state.process_actions(executor, prepared) {
                        exit_requested = true;
                        break;
                    }
                }
                if exit_requested {
                    break Ok(match state.session_transition.take() {
                        Some(transition) => InteractiveTerminalOutcome::Transition(transition),
                        None => {
                            InteractiveTerminalOutcome::Completion(RuntimeCompletion::success())
                        }
                    });
                }
            }
            TerminalEvent::Resize(size) => {
                state.size = size;
                state.header_cache = None;
                state.renderer.reset();
                state.last_view.clear();
                state.last_cursor = None;
            }
            TerminalEvent::Signal(signal) => {
                break Ok(InteractiveTerminalOutcome::Completion(
                    RuntimeCompletion::Signal(map_terminal_signal(signal)),
                ));
            }
            TerminalEvent::EndOfInput if state.exit_after_turn && state.active.is_some() => {
                // Exit already requested, stdin gone, turn still running: give
                // the turn a bounded grace period to finish on its own, then
                // cancel it and exit. Never wait on it forever - a stalled
                // provider must not make the session unkillable.
                let started = *exit_grace_started.get_or_insert_with(Instant::now);
                if started.elapsed() >= EXIT_TURN_GRACE {
                    if let Some(active) = &state.active {
                        active.cancellation.cancel();
                    }
                    state.status(format!(
                        "exit: current turn did not finish within {}s; cancelled",
                        EXIT_TURN_GRACE.as_secs()
                    ));
                    break Ok(InteractiveTerminalOutcome::Completion(
                        RuntimeCompletion::success(),
                    ));
                }
                std::thread::sleep(INTERACTIVE_POLL);
            }
            TerminalEvent::EndOfInput => {
                break Ok(InteractiveTerminalOutcome::Completion(
                    RuntimeCompletion::success(),
                ));
            }
            TerminalEvent::Timeout | TerminalEvent::KeyboardProtocolChanged(_) => {}
        }
    })();
    state.shutdown(executor);
    result
}

fn is_control_d(event: &InputEvent) -> bool {
    matches!(
        event,
        InputEvent::Key(key)
            if key.modifiers.control && key.code == KeyCode::Char('d')
    )
}

fn is_control_c(event: &InputEvent) -> bool {
    matches!(
        event,
        InputEvent::Key(key)
            if key.modifiers.control && key.code == KeyCode::Char('c')
    )
}

fn is_control_g(event: &InputEvent) -> bool {
    matches!(
        event,
        InputEvent::Key(key)
            if key.modifiers.control && key.code == KeyCode::Char('g')
    )
}

fn is_escape(event: &InputEvent) -> bool {
    matches!(event, InputEvent::Key(key) if key.code == KeyCode::Escape)
}

fn map_terminal_signal(signal: TerminalSignal) -> TerminationSignal {
    match signal {
        TerminalSignal::Hangup => TerminationSignal::Hangup,
        TerminalSignal::Interrupt => TerminationSignal::Interrupt,
        TerminalSignal::Quit => TerminationSignal::Quit,
        TerminalSignal::Terminate => TerminationSignal::Terminate,
    }
}

fn render_interactive<B: TerminalBackend>(
    state: &mut InteractiveLoopState,
    terminal: &mut TerminalSession<'_, B>,
) -> Result<(), String> {
    let width = usize::from(state.size.columns.max(1));
    let height = usize::from(state.size.rows.max(1));
    let (lines, cursor_row, cursor_column) = interactive_view(state, width, height);
    let view_changed = state.last_view != lines;
    state
        .renderer
        .render(&lines, width, terminal)
        .map_err(|error| format!("could not render terminal: {error}"))?;
    let cursor = (cursor_row, cursor_column);
    if view_changed || state.last_cursor != Some(cursor) {
        crate::terminal::TerminalSink::write(
            terminal,
            format!("\x1b[{cursor_row};{cursor_column}H").as_bytes(),
        )
        .map_err(|error| format!("could not position terminal cursor: {error}"))?;
        terminal
            .flush_output()
            .map_err(|error| format!("could not flush terminal output: {error}"))?;
    }
    state.last_view = lines;
    state.last_cursor = Some(cursor);
    Ok(())
}

/// Snapshot the welcome-card data from the prepared session. The substrate
/// summary was read once in `prepare_interactive`; the loop refreshes it on
/// ecology-mutating events. Recent sessions were captured in
/// `prepare_interactive` from the discovery its register/refresh produced, so
/// building the header never re-acquires the cross-process index lock or
/// re-runs the repair scan.
fn build_header(prepared: &PreparedInteractive) -> HeaderData {
    HeaderData {
        model: prepared.model_alias.clone(),
        provider: prepared.provider.clone(),
        cwd: display_home_path(&prepared.working_dir, prepared.user_home.as_deref()),
        // TODO: context OCCUPANCY is not derivable from the loop's event
        // stream. `AgentEvent::TurnEnded` carries no usage
        // (crates/mycel-agent-protocol/src/event.rs:692-699) and the session's
        // `usage_by_model` (read by `/usage` above) accumulates turn totals,
        // which is not the live context size. Until the runtime exposes
        // occupancy, 0 here renders as the window alone, never a made-up fill.
        ctx_used: 0,
        ctx_window: prepared.context_window,
        substrate: substrate_summary_display(&prepared.substrate),
        recent: prepared.recent_sessions.clone(),
    }
}

/// Map the ecology-side substrate snapshot onto the header card's display
/// summary. `Tripwire` (wired fail-closed, db missing: everything refused) is
/// the card's `blocked`; `Disarmed` covers unwired and fail-open wiring.
fn substrate_summary_display(substrate: &SubstrateStatus) -> SubstrateSummary {
    SubstrateSummary {
        antibodies: substrate.antibodies_active,
        candidates_pending: substrate.candidates_pending,
        gate: match substrate.gate {
            GateStatus::Ok => GateDisplay::Ok,
            GateStatus::Tripwire => GateDisplay::Blocked,
            GateStatus::Disarmed => GateDisplay::Disarmed,
            GateStatus::Unknown => GateDisplay::Unknown,
        },
    }
}

/// Pull the antibody id out of a gate deny reason. Substrate-matched refusals
/// end in `(source: antibody:<uuid>)` (crates/mycel-core/src/lib.rs `refusal`
/// construction; crates/mycel-gate/src/main.rs `emit_block`); floor and
/// structural denies carry `mycel-gate:` pointers instead and resolve to
/// `None`. `rfind` guards against remediation text containing the marker.
fn parse_antibody_source(detail: &str) -> Option<uuid::Uuid> {
    const MARKER: &str = "(source: antibody:";
    let start = detail.rfind(MARKER)? + MARKER.len();
    let rest = &detail[start..];
    let end = rest.find(')')?;
    rest[..end].trim().parse().ok()
}

/// Snapshot the substrate record for the inspector's detail box. Labels match
/// the enums' snake_case serde names; the mockup's `name`, last-hit date, and
/// per-decision trace do not exist in the record (see `AntibodyDetail`).
fn antibody_detail(antibody: &mycel_core::Antibody) -> AntibodyDetail {
    use mycel_core::{AntibodySource, Confidence, RefusalMode, Severity, SignatureScope};
    let mut signature = Vec::new();
    for (field, value) in [
        ("command_pattern", &antibody.signature.command_pattern),
        ("tool_pattern", &antibody.signature.tool_pattern),
        ("file_pattern", &antibody.signature.file_pattern),
        ("error_class", &antibody.signature.error_class),
        ("agent_role", &antibody.signature.agent_role),
    ] {
        if let Some(value) = value {
            signature.push((field.to_owned(), value.clone()));
        }
    }
    AntibodyDetail {
        id: crate::util::short_id(&antibody.id.to_string()),
        source: match antibody.source {
            AntibodySource::SentinelBlock => "sentinel_block",
            AntibodySource::FailedRun => "failed_run",
            AntibodySource::Manual => "manual",
        }
        .to_owned(),
        scope: match antibody.signature.scope {
            SignatureScope::Project => "project",
            SignatureScope::Global => "global",
            SignatureScope::Personal => "personal",
        }
        .to_owned(),
        severity: match antibody.severity {
            Severity::Refuse => "refuse",
            Severity::Warn => "warn",
            Severity::Info => "info",
        }
        .to_owned(),
        confidence: match antibody.confidence {
            Confidence::Solid => "solid",
            Confidence::Directional => "directional",
            Confidence::Vibes => "vibes",
        }
        .to_owned(),
        refusal: match antibody.refusal_mode {
            RefusalMode::Hard => "hard",
            RefusalMode::Soft => "soft",
            RefusalMode::LogOnly => "log-only",
        }
        .to_owned(),
        hits: antibody.hit_count,
        signature,
        remediation: antibody.remediation.clone(),
    }
}

/// Compact age for the rail's hyphae line: `now` under a minute, then whole
/// minutes, then whole hours.
fn format_age(elapsed_ms: u64) -> String {
    let minutes = elapsed_ms / 60_000;
    if minutes == 0 {
        "now".to_owned()
    } else if minutes < 60 {
        format!("{minutes}m ago")
    } else {
        format!("{}h ago", minutes / 60)
    }
}

/// Wall-clock unix epoch milliseconds, shared by the loop tick and the
/// construction-time seed frames so every gutter timestamp is real.
fn epoch_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Seed the transcript shown at construction: the session/model line plus any
/// startup warning. Both frames share one wall-clock stamp.
fn seed_transcript(session_line: String, warning: Option<&str>) -> TranscriptReducer {
    let now = epoch_now_ms();
    let mut transcript = TranscriptReducer::default();
    transcript.push(TranscriptEvent::Status(session_line), now);
    if let Some(warning) = warning {
        transcript.push(TranscriptEvent::Status(format!("warning: {warning}")), now);
    }
    transcript
}

/// Render a path with the user's home directory collapsed to `~`.
fn display_home_path(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home {
        if let Ok(relative) = path.strip_prefix(home) {
            if relative.as_os_str().is_empty() {
                return "~".to_owned();
            }
            return format!("~/{}", relative.display());
        }
    }
    path.display().to_string()
}

fn interactive_view(
    state: &mut InteractiveLoopState,
    width: usize,
    height: usize,
) -> (Vec<String>, usize, usize) {
    // Theme and truecolor come from the caches `refresh_render_caches`
    // maintains; resolving them here would re-read config and the environment
    // at ~40Hz. The header render is likewise cached: its data is fixed at
    // construction, so it only re-renders when the width or theme changed.
    let frame_ctx = FrameCtx {
        width,
        truecolor: state.truecolor,
        spinner_phase: state.spinner_phase,
    };
    if state.header_cache.as_ref().map(|(cached, _)| *cached) != Some(width) {
        let rendered = header_card(&state.header, &state.theme, width, state.truecolor);
        state.header_cache = Some((width, rendered));
    }
    let theme = state.theme.clone();
    let mut lines = state
        .header_cache
        .as_ref()
        .map(|(_, rendered)| rendered.clone())
        .unwrap_or_default();
    for frame in state.transcript.frames() {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.extend(transcript_frame_lines(frame, &theme, &frame_ctx));
    }
    if state.reducer.phase != SessionPhase::Idle {
        lines.push(match state.reducer.phase {
            SessionPhase::Busy => "[running · ctrl-c cancels]".to_owned(),
            SessionPhase::Compacting => "[compacting · ctrl-c cancels]".to_owned(),
            SessionPhase::Shell => "[shell]".to_owned(),
            SessionPhase::Idle => String::new(),
        });
    }

    if let Some(panel) = &state.btw {
        lines.push(String::new());
        lines.push("┌─ BTW ─ side channel ─────────────────────────".to_owned());
        for frame in panel.transcript.frames() {
            lines.extend(transcript_frame_lines(frame, &theme, &frame_ctx));
        }
        lines.push(if panel.active.is_some() {
            "└─ running · ctrl-c cancels · esc closes".to_owned()
        } else {
            "└─ type a follow-up · esc closes".to_owned()
        });
    }

    if state.dialogs.active.is_some() {
        lines.push(String::new());
        lines.extend(dialog_view_lines(&state.dialogs, width));
        let cursor_absolute_row = lines.len().saturating_sub(1);
        let cursor_absolute_column =
            visible_width(lines.last().map(String::as_str).unwrap_or("")) + 1;
        let viewport_start = lines.len().saturating_sub(height);
        let visible = lines.into_iter().skip(viewport_start).collect::<Vec<_>>();
        let cursor_row = cursor_absolute_row
            .saturating_sub(viewport_start)
            .saturating_add(1)
            .clamp(1, height);
        return (
            visible,
            cursor_row,
            cursor_absolute_column.clamp(1, width.saturating_add(1)),
        );
    }

    let prompt = match state.reducer.input_mode {
        crate::tui::InputMode::Prompt => "> ",
        crate::tui::InputMode::Shell => "! ",
    };
    let before_cursor = format!(
        "{prompt}{}",
        &state.reducer.editor.text()[..state.reducer.editor.cursor()]
    );
    let cursor_lines = wrap_text(&before_cursor, width);
    let editor_lines = wrap_text(&format!("{prompt}{}", state.reducer.editor.text()), width);
    let editor_start = lines.len();
    let cursor_absolute_row = editor_start + cursor_lines.len().saturating_sub(1);
    let cursor_absolute_column =
        visible_width(cursor_lines.last().map(String::as_str).unwrap_or("")) + 1;
    lines.extend(editor_lines);

    let viewport_start = lines.len().saturating_sub(height);
    let visible = lines.into_iter().skip(viewport_start).collect::<Vec<_>>();
    let cursor_row = cursor_absolute_row
        .saturating_sub(viewport_start)
        .saturating_add(1)
        .clamp(1, height);
    let cursor_column = cursor_absolute_column.clamp(1, width.saturating_add(1));
    (visible, cursor_row, cursor_column)
}

fn dialog_view_lines(host: &DialogHost, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    match host.active.as_ref() {
        Some(ActiveDialog::Approval {
            request, reducer, ..
        }) => {
            push_wrapped(
                &mut lines,
                &format!("approval required · {}", request.tool_name),
                width,
            );
            push_wrapped(&mut lines, &request.action, width);
            if host.show_detail {
                for detail in approval_display_lines(&request.display) {
                    push_wrapped(&mut lines, &detail, width);
                }
            }
            lines.push(String::new());
            for (index, choice) in reducer.choices.iter().enumerate() {
                push_wrapped(
                    &mut lines,
                    &format!(
                        "{} {}. {}",
                        if index == reducer.selected { ">" } else { " " },
                        index + 1,
                        choice.label
                    ),
                    width,
                );
            }
            if reducer.feedback_mode {
                push_wrapped(
                    &mut lines,
                    &format!("feedback> {}", reducer.feedback),
                    width,
                );
            } else {
                push_wrapped(
                    &mut lines,
                    "enter/number selects · esc rejects · ctrl-o toggles detail",
                    width,
                );
            }
        }
        Some(ActiveDialog::Question {
            request, reducer, ..
        }) => {
            let view = reducer.view();
            if view.submit_tab {
                push_wrapped(&mut lines, "review answers", width);
                for (index, question) in request.questions.iter().enumerate() {
                    let answer = view
                        .answers
                        .get(index)
                        .and_then(Option::as_deref)
                        .unwrap_or("unanswered");
                    push_wrapped(
                        &mut lines,
                        &format!("{}. {}: {answer}", index + 1, question.prompt),
                        width,
                    );
                }
                lines.push(String::new());
                for (index, label) in ["Submit answers", "Cancel"].iter().enumerate() {
                    push_wrapped(
                        &mut lines,
                        &format!(
                            "{} {}. {label}",
                            if index == view.submit_action {
                                ">"
                            } else {
                                " "
                            },
                            index + 1
                        ),
                        width,
                    );
                }
            } else if let Some(question) = reducer.questions.get(view.current_tab) {
                push_wrapped(
                    &mut lines,
                    &format!(
                        "question {}/{}{}",
                        view.current_tab + 1,
                        reducer.questions.len(),
                        if question.multi_select {
                            " · select multiple"
                        } else {
                            ""
                        }
                    ),
                    width,
                );
                push_wrapped(&mut lines, &question.question, width);
                lines.push(String::new());
                for (index, option) in question.options.iter().enumerate() {
                    let selected = view.selected_options.contains(&index);
                    push_wrapped(
                        &mut lines,
                        &format!(
                            "{}{} {}. {}",
                            if index == view.cursor { ">" } else { " " },
                            if selected { "[x]" } else { "[ ]" },
                            index + 1,
                            option.label
                        ),
                        width,
                    );
                    if let Some(description) = option.description.as_deref() {
                        push_wrapped(&mut lines, &format!("    {description}"), width);
                    }
                }
                let other_index = question.options.len();
                push_wrapped(
                    &mut lines,
                    &format!(
                        "{}{} {}. {}{}",
                        if other_index == view.cursor { ">" } else { " " },
                        if view.selected_options.contains(&other_index) {
                            "[x]"
                        } else {
                            "[ ]"
                        },
                        other_index + 1,
                        question.other_label.as_deref().unwrap_or("Other"),
                        if view.editing_other {
                            format!(": {}", view.other_draft.as_deref().unwrap_or(""))
                        } else {
                            String::new()
                        }
                    ),
                    width,
                );
                push_wrapped(
                    &mut lines,
                    "arrows/tab navigate · space toggles · esc cancels",
                    width,
                );
            }
        }
        None => {}
    }
    lines
}

fn approval_display_lines(display: &ToolInputDisplay) -> Vec<String> {
    match display {
        ToolInputDisplay::Command {
            command,
            cwd,
            description,
            ..
        } => vec![
            description
                .clone()
                .unwrap_or_else(|| "run command".to_owned()),
            format!("$ {command}"),
            cwd.as_ref()
                .map(|cwd| format!("cwd: {cwd}"))
                .unwrap_or_default(),
        ],
        ToolInputDisplay::FileIo {
            operation, path, ..
        } => vec![format!("{operation:?}: {path}")],
        ToolInputDisplay::Diff { path, .. } => vec![format!("edit: {path}")],
        ToolInputDisplay::Search { query, scope } => vec![format!(
            "search: {query}{}",
            scope
                .as_ref()
                .map(|scope| format!(" in {scope}"))
                .unwrap_or_default()
        )],
        ToolInputDisplay::UrlFetch { url, method } => {
            vec![format!("{} {url}", method.as_deref().unwrap_or("fetch"))]
        }
        ToolInputDisplay::AgentCall {
            agent_name, prompt, ..
        } => vec![format!("agent: {agent_name}"), prompt.clone()],
        ToolInputDisplay::SkillCall { skill_name, args } => vec![format!(
            "skill: {skill_name} {}",
            args.as_deref().unwrap_or("")
        )],
        ToolInputDisplay::TodoList { items } => {
            vec![format!("update todo list ({} items)", items.len())]
        }
        ToolInputDisplay::Task {
            task_id,
            status,
            description,
            ..
        } => vec![format!("task {task_id} [{status}]: {description}")],
        ToolInputDisplay::TaskStop {
            task_id,
            task_description,
        } => vec![format!("stop task {task_id}: {task_description}")],
        ToolInputDisplay::PlanReview { plan, path, .. } => {
            let mut lines = path
                .as_ref()
                .map(|path| vec![format!("plan: {path}")])
                .unwrap_or_default();
            lines.push(plan.clone());
            lines
        }
        ToolInputDisplay::GoalStart { objective, .. } => {
            vec![format!("start goal: {objective}")]
        }
        ToolInputDisplay::Generic { summary, detail } => vec![
            summary.clone(),
            detail.as_ref().map(Value::to_string).unwrap_or_default(),
        ],
    }
    .into_iter()
    .filter(|line| !line.is_empty())
    .collect()
}

fn push_wrapped(lines: &mut Vec<String>, text: &str, width: usize) {
    let sanitized = text
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    lines.extend(wrap_text(&sanitized, width));
}

fn format_tui_settings(config: &TuiConfig) -> String {
    format!(
        "theme: {}\neditor: {}\npaste burst fallback: {}\nnotifications: {} ({})\nconfig: ~/.mycel/tui.toml",
        config.theme.as_str(),
        config.editor_command.as_deref().unwrap_or("auto"),
        if config.disable_paste_burst {
            "disabled"
        } else {
            "enabled"
        },
        if config.notifications_enabled {
            "enabled"
        } else {
            "disabled"
        },
        config.notification_condition.as_str(),
    )
}

fn project_interactive_event(event: AgentEvent, transcript: &mut TranscriptReducer, now_ms: u64) {
    let projected = match event {
        AgentEvent::TurnStarted { .. } => Some(TranscriptEvent::TurnStarted),
        AgentEvent::AssistantDelta { delta, .. } => Some(TranscriptEvent::AssistantDelta(delta)),
        AgentEvent::ThinkingDelta { delta, .. } => Some(TranscriptEvent::ThinkingDelta(delta)),
        AgentEvent::ToolCallStarted {
            tool_call_id,
            name,
            args,
            ..
        } => Some(TranscriptEvent::ToolStarted {
            id: tool_call_id,
            name,
            preview: Some(args.to_string()),
        }),
        AgentEvent::ToolCallDelta {
            tool_call_id,
            arguments_part,
            ..
        } => arguments_part.map(|text| TranscriptEvent::ToolProgress {
            id: tool_call_id,
            text,
        }),
        AgentEvent::ToolProgress {
            tool_call_id,
            update,
            ..
        } => update.text.map(|text| TranscriptEvent::ToolProgress {
            id: tool_call_id,
            text,
        }),
        AgentEvent::ToolResult {
            tool_call_id,
            output,
            is_error,
            ..
        } => Some(TranscriptEvent::ToolResult {
            id: tool_call_id,
            output: json_text(output),
            failed: is_error.unwrap_or(false),
        }),
        AgentEvent::HookResult {
            hook_event,
            content,
            blocked,
            ..
        } => Some(TranscriptEvent::HookResult {
            name: hook_event,
            content,
            blocked: blocked.unwrap_or(false),
        }),
        AgentEvent::TurnStepRetrying {
            failed_attempt,
            next_attempt,
            ..
        } => Some(TranscriptEvent::Retrying {
            failed_attempt: failed_attempt.try_into().unwrap_or(u32::MAX),
            next_attempt: next_attempt.try_into().unwrap_or(u32::MAX),
        }),
        AgentEvent::TurnStepCompleted { .. } => Some(TranscriptEvent::StepCompleted),
        AgentEvent::TurnEnded {
            reason: TurnEndReason::Failed | TurnEndReason::Blocked,
            error,
            ..
        } => {
            if let Some(error) = error {
                transcript.push(
                    TranscriptEvent::Status(format!("turn failed: {}", error.message)),
                    now_ms,
                );
            }
            Some(TranscriptEvent::TurnEnded)
        }
        AgentEvent::TurnEnded { .. } => Some(TranscriptEvent::TurnEnded),
        AgentEvent::Warning { message, code } => Some(TranscriptEvent::Status(match code {
            Some(code) => format!("warning [{code}]: {message}"),
            None => format!("warning: {message}"),
        })),
        AgentEvent::Error { error } => Some(TranscriptEvent::Status(format!(
            "error [{}]: {}",
            error.name.unwrap_or_else(|| format!("{:?}", error.code)),
            error.message
        ))),
        AgentEvent::TurnStepInterrupted {
            reason, message, ..
        } => Some(TranscriptEvent::Status(message.map_or_else(
            || format!("turn interrupted: {reason}"),
            |message| format!("turn interrupted ({reason}): {message}"),
        ))),
        AgentEvent::SubagentSpawned {
            subagent_id,
            subagent_name,
            description,
            ..
        } => Some(TranscriptEvent::SubagentState {
            id: subagent_id,
            name: subagent_name,
            state: "spawned".to_owned(),
            detail: description,
        }),
        AgentEvent::SubagentStarted { subagent_id } => Some(TranscriptEvent::SubagentState {
            name: subagent_id.clone(),
            id: subagent_id,
            state: "started".to_owned(),
            detail: None,
        }),
        AgentEvent::SubagentSuspended {
            subagent_id,
            reason,
        } => Some(TranscriptEvent::SubagentState {
            name: subagent_id.clone(),
            id: subagent_id,
            state: "suspended".to_owned(),
            detail: Some(reason),
        }),
        AgentEvent::SubagentCompleted {
            subagent_id,
            result_summary,
            ..
        } => Some(TranscriptEvent::SubagentState {
            name: subagent_id.clone(),
            id: subagent_id,
            state: "completed".to_owned(),
            detail: Some(result_summary),
        }),
        AgentEvent::SubagentFailed { subagent_id, error } => Some(TranscriptEvent::SubagentState {
            name: subagent_id.clone(),
            id: subagent_id,
            state: "failed".to_owned(),
            detail: Some(error),
        }),
        AgentEvent::CompactionStarted { instruction, .. } => {
            Some(TranscriptEvent::CompactionStarted { instruction })
        }
        AgentEvent::CompactionCompleted { result } => Some(TranscriptEvent::CompactionCompleted {
            tokens_before: result.tokens_before,
            tokens_after: result.tokens_after,
            summary: result.summary,
        }),
        AgentEvent::CompactionCancelled => Some(TranscriptEvent::CompactionCancelled),
        AgentEvent::CompactionBlocked { .. } => {
            Some(TranscriptEvent::CompactionBlocked { reason: None })
        }
        AgentEvent::BackgroundTaskStarted { info } => {
            let (id, kind, state, description) = background_frame(info);
            Some(TranscriptEvent::BackgroundTaskState {
                id,
                kind,
                state,
                description,
            })
        }
        AgentEvent::BackgroundTaskTerminated { info } => {
            let (id, kind, state, description) = background_frame(info);
            Some(TranscriptEvent::BackgroundTaskState {
                id,
                kind,
                state,
                description,
            })
        }
        AgentEvent::GoalUpdated {
            snapshot: Some(snapshot),
            ..
        } => Some(TranscriptEvent::GoalState {
            status: goal_status_name(snapshot.status).to_owned(),
            objective: snapshot.objective,
            detail: snapshot.terminal_reason,
        }),
        AgentEvent::GoalUpdated { snapshot: None, .. } => {
            Some(TranscriptEvent::Status("goal cleared".to_owned()))
        }
        AgentEvent::McpServerStatus { server } => Some(TranscriptEvent::McpServerState {
            name: server.name,
            status: format!("{:?}", server.status).to_ascii_lowercase(),
            detail: server.error,
        }),
        AgentEvent::ShellOutput { update, .. } => update.text.map(TranscriptEvent::Status),
        AgentEvent::ShellStarted { command_id, .. } => Some(TranscriptEvent::Status(format!(
            "shell command {command_id} started"
        ))),
        AgentEvent::SkillActivated { skill_name, .. } => Some(TranscriptEvent::Status(format!(
            "skill activated: {skill_name}"
        ))),
        AgentEvent::PluginCommandActivated {
            plugin_id,
            command_name,
            ..
        } => Some(TranscriptEvent::Status(format!(
            "plugin command activated: {plugin_id}/{command_name}"
        ))),
        AgentEvent::CronFired { .. }
        | AgentEvent::ToolListUpdated { .. }
        | AgentEvent::AgentStatusUpdated { .. }
        | AgentEvent::SessionMetaUpdated { .. }
        | AgentEvent::TurnStepStarted { .. } => None,
    };
    if let Some(projected) = projected {
        transcript.push(projected, now_ms);
    }
}

fn project_orchestration_event(
    event: OrchestrationEvent,
    prepared: &PreparedInteractive,
    transcript: &mut TranscriptReducer,
    now_ms: u64,
) {
    let projected = match event.scope.as_str() {
        "native-child-host" | "subagent" => {
            let id = event.entity_id.unwrap_or_else(|| "subagent".to_owned());
            TranscriptEvent::SubagentState {
                name: id.clone(),
                id,
                state: event.action,
                detail: None,
            }
        }
        "workflow" | "background" => {
            let id = event.entity_id.unwrap_or_else(|| event.scope.clone());
            TranscriptEvent::BackgroundTaskState {
                id,
                kind: event.scope,
                state: event.action,
                description: String::new(),
            }
        }
        "goal" => {
            let board = prepared.orchestration.goal_driver().snapshot();
            match board.current {
                Some(goal) => TranscriptEvent::GoalState {
                    status: format!("{:?}", goal.status).to_ascii_lowercase(),
                    objective: goal.objective,
                    detail: goal.reason,
                },
                None if event.action == "completed" => TranscriptEvent::GoalState {
                    status: "complete".to_owned(),
                    objective: event
                        .detail
                        .get("objective")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or(event.entity_id)
                        .unwrap_or_else(|| "goal".to_owned()),
                    detail: event
                        .detail
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
                None => TranscriptEvent::Status(format!("goal {}", event.action)),
            }
        }
        "cron" if event.action == "fired" => TranscriptEvent::Status("cron fired".to_owned()),
        _ => TranscriptEvent::Status(format!(
            "{} {}{}",
            event.scope,
            event.action,
            event
                .entity_id
                .as_deref()
                .map(|id| format!(" · {id}"))
                .unwrap_or_default()
        )),
    };
    transcript.push(projected, now_ms);
}

fn json_text(value: Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

fn background_frame(info: mycel_agent_protocol::TaskInfo) -> (String, String, String, String) {
    use mycel_agent_protocol::TaskInfo;
    let (base, kind) = match &info {
        TaskInfo::Process { base, .. } => (base, "process"),
        TaskInfo::Agent { base, .. } => (base, "agent"),
        TaskInfo::Question { base, .. } => (base, "question"),
        TaskInfo::Workflow { base, .. } => (base, "workflow"),
    };
    (
        base.task_id.clone(),
        kind.to_owned(),
        format!("{:?}", base.status).to_ascii_lowercase(),
        base.description.clone(),
    )
}

struct HeadlessRunContext {
    home: PathBuf,
    working_dir: PathBuf,
    config: MycelConfig,
    resolved: ResolvedModel,
    transport: Arc<dyn HttpTransport>,
    version: String,
    tool_registry: Arc<dyn ToolRegistryBuilder>,
    user_home: Option<PathBuf>,
    shell: Option<String>,
    mcp: SessionMcpServices,
    plugins: PluginComposition,
}

async fn run_headless(
    context: HeadlessRunContext,
    request: &PromptRequest,
    events: &mut dyn HeadlessEventSink,
) -> Result<RuntimeCompletion, String> {
    let HeadlessRunContext {
        home,
        working_dir,
        config,
        resolved,
        transport,
        version,
        tool_registry,
        user_home,
        shell,
        mcp,
        plugins,
    } = context;
    let mut factory = ProviderFactory::new(transport, home.clone(), version);
    if let Some(path) = resolved.google_application_credentials.clone() {
        factory = factory.with_google_application_credentials(path);
    }
    let registry = factory
        .build(resolved.registry.clone())
        .await
        .map_err(|error| format!("could not initialize provider registry: {error}"))?;
    let detected_capability = registry
        .model(&resolved.provider_id, &resolved.model_id)
        .map(|model| model.capability)
        .ok_or_else(|| {
            format!(
                "Model {:?} resolved to {}/{} but that provider model is unavailable.",
                resolved.alias, resolved.provider_id, resolved.model_id
            )
        })?;
    let provider: Arc<dyn TurnProvider> = Arc::new(RegistryTurnProvider {
        registry: Arc::new(registry),
        provider_id: resolved.provider_id.clone(),
        model_id: resolved.model_id.clone(),
    });
    let hooks = configured_hook_runner(&config, &working_dir)?;

    let runtime = Runtime::new(home.join(SESSIONS_DIR));
    let id = match &request.session {
        SessionSelection::New => SessionId::generate(),
        SessionSelection::Resume(id) => SessionId::new(id.clone())
            .map_err(|error| format!("invalid session id {id:?}: {error}"))?,
        SessionSelection::Pick | SessionSelection::Continue => {
            return Err("unsupported headless session selection reached runtime".to_owned())
        }
    };
    let mut options = SessionOptions::new(id);
    options.initial_permission_mode = ProtocolPermissionMode::Auto;
    options.permission_rules = config
        .permission
        .as_ref()
        .map(|permission| permission.rules.clone())
        .unwrap_or_default();
    options.hooks = hooks.clone();
    let is_new = matches!(request.session, SessionSelection::New);
    let session = match request.session {
        SessionSelection::New => runtime.create_session(options).await,
        SessionSelection::Resume(_) => runtime.resume_session(options).await,
        SessionSelection::Pick | SessionSelection::Continue => unreachable!("validated above"),
    }
    .map_err(|error| error.to_string())?;
    let plan_file = match resolve_plan_file(&home, &session).await {
        Ok(path) => path,
        Err(error) => return Err(close_after_setup_error(&session, error).await),
    };
    let plan_local = match LocalToolConfig::new(&working_dir, request.add_dirs.iter())
        .map_err(|error| format!("invalid plan workspace roots: {error}"))
        .and_then(|local| {
            local
                .with_allowed_files([&plan_file])
                .map_err(|error| format!("invalid plan-file grant: {error}"))
        }) {
        Ok(local) => local,
        Err(error) => return Err(close_after_setup_error(&session, error).await),
    };
    let foreground_processes = Arc::new(DeferredForegroundProcessPort::default());
    let tools = match tool_registry.build(
        &working_dir,
        &request.add_dirs,
        std::slice::from_ref(&plan_file),
        Some(foreground_processes.clone()),
    ) {
        Ok(tools) => tools,
        Err(error) => return Err(close_after_setup_error(&session, error).await),
    };
    if let Err(error) = register_plugin_commands(&tools, &plugins) {
        return Err(close_after_setup_error(&session, error).await);
    }
    let skills = match compose_skills(
        &config,
        &request.skills_dirs,
        &home,
        user_home.as_deref(),
        &working_dir,
        &plugins.plan.skill_roots,
    ) {
        Ok(skills) => skills,
        Err(error) => return Err(close_after_setup_error(&session, error).await),
    };
    let media = match media_config(
        &working_dir,
        &request.add_dirs,
        &resolved,
        detected_capability,
    ) {
        Ok(media) => media,
        Err(error) => return Err(close_after_setup_error(&session, error).await),
    };
    let PreparedSystemPrompt {
        text: system_prompt,
        warnings: system_prompt_warnings,
    } = build_system_prompt(SystemPromptContext {
        cwd: &working_dir,
        additional_dirs: &request.add_dirs,
        mycel_home: &home,
        user_home: user_home.as_deref(),
        shell: shell.as_deref(),
        now: Utc::now(),
        skills: &skills.catalog,
    });
    let system_prompt: Arc<str> = Arc::from(system_prompt);
    if is_new && config.default_plan_mode == Some(true) {
        if let Err(error) = session
            .enter_plan_mode(Some(plan_file.to_string_lossy().into_owned()))
            .await
        {
            return Err(close_after_setup_error(
                &session,
                format!("could not enable default plan mode: {error}"),
            )
            .await);
        }
    }
    let session_id = session.id().as_str().to_owned();
    let session_index = SessionIndex::new(&home);
    let indexed = if is_new {
        session_index.register_session(&session_id, &working_dir, &request.add_dirs)
    } else {
        session_index.refresh(&session_id)
    };
    if let Err(error) = indexed {
        let close = session.close().await;
        return Err(match close {
            Ok(()) => format!("could not update session index: {error}"),
            Err(close) => format!(
                "could not update session index: {error}; additionally session cleanup failed: {close}"
            ),
        });
    }
    let previous_permission = session.snapshot().await.state.permission_mode;
    if previous_permission != ProtocolPermissionMode::Auto {
        if let Err(error) = session
            .set_permission_mode(ProtocolPermissionMode::Auto)
            .await
        {
            let close = session
                .close()
                .await
                .map_err(|close| format!("could not close session: {close}"));
            let refresh = session_index
                .refresh(&session_id)
                .map(|_| ())
                .map_err(|refresh| format!("could not refresh session index: {refresh}"));
            let cleanup = combine_string_cleanup_results(Ok(()), Ok(()), close, refresh);
            return Err(match cleanup {
                Ok(()) => format!("could not enable headless permission mode: {error}"),
                Err(cleanup) => format!(
                    "could not enable headless permission mode: {error}; additionally cleanup failed: {cleanup}"
                ),
            });
        }
    }
    let mcp_runtime =
        match start_configured_session_mcp(&mcp, &home, &tools, &session, &working_dir).await {
            Ok(runtime) => runtime,
            Err(error) => {
                let restore = restore_permission(&session, previous_permission).await;
                let close = session
                    .close()
                    .await
                    .map_err(|close| format!("could not close session: {close}"));
                let refresh = session_index
                    .refresh(&session_id)
                    .map(|_| ())
                    .map_err(|refresh| format!("could not refresh session index: {refresh}"));
                let cleanup = combine_string_cleanup_results(Ok(()), restore, close, refresh);
                return Err(match cleanup {
                    Ok(()) => error,
                    Err(cleanup) => {
                        format!("{error}; additionally cleanup failed: {cleanup}")
                    }
                });
            }
        };
    let engine_config = turn_engine_config(&config, resolved.max_context_tokens);
    let orchestration_events = Arc::new(ProductionOrchestrationEvents::default());
    let orchestration = match open_native_orchestration(NativeOrchestrationContext {
        runtime: runtime.clone(),
        registry: tools.clone(),
        session: session.clone(),
        home: home.clone(),
        working_dir: working_dir.clone(),
        additional_dirs: request.add_dirs.clone(),
        provider: Arc::clone(&provider),
        hooks: hooks.clone(),
        engine_config: engine_config.clone(),
        system_prompt: Arc::clone(&system_prompt),
        permission: ProtocolPermissionMode::Auto,
        permission_rules: config
            .permission
            .as_ref()
            .map(|permission| permission.rules.clone())
            .unwrap_or_default(),
        approval_port: None,
        question_port: None,
        thinking_effort: resolved.thinking_effort.clone(),
        max_completion_tokens: resolved.max_completion_tokens,
        xhigh_supported: resolved.xhigh_supported,
        live_events: Arc::clone(&orchestration_events),
    }) {
        Ok(orchestration) => orchestration,
        Err(error) => {
            let mcp_shutdown = shutdown_mcp(mcp_runtime.as_ref()).await;
            let restore = restore_permission(&session, previous_permission).await;
            let close = session
                .close()
                .await
                .map_err(|close| format!("could not close session: {close}"));
            let refresh = session_index
                .refresh(&session_id)
                .map(|_| ())
                .map_err(|refresh| format!("could not refresh session index: {refresh}"));
            let cleanup = combine_string_cleanup_results(mcp_shutdown, restore, close, refresh);
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; additionally cleanup failed: {cleanup}"),
            });
        }
    };
    if let Err(error) = foreground_processes.bind(orchestration.foreground_process_port()) {
        let orchestration_shutdown = shutdown_orchestration(Some(orchestration.as_ref())).await;
        let mcp_shutdown = shutdown_mcp(mcp_runtime.as_ref()).await;
        let services_shutdown =
            combine_string_cleanup_results(orchestration_shutdown, mcp_shutdown, Ok(()), Ok(()));
        let restore = restore_permission(&session, previous_permission).await;
        let close = session
            .close()
            .await
            .map_err(|close| format!("could not close session: {close}"));
        let refresh = session_index
            .refresh(&session_id)
            .map(|_| ())
            .map_err(|refresh| format!("could not refresh session index: {refresh}"));
        let cleanup = combine_string_cleanup_results(services_shutdown, restore, close, refresh);
        return Err(match cleanup {
            Ok(()) => error,
            Err(cleanup) => format!("{error}; additionally cleanup failed: {cleanup}"),
        });
    }
    if let Err(error) = register_canonical_session_builtins(
        &tools,
        &session,
        plan_local,
        plan_file.clone(),
        skills.activation,
        media,
        Some(orchestration.goal_budget_port()),
    ) {
        let orchestration_shutdown = shutdown_orchestration(Some(orchestration.as_ref())).await;
        let mcp_shutdown = shutdown_mcp(mcp_runtime.as_ref()).await;
        let services_shutdown =
            combine_string_cleanup_results(orchestration_shutdown, mcp_shutdown, Ok(()), Ok(()));
        let restore = restore_permission(&session, previous_permission).await;
        let close = session
            .close()
            .await
            .map_err(|close| format!("could not close session: {close}"));
        let refresh = session_index
            .refresh(&session_id)
            .map(|_| ())
            .map_err(|refresh| format!("could not refresh session index: {refresh}"));
        let cleanup = combine_string_cleanup_results(services_shutdown, restore, close, refresh);
        return Err(match cleanup {
            Ok(()) => error,
            Err(cleanup) => format!("{error}; additionally cleanup failed: {cleanup}"),
        });
    }
    let engine = match TurnEngine::new(
        Arc::clone(&provider),
        tools.clone(),
        hooks.clone(),
        ToolScheduler::new(),
        engine_config,
    ) {
        Ok(engine) => engine,
        Err(error) => {
            let orchestration_shutdown = shutdown_orchestration(Some(orchestration.as_ref())).await;
            let mcp_shutdown = shutdown_mcp(mcp_runtime.as_ref()).await;
            let services_shutdown = combine_string_cleanup_results(
                orchestration_shutdown,
                mcp_shutdown,
                Ok(()),
                Ok(()),
            );
            let restore = restore_permission(&session, previous_permission).await;
            let close = session
                .close()
                .await
                .map_err(|close| format!("could not close session: {close}"));
            let refresh = session_index
                .refresh(&session_id)
                .map(|_| ())
                .map_err(|refresh| format!("could not refresh session index: {refresh}"));
            let cleanup =
                combine_string_cleanup_results(services_shutdown, restore, close, refresh);
            return Err(match cleanup {
                Ok(()) => error.to_string(),
                Err(cleanup) => format!("{error}; additionally cleanup failed: {cleanup}"),
            });
        }
    };
    let result = async {
        if let Some(warning) = session.warning() {
            events
                .emit(HeadlessEvent::Progress(format!("warning: {warning}")))
                .map_err(|error| error.to_string())?;
        }
        for warning in skills.warnings {
            events
                .emit(HeadlessEvent::Progress(format!("warning: {warning}")))
                .map_err(|error| error.to_string())?;
        }
        for warning in system_prompt_warnings {
            events
                .emit(HeadlessEvent::Progress(format!("warning: {warning}")))
                .map_err(|error| error.to_string())?;
        }
        for warning in &plugins.warnings {
            events
                .emit(HeadlessEvent::Progress(format!("warning: {warning}")))
                .map_err(|error| error.to_string())?;
        }

        if let Some(goal) = request.goal.as_ref() {
            let turn_config = HeadlessGoalTurnConfig {
                system_prompt: Arc::clone(&system_prompt),
                thinking_effort: resolved.thinking_effort.clone(),
                max_completion_tokens: resolved.max_completion_tokens,
            };
            drive_headless_goal(
                &engine,
                &session,
                orchestration.as_ref(),
                orchestration_events.as_ref(),
                goal,
                &turn_config,
                events,
            )
            .await
        } else {
            let mut input = TurnInput::user(&request.prompt, system_prompt.as_ref());
            input.thinking_effort = resolved.thinking_effort.clone();
            input.max_completion_tokens = resolved.max_completion_tokens;
            drive_turn(&engine, &session, input, events)
                .await
                .map(|driven| driven.completion)
        }
    }
    .await;
    let orchestration_shutdown = shutdown_orchestration(Some(orchestration.as_ref())).await;
    let mcp_shutdown = shutdown_mcp(mcp_runtime.as_ref()).await;
    let services_shutdown =
        combine_string_cleanup_results(orchestration_shutdown, mcp_shutdown, Ok(()), Ok(()));
    let restore = restore_permission(&session, previous_permission).await;
    let close = session
        .close()
        .await
        .map_err(|error| format!("could not close session: {error}"));
    let refresh = session_index
        .refresh(&session_id)
        .map(|_| ())
        .map_err(|error| format!("could not refresh session index: {error}"));
    let cleanup = combine_string_cleanup_results(services_shutdown, restore, close, refresh);
    match (result, cleanup) {
        (Ok(completion), Ok(())) => Ok(with_session(completion, session_id)),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), Err(cleanup_error)) => Err(format!(
            "{error}; additionally cleanup failed: {cleanup_error}"
        )),
    }
}

struct HeadlessGoalTurnConfig {
    system_prompt: Arc<str>,
    thinking_effort: Option<ThinkingEffort>,
    max_completion_tokens: Option<u64>,
}

async fn drive_headless_goal(
    engine: &TurnEngine,
    session: &SessionHandle,
    orchestration: &NativeOrchestrationBundle,
    orchestration_events: &ProductionOrchestrationEvents,
    request: &GoalCreateRequest,
    turn_config: &HeadlessGoalTurnConfig,
    events: &mut dyn HeadlessEventSink,
) -> Result<RuntimeCompletion, String> {
    let goal_id = RequestId::generate().into_string();
    orchestration
        .goal_driver()
        .create(&goal_id, &request.objective, request.replace)
        .map_err(|error| format!("could not create headless goal: {error}"))?;
    let started = Instant::now();
    let mut turns_used = 0u64;
    let mut tokens_used = 0u64;
    let mut first_turn = true;

    loop {
        let budget = orchestration
            .enforce_goal_budget()
            .map_err(|error| format!("could not enforce goal budget: {error}"))?;
        if budget.over_budget {
            emit_headless_goal_summary(
                events,
                &goal_id,
                GoalStatus::Blocked,
                Some("goal budget exhausted".to_owned()),
                turns_used,
                tokens_used,
                started,
            )?;
            return Ok(RuntimeCompletion::Goal {
                status: GoalStatus::Blocked,
                session_id: None,
            });
        }

        let prompt = if first_turn {
            request.objective.clone()
        } else {
            format!(
                "Continue working on the active goal. Objective: {}",
                request.objective
            )
        };
        let mut input = TurnInput::user(prompt, turn_config.system_prompt.as_ref());
        if !first_turn {
            input.origin = PromptOrigin::SystemTrigger {
                name: "goal_continuation".to_owned(),
            };
        }
        input.thinking_effort = turn_config.thinking_effort.clone();
        input.max_completion_tokens = turn_config.max_completion_tokens;
        let driven = drive_turn(engine, session, input, events).await?;
        turns_used = turns_used.saturating_add(1);
        tokens_used = tokens_used.saturating_add(driven.outcome.usage.grand_total());
        first_turn = false;

        let completion_reason = orchestration_events
            .drain()
            .into_iter()
            .rev()
            .find(|event| event.scope == "goal" && event.action == "completed")
            .and_then(|event| {
                event
                    .detail
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        let board = orchestration.goal_driver().snapshot();
        let Some(current) = board.current else {
            emit_headless_goal_summary(
                events,
                &goal_id,
                GoalStatus::Complete,
                completion_reason,
                turns_used,
                tokens_used,
                started,
            )?;
            return Ok(RuntimeCompletion::Goal {
                status: GoalStatus::Complete,
                session_id: None,
            });
        };

        match current.status {
            mycel_agent_runtime::GoalStatus::Paused => {
                emit_headless_goal_summary(
                    events,
                    &goal_id,
                    GoalStatus::Paused,
                    current.reason,
                    turns_used,
                    tokens_used,
                    started,
                )?;
                return Ok(RuntimeCompletion::Goal {
                    status: GoalStatus::Paused,
                    session_id: None,
                });
            }
            mycel_agent_runtime::GoalStatus::Blocked => {
                emit_headless_goal_summary(
                    events,
                    &goal_id,
                    GoalStatus::Blocked,
                    current.reason,
                    turns_used,
                    tokens_used,
                    started,
                )?;
                return Ok(RuntimeCompletion::Goal {
                    status: GoalStatus::Blocked,
                    session_id: None,
                });
            }
            mycel_agent_runtime::GoalStatus::Active => {
                let budget = orchestration
                    .record_goal_turn_usage(driven.outcome.usage.grand_total())
                    .map_err(|error| format!("could not record goal turn usage: {error}"))?;
                if budget.over_budget {
                    emit_headless_goal_summary(
                        events,
                        &goal_id,
                        GoalStatus::Blocked,
                        Some("goal budget exhausted".to_owned()),
                        turns_used,
                        tokens_used,
                        started,
                    )?;
                    return Ok(RuntimeCompletion::Goal {
                        status: GoalStatus::Blocked,
                        session_id: None,
                    });
                }
            }
        }
    }
}

fn emit_headless_goal_summary(
    events: &mut dyn HeadlessEventSink,
    goal_id: &str,
    status: GoalStatus,
    reason: Option<String>,
    turns_used: u64,
    tokens_used: u64,
    started: Instant,
) -> Result<(), String> {
    events
        .emit(HeadlessEvent::GoalSummary {
            goal_id: Some(goal_id.to_owned()),
            status: Some(
                match status {
                    GoalStatus::Complete => "complete",
                    GoalStatus::Blocked => "blocked",
                    GoalStatus::Paused => "paused",
                }
                .to_owned(),
            ),
            reason,
            turns_used: Some(turns_used),
            tokens_used: Some(tokens_used),
            wall_clock_ms: Some(started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)),
        })
        .map_err(|error| error.to_string())
}

fn with_session(completion: RuntimeCompletion, session_id: String) -> RuntimeCompletion {
    match completion {
        RuntimeCompletion::Success { .. } => RuntimeCompletion::success_with_session(session_id),
        RuntimeCompletion::Failure => RuntimeCompletion::Failure,
        RuntimeCompletion::Goal { status, .. } => RuntimeCompletion::Goal {
            status,
            session_id: Some(session_id),
        },
        RuntimeCompletion::Signal(signal) => RuntimeCompletion::Signal(signal),
    }
}

struct DrivenTurn {
    completion: RuntimeCompletion,
    outcome: TurnOutcome,
}

async fn drive_turn(
    engine: &TurnEngine,
    session: &SessionHandle,
    input: TurnInput,
    events: &mut dyn HeadlessEventSink,
) -> Result<DrivenTurn, String> {
    let mut receiver = session.subscribe();
    let turn = engine.run_turn(session, input, CancellationToken::new());
    tokio::pin!(turn);
    let mut terminal_goal = None;
    let outcome = loop {
        tokio::select! {
            biased;
            event = receiver.recv() => match event {
                Ok(event) => project_event(event.event, events, &mut terminal_goal)?,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                    session.cancel();
                    return Err(format!("headless event stream lagged by {count} events"));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    session.cancel();
                    return Err("headless event stream closed before the turn completed".to_owned());
                }
            },
            outcome = &mut turn => break outcome.map_err(|error| error.to_string())?,
        }
    };
    let completion = if let Some(status) = terminal_goal {
        RuntimeCompletion::Goal {
            status,
            session_id: None,
        }
    } else {
        match outcome.reason {
            TurnOutcomeReason::Completed
            | TurnOutcomeReason::MaxTokens
            | TurnOutcomeReason::Filtered
            | TurnOutcomeReason::ToolStopped => RuntimeCompletion::success(),
            TurnOutcomeReason::Paused => {
                return Err("provider paused the turn before completion".to_owned())
            }
            TurnOutcomeReason::Aborted => return Err("turn was aborted".to_owned()),
        }
    };
    Ok(DrivenTurn {
        completion,
        outcome,
    })
}

fn project_event(
    event: AgentEvent,
    events: &mut dyn HeadlessEventSink,
    terminal_goal: &mut Option<GoalStatus>,
) -> Result<(), String> {
    let projected = match event {
        AgentEvent::TurnStepStarted { .. } => Some(HeadlessEvent::StepStarted),
        AgentEvent::AssistantDelta { delta, .. } => Some(HeadlessEvent::AssistantDelta(delta)),
        AgentEvent::ThinkingDelta { delta, .. } => Some(HeadlessEvent::ThinkingDelta(delta)),
        AgentEvent::ToolCallStarted {
            tool_call_id,
            name,
            args,
            ..
        } => Some(HeadlessEvent::ToolCall {
            id: tool_call_id,
            name,
            arguments: args,
        }),
        AgentEvent::ToolCallDelta {
            tool_call_id,
            name,
            arguments_part,
            ..
        } => Some(HeadlessEvent::ToolCallDelta {
            id: tool_call_id,
            name,
            arguments_part,
        }),
        AgentEvent::ToolResult {
            tool_call_id,
            output,
            ..
        } => Some(HeadlessEvent::ToolResult {
            id: tool_call_id,
            output,
        }),
        AgentEvent::HookResult {
            hook_event,
            content,
            blocked,
            ..
        } => Some(HeadlessEvent::HookResult {
            hook_event,
            content,
            blocked: blocked.unwrap_or(false),
        }),
        AgentEvent::TurnStepRetrying {
            failed_attempt,
            next_attempt,
            max_attempts,
            delay_ms,
            error_name,
            error_message,
            status_code,
            ..
        } => Some(HeadlessEvent::Retrying(RetryMetadata {
            failed_attempt: checked_attempt(failed_attempt)?,
            next_attempt: checked_attempt(next_attempt)?,
            max_attempts: checked_attempt(max_attempts)?,
            delay_ms,
            error_name,
            error_message,
            status_code,
        })),
        AgentEvent::TurnStepCompleted { .. } => Some(HeadlessEvent::StepCompleted),
        AgentEvent::Warning { message, code } => Some(HeadlessEvent::Progress(match code {
            Some(code) => format!("warning [{code}]: {message}"),
            None => format!("warning: {message}"),
        })),
        AgentEvent::Error { error } => Some(HeadlessEvent::Progress(format!(
            "error [{}]: {}",
            error.name.unwrap_or_else(|| format!("{:?}", error.code)),
            error.message
        ))),
        AgentEvent::ToolProgress { update, .. } | AgentEvent::ShellOutput { update, .. } => {
            update.text.map(HeadlessEvent::Progress)
        }
        AgentEvent::TurnStepInterrupted {
            reason, message, ..
        } => Some(HeadlessEvent::Progress(match message {
            Some(message) => format!("turn interrupted ({reason}): {message}"),
            None => format!("turn interrupted ({reason})"),
        })),
        AgentEvent::GoalUpdated { snapshot, .. } => {
            if let Some(snapshot) = snapshot {
                *terminal_goal = match snapshot.status {
                    ProtocolGoalStatus::Complete => Some(GoalStatus::Complete),
                    ProtocolGoalStatus::Blocked => Some(GoalStatus::Blocked),
                    ProtocolGoalStatus::Paused => Some(GoalStatus::Paused),
                    ProtocolGoalStatus::Active => None,
                };
                Some(HeadlessEvent::GoalSummary {
                    goal_id: Some(snapshot.goal_id),
                    status: Some(goal_status_name(snapshot.status).to_owned()),
                    reason: snapshot.terminal_reason,
                    turns_used: Some(snapshot.turns_used),
                    tokens_used: Some(snapshot.tokens_used),
                    wall_clock_ms: Some(snapshot.wall_clock_ms),
                })
            } else {
                *terminal_goal = None;
                Some(HeadlessEvent::GoalSummary {
                    goal_id: None,
                    status: None,
                    reason: None,
                    turns_used: None,
                    tokens_used: None,
                    wall_clock_ms: None,
                })
            }
        }
        AgentEvent::TurnEnded {
            reason: TurnEndReason::Failed | TurnEndReason::Blocked,
            error,
            ..
        } => error.map(|error| HeadlessEvent::Progress(format!("turn failed: {}", error.message))),
        AgentEvent::TurnEnded { .. }
        | AgentEvent::AgentStatusUpdated { .. }
        | AgentEvent::SessionMetaUpdated { .. }
        | AgentEvent::SkillActivated { .. }
        | AgentEvent::PluginCommandActivated { .. }
        | AgentEvent::TurnStarted { .. }
        | AgentEvent::SubagentSpawned { .. }
        | AgentEvent::SubagentStarted { .. }
        | AgentEvent::SubagentSuspended { .. }
        | AgentEvent::SubagentCompleted { .. }
        | AgentEvent::SubagentFailed { .. }
        | AgentEvent::CompactionStarted { .. }
        | AgentEvent::CompactionBlocked { .. }
        | AgentEvent::CompactionCancelled
        | AgentEvent::CompactionCompleted { .. }
        | AgentEvent::BackgroundTaskStarted { .. }
        | AgentEvent::BackgroundTaskTerminated { .. }
        | AgentEvent::CronFired { .. }
        | AgentEvent::ToolListUpdated { .. }
        | AgentEvent::McpServerStatus { .. }
        | AgentEvent::ShellStarted { .. } => None,
    };
    if let Some(projected) = projected {
        events.emit(projected).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn checked_attempt(value: u64) -> Result<u32, String> {
    value
        .try_into()
        .map_err(|_| format!("retry attempt value {value} exceeds u32"))
}

const fn goal_status_name(status: ProtocolGoalStatus) -> &'static str {
    match status {
        ProtocolGoalStatus::Active => "active",
        ProtocolGoalStatus::Paused => "paused",
        ProtocolGoalStatus::Blocked => "blocked",
        ProtocolGoalStatus::Complete => "complete",
    }
}

struct RegistryTurnProvider {
    registry: Arc<ProviderRegistry>,
    provider_id: String,
    model_id: String,
}

impl TurnProvider for RegistryTurnProvider {
    fn name(&self) -> &str {
        &self.provider_id
    }

    fn model(&self) -> &str {
        &self.model_id
    }

    fn complete<'a>(
        &'a self,
        request: ProviderRequest,
        cancellation: CancellationToken,
    ) -> TurnProviderFuture<'a> {
        Box::pin(async move {
            let mut stream = tokio::select! {
                _ = cancellation.cancelled() => return Err(cancelled_provider_error()),
                stream = self.registry.stream(&request) => stream?,
            };
            let mut assembler = StreamAssembler::default();
            loop {
                let event = tokio::select! {
                    _ = cancellation.cancelled() => return Err(cancelled_provider_error()),
                    event = stream.next() => event,
                };
                let Some(event) = event else {
                    break;
                };
                assembler.push(event?).map_err(protocol_provider_error)?;
            }
            assembler.finish().map_err(protocol_provider_error)
        })
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancellation: CancellationToken,
        sink: &'a mut dyn TurnProviderStreamSink,
    ) -> TurnProviderStreamFuture<'a> {
        Box::pin(async move {
            let mut stream = tokio::select! {
                _ = cancellation.cancelled() => return Err(cancelled_provider_error()),
                stream = self.registry.stream(&request) => stream?,
            };
            loop {
                let event = tokio::select! {
                    _ = cancellation.cancelled() => return Err(cancelled_provider_error()),
                    event = stream.next() => event,
                };
                let Some(event) = event else {
                    break;
                };
                sink.push(event?)?;
            }
            Ok(())
        })
    }
}

fn protocol_provider_error(error: mycel_agent_protocol::ProtocolError) -> ProviderError {
    ProviderError::new(ProviderErrorKind::MalformedResponse, error.to_string())
}

fn cancelled_provider_error() -> ProviderError {
    ProviderError::new(ProviderErrorKind::Cancelled, "provider request cancelled")
}

pub(crate) fn parse_config(source: &str) -> Result<MycelConfig, String> {
    let value: toml::Value = toml::from_str(source).map_err(|error| error.to_string())?;
    let value = serde_json::to_value(value).map_err(|error| error.to_string())?;
    let normalized = normalize_config_value(value, NormalizeMode::Structured);
    let config: MycelConfig =
        serde_json::from_value(normalized).map_err(|error| error.to_string())?;
    config
        .validate_runtime()
        .map_err(|error| error.to_string())?;
    Ok(config)
}

#[derive(Clone, Copy)]
enum NormalizeMode {
    Structured,
    DynamicStructured,
    Opaque,
}

fn normalize_config_value(value: Value, mode: NormalizeMode) -> Value {
    match value {
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| normalize_config_value(value, mode))
                .collect(),
        ),
        Value::Object(values) => {
            let mut normalized = serde_json::Map::new();
            for (key, value) in values {
                let (key, child_mode) = match mode {
                    NormalizeMode::Opaque => (key, NormalizeMode::Opaque),
                    NormalizeMode::DynamicStructured => (key, NormalizeMode::Structured),
                    NormalizeMode::Structured => {
                        let key = if key == "fail_mode" {
                            key
                        } else {
                            snake_to_camel(&key)
                        };
                        let child_mode = match key.as_str() {
                            "providers" | "models" => NormalizeMode::DynamicStructured,
                            "experimental" | "raw" | "env" | "customHeaders" | "source"
                            | "headers" => NormalizeMode::Opaque,
                            _ => NormalizeMode::Structured,
                        };
                        (key, child_mode)
                    }
                };
                normalized.insert(key, normalize_config_value(value, child_mode));
            }
            Value::Object(normalized)
        }
        value => value,
    }
}

fn snake_to_camel(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut uppercase = false;
    for character in value.chars() {
        if character == '_' {
            uppercase = true;
        } else if uppercase {
            output.extend(character.to_uppercase());
            uppercase = false;
        } else {
            output.push(character);
        }
    }
    output
}

fn nonempty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn nonempty_os_path(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    value.and_then(|value| (!value.is_empty()).then(|| PathBuf::from(value)))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashMap, VecDeque},
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Mutex,
        },
        thread,
        time::Duration,
    };

    use clap::Parser;
    use futures_util::stream;
    use mycel_agent_protocol::{
        GoalBudgetReport, GoalSnapshot, GoalStatus as ProtocolGoalStatus, SessionSummary,
        ToolUpdate,
    };
    use mycel_agent_runtime::{
        AgentId, McpConnectedTransport, McpFuture, McpHttpConnectRequest, McpPeer, McpRequest,
        McpRequestError, McpStdioConnectRequest, McpTransportError, McpTransportEvent,
        McpTransportEvents, ToolCallId, ToolHookInput, ToolInvocation, ToolPrepareContext,
        ToolUpdateSink,
    };
    use mycel_providers::{HttpRequest, HttpResponse, TransportFuture};
    use tempfile::TempDir;

    use super::*;
    use crate::{
        cli::{OutputFormat, ProviderArgs, ProviderCommand},
        headless::HeadlessError,
        terminal::{
            BackendEvent, TerminalBackend, DISABLE_BRACKETED_PASTE, LEAVE_ALTERNATE_SCREEN,
        },
        tui::TranscriptFrame,
    };

    #[test]
    fn display_home_path_collapses_home_prefix() {
        let home = Path::new("/Users/joe");
        assert_eq!(
            display_home_path(Path::new("/Users/joe/dev/mycoforge"), Some(home)),
            "~/dev/mycoforge"
        );
        assert_eq!(display_home_path(home, Some(home)), "~");
        assert_eq!(
            display_home_path(Path::new("/etc/hosts"), Some(home)),
            "/etc/hosts"
        );
        assert_eq!(display_home_path(Path::new("/tmp/x"), None), "/tmp/x");
    }

    #[derive(Default)]
    struct TestEnvironment(Mutex<HashMap<String, String>>);

    impl RuntimeEnvironment for TestEnvironment {
        fn get(&self, key: &str) -> Option<String> {
            self.0.lock().expect("environment").get(key).cloned()
        }
    }

    impl McpEnvironment for TestEnvironment {
        fn get(&self, key: &str) -> Option<String> {
            self.0.lock().expect("environment").get(key).cloned()
        }
    }

    struct FixedHome(PathBuf);

    impl HomeLocator for FixedHome {
        fn mycel_home(&self) -> Result<PathBuf, String> {
            Ok(self.0.clone())
        }
    }

    struct RecordingConfig {
        source: String,
        paths: Mutex<Vec<PathBuf>>,
    }

    struct FixedPicker {
        selected: Option<String>,
        seen: Mutex<Vec<SessionSummary>>,
    }

    impl SessionPickerPort for FixedPicker {
        fn choose(
            &self,
            sessions: &[SessionSummary],
            _current_work_dir: &Path,
        ) -> Result<Option<String>, String> {
            self.seen
                .lock()
                .expect("picker sessions")
                .extend_from_slice(sessions);
            Ok(self.selected.clone())
        }
    }

    impl ConfigSource for RecordingConfig {
        fn read_to_string(&self, path: &Path) -> io::Result<String> {
            self.paths.lock().expect("paths").push(path.to_owned());
            Ok(self.source.clone())
        }
    }

    #[derive(Default)]
    struct ScriptedTransport {
        requests: Mutex<Vec<HttpRequest>>,
        responses: Mutex<VecDeque<(Duration, Vec<String>)>>,
        requests_started: Arc<AtomicUsize>,
    }

    impl ScriptedTransport {
        fn respond(&self, text: &str) {
            self.respond_after(Duration::ZERO, text);
        }

        fn respond_after(&self, delay: Duration, text: &str) {
            self.responses.lock().expect("responses").push_back((
                delay,
                vec![
                    format!(
                        "data: {{\"id\":\"chat\",\"choices\":[{{\"delta\":{{\"content\":{}}},\"finish_reason\":\"stop\"}}]}}\n\n",
                        serde_json::to_string(text).expect("text")
                    ),
                    "data: [DONE]\n\n".to_owned(),
                ],
            ));
        }

        fn request_counter(&self) -> Arc<AtomicUsize> {
            Arc::clone(&self.requests_started)
        }

        fn respond_tool_call(&self, id: &str, name: &str, arguments: Value) {
            let id = serde_json::to_string(id).expect("tool id");
            let name = serde_json::to_string(name).expect("tool name");
            let arguments = serde_json::to_string(&arguments.to_string()).expect("tool arguments");
            self.responses.lock().expect("responses").push_back((
                Duration::ZERO,
                vec![
                    format!(
                        "data: {{\"id\":\"chat\",\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":{id},\"function\":{{\"name\":{name},\"arguments\":{arguments}}}}}]}},\"finish_reason\":\"tool_calls\"}}]}}\n\n"
                    ),
                    "data: [DONE]\n\n".to_owned(),
                ],
            ));
        }
    }

    impl HttpTransport for ScriptedTransport {
        fn send<'a>(&'a self, request: HttpRequest) -> TransportFuture<'a> {
            self.requests.lock().expect("requests").push(request);
            self.requests_started.fetch_add(1, Ordering::SeqCst);
            let (delay, chunks) = self
                .responses
                .lock()
                .expect("responses")
                .pop_front()
                .expect("scripted response");
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                let body = chunks
                    .into_iter()
                    .map(|chunk| Ok::<bytes::Bytes, mycel_providers::TransportError>(chunk.into()));
                Ok(HttpResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: Box::pin(stream::iter(body)),
                })
            })
        }
    }

    struct PendingMcpEvents;

    impl McpTransportEvents for PendingMcpEvents {
        fn next<'a>(&'a mut self) -> McpFuture<'a, Option<McpTransportEvent>> {
            Box::pin(std::future::pending())
        }
    }

    struct ScriptedMcpPeer {
        responses: Mutex<VecDeque<Value>>,
        requests: Mutex<Vec<McpRequest>>,
        close_count: AtomicUsize,
    }

    impl ScriptedMcpPeer {
        fn modern_with_tool() -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(VecDeque::from([
                    serde_json::json!({
                        "resultType":"complete",
                        "supportedVersions":["2026-07-28"],
                        "capabilities":{"tools":{"listChanged":false}},
                        "_meta":{"io.modelcontextprotocol/serverInfo":{
                            "name":"production-test",
                            "version":"1"
                        }}
                    }),
                    serde_json::json!({
                        "tools":[{
                            "name":"ping",
                            "description":"production wiring test tool",
                            "inputSchema":{"type":"object","additionalProperties":false}
                        }]
                    }),
                ])),
                requests: Mutex::new(Vec::new()),
                close_count: AtomicUsize::new(0),
            })
        }
    }

    impl McpPeer for ScriptedMcpPeer {
        fn request<'a>(
            &'a self,
            request: McpRequest,
            _cancellation: &'a CancellationToken,
        ) -> McpFuture<'a, Result<Value, McpRequestError>> {
            self.requests.lock().expect("MCP requests").push(request);
            let response = self
                .responses
                .lock()
                .expect("MCP responses")
                .pop_front()
                .ok_or_else(|| {
                    McpRequestError::Transport(McpTransportError::Failed(
                        "unexpected production MCP request".to_owned(),
                    ))
                });
            Box::pin(async move { response })
        }

        fn notify<'a>(
            &'a self,
            _request: McpRequest,
        ) -> McpFuture<'a, Result<(), McpRequestError>> {
            Box::pin(async { Ok(()) })
        }

        fn close<'a>(&'a self) -> McpFuture<'a, Result<(), McpTransportError>> {
            self.close_count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    struct OneHttpMcpConnector {
        peer: Arc<ScriptedMcpPeer>,
        connected: AtomicBool,
    }

    impl OneHttpMcpConnector {
        fn new(peer: Arc<ScriptedMcpPeer>) -> Self {
            Self {
                peer,
                connected: AtomicBool::new(false),
            }
        }
    }

    impl McpTransportConnector for OneHttpMcpConnector {
        fn connect_stdio<'a>(
            &'a self,
            _request: McpStdioConnectRequest,
            _cancellation: &'a CancellationToken,
        ) -> McpFuture<'a, Result<McpConnectedTransport, McpTransportError>> {
            Box::pin(async {
                Err(McpTransportError::Failed(
                    "unexpected MCP stdio connection".to_owned(),
                ))
            })
        }

        fn connect_streamable_http<'a>(
            &'a self,
            _request: McpHttpConnectRequest,
            _cancellation: &'a CancellationToken,
        ) -> McpFuture<'a, Result<McpConnectedTransport, McpTransportError>> {
            let first = !self.connected.swap(true, Ordering::SeqCst);
            let peer: Arc<dyn McpPeer> = self.peer.clone();
            Box::pin(async move {
                if !first {
                    return Err(McpTransportError::Failed(
                        "duplicate production MCP connection".to_owned(),
                    ));
                }
                Ok(McpConnectedTransport {
                    peer,
                    events: Box::new(PendingMcpEvents),
                })
            })
        }
    }

    struct TestMcpConnectorFactory {
        calls: AtomicUsize,
        connector: Option<Arc<dyn McpTransportConnector>>,
    }

    impl TestMcpConnectorFactory {
        fn fail_if_called() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                connector: None,
            })
        }

        fn fixed(connector: Arc<dyn McpTransportConnector>) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                connector: Some(connector),
            })
        }
    }

    impl McpConnectorFactory for TestMcpConnectorFactory {
        fn create(&self, _mycel_home: &Path) -> Result<Arc<dyn McpTransportConnector>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.connector
                .clone()
                .ok_or_else(|| "empty MCP config must not create a connector".to_owned())
        }
    }

    #[derive(Default)]
    struct PendingTransport {
        requests: Mutex<Vec<HttpRequest>>,
    }

    impl HttpTransport for PendingTransport {
        fn send<'a>(&'a self, request: HttpRequest) -> TransportFuture<'a> {
            self.requests.lock().expect("requests").push(request);
            Box::pin(async move {
                let body =
                    stream::pending::<Result<bytes::Bytes, mycel_providers::TransportError>>();
                Ok(HttpResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: Box::pin(body),
                })
            })
        }
    }

    #[derive(Default)]
    struct CollectingSink(Vec<HeadlessEvent>);

    impl HeadlessEventSink for CollectingSink {
        fn emit(&mut self, event: HeadlessEvent) -> Result<(), HeadlessError> {
            self.0.push(event);
            Ok(())
        }
    }

    struct MemoryBackend {
        events: VecDeque<BackendEvent>,
        output: Arc<Mutex<Vec<u8>>>,
        restored: Arc<AtomicBool>,
        size: TerminalSize,
        writes: usize,
        fail_write_at: Option<usize>,
        emitted_events: usize,
        request_wait: Option<(usize, Arc<AtomicUsize>, usize)>,
        path_wait: Option<(usize, PathBuf)>,
        output_waits: VecDeque<(usize, Vec<u8>, Option<Instant>)>,
    }

    impl MemoryBackend {
        fn scripted(events: impl IntoIterator<Item = BackendEvent>) -> Self {
            Self {
                events: events.into_iter().collect(),
                output: Arc::new(Mutex::new(Vec::new())),
                restored: Arc::new(AtomicBool::new(false)),
                size: TerminalSize::new(80, 24),
                writes: 0,
                fail_write_at: None,
                emitted_events: 0,
                request_wait: None,
                path_wait: None,
                output_waits: VecDeque::new(),
            }
        }

        fn wait_after_events_for_requests(
            mut self,
            emitted_events: usize,
            requests: Arc<AtomicUsize>,
            expected: usize,
        ) -> Self {
            self.request_wait = Some((emitted_events, requests, expected));
            self
        }

        fn wait_after_events_for_path(mut self, emitted_events: usize, path: PathBuf) -> Self {
            self.path_wait = Some((emitted_events, path));
            self
        }

        fn wait_after_events_for_output(
            mut self,
            emitted_events: usize,
            needle: impl Into<Vec<u8>>,
        ) -> Self {
            self.output_waits
                .push_back((emitted_events, needle.into(), None));
            self
        }
    }

    impl TerminalBackend for MemoryBackend {
        type SavedMode = ();

        fn capture_mode(&mut self) -> io::Result<Self::SavedMode> {
            Ok(())
        }

        fn enable_raw_mode(&mut self, _saved: &Self::SavedMode) -> io::Result<()> {
            Ok(())
        }

        fn restore_mode(&mut self, _saved: &Self::SavedMode) -> io::Result<()> {
            self.restored.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn install_signal_handlers(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn uninstall_signal_handlers(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn write_output(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.writes += 1;
            self.output.lock().expect("terminal output").extend(bytes);
            if self.fail_write_at == Some(self.writes) {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected terminal write failure",
                ));
            }
            Ok(())
        }

        fn flush_output(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn terminal_size(&mut self) -> io::Result<TerminalSize> {
            Ok(self.size)
        }

        fn next_event(&mut self, timeout: Option<Duration>) -> io::Result<BackendEvent> {
            if let Some((after, requests, expected)) = &self.request_wait {
                if self.emitted_events >= *after {
                    // Bounded like path_wait/output_waits below: an unbounded
                    // spin here hung the whole CI test binary (no output, no
                    // failure) when the expected provider request never came.
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while requests.load(Ordering::SeqCst) < *expected {
                        if Instant::now() >= deadline {
                            return Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                format!(
                                    "timed out waiting for {expected} provider request(s), saw {}",
                                    requests.load(Ordering::SeqCst)
                                ),
                            ));
                        }
                        thread::sleep(Duration::from_millis(1));
                    }
                    self.request_wait = None;
                }
            }
            if let Some((after, path)) = &self.path_wait {
                if self.emitted_events >= *after {
                    let deadline = std::time::Instant::now() + Duration::from_secs(2);
                    while !path.exists() {
                        if std::time::Instant::now() >= deadline {
                            return Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                format!("timed out waiting for {}", path.display()),
                            ));
                        }
                        thread::sleep(Duration::from_millis(1));
                    }
                    self.path_wait = None;
                }
            }
            if let Some((after, needle, deadline)) = self.output_waits.front_mut() {
                if self.emitted_events >= *after {
                    let found = self
                        .output
                        .lock()
                        .expect("terminal output")
                        .windows(needle.len())
                        .any(|window| window == needle.as_slice());
                    if found {
                        self.output_waits.pop_front();
                    } else {
                        let deadline =
                            deadline.get_or_insert_with(|| Instant::now() + Duration::from_secs(2));
                        if Instant::now() >= *deadline {
                            return Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                format!(
                                    "timed out waiting for terminal output {:?}",
                                    String::from_utf8_lossy(needle)
                                ),
                            ));
                        }
                        thread::sleep(timeout.unwrap_or(Duration::from_millis(1)));
                        return Ok(BackendEvent::Timeout);
                    }
                }
            }
            let event = self.events.pop_front().unwrap_or(BackendEvent::EndOfInput);
            self.emitted_events = self.emitted_events.saturating_add(1);
            if event == BackendEvent::Timeout {
                thread::sleep(timeout.unwrap_or(Duration::from_millis(1)));
            }
            Ok(event)
        }
    }

    #[derive(Default)]
    struct CapturingToolRegistryBuilder {
        registry: ToolRegistry,
    }

    impl ToolRegistryBuilder for CapturingToolRegistryBuilder {
        fn build(
            &self,
            _working_dir: &Path,
            _additional_dirs: &[PathBuf],
            _allowed_files: &[PathBuf],
            _foreground_processes: Option<Arc<dyn ForegroundProcessPort>>,
        ) -> Result<ToolRegistry, String> {
            Ok(self.registry.clone())
        }
    }

    #[derive(Default)]
    struct CapturingLocalToolRegistryBuilder {
        registry: ToolRegistry,
    }

    impl ToolRegistryBuilder for CapturingLocalToolRegistryBuilder {
        fn build(
            &self,
            working_dir: &Path,
            additional_dirs: &[PathBuf],
            allowed_files: &[PathBuf],
            foreground_processes: Option<Arc<dyn ForegroundProcessPort>>,
        ) -> Result<ToolRegistry, String> {
            let local = LocalToolConfig::new(working_dir, additional_dirs.iter())
                .map_err(|error| error.to_string())?
                .with_allowed_files(allowed_files.iter())
                .map_err(|error| error.to_string())?;
            register_local_builtins_with_process_port(&self.registry, local, foreground_processes)
                .map_err(|error| error.to_string())?;
            Ok(self.registry.clone())
        }
    }

    #[derive(Default)]
    struct NoToolUpdates;

    impl ToolUpdateSink for NoToolUpdates {
        fn emit(&self, _update: ToolUpdate) {}
    }

    async fn invoke_registered_tool(
        registry: &ToolRegistry,
        session_id: &SessionId,
        name: &str,
        arguments: Value,
    ) -> mycel_agent_protocol::ExecutableToolResult {
        let tool = registry.snapshot().get(name).expect("registered tool");
        let context = ToolPrepareContext {
            session_id: session_id.clone(),
            agent_id: AgentId::main(),
            turn_id: 1,
            tool_call_id: ToolCallId::new(format!("test-{name}")).expect("valid test tool call id"),
        };
        tool.validate_arguments(&arguments)
            .expect("valid tool arguments");
        tool.prepare(&arguments, &context).expect("prepare tool");
        tool.execute(ToolInvocation {
            context,
            arguments,
            cancellation: CancellationToken::new(),
            updates: Arc::new(NoToolUpdates),
        })
        .await
        .expect("execute tool")
    }

    fn config() -> String {
        r#"
default_model = "local"
default_permission_mode = "manual"

[providers.local]
type = "openai"
base_url = "http://127.0.0.1:11434/v1"
api_key = "test-key"
custom_headers = { X-Test = "yes" }

[models.local]
provider = "local"
model = "gpt-test"
max_context_size = 8192
max_output_size = 128
default_effort = "low"

[thinking]
effort = "high"

[loop_control]
max_steps_per_turn = 4
max_retries_per_step = 0

# A benign always-available PreToolUse hook, so these orchestration tests
# exercise the tool-flow-with-a-hook path without depending on an installed
# `mycel-gate` on PATH (which resolves on a dev box but not on CI, where a
# failed fail-closed hook would block every tool). The real gate binary is
# covered by the mycel-gate crate tests and the gate-contract/immunity e2e.
[[hooks]]
event = "PreToolUse"
command = "true"
fail_mode = "closed"
"#
        .to_owned()
    }

    fn prompt(session: SessionSelection) -> PromptRequest {
        PromptRequest {
            prompt: "hello".to_owned(),
            output_format: OutputFormat::Text,
            session,
            model: None,
            skills_dirs: Vec::new(),
            add_dirs: Vec::new(),
            goal: None,
        }
    }

    fn interactive(session: SessionSelection, permission: PermissionMode) -> InteractiveRequest {
        InteractiveRequest {
            session,
            permission,
            plan: false,
            model: None,
            skills_dirs: Vec::new(),
            add_dirs: Vec::new(),
        }
    }

    fn adapter(
        temp: &TempDir,
        source: Arc<RecordingConfig>,
        transport: Arc<ScriptedTransport>,
    ) -> ProductionRuntimeAdapter {
        ProductionRuntimeAdapter::with_components(
            Arc::new(FixedHome(temp.path().join("mycel"))),
            source,
            Arc::new(TestEnvironment::default()),
            transport,
        )
        .expect("adapter")
    }

    fn adapter_with_mcp(
        temp: &TempDir,
        source: Arc<RecordingConfig>,
        transport: Arc<ScriptedTransport>,
        connector_factory: Arc<dyn McpConnectorFactory>,
    ) -> ProductionRuntimeAdapter {
        let environment = Arc::new(TestEnvironment::default());
        ProductionRuntimeAdapter::with_components_and_services(
            Arc::new(FixedHome(temp.path().join("mycel"))),
            source,
            environment.clone(),
            transport,
            ProductionRuntimeServices::new(Arc::new(LocalToolRegistryBuilder))
                .with_mcp_services(connector_factory, environment),
        )
        .expect("adapter with MCP services")
    }

    fn write_test_mcp_config(temp: &TempDir) {
        let home = temp.path().join("mycel");
        fs::create_dir_all(&home).expect("MYCEL_HOME");
        fs::write(
            home.join(MCP_CONFIG_FILE),
            r#"{"mcpServers":{"fixture":{"transport":"http","url":"https://mcp.example.test/rpc"}}}"#,
        )
        .expect("MCP config");
    }

    fn approval_request(id: &str) -> ApprovalRequest {
        ApprovalRequest {
            tool_call_id: id.to_owned(),
            tool_name: "Bash".to_owned(),
            action: "run command".to_owned(),
            display: ToolInputDisplay::Generic {
                summary: "run test command".to_owned(),
                detail: None,
            },
        }
    }

    fn bash_permission_request(id: &str) -> mycel_agent_runtime::ToolPermissionRequest {
        mycel_agent_runtime::ToolPermissionRequest {
            turn_id: 1,
            tool_call_id: ToolCallId::new(id).expect("tool call id"),
            tool_name: "Bash".to_owned(),
            action: "run command".to_owned(),
            display: ToolInputDisplay::Generic {
                summary: "run echo ok".to_owned(),
                detail: Some(Value::String("echo ok".to_owned())),
            },
            approval_rule: Some("Bash(echo *)".to_owned()),
            rule_subject: Some("echo ok".to_owned()),
            exclusive_tool: None,
            plan_policy: mycel_agent_runtime::PlanPolicy::NotInPlan,
            create_goal_review: false,
            sensitive_file: false,
            git_control: false,
            git_cwd_write: false,
        }
    }

    fn question_request() -> QuestionRequest {
        QuestionRequest {
            request_id: mycel_agent_runtime::RequestId::generate(),
            agent_id: mycel_agent_runtime::AgentId::main(),
            questions: vec![
                mycel_agent_runtime::Question {
                    id: "color".to_owned(),
                    prompt: "Color?".to_owned(),
                    options: vec![
                        mycel_agent_runtime::QuestionOption {
                            label: "red".to_owned(),
                            description: None,
                        },
                        mycel_agent_runtime::QuestionOption {
                            label: "blue".to_owned(),
                            description: None,
                        },
                    ],
                    multiple: false,
                },
                mycel_agent_runtime::Question {
                    id: "targets".to_owned(),
                    prompt: "Targets?".to_owned(),
                    options: vec![
                        mycel_agent_runtime::QuestionOption {
                            label: "cli".to_owned(),
                            description: None,
                        },
                        mycel_agent_runtime::QuestionOption {
                            label: "sdk".to_owned(),
                            description: None,
                        },
                    ],
                    multiple: true,
                },
            ],
        }
    }

    fn apply_dialog_bytes(host: &mut DialogHost, decoder: &mut InputDecoder, bytes: &[u8]) {
        for event in decoder.feed(bytes) {
            host.apply(event);
        }
    }

    #[test]
    fn dialog_host_preserves_fifo_and_maps_all_response_shapes() {
        let (port, receiver) = interactive_dialog_channel();
        let approval = port.request_approval(approval_request("approval-1"));
        let question = port.ask(question_request());
        let feedback = port.request_approval(approval_request("approval-2"));
        let rejection = port.request_approval(approval_request("approval-3"));
        let mut host = DialogHost::new(Arc::clone(&port), receiver);
        let mut decoder = InputDecoder::default();

        host.poll();
        assert!(matches!(
            host.active.as_ref(),
            Some(ActiveDialog::Approval { request, .. }) if request.tool_call_id == "approval-1"
        ));
        apply_dialog_bytes(&mut host, &mut decoder, b"2");
        let approval = tokio::runtime::Runtime::new()
            .expect("executor")
            .block_on(approval)
            .expect("approval response");
        assert_eq!(approval.decision, ProtocolApprovalDecision::Approved);
        assert_eq!(approval.scope, Some(ApprovalScope::Session));

        assert!(matches!(
            host.active.as_ref(),
            Some(ActiveDialog::Question { request, .. }) if request.questions[0].id == "color"
        ));
        apply_dialog_bytes(&mut host, &mut decoder, b"2");
        apply_dialog_bytes(&mut host, &mut decoder, b" ");
        apply_dialog_bytes(&mut host, &mut decoder, b"\x1b[B\x1b[B\rdocs\r\t\r");
        let question = tokio::runtime::Runtime::new()
            .expect("executor")
            .block_on(question)
            .expect("question response");
        assert_eq!(question.answers.len(), 2);
        assert_eq!(question.answers[0].question_id, "color");
        assert_eq!(question.answers[0].selected_labels, ["blue"]);
        assert_eq!(question.answers[0].text, None);
        assert_eq!(question.answers[1].selected_labels, ["cli"]);
        assert_eq!(question.answers[1].text.as_deref(), Some("docs"));

        assert!(matches!(
            host.active.as_ref(),
            Some(ActiveDialog::Approval { request, .. }) if request.tool_call_id == "approval-2"
        ));
        apply_dialog_bytes(&mut host, &mut decoder, b"4needs review\r");
        let feedback = tokio::runtime::Runtime::new()
            .expect("executor")
            .block_on(feedback)
            .expect("feedback response");
        assert_eq!(feedback.decision, ProtocolApprovalDecision::Rejected);
        assert_eq!(feedback.feedback.as_deref(), Some("needs review"));

        assert!(matches!(
            host.active.as_ref(),
            Some(ActiveDialog::Approval { request, .. }) if request.tool_call_id == "approval-3"
        ));
        apply_dialog_bytes(&mut host, &mut decoder, b"3");
        let rejection = tokio::runtime::Runtime::new()
            .expect("executor")
            .block_on(rejection)
            .expect("rejection response");
        assert_eq!(rejection.decision, ProtocolApprovalDecision::Rejected);
        assert_eq!(rejection.feedback, None);
        assert!(!host.is_active());
    }

    #[test]
    fn dialog_shutdown_cancels_active_queued_and_future_requests() {
        let (port, receiver) = interactive_dialog_channel();
        let active = port.request_approval(approval_request("active"));
        let queued = port.ask(question_request());
        let mut host = DialogHost::new(Arc::clone(&port), receiver);
        host.poll();
        host.cancel_all("session switched");
        let executor = tokio::runtime::Runtime::new().expect("executor");
        assert!(executor
            .block_on(active)
            .expect_err("active cancelled")
            .message
            .contains("session switched"));
        assert!(executor
            .block_on(queued)
            .expect_err("queued cancelled")
            .message
            .contains("session switched"));
        assert!(executor
            .block_on(port.request_approval(approval_request("late")))
            .expect_err("closed host")
            .message
            .contains("closed"));
    }

    #[test]
    fn question_cancel_returns_a_null_response() {
        let (port, receiver) = interactive_dialog_channel();
        let response = port.ask(question_request());
        let mut host = DialogHost::new(Arc::clone(&port), receiver);
        host.poll();
        apply_dialog_bytes(&mut host, &mut InputDecoder::default(), &[0x03]);
        let response = tokio::runtime::Runtime::new()
            .expect("executor")
            .block_on(response)
            .expect("cancel response");
        assert!(response.answers.is_empty());
        assert!(!host.is_active());
    }

    #[test]
    fn manual_tool_authorization_waits_for_dialog_and_reuses_session_approval() {
        let temp = TempDir::new().expect("temp");
        let source = Arc::new(RecordingConfig {
            source: config(),
            paths: Mutex::new(Vec::new()),
        });
        let adapter = adapter(&temp, source, Arc::new(ScriptedTransport::default()));
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Manual))
            .expect("prepare");
        let receiver = prepared
            .dialog_receiver
            .lock()
            .expect("dialog receiver")
            .take()
            .expect("dialog receiver once");
        let mut host = DialogHost::new(Arc::clone(&prepared.dialog_port), receiver);
        let session = prepared.session.clone();
        let request = bash_permission_request("permission-call");
        let request_for_task = request.clone();
        let authorization = adapter
            .executor
            .spawn(async move { session.authorize_tool(&request_for_task).await });
        for _ in 0..100 {
            host.poll();
            if host.is_active() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            host.is_active(),
            "authorization must open an approval dialog"
        );
        assert!(
            !authorization.is_finished(),
            "tool authorization must block until the dialog answers"
        );

        apply_dialog_bytes(&mut host, &mut InputDecoder::default(), b"2");
        let authorization = adapter
            .executor
            .block_on(authorization)
            .expect("authorization task")
            .expect("authorization");
        assert_eq!(
            authorization.verdict,
            mycel_agent_runtime::PermissionVerdict::Allow
        );
        assert_eq!(
            authorization.remember_session_rule.as_deref(),
            Some("Bash(echo *)")
        );

        let reused = adapter
            .executor
            .block_on(prepared.session.authorize_tool(&request))
            .expect("reused session approval");
        assert_eq!(
            reused.matched_by,
            mycel_agent_runtime::PermissionMatch::SessionApproval
        );
        host.poll();
        assert!(!host.is_active(), "reused approval must not prompt again");
        adapter
            .executor
            .block_on(shutdown_orchestration(Some(
                prepared.orchestration.as_ref(),
            )))
            .expect("shut down orchestration");
        adapter
            .executor
            .block_on(shutdown_mcp(prepared.mcp.as_ref()))
            .expect("shut down MCP");
        adapter
            .executor
            .block_on(prepared.session.close())
            .expect("close session");
    }

    #[test]
    fn production_retained_tools_reach_dialog_and_canonical_session_state() {
        let temp = TempDir::new().expect("temp");
        let skills_dir = temp.path().join("explicit-skills");
        fs::create_dir_all(skills_dir.join("review")).expect("skill directory");
        fs::write(
            skills_dir.join("review/SKILL.md"),
            "---\nname: review\ndescription: review changes\n---\nReview $ARGUMENTS for session $SESSION_ID.\n",
        )
        .expect("skill file");
        let media_path = temp.path().join("sample.png");
        fs::write(&media_path, b"\x89PNG\r\n\x1a\n").expect("media fixture");
        let media_path = fs::canonicalize(media_path).expect("canonical media fixture");
        let source = Arc::new(RecordingConfig {
            source: config().replace(
                "max_output_size = 128",
                "max_output_size = 128\ncapabilities = [\"image_in\"]",
            ),
            paths: Mutex::new(Vec::new()),
        });
        let tools = Arc::new(CapturingToolRegistryBuilder::default());
        let adapter = ProductionRuntimeAdapter::with_components_and_tools(
            Arc::new(FixedHome(temp.path().join("mycel"))),
            source,
            Arc::new(TestEnvironment::default()),
            Arc::new(ScriptedTransport::default()),
            tools.clone(),
        )
        .expect("adapter");
        let mut request = interactive(SessionSelection::New, PermissionMode::Manual);
        request.skills_dirs.push(skills_dir);
        request.add_dirs.push(temp.path().to_path_buf());
        let prepared = adapter.prepare_interactive(&request).expect("prepare");
        let tool_snapshot = tools.registry.snapshot();
        let names = tool_snapshot
            .definitions()
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "Agent",
                "AgentSwarm",
                "AskUserQuestion",
                "CreateGoal",
                "CronCreate",
                "CronDelete",
                "CronList",
                "EnterPlanMode",
                "ExitPlanMode",
                "GetGoal",
                "Hyphae",
                "ReadMediaFile",
                "SetGoalBudget",
                "Skill",
                "TaskDetach",
                "TaskList",
                "TaskOutput",
                "TaskStop",
                "TodoList",
                "UpdateGoal",
                "Workflow",
            ]
        );

        let receiver = prepared
            .dialog_receiver
            .lock()
            .expect("dialog receiver")
            .take()
            .expect("dialog receiver once");
        let mut host = DialogHost::new(Arc::clone(&prepared.dialog_port), receiver);
        let registry = tools.registry.clone();
        let session_id = prepared.session.id().clone();
        let question = adapter.executor.spawn(async move {
            invoke_registered_tool(
                &registry,
                &session_id,
                "AskUserQuestion",
                serde_json::json!({
                    "questions": [{
                        "question": "Choose?",
                        "header": "Choice",
                        "options": [{"label": "A"}, {"label": "B"}]
                    }]
                }),
            )
            .await
        });
        for _ in 0..100 {
            host.poll();
            if host.is_active() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            host.is_active(),
            "AskUserQuestion must reach the dialog host"
        );
        apply_dialog_bytes(&mut host, &mut InputDecoder::default(), b"1\r");
        let question = adapter.executor.block_on(question).expect("question task");
        assert!(!question.is_error);

        let todos = adapter.executor.block_on(invoke_registered_tool(
            &tools.registry,
            prepared.session.id(),
            "TodoList",
            serde_json::json!({
                "todos": [{"title": "keep canonical state", "status": "in_progress"}]
            }),
        ));
        assert!(!todos.is_error);
        let entered = adapter.executor.block_on(invoke_registered_tool(
            &tools.registry,
            prepared.session.id(),
            "EnterPlanMode",
            serde_json::json!({}),
        ));
        assert!(!entered.is_error);
        let skill = adapter.executor.block_on(invoke_registered_tool(
            &tools.registry,
            prepared.session.id(),
            "Skill",
            serde_json::json!({"skill":"review","args":"the patch"}),
        ));
        assert!(!skill.is_error);
        let media = adapter.executor.block_on(invoke_registered_tool(
            &tools.registry,
            prepared.session.id(),
            "ReadMediaFile",
            serde_json::json!({"path":media_path.to_string_lossy()}),
        ));
        assert!(!media.is_error);
        assert!(matches!(
            media.output,
            mycel_agent_protocol::ExecutableToolOutput::Parts(_)
        ));
        let outside = TempDir::new().expect("outside media root");
        let outside_media = outside.path().join("outside.png");
        fs::write(&outside_media, b"\x89PNG\r\n\x1a\n").expect("outside media");
        let outside_media = fs::canonicalize(outside_media).expect("canonical outside media");
        let media_tool = tools
            .registry
            .snapshot()
            .get("ReadMediaFile")
            .expect("ReadMediaFile");
        let media_context = ToolPrepareContext {
            session_id: prepared.session.id().clone(),
            agent_id: AgentId::main(),
            turn_id: 2,
            tool_call_id: ToolCallId::new("test-outside-media").expect("tool call id"),
        };
        assert!(media_tool
            .prepare(
                &serde_json::json!({"path":outside_media.to_string_lossy()}),
                &media_context,
            )
            .is_err());

        let snapshot = adapter.executor.block_on(prepared.session.snapshot());
        assert!(snapshot.state.plan_mode);
        assert_eq!(
            snapshot.state.tool_store["todos"][0]["title"],
            "keep canonical state"
        );
        assert!(matches!(
            snapshot
                .state
                .context
                .history()
                .last()
                .and_then(|entry| entry.origin.as_ref()),
            Some(mycel_agent_protocol::PromptOrigin::SkillActivation { .. })
        ));
        adapter
            .executor
            .block_on(shutdown_orchestration(Some(
                prepared.orchestration.as_ref(),
            )))
            .expect("shut down orchestration");
        adapter
            .executor
            .block_on(shutdown_mcp(prepared.mcp.as_ref()))
            .expect("shut down MCP");
        adapter
            .executor
            .block_on(prepared.session.close())
            .expect("close session");
    }

    #[test]
    fn interactive_ecology_command_renders_without_provider_or_substrate_creation() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut backend = MemoryBackend::scripted([
            BackendEvent::Input(b"/gate\r".to_vec()),
            BackendEvent::EndOfInput,
        ]);
        backend.output = output.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("ecology command");

        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("Gate"));
        assert!(rendered.contains("Substrate db MISSING"));
        assert!(transport.requests.lock().expect("requests").is_empty());
        assert!(!temp.path().join("mycel/substrate/mycel.db").exists());
    }

    #[test]
    fn interactive_tui_settings_persist_reload_and_render_with_ansi_safe_width() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut backend = MemoryBackend::scripted([
            BackendEvent::Input(b"/theme light\r".to_vec()),
            BackendEvent::Input(b"/editor nvim\r".to_vec()),
            BackendEvent::Input(b"/reload-tui\r".to_vec()),
            BackendEvent::Input(b"/settings\r".to_vec()),
            BackendEvent::Input(vec![0x04]),
        ]);
        backend.output = output.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("TUI settings");

        let (saved, warning) = load_tui_config(&temp.path().join("mycel"));
        assert!(warning.is_none());
        assert_eq!(saved.theme, ThemeName::Light);
        assert_eq!(saved.editor_command.as_deref(), Some("nvim"));
        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("theme: light"));
        assert!(rendered.contains("editor: nvim"));
        // Frames render styled; the exact SGR encoding depends on the test
        // process's COLORTERM, so assert styling exists without pinning codes.
        assert!(rendered.contains("\x1b["));
        assert!(transport.requests.lock().expect("requests").is_empty());
    }

    #[test]
    fn configured_light_theme_warns_at_startup_and_named_themes_do_not() {
        for (theme, expect_warning) in [("light", true), ("amanita", false)] {
            let temp = TempDir::new().expect("temp");
            let home = temp.path().join("mycel");
            fs::create_dir_all(&home).expect("home");
            fs::write(home.join("tui.toml"), format!("theme = \"{theme}\"\n")).expect("tui config");
            let transport = Arc::new(ScriptedTransport::default());
            let adapter = adapter(
                &temp,
                Arc::new(RecordingConfig {
                    source: config(),
                    paths: Mutex::new(Vec::new()),
                }),
                transport.clone(),
            );
            let prepared = adapter
                .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
                .expect("prepare");
            let warned = prepared
                .warning
                .as_deref()
                .is_some_and(|warning| warning.contains(LIGHT_THEME_WARNING));
            assert_eq!(
                warned, expect_warning,
                "theme {theme}: {:?}",
                prepared.warning
            );
            let output = Arc::new(Mutex::new(Vec::new()));
            let mut backend = MemoryBackend::scripted([BackendEvent::Input(vec![0x04])]);
            backend.output = output.clone();
            let mut driver = TerminalDriver::new(backend);
            adapter
                .run_prepared_interactive(prepared, &mut driver)
                .expect("run");
            let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
            assert_eq!(
                rendered.contains("light theme is not supported"),
                expect_warning,
                "theme {theme} rendering"
            );
        }
    }

    #[test]
    fn seed_frames_carry_the_real_wall_clock() {
        let transcript =
            seed_transcript("session s · model m".to_owned(), Some("substrate warning"));
        let frames = transcript.frames();
        assert_eq!(frames.len(), 2);
        for frame in frames {
            assert_ne!(frame.at_ms, 0, "seed frames must not render as epoch 0");
        }
        // Both seed frames share one stamp so the gutter shows a single time.
        assert_eq!(frames[0].at_ms, frames[1].at_ms);
    }

    #[test]
    fn theme_command_recolors_the_live_view_through_the_cache() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut backend = MemoryBackend::scripted([
            BackendEvent::Input(b"/theme phosphor\r".to_vec()),
            BackendEvent::Input(vec![0x04]),
        ]);
        backend.output = output.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("run");
        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        // The renders after the command must carry phosphor's accent (#33ff66):
        // truecolor SGR or its 256-cube downgrade (index 84), depending on the
        // test environment's COLORTERM. A stale cached theme keeps painting
        // amanita and neither appears.
        assert!(
            rendered.contains("38;2;51;255;102") || rendered.contains("38;5;84"),
            "no phosphor accent in the post-/theme render"
        );
    }

    #[test]
    fn builtin_themes_color_frames_without_changing_terminal_width() {
        let frame = TranscriptFrame {
            kind: FrameKind::Assistant,
            text: "hello 界".to_owned(),
            streaming: false,
            tool_id: None,
            tool_status: None,
            entity_id: None,
            state: None,
            at_ms: 0,
        };
        let ctx = FrameCtx {
            width: 80,
            truecolor: true,
            spinner_phase: 0,
        };
        for theme_name in ["amanita", "phosphor"] {
            let theme = active_theme(&ThemeName::Named(theme_name.to_owned()));
            let lines = transcript_frame_lines(&frame, &theme, &ctx);
            for line in &lines {
                assert!(line.starts_with("\x1b["));
                assert!(line.contains("hello 界"));
                assert!(visible_width(line) <= 80);
            }
        }
    }

    #[test]
    fn interactive_btw_is_a_tool_free_followup_side_channel_and_cleans_ephemeral_state() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        transport.respond("first side answer");
        transport.respond("second side answer");
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        let output = Arc::new(Mutex::new(Vec::new()));
        let events = std::iter::once(BackendEvent::Input(b"/btw first question\r".to_vec()))
            .chain(std::iter::repeat_n(BackendEvent::Timeout, 8))
            .chain(std::iter::once(BackendEvent::Input(
                b"follow-up question\r".to_vec(),
            )))
            .chain(std::iter::repeat_n(BackendEvent::Timeout, 8))
            .chain([
                BackendEvent::Input(vec![0x1b]),
                BackendEvent::Input(vec![0x04]),
            ]);
        let mut backend = MemoryBackend::scripted(events);
        backend.output = output.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("BTW side channel");

        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("BTW"));
        assert!(rendered.contains("first side answer"));
        assert!(rendered.contains("second side answer"));
        let requests = transport.requests.lock().expect("requests");
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            let body: Value = serde_json::from_slice(&request.body).expect("request body");
            assert!(body["tools"].as_array().is_none_or(Vec::is_empty));
            let messages = body["messages"].as_array().expect("messages");
            assert!(messages.iter().any(|message| {
                message["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("side-channel conversation"))
            }));
        }
        let second: Value = serde_json::from_slice(&requests[1].body).expect("second request");
        assert!(second["messages"].as_array().is_some_and(|messages| {
            messages
                .iter()
                .any(|message| message["content"] == "first side answer")
        }));
        drop(requests);
        assert!(!temp.path().join("mycel/run/btw").exists());
    }

    #[test]
    fn interactive_btw_can_run_while_the_main_turn_continues() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        transport.respond_after(Duration::from_millis(200), "main answer");
        transport.respond("side answer while main runs");
        let requests_started = transport.request_counter();
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        let output = Arc::new(Mutex::new(Vec::new()));
        let events = [
            BackendEvent::Input(b"main question\r".to_vec()),
            BackendEvent::Input(b"/btw side question\r".to_vec()),
        ]
        .into_iter()
        .chain(std::iter::repeat_n(BackendEvent::Timeout, 12))
        .chain([
            BackendEvent::Input(vec![0x1b]),
            BackendEvent::Input(vec![0x04]),
        ]);
        let mut backend =
            MemoryBackend::scripted(events).wait_after_events_for_requests(1, requests_started, 1);
        backend.output = output.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("parallel main and BTW turns");

        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("main answer"));
        assert!(rendered.contains("side answer while main runs"));
        let requests = transport.requests.lock().expect("requests");
        assert_eq!(requests.len(), 2);
        let side: Value = serde_json::from_slice(&requests[1].body).expect("side request");
        assert!(side["tools"].as_array().is_none_or(Vec::is_empty));
    }

    #[cfg(unix)]
    #[test]
    fn interactive_ctrl_g_restores_terminal_runs_configured_editor_and_restores_draft() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("temp");
        let editor = temp.path().join("editor.sh");
        fs::write(&editor, "#!/bin/sh\nprintf 'edited draft\\n' > \"$1\"\n")
            .expect("editor script");
        fs::set_permissions(&editor, fs::Permissions::from_mode(0o700)).expect("editor mode");
        let environment = Arc::new(TestEnvironment::default());
        environment
            .0
            .lock()
            .expect("environment")
            .insert("EDITOR".to_owned(), editor.to_string_lossy().into_owned());
        let transport = Arc::new(ScriptedTransport::default());
        let adapter = ProductionRuntimeAdapter::with_components(
            Arc::new(FixedHome(temp.path().join("mycel"))),
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            environment,
            transport.clone(),
        )
        .expect("adapter");
        let output = Arc::new(Mutex::new(Vec::new()));
        let restored = Arc::new(AtomicBool::new(false));
        let mut backend = MemoryBackend::scripted([
            BackendEvent::Input(b"original draft\x07".to_vec()),
            BackendEvent::Input(vec![0x03]),
            BackendEvent::Input(vec![0x04]),
        ]);
        backend.output = output.clone();
        backend.restored = restored.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_interactive_with_terminal(
                &interactive(SessionSelection::New, PermissionMode::Auto),
                &mut driver,
            )
            .expect("external editor lifecycle");

        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("edited draft"));
        assert!(restored.load(Ordering::SeqCst));
        assert!(transport.requests.lock().expect("requests").is_empty());
    }

    #[test]
    fn interactive_shell_runs_through_the_governed_local_tool_without_provider() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut backend = MemoryBackend::scripted(
            std::iter::once(BackendEvent::Input(b"!printf shell-ok\r".to_vec()))
                .chain(std::iter::repeat_n(BackendEvent::Timeout, 4))
                .chain(std::iter::once(BackendEvent::Input(vec![0x04]))),
        );
        backend.output = output.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("interactive shell");

        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("!printf shell-ok"));
        assert!(rendered.contains("shell-ok"));
        assert!(transport.requests.lock().expect("requests").is_empty());
    }

    #[test]
    fn interactive_add_dir_updates_durable_session_roots_before_reprepare() {
        let temp = TempDir::new().expect("temp");
        let additional = temp.path().join("additional-workspace");
        fs::create_dir_all(&additional).expect("additional workspace");
        let additional = fs::canonicalize(additional).expect("canonical additional workspace");
        let transport = Arc::new(ScriptedTransport::default());
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        let session_id = prepared.session.id().as_str().to_owned();
        let command = format!("/add-dir {}\r", additional.display()).into_bytes();
        let mut driver = TerminalDriver::new(MemoryBackend::scripted([
            BackendEvent::Input(command),
            BackendEvent::EndOfInput,
        ]));

        let outcome = adapter
            .run_prepared_interactive_outcome(prepared, &mut driver)
            .expect("add directory transition");
        let PreparedInteractiveOutcome::Transition(transition) = outcome else {
            panic!("add-dir must reprepare the active session");
        };
        assert_eq!(
            transition.action,
            InteractiveSessionTransition::AddDir {
                path: additional.clone(),
                notice: format!(
                    "Added workspace directory for this session: {}",
                    additional.display()
                ),
            }
        );
        let indexed = SessionIndex::new(temp.path().join("mycel"))
            .get(&session_id)
            .expect("session lookup")
            .expect("indexed session");
        assert_eq!(
            indexed.additional_dirs,
            vec![additional.to_string_lossy().into_owned()]
        );
        assert!(transport.requests.lock().expect("requests").is_empty());
    }

    #[test]
    fn interactive_ctrl_b_detaches_the_running_shell_process() {
        let temp = TempDir::new().expect("temp");
        let marker = temp.path().join("shell-started");
        let marker_arg = marker.to_string_lossy().replace('\'', "'\"'\"'");
        let transport = Arc::new(ScriptedTransport::default());
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        let output = Arc::new(Mutex::new(Vec::new()));
        let command = format!("!touch '{marker_arg}'; sleep 5\r").into_bytes();
        let mut backend = MemoryBackend::scripted([
            BackendEvent::Input(command),
            BackendEvent::Input(vec![0x02]),
            BackendEvent::Timeout,
            BackendEvent::Input(vec![0x04]),
        ])
        .wait_after_events_for_path(1, marker);
        backend.output = output.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("detached interactive shell");

        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("Moved 1 task to background. /tasks to view."));
        assert!(transport.requests.lock().expect("requests").is_empty());
    }

    #[test]
    fn interactive_status_permission_plan_mcp_help_and_version_are_native() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        adapter
            .executor
            .block_on(
                prepared
                    .session
                    .append_user_message("keep", PromptOrigin::User),
            )
            .expect("seed retained context");
        adapter
            .executor
            .block_on(
                prepared
                    .session
                    .append_user_message("remove", PromptOrigin::User),
            )
            .expect("seed undo context");
        let session = prepared.session.clone();
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut backend = MemoryBackend::scripted([
            BackendEvent::Input(b"/status\r".to_vec()),
            BackendEvent::Input(b"/usage\r".to_vec()),
            BackendEvent::Input(b"/permission manual\r".to_vec()),
            BackendEvent::Input(b"/plan on\r".to_vec()),
            BackendEvent::Input(b"/plan off\r".to_vec()),
            BackendEvent::Input(b"/undo\r".to_vec()),
            BackendEvent::Input(b"/tasks\r".to_vec()),
            BackendEvent::Input(b"/mcp\r".to_vec()),
            BackendEvent::Input(b"/version\r".to_vec()),
            BackendEvent::Input(b"/help\r".to_vec()),
            BackendEvent::Input(vec![0x04]),
        ]);
        backend.output = output.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("native status commands");

        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("permission: auto"));
        assert!(rendered.contains("no token usage recorded"));
        assert!(rendered.contains("permission mode: manual"));
        assert!(rendered.contains("plan mode enabled"));
        assert!(rendered.contains("plan mode disabled"));
        assert!(rendered.contains("no background tasks"));
        assert!(rendered.contains("undid 1 user message · removed 1 context entry"));
        assert!(rendered.contains("no MCP servers configured"));
        assert!(rendered.contains(concat!("mycel ", env!("CARGO_PKG_VERSION"))));
        assert!(rendered.contains("commands: /help /status /usage"));
        assert!(transport.requests.lock().expect("requests").is_empty());
        let snapshot = adapter.executor.block_on(session.snapshot());
        assert!(snapshot
            .state
            .context
            .history()
            .iter()
            .all(|entry| entry.message.text("") != "remove"));
    }

    #[test]
    fn interactive_permission_toggles_plan_clear_and_experimental_alias_are_native() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        fs::write(&prepared.plan_file, "# replace me\n").expect("seed plan file");
        let plan_file = prepared.plan_file.clone();
        let session = prepared.session.clone();
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut backend = MemoryBackend::scripted([
            BackendEvent::Input(b"/auto\r".to_vec()),
            BackendEvent::Input(b"/yolo on\r".to_vec()),
            BackendEvent::Input(b"/yolo off\r".to_vec()),
            BackendEvent::Input(b"/auto on\r".to_vec()),
            BackendEvent::Input(b"/auto off\r".to_vec()),
            BackendEvent::Input(b"/experimental\r".to_vec()),
            BackendEvent::Input(b"/plan on\r".to_vec()),
            BackendEvent::Input(b"/plan clear\r".to_vec()),
            BackendEvent::Input(vec![0x04]),
        ]);
        backend.output = output.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("native toggle commands");

        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("Auto mode: OFF"));
        assert!(rendered.contains("YOLO mode: ON"));
        assert!(rendered.contains("YOLO mode: OFF"));
        assert!(rendered.contains("Auto mode: ON"));
        assert!(rendered.contains("No runtime experiments are registered"));
        assert!(rendered.contains("Plan cleared"));
        assert_eq!(fs::read_to_string(&plan_file).expect("cleared plan"), "");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&plan_file)
                    .expect("cleared plan metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let snapshot = adapter.executor.block_on(session.snapshot());
        assert_eq!(
            snapshot.state.permission_mode,
            ProtocolPermissionMode::Manual
        );
        assert!(snapshot.state.plan_mode);
        assert!(transport.requests.lock().expect("requests").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn plan_clear_rejects_a_symlink_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp");
        let home = temp.path().join("mycel");
        let plans = ensure_plan_directory(&home).expect("plan directory");
        let target = temp.path().join("outside.md");
        fs::write(&target, "keep me").expect("outside target");
        let plan = plans.join("safe-id.md");
        symlink(&target, &plan).expect("plan symlink");

        let error = clear_plan_file(&home, &plan).expect_err("symlink must be rejected");
        assert!(error.contains("non-regular plan file"));
        assert_eq!(
            fs::read_to_string(target).expect("outside target"),
            "keep me"
        );
    }

    #[test]
    fn interactive_export_markdown_writes_the_current_session_without_provider() {
        let temp = TempDir::new().expect("temp");
        let output_path = temp.path().join("session-export.md");
        let transport = Arc::new(ScriptedTransport::default());
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        adapter
            .executor
            .block_on(
                prepared
                    .session
                    .append_user_message("export this conversation", PromptOrigin::User),
            )
            .expect("seed context");
        let output = Arc::new(Mutex::new(Vec::new()));
        let command = format!("/export-md {}\r", output_path.display()).into_bytes();
        let mut backend = MemoryBackend::scripted([
            BackendEvent::Input(command),
            BackendEvent::Input(vec![0x04]),
        ]);
        backend.output = output.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("Markdown export");

        let markdown = fs::read_to_string(&output_path).expect("exported Markdown");
        assert!(markdown.contains("# Mycel Session Export"));
        assert!(markdown.contains("export this conversation"));
        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("Exported 1 messages"));
        assert!(transport.requests.lock().expect("requests").is_empty());
    }

    #[test]
    fn interactive_compact_uses_the_native_durable_compaction_engine() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        transport.respond("native compaction handoff");
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        adapter
            .executor
            .block_on(
                prepared
                    .session
                    .append_user_message("preserve this task", PromptOrigin::User),
            )
            .expect("seed context");
        let session = prepared.session.clone();
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut backend = MemoryBackend::scripted(
            std::iter::once(BackendEvent::Input(
                b"/compact keep exact verification commands\r".to_vec(),
            ))
            .chain(std::iter::repeat_n(BackendEvent::Timeout, 5))
            .chain(std::iter::once(BackendEvent::Input(vec![0x04]))),
        );
        backend.output = output.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("native compaction");

        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(
            rendered.contains("native compaction handoff"),
            "{rendered:?}"
        );
        let requests = transport.requests.lock().expect("requests");
        assert_eq!(requests.len(), 1);
        assert!(
            String::from_utf8_lossy(&requests[0].body).contains("keep exact verification commands")
        );
        drop(requests);
        let snapshot = adapter.executor.block_on(session.snapshot());
        assert_eq!(
            snapshot.state.compaction,
            mycel_agent_runtime::CompactionState::Completed
        );
        assert_eq!(snapshot.state.context.history().len(), 2);
        assert_eq!(
            snapshot.state.context.history()[0].message.text(""),
            "preserve this task"
        );
        assert!(snapshot.state.context.history()[1]
            .message
            .text("")
            .contains("native compaction handoff"));
    }

    #[test]
    fn interactive_hyphae_controls_session_effort_and_runs_one_shot_prompt() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        transport.respond("hyphae completed");
        let source = config().replace(
            "default_effort = \"low\"",
            "default_effort = \"low\"\nsupport_efforts = [\"low\", \"high\", \"xhigh\"]",
        );
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source,
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut backend = MemoryBackend::scripted([
            BackendEvent::Input(b"/hyphae on\r".to_vec()),
            BackendEvent::Input(b"/hyphae off\r".to_vec()),
            BackendEvent::Input(b"/hyphae inspect the patch\r".to_vec()),
            BackendEvent::Input(vec![0x04]),
        ]);
        backend.output = output.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("hyphae session");

        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("hyphae Standing · effort xhigh"));
        assert!(rendered.contains("hyphae Off · effort xhigh"));
        assert!(
            rendered.contains("hyphae one-shot enabled · effort xhigh"),
            "terminal output omitted the one-shot transition: {rendered:?}"
        );
        assert!(rendered.contains("hyphae completed"));
        assert!(rendered.contains("hyphae one-shot finished · swarm disabled"));
        let requests = transport.requests.lock().expect("requests");
        assert_eq!(requests.len(), 1);
        let body = String::from_utf8_lossy(&requests[0].body);
        assert!(body.contains("inspect the patch"), "{body}");
        assert!(body.contains("xhigh"), "{body}");
    }

    #[test]
    fn interactive_hyphae_uses_the_normal_manual_approval_path() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        let source = config().replace(
            "default_effort = \"low\"",
            "default_effort = \"low\"\nsupport_efforts = [\"low\", \"high\", \"xhigh\"]",
        );
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source,
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Manual))
            .expect("prepare");
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut backend = MemoryBackend::scripted([
            BackendEvent::Input(b"/hyphae on\r".to_vec()),
            BackendEvent::Input(b"1".to_vec()),
            BackendEvent::Input(vec![0x04]),
        ])
        .wait_after_events_for_output(1, b"Approve once".to_vec())
        .wait_after_events_for_output(2, b"hyphae Standing".to_vec());
        backend.output = output.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("hyphae approval");

        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("Approve once"), "{rendered:?}");
        assert!(rendered.contains("hyphae Standing · effort xhigh"));
        assert!(transport.requests.lock().expect("requests").is_empty());
    }

    #[test]
    fn installed_local_plugin_composes_list_and_governed_argv_command() {
        let temp = TempDir::new().expect("temp");
        let home = temp.path().join("mycel");
        let plugin_root = home.join("plugins/managed/reviewer");
        fs::create_dir_all(&plugin_root).expect("plugin root");
        fs::write(
            plugin_root.join("mycel.plugin.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "name":"reviewer",
                "version":"1.0.0",
                "description":"local reviewer",
                "commands":{
                    "check":{
                        "command":"printf",
                        "args":["plugin-fixed:%s"]
                    }
                }
            }))
            .expect("manifest JSON"),
        )
        .expect("manifest");
        fs::write(
            home.join("plugins/installed.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "version":1,
                "plugins":[{
                    "id":"reviewer",
                    "root":plugin_root,
                    "source":"local-path",
                    "enabled":true,
                    "installedAt":"2026-08-14T00:00:00Z"
                }]
            }))
            .expect("ledger JSON"),
        )
        .expect("ledger");
        let transport = Arc::new(ScriptedTransport::default());
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut backend = MemoryBackend::scripted(
            [
                BackendEvent::Input(b"/plugins\r".to_vec()),
                BackendEvent::Input(b"/reviewer:check literal; echo nope\r".to_vec()),
            ]
            .into_iter()
            .chain(std::iter::repeat_n(BackendEvent::Timeout, 4))
            .chain(std::iter::once(BackendEvent::Input(vec![0x04]))),
        );
        backend.output = output.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("local plugin command");

        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("reviewer 1.0.0"), "{rendered:?}");
        assert!(
            rendered.contains("plugin-fixed:literal; echo nope"),
            "{rendered:?}"
        );
        assert!(transport.requests.lock().expect("requests").is_empty());
    }

    #[test]
    fn interactive_plugin_lifecycle_installs_toggles_and_removes_local_only() {
        let temp = TempDir::new().expect("temp");
        let source_root = temp.path().join("source-plugin");
        fs::create_dir_all(&source_root).expect("source root");
        fs::write(
            source_root.join("mycel.plugin.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "name":"reviewer",
                "version":"1.0.0",
                "description":"local reviewer"
            }))
            .expect("manifest JSON"),
        )
        .expect("manifest");
        let transport = Arc::new(ScriptedTransport::default());
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        let home = prepared.home.clone();
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut events = vec![BackendEvent::Input(
            format!("/plugins install {}\r", source_root.display()).into_bytes(),
        )];
        events.extend(std::iter::repeat_n(BackendEvent::Timeout, 8));
        events.push(BackendEvent::Input(b"/plugins disable reviewer\r".to_vec()));
        events.extend(std::iter::repeat_n(BackendEvent::Timeout, 8));
        events.push(BackendEvent::Input(b"/plugins list\r".to_vec()));
        events.push(BackendEvent::Input(b"/plugins remove reviewer\r".to_vec()));
        events.extend(std::iter::repeat_n(BackendEvent::Timeout, 8));
        events.push(BackendEvent::Input(vec![0x04]));
        let mut backend = MemoryBackend::scripted(events);
        backend.output = output.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("plugin lifecycle");

        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("installed local plugin reviewer 1.0.0"));
        assert!(rendered.contains("disabled local plugin reviewer"));
        assert!(rendered.contains("reviewer 1.0.0"));
        assert!(rendered.contains("removed local plugin reviewer"));
        assert!(load_plugin_registrations(&home)
            .expect("registrations")
            .is_empty());
        assert!(!home.join("plugins/managed/reviewer").exists());
        assert!(source_root.exists(), "source directory must not be removed");
        assert!(transport.requests.lock().expect("requests").is_empty());
    }

    #[test]
    fn native_delegate_uses_production_child_runtime_without_parent_provider_turn() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        transport.respond("child completed the delegated task");
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        let invocation = prepared
            .orchestration
            .native_delegate_invocation("inspect the current patch")
            .expect("delegate invocation");
        let result = adapter
            .executor
            .block_on(prepared.engine.invoke_host_tool(
                &prepared.session,
                "/delegate inspect the current patch",
                invocation.tool.definition().name,
                invocation.arguments,
                CancellationToken::new(),
            ))
            .expect("delegate execution");
        assert!(!result.is_error, "{result:?}");
        let requests = transport.requests.lock().expect("requests");
        assert_eq!(
            requests.len(),
            1,
            "only the native child should call the provider"
        );
        let body: Value = serde_json::from_slice(&requests[0].body).expect("child request body");
        let system_message = body["messages"]
            .as_array()
            .and_then(|messages| messages.first())
            .expect("child system message")
            .to_string();
        assert!(system_message.contains("You are Mycel"));
        assert!(system_message.contains("# Subagent role"));
        drop(requests);
        adapter
            .executor
            .block_on(shutdown_orchestration(Some(
                prepared.orchestration.as_ref(),
            )))
            .expect("shut down orchestration");
        adapter
            .executor
            .block_on(shutdown_mcp(prepared.mcp.as_ref()))
            .expect("shut down MCP");
        adapter
            .executor
            .block_on(prepared.session.close())
            .expect("close session");
    }

    #[test]
    fn interactive_goal_command_runs_native_goal_and_renders_completion() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        transport.respond_tool_call(
            "goal-complete",
            "UpdateGoal",
            serde_json::json!({"action":"complete","reason":"interactive objective done"}),
        );
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        let output = Arc::new(Mutex::new(Vec::new()));
        // Six blind Timeout ticks were meant to let the goal run to completion
        // before Ctrl-D; under contention that is not enough and the test
        // races the goal. Wait for the completion render instead (bounded).
        let mut backend = MemoryBackend::scripted([
            BackendEvent::Input(b"/goal ship the patch\r".to_vec()),
            BackendEvent::Input(vec![0x04]),
        ])
        .wait_after_events_for_output(1, b"complete".to_vec());
        backend.output = output.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("interactive goal");

        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("/goal ship the patch"));
        assert!(rendered.contains("ship the patch"));
        assert!(rendered.contains("complete"));
        assert_eq!(transport.requests.lock().expect("requests").len(), 1);
    }

    #[test]
    fn repeated_skill_roots_replace_defaults_and_configured_extra_roots_append() {
        fn write_skill(root: &Path, directory: &str, name: &str) {
            let bundle = root.join(directory);
            fs::create_dir_all(&bundle).expect("skill bundle");
            fs::write(
                bundle.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name} skill\n---\n{name} body\n"),
            )
            .expect("skill definition");
        }

        let temp = TempDir::new().expect("temp");
        let project = temp.path().join("project");
        let working = project.join("nested");
        fs::create_dir_all(project.join(".git")).expect("project marker");
        fs::create_dir_all(&working).expect("working directory");
        let first = temp.path().join("first-skills");
        let second = temp.path().join("second-skills");
        let extra = temp.path().join("extra-skills");
        let default = project.join(".mycel/skills");
        write_skill(&first, "first", "first");
        write_skill(&second, "second", "second");
        write_skill(&extra, "extra", "extra");
        write_skill(&default, "default", "default");
        let mut parsed = parse_config(&config()).expect("config");
        parsed.extra_skill_dirs = vec![extra.to_string_lossy().into_owned()];

        let composed = compose_skills(
            &parsed,
            &[first, second],
            &temp.path().join("mycel"),
            None,
            &working,
            &[],
        )
        .expect("skill composition");
        assert!(composed.warnings.is_empty());
        let activation = composed.activation.expect("loaded skill port");
        for id in ["first", "second", "extra"] {
            assert!(activation
                .activate(
                    id,
                    &[],
                    mycel_agent_runtime::SkillTrigger::ModelTool,
                    "session-skill-roots",
                )
                .is_ok());
        }
        assert!(activation
            .activate(
                "default",
                &[],
                mycel_agent_runtime::SkillTrigger::ModelTool,
                "session-skill-roots",
            )
            .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn home_working_dir_scans_user_agents_skills_once_and_follows_symlinks() {
        // Launching from $HOME with no .git makes the project root equal the
        // user home: ~/.agents/skills used to arrive twice (Project + User)
        // and a symlinked skill tree inside it was refused as an escape.
        fn write_skill(root: &Path, directory: &str, name: &str) {
            let bundle = root.join(directory);
            fs::create_dir_all(&bundle).expect("skill bundle");
            fs::write(
                bundle.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name} skill\n---\n{name} body\n"),
            )
            .expect("skill definition");
        }

        let temp = TempDir::new().expect("temp");
        let user_home = temp.path().join("home");
        let agents_skills = user_home.join(".agents/skills");
        let elsewhere = temp.path().join("other-harness/skills");
        write_skill(&agents_skills, "plain", "plain");
        write_skill(&elsewhere, "linked", "linked");
        std::os::unix::fs::symlink(&elsewhere, agents_skills.join("linked-tree"))
            .expect("symlink into another skill tree");
        let parsed = parse_config(&config()).expect("config");

        let composed = compose_skills(
            &parsed,
            &[],
            &user_home.join(".mycel"),
            Some(&user_home),
            &user_home,
            &[],
        )
        .expect("skill composition");
        assert!(
            composed.warnings.is_empty(),
            "unexpected skill warnings: {:?}",
            composed.warnings
        );
        let activation = composed.activation.expect("loaded skill port");
        for id in ["plain", "linked"] {
            assert!(
                activation
                    .activate(
                        id,
                        &[],
                        mycel_agent_runtime::SkillTrigger::ModelTool,
                        "session-home-skills",
                    )
                    .is_ok(),
                "skill {id} should load from ~/.agents/skills"
            );
        }
    }

    #[test]
    fn startup_plan_uses_one_global_opaque_file_and_replays_without_sibling_access() {
        let temp = TempDir::new().expect("temp");
        let home = temp.path().join("mycel");
        let tools = Arc::new(CapturingLocalToolRegistryBuilder::default());
        let adapter = ProductionRuntimeAdapter::with_components_and_tools(
            Arc::new(FixedHome(home.clone())),
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            Arc::new(TestEnvironment::default()),
            Arc::new(ScriptedTransport::default()),
            tools.clone(),
        )
        .expect("adapter");
        let mut request = interactive(SessionSelection::New, PermissionMode::Manual);
        request.plan = true;
        let prepared = adapter.prepare_interactive(&request).expect("startup plan");
        let first_snapshot = adapter.executor.block_on(prepared.session.snapshot());
        assert!(first_snapshot.state.plan_mode);
        let plan_file = PathBuf::from(
            first_snapshot.state.tool_store["plan_file"]
                .as_str()
                .expect("plan path"),
        );
        let canonical_plans = fs::canonicalize(home.join(PLANS_DIR)).expect("plans directory");
        assert_eq!(plan_file.parent(), Some(canonical_plans.as_path()));
        assert_ne!(
            plan_file.file_stem().and_then(|stem| stem.to_str()),
            Some(prepared.session.id().as_str())
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&canonical_plans)
                    .expect("plan directory metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }

        let written = adapter.executor.block_on(invoke_registered_tool(
            &tools.registry,
            prepared.session.id(),
            "Write",
            serde_json::json!({
                "path":plan_file.to_string_lossy(),
                "content":"# retained plan\n\n- verify replay\n"
            }),
        ));
        assert!(!written.is_error);
        let sibling = canonical_plans.join("sibling-plan.md");
        fs::write(&sibling, "# another session").expect("sibling plan");
        let write = tools.registry.snapshot().get("Write").expect("Write");
        let context = ToolPrepareContext {
            session_id: prepared.session.id().clone(),
            agent_id: AgentId::main(),
            turn_id: 2,
            tool_call_id: ToolCallId::new("test-sibling-plan").expect("tool call id"),
        };
        assert!(write
            .prepare(
                &serde_json::json!({
                    "path":sibling.to_string_lossy(),
                    "content":"overwrite"
                }),
                &context,
            )
            .is_err());

        let session_id = prepared.session.id().as_str().to_owned();
        adapter
            .executor
            .block_on(shutdown_orchestration(Some(
                prepared.orchestration.as_ref(),
            )))
            .expect("shut down orchestration");
        adapter
            .executor
            .block_on(shutdown_mcp(prepared.mcp.as_ref()))
            .expect("shut down MCP");
        adapter
            .executor
            .block_on(prepared.session.close())
            .expect("close active plan session");

        let resumed_tools = Arc::new(CapturingLocalToolRegistryBuilder::default());
        let resumed_adapter = ProductionRuntimeAdapter::with_components_and_tools(
            Arc::new(FixedHome(home)),
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            Arc::new(TestEnvironment::default()),
            Arc::new(ScriptedTransport::default()),
            resumed_tools,
        )
        .expect("resumed adapter");
        let resumed = resumed_adapter
            .prepare_interactive(&interactive(
                SessionSelection::Resume(session_id),
                PermissionMode::Manual,
            ))
            .expect("resume active plan");
        let replayed = resumed_adapter
            .executor
            .block_on(resumed.session.snapshot());
        assert!(replayed.state.plan_mode);
        assert_eq!(
            replayed.state.tool_store["plan_file"].as_str(),
            Some(plan_file.to_string_lossy().as_ref())
        );
        resumed_adapter
            .executor
            .block_on(shutdown_orchestration(Some(resumed.orchestration.as_ref())))
            .expect("shut down resumed orchestration");
        resumed_adapter
            .executor
            .block_on(shutdown_mcp(resumed.mcp.as_ref()))
            .expect("shut down resumed MCP");
        resumed_adapter
            .executor
            .block_on(resumed.session.close())
            .expect("close resumed session");
    }

    #[test]
    fn prepared_header_carries_the_live_substrate_summary() {
        let temp = TempDir::new().expect("temp");
        let home = temp.path().join("mycel");
        fs::create_dir_all(home.join("substrate")).expect("substrate dir");
        // Seed one live antibody through the same service `/deny` uses, and
        // arm the gate wiring the summary reads.
        let ecology = EcologyService::new(&home);
        mycel_mcp::McpTools::open(&ecology.paths().database).expect("initialize db");
        ecology.run(crate::ecology::EcologyCommand::Deny, "rm -rf /", Utc::now());
        fs::write(
            home.join("config.toml"),
            "[[hooks]]\nevent = \"PreToolUse\"\nmatcher = \"\"\ncommand = \"$HOME/.mycel/bin/mycel-gate\"\nfail_mode = \"closed\"\n",
        )
        .expect("gate wiring");

        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            Arc::new(ScriptedTransport::default()),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Manual))
            .expect("prepare");
        assert_eq!(
            prepared.substrate,
            SubstrateStatus {
                antibodies_active: 1,
                candidates_pending: 0,
                gate: GateStatus::Ok,
            }
        );
        let header = build_header(&prepared);
        assert_eq!(
            header.substrate,
            SubstrateSummary {
                antibodies: 1,
                candidates_pending: 0,
                gate: GateDisplay::Ok,
            }
        );
        // The live Ok state renders the green dot with its verdict word.
        let rendered = header_card(&header, &Theme::amanita(), 120, true).join("\n");
        assert!(rendered.contains("1 antibodies"));
        assert!(rendered.contains("38;2;85;168;104m●"), "{rendered}");

        adapter
            .executor
            .block_on(shutdown_orchestration(Some(
                prepared.orchestration.as_ref(),
            )))
            .expect("shut down orchestration");
        adapter
            .executor
            .block_on(shutdown_mcp(prepared.mcp.as_ref()))
            .expect("shut down MCP");
        adapter
            .executor
            .block_on(prepared.session.close())
            .expect("close session");
    }

    #[test]
    fn rail_data_snapshots_session_identity_and_transcript_hyphae() {
        let temp = TempDir::new().expect("temp");
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            Arc::new(ScriptedTransport::default()),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Manual))
            .expect("prepare");
        let mut state = InteractiveLoopState::new(
            &adapter.executor,
            &prepared,
            TerminalSize {
                columns: 120,
                rows: 30,
            },
        );
        let now = state.now_ms();
        state.transcript.push(
            TranscriptEvent::SubagentState {
                id: "sub/1".to_owned(),
                name: "test-runner".to_owned(),
                state: "started".to_owned(),
                detail: None,
            },
            now,
        );
        let data = state.rail_data(&prepared);
        // A fresh session carries the index's default title
        // (session_index.rs DEFAULT_TITLE), which the rail shows verbatim.
        assert_eq!(data.name, "New Session");
        assert_eq!(data.model, prepared.model_alias);
        assert_eq!(data.provider, prepared.provider);
        assert_eq!(data.ctx_window, prepared.context_window);
        assert_eq!(data.substrate, prepared.substrate);
        assert_eq!(data.hyphae_active, 1);
        let last = data.hyphae_last.expect("hyphae line");
        assert!(last.starts_with("test-runner · started · "), "{last}");

        state.transcript.push(
            TranscriptEvent::SubagentState {
                id: "sub/1".to_owned(),
                name: "test-runner".to_owned(),
                state: "exited".to_owned(),
                detail: None,
            },
            now,
        );
        let data = state.rail_data(&prepared);
        assert_eq!(data.hyphae_active, 0);
        assert!(data
            .hyphae_last
            .expect("hyphae line")
            .starts_with("test-runner · exited"));

        state.event_pump.abort();
        adapter
            .executor
            .block_on(shutdown_orchestration(Some(
                prepared.orchestration.as_ref(),
            )))
            .expect("shut down orchestration");
        adapter
            .executor
            .block_on(shutdown_mcp(prepared.mcp.as_ref()))
            .expect("shut down MCP");
        adapter
            .executor
            .block_on(prepared.session.close())
            .expect("close session");
    }

    #[test]
    fn projected_gate_deny_resolves_the_antibody_for_the_inspector() {
        let temp = TempDir::new().expect("temp");
        let home = temp.path().join("mycel");
        fs::create_dir_all(home.join("substrate")).expect("substrate dir");
        let antibody_id = uuid::Uuid::new_v4();
        let antibody = mycel_core::Antibody {
            id: antibody_id,
            signature: mycel_core::Signature {
                error_class: None,
                file_pattern: Some("~/.mycel/**".to_owned()),
                agent_role: None,
                tool_pattern: Some("write".to_owned()),
                command_pattern: None,
                scope: mycel_core::SignatureScope::Project,
            },
            source: mycel_core::AntibodySource::Manual,
            severity: mycel_core::Severity::Refuse,
            confidence: mycel_core::Confidence::Solid,
            refusal_mode: mycel_core::RefusalMode::Hard,
            remediation: "stage the change in-repo".to_owned(),
            examples: Vec::new(),
            created_at: Utc::now(),
            expires_at: None,
            hit_count: 3,
        };
        let ecology = EcologyService::new(&home);
        mycel_mcp::McpTools::open(&ecology.paths().database)
            .expect("initialize db")
            .insert_antibodies([antibody])
            .expect("insert antibody");

        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            Arc::new(ScriptedTransport::default()),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Manual))
            .expect("prepare");
        let mut state = InteractiveLoopState::new(
            &adapter.executor,
            &prepared,
            TerminalSize {
                columns: 120,
                rows: 30,
            },
        );
        // Replay the exact event shapes the runtime publishes for a denied
        // call (turn.rs `emit_hook_report` + the synthetic result).
        state
            .message_sender
            .send(InteractiveRuntimeMessage::Event(Box::new(
                AgentEvent::ToolCallStarted {
                    turn_id: 1,
                    tool_call_id: "call/1".to_owned(),
                    name: "write".to_owned(),
                    args: serde_json::json!({"file_path": "~/.mycel/config.toml"}),
                    description: None,
                    display: None,
                },
            )))
            .expect("send started");
        state
            .message_sender
            .send(InteractiveRuntimeMessage::Event(Box::new(
                AgentEvent::HookResult {
                    turn_id: Some(1),
                    hook_event: "PreToolUse".to_owned(),
                    content: format!("Denied by operator. (source: antibody:{antibody_id})"),
                    blocked: Some(true),
                },
            )))
            .expect("send deny");
        state
            .process_runtime_messages(&adapter.executor, &prepared)
            .expect("process events");

        let detail = state
            .last_deny_antibody
            .clone()
            .expect("deny resolved its antibody");
        assert_eq!(detail.id, crate::util::short_id(&antibody_id.to_string()));
        assert_eq!(detail.source, "manual");
        assert_eq!(detail.severity, "refuse");
        assert_eq!(detail.refusal, "hard");
        assert_eq!(detail.hits, 3);
        assert!(detail
            .signature
            .contains(&("file_pattern".to_owned(), "~/.mycel/**".to_owned())));

        let inspector = state.inspector_data();
        assert_eq!(inspector.antibody.as_ref(), Some(&detail));
        let last = inspector.activity.last().expect("deny in the ring");
        assert_eq!(last.verdict, crate::tui::GateVerdict::Deny);
        assert_eq!(last.tool, "write");
        assert_eq!(last.target, "~/.mycel/config.toml");

        state.event_pump.abort();
        adapter
            .executor
            .block_on(shutdown_orchestration(Some(
                prepared.orchestration.as_ref(),
            )))
            .expect("shut down orchestration");
        adapter
            .executor
            .block_on(shutdown_mcp(prepared.mcp.as_ref()))
            .expect("shut down MCP");
        adapter
            .executor
            .block_on(prepared.session.close())
            .expect("close session");
    }

    #[test]
    fn shift_tab_commits_plan_transitions_and_rolls_back_failed_optimism() {
        let temp = TempDir::new().expect("temp");
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            Arc::new(ScriptedTransport::default()),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Manual))
            .expect("prepare");
        let mut state = InteractiveLoopState::new(
            &adapter.executor,
            &prepared,
            TerminalSize {
                columns: 80,
                rows: 24,
            },
        );
        let shift_tab = || {
            InputEvent::Key(crate::terminal::KeyEvent {
                code: KeyCode::Tab,
                modifiers: crate::terminal::Modifiers {
                    shift: true,
                    ..crate::terminal::Modifiers::default()
                },
                kind: crate::terminal::KeyKind::Press,
            })
        };

        state.reducer.apply(shift_tab());
        assert!(state.reducer.plan);
        assert!(!state.process_actions(&adapter.executor, &prepared));
        assert!(
            adapter
                .executor
                .block_on(prepared.session.snapshot())
                .state
                .plan_mode
        );

        state.reducer.apply(shift_tab());
        assert!(!state.reducer.plan);
        assert!(!state.process_actions(&adapter.executor, &prepared));
        assert!(
            !adapter
                .executor
                .block_on(prepared.session.snapshot())
                .state
                .plan_mode
        );

        adapter
            .executor
            .block_on(
                prepared
                    .session
                    .enter_plan_mode(Some(prepared.plan_file.to_string_lossy().into_owned())),
            )
            .expect("external plan transition");
        state.reducer.plan = false;
        state.reducer.apply(shift_tab());
        assert!(state.reducer.plan);
        assert!(!state.process_actions(&adapter.executor, &prepared));
        assert!(
            !state.reducer.plan,
            "failed optimistic transition must roll back"
        );
        for _ in 0..100 {
            state
                .process_runtime_messages(&adapter.executor, &prepared)
                .expect("runtime messages");
            if state.reducer.plan {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            state.reducer.plan,
            "canonical status event must resynchronize UI"
        );

        state.event_pump.abort();
        adapter
            .executor
            .block_on(shutdown_orchestration(Some(
                prepared.orchestration.as_ref(),
            )))
            .expect("shut down orchestration");
        adapter
            .executor
            .block_on(shutdown_mcp(prepared.mcp.as_ref()))
            .expect("shut down MCP");
        adapter
            .executor
            .block_on(prepared.session.close())
            .expect("close session");
    }

    #[test]
    fn interactive_pasted_image_reaches_the_provider_as_media_content() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        transport.respond("image received");
        let source = config().replace(
            "max_output_size = 128",
            "max_output_size = 128\ncapabilities = [\"image_in\"]",
        );
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source,
                paths: Mutex::new(Vec::new()),
            }),
            Arc::clone(&transport),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        let mut state = InteractiveLoopState::new(
            &adapter.executor,
            &prepared,
            TerminalSize {
                columns: 80,
                rows: 24,
            },
        );
        let placeholder = state
            .pasted_images
            .add(crate::clipboard::ClipboardImage {
                bytes: b"\x89PNG\r\n\x1a\nproduction-image".to_vec(),
                mime: "image/png",
            })
            .expect("clipboard image");

        state.start_turn(
            &adapter.executor,
            &prepared,
            format!("inspect {placeholder} carefully"),
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while state.active.is_some() && Instant::now() < deadline {
            state
                .process_runtime_messages(&adapter.executor, &prepared)
                .expect("runtime messages");
            thread::sleep(Duration::from_millis(1));
        }
        assert!(state.active.is_none(), "image turn did not finish");

        let requests = transport.requests.lock().expect("requests");
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).expect("provider body");
        let messages = body["messages"].to_string();
        assert!(messages.contains("inspect "), "{messages}");
        assert!(messages.contains(" carefully"), "{messages}");
        assert!(
            messages.contains("data:image/png;base64,"),
            "clipboard media did not reach the provider: {messages}"
        );
        drop(requests);

        adapter
            .executor
            .block_on(shutdown_orchestration(Some(
                prepared.orchestration.as_ref(),
            )))
            .expect("shut down orchestration");
        adapter
            .executor
            .block_on(shutdown_mcp(prepared.mcp.as_ref()))
            .expect("shut down MCP");
        adapter
            .executor
            .block_on(prepared.session.close())
            .expect("close session");
    }

    #[test]
    fn snake_case_toml_is_normalized_and_validated() {
        let parsed = parse_config(&config()).expect("valid config");
        assert_eq!(parsed.default_model.as_deref(), Some("local"));
        assert_eq!(parsed.models["local"].max_context_size, 8192);
        assert_eq!(parsed.providers["local"].custom_headers["X-Test"], "yes");
        assert_eq!(parsed.hooks[0].fail_mode, Some(HookFailMode::Closed));

        let invalid = config().replace("max_context_size = 8192", "max_context_size = 0");
        assert!(parse_config(&invalid)
            .expect_err("invalid context size")
            .contains("positive max_context_size"));
    }

    #[test]
    fn global_thinking_effort_overrides_model_default() {
        let parsed = parse_config(&config()).expect("valid config");
        let resolved = resolve_model(
            None,
            &parsed,
            &TestEnvironment::default(),
            "prompt execution",
        )
        .expect("resolved model");
        assert_eq!(
            resolved
                .thinking_effort
                .as_ref()
                .map(ThinkingEffort::as_str),
            Some("high")
        );
    }

    #[test]
    fn vertex_service_account_resolution_uses_retained_env_contract_and_precedence() {
        let source = r#"
default_model = "vertex"

[providers.vertex]
type = "vertexai"
base_url = "https://us-central1-aiplatform.googleapis.com"
env = { GOOGLE_CLOUD_PROJECT = "configured-project", GOOGLE_APPLICATION_CREDENTIALS = "/configured/key.json" }

[models.vertex]
provider = "vertex"
model = "gemini-test"
max_context_size = 8192
"#;
        let parsed = parse_config(source).expect("vertex config");
        let environment = TestEnvironment::default();
        environment.0.lock().expect("environment").extend([
            (
                "GOOGLE_CLOUD_PROJECT".to_owned(),
                "process-project".to_owned(),
            ),
            (
                GOOGLE_APPLICATION_CREDENTIALS.to_owned(),
                "/process/key.json".to_owned(),
            ),
        ]);
        let resolved = resolve_model(None, &parsed, &environment, "prompt execution")
            .expect("resolved vertex");
        let provider = &resolved.registry.providers[0];
        assert!(matches!(
            &provider.adapter,
            ProviderAdapterConfig::VertexServiceAccount {
                project,
                location,
                ..
            } if project == "configured-project" && location == "us-central1"
        ));
        assert!(matches!(
            &provider.credential,
            ProviderCredentialConfig::GoogleServiceAccount(
                GoogleServiceAccountCredentialSource::File(path)
            ) if path == Path::new("/configured/key.json")
        ));
        assert_eq!(
            resolved.google_application_credentials.as_deref(),
            Some(Path::new("/process/key.json"))
        );

        let api_key_source = source
            .replace(
                "base_url = \"https://us-central1-aiplatform.googleapis.com\"",
                "api_key = \"vertex-key\"",
            )
            .replace(
                "env = { GOOGLE_CLOUD_PROJECT = \"configured-project\", GOOGLE_APPLICATION_CREDENTIALS = \"/configured/key.json\" }",
                "env = { GOOGLE_CLOUD_PROJECT = \"configured-project\" }",
            );
        let parsed = parse_config(&api_key_source).expect("api key vertex config");
        let resolved = resolve_model(
            None,
            &parsed,
            &TestEnvironment::default(),
            "prompt execution",
        )
        .expect("api key vertex");
        assert!(matches!(
            resolved.registry.providers[0].adapter,
            ProviderAdapterConfig::VertexApiKey { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn retained_tool_hooks_register_matchers_and_fail_mode_before_tools_exist() {
        let temp = TempDir::new().expect("temp");
        let mut parsed = parse_config(&config()).expect("valid config");
        parsed.hooks[0].command = "printf denied >&2; exit 2".to_owned();
        let mut matched = parsed.hooks[0].clone();
        matched.event = HookEvent::PostToolUse;
        matched.matcher = Some("Write".to_owned());
        matched.command = "printf matched".to_owned();
        matched.fail_mode = Some(HookFailMode::Open);
        parsed.hooks.push(matched);
        let runner = configured_hook_runner(&parsed, temp.path()).expect("hooks");
        let executor = tokio::runtime::Runtime::new().expect("executor");
        let mut input = ToolHookInput {
            hook_event_name: ToolHookEvent::PreToolUse,
            session_id: SessionId::new("s1").expect("session id"),
            agent_id: AgentId::main(),
            turn_id: 1,
            tool_call_id: ToolCallId::new("t1").expect("tool id"),
            tool_name: "Read".to_owned(),
            arguments: serde_json::json!({}),
            content: String::new(),
            result: None,
        };
        let denied = executor.block_on(runner.run(
            ToolHookEvent::PreToolUse,
            &input,
            &CancellationToken::new(),
        ));
        assert_eq!(denied.executions.len(), 1);
        assert_eq!(denied.executions[0].stderr, "denied");
        assert!(denied.blocked.is_some());

        input.hook_event_name = ToolHookEvent::PostToolUse;
        let unmatched = executor.block_on(runner.run(
            ToolHookEvent::PostToolUse,
            &input,
            &CancellationToken::new(),
        ));
        assert!(unmatched.executions.is_empty());
        input.tool_name = "Write".to_owned();
        let matched = executor.block_on(runner.run(
            ToolHookEvent::PostToolUse,
            &input,
            &CancellationToken::new(),
        ));
        assert_eq!(matched.executions[0].stdout, "matched");
        assert!(matched.blocked.is_none());
    }

    #[test]
    fn every_retained_hook_event_maps_to_the_runtime_lifecycle_surface() {
        let cases = [
            (HookEvent::PreToolUse, ToolHookEvent::PreToolUse),
            (HookEvent::PostToolUse, ToolHookEvent::PostToolUse),
            (
                HookEvent::PostToolUseFailure,
                ToolHookEvent::PostToolUseFailure,
            ),
            (
                HookEvent::PermissionRequest,
                ToolHookEvent::PermissionRequest,
            ),
            (HookEvent::PermissionResult, ToolHookEvent::PermissionResult),
            (HookEvent::UserPromptSubmit, ToolHookEvent::UserPromptSubmit),
            (HookEvent::Stop, ToolHookEvent::Stop),
            (HookEvent::StopFailure, ToolHookEvent::StopFailure),
            (HookEvent::Interrupt, ToolHookEvent::Interrupt),
            (HookEvent::SessionStart, ToolHookEvent::SessionStart),
            (HookEvent::SessionEnd, ToolHookEvent::SessionEnd),
            (HookEvent::SubagentStart, ToolHookEvent::SubagentStart),
            (HookEvent::SubagentStop, ToolHookEvent::SubagentStop),
            (HookEvent::PreCompact, ToolHookEvent::PreCompact),
            (HookEvent::PostCompact, ToolHookEvent::PostCompact),
            (HookEvent::Notification, ToolHookEvent::Notification),
        ];
        for (configured, runtime) in cases {
            assert_eq!(runtime_hook_event(configured), runtime);
        }
    }

    #[test]
    fn parsed_provider_list_dispatches_through_the_production_runner_without_secrets() {
        let temp = TempDir::new().expect("temp");
        let home = temp.path().join("mycel");
        fs::create_dir_all(&home).expect("MYCEL_HOME");
        fs::write(home.join(CONFIG_FILE), config()).expect("provider config");
        let source = Arc::new(RecordingConfig {
            source: config(),
            paths: Mutex::new(Vec::new()),
        });
        let mut adapter = adapter(&temp, source, Arc::new(ScriptedTransport::default()));
        let parsed = crate::cli::Cli::try_parse_from(["mycel", "provider", "list", "--json"])
            .expect("provider command");
        let output = adapter
            .run_command(RuntimeRequest::Command(
                parsed.command.expect("parsed command"),
            ))
            .expect("provider list");
        assert!(matches!(
            output.completion,
            RuntimeCompletion::Success { .. }
        ));
        let value: Value = serde_json::from_str(&output.stdout).expect("provider list JSON");
        assert_eq!(value["providers"][0]["id"], "local");
        assert_eq!(value["providers"][0]["credential"], "configured");
        assert!(!output.stdout.contains("test-key"));
        assert!(!output.stderr.contains("test-key"));
    }

    #[test]
    fn interactive_provider_list_restores_terminal_and_resumes_the_session() {
        let temp = TempDir::new().expect("temp");
        let home = temp.path().join("mycel");
        fs::create_dir_all(&home).expect("MYCEL_HOME");
        fs::write(home.join(CONFIG_FILE), config()).expect("provider config");
        let transport = Arc::new(ScriptedTransport::default());
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut backend = MemoryBackend::scripted([
            BackendEvent::Input(b"/provider list\r".to_vec()),
            BackendEvent::Input(vec![0x04]),
        ]);
        backend.output = output.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_interactive_with_terminal(
                &interactive(SessionSelection::New, PermissionMode::Auto),
                &mut driver,
            )
            .expect("interactive provider list");

        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("local"), "{rendered:?}");
        assert!(!rendered.contains("test-key"), "{rendered:?}");
        assert!(transport.requests.lock().expect("requests").is_empty());
    }

    #[test]
    fn interactive_provider_parser_is_bounded_and_kimi_auth_is_explicit() {
        assert_eq!(
            parse_interactive_provider_command("list --json").expect("list"),
            (
                Command::Provider(ProviderArgs {
                    command: ProviderCommand::List { json: true },
                }),
                false,
            )
        );
        assert_eq!(
            parse_interactive_provider_command("logout kimi").expect("logout"),
            (
                Command::Provider(ProviderArgs {
                    command: ProviderCommand::Logout {
                        provider: ProviderAuthTarget::Kimi,
                    },
                }),
                true,
            )
        );
        assert!(parse_interactive_provider_command("login anthropic").is_err());
        assert!(parse_interactive_provider_command("remove ../escape").is_err());
        assert!(
            parse_interactive_provider_command(&format!("remove {}", "x".repeat(129))).is_err()
        );
    }

    #[test]
    fn provider_command_config_errors_and_mcp_config_errors_redact_secret_values() {
        let temp = TempDir::new().expect("temp");
        let home = temp.path().join("mycel");
        fs::create_dir_all(&home).expect("MYCEL_HOME");
        let provider_secret = "provider-secret-must-not-escape";
        fs::write(
            home.join(CONFIG_FILE),
            format!(
                "[providers.local]\ntype = \"openai\"\napi_key = \"{provider_secret}\"\nbroken = ["
            ),
        )
        .expect("invalid provider config");
        let source = Arc::new(RecordingConfig {
            source: config(),
            paths: Mutex::new(Vec::new()),
        });
        let transport = Arc::new(ScriptedTransport::default());
        let mut adapter = adapter(&temp, source, transport.clone());
        let error = adapter
            .run_command(RuntimeRequest::Command(Command::Provider(ProviderArgs {
                command: ProviderCommand::List { json: false },
            })))
            .expect_err("malformed provider config");
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains(provider_secret), "{rendered}");

        let mcp_secret = "mcp-secret-must-not-escape";
        fs::write(
            home.join(MCP_CONFIG_FILE),
            format!(
                r#"{{"mcpServers":{{"private":{{"transport":"http","url":"https://mcp.example.test","headers":{{"Authorization":"{mcp_secret}"}}}}}}"#
            ),
        )
        .expect("invalid MCP config");
        let error = adapter
            .run_prompt(
                &prompt(SessionSelection::New),
                &mut CollectingSink::default(),
            )
            .expect_err("malformed MCP config");
        let rendered = format!("{error:?} {error}");
        assert!(rendered.contains("invalid MCP configuration"));
        assert!(!rendered.contains(mcp_secret), "{rendered}");
        assert!(transport.requests.lock().expect("requests").is_empty());
        assert!(!home.join(SESSIONS_DIR).exists());
    }

    #[test]
    fn missing_or_empty_mcp_config_never_constructs_a_connector() {
        let temp = TempDir::new().expect("temp");
        let source = Arc::new(RecordingConfig {
            source: config(),
            paths: Mutex::new(Vec::new()),
        });
        let transport = Arc::new(ScriptedTransport::default());
        transport.respond("answer without MCP");
        let factory = TestMcpConnectorFactory::fail_if_called();
        let mut adapter = adapter_with_mcp(&temp, source, transport, factory.clone());
        let missing = adapter
            .load_mcp_config(&temp.path().join("mycel"), "missing MCP")
            .expect("missing MCP config");
        assert!(missing.mcp_servers.is_empty());
        let completion = adapter
            .run_prompt(
                &prompt(SessionSelection::New),
                &mut CollectingSink::default(),
            )
            .expect("headless prompt without MCP");
        assert!(matches!(completion, RuntimeCompletion::Success { .. }));
        assert_eq!(factory.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn headless_mcp_discovers_before_the_first_turn_and_shuts_down_exactly_once() {
        let temp = TempDir::new().expect("temp");
        write_test_mcp_config(&temp);
        let source = Arc::new(RecordingConfig {
            source: config(),
            paths: Mutex::new(Vec::new()),
        });
        let transport = Arc::new(ScriptedTransport::default());
        transport.respond("answer with MCP");
        let peer = ScriptedMcpPeer::modern_with_tool();
        let connector: Arc<dyn McpTransportConnector> =
            Arc::new(OneHttpMcpConnector::new(peer.clone()));
        let factory = TestMcpConnectorFactory::fixed(connector);
        let mut adapter = adapter_with_mcp(&temp, source, transport.clone(), factory.clone());

        let completion = adapter
            .run_prompt(
                &prompt(SessionSelection::New),
                &mut CollectingSink::default(),
            )
            .expect("headless prompt with MCP");
        assert!(matches!(completion, RuntimeCompletion::Success { .. }));
        assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
        assert_eq!(peer.close_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            peer.requests
                .lock()
                .expect("MCP requests")
                .iter()
                .map(|request| request.method.as_str())
                .collect::<Vec<_>>(),
            ["server/discover", "tools/list"]
        );
        let provider_requests = transport.requests.lock().expect("provider requests");
        let body: Value =
            serde_json::from_slice(&provider_requests[0].body).expect("provider body");
        assert!(body["tools"]
            .as_array()
            .expect("provider tools")
            .iter()
            .any(|tool| tool["function"]["name"] == "mcp__fixture__ping"));
    }

    #[test]
    fn production_prompt_runs_real_provider_turn_and_continues_newest_session() {
        let temp = TempDir::new().expect("temp");
        let skills = temp.path().join("headless-skills");
        fs::create_dir_all(skills.join("headless-review")).expect("skill directory");
        fs::write(
            skills.join("headless-review/SKILL.md"),
            "---\nname: headless-review\ndescription: headless review\n---\nReview the prompt.\n",
        )
        .expect("skill file");
        let source_text = config()
            .replace(
                "default_permission_mode = \"manual\"",
                &format!(
                    "default_permission_mode = \"manual\"\nextra_skill_dirs = [{}]",
                    toml::Value::String(skills.to_string_lossy().into_owned())
                ),
            )
            .replace(
                "max_output_size = 128",
                "max_output_size = 128\ncapabilities = [\"image_in\"]",
            );
        let source = Arc::new(RecordingConfig {
            source: source_text,
            paths: Mutex::new(Vec::new()),
        });
        let transport = Arc::new(ScriptedTransport::default());
        transport.respond("first answer");
        transport.respond("second answer");
        let mut adapter = adapter(&temp, Arc::clone(&source), Arc::clone(&transport));

        let mut first_events = CollectingSink::default();
        let first = adapter
            .run_prompt(&prompt(SessionSelection::New), &mut first_events)
            .expect("first turn");
        let first_id = first.session_id().expect("session id").to_owned();
        assert!(first_events.0.iter().any(
            |event| matches!(event, HeadlessEvent::AssistantDelta(text) if text == "first answer")
        ));
        assert!(temp
            .path()
            .join("mycel/sessions")
            .join(&first_id)
            .join("agents/main/records.jsonl")
            .is_file());

        let mut resumed_events = CollectingSink::default();
        let resumed = adapter
            .run_prompt(&prompt(SessionSelection::Continue), &mut resumed_events)
            .expect("resumed turn");
        assert_eq!(resumed.session_id(), Some(first_id.as_str()));
        assert!(resumed_events.0.iter().any(
            |event| matches!(event, HeadlessEvent::AssistantDelta(text) if text == "second answer")
        ));

        let requests = transport.requests.lock().expect("requests");
        assert_eq!(requests.len(), 2);
        assert!(requests[0].url.ends_with("/chat/completions"));
        let first_body: Value = serde_json::from_slice(&requests[0].body).expect("request body");
        let system_message = first_body["messages"]
            .as_array()
            .and_then(|messages| messages.first())
            .expect("system message")
            .to_string();
        assert!(system_message.contains("You are Mycel"));
        assert!(system_message.contains("headless-review"));
        assert!(system_message.contains("Working-directory snapshot"));
        assert!(!system_message.contains("KIMI_"));
        let first_tools = first_body["tools"]
            .as_array()
            .expect("headless tool definitions")
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect::<Vec<_>>();
        for name in [
            "AskUserQuestion",
            "EnterPlanMode",
            "ExitPlanMode",
            "ReadMediaFile",
            "Skill",
            "TodoList",
        ] {
            assert!(
                first_tools.contains(&name),
                "headless provider request must expose {name}"
            );
        }
        let second_body: Value = serde_json::from_slice(&requests[1].body).expect("request body");
        let messages = second_body["messages"].as_array().expect("messages");
        assert!(messages
            .iter()
            .any(|message| message["content"] == "first answer"));
        assert_eq!(
            source.paths.lock().expect("paths").as_slice(),
            &[
                temp.path().join("mycel/config.toml"),
                temp.path().join("mycel/config.toml")
            ]
        );
    }

    #[test]
    fn session_resolution_merges_roots_and_guards_picker_cross_cwd() {
        let temp = TempDir::new().expect("temp");
        let home = temp.path().join("mycel");
        let work_dir = temp.path().join("work");
        let other_dir = temp.path().join("other");
        let persisted_root = temp.path().join("persisted");
        let requested_root = temp.path().join("requested");
        for directory in [&work_dir, &other_dir, &persisted_root, &requested_root] {
            fs::create_dir(directory).expect("test directory");
        }
        let records = home.join("sessions/session-1/agents/main/records.jsonl");
        fs::create_dir_all(records.parent().expect("records parent")).expect("session directory");
        fs::write(
            &records,
            "{\"type\":\"metadata\",\"protocol_version\":\"1.4\",\"created_at\":1}\n",
        )
        .expect("records");
        let index = SessionIndex::new(&home);
        index
            .register_session(
                "session-1",
                &work_dir,
                std::slice::from_ref(&persisted_root),
            )
            .expect("indexed session");

        let continued = resolve_session_selection(
            &index,
            &work_dir,
            &SessionSelection::Continue,
            std::slice::from_ref(&requested_root),
            None,
            "test",
        )
        .expect("continue selection")
        .expect("selected session");
        assert_eq!(
            continued.session,
            SessionSelection::Resume("session-1".to_owned())
        );
        assert_eq!(
            continued.additional_dirs,
            vec![
                fs::canonicalize(&persisted_root).expect("canonical persisted root"),
                requested_root,
            ]
        );

        let cancelled = FixedPicker {
            selected: None,
            seen: Mutex::new(Vec::new()),
        };
        assert!(resolve_session_selection(
            &index,
            &work_dir,
            &SessionSelection::Pick,
            &[],
            Some(&cancelled),
            "test",
        )
        .expect("picker cancellation")
        .is_none());
        assert_eq!(cancelled.seen.lock().expect("picker sessions").len(), 1);

        let cross_cwd_picker = FixedPicker {
            selected: Some("session-1".to_owned()),
            seen: Mutex::new(Vec::new()),
        };
        let error = resolve_session_selection(
            &index,
            &other_dir,
            &SessionSelection::Pick,
            &[],
            Some(&cross_cwd_picker),
            "test",
        )
        .expect_err("cross-cwd picker selection");
        let message = error.to_string();
        assert!(message.contains("different directory"));
        assert!(message.contains("cd '"));
        assert!(message.contains("mycel --resume 'session-1'"));
    }

    #[test]
    fn interactive_memory_terminal_runs_new_and_explicit_resume_turns_and_restores() {
        let temp = TempDir::new().expect("temp");
        let source = Arc::new(RecordingConfig {
            source: config(),
            paths: Mutex::new(Vec::new()),
        });
        let transport = Arc::new(ScriptedTransport::default());
        transport.respond_after(Duration::from_millis(150), "first interactive answer");
        transport.respond_after(Duration::from_millis(150), "resumed interactive answer");
        let adapter = adapter(&temp, source, Arc::clone(&transport));

        let first_request = interactive(SessionSelection::New, PermissionMode::Manual);
        let first_prepared = adapter
            .prepare_interactive(&first_request)
            .expect("prepare first session");
        let first_output = Arc::new(Mutex::new(Vec::new()));
        let first_restored = Arc::new(AtomicBool::new(false));
        let mut first_backend = MemoryBackend::scripted(
            std::iter::once(BackendEvent::Input(b"hello\r".to_vec()))
                .chain(std::iter::repeat_n(BackendEvent::Timeout, 4))
                .chain(std::iter::once(BackendEvent::Input(vec![0x04]))),
        );
        first_backend.output = Arc::clone(&first_output);
        first_backend.restored = Arc::clone(&first_restored);
        let mut first_driver = TerminalDriver::new(first_backend);
        let first = adapter
            .run_prepared_interactive(first_prepared, &mut first_driver)
            .expect("first interactive turn");
        let session_id = first.session_id().expect("session id").to_owned();
        let first_bytes = first_output.lock().expect("first output").clone();
        let first_text = String::from_utf8_lossy(&first_bytes);
        assert!(first_text.contains("first interactive answer"));
        assert!(first_bytes
            .windows(DISABLE_BRACKETED_PASTE.len())
            .any(|window| window == DISABLE_BRACKETED_PASTE));
        assert!(first_bytes
            .windows(LEAVE_ALTERNATE_SCREEN.len())
            .any(|window| window == LEAVE_ALTERNATE_SCREEN));
        assert!(first_restored.load(Ordering::SeqCst));

        let resumed_request = interactive(
            SessionSelection::Resume(session_id.clone()),
            PermissionMode::Yolo,
        );
        let resumed_prepared = adapter
            .prepare_interactive(&resumed_request)
            .expect("prepare resumed session");
        assert_eq!(
            adapter
                .executor
                .block_on(resumed_prepared.session.snapshot())
                .state
                .permission_mode,
            ProtocolPermissionMode::Yolo
        );
        let resumed_output = Arc::new(Mutex::new(Vec::new()));
        let resumed_restored = Arc::new(AtomicBool::new(false));
        let mut resumed_backend = MemoryBackend::scripted(
            std::iter::once(BackendEvent::Input(b"again\r".to_vec()))
                .chain(std::iter::repeat_n(BackendEvent::Timeout, 4))
                .chain(std::iter::once(BackendEvent::Input(vec![0x04]))),
        );
        resumed_backend.output = Arc::clone(&resumed_output);
        resumed_backend.restored = Arc::clone(&resumed_restored);
        let mut resumed_driver = TerminalDriver::new(resumed_backend);
        let resumed = adapter
            .run_prepared_interactive(resumed_prepared, &mut resumed_driver)
            .expect("resumed interactive turn");
        assert_eq!(resumed.session_id(), Some(session_id.as_str()));
        assert!(
            String::from_utf8_lossy(&resumed_output.lock().expect("resumed output"))
                .contains("resumed interactive answer")
        );
        assert!(resumed_restored.load(Ordering::SeqCst));

        let requests = transport.requests.lock().expect("requests");
        assert_eq!(requests.len(), 2);
        let resumed_body: Value =
            serde_json::from_slice(&requests[1].body).expect("resumed request body");
        assert!(resumed_body["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .any(|message| message["content"] == "first interactive answer"));
    }

    #[test]
    fn interactive_new_and_reload_apply_through_the_production_lifecycle() {
        let temp = TempDir::new().expect("temp");
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            Arc::new(ScriptedTransport::default()),
        );
        let output = Arc::new(Mutex::new(Vec::new()));
        let restored = Arc::new(AtomicBool::new(false));
        let mut backend = MemoryBackend::scripted([
            BackendEvent::Input(b"/title source session\r".to_vec()),
            BackendEvent::Input(b"/new\r".to_vec()),
            BackendEvent::Input(b"/title replacement session\r".to_vec()),
            BackendEvent::Input(b"/reload\r".to_vec()),
            BackendEvent::Input(vec![0x04]),
        ]);
        backend.output = Arc::clone(&output);
        backend.restored = Arc::clone(&restored);
        let mut driver = TerminalDriver::new(backend);

        let completion = adapter
            .run_interactive_with_terminal(
                &interactive(SessionSelection::New, PermissionMode::Manual),
                &mut driver,
            )
            .expect("new and reload lifecycle");
        let active_id = completion.session_id().expect("replacement session id");
        let sessions = SessionIndex::new(temp.path().join("mycel"))
            .list(None)
            .expect("session index")
            .sessions;
        assert_eq!(sessions.len(), 2);
        assert_eq!(
            sessions
                .iter()
                .find(|session| session.id == active_id)
                .and_then(|session| session.title.as_deref()),
            Some("replacement session")
        );
        assert!(sessions
            .iter()
            .any(|session| session.title.as_deref() == Some("source session")));
        assert!(restored.load(Ordering::SeqCst));
        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("source session"));
        assert!(rendered.contains("replacement session"));
    }

    #[test]
    fn interactive_fork_and_sessions_switch_preserve_validated_history() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        transport.respond("durable source answer");
        transport.respond("other session answer");
        let mut adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            Arc::clone(&transport),
        );
        let source = adapter
            .run_prompt(
                &prompt(SessionSelection::New),
                &mut CollectingSink::default(),
            )
            .expect("source session");
        let source_id = source.session_id().expect("source id").to_owned();
        let other = adapter
            .run_prompt(
                &prompt(SessionSelection::New),
                &mut CollectingSink::default(),
            )
            .expect("other session");
        let other_id = other.session_id().expect("other id").to_owned();

        let output = Arc::new(Mutex::new(Vec::new()));
        let mut backend = MemoryBackend::scripted([
            BackendEvent::Input(b"/fork\r".to_vec()),
            BackendEvent::Input(format!("/sessions {other_id}\r").into_bytes()),
            BackendEvent::Input(vec![0x04]),
        ]);
        backend.output = Arc::clone(&output);
        let mut driver = TerminalDriver::new(backend);
        let completion = adapter
            .run_interactive_with_terminal(
                &interactive(
                    SessionSelection::Resume(source_id.clone()),
                    PermissionMode::Manual,
                ),
                &mut driver,
            )
            .expect("fork and switch");
        assert_eq!(completion.session_id(), Some(other_id.as_str()));

        let index = SessionIndex::new(temp.path().join("mycel"));
        let sessions = index.list(None).expect("sessions").sessions;
        assert_eq!(sessions.len(), 3);
        let fork = sessions
            .iter()
            .find(|session| {
                session
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("forkedFrom"))
                    .and_then(Value::as_str)
                    == Some(source_id.as_str())
            })
            .expect("fork metadata");
        assert!(fork
            .title
            .as_deref()
            .is_some_and(|title| title.starts_with("Fork: ")));
        let fork_records = fs::read_to_string(
            temp.path()
                .join("mycel/sessions")
                .join(&fork.id)
                .join("agents/main/records.jsonl"),
        )
        .expect("fork records");
        assert!(fork_records.contains("durable source answer"));
        assert!(String::from_utf8_lossy(&output.lock().expect("output")).contains("session "));
    }

    #[test]
    fn interactive_model_reconfiguration_and_effort_reach_the_next_provider_request() {
        let temp = TempDir::new().expect("temp");
        let source = config().replace(
            "[thinking]",
            "[models.other]\nprovider = \"local\"\nmodel = \"gpt-other\"\nmax_context_size = 8192\nmax_output_size = 128\ncapabilities = [\"thinking\"]\nsupport_efforts = [\"low\", \"high\"]\n\n[thinking]",
        );
        let transport = Arc::new(ScriptedTransport::default());
        transport.respond("model switch answer");
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source,
                paths: Mutex::new(Vec::new()),
            }),
            Arc::clone(&transport),
        );
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut backend = MemoryBackend::scripted(
            [
                BackendEvent::Input(b"/model other\r".to_vec()),
                BackendEvent::Input(b"/effort low\r".to_vec()),
                BackendEvent::Input(b"use the other model\r".to_vec()),
            ]
            .into_iter()
            .chain(std::iter::repeat_n(BackendEvent::Timeout, 4))
            .chain(std::iter::once(BackendEvent::Input(vec![0x04]))),
        );
        backend.output = Arc::clone(&output);
        let mut driver = TerminalDriver::new(backend);

        adapter
            .run_interactive_with_terminal(
                &interactive(SessionSelection::New, PermissionMode::Manual),
                &mut driver,
            )
            .expect("model and effort switch");
        let requests = transport.requests.lock().expect("requests");
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).expect("request body");
        assert_eq!(body["model"], "gpt-other");
        assert_eq!(body["reasoning_effort"], "low");
        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("thinking set to low for this session"));
        assert!(rendered.contains("model switch answer"));
    }

    #[test]
    fn interactive_swarm_mode_is_durable_and_changes_the_next_provider_contract() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        transport.respond("swarm task answer");
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut backend = MemoryBackend::scripted(
            [
                BackendEvent::Input(b"/swarm on\r".to_vec()),
                BackendEvent::Input(b"parallel review\r".to_vec()),
            ]
            .into_iter()
            .chain(std::iter::repeat_n(BackendEvent::Timeout, 2))
            .chain(std::iter::once(BackendEvent::Input(vec![0x04]))),
        )
        .wait_after_events_for_requests(2, transport.request_counter(), 1);
        backend.output = output.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("swarm interactive turn");

        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("swarm mode enabled"));
        assert!(rendered.contains("swarm task answer"));
        let requests = transport.requests.lock().expect("requests");
        let body: Value = serde_json::from_slice(&requests[0].body).expect("provider body");
        let system_message = body["messages"]
            .as_array()
            .and_then(|messages| messages.first())
            .expect("system message")
            .to_string();
        assert!(system_message.contains("Swarm mode is active"));
    }

    /// Ctrl-D once during a stalled provider turn, then stdin closes. Ctrl-D
    /// once means "exit after the current turn"; before this the EndOfInput
    /// arm then slept in a loop with NO deadline while
    /// `exit_after_turn && active.is_some()`, so a provider that never answers
    /// made the session unkillable - and wedged the CI runner. Now the wait
    /// for the in-flight turn is bounded: a grace period for it to finish on
    /// its own, then it is cancelled and the session exits.
    #[test]
    fn interactive_quit_during_stalled_turn_exits_within_the_grace_bound() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        // A provider that will not answer within any bound the test tolerates.
        transport.respond_after(Duration::from_secs(3600), "never arrives");
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        // Prompt starts a turn that stalls; ONE Ctrl-D lands while it is in
        // flight (= exit after turn, keep waiting); then the script exhausts
        // -> EndOfInput with exit_after_turn set and an active turn: exactly
        // the previously-unbounded path.
        let backend = MemoryBackend::scripted([
            BackendEvent::Input(b"answer me\r".to_vec()),
            BackendEvent::Input(vec![0x04]),
        ])
        .wait_after_events_for_requests(1, transport.request_counter(), 1);
        let restored = backend.restored.clone();
        let mut driver = TerminalDriver::new(backend);

        let started = Instant::now();
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = adapter.run_prepared_interactive(prepared, &mut driver);
            let _ = done_tx.send(result.map(|_| ()));
        });
        // The exit must complete well within the grace bound plus join bound;
        // 30s is far above both and far below "hangs forever".
        let result = done_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("quit during a stalled turn must not hang the session");
        result.expect("stalled-turn quit should exit cleanly");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "exit took {:?}",
            started.elapsed()
        );
        assert!(restored.load(Ordering::SeqCst), "terminal must be restored");
    }

    #[test]
    fn interactive_copy_emits_bounded_osc52_and_q_exits_cleanly() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        transport.respond("copy this answer");
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        let output = Arc::new(Mutex::new(Vec::new()));
        // Two blind Timeout ticks used to sit between the prompt and /copy.
        // Under test-thread contention the turn outlasted the script: the
        // backend hit EndOfInput and the session exited mid-turn before /copy
        // ever produced its effect (a flaky assertion here). Do not let the
        // script exhaust while the effect is pending: wait (bounded) for the
        // answer to render before /copy, and for the copy status before /q.
        let mut backend = MemoryBackend::scripted([
            BackendEvent::Input(b"answer me\r".to_vec()),
            BackendEvent::Input(b"/copy\r".to_vec()),
            BackendEvent::Input(b"/q\r".to_vec()),
        ])
        .wait_after_events_for_requests(1, transport.request_counter(), 1)
        .wait_after_events_for_output(1, b"copy this answer".to_vec())
        .wait_after_events_for_output(2, b"Copied via terminal escape".to_vec());
        backend.output = output.clone();
        let restored = backend.restored.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("copy and exit");

        let output = output.lock().expect("output");
        let expected = format!(
            "\u{1b}]52;c;{}\u{7}",
            BASE64_STANDARD.encode("copy this answer")
        );
        let rendered = String::from_utf8_lossy(&output);
        assert!(
            output
                .windows(expected.len())
                .any(|window| window == expected.as_bytes()),
            "OSC52 copy escape missing from output. saw 'No assistant message': {}, saw \
             'Copied via': {}. rendered tail: {:?}",
            rendered.contains("No assistant message to copy"),
            rendered.contains("Copied via terminal escape"),
            &rendered[rendered.len().saturating_sub(600)..]
        );
        assert!(
            rendered.contains("Copied via terminal escape sequence (unverified, 16 characters).")
        );
        assert!(restored.load(Ordering::SeqCst));
    }

    #[test]
    fn interactive_init_runs_the_retained_agents_prompt_in_a_native_child() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        transport.respond("AGENTS.md initialized");
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        // Wait for the native child's answer to render before Ctrl-D, rather
        // than trusting two blind Timeout ticks to outlast it under contention.
        let backend = MemoryBackend::scripted([
            BackendEvent::Input(b"/init\r".to_vec()),
            BackendEvent::Input(vec![0x04]),
        ])
        .wait_after_events_for_requests(1, transport.request_counter(), 1)
        .wait_after_events_for_output(1, b"AGENTS.md initialized".to_vec());
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("init command");

        let requests = transport.requests.lock().expect("requests");
        assert_eq!(requests.len(), 1, "init should use only its native child");
        let body: Value = serde_json::from_slice(&requests[0].body).expect("provider body");
        assert!(body["messages"]
            .to_string()
            .contains("project-root `AGENTS.md`"));
        assert!(body["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .any(|tool| tool["function"]["name"] == "Write"));
    }

    #[test]
    fn interactive_ctrl_s_steers_the_active_provider_turn() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        transport.respond_after(Duration::from_millis(200), "first answer");
        transport.respond("redirected answer");
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut backend = MemoryBackend::scripted(
            [
                BackendEvent::Input(b"initial\r".to_vec()),
                BackendEvent::Input(b"redirect".to_vec()),
                BackendEvent::Input(vec![0x13]),
            ]
            .into_iter()
            .chain(std::iter::repeat_n(BackendEvent::Timeout, 2))
            .chain(std::iter::once(BackendEvent::Input(vec![0x04]))),
        )
        .wait_after_events_for_requests(1, transport.request_counter(), 1);
        backend.output = output.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("steered interactive turn");

        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("steered 1 message into the current turn"));
        assert!(rendered.contains("first answer"));
        assert!(
            rendered.contains("redirected answer"),
            "terminal output omitted the redirected answer: {rendered:?}"
        );
        let requests = transport.requests.lock().expect("requests");
        assert_eq!(requests.len(), 2);
        let second: Value = serde_json::from_slice(&requests[1].body).expect("second request");
        assert!(second["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .any(|message| message["content"] == "redirect"));
    }

    #[test]
    fn interactive_ctrl_b_detaches_the_exact_running_native_subagent() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        transport.respond_tool_call(
            "agent-call",
            "Agent",
            serde_json::json!({
                "prompt":"run until detached",
                "description":"foreground child"
            }),
        );
        transport.respond_after(Duration::from_millis(200), "child answer");
        transport.respond("parent after detach");
        let adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut backend = MemoryBackend::scripted(
            [
                BackendEvent::Input(b"delegate something\r".to_vec()),
                BackendEvent::Input(vec![0x02]),
            ]
            .into_iter()
            .chain(std::iter::repeat_n(BackendEvent::Timeout, 2))
            .chain(std::iter::once(BackendEvent::Input(vec![0x04]))),
        )
        .wait_after_events_for_requests(1, transport.request_counter(), 2);
        backend.output = output.clone();
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("detached interactive subagent");

        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("Moved 1 task to background. /tasks to view."));
        assert!(rendered.contains("parent after detach"));
        assert_eq!(transport.requests.lock().expect("requests").len(), 3);
    }

    #[test]
    fn interactive_signal_maps_exit_and_restores_terminal() {
        let temp = TempDir::new().expect("temp");
        write_test_mcp_config(&temp);
        let source = Arc::new(RecordingConfig {
            source: config(),
            paths: Mutex::new(Vec::new()),
        });
        let transport = Arc::new(ScriptedTransport::default());
        let peer = ScriptedMcpPeer::modern_with_tool();
        let connector: Arc<dyn McpTransportConnector> =
            Arc::new(OneHttpMcpConnector::new(peer.clone()));
        let factory = TestMcpConnectorFactory::fixed(connector);
        let adapter = adapter_with_mcp(&temp, source, transport, factory.clone());
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Auto))
            .expect("prepare");
        let restored = Arc::new(AtomicBool::new(false));
        let mut backend =
            MemoryBackend::scripted([BackendEvent::Signal(TerminalSignal::Interrupt)]);
        backend.restored = Arc::clone(&restored);
        let mut driver = TerminalDriver::new(backend);
        let completion = adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("signal completion");
        assert_eq!(
            completion,
            RuntimeCompletion::Signal(TerminationSignal::Interrupt)
        );
        assert!(restored.load(Ordering::SeqCst));
        assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            peer.close_count.load(Ordering::SeqCst),
            1,
            "signal cleanup must close the session MCP transport exactly once"
        );
    }

    #[test]
    fn interactive_dialog_response_restores_terminal_on_clean_exit() {
        let temp = TempDir::new().expect("temp");
        let source = Arc::new(RecordingConfig {
            source: config(),
            paths: Mutex::new(Vec::new()),
        });
        let adapter = adapter(&temp, source, Arc::new(ScriptedTransport::default()));
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Manual))
            .expect("prepare");
        let dialog_port = Arc::clone(&prepared.dialog_port);
        let approval = dialog_port.request_approval(approval_request("terminal-approval"));
        let output = Arc::new(Mutex::new(Vec::new()));
        let restored = Arc::new(AtomicBool::new(false));
        let mut backend = MemoryBackend::scripted([BackendEvent::Input(vec![b'1', 0x04])]);
        backend.output = Arc::clone(&output);
        backend.restored = Arc::clone(&restored);
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("dialog and clean exit");
        let approval = adapter
            .executor
            .block_on(approval)
            .expect("approval response");
        assert_eq!(approval.decision, ProtocolApprovalDecision::Approved);
        assert_eq!(approval.scope, None);
        assert!(String::from_utf8_lossy(&output.lock().expect("output")).contains("Approve once"));
        assert!(restored.load(Ordering::SeqCst));
    }

    #[test]
    fn interactive_signal_cancels_pending_dialog_and_restores_terminal() {
        let temp = TempDir::new().expect("temp");
        let source = Arc::new(RecordingConfig {
            source: config(),
            paths: Mutex::new(Vec::new()),
        });
        let adapter = adapter(&temp, source, Arc::new(ScriptedTransport::default()));
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Manual))
            .expect("prepare");
        let dialog_port = Arc::clone(&prepared.dialog_port);
        let approval = dialog_port.request_approval(approval_request("signal-approval"));
        let restored = Arc::new(AtomicBool::new(false));
        let mut backend =
            MemoryBackend::scripted([BackendEvent::Signal(TerminalSignal::Terminate)]);
        backend.restored = Arc::clone(&restored);
        let mut driver = TerminalDriver::new(backend);
        let completion = adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("signal completion");
        assert_eq!(
            completion,
            RuntimeCompletion::Signal(TerminationSignal::Terminate)
        );
        let error = adapter
            .executor
            .block_on(approval)
            .expect_err("pending approval must be cancelled");
        assert!(error.message.contains("session closed"));
        assert!(restored.load(Ordering::SeqCst));
    }

    #[test]
    fn interactive_ctrl_c_cancels_an_active_provider_turn() {
        let temp = TempDir::new().expect("temp");
        let source = Arc::new(RecordingConfig {
            source: config(),
            paths: Mutex::new(Vec::new()),
        });
        let transport = Arc::new(PendingTransport::default());
        let adapter = ProductionRuntimeAdapter::with_components(
            Arc::new(FixedHome(temp.path().join("mycel"))),
            source,
            Arc::new(TestEnvironment::default()),
            transport.clone(),
        )
        .expect("adapter");
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Manual))
            .expect("prepare");
        let output = Arc::new(Mutex::new(Vec::new()));
        let restored = Arc::new(AtomicBool::new(false));
        let mut backend = MemoryBackend::scripted([
            BackendEvent::Input(b"wait\r".to_vec()),
            BackendEvent::Timeout,
            BackendEvent::Input(vec![0x03]),
            BackendEvent::Timeout,
            BackendEvent::Timeout,
            BackendEvent::Input(vec![0x04]),
        ]);
        backend.output = Arc::clone(&output);
        backend.restored = Arc::clone(&restored);
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("cancelled interactive session");
        let rendered = String::from_utf8_lossy(&output.lock().expect("output")).into_owned();
        assert!(rendered.contains("cancelling current turn"));
        assert!(restored.load(Ordering::SeqCst));
    }

    #[test]
    fn interactive_render_error_still_restores_terminal_and_closes_session() {
        let temp = TempDir::new().expect("temp");
        let source = Arc::new(RecordingConfig {
            source: config(),
            paths: Mutex::new(Vec::new()),
        });
        let transport = Arc::new(ScriptedTransport::default());
        let adapter = adapter(&temp, source, transport);
        let prepared = adapter
            .prepare_interactive(&interactive(SessionSelection::New, PermissionMode::Manual))
            .expect("prepare");
        let restored = Arc::new(AtomicBool::new(false));
        let mut backend = MemoryBackend::scripted([BackendEvent::EndOfInput]);
        backend.restored = Arc::clone(&restored);
        // Three writes activate the terminal. The next write is the first
        // differential-render command.
        backend.fail_write_at = Some(4);
        let mut driver = TerminalDriver::new(backend);
        let error = adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect_err("render failure");
        assert!(error
            .to_string()
            .contains("injected terminal write failure"));
        assert!(restored.load(Ordering::SeqCst));
    }

    #[test]
    fn local_tool_builder_honors_additional_roots_for_prompt_and_interactive() {
        let temp = TempDir::new().expect("temp");
        let workspace = temp.path().join("workspace");
        let additional = temp.path().join("additional");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&additional).expect("additional");
        let registry = LocalToolRegistryBuilder
            .build(&workspace, std::slice::from_ref(&additional), &[], None)
            .expect("local tools");
        let snapshot = registry.snapshot();
        let names = snapshot
            .definitions()
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["Bash", "Edit", "Glob", "Grep", "Read", "Write"]);

        let mut interactive_request = interactive(SessionSelection::New, PermissionMode::Manual);
        interactive_request.add_dirs.push(additional.clone());

        let mut prompt_request = prompt(SessionSelection::New);
        prompt_request.add_dirs.push(additional.clone());
        assert!(validate_supported_prompt(&prompt_request).is_ok());

        let source = Arc::new(RecordingConfig {
            source: config(),
            paths: Mutex::new(Vec::new()),
        });
        let transport = Arc::new(ScriptedTransport::default());
        transport.respond("add-dir prompt answer");
        let mut adapter = adapter(&temp, source, transport);
        let completion = adapter
            .run_prompt(&prompt_request, &mut CollectingSink::default())
            .expect("headless add-dir turn");
        assert!(completion.session_id().is_some());

        let prepared = adapter
            .prepare_interactive(&interactive_request)
            .expect("interactive add-dir preparation");
        let restored = Arc::new(AtomicBool::new(false));
        let mut backend = MemoryBackend::scripted([BackendEvent::EndOfInput]);
        backend.restored = Arc::clone(&restored);
        let mut driver = TerminalDriver::new(backend);
        adapter
            .run_prepared_interactive(prepared, &mut driver)
            .expect("interactive add-dir session");
        assert!(restored.load(Ordering::SeqCst));
    }

    #[test]
    fn unknown_model_fails_before_terminal_construction() {
        let temp = TempDir::new().expect("temp");
        let source = Arc::new(RecordingConfig {
            source: config(),
            paths: Mutex::new(Vec::new()),
        });
        let transport = Arc::new(ScriptedTransport::default());
        let adapter = adapter(&temp, source, transport);
        let mut request = interactive(SessionSelection::New, PermissionMode::Manual);
        request.model = Some("missing".to_owned());
        let error = match adapter.prepare_interactive(&request) {
            Ok(_) => panic!("unknown model must fail during preparation"),
            Err(error) => error,
        };
        assert!(error
            .to_string()
            .contains("Model \"missing\" is not configured"));
        assert!(!temp.path().join("mycel/sessions").exists());
    }

    #[test]
    fn headless_goal_completes_natively_and_missing_continue_fails_before_transport() {
        let temp = TempDir::new().expect("temp");
        let source = Arc::new(RecordingConfig {
            source: config(),
            paths: Mutex::new(Vec::new()),
        });
        let transport = Arc::new(ScriptedTransport::default());
        let mut adapter = adapter(&temp, source, Arc::clone(&transport));

        let error = adapter
            .run_prompt(
                &prompt(SessionSelection::Continue),
                &mut CollectingSink::default(),
            )
            .expect_err("continue requires an indexed session");
        assert!(error.to_string().contains("No previous session was found"));
        assert!(transport.requests.lock().expect("requests").is_empty());

        transport.respond_tool_call(
            "goal-complete",
            "UpdateGoal",
            serde_json::json!({"action":"complete","reason":"objective satisfied"}),
        );
        let mut goal = prompt(SessionSelection::New);
        goal.prompt = "/goal do it".to_owned();
        goal.goal = Some(GoalCreateRequest {
            objective: "do it".to_owned(),
            replace: false,
        });
        let mut events = CollectingSink::default();
        let completion = adapter
            .run_prompt(&goal, &mut events)
            .expect("native headless goal");
        assert!(matches!(
            completion,
            RuntimeCompletion::Goal {
                status: GoalStatus::Complete,
                ..
            }
        ));
        assert!(events.0.iter().any(|event| matches!(
            event,
            HeadlessEvent::GoalSummary {
                status: Some(status),
                reason: Some(reason),
                turns_used: Some(1),
                ..
            } if status == "complete" && reason == "objective satisfied"
        )));
        assert_eq!(transport.requests.lock().expect("requests").len(), 1);
    }

    #[test]
    fn active_headless_goal_runs_continuation_turns_until_terminal() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(ScriptedTransport::default());
        transport.respond("first turn still working");
        transport.respond_tool_call(
            "goal-complete",
            "UpdateGoal",
            serde_json::json!({"action":"complete","reason":"finished on continuation"}),
        );
        let mut adapter = adapter(
            &temp,
            Arc::new(RecordingConfig {
                source: config(),
                paths: Mutex::new(Vec::new()),
            }),
            transport.clone(),
        );
        let mut request = prompt(SessionSelection::New);
        request.goal = Some(GoalCreateRequest {
            objective: "finish in two turns".to_owned(),
            replace: false,
        });
        let mut events = CollectingSink::default();
        let completion = adapter
            .run_prompt(&request, &mut events)
            .expect("continued goal");

        assert!(matches!(
            completion,
            RuntimeCompletion::Goal {
                status: GoalStatus::Complete,
                ..
            }
        ));
        assert_eq!(transport.requests.lock().expect("requests").len(), 2);
        assert!(events.0.iter().any(|event| matches!(
            event,
            HeadlessEvent::GoalSummary {
                turns_used: Some(2),
                reason: Some(reason),
                ..
            } if reason == "finished on continuation"
        )));
    }

    #[test]
    fn missing_default_model_fails_before_provider_or_session_creation() {
        let temp = TempDir::new().expect("temp");
        let source = Arc::new(RecordingConfig {
            source: config().replace("default_model = \"local\"", ""),
            paths: Mutex::new(Vec::new()),
        });
        let transport = Arc::new(ScriptedTransport::default());
        let mut adapter = adapter(&temp, source, Arc::clone(&transport));
        let error = adapter
            .run_prompt(
                &prompt(SessionSelection::New),
                &mut CollectingSink::default(),
            )
            .expect_err("default model is required");
        assert!(error.to_string().contains("No default_model configured"));
        assert!(transport.requests.lock().expect("requests").is_empty());
        assert!(!temp.path().join("mycel/sessions").exists());
    }

    #[test]
    fn goal_projection_maps_terminal_exit_status_without_inventing_completion() {
        let snapshot = |status| GoalSnapshot {
            goal_id: "g1".to_owned(),
            objective: "objective".to_owned(),
            completion_criterion: None,
            status,
            turns_used: 2,
            tokens_used: 3,
            wall_clock_ms: 4,
            budget: GoalBudgetReport {
                token_budget: None,
                turn_budget: None,
                wall_clock_budget_ms: None,
                remaining_tokens: None,
                remaining_turns: None,
                remaining_wall_clock_ms: None,
                token_budget_reached: false,
                turn_budget_reached: false,
                wall_clock_budget_reached: false,
                over_budget: false,
            },
            terminal_reason: Some("done".to_owned()),
        };
        let cases = [
            (ProtocolGoalStatus::Complete, Some(GoalStatus::Complete)),
            (ProtocolGoalStatus::Blocked, Some(GoalStatus::Blocked)),
            (ProtocolGoalStatus::Paused, Some(GoalStatus::Paused)),
            (ProtocolGoalStatus::Active, None),
        ];
        for (protocol, expected) in cases {
            let mut sink = CollectingSink::default();
            let mut terminal = None;
            project_event(
                AgentEvent::GoalUpdated {
                    snapshot: Some(snapshot(protocol)),
                    change: None,
                },
                &mut sink,
                &mut terminal,
            )
            .expect("projection");
            assert_eq!(terminal, expected);
            assert!(matches!(
                sink.0.as_slice(),
                [HeadlessEvent::GoalSummary { .. }]
            ));
        }
    }
}
