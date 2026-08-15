use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use mycel_agent_protocol::{
    ChatProvider, ModelCapability, ProviderError, ProviderErrorKind, ProviderEventStream,
    ProviderRequest, ProviderRequestAuth, SecretString,
};
use reqwest::header::{HeaderName, HeaderValue};
use url::Url;

use crate::{
    auth::{
        discover_kimi_models, stream_with_refresh, AuthFuture, CodexStatusSource,
        CodexSubscriptionAuth, CredentialStore, KimiIdentity, KimiOAuthClient, KimiOAuthConfig,
        KimiTokenProvider, ProcessCodexStatusSource, RequestAuthProvider,
        CODEX_SUBSCRIPTION_BASE_URL, KIMI_CLIENT_ID,
    },
    capabilities::{detect_capability, ProviderFamily},
    error::invalid_request,
    google_auth::{
        GoogleServiceAccountCredentialSource, GoogleServiceAccountTokenProvider, SystemUnixClock,
        UnixClock,
    },
    http::HttpTransport,
    providers::{
        AnthropicProvider, GoogleEndpoint, GoogleProvider, KimiProvider, OpenAiChatProvider,
        OpenAiResponsesProvider,
    },
};

#[cfg(test)]
use crate::google_auth::AssertionSigner;

/// Complete typed input for constructing the provider boundary used by the
/// runtime. Credential sources are explicit; the application-default Google
/// source is the sole exception and reads only `GOOGLE_APPLICATION_CREDENTIALS`.
/// The factory override keeps that path deterministic in tests and embedders.
#[derive(Clone, Debug, Default)]
pub struct ProviderRegistryConfig {
    pub providers: Vec<ProviderConfig>,
}

#[derive(Clone, Debug)]
pub struct ProviderConfig {
    pub id: String,
    pub adapter: ProviderAdapterConfig,
    pub credential: ProviderCredentialConfig,
    pub headers: BTreeMap<String, String>,
    pub models: Vec<ProviderModelConfig>,
}

#[derive(Clone, Debug)]
pub enum ProviderAdapterConfig {
    Anthropic {
        base_url: Option<String>,
        beta_api: bool,
        beta_features: Vec<String>,
        adaptive_thinking: Option<bool>,
    },
    OpenAiChat {
        base_url: Option<String>,
    },
    OpenAiResponses {
        base_url: Option<String>,
    },
    Kimi {
        base_url: Option<String>,
    },
    Gemini {
        base_url: Option<String>,
    },
    VertexApiKey {
        base_url: Option<String>,
    },
    VertexServiceAccount {
        base_url: Option<String>,
        project: String,
        location: String,
    },
    ManagedKimi {
        oauth_host: String,
        api_base_url: String,
        client_id: String,
    },
    CodexSubscription,
}

#[derive(Clone, Debug)]
pub enum ProviderCredentialConfig {
    ApiKey(ApiKeyCredentialConfig),
    GoogleServiceAccount(GoogleServiceAccountCredentialSource),
    ManagedKimi,
    CodexSubscription,
}

/// Static credential candidates after configuration and environment parsing.
/// A non-empty configured value is authoritative; the environment value is
/// used only when configured is absent.
#[derive(Clone, Debug, Default)]
pub struct ApiKeyCredentialConfig {
    pub configured: Option<SecretString>,
    pub environment: Option<SecretString>,
    pub headers: BTreeMap<String, SecretString>,
}

