use std::{collections::HashMap, error::Error, fmt, path::PathBuf};

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use mycel_agent_protocol::SecretString;

pub const OUTPUT_FORMAT_ENV: &str = "MYCEL_OUTPUT_FORMAT";

#[derive(Debug, Clone, Parser, PartialEq, Eq)]
#[command(
    name = "mycel",
    version,
    about = "a personal agent harness built around substrate ecology",
    after_help = "Documentation:        https://github.com/StressTestor/Mycel",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Resume a session. With ID: resume that session. Without ID: interactively pick.
    #[arg(
        short = 'S',
        long,
        short_alias = 'r',
        alias = "resume",
        value_name = "id",
        num_args = 0..=1,
        default_missing_value = ""
    )]
    pub session: Option<String>,

    /// Continue the previous session for the working directory.
    #[arg(short = 'c', long = "continue", short_alias = 'C')]
    pub continue_session: bool,

    /// Auto-approve regular tool calls; the agent may still ask questions.
    #[arg(short = 'y', long, alias = "yes", alias = "auto-approve")]
    pub yolo: bool,

    /// Start fully autonomous; the agent will not ask questions.
    #[arg(long)]
    pub auto: bool,

    /// LLM model alias for this invocation.
    #[arg(short = 'm', long, value_name = "model")]
    pub model: Option<String>,

    /// Run one prompt non-interactively and print the response.
    #[arg(short = 'p', long, value_name = "prompt")]
    pub prompt: Option<String>,

    /// Output format for prompt mode.
    #[arg(long, value_name = "format")]
    pub output_format: Option<OutputFormat>,

    /// Load skills from this directory. Can be repeated.
    #[arg(long = "skills-dir", value_name = "dir", action = ArgAction::Append)]
    pub skills_dirs: Vec<PathBuf>,

    /// Add a workspace directory to the session. Can be repeated.
    #[arg(long = "add-dir", value_name = "dir", action = ArgAction::Append)]
    pub add_dirs: Vec<PathBuf>,

    /// Start in plan mode.
    #[arg(long)]
    pub plan: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Text,
    #[value(name = "stream-json")]
    StreamJson,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Validate Mycel configuration files.
    Doctor(DoctorArgs),
    /// Export a session as a ZIP archive.
    Export(ExportArgs),
    /// Authenticate with the Kimi Code provider via device code.
    Login,
    /// Manage LLM providers non-interactively.
    Provider(ProviderArgs),
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct DoctorArgs {
    #[command(subcommand)]
    pub target: Option<DoctorTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum DoctorTarget {
    /// Validate config.toml.
    Config { path: Option<PathBuf> },
    /// Validate tui.toml.
    Tui { path: Option<PathBuf> },
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ExportArgs {
    /// Session id. Defaults to the most recent session in the current directory.
    pub session_id: Option<String>,
    /// Output ZIP path.
    #[arg(short = 'o', long, value_name = "path")]
    pub output: Option<PathBuf>,
    /// Skip previous-session confirmation.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Skip bundling the active global diagnostic log.
    #[arg(
        long = "no-include-global-log",
        action = ArgAction::SetFalse,
        default_value_t = true
    )]
    pub include_global_log: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ProviderArgs {
    #[command(subcommand)]
    pub command: ProviderCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum ProviderCommand {
    /// Authenticate with a supported provider.
    Login {
        #[arg(value_enum)]
        provider: ProviderAuthTarget,
    },
    /// Remove locally stored authentication for a supported provider.
    Logout {
        #[arg(value_enum)]
        provider: ProviderAuthTarget,
    },
    /// Import every provider in a custom api.json registry.
    Add {
        url: String,
        #[arg(long, value_name = "key", value_parser = parse_provider_api_key)]
        api_key: Option<SecretString>,
    },
    /// Remove a provider and its model aliases.
    Remove {
        #[arg(value_parser = parse_provider_id)]
        provider_id: String,
    },
    /// Show configured providers.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Discover and import providers from a models.dev catalog.
    Catalog(CatalogArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ProviderAuthTarget {
    Kimi,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct CatalogArgs {
    #[command(subcommand)]
    pub command: CatalogCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum CatalogCommand {
    /// List catalog providers, or models for one provider.
    List {
        #[arg(value_parser = parse_provider_id)]
        provider_id: Option<String>,
        #[arg(long, value_name = "substring")]
        filter: Option<String>,
        #[arg(long, value_name = "url")]
        url: String,
        #[arg(long)]
        json: bool,
    },
    /// Import a provider from the catalog.
    Add {
        #[arg(value_parser = parse_provider_id)]
        provider_id: String,
        #[arg(long, value_name = "key", value_parser = parse_provider_api_key)]
        api_key: Option<SecretString>,
        #[arg(long, value_name = "modelId")]
        default_model: Option<String>,
        #[arg(long, value_name = "url")]
        url: String,
    },
}

fn parse_provider_api_key(value: &str) -> Result<SecretString, String> {
    Ok(SecretString::new(value))
}

fn parse_provider_id(value: &str) -> Result<String, String> {
    if !value.is_empty()
        && value.len() <= 128
        && value.bytes().enumerate().all(|(index, byte)| match byte {
            b'a'..=b'z' | b'0'..=b'9' => true,
            b'_' | b'-' => index > 0,
            _ => false,
        })
    {
        Ok(value.to_owned())
    } else {
        Err("provider id must use lowercase ASCII letters, digits, dashes, or underscores".into())
    }
}

pub fn validate_provider_command(command: &ProviderCommand) -> Result<(), ValidationError> {
    match command {
        ProviderCommand::Login { .. }
        | ProviderCommand::Logout { .. }
        | ProviderCommand::Remove { .. }
        | ProviderCommand::List { .. } => Ok(()),
        ProviderCommand::Add { url, api_key } => {
            validate_provider_api_key(api_key.as_ref())?;
            validate_registry_url(url)
        }
        ProviderCommand::Catalog(catalog) => match &catalog.command {
            CatalogCommand::List { url, .. } => validate_registry_url(url),
            CatalogCommand::Add {
                api_key,
                default_model,
                url,
                ..
            } => {
                validate_provider_api_key(api_key.as_ref())?;
                if default_model
                    .as_deref()
                    .is_some_and(|model| model.trim().is_empty())
                {
                    return Err(ValidationError::new("Default model cannot be empty."));
                }
                validate_registry_url(url)
            }
        },
    }
}

fn validate_provider_api_key(api_key: Option<&SecretString>) -> Result<(), ValidationError> {
    if api_key.is_some_and(|key| {
        key.expose().trim().is_empty() || key.expose().chars().any(char::is_control)
    }) {
        Err(ValidationError::new(
            "Provider API key must be non-empty and contain no control characters.",
        ))
    } else {
        Ok(())
    }
}

fn validate_registry_url(value: &str) -> Result<(), ValidationError> {
    if value.len() > 4_096 {
        return Err(ValidationError::new(
            "Registry URL must be 4096 bytes or fewer.",
        ));
    }
    let url = reqwest::Url::parse(value)
        .map_err(|_| ValidationError::new("Registry URL must be a valid absolute URL."))?;
    let local_http =
        url.scheme() == "http" && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if url.scheme() != "https" && !local_http {
        return Err(ValidationError::new(
            "Registry URL must use HTTPS or loopback HTTP.",
        ));
    }
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ValidationError::new(
            "Registry URL must not contain credentials, a query, or a fragment.",
        ));
    }
    if !url.path().ends_with(".json") {
        return Err(ValidationError::new(
            "Registry URL must identify an explicit JSON document.",
        ));
    }
    Ok(())
}

pub trait Environment {
    fn get(&self, key: &str) -> Option<String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnvironment;

impl Environment for ProcessEnvironment {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

impl Environment for HashMap<String, String> {
    fn get(&self, key: &str) -> Option<String> {
        HashMap::get(self, key).cloned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    Manual,
    Yolo,
    Auto,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionSelection {
    New,
    Pick,
    Resume(String),
    Continue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractiveRequest {
    pub session: SessionSelection,
    pub permission: PermissionMode,
    pub plan: bool,
    pub model: Option<String>,
    pub skills_dirs: Vec<PathBuf>,
    pub add_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalCreateRequest {
    pub objective: String,
    pub replace: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptRequest {
    pub prompt: String,
    pub output_format: OutputFormat,
    pub session: SessionSelection,
    pub model: Option<String>,
    pub skills_dirs: Vec<PathBuf>,
    pub add_dirs: Vec<PathBuf>,
    pub goal: Option<GoalCreateRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatedMode {
    Interactive(InteractiveRequest),
    Prompt(PromptRequest),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedOptions {
    pub mode: ValidatedMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    message: String,
}

impl ValidationError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ValidationError {}

pub fn validate(
    cli: Cli,
    environment: &dyn Environment,
) -> Result<ValidatedOptions, ValidationError> {
    let prompt_mode = cli.prompt.is_some();
    if cli
        .prompt
        .as_ref()
        .is_some_and(|prompt| prompt.trim().is_empty())
    {
        return Err(ValidationError::new("Prompt cannot be empty."));
    }
    if cli
        .model
        .as_ref()
        .is_some_and(|model| model.trim().is_empty())
    {
        return Err(ValidationError::new("Model cannot be empty."));
    }
    if !prompt_mode && cli.output_format.is_some() {
        return Err(ValidationError::new(
            "Output format is only supported in prompt mode.",
        ));
    }
    if prompt_mode && cli.yolo {
        return Err(ValidationError::new("Cannot combine --prompt with --yolo."));
    }
    if prompt_mode && cli.auto {
        return Err(ValidationError::new("Cannot combine --prompt with --auto."));
    }
    if prompt_mode && cli.plan {
        return Err(ValidationError::new("Cannot combine --prompt with --plan."));
    }
    if prompt_mode && cli.session.as_deref() == Some("") {
        return Err(ValidationError::new(
            "Cannot use --session without an id in prompt mode.",
        ));
    }
    if cli.continue_session && cli.session.is_some() {
        return Err(ValidationError::new(
            "Cannot combine --continue, --session.",
        ));
    }
    if cli.yolo && cli.auto {
        return Err(ValidationError::new("Cannot combine --yolo with --auto."));
    }

    let session = session_selection(cli.session.as_deref(), cli.continue_session);
    if let Some(prompt) = cli.prompt {
        let output_format = resolve_output_format(cli.output_format, environment)?;
        let goal = parse_headless_goal_create(&prompt)?;
        return Ok(ValidatedOptions {
            mode: ValidatedMode::Prompt(PromptRequest {
                prompt,
                output_format,
                session,
                model: cli.model,
                skills_dirs: cli.skills_dirs,
                add_dirs: cli.add_dirs,
                goal,
            }),
        });
    }

    let permission = if cli.auto {
        PermissionMode::Auto
    } else if cli.yolo {
        PermissionMode::Yolo
    } else {
        PermissionMode::Manual
    };
    Ok(ValidatedOptions {
        mode: ValidatedMode::Interactive(InteractiveRequest {
            session,
            permission,
            plan: cli.plan,
            model: cli.model,
            skills_dirs: cli.skills_dirs,
            add_dirs: cli.add_dirs,
        }),
    })
}

fn resolve_output_format(
    explicit: Option<OutputFormat>,
    environment: &dyn Environment,
) -> Result<OutputFormat, ValidationError> {
    if let Some(explicit) = explicit {
        return Ok(explicit);
    }
    let raw = environment.get(OUTPUT_FORMAT_ENV).unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() || raw == "text" {
        return Ok(OutputFormat::Text);
    }
    if raw == "stream-json" {
        return Ok(OutputFormat::StreamJson);
    }
    Err(ValidationError::new(format!(
        "Invalid {OUTPUT_FORMAT_ENV} value \"{raw}\". Expected one of: text, stream-json."
    )))
}

fn session_selection(session: Option<&str>, continue_session: bool) -> SessionSelection {
    if continue_session {
        SessionSelection::Continue
    } else {
        match session {
            None => SessionSelection::New,
            Some("") => SessionSelection::Pick,
            Some(id) => SessionSelection::Resume(id.to_owned()),
        }
    }
}

fn parse_headless_goal_create(prompt: &str) -> Result<Option<GoalCreateRequest>, ValidationError> {
    let trimmed = prompt.trim();
    let Some(rest) = trimmed.strip_prefix("/goal") else {
        return Ok(None);
    };
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return Ok(None);
    }
    let args = rest.trim();
    if matches!(args, "" | "status" | "pause" | "resume" | "cancel" | "next")
        || args.starts_with("next ")
    {
        return Ok(None);
    }

    let (replace, objective) = if let Some(rest) = args.strip_prefix("replace") {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            (true, rest.trim())
        } else {
            (false, args)
        }
    } else {
        (false, args)
    };
    let objective = objective
        .strip_prefix("--")
        .map_or(objective, str::trim_start);
    if objective.is_empty() {
        return Err(ValidationError::new("Goal objective is required."));
    }
    if objective.chars().count() > 4_000 {
        return Err(ValidationError::new(
            "Goal objective must be 4000 characters or fewer.",
        ));
    }
    Ok(Some(GoalCreateRequest {
        objective: objective.to_owned(),
        replace,
    }))
}
