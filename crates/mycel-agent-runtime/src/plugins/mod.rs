//! Confined local plugin manifests and declarative contribution plans.
//!
//! This module never downloads, extracts, executes, or opens a network
//! connection. A plugin is an explicitly registered local directory whose
//! manifest can contribute namespaced skill roots, MCP connection plans, and
//! argv-based subprocess descriptors. The host remains responsible for policy
//! and execution.

use crate::skills::{FileMetadata, SkillFileSystem, SkillRoot, SkillSource, StdSkillFileSystem};
use mycel_agent_protocol::{
    McpAuth, McpCommonConfig, McpServerConfig, ToolDefinition, ToolInputDisplay,
};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

use crate::local_builtins::{
    output::{error_result, OutputBuffer},
    process::{run_process, ProcessRequest},
};
use crate::{
    ExecutableTool, PlanPolicy, ToolAccess, ToolError, ToolExecutionSpec, ToolFuture,
    ToolInvocation, ToolPrepareContext,
};

const ROOT_MANIFEST: &str = "mycel.plugin.json";
const NESTED_MANIFEST: &str = ".mycel-plugin/plugin.json";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PluginLimits {
    pub max_plugins: usize,
    pub max_manifest_bytes: u64,
    pub max_contributions_per_plugin: usize,
}