impl ApiKeyCredentialConfig {
    pub fn configured(api_key: impl Into<String>) -> Self {
        Self {
            configured: Some(SecretString::new(api_key)),
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProviderModelConfig {
    pub id: String,
    pub display_name: Option<String>,
    pub capability: Option<ModelCapability>,
}

impl ProviderModelConfig {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            display_name: None,
            capability: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderWireProtocol {
    AnthropicMessages,
    OpenAiChatCompletions,
    OpenAiResponses,
    KimiChatCompletions,
    GoogleGenerateContent,
}

#[derive(Clone, Debug)]
pub struct ProviderModelInfo {
    pub provider_id: String,
    pub model: String,
    pub display_name: Option<String>,
    pub capability: ModelCapability,
    pub always_thinking: bool,
    pub thinking_efforts: Vec<String>,
    pub default_thinking_effort: Option<String>,
    pub wire_protocol: ProviderWireProtocol,
}

#[derive(Clone)]
pub struct ProviderBinding {
    info: ProviderModelInfo,
    provider: Arc<dyn ChatProvider>,
    auth: Arc<dyn RequestAuthProvider>,
}

impl ProviderBinding {
    pub fn model_info(&self) -> &ProviderModelInfo {
        &self.info
    }

    pub fn provider(&self) -> &dyn ChatProvider {
        self.provider.as_ref()
    }

    pub async fn request_auth(
        &self,
        force_refresh: bool,
    ) -> Result<ProviderRequestAuth, ProviderError> {
        self.auth.request_auth(force_refresh).await
    }

    pub async fn stream(
        &self,
        request: &ProviderRequest,
    ) -> Result<ProviderEventStream, ProviderError> {
        if request.provider != self.info.provider_id || request.model != self.info.model {
            return Err(invalid_request(format!(
                "registry binding {}/{} does not match request {}/{}",
                self.info.provider_id, self.info.model, request.provider, request.model
            )));
        }
        stream_with_refresh(self.provider.as_ref(), self.auth.as_ref(), request).await
    }
}

pub struct ProviderRegistry {
    bindings: BTreeMap<(String, String), ProviderBinding>,
}

impl ProviderRegistry {
    pub fn binding(&self, provider: &str, model: &str) -> Option<&ProviderBinding> {
        self.bindings.get(&(provider.to_owned(), model.to_owned()))
    }

    pub fn model(&self, provider: &str, model: &str) -> Option<&ProviderModelInfo> {
        self.binding(provider, model)
            .map(ProviderBinding::model_info)
    }

    pub fn models(&self) -> Vec<&ProviderModelInfo> {
        self.bindings
            .values()
            .map(ProviderBinding::model_info)
            .collect()
    }

    /// Runtime integration seam: resolve the explicit provider/model pair,
    /// resolve its credential mode, perform one coordinated 401 refresh, and
    /// return the real provider event stream.
    pub async fn stream(
        &self,
        request: &ProviderRequest,
    ) -> Result<ProviderEventStream, ProviderError> {
        let binding = self
            .binding(&request.provider, &request.model)
            .ok_or_else(|| {
                invalid_request(format!(
                    "provider model {}/{} is not registered",
                    request.provider, request.model
                ))
            })?;
        binding.stream(request).await
    }
}

pub struct ProviderFactory {
    transport: Arc<dyn HttpTransport>,
    mycel_home: PathBuf,
    version: String,
    codex_source: Arc<dyn CodexStatusSource>,
    clock: Arc<dyn UnixClock>,
    google_application_credentials: Option<PathBuf>,
    #[cfg(test)]
    google_assertion_signer: Option<Arc<dyn AssertionSigner>>,
}

impl ProviderFactory {
    pub fn new(
        transport: Arc<dyn HttpTransport>,
        mycel_home: PathBuf,
        version: impl Into<String>,
    ) -> Self {
        Self {
            transport,
            mycel_home,
            version: version.into(),
            codex_source: Arc::new(ProcessCodexStatusSource::new("codex")),
            clock: Arc::new(SystemUnixClock),
            google_application_credentials: None,
            #[cfg(test)]
            google_assertion_signer: None,
        }
    }

    pub fn with_codex_source(mut self, source: Arc<dyn CodexStatusSource>) -> Self {
        self.codex_source = source;
        self
    }

    pub fn with_clock(mut self, clock: Arc<dyn UnixClock>) -> Self {
        self.clock = clock;
        self
    }

    /// Deterministic override for `GOOGLE_APPLICATION_CREDENTIALS`. The CLI
    /// normally leaves this unset so the standard environment variable is
    /// resolved by the service-account credential source.
    pub fn with_google_application_credentials(mut self, path: PathBuf) -> Self {
        self.google_application_credentials = Some(path);
        self
    }

    #[cfg(test)]
    fn with_google_assertion_signer(mut self, signer: Arc<dyn AssertionSigner>) -> Self {
        self.google_assertion_signer = Some(signer);
        self
    }

    pub async fn build(
        &self,
        config: ProviderRegistryConfig,
    ) -> Result<ProviderRegistry, ProviderError> {
        if config.providers.is_empty() {
            return Err(invalid_request("provider registry is empty"));
        }
        let mut provider_ids = BTreeMap::<String, ()>::new();
        for provider in &config.providers {
            validate_provider_id(&provider.id)?;
            validate_headers(&provider.headers)?;
            validate_provider_config(provider)?;
            if provider_ids.insert(provider.id.clone(), ()).is_some() {
                return Err(invalid_request(format!(
                    "duplicate provider id {:?}",
                    provider.id
                )));
            }
        }
        let mut bindings = BTreeMap::new();
        for provider in config.providers {
            match &provider.adapter {
                ProviderAdapterConfig::ManagedKimi { .. } => {
                    self.register_managed_kimi(&mut bindings, provider).await?
                }
                _ => self.register_static(&mut bindings, provider)?,
            }
        }
        Ok(ProviderRegistry { bindings })
    }

    fn register_static(
        &self,
        bindings: &mut BTreeMap<(String, String), ProviderBinding>,
        config: ProviderConfig,
    ) -> Result<(), ProviderError> {
        if config.models.is_empty() {
            return Err(invalid_request(format!(
                "provider {:?} has no configured models",
                config.id
            )));
        }
        validate_adapter(&config.adapter)?;
        let auth: Arc<dyn RequestAuthProvider> = match (&config.adapter, &config.credential) {
            (
                ProviderAdapterConfig::CodexSubscription,
                ProviderCredentialConfig::CodexSubscription,
            ) => Arc::new(CodexSubscriptionAuth::new(self.codex_source.clone())),
            (ProviderAdapterConfig::CodexSubscription, _) => {
                return Err(invalid_request(
                    "Codex subscription adapter requires codex_subscription credentials",
                ))
            }
            (
                ProviderAdapterConfig::VertexServiceAccount { .. },
                ProviderCredentialConfig::GoogleServiceAccount(source),
            ) => {
                let credentials = source.resolve(self.google_application_credentials.as_deref())?;
                #[cfg(test)]
                let provider = if let Some(signer) = &self.google_assertion_signer {
                    GoogleServiceAccountTokenProvider::new_with_signer(
                        credentials,
                        self.transport.clone(),
                        self.clock.clone(),
                        signer.clone(),
                    )?
                } else {
                    GoogleServiceAccountTokenProvider::new(
                        credentials,
                        self.transport.clone(),
                        self.clock.clone(),
                    )?
                };
                #[cfg(not(test))]
                let provider = GoogleServiceAccountTokenProvider::new(
                    credentials,
                    self.transport.clone(),
                    self.clock.clone(),
                )?;
                Arc::new(provider)
            }
            (ProviderAdapterConfig::VertexServiceAccount { .. }, _) => {
                return Err(invalid_request(
                    "Vertex service-account adapter requires google_service_account credentials",
                ))
            }
            (_, ProviderCredentialConfig::ApiKey(credentials)) => {
                Arc::new(StaticAuthProvider::new(credentials)?)
            }
            (_, _) => {
                return Err(invalid_request(format!(
                    "provider {:?} requires static API-key credentials",
                    config.id
                )))
            }
        };
        let family = provider_family(&config.adapter);
        for model in config.models {
            validate_model_id(&model.id)?;
            let info = ProviderModelInfo {
                provider_id: config.id.clone(),
                model: model.id.clone(),
                display_name: model.display_name,
                capability: model
                    .capability
                    .unwrap_or_else(|| detect_capability(family, &model.id)),
                always_thinking: false,
                thinking_efforts: Vec::new(),
                default_thinking_effort: None,
                wire_protocol: wire_protocol(&config.adapter),
            };
            let provider = build_provider(
                &config.adapter,
                &model.id,
                config.headers.clone(),
                self.transport.clone(),
            )?;
            insert_binding(
                bindings,
                ProviderBinding {
                    info,
                    provider,
                    auth: auth.clone(),
                },
            )?;
        }
        Ok(())
    }

    async fn register_managed_kimi(
        &self,
        bindings: &mut BTreeMap<(String, String), ProviderBinding>,
        config: ProviderConfig,
    ) -> Result<(), ProviderError> {
        let ProviderAdapterConfig::ManagedKimi {
            oauth_host,
            api_base_url,
            client_id,
        } = &config.adapter
        else {
            unreachable!("caller selected managed Kimi")
        };
        if !matches!(config.credential, ProviderCredentialConfig::ManagedKimi) {
            return Err(invalid_request(
                "managed Kimi adapter requires managed_kimi credentials",
            ));
        }
        if !config.models.is_empty() {
            return Err(invalid_request(
                "managed Kimi models must come from the authenticated model catalog",
            ));
        }
        validate_base_url("Kimi OAuth host", oauth_host)?;
        validate_base_url("Kimi API base URL", api_base_url)?;
        if client_id.trim().is_empty() {
            return Err(invalid_request("Kimi OAuth client id is empty"));
        }
        let identity = KimiIdentity::load(&self.mycel_home, &self.version)?;
        let oauth = KimiOAuthClient::new(
            KimiOAuthConfig {
                oauth_host: oauth_host.clone(),
                client_id: client_id.clone(),
                api_base_url: api_base_url.clone(),
                storage_name: "kimi-code".into(),
            },
            identity.clone(),
            self.transport.clone(),
        );
        let token_provider = Arc::new(KimiTokenProvider::new(
            oauth.clone(),
            CredentialStore::mycel_home(&self.mycel_home),
            self.mycel_home.join("credentials/locks"),
        ));
        let mut discovery_auth = token_provider.request_auth(false).await?;
        for (name, value) in &config.headers {
            discovery_auth
                .headers
                .insert(name.clone(), SecretString::new(value.clone()));
        }
        let models = discover_kimi_models(&oauth, &discovery_auth).await?;
        if models.is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::MalformedResponse,
                "managed Kimi model catalog is empty",
            ));
        }
        let auth: Arc<dyn RequestAuthProvider> = token_provider;
        for model in models {
            validate_model_id(&model.id)?;
            let anthropic = model.protocol.as_deref() == Some("anthropic");
            let wire_protocol = if anthropic {
                ProviderWireProtocol::AnthropicMessages
            } else {
                ProviderWireProtocol::KimiChatCompletions
            };
            let efforts = model.valid_thinking_efforts().to_vec();
            let default_effort = model
                .think_efforts
                .as_ref()
                .filter(|value| value.support)
                .and_then(|value| value.default_effort.clone());
            let info = ProviderModelInfo {
                provider_id: config.id.clone(),
                model: model.id.clone(),
                display_name: model.display_name.clone(),
                capability: model.capability(),
                always_thinking: model.always_thinking(),
                thinking_efforts: efforts,
                default_thinking_effort: default_effort,
                wire_protocol,
            };
            let provider: Arc<dyn ChatProvider> = if anthropic {
                let mut headers = identity.headers.clone();
                headers.extend(config.headers.clone());
                Arc::new(AnthropicProvider::kimi_protocol(
                    model.id.clone(),
                    api_base_url.clone(),
                    headers,
                    self.transport.clone(),
                ))
            } else {
                Arc::new(KimiProvider::managed(
                    model.id.clone(),
                    Some(api_base_url.clone()),
                    &identity.headers,
                    config.headers.clone(),
                    self.transport.clone(),
                ))
            };
            insert_binding(
                bindings,
                ProviderBinding {
                    info,
                    provider,
                    auth: auth.clone(),
                },
            )?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct StaticAuthProvider {
    auth: ProviderRequestAuth,
}

impl StaticAuthProvider {
    fn new(config: &ApiKeyCredentialConfig) -> Result<Self, ProviderError> {
        validate_secret_headers(&config.headers)?;
        let api_key = match (&config.configured, &config.environment) {
            (Some(value), _) if value.is_empty() => {
                return Err(invalid_request("configured provider API key is empty"))
            }
            (Some(value), _) => value.clone(),
            (None, Some(value)) if value.is_empty() => {
                return Err(invalid_request("environment provider API key is empty"))
            }
            (None, Some(value)) => value.clone(),
            (None, None) => return Err(invalid_request("provider API key is missing")),
        };
        Ok(Self {
            auth: ProviderRequestAuth {
                api_key: Some(api_key),
                headers: config.headers.clone(),
            },
        })
    }
}

impl RequestAuthProvider for StaticAuthProvider {
    fn request_auth<'a>(&'a self, _force_refresh: bool) -> AuthFuture<'a> {
        Box::pin(async move { Ok(self.auth.clone()) })
    }
}

fn build_provider(
    adapter: &ProviderAdapterConfig,
    model: &str,
    headers: BTreeMap<String, String>,
    transport: Arc<dyn HttpTransport>,
) -> Result<Arc<dyn ChatProvider>, ProviderError> {
    Ok(match adapter {
        ProviderAdapterConfig::Anthropic {
            base_url,
            beta_api,
            beta_features,
            adaptive_thinking,
        } => Arc::new(
            AnthropicProvider::new(model, base_url.clone(), headers, transport)
                .with_beta_api(*beta_api)
                .with_beta_features(beta_features.clone())
                .with_adaptive_thinking(*adaptive_thinking),
        ),
        ProviderAdapterConfig::OpenAiChat { base_url } => Arc::new(OpenAiChatProvider::new(
            model,
            base_url.clone(),
            headers,
            transport,
        )),
        ProviderAdapterConfig::OpenAiResponses { base_url } => Arc::new(
            OpenAiResponsesProvider::new(model, base_url.clone(), headers, transport)?,
        ),
        ProviderAdapterConfig::Kimi { base_url } => Arc::new(KimiProvider::new(
            model,
            base_url.clone(),
            headers,
            transport,
        )),
        ProviderAdapterConfig::Gemini { base_url } => Arc::new(GoogleProvider::new(
            model,
            GoogleEndpoint::Gemini,
            base_url.clone(),
            headers,
            transport,
        )),
        ProviderAdapterConfig::VertexApiKey { base_url } => Arc::new(GoogleProvider::new(
            model,
            GoogleEndpoint::VertexApiKey,
            base_url.clone(),
            headers,
            transport,
        )),
        ProviderAdapterConfig::VertexServiceAccount {
            base_url,
            project,
            location,
        } => Arc::new(GoogleProvider::new(
            model,
            GoogleEndpoint::VertexServiceAccount {
                project: project.clone(),
                location: location.clone(),
            },
            base_url.clone(),
            headers,
            transport,
        )),
        ProviderAdapterConfig::CodexSubscription => Arc::new(OpenAiResponsesProvider::new(
            model,
            Some(CODEX_SUBSCRIPTION_BASE_URL.into()),
            headers,
            transport,
        )?),
        ProviderAdapterConfig::ManagedKimi { .. } => {
            return Err(invalid_request(
                "managed Kimi must be constructed from model discovery",
            ))
        }
    })
}

fn validate_provider_config(config: &ProviderConfig) -> Result<(), ProviderError> {
    match &config.adapter {
        ProviderAdapterConfig::ManagedKimi {
            oauth_host,
            api_base_url,
            client_id,
        } => {
            validate_base_url("Kimi OAuth host", oauth_host)?;
            validate_base_url("Kimi API base URL", api_base_url)?;
            if client_id.trim().is_empty() {
                return Err(invalid_request("Kimi OAuth client id is empty"));
            }
            if !matches!(&config.credential, ProviderCredentialConfig::ManagedKimi) {
                return Err(invalid_request(
                    "managed Kimi adapter requires managed_kimi credentials",
                ));
            }
            if !config.models.is_empty() {
                return Err(invalid_request(
                    "managed Kimi models must come from the authenticated model catalog",
                ));
            }
            return Ok(());
        }
        _ => validate_adapter(&config.adapter)?,
    }
    if config.models.is_empty() {
        return Err(invalid_request(format!(
            "provider {:?} has no configured models",
            config.id
        )));
    }
    for model in &config.models {
        validate_model_id(&model.id)?;
    }
    match (&config.adapter, &config.credential) {
        (ProviderAdapterConfig::CodexSubscription, ProviderCredentialConfig::CodexSubscription) => {
            Ok(())
        }
        (ProviderAdapterConfig::CodexSubscription, _) => Err(invalid_request(
            "Codex subscription adapter requires codex_subscription credentials",
        )),
        (
            ProviderAdapterConfig::VertexServiceAccount { .. },
            ProviderCredentialConfig::GoogleServiceAccount(_),
        ) => Ok(()),
        (ProviderAdapterConfig::VertexServiceAccount { .. }, _) => Err(invalid_request(
            "Vertex service-account adapter requires google_service_account credentials",
        )),
        (_, ProviderCredentialConfig::ApiKey(credentials)) => {
            StaticAuthProvider::new(credentials).map(|_| ())
        }
        (_, _) => Err(invalid_request(format!(
            "provider {:?} requires static API-key credentials",
            config.id
        ))),
    }
}

fn validate_adapter(adapter: &ProviderAdapterConfig) -> Result<(), ProviderError> {
    match adapter {
        ProviderAdapterConfig::Anthropic { base_url, .. }
        | ProviderAdapterConfig::OpenAiChat { base_url }
        | ProviderAdapterConfig::OpenAiResponses { base_url }
        | ProviderAdapterConfig::Kimi { base_url }
        | ProviderAdapterConfig::Gemini { base_url }
        | ProviderAdapterConfig::VertexApiKey { base_url } => {
            if let Some(base_url) = base_url {
                validate_base_url("provider base URL", base_url)?;
            }
        }
        ProviderAdapterConfig::VertexServiceAccount {
            base_url,
            project,
            location,
        } => {
            if let Some(base_url) = base_url {
                validate_base_url("Vertex base URL", base_url)?;
            }
            validate_path_segment("Vertex project", project)?;
            validate_path_segment("Vertex location", location)?;
        }
        ProviderAdapterConfig::CodexSubscription => {}
        ProviderAdapterConfig::ManagedKimi { .. } => {
            return Err(invalid_request(
                "managed Kimi must be registered through discovery",
            ))
        }
    }
    Ok(())
}

fn validate_base_url(label: &str, value: &str) -> Result<(), ProviderError> {
    let url =
        Url::parse(value).map_err(|error| invalid_request(format!("invalid {label}: {error}")))?;
    let local_http =
        url.scheme() == "http" && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if url.scheme() != "https" && !local_http {
        return Err(invalid_request(format!(
            "{label} must use HTTPS or loopback HTTP"
        )));
    }
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid_request(format!(
            "{label} must not contain credentials, a query, or a fragment"
        )));
    }
    Ok(())
}

