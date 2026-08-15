//! Executable provider-command dispatch independent of the terminal frontend.

use std::{error::Error, fmt, future::Future, io::Write, path::PathBuf, pin::Pin, sync::Arc};

use mycel_agent_protocol::ProviderType;
use mycel_providers::{DiscoveryCancellationToken, ImportWireFamily, ProviderCatalogListItem};
use serde::Serialize;

use crate::{
    cli::{
        validate_provider_command, CatalogCommand, Command, ProviderAuthTarget, ProviderCommand,
    },
    exit::TerminationSignal,
    provider_commands::{
        CatalogAddRequest, CatalogListResult, ConfiguredProviderView, CredentialStatus,
        CustomRegistryRequest, ProviderCommandCancellation, ProviderCommandClock,
        ProviderCommandDependencies, ProviderCommandEnvironment, ProviderCommandError,
        ProviderCommandEvent, ProviderCommandInput, ProviderCommandOutput, ProviderCommandService,
        ProviderConfigStore, ProviderMutationSummary,
    },
    runtime::{AdapterOutput, RuntimeCompletion},
};

const OPERATION: &str = "provider command";

pub type ProviderCommandSignalFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), ProviderCommandRunnerError>> + Send + 'a>>;

/// Awaitable interrupt boundary. Tests inject deterministic signals; process
/// integration uses Tokio's cross-platform Ctrl-C listener.
pub trait ProviderCommandSignal: Send + Sync {
    fn interrupted<'a>(&'a self) -> ProviderCommandSignalFuture<'a>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessProviderCommandSignal;

impl ProviderCommandSignal for ProcessProviderCommandSignal {
    fn interrupted<'a>(&'a self) -> ProviderCommandSignalFuture<'a> {
        Box::pin(async {
            tokio::signal::ctrl_c().await.map_err(|error| {
                ProviderCommandRunnerError::Signal(format!(
                    "could not install Ctrl-C listener: {error}"
                ))
            })
        })
    }
}

/// Immediate stderr boundary used for device-authorization instructions.
pub trait ProviderCommandStderr: Send + Sync {
    fn write_and_flush(&self, message: &str) -> Result<(), ProviderCommandError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessProviderCommandStderr;

impl ProviderCommandStderr for ProcessProviderCommandStderr {
    fn write_and_flush(&self, message: &str) -> Result<(), ProviderCommandError> {
        let mut stderr = std::io::stderr().lock();
        stderr.write_all(message.as_bytes()).map_err(|error| {
            ProviderCommandError::Io(format!("could not write provider instructions: {error}"))
        })?;
        stderr.flush().map_err(|error| {
            ProviderCommandError::Io(format!("could not flush provider instructions: {error}"))
        })
    }
}

struct StreamingProviderOutput {
    stderr: Arc<dyn ProviderCommandStderr>,
}

impl ProviderCommandOutput for StreamingProviderOutput {
    fn emit(&self, event: ProviderCommandEvent) -> Result<(), ProviderCommandError> {
        if let ProviderCommandEvent::DeviceAuthorization {
            user_code,
            verification_uri,
            expires_in,
        } = event
        {
            let message = format!(
                "Open {}\nEnter code: {}\nAuthorization expires in {} seconds.\n",
                terminal_field(&verification_uri),
                terminal_field(&user_code),
                expires_in
            );
            self.stderr.write_and_flush(&message)?;
        }
        Ok(())
    }
}

pub struct ProviderCommandRunnerDependencies {
    pub transport: Arc<dyn mycel_providers::HttpTransport>,
    pub config_store: Arc<dyn ProviderConfigStore>,
    pub environment: Arc<dyn ProviderCommandEnvironment>,
    pub input: Arc<dyn ProviderCommandInput>,
    pub clock: Arc<dyn ProviderCommandClock>,
    pub stderr: Arc<dyn ProviderCommandStderr>,
}

/// One-call composition boundary for parsed provider commands.
pub struct ProviderCommandRunner {
    service: ProviderCommandService,
}

impl ProviderCommandRunner {
    pub fn new(
        dependencies: ProviderCommandRunnerDependencies,
        mycel_home: PathBuf,
        version: impl Into<String>,
    ) -> Self {
        let output = Arc::new(StreamingProviderOutput {
            stderr: dependencies.stderr,
        });
        Self {
            service: ProviderCommandService::new(
                ProviderCommandDependencies {
                    transport: dependencies.transport,
                    config_store: dependencies.config_store,
                    environment: dependencies.environment,
                    input: dependencies.input,
                    output,
                    clock: dependencies.clock,
                },
                mycel_home,
                version,
            ),
        }
    }