impl Default for PluginLimits {
    fn default() -> Self {
        Self {
            max_plugins: 256,
            max_manifest_bytes: 1024 * 1024,
            max_contributions_per_plugin: 256,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginRegistration {
    /// Must be an absolute local directory. URL and archive registrations are
    /// intentionally unsupported.
    pub root: PathBuf,
    pub enabled: bool,
    pub disabled_mcp_servers: BTreeSet<String>,
    /// Optional store-level identity. When present, the manifest name must
    /// match so replacing a managed root cannot silently change the installed
    /// plugin identity.
    pub expected_id: Option<String>,
}

impl PluginRegistration {
    pub fn enabled(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            enabled: true,
            disabled_mcp_servers: BTreeSet::new(),
            expected_id: None,
        }
    }

    pub fn with_expected_id(mut self, id: impl Into<String>) -> Self {
        self.expected_id = Some(id.into());
        self
    }
}

/// PATH lookup boundary. It returns the concrete executable path but never
/// starts a process.
pub trait ExecutableResolver: Send + Sync {
    fn resolve(&self, executable: &str) -> io::Result<Option<PathBuf>>;
}

#[derive(Clone, Debug)]
pub struct PathExecutableResolver {
    search_path: Option<OsString>,
}

impl PathExecutableResolver {
    pub fn ambient() -> Self {
        Self {
            search_path: env::var_os("PATH"),
        }
    }

    pub fn new(search_path: Option<OsString>) -> Self {
        Self { search_path }
    }
}

impl ExecutableResolver for PathExecutableResolver {
    fn resolve(&self, executable: &str) -> io::Result<Option<PathBuf>> {
        let Some(search_path) = self.search_path.as_deref() else {
            return Ok(None);
        };
        for directory in env::split_paths(search_path) {
            let candidate = directory.join(executable);
            match std::fs::metadata(&candidate) {
                Ok(metadata) if metadata.is_file() => {
                    return std::fs::canonicalize(candidate).map(Some)
                }
                Ok(_) => continue,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginInfo {
    pub id: String,
    pub version: String,
    pub description: Option<String>,
    pub root: PathBuf,
    pub enabled: bool,
    pub skill_roots: usize,
    pub mcp_servers: Vec<String>,
    pub commands: Vec<String>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct PluginProcessDescriptor {
    pub executable: PathBuf,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}

impl fmt::Debug for PluginProcessDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginProcessDescriptor")
            .field("executable", &self.executable)
            .field("argv", &format_args!("[{} arguments]", self.argv.len()))
            .field("cwd", &self.cwd)
            .field(
                "env",
                &self
                    .env
                    .keys()
                    .map(|key| (key, "[REDACTED]"))
                    .collect::<BTreeMap<_, _>>(),
            )
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct PluginHttpDescriptor {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub bearer_token_env_var: Option<String>,
    pub auth: Option<McpAuth>,
}

impl fmt::Debug for PluginHttpDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginHttpDescriptor")
            .field("url", &self.url)
            .field("bearer_token_env_var", &self.bearer_token_env_var)
            .field("auth", &self.auth)
            .field(
                "headers",
                &self
                    .headers
                    .keys()
                    .map(|key| (key, "[REDACTED]"))
                    .collect::<BTreeMap<_, _>>(),
            )
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginMcpDescriptor {
    Stdio(PluginProcessDescriptor),
    StreamableHttp(PluginHttpDescriptor),
}

impl PluginMcpDescriptor {
    pub fn runtime_config(&self) -> McpServerConfig {
        match self {
            Self::Stdio(process) => McpServerConfig::Stdio {
                command: process.executable.to_string_lossy().into_owned(),
                args: process.argv.clone(),
                env: process.env.clone(),
                cwd: Some(process.cwd.to_string_lossy().into_owned()),
                common: McpCommonConfig::default(),
            },
            Self::StreamableHttp(http) => McpServerConfig::Http {
                url: http.url.clone(),
                headers: http.headers.clone(),
                bearer_token_env_var: http.bearer_token_env_var.clone(),
                auth: http.auth,
                common: McpCommonConfig::default(),
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginMcpPlan {
    /// Stable runtime name, namespaced as `<plugin>.<server>`.
    pub runtime_name: String,
    pub plugin_id: String,
    pub server_name: String,
    pub descriptor: PluginMcpDescriptor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginCommandPlan {
    /// Stable runtime name, namespaced as `<plugin>.<command>`.
    pub runtime_name: String,
    pub plugin_id: String,
    pub command_name: String,
    pub process: PluginProcessDescriptor,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PluginContributionPlan {
    pub skill_roots: Vec<SkillRoot>,
    pub mcp_servers: Vec<PluginMcpPlan>,
    pub commands: Vec<PluginCommandPlan>,
}

impl PluginContributionPlan {
    /// Direct merge input for `McpRuntime::connect_all`. Runtime names are already
    /// plugin-qualified; a host must still reject collisions with non-plugin
    /// configuration instead of silently replacing either side.
    pub fn runtime_mcp_configs(&self) -> BTreeMap<String, McpServerConfig> {
        self.mcp_servers
            .iter()
            .map(|server| {
                (
                    server.runtime_name.clone(),
                    server.descriptor.runtime_config(),
                )
            })
            .collect()
    }
}

/// One governed executable for all locally installed plugin commands. The
/// command name selects a descriptor that was already validated and confined
/// during plugin reload; free-form user text is appended as one argv element,
/// never parsed by a shell.
pub struct PluginCommandTool {
    commands: BTreeMap<String, PluginCommandPlan>,
    timeout: Duration,
}

impl PluginCommandTool {
    pub fn new(commands: Vec<PluginCommandPlan>) -> Result<Self, PluginCommandToolError> {
        let mut indexed = BTreeMap::new();
        for command in commands {
            let name = command.runtime_name.clone();
            if indexed.insert(name.clone(), command).is_some() {
                return Err(PluginCommandToolError::Duplicate(name));
            }
        }
        if indexed.is_empty() {
            return Err(PluginCommandToolError::Empty);
        }
        Ok(Self {
            commands: indexed,
            timeout: Duration::from_secs(10 * 60),
        })
    }

    pub fn command_names(&self) -> impl Iterator<Item = &str> {
        self.commands.keys().map(String::as_str)
    }

    fn command<'a>(&'a self, arguments: &Value) -> Result<&'a PluginCommandPlan, ToolError> {
        let name = arguments
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::Prepare("plugin command name is missing".to_owned()))?;
        self.commands
            .get(name)
            .ok_or_else(|| ToolError::Prepare(format!("plugin command {name:?} is not installed")))
    }
}

impl ExecutableTool for PluginCommandTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "PluginCommand".to_owned(),
            description: "Run an explicitly installed local plugin command without a shell."
                .to_owned(),
            parameters: json!({
                "type":"object",
                "properties":{
                    "command":{"type":"string", "minLength":1},
                    "arguments":{"type":"string"}
                },
                "required":["command"],
                "additionalProperties":false
            }),
            deferred: false,
        }
    }

    fn prepare(
        &self,
        arguments: &Value,
        _context: &ToolPrepareContext,
    ) -> Result<ToolExecutionSpec, ToolError> {
        let command = self.command(arguments)?;
        let user_arguments = arguments
            .get("arguments")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let mut display = format!("/{}", command.runtime_name.replace('.', ":"));
        if let Some(user_arguments) = user_arguments {
            display.push(' ');
            display.push_str(user_arguments);
        }
        let mut spec = ToolExecutionSpec::new(
            ToolInputDisplay::Command {
                command: display,
                cwd: Some(command.process.cwd.to_string_lossy().into_owned()),
                description: Some("local plugin command".to_owned()),
                language: None,
            },
            "PluginCommand",
        );
        spec.accesses = vec![ToolAccess::All];
        spec.description = Some(format!(
            "Running local plugin command {}",
            command.runtime_name
        ));
        spec.approval_rule = Some(format!("PluginCommand({})", command.runtime_name));
        spec.rule_subject = Some(command.runtime_name.clone());
        spec.plan_policy = PlanPolicy::NotInPlan;
        Ok(spec)
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation) -> ToolFuture<'a> {
        Box::pin(async move {
            self.validate_arguments(&invocation.arguments)?;
            let command = self.command(&invocation.arguments)?;
            let mut argv = command
                .process
                .argv
                .iter()
                .map(OsString::from)
                .collect::<Vec<_>>();
            if let Some(arguments) = invocation
                .arguments
                .get("arguments")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                argv.push(OsString::from(arguments));
            }
            let environment = command
                .process
                .env
                .iter()
                .map(|(name, value)| (name.as_str(), OsString::from(value)))
                .collect::<Vec<_>>();
            let outcome = match run_process(ProcessRequest {
                program: &command.process.executable,
                args: &argv,
                cwd: &command.process.cwd,
                env: &environment,
                timeout: self.timeout,
                cancellation: &invocation.cancellation,
                updates: Arc::clone(&invocation.updates),
                stream_updates: true,
            })
            .await
            {
                Ok(outcome) => outcome,
                Err(error) => return Ok(error_result(error)),
            };
            let mut output = OutputBuffer::default();
            output.push(&outcome.combined);
            if outcome.raw_truncated {
                output.push("\n[process output truncated at 10 MiB]");
            }
            if outcome.cancelled {
                return Ok(output.into_result(true, Some("Interrupted by user".to_owned())));
            }
            if outcome.timed_out {
                return Ok(output
                    .into_result(true, Some("Plugin command timed out after 600s".to_owned())));
            }
            let exit_code = outcome.exit_code.unwrap_or(-1);
            if exit_code != 0 {
                return Ok(output.into_result(
                    true,
                    Some(format!("Plugin command exited with code {exit_code}")),
                ));
            }
            Ok(output.into_result(false, Some("Plugin command completed".to_owned())))
        })
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum PluginCommandToolError {
    #[error("plugin command registry is empty")]
    Empty,
    #[error("duplicate plugin command {0:?}")]
    Duplicate(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginDiagnosticLevel {
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PluginDiagnosticCode {
    PluginLimit,
    NonLocalRegistration,
    InvalidRoot,
    MissingManifest,
    AmbiguousManifest,
    ManifestTooLarge,
    InvalidManifest,
    UnsupportedRemoteField,
    DuplicatePlugin,
    EscapesRoot,
    MissingExecutable,
    InvalidCommand,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginDiagnostic {
    pub level: PluginDiagnosticLevel,
    pub code: PluginDiagnosticCode,
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PluginReload {
    pub loaded: usize,
    pub diagnostics: Vec<PluginDiagnostic>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum PluginStateError {
    #[error("plugin not found: {0}")]
    NotFound(String),
    #[error("MCP server not found: {0}.{1}")]
    McpServerNotFound(String, String),
}

#[derive(Clone, Debug)]
struct PluginState {
    enabled: bool,
    disabled_mcp_servers: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct LoadedPlugin {
    id: String,
    version: String,
    description: Option<String>,
    root: PathBuf,
    skill_roots: Vec<SkillRoot>,
    mcp_servers: BTreeMap<String, PluginMcpDescriptor>,
    commands: BTreeMap<String, PluginProcessDescriptor>,
    state: PluginState,
}

#[derive(Debug, Error)]
enum ManifestError {
    #[error("manifest root must be a JSON object")]
    NotObject,
    #[error("missing required string field: {0}")]
    MissingField(&'static str),
    #[error("invalid plugin name: {0}")]
    InvalidName(String),
    #[error("invalid semantic version: {0}")]
    InvalidVersion(String),
    #[error("unsupported remote acquisition field: {0}")]
    UnsupportedRemoteField(String),
    #[error("unknown manifest field: {0}")]
    UnknownField(String),
    #[error("field {0} has the wrong JSON type")]
    WrongType(String),
    #[error("too many plugin contributions")]
    ContributionLimit,
    #[error("path must be a confined relative path: {0}")]
    InvalidPath(String),
    #[error("path escapes plugin root: {0}")]
    EscapesRoot(String),
    #[error("path does not have the required type: {0}")]
    WrongPathType(String),
    #[error("invalid command {0}: {1}")]
    InvalidCommand(String, String),
    #[error("executable is not available: {0}")]
    MissingExecutable(String),
    #[error("invalid MCP server {0}: {1}")]
    InvalidMcp(String, String),
    #[error("filesystem error while validating manifest")]
    FileSystem,
}

/// Reloadable registry for explicitly registered local plugin directories.
pub struct LocalPluginRegistry<
    F: SkillFileSystem = StdSkillFileSystem,
    R: ExecutableResolver = PathExecutableResolver,
> {
    fs: Arc<F>,
    resolver: Arc<R>,
    registrations: Vec<PluginRegistration>,
    limits: PluginLimits,
    state_overrides: BTreeMap<String, PluginState>,
    plugins: BTreeMap<String, LoadedPlugin>,
    diagnostics: Vec<PluginDiagnostic>,
}

impl LocalPluginRegistry<StdSkillFileSystem, PathExecutableResolver> {
    pub fn local(registrations: Vec<PluginRegistration>, limits: PluginLimits) -> Self {
        Self::new(
            Arc::new(StdSkillFileSystem),
            Arc::new(PathExecutableResolver::ambient()),
            registrations,
            limits,
        )
    }
}

impl<F: SkillFileSystem, R: ExecutableResolver> LocalPluginRegistry<F, R> {
    pub fn new(
        fs: Arc<F>,
        resolver: Arc<R>,
        registrations: Vec<PluginRegistration>,
        limits: PluginLimits,
    ) -> Self {
        Self {
            fs,
            resolver,
            registrations,
            limits,
            state_overrides: BTreeMap::new(),
            plugins: BTreeMap::new(),
            diagnostics: Vec::new(),
        }
    }

    pub fn reload(&mut self) -> PluginReload {
        let mut registrations = self.registrations.clone();
        registrations.sort_by(|left, right| left.root.cmp(&right.root));
        let mut plugins = BTreeMap::new();
        let mut diagnostics = Vec::new();
        for registration in registrations.into_iter().take(self.limits.max_plugins) {
            match load_plugin(
                self.fs.as_ref(),
                self.resolver.as_ref(),
                &registration,
                self.limits,
            ) {
                Ok(mut plugin) => {
                    if let Some(state) = self.state_overrides.get(&plugin.id) {
                        plugin.state = state.clone();
                    }
                    if plugins.contains_key(&plugin.id) {
                        diagnostics.push(PluginDiagnostic {
                            level: PluginDiagnosticLevel::Error,
                            code: PluginDiagnosticCode::DuplicatePlugin,
                            path: plugin.root,
                            message: format!(
                                "duplicate plugin id {} ignored; lexically first root retained",
                                plugin.id
                            ),
                        });
                    } else {
                        plugins.insert(plugin.id.clone(), plugin);
                    }
                }
                Err(diagnostic) => diagnostics.push(diagnostic),
            }
        }
        if self.registrations.len() > self.limits.max_plugins {
            diagnostics.push(PluginDiagnostic {
                level: PluginDiagnosticLevel::Error,
                code: PluginDiagnosticCode::PluginLimit,
                path: PathBuf::new(),
                message: format!(
                    "{} plugin registrations exceed limit {}",
                    self.registrations.len(),
                    self.limits.max_plugins
                ),
            });
        }
        self.plugins = plugins;
        self.diagnostics = diagnostics;
        PluginReload {
            loaded: self.plugins.len(),
            diagnostics: self.diagnostics.clone(),
        }
    }

    pub fn diagnostics(&self) -> &[PluginDiagnostic] {
        &self.diagnostics
    }

    pub fn list(&self) -> Vec<PluginInfo> {
        self.plugins.values().map(plugin_info).collect()
    }

    pub fn get(&self, id: &str) -> Option<PluginInfo> {
        self.plugins.get(id).map(plugin_info)
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<(), PluginStateError> {
        let plugin = self
            .plugins
            .get_mut(id)
            .ok_or_else(|| PluginStateError::NotFound(id.to_owned()))?;
        plugin.state.enabled = enabled;
        self.state_overrides
            .insert(id.to_owned(), plugin.state.clone());
        Ok(())
    }

    pub fn set_mcp_enabled(
        &mut self,
        plugin_id: &str,
        server_name: &str,
        enabled: bool,
    ) -> Result<(), PluginStateError> {
        let plugin = self
            .plugins
            .get_mut(plugin_id)
            .ok_or_else(|| PluginStateError::NotFound(plugin_id.to_owned()))?;
        if !plugin.mcp_servers.contains_key(server_name) {
            return Err(PluginStateError::McpServerNotFound(
                plugin_id.to_owned(),
                server_name.to_owned(),
            ));
        }
        if enabled {
            plugin.state.disabled_mcp_servers.remove(server_name);
        } else {
            plugin
                .state
                .disabled_mcp_servers
                .insert(server_name.to_owned());
        }
        self.state_overrides
            .insert(plugin_id.to_owned(), plugin.state.clone());
        Ok(())
    }

    /// Produces only enabled contributions. There is no simulated fallback:
    /// invalid or unavailable commands never enter the registry.
    pub fn contribution_plan(&self) -> PluginContributionPlan {
        let mut plan = PluginContributionPlan::default();
        for plugin in self.plugins.values().filter(|plugin| plugin.state.enabled) {
            plan.skill_roots.extend(plugin.skill_roots.clone());
            for (name, descriptor) in &plugin.mcp_servers {
                if plugin.state.disabled_mcp_servers.contains(name) {
                    continue;
                }
                plan.mcp_servers.push(PluginMcpPlan {
                    runtime_name: format!("{}.{}", plugin.id, name),
                    plugin_id: plugin.id.clone(),
                    server_name: name.clone(),
                    descriptor: descriptor.clone(),
                });
            }
            for (name, process) in &plugin.commands {
                plan.commands.push(PluginCommandPlan {
                    runtime_name: format!("{}.{}", plugin.id, name),
                    plugin_id: plugin.id.clone(),
                    command_name: name.clone(),
                    process: process.clone(),
                });
            }
        }
        plan
    }
}

fn plugin_info(plugin: &LoadedPlugin) -> PluginInfo {
    PluginInfo {
        id: plugin.id.clone(),
        version: plugin.version.clone(),
        description: plugin.description.clone(),
        root: plugin.root.clone(),
        enabled: plugin.state.enabled,
        skill_roots: plugin.skill_roots.len(),
        mcp_servers: plugin.mcp_servers.keys().cloned().collect(),
        commands: plugin.commands.keys().cloned().collect(),
    }
}

fn load_plugin<F: SkillFileSystem, R: ExecutableResolver>(
    fs: &F,
    resolver: &R,
    registration: &PluginRegistration,
    limits: PluginLimits,
) -> Result<LoadedPlugin, PluginDiagnostic> {
    if !registration.root.is_absolute() || looks_remote_or_archive(&registration.root) {
        return Err(diagnostic(
            PluginDiagnosticCode::NonLocalRegistration,
            registration.root.clone(),
            "plugin registration must be an absolute local directory",
        ));
    }
    let root = fs.canonicalize(&registration.root).map_err(|_| {
        diagnostic(
            PluginDiagnosticCode::InvalidRoot,
            registration.root.clone(),
            "plugin root cannot be canonicalized",
        )
    })?;
    match fs.metadata(&root) {
        Ok(FileMetadata { is_dir: true, .. }) => {}
        _ => {
            return Err(diagnostic(
                PluginDiagnosticCode::InvalidRoot,
                root,
                "plugin root is not a directory",
            ))
        }
    }

    let root_manifest = root.join(ROOT_MANIFEST);
    let nested_manifest = root.join(NESTED_MANIFEST);
    let root_exists = fs
        .metadata(&root_manifest)
        .map(|metadata| metadata.is_file)
        .unwrap_or(false);
    let nested_exists = fs
        .metadata(&nested_manifest)
        .map(|metadata| metadata.is_file)
        .unwrap_or(false);
    let manifest_path = match (root_exists, nested_exists) {
        (true, false) => root_manifest,
        (false, true) => nested_manifest,
        (false, false) => {
            return Err(diagnostic(
                PluginDiagnosticCode::MissingManifest,
                root,
                "plugin has no mycel.plugin.json manifest",
            ))
        }
        (true, true) => {
            return Err(diagnostic(
                PluginDiagnosticCode::AmbiguousManifest,
                root,
                "plugin has both supported manifest locations",
            ))
        }
    };
    let canonical_manifest = confined(fs, &root, &manifest_path)
        .map_err(|error| diagnostic_from_manifest_error(error, manifest_path.clone()))?;
    let metadata = fs.metadata(&canonical_manifest).map_err(|_| {
        diagnostic(
            PluginDiagnosticCode::InvalidManifest,
            canonical_manifest.clone(),
            "cannot inspect plugin manifest",
        )
    })?;
    if metadata.len > limits.max_manifest_bytes {
        return Err(diagnostic(
            PluginDiagnosticCode::ManifestTooLarge,
            canonical_manifest,
            format!(
                "plugin manifest is {} bytes; limit is {}",
                metadata.len, limits.max_manifest_bytes
            ),
        ));
    }
    let bytes = fs
        .read_bounded(&canonical_manifest, limits.max_manifest_bytes)
        .map_err(|error| {
            let code = if error.kind() == io::ErrorKind::InvalidData {
                PluginDiagnosticCode::ManifestTooLarge
            } else {
                PluginDiagnosticCode::InvalidManifest
            };
            diagnostic(
                code,
                canonical_manifest.clone(),
                "cannot read plugin manifest",
            )
        })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        diagnostic(
            PluginDiagnosticCode::InvalidManifest,
            canonical_manifest.clone(),
            "plugin manifest is not valid JSON",
        )
    })?;
    parse_manifest(fs, resolver, &root, value, registration, limits)
        .map_err(|error| diagnostic_from_manifest_error(error, canonical_manifest))
}

fn parse_manifest<F: SkillFileSystem, R: ExecutableResolver>(
    fs: &F,
    resolver: &R,
    root: &Path,
    value: Value,
    registration: &PluginRegistration,
    limits: PluginLimits,
) -> Result<LoadedPlugin, ManifestError> {
    let object = value.as_object().ok_or(ManifestError::NotObject)?;
    const ALLOWED: &[&str] = &[
        "name",
        "version",
        "description",
        "skills",
        "mcpServers",
        "commands",
    ];
    const REMOTE: &[&str] = &[
        "download",
        "downloads",
        "archive",
        "github",
        "marketplace",
        "repository",
        "sourceUrl",
        "updateUrl",
    ];
    for key in object.keys() {
        if REMOTE.contains(&key.as_str()) {
            return Err(ManifestError::UnsupportedRemoteField(key.clone()));
        }
        if !ALLOWED.contains(&key.as_str()) {
            return Err(ManifestError::UnknownField(key.clone()));
        }
    }
    let id = required_string(object, "name")?.to_owned();
    if !valid_component(&id) {
        return Err(ManifestError::InvalidName(id));
    }
    if registration
        .expected_id
        .as_deref()
        .is_some_and(|expected| expected != id)
    {
        return Err(ManifestError::InvalidName(format!(
            "manifest name {id:?} does not match installed id {:?}",
            registration.expected_id.as_deref().unwrap_or_default()
        )));
    }
    let version = required_string(object, "version")?.to_owned();
    if !valid_semver(&version) {
        return Err(ManifestError::InvalidVersion(version));
    }
    let description = optional_string(object, "description")?.map(str::to_owned);
    let mut contribution_count = 0usize;

    let mut skill_roots = Vec::new();
    if let Some(skills) = object.get("skills") {
        for raw_path in string_or_array(skills, "skills")? {
            contribution_count = contribution_count.saturating_add(1);
            let directory = confined_directory(fs, root, &raw_path)?;
            skill_roots.push(SkillRoot {
                path: directory,
                source: SkillSource::Extra,
                namespace: Some(id.clone()),
            });
        }
    } else if fs
        .metadata(&root.join("SKILL.md"))
        .map(|metadata| metadata.is_file)
        .unwrap_or(false)
    {
        contribution_count = contribution_count.saturating_add(1);
        skill_roots.push(SkillRoot {
            path: root.to_path_buf(),
            source: SkillSource::Extra,
            namespace: Some(id.clone()),
        });
    }
    skill_roots.sort_by(|left, right| left.path.cmp(&right.path));
    skill_roots.dedup_by(|left, right| left.path == right.path);

    let mut mcp_servers = BTreeMap::new();
    if let Some(value) = object.get("mcpServers") {
        let servers = value
            .as_object()
            .ok_or_else(|| ManifestError::WrongType("mcpServers".to_owned()))?;
        for (name, value) in servers {
            contribution_count = contribution_count.saturating_add(1);
            if !valid_component(name) {
                return Err(ManifestError::InvalidMcp(
                    name.clone(),
                    "invalid server name".to_owned(),
                ));
            }
            let descriptor = parse_mcp(fs, resolver, root, name, value)?;
            mcp_servers.insert(name.clone(), descriptor);
        }
    }

    let mut commands = BTreeMap::new();
    if let Some(value) = object.get("commands") {
        let command_object = value
            .as_object()
            .ok_or_else(|| ManifestError::WrongType("commands".to_owned()))?;
        for (name, value) in command_object {
            contribution_count = contribution_count.saturating_add(1);
            if !valid_component(name) {
                return Err(ManifestError::InvalidCommand(
                    name.clone(),
                    "invalid command name".to_owned(),
                ));
            }
            commands.insert(
                name.clone(),
                parse_process(fs, resolver, root, name, value)?,
            );
        }
    }
    if contribution_count > limits.max_contributions_per_plugin {
        return Err(ManifestError::ContributionLimit);
    }
    if let Some(server) = registration
        .disabled_mcp_servers
        .iter()
        .find(|server| !valid_component(server) || !mcp_servers.contains_key(*server))
    {
        return Err(ManifestError::InvalidMcp(
            server.clone(),
            "disabled MCP state names a server not declared by the manifest".to_owned(),
        ));
    }

    Ok(LoadedPlugin {
        id,
        version,
        description,
        root: root.to_path_buf(),
        skill_roots,
        mcp_servers,
        commands,
        state: PluginState {
            enabled: registration.enabled,
            disabled_mcp_servers: registration.disabled_mcp_servers.clone(),
        },
    })
}

fn parse_mcp<F: SkillFileSystem, R: ExecutableResolver>(
    fs: &F,
    resolver: &R,
    root: &Path,
    name: &str,
    value: &Value,
) -> Result<PluginMcpDescriptor, ManifestError> {
    let object = value.as_object().ok_or_else(|| {
        ManifestError::InvalidMcp(
            name.to_owned(),
            "configuration must be an object".to_owned(),
        )
    })?;
    let transport = optional_string(object, "transport")?.unwrap_or("stdio");
    match transport {
        "stdio" => parse_process(fs, resolver, root, name, value).map(PluginMcpDescriptor::Stdio),
        "http" | "streamable-http" => {
            const HTTP_FIELDS: &[&str] =
                &["transport", "url", "headers", "bearerTokenEnvVar", "auth"];
            reject_unknown(object, HTTP_FIELDS, name, true)?;
            let url = required_string(object, "url")?;
            validate_http_endpoint(url).map_err(|message| {
                ManifestError::InvalidMcp(name.to_owned(), message.to_owned())
            })?;
            let headers = optional_string_map(object, "headers")?;
            if headers.iter().any(|(name, value)| {
                !valid_http_header_name(name) || contains_forbidden_string_value(value)
            }) {
                return Err(ManifestError::InvalidMcp(
                    name.to_owned(),
                    "headers contain an invalid name or value".to_owned(),
                ));
            }
            let bearer_token_env_var =
                optional_string(object, "bearerTokenEnvVar")?.map(str::to_owned);
            if bearer_token_env_var
                .as_deref()
                .is_some_and(|name| !valid_environment_key(name))
            {
                return Err(ManifestError::InvalidMcp(
                    name.to_owned(),
                    "bearerTokenEnvVar is not a valid environment key".to_owned(),
                ));
            }
            let auth = match optional_string(object, "auth")? {
                None => None,
                Some("oauth") => Some(McpAuth::Oauth),
                Some(_) => {
                    return Err(ManifestError::InvalidMcp(
                        name.to_owned(),
                        "auth must be oauth when present".to_owned(),
                    ))
                }
            };
            Ok(PluginMcpDescriptor::StreamableHttp(PluginHttpDescriptor {
                url: url.to_owned(),
                headers,
                bearer_token_env_var,
                auth,
            }))
        }
        other => Err(ManifestError::InvalidMcp(
            name.to_owned(),
            format!("unsupported transport {other}"),
        )),
    }
}

fn parse_process<F: SkillFileSystem, R: ExecutableResolver>(
    fs: &F,
    resolver: &R,
    root: &Path,
    name: &str,
    value: &Value,
) -> Result<PluginProcessDescriptor, ManifestError> {
    let object = value.as_object().ok_or_else(|| {
        ManifestError::InvalidCommand(name.to_owned(), "descriptor must be an object".to_owned())
    })?;
    const PROCESS_FIELDS: &[&str] = &["transport", "command", "args", "cwd", "env"];
    reject_unknown(object, PROCESS_FIELDS, name, false)?;
    let command = required_string(object, "command")?;
    validate_command_token(command)
        .map_err(|message| ManifestError::InvalidCommand(name.to_owned(), message.to_owned()))?;
    let mut argv = optional_string_array(object, "args")?;
    if argv.iter().any(|argument| {
        argument.contains('\0')
            || argument
                .chars()
                .any(|character| character.is_control() && character != '\t')
    }) {
        return Err(ManifestError::InvalidCommand(
            name.to_owned(),
            "argv contains a control character".to_owned(),
        ));
    }
    let cwd = match optional_string(object, "cwd")? {
        None | Some(".") => root.to_path_buf(),
        Some(path) => confined_directory(fs, root, path)?,
    };
    let env = optional_string_map(object, "env")?;
    if env
        .iter()
        .any(|(key, value)| !valid_environment_key(key) || contains_forbidden_string_value(value))
    {
        return Err(ManifestError::InvalidCommand(
            name.to_owned(),
            "environment contains an invalid key or value".to_owned(),
        ));
    }

    let executable = if command.starts_with("./") {
        confined_file(fs, root, command)?
    } else {
        resolver
            .resolve(command)
            .map_err(|_| ManifestError::FileSystem)?
            .filter(|path| path.is_absolute())
            .ok_or_else(|| ManifestError::MissingExecutable(command.to_owned()))?
    };

    // Node receives a single explicit script path as argv[0]. There is no
    // bundled runtime, source transformation, eval flag, or hidden fallback.
    if command == "node" {
        let script = argv.first().ok_or_else(|| {
            ManifestError::InvalidCommand(
                name.to_owned(),
                "explicit node commands require a confined script as argv[0]".to_owned(),
            )
        })?;
        let canonical_script = confined_file(fs, root, script)?;
        argv[0] = canonical_script.to_string_lossy().into_owned();
    }

    Ok(PluginProcessDescriptor {
        executable,
        argv,
        cwd,
        env,
    })
}

fn reject_unknown(
    object: &Map<String, Value>,
    allowed: &[&str],
    name: &str,
    mcp: bool,
) -> Result<(), ManifestError> {
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        let message = format!("unknown descriptor field {key}");
        return if mcp {
            Err(ManifestError::InvalidMcp(name.to_owned(), message))
        } else {
            Err(ManifestError::InvalidCommand(name.to_owned(), message))
        };
    }
    Ok(())
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    key: &'static str,
) -> Result<&'a str, ManifestError> {
    object
        .get(key)
        .ok_or(ManifestError::MissingField(key))?
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ManifestError::WrongType(key.to_owned()))
}

fn optional_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, ManifestError> {
    match object.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| ManifestError::WrongType(key.to_owned())),
    }
}

fn optional_string_array(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Vec<String>, ManifestError> {
    let Some(value) = object.get(key) else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .ok_or_else(|| ManifestError::WrongType(key.to_owned()))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| ManifestError::WrongType(key.to_owned()))
        })
        .collect()
}

fn optional_string_map(
    object: &Map<String, Value>,
    key: &str,
) -> Result<BTreeMap<String, String>, ManifestError> {
    let Some(value) = object.get(key) else {
        return Ok(BTreeMap::new());
    };
    value
        .as_object()
        .ok_or_else(|| ManifestError::WrongType(key.to_owned()))?
        .iter()
        .map(|(name, value)| {
            value
                .as_str()
                .map(|value| (name.clone(), value.to_owned()))
                .ok_or_else(|| ManifestError::WrongType(key.to_owned()))
        })
        .collect()
}

fn string_or_array(value: &Value, key: &str) -> Result<Vec<String>, ManifestError> {
    if let Some(value) = value.as_str() {
        return Ok(vec![value.to_owned()]);
    }
    value
        .as_array()
        .ok_or_else(|| ManifestError::WrongType(key.to_owned()))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| ManifestError::WrongType(key.to_owned()))
        })
        .collect()
}

fn confined_directory<F: SkillFileSystem>(
    fs: &F,
    root: &Path,
    raw: &str,
) -> Result<PathBuf, ManifestError> {
    let path = confined_relative(fs, root, raw)?;
    match fs.metadata(&path) {
        Ok(FileMetadata { is_dir: true, .. }) => Ok(path),
        _ => Err(ManifestError::WrongPathType(raw.to_owned())),
    }
}

fn confined_file<F: SkillFileSystem>(
    fs: &F,
    root: &Path,
    raw: &str,
) -> Result<PathBuf, ManifestError> {
    let path = confined_relative(fs, root, raw)?;
    match fs.metadata(&path) {
        Ok(FileMetadata { is_file: true, .. }) => Ok(path),
        _ => Err(ManifestError::WrongPathType(raw.to_owned())),
    }
}

fn confined_relative<F: SkillFileSystem>(
    fs: &F,
    root: &Path,
    raw: &str,
) -> Result<PathBuf, ManifestError> {
    if raw != "." && !raw.starts_with("./") {
        return Err(ManifestError::InvalidPath(raw.to_owned()));
    }
    if raw.contains('\0') {
        return Err(ManifestError::InvalidPath("contains NUL".to_owned()));
    }
    let relative = Path::new(raw);
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(ManifestError::InvalidPath(raw.to_owned()));
    }
    confined(fs, root, &root.join(relative))
}

fn confined<F: SkillFileSystem>(
    fs: &F,
    root: &Path,
    path: &Path,
) -> Result<PathBuf, ManifestError> {
    let canonical = fs
        .canonicalize(path)
        .map_err(|_| ManifestError::FileSystem)?;
    if canonical.starts_with(root) {
        Ok(canonical)
    } else {
        Err(ManifestError::EscapesRoot(
            path.to_string_lossy().into_owned(),
        ))
    }
}

fn validate_command_token(command: &str) -> Result<(), &'static str> {
    if command.is_empty() || command.contains('\0') || command.chars().any(char::is_whitespace) {
        return Err("command must be one executable token");
    }
    if command.starts_with("./") {
        return Ok(());
    }
    if command.contains('/') || command.contains('\\') {
        return Err("bare commands are resolved on PATH; local commands must start with ./");
    }
    if !command
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
    {
        return Err("command contains shell or control metacharacters");
    }
    Ok(())
}

fn validate_http_endpoint(url: &str) -> Result<(), &'static str> {
    if url
        .chars()
        .any(|character| character.is_whitespace() || matches!(character, '\0' | '#' | '?'))
    {
        return Err("HTTP endpoint contains a forbidden character");
    }
    let (scheme, remainder) = url
        .split_once("://")
        .ok_or("HTTP endpoint must include a scheme")?;
    if scheme != "https" && scheme != "http" {
        return Err("HTTP endpoint must use https or loopback http");
    }
    let authority = remainder.split('/').next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err("HTTP endpoint has an invalid authority");
    }
    let host = authority
        .strip_prefix('[')
        .and_then(|value| value.split_once(']'))
        .map(|(host, _)| host)
        .unwrap_or_else(|| authority.split(':').next().unwrap_or(authority));
    if host.is_empty() {
        return Err("HTTP endpoint has an empty host");
    }
    if scheme == "http" && !matches!(host, "127.0.0.1" | "localhost" | "::1") {
        return Err("plaintext HTTP is restricted to loopback");
    }
    Ok(())
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().enumerate().all(|(index, byte)| match byte {
            b'a'..=b'z' | b'0'..=b'9' => true,
            b'_' | b'-' => index > 0,
            _ => false,
        })
}