fn validate_provider_id(value: &str) -> Result<(), ProviderError> {
    if value.is_empty()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control() || byte == b'/')
    {
        return Err(invalid_request(format!("invalid provider id {value:?}")));
    }
    Ok(())
}

fn validate_model_id(value: &str) -> Result<(), ProviderError> {
    if value.is_empty()
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/' | b'@')
        })
    {
        return Err(invalid_request(format!("invalid model id {value:?}")));
    }
    Ok(())
}

fn validate_path_segment(label: &str, value: &str) -> Result<(), ProviderError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(invalid_request(format!("invalid {label} {value:?}")));
    }
    Ok(())
}

fn validate_headers(headers: &BTreeMap<String, String>) -> Result<(), ProviderError> {
    for (name, value) in headers {
        HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            invalid_request(format!("invalid provider header {name:?}: {error}"))
        })?;
        HeaderValue::from_str(value).map_err(|error| {
            invalid_request(format!(
                "invalid value for provider header {name:?}: {error}"
            ))
        })?;
    }
    Ok(())
}

fn validate_secret_headers(headers: &BTreeMap<String, SecretString>) -> Result<(), ProviderError> {
    for (name, value) in headers {
        HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| invalid_request(format!("invalid auth header {name:?}: {error}")))?;
        HeaderValue::from_str(value.expose()).map_err(|error| {
            invalid_request(format!("invalid value for auth header {name:?}: {error}"))
        })?;
    }
    Ok(())
}