    pub async fn run(
        &self,
        command: &Command,
        cancellation: &ProviderCommandCancellation,
    ) -> Result<AdapterOutput, ProviderCommandRunnerError> {
        if cancellation.is_cancelled() {
            return Ok(interrupted());
        }
        match command {
            Command::Login => self.login_kimi(cancellation).await,
            Command::Provider(arguments) => {
                validate_provider_command(&arguments.command)
                    .map_err(|error| ProviderCommandRunnerError::Validation(error.to_string()))?;
                self.run_provider(&arguments.command, cancellation).await
            }
            Command::Doctor(_) | Command::Export(_) => {
                Err(ProviderCommandRunnerError::UnsupportedCommand)
            }
        }
    }

    /// Execute with a real process Ctrl-C listener. A received interrupt
    /// cancels the in-flight OAuth/discovery operation and returns exit 130.
    pub async fn run_with_process_sigint(
        &self,
        command: &Command,
    ) -> Result<AdapterOutput, ProviderCommandRunnerError> {
        self.run_with_signal(command, &ProcessProviderCommandSignal)
            .await
    }

    /// Signal-injected counterpart used by non-process frontends and tests.
    pub async fn run_with_signal(
        &self,
        command: &Command,
        signal: &dyn ProviderCommandSignal,
    ) -> Result<AdapterOutput, ProviderCommandRunnerError> {
        let cancellation = ProviderCommandCancellation::default();
        let operation = self.run(command, &cancellation);
        tokio::pin!(operation);
        tokio::select! {
            result = &mut operation => result,
            signal_result = signal.interrupted() => {
                signal_result?;
                cancellation.cancel();
                operation.await
            }
        }
    }

    async fn run_provider(
        &self,
        command: &ProviderCommand,
        cancellation: &ProviderCommandCancellation,
    ) -> Result<AdapterOutput, ProviderCommandRunnerError> {
        match command {
            ProviderCommand::Login { provider } => match provider {
                ProviderAuthTarget::Kimi => self.login_kimi(cancellation).await,
            },
            ProviderCommand::Logout { provider } => {
                if cancellation.is_cancelled() {
                    return Ok(interrupted());
                }
                match provider {
                    ProviderAuthTarget::Kimi => self.service.logout_kimi()?,
                }
                Ok(AdapterOutput::success("Logged out of kimi.\n", ""))
            }
            ProviderCommand::List { json } => {
                if cancellation.is_cancelled() {
                    return Ok(interrupted());
                }
                let providers = self.service.list()?;
                Ok(AdapterOutput::success(
                    format_provider_list(&providers, *json)?,
                    "",
                ))
            }
            ProviderCommand::Remove { provider_id } => {
                if cancellation.is_cancelled() {
                    return Ok(interrupted());
                }
                let summary = self.service.remove(provider_id)?;
                Ok(AdapterOutput::success(
                    format_mutation("Removed provider", Some(provider_id), &summary),
                    "",
                ))
            }
            ProviderCommand::Add { url, api_key } => {
                let request = CustomRegistryRequest {
                    url: url.clone(),
                    api_key: api_key.clone(),
                };
                let summary = self
                    .with_discovery(cancellation, |token| {
                        self.service.custom_registry_add(request, Some(token))
                    })
                    .await?;
                match summary {
                    Some(summary) => Ok(AdapterOutput::success(
                        format_mutation("Imported registry", None, &summary),
                        "",
                    )),
                    None => Ok(interrupted()),
                }
            }
            ProviderCommand::Catalog(arguments) => match &arguments.command {
                CatalogCommand::List {
                    provider_id,
                    filter,
                    url,
                    json,
                } => {
                    let result = self
                        .with_discovery(cancellation, |token| {
                            self.service.catalog_list(
                                url,
                                provider_id.as_deref(),
                                filter.as_deref(),
                                Some(token),
                            )
                        })
                        .await?;
                    match result {
                        Some(result) => Ok(AdapterOutput::success(
                            format_catalog_list(&result, *json)?,
                            "",
                        )),
                        None => Ok(interrupted()),
                    }
                }
                CatalogCommand::Add {
                    provider_id,
                    api_key,
                    default_model,
                    url,
                } => {
                    let request = CatalogAddRequest {
                        url: url.clone(),
                        provider_id: provider_id.clone(),
                        api_key: api_key.clone(),
                        default_model: default_model.clone(),
                    };
                    let summary = self
                        .with_discovery(cancellation, |token| {
                            self.service.catalog_add(request, Some(token))
                        })
                        .await?;
                    match summary {
                        Some(summary) => Ok(AdapterOutput::success(
                            format_mutation(
                                "Imported catalog provider",
                                Some(provider_id),
                                &summary,
                            ),
                            "",
                        )),
                        None => Ok(interrupted()),
                    }
                }
            },
        }
    }