fn valid_environment_key(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().enumerate().all(|(index, byte)| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'_' => true,
            b'0'..=b'9' => index > 0,
            _ => false,
        })
}

fn valid_http_header_name(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn contains_forbidden_string_value(value: &str) -> bool {
    value
        .chars()
        .any(|character| character == '\0' || (character.is_control() && character != '\t'))
}

fn valid_semver(value: &str) -> bool {
    if value.is_empty() || value.matches('+').count() > 1 {
        return false;
    }
    let without_build = match value.split_once('+') {
        Some((main, build)) if valid_semver_identifiers(build, false) => main,
        Some(_) => return false,
        None => value,
    };
    let core = match without_build.split_once('-') {
        Some((core, pre_release)) if valid_semver_identifiers(pre_release, true) => core,
        Some(_) => return false,
        None => without_build,
    };
    let parts = core.split('.').collect::<Vec<_>>();
    if parts.len() != 3 {
        return false;
    }
    parts.iter().all(|part| {
        !part.is_empty()
            && part.bytes().all(|byte| byte.is_ascii_digit())
            && (part.len() == 1 || !part.starts_with('0'))
    })
}

fn valid_semver_identifiers(value: &str, reject_numeric_leading_zero: bool) -> bool {
    !value.is_empty()
        && value.split('.').all(|identifier| {
            !identifier.is_empty()
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && !(reject_numeric_leading_zero
                    && identifier.len() > 1
                    && identifier.bytes().all(|byte| byte.is_ascii_digit())
                    && identifier.starts_with('0'))
        })
}

fn looks_remote_or_archive(path: &Path) -> bool {
    let value = path.to_string_lossy().to_ascii_lowercase();
    value.starts_with("http://")
        || value.starts_with("https://")
        || value.starts_with("git://")
        || value.ends_with(".zip")
        || value.ends_with(".tar")
        || value.ends_with(".tar.gz")
        || value.ends_with(".tgz")
}

fn diagnostic(
    code: PluginDiagnosticCode,
    path: PathBuf,
    message: impl Into<String>,
) -> PluginDiagnostic {
    PluginDiagnostic {
        level: PluginDiagnosticLevel::Error,
        code,
        path,
        message: message.into(),
    }
}

fn diagnostic_from_manifest_error(error: ManifestError, path: PathBuf) -> PluginDiagnostic {
    let code = match &error {
        ManifestError::UnsupportedRemoteField(_) => PluginDiagnosticCode::UnsupportedRemoteField,
        ManifestError::EscapesRoot(_) => PluginDiagnosticCode::EscapesRoot,
        ManifestError::MissingExecutable(_) => PluginDiagnosticCode::MissingExecutable,
        ManifestError::InvalidCommand(_, _) => PluginDiagnosticCode::InvalidCommand,
        _ => PluginDiagnosticCode::InvalidManifest,
    };
    diagnostic(code, path, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct NoUpdates;

    impl crate::ToolUpdateSink for NoUpdates {
        fn emit(&self, _update: mycel_agent_protocol::ToolUpdate) {}
    }

    #[derive(Default)]
    struct MemoryFs {
        nodes: Mutex<BTreeMap<PathBuf, MemoryNode>>,
        aliases: Mutex<BTreeMap<PathBuf, PathBuf>>,
    }

    #[derive(Clone)]
    enum MemoryNode {
        Directory,
        File(Vec<u8>),
    }

    impl MemoryFs {
        fn directory(&self, path: &str) {
            self.nodes
                .lock()
                .expect("nodes")
                .insert(PathBuf::from(path), MemoryNode::Directory);
        }

        fn file(&self, path: &str, content: impl Into<Vec<u8>>) {
            self.nodes
                .lock()
                .expect("nodes")
                .insert(PathBuf::from(path), MemoryNode::File(content.into()));
        }

        fn alias(&self, from: &str, to: &str) {
            self.aliases
                .lock()
                .expect("aliases")
                .insert(normalize(Path::new(from)), normalize(Path::new(to)));
        }

        fn remove(&self, path: &str) {
            self.nodes.lock().expect("nodes").remove(Path::new(path));
        }

        fn resolved(&self, path: &Path) -> PathBuf {
            let path = normalize(path);
            self.aliases
                .lock()
                .expect("aliases")
                .get(&path)
                .cloned()
                .unwrap_or(path)
        }
    }

    fn normalize(path: &Path) -> PathBuf {
        path.components().collect()
    }

    impl SkillFileSystem for MemoryFs {
        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            let resolved = self.resolved(path);
            if self.nodes.lock().expect("nodes").contains_key(&resolved) {
                Ok(resolved)
            } else {
                Err(io::Error::new(io::ErrorKind::NotFound, "missing"))
            }
        }

        fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
            match self.nodes.lock().expect("nodes").get(path) {
                Some(MemoryNode::Directory) => Ok(FileMetadata {
                    is_file: false,
                    is_dir: true,
                    len: 0,
                }),
                Some(MemoryNode::File(bytes)) => Ok(FileMetadata {
                    is_file: true,
                    is_dir: false,
                    len: bytes.len() as u64,
                }),
                None => Err(io::Error::new(io::ErrorKind::NotFound, "missing")),
            }
        }

        fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
            Ok(self
                .nodes
                .lock()
                .expect("nodes")
                .keys()
                .filter(|candidate| candidate.parent() == Some(path))
                .cloned()
                .collect())
        }

        fn read_bounded(&self, path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
            match self.nodes.lock().expect("nodes").get(path) {
                Some(MemoryNode::File(bytes)) if bytes.len() as u64 <= max_bytes => {
                    Ok(bytes.clone())
                }
                Some(MemoryNode::File(_)) => {
                    Err(io::Error::new(io::ErrorKind::InvalidData, "too large"))
                }
                _ => Err(io::Error::new(io::ErrorKind::NotFound, "missing")),
            }
        }
    }

    #[derive(Default)]
    struct FakeResolver {
        paths: BTreeMap<String, PathBuf>,
    }

    impl ExecutableResolver for FakeResolver {
        fn resolve(&self, executable: &str) -> io::Result<Option<PathBuf>> {
            Ok(self.paths.get(executable).cloned())
        }
    }

    fn resolver() -> Arc<FakeResolver> {
        Arc::new(FakeResolver {
            paths: BTreeMap::from([
                ("node".to_owned(), PathBuf::from("/usr/bin/node")),
                ("tool".to_owned(), PathBuf::from("/usr/bin/tool")),
            ]),
        })
    }

    fn setup_plugin(fs: &MemoryFs, root: &str, manifest: Value) {
        fs.directory(root);
        fs.file(
            &format!("{root}/{ROOT_MANIFEST}"),
            serde_json::to_vec(&manifest).unwrap(),
        );
    }

    fn registration(root: &str) -> PluginRegistration {
        PluginRegistration::enabled(root)
    }

    #[test]
    fn local_manifest_produces_namespaced_declarative_plans() {
        let fs = Arc::new(MemoryFs::default());
        for path in ["/p/skills", "/p/bin"] {
            fs.directory(path);
        }
        fs.file("/p/bin/server.mjs", "export {};");
        setup_plugin(
            &fs,
            "/p",
            serde_json::json!({
                "name": "reviewer",
                "version": "1.2.3",
                "description": "local only",
                "skills": "./skills",
                "mcpServers": {
                    "local": {
                        "transport": "stdio",
                        "command": "node",
                        "args": ["./bin/server.mjs", "--mode", "safe; rm -rf /"]
                    },
                    "docs": {
                        "transport": "streamable-http",
                        "url": "http://127.0.0.1:8123/mcp",
                        "headers": {"X-Client": "secret"},
                        "bearerTokenEnvVar": "DOCS_TOKEN",
                        "auth": "oauth"
                    }
                },
                "commands": {
                    "check": {
                        "command": "tool",
                        "args": ["--value", "x; echo nope"],
                        "env": {"TOKEN": "env-secret"}
                    }
                }
            }),
        );
        let mut registry = LocalPluginRegistry::new(
            Arc::clone(&fs),
            resolver(),
            vec![registration("/p")],
            PluginLimits::default(),
        );
        assert_eq!(registry.reload().loaded, 1);
        let plan = registry.contribution_plan();
        assert_eq!(plan.skill_roots[0].namespace.as_deref(), Some("reviewer"));
        assert_eq!(plan.mcp_servers.len(), 2);
        let local = plan
            .mcp_servers
            .iter()
            .find(|server| server.server_name == "local")
            .unwrap();
        let PluginMcpDescriptor::Stdio(process) = &local.descriptor else {
            panic!("stdio")
        };
        assert_eq!(process.executable, PathBuf::from("/usr/bin/node"));
        assert_eq!(process.argv[0], "/p/bin/server.mjs");
        assert_eq!(process.argv[2], "safe; rm -rf /");
        assert_eq!(plan.commands[0].process.argv[1], "x; echo nope");
        assert!(!format!("{:?}", plan.mcp_servers).contains("secret"));
        assert!(!format!("{:?}", plan.commands).contains("x; echo nope"));
        assert!(!format!("{:?}", plan.commands).contains("env-secret"));
        let runtime_configs = plan.runtime_mcp_configs();
        let McpServerConfig::Stdio { command, args, .. } = &runtime_configs["reviewer.local"]
        else {
            panic!("stdio runtime config")
        };
        assert_eq!(command, "/usr/bin/node");
        assert_eq!(args[0], "/p/bin/server.mjs");
        let McpServerConfig::Http {
            bearer_token_env_var,
            auth,
            ..
        } = &runtime_configs["reviewer.docs"]
        else {
            panic!("HTTP runtime config")
        };
        assert_eq!(bearer_token_env_var.as_deref(), Some("DOCS_TOKEN"));
        assert_eq!(*auth, Some(McpAuth::Oauth));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn plugin_command_tool_appends_free_form_text_as_one_literal_argument() {
        let tool = PluginCommandTool::new(vec![PluginCommandPlan {
            runtime_name: "reviewer.check".to_owned(),
            plugin_id: "reviewer".to_owned(),
            command_name: "check".to_owned(),
            process: PluginProcessDescriptor {
                executable: PathBuf::from("/bin/echo"),
                argv: vec!["fixed".to_owned()],
                cwd: PathBuf::from("/tmp"),
                env: BTreeMap::new(),
            },
        }])
        .expect("plugin command tool");
        let arguments = json!({
            "command":"reviewer.check",
            "arguments":"x; echo not-a-second-command"
        });
        let context = ToolPrepareContext {
            session_id: crate::SessionId::new("plugin-session").expect("session id"),
            agent_id: crate::AgentId::main(),
            turn_id: 1,
            tool_call_id: crate::ToolCallId::new("plugin-call").expect("tool call id"),
        };
        tool.prepare(&arguments, &context).expect("prepare");
        let result = tool
            .execute(ToolInvocation {
                context,
                arguments,
                cancellation: crate::CancellationToken::new(),
                updates: Arc::new(NoUpdates),
            })
            .await
            .expect("execute");
        let mycel_agent_protocol::ExecutableToolOutput::Text(output) = result.output else {
            panic!("expected text output")
        };
        assert!(!result.is_error);
        assert_eq!(output.trim(), "fixed x; echo not-a-second-command");
    }

    #[test]
    fn disabled_plugin_and_server_emit_no_contributions() {
        let fs = Arc::new(MemoryFs::default());
        fs.directory("/p/skills");
        setup_plugin(
            &fs,
            "/p",
            serde_json::json!({
                "name": "toggle",
                "version": "1.0.0",
                "skills": "./skills",
                "mcpServers": {
                    "local": {"command": "tool"}
                }
            }),
        );
        let mut registry = LocalPluginRegistry::new(
            Arc::clone(&fs),
            resolver(),
            vec![registration("/p")],
            PluginLimits::default(),
        );
        registry.reload();
        registry.set_mcp_enabled("toggle", "local", false).unwrap();
        let plan = registry.contribution_plan();
        assert_eq!(plan.skill_roots.len(), 1);
        assert!(plan.mcp_servers.is_empty());
        registry.set_enabled("toggle", false).unwrap();
        let plan = registry.contribution_plan();
        assert!(plan.skill_roots.is_empty());
        assert!(plan.mcp_servers.is_empty());
        assert!(plan.commands.is_empty());
        registry.reload();
        assert!(!registry.get("toggle").unwrap().enabled);
    }

    #[test]
    fn symlink_escape_is_rejected_for_skills_and_node_scripts() {
        let fs = Arc::new(MemoryFs::default());
        fs.directory("/p/link");
        fs.directory("/outside");
        fs.file("/outside/server.mjs", "bad");
        fs.alias("/p/link", "/outside");
        setup_plugin(
            &fs,
            "/p",
            serde_json::json!({
                "name": "escape",
                "version": "1.0.0",
                "skills": "./link",
                "commands": {
                    "bad": {"command": "node", "args": ["./link/server.mjs"]}
                }
            }),
        );
        let mut registry = LocalPluginRegistry::new(
            Arc::clone(&fs),
            resolver(),
            vec![registration("/p")],
            PluginLimits::default(),
        );
        let reload = registry.reload();
        assert_eq!(reload.loaded, 0);
        assert_eq!(
            reload.diagnostics[0].code,
            PluginDiagnosticCode::EscapesRoot
        );

        fs.directory("/q");
        fs.directory("/q/link");
        fs.alias("/q/link/server.mjs", "/outside/server.mjs");
        setup_plugin(
            &fs,
            "/q",
            serde_json::json!({
                "name": "node_escape",
                "version": "1.0.0",
                "commands": {
                    "bad": {"command": "node", "args": ["./link/server.mjs"]}
                }
            }),
        );
        let mut node_registry = LocalPluginRegistry::new(
            fs,
            resolver(),
            vec![registration("/q")],
            PluginLimits::default(),
        );
        let reload = node_registry.reload();
        assert_eq!(reload.loaded, 0);
        assert_eq!(
            reload.diagnostics[0].code,
            PluginDiagnosticCode::EscapesRoot
        );
    }

    #[test]
    fn duplicate_ids_are_deterministic() {
        let fs = Arc::new(MemoryFs::default());
        setup_plugin(
            &fs,
            "/a",
            serde_json::json!({"name":"same","version":"1.0.0"}),
        );
        setup_plugin(
            &fs,
            "/z",
            serde_json::json!({"name":"same","version":"2.0.0"}),
        );
        let mut registry = LocalPluginRegistry::new(
            fs,
            resolver(),
            vec![registration("/z"), registration("/a")],
            PluginLimits::default(),
        );
        let reload = registry.reload();
        assert_eq!(reload.loaded, 1);
        assert_eq!(registry.get("same").unwrap().version, "1.0.0");
        assert_eq!(
            reload.diagnostics[0].code,
            PluginDiagnosticCode::DuplicatePlugin
        );
    }

    #[test]
    fn malformed_oversized_remote_and_implicit_node_are_rejected() {
        let fs = Arc::new(MemoryFs::default());
        fs.directory("/bad");
        fs.file("/bad/mycel.plugin.json", "{");
        setup_plugin(
            &fs,
            "/remote",
            serde_json::json!({
                "name":"remote",
                "version":"1.0.0",
                "github":"owner/repo"
            }),
        );
        setup_plugin(
            &fs,
            "/node",
            serde_json::json!({
                "name":"nodeish",
                "version":"1.0.0",
                "commands":{"run":{"command":"node --eval","args":[]}}
            }),
        );
        fs.directory("/large");
        fs.file("/large/mycel.plugin.json", vec![b'x'; 200]);
        let limits = PluginLimits {
            max_manifest_bytes: 100,
            ..PluginLimits::default()
        };
        let mut registry = LocalPluginRegistry::new(
            fs,
            resolver(),
            vec![
                registration("/bad"),
                registration("/remote"),
                registration("/node"),
                registration("/large"),
            ],
            limits,
        );
        let reload = registry.reload();
        assert_eq!(reload.loaded, 0);
        let codes = reload
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code)
            .collect::<BTreeSet<_>>();
        assert!(codes.contains(&PluginDiagnosticCode::InvalidManifest));
        assert!(codes.contains(&PluginDiagnosticCode::ManifestTooLarge));
        assert!(codes.contains(&PluginDiagnosticCode::UnsupportedRemoteField));
        assert!(codes.contains(&PluginDiagnosticCode::InvalidCommand));
    }

    #[test]
    fn reload_reparses_local_manifest_without_remote_state() {
        let fs = Arc::new(MemoryFs::default());
        setup_plugin(
            &fs,
            "/p",
            serde_json::json!({"name":"reload","version":"1.0.0"}),
        );
        let mut registry = LocalPluginRegistry::new(
            Arc::clone(&fs),
            resolver(),
            vec![registration("/p")],
            PluginLimits::default(),
        );
        registry.reload();
        assert_eq!(registry.get("reload").unwrap().version, "1.0.0");
        fs.remove("/p/mycel.plugin.json");
        fs.file(
            "/p/mycel.plugin.json",
            serde_json::to_vec(&serde_json::json!({"name":"reload","version":"1.1.0"})).unwrap(),
        );
        registry.reload();
        assert_eq!(registry.get("reload").unwrap().version, "1.1.0");
    }

    #[test]
    fn installed_identity_must_match_the_manifest_name() {
        let fs = Arc::new(MemoryFs::default());
        setup_plugin(
            &fs,
            "/p",
            serde_json::json!({"name":"replacement","version":"1.0.0"}),
        );
        let mut registry = LocalPluginRegistry::new(
            Arc::clone(&fs),
            resolver(),
            vec![registration("/p").with_expected_id("original")],
            PluginLimits::default(),
        );
        let reload = registry.reload();
        assert_eq!(reload.loaded, 0);
        assert_eq!(reload.diagnostics.len(), 1);
        assert_eq!(
            reload.diagnostics[0].code,
            PluginDiagnosticCode::InvalidManifest
        );
        assert!(reload.diagnostics[0]
            .message
            .contains("does not match installed id"));
    }

    #[test]
    fn endpoint_validation_rejects_credentials_and_non_loopback_plaintext() {
        assert!(validate_http_endpoint("https://example.test/mcp").is_ok());
        assert!(validate_http_endpoint("http://localhost:8000/mcp").is_ok());
        assert!(validate_http_endpoint("http://[::1]:8000/mcp").is_ok());
        assert!(validate_http_endpoint("http://example.test/mcp").is_err());
        assert!(validate_http_endpoint("https://user:pass@example.test/mcp").is_err());
        assert!(validate_http_endpoint("https://exam ple.test/mcp").is_err());
        assert!(validate_http_endpoint("https://:443/mcp").is_err());
        assert!(validate_http_endpoint("file:///tmp/mcp").is_err());
    }

    #[test]
    fn semantic_versions_are_strict() {
        for version in ["0.1.0", "1.2.3-alpha.1", "1.2.3+build.9"] {
            assert!(valid_semver(version), "{version}");
        }
        for version in ["1", "01.2.3", "1.2.3-", "1.2.3+", "1.2.3-01"] {
            assert!(!valid_semver(version), "{version}");
        }
    }
}