fn provider_family(adapter: &ProviderAdapterConfig) -> ProviderFamily {
    match adapter {
        ProviderAdapterConfig::Anthropic { .. } => ProviderFamily::Anthropic,
        ProviderAdapterConfig::OpenAiChat { .. } => ProviderFamily::OpenAiChat,
        ProviderAdapterConfig::OpenAiResponses { .. }
        | ProviderAdapterConfig::CodexSubscription => ProviderFamily::OpenAiResponses,
        ProviderAdapterConfig::Kimi { .. } => ProviderFamily::Kimi,
        ProviderAdapterConfig::Gemini { .. } => ProviderFamily::Gemini,
        ProviderAdapterConfig::VertexApiKey { .. }
        | ProviderAdapterConfig::VertexServiceAccount { .. } => ProviderFamily::Vertex,
        ProviderAdapterConfig::ManagedKimi { .. } => ProviderFamily::Kimi,
    }
}

fn wire_protocol(adapter: &ProviderAdapterConfig) -> ProviderWireProtocol {
    match adapter {
        ProviderAdapterConfig::Anthropic { .. } => ProviderWireProtocol::AnthropicMessages,
        ProviderAdapterConfig::OpenAiChat { .. } => ProviderWireProtocol::OpenAiChatCompletions,
        ProviderAdapterConfig::OpenAiResponses { .. }
        | ProviderAdapterConfig::CodexSubscription => ProviderWireProtocol::OpenAiResponses,
        ProviderAdapterConfig::Kimi { .. } => ProviderWireProtocol::KimiChatCompletions,
        ProviderAdapterConfig::Gemini { .. }
        | ProviderAdapterConfig::VertexApiKey { .. }
        | ProviderAdapterConfig::VertexServiceAccount { .. } => {
            ProviderWireProtocol::GoogleGenerateContent
        }
        ProviderAdapterConfig::ManagedKimi { .. } => ProviderWireProtocol::KimiChatCompletions,
    }
}