    async fn login_kimi(
        &self,
        cancellation: &ProviderCommandCancellation,
    ) -> Result<AdapterOutput, ProviderCommandRunnerError> {
        match self.service.login_kimi(cancellation).await {
            Ok(()) => Ok(AdapterOutput::success("Logged in to kimi.\n", "")),
            Err(ProviderCommandError::Cancelled) => Ok(interrupted()),
            Err(error) => Err(error.into()),
        }
    }

    async fn with_discovery<T, F, Fut>(
        &self,
        cancellation: &ProviderCommandCancellation,
        operation: F,
    ) -> Result<Option<T>, ProviderCommandRunnerError>
    where
        F: FnOnce(DiscoveryCancellationToken) -> Fut,
        Fut: Future<Output = Result<T, ProviderCommandError>>,
    {
        if cancellation.is_cancelled() {
            return Ok(None);
        }
        let discovery = DiscoveryCancellationToken::default();
        let future = operation(discovery.clone());
        tokio::pin!(future);
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                discovery.cancel();
                Ok(None)
            }
            result = &mut future => match result {
                Err(ProviderCommandError::Cancelled) => Ok(None),
                result => result.map(Some).map_err(Into::into),
            }
        }
    }
}

#[derive(Debug)]
pub enum ProviderCommandRunnerError {
    UnsupportedCommand,
    Validation(String),
    Command(ProviderCommandError),
    Formatting(String),
    Signal(String),
}

impl fmt::Display for ProviderCommandRunnerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedCommand => formatter.write_str("not a provider command"),
            Self::Validation(message) | Self::Formatting(message) | Self::Signal(message) => {
                formatter.write_str(message)
            }
            Self::Command(error) => error.fmt(formatter),
        }
    }
}

impl Error for ProviderCommandRunnerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Command(error) => Some(error),
            Self::UnsupportedCommand
            | Self::Validation(_)
            | Self::Formatting(_)
            | Self::Signal(_) => None,
        }
    }
}

impl From<ProviderCommandError> for ProviderCommandRunnerError {
    fn from(value: ProviderCommandError) -> Self {
        Self::Command(value)
    }
}

impl ProviderCommandRunnerError {
    pub const fn operation(&self) -> &'static str {
        OPERATION
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderListJson<'a> {
    providers: Vec<ProviderJson<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderJson<'a> {
    id: &'a str,
    provider_type: &'static str,
    base_url: Option<&'a str>,
    model_count: usize,
    credential: String,
    is_default: bool,
}

fn format_provider_list(
    providers: &[ConfiguredProviderView],
    json: bool,
) -> Result<String, ProviderCommandRunnerError> {
    if json {
        let value = ProviderListJson {
            providers: providers
                .iter()
                .map(|provider| ProviderJson {
                    id: &provider.id,
                    provider_type: provider_type_name(provider.provider_type),
                    base_url: provider.base_url.as_deref(),
                    model_count: provider.model_count,
                    credential: credential_name(&provider.credential),
                    is_default: provider.is_default,
                })
                .collect(),
        };
        return encode_json(&value);
    }
    let mut output = String::from("ID\tTYPE\tMODELS\tCREDENTIAL\tDEFAULT\tBASE URL\n");
    for provider in providers {
        output.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\n",
            terminal_field(&provider.id),
            provider_type_name(provider.provider_type),
            provider.model_count,
            credential_name(&provider.credential),
            if provider.is_default { "yes" } else { "no" },
            terminal_field(provider.base_url.as_deref().unwrap_or("-"))
        ));
    }
    Ok(output)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogProvidersJson<'a> {
    providers: Vec<CatalogProviderJson<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogProviderJson<'a> {
    id: &'a str,
    display_name: &'a str,
    wire: Option<&'static str>,
    raw_model_count: usize,
    usable_model_count: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogModelsJson<'a> {
    provider_id: &'a str,
    display_name: &'a str,
    models: &'a [String],
}

fn format_catalog_list(
    result: &CatalogListResult,
    json: bool,
) -> Result<String, ProviderCommandRunnerError> {
    match result {
        CatalogListResult::Providers(providers) if json => encode_json(&CatalogProvidersJson {
            providers: providers
                .iter()
                .map(|provider| CatalogProviderJson {
                    id: &provider.id,
                    display_name: &provider.display_name,
                    wire: provider.wire.map(wire_name),
                    raw_model_count: provider.raw_model_count,
                    usable_model_count: provider.usable_model_count,
                })
                .collect(),
        }),
        CatalogListResult::Models {
            provider_id,
            display_name,
            models,
        } if json => encode_json(&CatalogModelsJson {
            provider_id,
            display_name,
            models,
        }),
        CatalogListResult::Providers(providers) => Ok(format_catalog_provider_text(providers)),
        CatalogListResult::Models {
            provider_id,
            display_name,
            models,
        } => {
            let mut output = format!(
                "{} ({})\n",
                terminal_field(display_name),
                terminal_field(provider_id)
            );
            for model in models {
                output.push_str(&format!("{}\n", terminal_field(model)));
            }
            Ok(output)
        }
    }
}

fn format_catalog_provider_text(providers: &[ProviderCatalogListItem]) -> String {
    let mut output = String::from("ID\tNAME\tWIRE\tUSABLE\tTOTAL\n");
    for provider in providers {
        output.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            terminal_field(&provider.id),
            terminal_field(&provider.display_name),
            provider.wire.map_or("unsupported", wire_name),
            provider.usable_model_count,
            provider.raw_model_count
        ));
    }
    output
}

fn format_mutation(
    action: &str,
    subject: Option<&str>,
    summary: &ProviderMutationSummary,
) -> String {
    let mut output = action.to_owned();
    if let Some(subject) = subject {
        output.push(' ');
        output.push_str(&terminal_field(subject));
    }
    output.push_str(".\nProviders: ");
    output.push_str(&format_values(&summary.providers));
    output.push_str("\nModels: ");
    output.push_str(&format_values(&summary.models));
    output.push_str("\nDefault provider: ");
    output.push_str(
        &summary
            .default_provider
            .as_deref()
            .map(terminal_field)
            .unwrap_or_else(|| "-".to_owned()),
    );
    output.push_str("\nDefault model: ");
    output.push_str(
        &summary
            .default_model
            .as_deref()
            .map(terminal_field)
            .unwrap_or_else(|| "-".to_owned()),
    );
    output.push('\n');
    output
}

fn format_values(values: &[String]) -> String {
    if values.is_empty() {
        "-".to_owned()
    } else {
        values
            .iter()
            .map(|value| terminal_field(value))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn credential_name(credential: &CredentialStatus) -> String {
    match credential {
        CredentialStatus::Configured => "configured".to_owned(),
        CredentialStatus::Codex => "codex".to_owned(),
        CredentialStatus::Environment(name) => format!("environment:{name}"),
        CredentialStatus::Missing => "missing".to_owned(),
    }
}

const fn provider_type_name(provider_type: ProviderType) -> &'static str {
    match provider_type {
        ProviderType::Anthropic => "anthropic",
        ProviderType::OpenAi => "openai",
        ProviderType::Kimi => "kimi",
        ProviderType::GoogleGenAi => "google-genai",
        ProviderType::OpenAiResponses => "openai_responses",
        ProviderType::VertexAi => "vertexai",
    }
}

const fn wire_name(wire: ImportWireFamily) -> &'static str {
    match wire {
        ImportWireFamily::Anthropic => "anthropic",
        ImportWireFamily::OpenAiChat => "openai-chat",
        ImportWireFamily::OpenAiResponses => "openai-responses",
        ImportWireFamily::Kimi => "kimi",
        ImportWireFamily::Gemini => "gemini",
        ImportWireFamily::Vertex => "vertex",
    }
}

fn encode_json(value: &impl Serialize) -> Result<String, ProviderCommandRunnerError> {
    serde_json::to_string(value)
        .map(|mut encoded| {
            encoded.push('\n');
            encoded
        })
        .map_err(|_| {
            ProviderCommandRunnerError::Formatting("could not format provider output".into())
        })
}