fn insert_binding(
    bindings: &mut BTreeMap<(String, String), ProviderBinding>,
    binding: ProviderBinding,
) -> Result<(), ProviderError> {
    let key = (binding.info.provider_id.clone(), binding.info.model.clone());
    if bindings.insert(key.clone(), binding).is_some() {
        return Err(invalid_request(format!(
            "duplicate provider model {}/{}",
            key.0, key.1
        )));
    }
    Ok(())
}

pub fn managed_kimi_defaults() -> ProviderAdapterConfig {
    ProviderAdapterConfig::ManagedKimi {
        oauth_host: crate::auth::KIMI_OAUTH_HOST.into(),
        api_base_url: crate::auth::KIMI_MANAGED_BASE_URL.into(),
        client_id: KIMI_CLIENT_ID.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        sync::{Arc, Mutex},
    };

    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use bytes::Bytes;
    use futures_util::{stream, FutureExt, StreamExt};
    use mycel_agent_protocol::{Message, ProviderErrorKind, ProviderRequest, StreamAssembler};
    use tempfile::TempDir;

    use super::*;
    use crate::{
        auth::{
            CodexAuthStatus, CodexStatusFuture, CodexVersionFuture, KIMI_MANAGED_BASE_URL,
            KIMI_OAUTH_HOST,
        },
        http::{ByteStream, HttpRequest, HttpResponse, TransportFuture},
    };

    type FakeResponse = (u16, BTreeMap<String, String>, Vec<Vec<u8>>);

    #[derive(Default)]
    struct FakeTransport {
        requests: Mutex<Vec<HttpRequest>>,
        responses: Mutex<VecDeque<FakeResponse>>,
    }

    impl FakeTransport {
        fn response(&self, status: u16, headers: BTreeMap<String, String>, chunks: &[&str]) {
            self.responses.lock().expect("responses").push_back((
                status,
                headers,
                chunks
                    .iter()
                    .map(|chunk| chunk.as_bytes().to_vec())
                    .collect(),
            ));
        }

        fn last_request(&self) -> HttpRequest {
            self.requests
                .lock()
                .expect("requests")
                .last()
                .expect("request")
                .clone()
        }
    }

    impl HttpTransport for FakeTransport {
        fn send<'a>(&'a self, request: HttpRequest) -> TransportFuture<'a> {
            self.requests.lock().expect("requests").push(request);
            let response = self
                .responses
                .lock()
                .expect("responses")
                .pop_front()
                .expect("fixture response");
            async move {
                let body: ByteStream = Box::pin(stream::iter(
                    response.2.into_iter().map(|chunk| Ok(Bytes::from(chunk))),
                ));
                Ok(HttpResponse {
                    status: response.0,
                    headers: response.1,
                    body,
                })
            }
            .boxed()
        }
    }

    #[derive(Clone)]
    struct FakeCodexSource {
        status: CodexAuthStatus,
    }

    impl CodexStatusSource for FakeCodexSource {
        fn read<'a>(&'a self, _force_refresh: bool) -> CodexStatusFuture<'a> {
            async move { Ok(self.status.clone()) }.boxed()
        }

        fn version<'a>(&'a self) -> CodexVersionFuture<'a> {
            async move { Ok("1.2.3".into()) }.boxed()
        }
    }

    fn codex_source() -> Arc<dyn CodexStatusSource> {
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&serde_json::json!({
                "https://api.openai.com/auth":{"chatgpt_account_id":"acct"},
                "exp":4_102_444_800_u64
            }))
            .expect("claims"),
        );
        Arc::new(FakeCodexSource {
            status: CodexAuthStatus {
                auth_method: Some("chatgpt".into()),
                auth_token: Some(format!("x.{payload}.y")),
                requires_openai_auth: Some(true),
            },
        })
    }

    fn api_key(value: &str) -> ProviderCredentialConfig {
        ProviderCredentialConfig::ApiKey(ApiKeyCredentialConfig::configured(value))
    }

    #[derive(Debug)]
    struct FakeGoogleSigner;

    impl AssertionSigner for FakeGoogleSigner {
        fn validate(&self, _private_key: &SecretString) -> Result<(), ProviderError> {
            Ok(())
        }

        fn sign(
            &self,
            _private_key: &SecretString,
            _message: &[u8],
        ) -> Result<Vec<u8>, ProviderError> {
            Ok(b"registry-signature".to_vec())
        }
    }

    fn write_service_account(path: &std::path::Path) {
        let pem = concat!(
            "-----BEGIN ",
            "PRIVATE KEY-----\n",
            "dGVzdA==\n",
            "-----END ",
            "PRIVATE KEY-----"
        );
        fs::write(
            path,
            serde_json::to_vec(&serde_json::json!({
                "type":"service_account",
                "private_key_id":"registry-key",
                "private_key":pem,
                "client_email":"mycel-test@example.iam.gserviceaccount.com",
                "token_uri":crate::google_auth::GOOGLE_OAUTH_TOKEN_URI
            }))
            .expect("credential JSON"),
        )
        .expect("credential file");
    }

    fn model(id: &str) -> Vec<ProviderModelConfig> {
        vec![ProviderModelConfig::new(id)]
    }

    fn request(provider: &str, model: &str) -> ProviderRequest {
        ProviderRequest {
            provider: provider.into(),
            model: model.into(),
            system_prompt: "system".into(),
            tools: Vec::new(),
            history: vec![Message::user("hello")],
            thinking_effort: None,
            max_completion_tokens: Some(32),
            response_format: None,
            metadata: BTreeMap::new(),
        }
    }

    async fn assembled_text(registry: &ProviderRegistry, provider: &str, model: &str) -> String {
        let mut events = registry
            .stream(&request(provider, model))
            .await
            .expect("provider stream");
        let mut assembler = StreamAssembler::default();
        while let Some(event) = events.next().await {
            assembler
                .push(event.expect("provider event"))
                .expect("valid stream event");
        }
        assembler
            .finish()
            .expect("assembled response")
            .message
            .text("")
    }

    fn chat_response() -> [&'static str; 2] {
        [
            "data: {\"id\":\"chat\",\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ]
    }

    fn responses_response() -> [&'static str; 3] {
        [
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
        ]
    }

    fn anthropic_response() -> [&'static str; 3] {
        [
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"a\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        ]
    }

    fn google_response() -> [&'static str; 1] {
        ["data: {\"responseId\":\"g\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}\n\n"]
    }

    #[tokio::test]
    async fn registry_constructs_and_streams_every_static_provider_kind() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(FakeTransport::default());
        let google_credentials = temp.path().join("google-service-account.json");
        write_service_account(&google_credentials);
        let configs = vec![
            ProviderConfig {
                id: "anthropic".into(),
                adapter: ProviderAdapterConfig::Anthropic {
                    base_url: Some("http://127.0.0.1/anthropic".into()),
                    beta_api: false,
                    beta_features: vec!["interleaved-thinking-2025-05-14".into()],
                    adaptive_thinking: Some(false),
                },
                credential: api_key("anthropic-key"),
                headers: BTreeMap::new(),
                models: model("claude-sonnet-4"),
            },
            ProviderConfig {
                id: "chat".into(),
                adapter: ProviderAdapterConfig::OpenAiChat {
                    base_url: Some("http://127.0.0.1/openai/v1".into()),
                },
                credential: ProviderCredentialConfig::ApiKey(ApiKeyCredentialConfig {
                    configured: Some(SecretString::new("configured-key")),
                    environment: Some(SecretString::new("environment-key")),
                    headers: BTreeMap::from([(
                        "x-credential-origin".into(),
                        SecretString::new("secret"),
                    )]),
                }),
                headers: BTreeMap::from([
                    ("AUTHORIZATION".into(), "Bearer plain-one".into()),
                    ("Authorization".into(), "Bearer plain-two".into()),
                    ("X-Credential-Origin".into(), "plain".into()),
                ]),
                models: model("gpt-4.1"),
            },
            ProviderConfig {
                id: "responses".into(),
                adapter: ProviderAdapterConfig::OpenAiResponses {
                    base_url: Some("http://127.0.0.1/responses/v1".into()),
                },
                credential: api_key("responses-key"),
                headers: BTreeMap::new(),
                models: model("gpt-4.1"),
            },
            ProviderConfig {
                id: "kimi".into(),
                adapter: ProviderAdapterConfig::Kimi {
                    base_url: Some("http://127.0.0.1/kimi/v1".into()),
                },
                credential: api_key("kimi-key"),
                headers: BTreeMap::new(),
                models: model("kimi-k2"),
            },
            ProviderConfig {
                id: "gemini".into(),
                adapter: ProviderAdapterConfig::Gemini {
                    base_url: Some("http://127.0.0.1/gemini".into()),
                },
                credential: api_key("gemini-key"),
                headers: BTreeMap::new(),
                models: model("gemini-2.5-pro"),
            },
            ProviderConfig {
                id: "vertex-key".into(),
                adapter: ProviderAdapterConfig::VertexApiKey {
                    base_url: Some("http://127.0.0.1/vertex-key".into()),
                },
                credential: api_key("vertex-key"),
                headers: BTreeMap::new(),
                models: model("gemini-2.5-pro"),
            },
            ProviderConfig {
                id: "vertex-service".into(),
                adapter: ProviderAdapterConfig::VertexServiceAccount {
                    base_url: Some("http://127.0.0.1/vertex-service".into()),
                    project: "project-1".into(),
                    location: "us-central1".into(),
                },
                credential: ProviderCredentialConfig::GoogleServiceAccount(
                    GoogleServiceAccountCredentialSource::ApplicationDefault,
                ),
                headers: BTreeMap::new(),
                models: model("gemini-2.5-pro"),
            },
            ProviderConfig {
                id: "codex".into(),
                adapter: ProviderAdapterConfig::CodexSubscription,
                credential: ProviderCredentialConfig::CodexSubscription,
                headers: BTreeMap::new(),
                models: model("gpt-4.1"),
            },
        ];
        let registry = ProviderFactory::new(transport.clone(), temp.path().into(), "1.0")
            .with_codex_source(codex_source())
            .with_google_application_credentials(google_credentials)
            .with_google_assertion_signer(Arc::new(FakeGoogleSigner))
            .build(ProviderRegistryConfig { providers: configs })
            .await
            .expect("registry");

        let cases = [
            (
                "anthropic",
                "claude-sonnet-4",
                "http://127.0.0.1/anthropic/v1/messages",
                "x-api-key",
                "anthropic-key",
                anthropic_response().to_vec(),
            ),
            (
                "chat",
                "gpt-4.1",
                "http://127.0.0.1/openai/v1/chat/completions",
                "authorization",
                "Bearer configured-key",
                chat_response().to_vec(),
            ),
            (
                "responses",
                "gpt-4.1",
                "http://127.0.0.1/responses/v1/responses",
                "authorization",
                "Bearer responses-key",
                responses_response().to_vec(),
            ),
            (
                "kimi",
                "kimi-k2",
                "http://127.0.0.1/kimi/v1/chat/completions",
                "authorization",
                "Bearer kimi-key",
                chat_response().to_vec(),
            ),
            (
                "gemini",
                "gemini-2.5-pro",
                "http://127.0.0.1/gemini/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
                "x-goog-api-key",
                "gemini-key",
                google_response().to_vec(),
            ),
            (
                "vertex-key",
                "gemini-2.5-pro",
                "http://127.0.0.1/vertex-key/v1beta1/publishers/google/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
                "x-goog-api-key",
                "vertex-key",
                google_response().to_vec(),
            ),
            (
                "vertex-service",
                "gemini-2.5-pro",
                "http://127.0.0.1/vertex-service/v1beta1/projects/project-1/locations/us-central1/publishers/google/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
                "authorization",
                "Bearer service-token",
                google_response().to_vec(),
            ),
            (
                "codex",
                "gpt-4.1",
                "https://chatgpt.com/backend-api/codex/responses",
                "ChatGPT-Account-ID",
                "acct",
                responses_response().to_vec(),
            ),
        ];

        for (provider, model, url, auth_header, auth_value, response) in cases {
            if provider == "vertex-service" {
                transport.response(
                    200,
                    BTreeMap::new(),
                    &[r#"{"access_token":"service-token","expires_in":3600,"token_type":"Bearer"}"#],
                );
            }
            transport.response(200, BTreeMap::new(), &response);
            assert_eq!(assembled_text(&registry, provider, model).await, "ok");
            let sent = transport.last_request();
            assert_eq!(sent.url, url);
            assert_eq!(sent.headers[auth_header], auth_value);
            if provider == "chat" {
                assert_eq!(sent.headers["x-credential-origin"], "secret");
                assert!(!sent.headers.contains_key("X-Credential-Origin"));
                assert!(!sent.headers.contains_key("AUTHORIZATION"));
                assert!(!sent.headers.contains_key("Authorization"));
            }
        }

        assert_eq!(registry.models().len(), 8);
        let openai = registry.model("chat", "gpt-4.1").expect("model lookup");
        assert!(openai.capability.image_in && openai.capability.tool_use);
        assert_eq!(
            openai.wire_protocol,
            ProviderWireProtocol::OpenAiChatCompletions
        );
    }

    fn write_managed_token(home: &std::path::Path) {
        let directory = home.join("credentials");
        fs::create_dir_all(&directory).expect("credentials directory");
        fs::write(
            directory.join("kimi-code.json"),
            serde_json::to_vec(&serde_json::json!({
                "access_token":"managed-access",
                "refresh_token":"managed-refresh",
                "expires_at":4_102_444_800_u64,
                "expires_in":3600,
                "scope":"",
                "token_type":"Bearer"
            }))
            .expect("token"),
        )
        .expect("credential fixture");
    }

    #[tokio::test]
    async fn managed_kimi_discovers_models_and_selects_each_declared_wire() {
        let temp = TempDir::new().expect("temp");
        write_managed_token(temp.path());
        let transport = Arc::new(FakeTransport::default());
        transport.response(
            200,
            BTreeMap::new(),
            &[r#"{"data":[
                {"id":"kimi-chat","display_name":"Kimi Chat","context_length":131072,"supports_reasoning":true,"supports_tool_use":true,"supports_thinking_type":"both"},
                {"id":"kimi-anthropic","context_length":262144,"protocol":"anthropic","supports_tool_use":true,"supports_thinking_type":"only","think_efforts":{"support":true,"valid_efforts":["low","high"],"default_effort":"high"}}
            ]}"#],
        );
        let registry = ProviderFactory::new(transport.clone(), temp.path().into(), "1.0")
            .with_codex_source(codex_source())
            .build(ProviderRegistryConfig {
                providers: vec![ProviderConfig {
                    id: "managed:kimi-code".into(),
                    adapter: ProviderAdapterConfig::ManagedKimi {
                        oauth_host: KIMI_OAUTH_HOST.into(),
                        api_base_url: KIMI_MANAGED_BASE_URL.into(),
                        client_id: KIMI_CLIENT_ID.into(),
                    },
                    credential: ProviderCredentialConfig::ManagedKimi,
                    headers: BTreeMap::from([("x-managed-test".into(), "yes".into())]),
                    models: Vec::new(),
                }],
            })
            .await
            .expect("managed registry");

        let catalog_request = transport
            .requests
            .lock()
            .expect("requests")
            .first()
            .expect("catalog request")
            .clone();
        assert_eq!(
            catalog_request.url,
            format!("{KIMI_MANAGED_BASE_URL}/models")
        );
        assert_eq!(
            catalog_request.headers["authorization"],
            "Bearer managed-access"
        );
        assert_eq!(catalog_request.headers["x-managed-test"], "yes");

        let chat = registry
            .model("managed:kimi-code", "kimi-chat")
            .expect("chat model");
        assert_eq!(
            chat.wire_protocol,
            ProviderWireProtocol::KimiChatCompletions
        );
        assert!(chat.capability.thinking && !chat.always_thinking);
        let anthropic = registry
            .model("managed:kimi-code", "kimi-anthropic")
            .expect("Anthropic model");
        assert_eq!(
            anthropic.wire_protocol,
            ProviderWireProtocol::AnthropicMessages
        );
        assert!(anthropic.always_thinking);
        assert_eq!(anthropic.thinking_efforts, ["low", "high"]);
        assert_eq!(anthropic.default_thinking_effort.as_deref(), Some("high"));

        transport.response(200, BTreeMap::new(), &chat_response());
        assert_eq!(
            assembled_text(&registry, "managed:kimi-code", "kimi-chat").await,
            "ok"
        );
        let sent = transport.last_request();
        assert_eq!(
            sent.url,
            format!("{KIMI_MANAGED_BASE_URL}/chat/completions")
        );
        assert_eq!(sent.headers["authorization"], "Bearer managed-access");

        transport.response(200, BTreeMap::new(), &anthropic_response());
        assert_eq!(
            assembled_text(&registry, "managed:kimi-code", "kimi-anthropic").await,
            "ok"
        );
        let sent = transport.last_request();
        assert_eq!(
            sent.url,
            "https://api.kimi.com/coding/v1/messages?beta=true"
        );
        assert_eq!(sent.headers["x-api-key"], "managed-access");
    }

    fn one_provider(adapter: ProviderAdapterConfig) -> ProviderRegistryConfig {
        ProviderRegistryConfig {
            providers: vec![ProviderConfig {
                id: "test".into(),
                adapter,
                credential: api_key("key"),
                headers: BTreeMap::new(),
                models: model("gpt-4.1"),
            }],
        }
    }

    async fn expect_build_error(
        factory: &ProviderFactory,
        config: ProviderRegistryConfig,
    ) -> ProviderError {
        match factory.build(config).await {
            Ok(_) => panic!("configuration must fail"),
            Err(error) => error,
        }
    }

    #[tokio::test]
    async fn registry_rejects_invalid_urls_credentials_duplicates_and_models() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(FakeTransport::default());
        let factory = ProviderFactory::new(transport.clone(), temp.path().into(), "1.0")
            .with_codex_source(codex_source());

        assert_eq!(
            expect_build_error(&factory, ProviderRegistryConfig::default())
                .await
                .kind,
            ProviderErrorKind::InvalidRequest
        );
        let insecure = one_provider(ProviderAdapterConfig::OpenAiChat {
            base_url: Some("http://example.com/v1".into()),
        });
        assert_eq!(
            expect_build_error(&factory, insecure).await.kind,
            ProviderErrorKind::InvalidRequest
        );
        let mut mismatched = one_provider(ProviderAdapterConfig::CodexSubscription);
        mismatched.providers[0].credential = api_key("wrong-mode");
        assert_eq!(
            expect_build_error(&factory, mismatched).await.kind,
            ProviderErrorKind::InvalidRequest
        );
        let mut empty_key = one_provider(ProviderAdapterConfig::OpenAiChat { base_url: None });
        empty_key.providers[0].credential =
            ProviderCredentialConfig::ApiKey(ApiKeyCredentialConfig::configured(String::new()));
        assert_eq!(
            expect_build_error(&factory, empty_key).await.kind,
            ProviderErrorKind::InvalidRequest
        );
        let mut no_models = one_provider(ProviderAdapterConfig::OpenAiChat { base_url: None });
        no_models.providers[0].models.clear();
        assert_eq!(
            expect_build_error(&factory, no_models).await.kind,
            ProviderErrorKind::InvalidRequest
        );
        let mut bad_model = one_provider(ProviderAdapterConfig::OpenAiChat { base_url: None });
        bad_model.providers[0].models[0].id = "gpt?injected=true".into();
        assert_eq!(
            expect_build_error(&factory, bad_model).await.kind,
            ProviderErrorKind::InvalidRequest
        );
        let mut bad_header = one_provider(ProviderAdapterConfig::OpenAiChat { base_url: None });
        bad_header.providers[0]
            .headers
            .insert("x-test".into(), "bad\r\nvalue".into());
        assert_eq!(
            expect_build_error(&factory, bad_header).await.kind,
            ProviderErrorKind::InvalidRequest
        );
        let mut duplicate = one_provider(ProviderAdapterConfig::OpenAiChat { base_url: None });
        duplicate.providers.push(duplicate.providers[0].clone());
        assert_eq!(
            expect_build_error(&factory, duplicate).await.kind,
            ProviderErrorKind::InvalidRequest
        );
        let bad_vertex = one_provider(ProviderAdapterConfig::VertexServiceAccount {
            base_url: None,
            project: "bad/project".into(),
            location: "us-central1".into(),
        });
        assert_eq!(
            expect_build_error(&factory, bad_vertex).await.kind,
            ProviderErrorKind::InvalidRequest
        );
        let wrong_vertex_credential = one_provider(ProviderAdapterConfig::VertexServiceAccount {
            base_url: None,
            project: "project-1".into(),
            location: "us-central1".into(),
        });
        assert_eq!(
            expect_build_error(&factory, wrong_vertex_credential)
                .await
                .kind,
            ProviderErrorKind::InvalidRequest
        );
        let managed_with_static_model = ProviderRegistryConfig {
            providers: vec![ProviderConfig {
                id: "managed".into(),
                adapter: managed_kimi_defaults(),
                credential: ProviderCredentialConfig::ManagedKimi,
                headers: BTreeMap::new(),
                models: model("stale-model"),
            }],
        };
        assert_eq!(
            expect_build_error(&factory, managed_with_static_model)
                .await
                .kind,
            ProviderErrorKind::InvalidRequest
        );
        assert!(transport.requests.lock().expect("requests").is_empty());
    }

    #[tokio::test]
    async fn registry_model_lookup_and_request_binding_fail_closed() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(FakeTransport::default());
        let mut config = one_provider(ProviderAdapterConfig::OpenAiChat {
            base_url: Some("http://127.0.0.1/v1".into()),
        });
        config.providers[0].credential = ProviderCredentialConfig::ApiKey(ApiKeyCredentialConfig {
            configured: None,
            environment: Some(SecretString::new("environment-only")),
            headers: BTreeMap::new(),
        });
        let registry = ProviderFactory::new(transport.clone(), temp.path().into(), "1.0")
            .build(config)
            .await
            .expect("registry");
        assert!(registry.model("test", "gpt-4.1").is_some());
        assert!(registry.model("test", "missing").is_none());
        let error = match registry.stream(&request("test", "missing")).await {
            Ok(_) => panic!("missing model must not produce a stream"),
            Err(error) => error,
        };
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert!(transport.requests.lock().expect("requests").is_empty());

        let binding = registry.binding("test", "gpt-4.1").expect("binding");
        assert_eq!(
            binding
                .request_auth(false)
                .await
                .expect("environment auth")
                .api_key
                .expect("API key")
                .expose(),
            "environment-only"
        );
        let error = match binding.stream(&request("other", "gpt-4.1")).await {
            Ok(_) => panic!("mismatched provider must not produce a stream"),
            Err(error) => error,
        };
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert!(transport.requests.lock().expect("requests").is_empty());
    }
}