fn terminal_field(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect()
}

fn interrupted() -> AdapterOutput {
    AdapterOutput {
        stdout: String::new(),
        stderr: String::new(),
        completion: RuntimeCompletion::Signal(TerminationSignal::Interrupt),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::{BTreeMap, VecDeque},
        path::Path,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Mutex,
        },
        time::Duration,
    };

    use bytes::Bytes;
    use futures_util::stream;
    use mycel_agent_protocol::SecretString;
    use mycel_providers::{HttpRequest, HttpResponse, HttpTransport, TransportFuture};

    enum FakeResponse {
        Json(u16, serde_json::Value),
        Pending,
    }

    #[derive(Default)]
    struct FakeTransport {
        responses: Mutex<VecDeque<FakeResponse>>,
        request_count: AtomicUsize,
        required_before_second: Mutex<Option<Arc<AtomicBool>>>,
    }

    impl FakeTransport {
        fn response(&self, status: u16, value: serde_json::Value) {
            self.responses
                .lock()
                .unwrap()
                .push_back(FakeResponse::Json(status, value));
        }

        fn pending(&self) {
            self.responses
                .lock()
                .unwrap()
                .push_back(FakeResponse::Pending);
        }

        fn require_before_second(&self, observed: Arc<AtomicBool>) {
            *self.required_before_second.lock().unwrap() = Some(observed);
        }
    }

    impl HttpTransport for FakeTransport {
        fn send<'a>(&'a self, _request: HttpRequest) -> TransportFuture<'a> {
            let request_number = self.request_count.fetch_add(1, Ordering::AcqRel) + 1;
            if request_number == 2 {
                if let Some(required) = self.required_before_second.lock().unwrap().as_ref() {
                    assert!(
                        required.load(Ordering::Acquire),
                        "device instructions must be flushed before token polling"
                    );
                }
            }
            let response = self.responses.lock().unwrap().pop_front().unwrap();
            match response {
                FakeResponse::Json(status, value) => Box::pin(async move {
                    Ok(HttpResponse {
                        status,
                        headers: BTreeMap::new(),
                        body: Box::pin(stream::iter([Ok(Bytes::from(
                            serde_json::to_vec(&value).unwrap(),
                        ))])),
                    })
                }),
                FakeResponse::Pending => Box::pin(std::future::pending()),
            }
        }
    }

    struct MemoryConfigStore {
        path: PathBuf,
        source: Mutex<String>,
    }

    impl MemoryConfigStore {
        fn new(source: &str) -> Self {
            Self {
                path: PathBuf::from("/memory/config.toml"),
                source: Mutex::new(source.to_owned()),
            }
        }
    }

    impl ProviderConfigStore for MemoryConfigStore {
        fn path(&self) -> &Path {
            &self.path
        }

        fn load(&self) -> Result<String, ProviderCommandError> {
            Ok(self.source.lock().unwrap().clone())
        }

        fn replace(&self, source: &str) -> Result<(), ProviderCommandError> {
            *self.source.lock().unwrap() = source.to_owned();
            Ok(())
        }
    }

    #[derive(Default)]
    struct EmptyEnvironment;

    impl ProviderCommandEnvironment for EmptyEnvironment {
        fn get(&self, _key: &str) -> Option<String> {
            None
        }
    }

    struct FixedInput(Option<SecretString>);

    impl ProviderCommandInput for FixedInput {
        fn api_key(
            &self,
            _provider_id: &str,
        ) -> Result<Option<SecretString>, ProviderCommandError> {
            Ok(self.0.clone())
        }
    }

    #[derive(Default)]
    struct ImmediateClock;

    impl ProviderCommandClock for ImmediateClock {
        fn now_seconds(&self) -> u64 {
            1
        }

        fn sleep<'a>(
            &'a self,
            _duration: Duration,
        ) -> crate::provider_commands::ProviderCommandFuture<'a> {
            Box::pin(async {})
        }
    }

    #[derive(Default)]
    struct RecordingStderr {
        messages: Mutex<Vec<String>>,
        flushed: Arc<AtomicBool>,
    }

    impl ProviderCommandStderr for RecordingStderr {
        fn write_and_flush(&self, message: &str) -> Result<(), ProviderCommandError> {
            self.messages.lock().unwrap().push(message.to_owned());
            self.flushed.store(true, Ordering::Release);
            Ok(())
        }
    }

    fn runner(
        transport: Arc<FakeTransport>,
        store: Arc<MemoryConfigStore>,
        stderr: Arc<RecordingStderr>,
        home: PathBuf,
    ) -> ProviderCommandRunner {
        ProviderCommandRunner::new(
            ProviderCommandRunnerDependencies {
                transport,
                config_store: store,
                environment: Arc::new(EmptyEnvironment),
                input: Arc::new(FixedInput(Some(SecretString::new("input-secret")))),
                clock: Arc::new(ImmediateClock),
                stderr,
            },
            home,
            "test",
        )
    }

    fn empty_store() -> Arc<MemoryConfigStore> {
        Arc::new(MemoryConfigStore::new(""))
    }

    fn provider(command: ProviderCommand) -> Command {
        Command::Provider(crate::cli::ProviderArgs { command })
    }

    #[tokio::test]
    async fn list_has_stable_text_and_json_without_secret_values() {
        let store = Arc::new(MemoryConfigStore::new(
            r#"
default_provider = "zeta"

[providers.alpha]
type = "anthropic"

[providers.responses]
type = "openai_responses"
oauth = { storage = "codex", key = "codex" }

[providers.zeta]
type = "openai"
api_key = "do-not-print"
base_url = "https://example.invalid/v1"

[models.main]
provider = "zeta"
model = "gpt-test"
max_context_size = 4096
"#,
        ));
        let runner = runner(
            Arc::new(FakeTransport::default()),
            store,
            Arc::new(RecordingStderr::default()),
            PathBuf::from("/memory/home"),
        );
        let cancellation = ProviderCommandCancellation::default();

        let text = runner
            .run(
                &provider(ProviderCommand::List { json: false }),
                &cancellation,
            )
            .await
            .unwrap();
        assert_eq!(
            text.stdout,
            "ID\tTYPE\tMODELS\tCREDENTIAL\tDEFAULT\tBASE URL\nalpha\tanthropic\t0\tmissing\tno\t-\nresponses\topenai_responses\t0\tcodex\tno\t-\nzeta\topenai\t1\tconfigured\tyes\thttps://example.invalid/v1\n"
        );
        let json = runner
            .run(
                &provider(ProviderCommand::List { json: true }),
                &cancellation,
            )
            .await
            .unwrap();
        assert_eq!(
            json.stdout,
            "{\"providers\":[{\"id\":\"alpha\",\"providerType\":\"anthropic\",\"baseUrl\":null,\"modelCount\":0,\"credential\":\"missing\",\"isDefault\":false},{\"id\":\"responses\",\"providerType\":\"openai_responses\",\"baseUrl\":null,\"modelCount\":0,\"credential\":\"codex\",\"isDefault\":false},{\"id\":\"zeta\",\"providerType\":\"openai\",\"baseUrl\":\"https://example.invalid/v1\",\"modelCount\":1,\"credential\":\"configured\",\"isDefault\":true}]}\n"
        );
        assert!(!text.stdout.contains("do-not-print"));
        assert!(!json.stdout.contains("do-not-print"));
    }

    #[tokio::test]
    async fn top_level_and_nested_login_stream_instructions_and_hide_tokens() {
        for command in [
            Command::Login,
            provider(ProviderCommand::Login {
                provider: ProviderAuthTarget::Kimi,
            }),
        ] {
            let home = tempfile::tempdir().unwrap();
            let transport = Arc::new(FakeTransport::default());
            transport.response(
                200,
                serde_json::json!({
                    "user_code":"ABCD-1234",
                    "device_code":"device-secret",
                    "verification_uri":"https://auth.kimi.com/device",
                    "verification_uri_complete":"https://auth.kimi.com/device",
                    "expires_in":60,
                    "interval":1
                }),
            );
            transport.response(
                200,
                serde_json::json!({
                    "access_token":"access-secret",
                    "refresh_token":"refresh-secret",
                    "expires_in":3600
                }),
            );
            let stderr = Arc::new(RecordingStderr::default());
            transport.require_before_second(Arc::clone(&stderr.flushed));
            let runner = runner(
                transport,
                empty_store(),
                Arc::clone(&stderr),
                home.path().to_path_buf(),
            );
            let output = runner
                .run(&command, &ProviderCommandCancellation::default())
                .await
                .unwrap();
            assert_eq!(output.stdout, "Logged in to kimi.\n");
            let messages = stderr.messages.lock().unwrap();
            assert_eq!(
                messages.as_slice(),
                ["Open https://auth.kimi.com/device\nEnter code: ABCD-1234\nAuthorization expires in 60 seconds.\n"]
            );
            let visible = format!("{}{}", output.stdout, messages.concat());
            for secret in ["device-secret", "access-secret", "refresh-secret"] {
                assert!(!visible.contains(secret));
            }
        }
    }

    #[tokio::test]
    async fn cancellation_returns_interrupt_without_work() {
        let runner = runner(
            Arc::new(FakeTransport::default()),
            empty_store(),
            Arc::new(RecordingStderr::default()),
            PathBuf::from("/memory/home"),
        );
        let cancellation = ProviderCommandCancellation::default();
        cancellation.cancel();
        let output = runner
            .run(
                &provider(ProviderCommand::List { json: false }),
                &cancellation,
            )
            .await
            .unwrap();
        assert_eq!(
            output.completion,
            RuntimeCompletion::Signal(TerminationSignal::Interrupt)
        );
        assert!(output.stdout.is_empty());
    }

    struct YieldingSignal;

    impl ProviderCommandSignal for YieldingSignal {
        fn interrupted<'a>(&'a self) -> ProviderCommandSignalFuture<'a> {
            Box::pin(async {
                tokio::task::yield_now().await;
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn injected_sigint_cancels_an_in_flight_catalog_request() {
        let transport = Arc::new(FakeTransport::default());
        transport.pending();
        let runner = runner(
            transport,
            empty_store(),
            Arc::new(RecordingStderr::default()),
            PathBuf::from("/memory/home"),
        );
        let command = provider(ProviderCommand::Catalog(crate::cli::CatalogArgs {
            command: CatalogCommand::List {
                provider_id: None,
                filter: None,
                url: "https://catalog.invalid/models.json".to_owned(),
                json: false,
            },
        }));

        let output = runner
            .run_with_signal(&command, &YieldingSignal)
            .await
            .unwrap();

        assert_eq!(
            output.completion,
            RuntimeCompletion::Signal(TerminationSignal::Interrupt)
        );
        assert!(output.stdout.is_empty());
    }

    #[tokio::test]
    async fn injected_sigint_cancels_device_poll_after_instructions_are_flushed() {
        let home = tempfile::tempdir().unwrap();
        let transport = Arc::new(FakeTransport::default());
        transport.response(
            200,
            serde_json::json!({
                "user_code":"WXYZ-9876",
                "device_code":"device-secret",
                "verification_uri_complete":"https://auth.kimi.com/device",
                "expires_in":60,
                "interval":1
            }),
        );
        transport.pending();
        let stderr = Arc::new(RecordingStderr::default());
        transport.require_before_second(Arc::clone(&stderr.flushed));
        let runner = runner(
            transport,
            empty_store(),
            Arc::clone(&stderr),
            home.path().to_path_buf(),
        );

        let output = runner
            .run_with_signal(&Command::Login, &YieldingSignal)
            .await
            .unwrap();

        assert_eq!(
            output.completion,
            RuntimeCompletion::Signal(TerminationSignal::Interrupt)
        );
        assert_eq!(stderr.messages.lock().unwrap().len(), 1);
        assert!(!home.path().join("credentials/kimi-code.json").exists());
    }

    #[tokio::test]
    async fn non_provider_commands_return_a_typed_error() {
        let runner = runner(
            Arc::new(FakeTransport::default()),
            empty_store(),
            Arc::new(RecordingStderr::default()),
            PathBuf::from("/memory/home"),
        );
        let error = runner
            .run(
                &Command::Doctor(crate::cli::DoctorArgs { target: None }),
                &ProviderCommandCancellation::default(),
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ProviderCommandRunnerError::UnsupportedCommand
        ));
        assert_eq!(error.operation(), "provider command");
    }

    #[tokio::test]
    async fn dispatches_logout_remove_and_rejects_invalid_registry_before_transport() {
        let home = tempfile::tempdir().unwrap();
        let runner = runner(
            Arc::new(FakeTransport::default()),
            Arc::new(MemoryConfigStore::new(
                r#"
[providers.old]
type = "openai"
api_key = "hidden"
"#,
            )),
            Arc::new(RecordingStderr::default()),
            home.path().to_path_buf(),
        );
        let cancellation = ProviderCommandCancellation::default();
        let logout = runner
            .run(
                &provider(ProviderCommand::Logout {
                    provider: ProviderAuthTarget::Kimi,
                }),
                &cancellation,
            )
            .await
            .unwrap();
        assert_eq!(logout.stdout, "Logged out of kimi.\n");

        let removed = runner
            .run(
                &provider(ProviderCommand::Remove {
                    provider_id: "old".to_owned(),
                }),
                &cancellation,
            )
            .await
            .unwrap();
        assert_eq!(
            removed.stdout,
            "Removed provider old.\nProviders: -\nModels: -\nDefault provider: -\nDefault model: -\n"
        );

        let error = runner
            .run(
                &provider(ProviderCommand::Add {
                    url: "https://user:secret@example.invalid/api.json".to_owned(),
                    api_key: Some(SecretString::new("another-secret")),
                }),
                &cancellation,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ProviderCommandRunnerError::Validation(_)));
        let visible = error.to_string();
        assert!(!visible.contains("secret"));
        assert!(!visible.contains("another-secret"));
    }

    #[tokio::test]
    async fn dispatches_catalog_list_detail_and_add_and_custom_add() {
        let catalog = serde_json::json!({
            "acme": {
                "name": "Acme\nModels",
                "api": "https://api.acme.invalid/v1",
                "env": ["ACME_API_KEY"],
                "npm": "@ai-sdk/openai-compatible",
                "models": {
                    "chat-a": {
                        "id": "chat-a",
                        "name": "Chat A",
                        "limit": {"context": 8192, "output": 1024},
                        "tool_call": true
                    }
                }
            }
        });
        let custom = serde_json::json!({
            "other": {
                "id": "other",
                "name": "Other",
                "type": "openai",
                "api": "https://api.other.invalid/v1",
                "models": {
                    "model-b": {"id":"model-b","maxContextSize":4096}
                }
            }
        });
        let transport = Arc::new(FakeTransport::default());
        transport.response(200, catalog.clone());
        transport.response(200, catalog.clone());
        transport.response(200, catalog);
        transport.response(200, custom);
        let runner = runner(
            transport,
            empty_store(),
            Arc::new(RecordingStderr::default()),
            PathBuf::from("/memory/home"),
        );
        let cancellation = ProviderCommandCancellation::default();

        let providers = runner
            .run(
                &provider(ProviderCommand::Catalog(crate::cli::CatalogArgs {
                    command: CatalogCommand::List {
                        provider_id: None,
                        filter: None,
                        url: "https://catalog.invalid/models.json".to_owned(),
                        json: false,
                    },
                })),
                &cancellation,
            )
            .await
            .unwrap();
        assert!(providers.stdout.contains("Acme�Models"));

        let models = runner
            .run(
                &provider(ProviderCommand::Catalog(crate::cli::CatalogArgs {
                    command: CatalogCommand::List {
                        provider_id: Some("acme".to_owned()),
                        filter: None,
                        url: "https://catalog.invalid/models.json".to_owned(),
                        json: true,
                    },
                })),
                &cancellation,
            )
            .await
            .unwrap();
        assert_eq!(
            models.stdout,
            "{\"providerId\":\"acme\",\"displayName\":\"Acme\\nModels\",\"models\":[\"chat-a\"]}\n"
        );

        let added = runner
            .run(
                &provider(ProviderCommand::Catalog(crate::cli::CatalogArgs {
                    command: CatalogCommand::Add {
                        provider_id: "acme".to_owned(),
                        api_key: Some(SecretString::new("catalog-secret")),
                        default_model: Some("chat-a".to_owned()),
                        url: "https://catalog.invalid/models.json".to_owned(),
                    },
                })),
                &cancellation,
            )
            .await
            .unwrap();
        assert!(added
            .stdout
            .starts_with("Imported catalog provider acme.\n"));
        assert!(!added.stdout.contains("catalog-secret"));

        let custom_added = runner
            .run(
                &provider(ProviderCommand::Add {
                    url: "https://registry.invalid/api.json".to_owned(),
                    api_key: Some(SecretString::new("custom-secret")),
                }),
                &cancellation,
            )
            .await
            .unwrap();
        assert!(custom_added.stdout.starts_with("Imported registry.\n"));
        assert!(!custom_added.stdout.contains("custom-secret"));
    }
}
