use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    future::{pending, Future},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use mycel_agent_protocol::{ModelCapability, ProviderError, ProviderErrorKind, SecretString};
use reqwest::header::HeaderValue;
use serde_json::{Map, Value};
use tokio::sync::Notify;
use url::Url;
use zeroize::Zeroizing;

use crate::http::{collect_body, HttpRequest, HttpTransport};

const DEFAULT_MAX_BYTES: usize = 8 * 1_024 * 1_024;
const DEFAULT_MAX_PROVIDERS: usize = 2_048;
const DEFAULT_MAX_MODELS_PER_PROVIDER: usize = 8_192;
const DEFAULT_MAX_TOTAL_MODELS: usize = 100_000;
const CUSTOM_DEFAULT_CONTEXT: u64 = 131_072;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportWireFamily {
    Anthropic,
    OpenAiChat,
    OpenAiResponses,
    Kimi,
    Gemini,
    Vertex,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredModel {
    pub id: String,
    pub display_name: Option<String>,
    pub max_output_tokens: Option<u64>,
    pub capability: ModelCapability,
    pub reasoning_key: Option<String>,
    pub thinking_efforts: Vec<String>,
    pub default_thinking_effort: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredProvider {
    pub id: String,
    pub display_name: String,
    pub wire: Option<ImportWireFamily>,
    pub base_url: Option<String>,
    pub credential_environment: Vec<String>,
    pub raw_model_count: usize,
    pub models: Vec<DiscoveredModel>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderCatalogListItem {
    pub id: String,
    pub display_name: String,
    pub wire: Option<ImportWireFamily>,
    pub raw_model_count: usize,
    pub usable_model_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderImportPlan {
    pub source_url: String,
    pub id: String,
    pub display_name: String,
    pub wire: ImportWireFamily,
    pub base_url: Option<String>,
    pub api_key: Option<SecretString>,
    pub credential_environment: Vec<String>,
    pub models: Vec<DiscoveredModel>,
    pub selected_model: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomRegistryImportPlan {
    pub source_url: String,
    pub providers: Vec<ProviderImportPlan>,
}

#[derive(Clone, Debug)]
pub struct ModelsCatalog {
    source_url: String,
    providers: BTreeMap<String, DiscoveredProvider>,
}

impl ModelsCatalog {
    pub fn source_url(&self) -> &str {
        &self.source_url
    }

    pub fn list(&self, filter: Option<&str>) -> Vec<ProviderCatalogListItem> {
        let filter = filter.map(str::to_ascii_lowercase);
        self.providers
            .values()
            .filter(|provider| {
                filter.as_ref().is_none_or(|needle| {
                    provider.id.to_ascii_lowercase().contains(needle)
                        || provider.display_name.to_ascii_lowercase().contains(needle)
                })
            })
            .map(|provider| ProviderCatalogListItem {
                id: provider.id.clone(),
                display_name: provider.display_name.clone(),
                wire: provider.wire,
                raw_model_count: provider.raw_model_count,
                usable_model_count: provider.models.len(),
            })
            .collect()
    }

    pub fn detail(&self, provider_id: &str) -> Option<&DiscoveredProvider> {
        self.providers.get(provider_id)
    }

    pub fn plan_provider(
        &self,
        provider_id: &str,
        api_key: SecretString,
        selected_model: Option<&str>,
    ) -> Result<ProviderImportPlan, ProviderError> {
        if api_key.is_empty() {
            return Err(invalid_discovery("catalog import API key is empty"));
        }
        let provider = self.providers.get(provider_id).ok_or_else(|| {
            invalid_discovery(format!("catalog provider {provider_id:?} was not found"))
        })?;
        let wire = provider.wire.ok_or_else(|| {
            invalid_discovery(format!(
                "catalog provider {provider_id:?} uses an unsupported wire family"
            ))
        })?;
        if provider.models.is_empty() {
            return Err(invalid_discovery(format!(
                "catalog provider {provider_id:?} has no usable chat models"
            )));
        }
        if let Some(model) = selected_model {
            if !provider
                .models
                .iter()
                .any(|candidate| candidate.id == model)
            {
                return Err(invalid_discovery(format!(
                    "model {model:?} is not available from catalog provider {provider_id:?}"
                )));
            }
        }
        Ok(ProviderImportPlan {
            source_url: self.source_url.clone(),
            id: provider.id.clone(),
            display_name: provider.display_name.clone(),
            wire,
            base_url: provider.base_url.clone(),
            api_key: Some(api_key),
            credential_environment: provider.credential_environment.clone(),
            models: provider.models.clone(),
            selected_model: selected_model.map(str::to_owned),
        })
    }
}

#[derive(Clone, Debug)]
pub struct CatalogFetchRequest {
    pub url: String,
    pub user_agent: Option<String>,
    pub timeout: Duration,
    pub cancellation: Option<DiscoveryCancellationToken>,
}

impl CatalogFetchRequest {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            user_agent: None,
            timeout: Duration::from_secs(30),
            cancellation: None,
        }
    }
}

#[derive(Clone)]
pub struct CustomRegistryFetchRequest {
    pub url: String,
    pub api_key: Option<SecretString>,
    pub user_agent: Option<String>,
    pub timeout: Duration,
    pub cancellation: Option<DiscoveryCancellationToken>,
}

impl CustomRegistryFetchRequest {
    pub fn new(url: impl Into<String>, api_key: Option<SecretString>) -> Self {
        Self {
            url: url.into(),
            api_key,
            user_agent: None,
            timeout: Duration::from_secs(30),
            cancellation: None,
        }
    }
}

impl fmt::Debug for CustomRegistryFetchRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CustomRegistryFetchRequest")
            .field("url", &self.url)
            .field("api_key", &self.api_key)
            .field("user_agent", &self.user_agent)
            .field("timeout", &self.timeout)
            .field("cancellation", &self.cancellation)
            .finish()
    }
}

struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

#[derive(Clone)]
pub struct DiscoveryCancellationToken(Arc<CancellationState>);

impl Default for DiscoveryCancellationToken {
    fn default() -> Self {
        Self(Arc::new(CancellationState {
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
        }))
    }
}

impl fmt::Debug for DiscoveryCancellationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiscoveryCancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl DiscoveryCancellationToken {
    pub fn cancel(&self) {
        if !self.0.cancelled.swap(true, Ordering::AcqRel) {
            self.0.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DiscoveryLimits {
    pub max_response_bytes: usize,
    pub max_providers: usize,
    pub max_models_per_provider: usize,
    pub max_total_models: usize,
}

impl Default for DiscoveryLimits {
    fn default() -> Self {
        Self {
            max_response_bytes: DEFAULT_MAX_BYTES,
            max_providers: DEFAULT_MAX_PROVIDERS,
            max_models_per_provider: DEFAULT_MAX_MODELS_PER_PROVIDER,
            max_total_models: DEFAULT_MAX_TOTAL_MODELS,
        }
    }
}

pub struct ProviderDiscoveryService {
    transport: Arc<dyn HttpTransport>,
    limits: DiscoveryLimits,
}

impl ProviderDiscoveryService {
    /// The injected transport must not auto-follow redirects. Production CLI
    /// callers should construct it with `ReqwestTransport::without_redirects`.
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            transport,
            limits: DiscoveryLimits::default(),
        }
    }

    pub fn with_limits(
        transport: Arc<dyn HttpTransport>,
        limits: DiscoveryLimits,
    ) -> Result<Self, ProviderError> {
        if limits.max_response_bytes == 0
            || limits.max_providers == 0
            || limits.max_models_per_provider == 0
            || limits.max_total_models == 0
        {
            return Err(invalid_discovery("discovery limits must be positive"));
        }
        Ok(Self { transport, limits })
    }

    pub async fn fetch_models_catalog(
        &self,
        request: CatalogFetchRequest,
    ) -> Result<ModelsCatalog, ProviderError> {
        let url = validate_url("catalog URL", &request.url, true)?;
        let payload = self
            .fetch_json(
                &url,
                None,
                request.user_agent.as_deref(),
                request.timeout,
                request.cancellation.as_ref(),
            )
            .await?;
        let providers = self.parse_models_catalog(&payload)?;
        Ok(ModelsCatalog {
            source_url: normalized_url(&url),
            providers,
        })
    }

    pub async fn plan_custom_registry(
        &self,
        request: CustomRegistryFetchRequest,
    ) -> Result<CustomRegistryImportPlan, ProviderError> {
        let url = validate_url("custom registry URL", &request.url, true)?;
        let payload = self
            .fetch_json(
                &url,
                request.api_key.as_ref(),
                request.user_agent.as_deref(),
                request.timeout,
                request.cancellation.as_ref(),
            )
            .await?;
        let source_url = normalized_url(&url);
        let providers =
            self.parse_custom_registry(&payload, &source_url, request.api_key.as_ref())?;
        Ok(CustomRegistryImportPlan {
            source_url,
            providers,
        })
    }

    async fn fetch_json(
        &self,
        url: &Url,
        api_key: Option<&SecretString>,
        user_agent: Option<&str>,
        timeout: Duration,
        cancellation: Option<&DiscoveryCancellationToken>,
    ) -> Result<Zeroizing<Vec<u8>>, ProviderError> {
        if timeout.is_zero() {
            return Err(invalid_discovery(
                "catalog request timeout must be positive",
            ));
        }
        let mut headers = BTreeMap::from([("accept".into(), "application/json".into())]);
        if let Some(user_agent) = user_agent {
            HeaderValue::from_str(user_agent)
                .map_err(|_| invalid_discovery("catalog User-Agent is invalid"))?;
            headers.insert("user-agent".into(), user_agent.to_owned());
        }
        if let Some(api_key) = api_key.filter(|key| !key.is_empty()) {
            let authorization = format!("Bearer {}", api_key.expose());
            HeaderValue::from_str(&authorization)
                .map_err(|_| invalid_discovery("custom registry API key is invalid"))?;
            headers.insert("authorization".into(), authorization);
        }
        let send = self.transport.send(HttpRequest {
            method: "GET".into(),
            url: normalized_url(url),
            headers,
            body: Vec::new(),
            timeout,
        });
        let response = select_cancel(send, cancellation).await?.map_err(|error| {
            if error.timeout {
                ProviderError::new(ProviderErrorKind::Connection, "catalog request timed out")
            } else {
                ProviderError::new(ProviderErrorKind::Connection, "catalog request failed")
            }
        })?;
        if (300..400).contains(&response.status) {
            return Err(ProviderError {
                kind: ProviderErrorKind::InvalidRequest,
                message: "catalog redirects are not allowed".into(),
                retryable: false,
                status_code: Some(response.status),
                retry_after_ms: None,
            });
        }
        if !(200..300).contains(&response.status) {
            return Err(catalog_status_error(response.status));
        }
        let collect = collect_body(response.body, self.limits.max_response_bytes);
        let bytes = select_cancel(collect, cancellation)
            .await?
            .map_err(|error| {
                if error.kind == ProviderErrorKind::MalformedResponse {
                    malformed_discovery("catalog response exceeds the configured size limit")
                } else {
                    ProviderError::new(
                        ProviderErrorKind::Connection,
                        "catalog response body failed",
                    )
                }
            })?;
        Ok(Zeroizing::new(bytes))
    }

    fn parse_models_catalog(
        &self,
        bytes: &[u8],
    ) -> Result<BTreeMap<String, DiscoveredProvider>, ProviderError> {
        let root = parse_root(bytes, "models catalog")?;
        self.check_provider_count(root.len())?;
        let mut total_models = 0_usize;
        let mut providers = BTreeMap::new();
        for (provider_key, raw) in root {
            validate_provider_id(&provider_key)?;
            let object = raw.as_object().ok_or_else(|| {
                malformed_discovery(format!(
                    "catalog provider {provider_key:?} must be an object"
                ))
            })?;
            let declared_id = optional_string(object, "id");
            let wire = infer_wire(
                optional_string(object, "type"),
                optional_string(object, "npm"),
                declared_id.or(Some(&provider_key)),
            );
            let empty_models = Map::new();
            let models_object = optional_object(object, "models").unwrap_or(&empty_models);
            self.check_model_count(&provider_key, models_object.len())?;
            total_models = total_models.saturating_add(models_object.len());
            self.check_total_models(total_models)?;
            let mut seen_ids = BTreeSet::new();
            let mut models = Vec::new();
            for raw_model in models_object.values() {
                if let Some(model) = normalize_model(raw_model, ModelSource::ModelsCatalog)? {
                    if !seen_ids.insert(model.id.clone()) {
                        return Err(malformed_discovery(format!(
                            "catalog provider {provider_key:?} contains duplicate model id {:?}",
                            model.id
                        )));
                    }
                    models.push(model);
                }
            }
            let base_url = optional_string(object, "api")
                .filter(|value| !value.is_empty())
                .map(|value| normalize_base_url(value, wire))
                .transpose()?;
            let provider = DiscoveredProvider {
                id: provider_key.clone(),
                display_name: optional_nonempty_string(object, "name")
                    .unwrap_or(&provider_key)
                    .to_owned(),
                wire,
                base_url,
                credential_environment: string_array(object.get("env")),
                raw_model_count: models_object.len(),
                models,
            };
            providers.insert(provider_key, provider);
        }
        Ok(providers)
    }

    fn parse_custom_registry(
        &self,
        bytes: &[u8],
        source_url: &str,
        api_key: Option<&SecretString>,
    ) -> Result<Vec<ProviderImportPlan>, ProviderError> {
        let root = parse_root(bytes, "custom registry")?;
        self.check_provider_count(root.len())?;
        let mut provider_ids = BTreeSet::new();
        let mut total_models = 0_usize;
        let mut providers = Vec::new();
        for (entry_key, raw) in root {
            let object = raw.as_object().ok_or_else(|| {
                malformed_discovery(format!(
                    "custom registry entry {entry_key:?} must be an object"
                ))
            })?;
            let id = required_string(object, "id", &entry_key)?;
            validate_provider_id(id)?;
            if !provider_ids.insert(id.to_owned()) {
                return Err(malformed_discovery(format!(
                    "custom registry contains duplicate provider id {id:?}"
                )));
            }
            let display_name = required_string(object, "name", &entry_key)?;
            let wire_name = required_string(object, "type", &entry_key)?;
            let wire = explicit_wire(wire_name).ok_or_else(|| {
                invalid_discovery(format!(
                    "custom registry provider {id:?} uses unsupported wire {wire_name:?}"
                ))
            })?;
            let api = required_string(object, "api", &entry_key)?;
            let base_url = Some(normalize_base_url(api, Some(wire))?);
            let models_object =
                object
                    .get("models")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        malformed_discovery(format!(
                            "custom registry provider {id:?} is missing a models object"
                        ))
                    })?;
            self.check_model_count(id, models_object.len())?;
            total_models = total_models.saturating_add(models_object.len());
            self.check_total_models(total_models)?;
            let mut seen_ids = BTreeSet::new();
            let mut models = Vec::new();
            for raw_model in models_object.values() {
                let model = normalize_model(raw_model, ModelSource::CustomRegistry)?
                    .ok_or_else(|| malformed_discovery("custom registry model is invalid"))?;
                if !seen_ids.insert(model.id.clone()) {
                    return Err(malformed_discovery(format!(
                        "custom registry provider {id:?} contains duplicate model id {:?}",
                        model.id
                    )));
                }
                models.push(model);
            }
            if models.is_empty() {
                return Err(malformed_discovery(format!(
                    "custom registry provider {id:?} has no usable models"
                )));
            }
            providers.push(ProviderImportPlan {
                source_url: source_url.to_owned(),
                id: id.to_owned(),
                display_name: display_name.to_owned(),
                wire,
                base_url,
                api_key: api_key.cloned().filter(|key| !key.is_empty()),
                credential_environment: string_array(object.get("env")),
                models,
                selected_model: None,
            });
        }
        Ok(providers)
    }

    fn check_provider_count(&self, count: usize) -> Result<(), ProviderError> {
        if count > self.limits.max_providers {
            Err(malformed_discovery(
                "catalog exceeds the configured provider limit",
            ))
        } else {
            Ok(())
        }
    }

    fn check_model_count(&self, provider: &str, count: usize) -> Result<(), ProviderError> {
        if count > self.limits.max_models_per_provider {
            Err(malformed_discovery(format!(
                "provider {provider:?} exceeds the configured model limit"
            )))
        } else {
            Ok(())
        }
    }

    fn check_total_models(&self, count: usize) -> Result<(), ProviderError> {
        if count > self.limits.max_total_models {
            Err(malformed_discovery(
                "catalog exceeds the configured total model limit",
            ))
        } else {
            Ok(())
        }
    }
}

async fn select_cancel<T, F>(
    future: F,
    cancellation: Option<&DiscoveryCancellationToken>,
) -> Result<T, ProviderError>
where
    F: Future<Output = T>,
{
    tokio::pin!(future);
    tokio::select! {
        biased;
        _ = cancellation_wait(cancellation) => Err(cancelled_discovery()),
        output = &mut future => Ok(output),
    }
}

fn cancellation_wait(
    cancellation: Option<&DiscoveryCancellationToken>,
) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
    match cancellation {
        Some(cancellation) => Box::pin(cancellation.cancelled()),
        None => Box::pin(pending()),
    }
}

fn parse_root(bytes: &[u8], label: &str) -> Result<Map<String, Value>, ProviderError> {
    match serde_json::from_slice(bytes)
        .map_err(|_| malformed_discovery(format!("{label} contains invalid JSON")))?
    {
        Value::Object(object) => Ok(object),
        _ => Err(malformed_discovery(format!(
            "{label} must be a JSON object"
        ))),
    }
}

#[derive(Clone, Copy)]
enum ModelSource {
    ModelsCatalog,
    CustomRegistry,
}

fn normalize_model(
    raw: &Value,
    source: ModelSource,
) -> Result<Option<DiscoveredModel>, ProviderError> {
    let Some(object) = raw.as_object() else {
        return Ok(None);
    };
    let Some(id) = optional_nonempty_string(object, "id") else {
        return Ok(None);
    };
    validate_model_id(id)?;
    if is_embedding_model(object) || has_non_text_output(object) {
        return Ok(None);
    }
    let limit = optional_object(object, "limit");
    let context = limit.and_then(|limit| positive_number(limit.get("context"), source));
    let output = limit.and_then(|limit| positive_number(limit.get("output"), source));
    let max_context_tokens = match source {
        ModelSource::ModelsCatalog => match context {
            Some(context) => context,
            None => return Ok(None),
        },
        ModelSource::CustomRegistry => context.or(output).unwrap_or(CUSTOM_DEFAULT_CONTEXT),
    };
    let inputs = string_array(
        object
            .get("modalities")
            .and_then(Value::as_object)
            .and_then(|modalities| modalities.get("input")),
    );
    let efforts = string_array(object.get("support_efforts"));
    let has_rich_custom_hints = object.get("tool_call").is_some()
        || object.get("reasoning").is_some()
        || object.get("modalities").is_some()
        || object.get("support_efforts").is_some();
    let tool_use = match source {
        ModelSource::ModelsCatalog => object
            .get("tool_call")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        ModelSource::CustomRegistry if !has_rich_custom_hints => true,
        ModelSource::CustomRegistry => object
            .get("tool_call")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    };
    let thinking = object
        .get("reasoning")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || !efforts.is_empty();
    let default_effort = optional_nonempty_string(object, "default_effort").map(str::to_owned);
    if let Some(default) = &default_effort {
        if !efforts.is_empty() && !efforts.contains(default) {
            return Err(malformed_discovery(format!(
                "model {id:?} has a default thinking effort not listed in support_efforts"
            )));
        }
    }
    Ok(Some(DiscoveredModel {
        id: id.to_owned(),
        display_name: optional_nonempty_string(object, "name").map(str::to_owned),
        max_output_tokens: output,
        capability: ModelCapability {
            image_in: inputs.iter().any(|value| value == "image"),
            video_in: inputs.iter().any(|value| value == "video"),
            audio_in: inputs.iter().any(|value| value == "audio"),
            thinking,
            tool_use,
            max_context_tokens,
            dynamically_loaded_tools: Some(
                object
                    .get("dynamically_loaded_tools")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ),
        },
        reasoning_key: reasoning_key(object.get("interleaved")),
        thinking_efforts: efforts,
        default_thinking_effort: default_effort,
    }))
}

fn positive_number(value: Option<&Value>, source: ModelSource) -> Option<u64> {
    match source {
        ModelSource::ModelsCatalog => value?.as_u64().filter(|value| *value > 0),
        ModelSource::CustomRegistry => {
            if let Some(value) = value?.as_u64().filter(|value| *value > 0) {
                return Some(value);
            }
            let value = value?.as_f64()?;
            (value.is_finite() && value > 0.0 && value.floor() <= u64::MAX as f64)
                .then(|| value.floor() as u64)
        }
    }
}

fn is_embedding_model(object: &Map<String, Value>) -> bool {
    ["id", "name", "family"]
        .iter()
        .filter_map(|field| optional_string(object, field))
        .any(has_embedding_marker)
}

fn has_embedding_marker(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.contains("embedding")
        || value == "embed"
        || value.starts_with("embed-")
        || value.ends_with("-embed")
        || value.contains("/embed-")
        || value.contains("_embed_")
}

fn has_non_text_output(object: &Map<String, Value>) -> bool {
    object
        .get("modalities")
        .and_then(Value::as_object)
        .and_then(|modalities| modalities.get("output"))
        .and_then(Value::as_array)
        .is_some_and(|outputs| !outputs.iter().any(|value| value.as_str() == Some("text")))
}

fn reasoning_key(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::Bool(true)) => Some("reasoning_content".into()),
        Some(Value::Object(object)) => optional_nonempty_string(object, "field")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        _ => None,
    }
}

fn infer_wire(
    explicit: Option<&str>,
    package: Option<&str>,
    id: Option<&str>,
) -> Option<ImportWireFamily> {
    if let Some(explicit) = explicit {
        return explicit_wire(explicit);
    }
    let package = package.unwrap_or_default().to_ascii_lowercase();
    let id = id.unwrap_or_default().to_ascii_lowercase();
    if package.contains("anthropic") || id.contains("anthropic") || id.contains("claude") {
        Some(ImportWireFamily::Anthropic)
    } else if id.contains("vertex") || package.contains("vertex") {
        Some(ImportWireFamily::Vertex)
    } else if package.contains("openai-responses") || id.contains("openai-responses") {
        Some(ImportWireFamily::OpenAiResponses)
    } else if package.contains("kimi")
        || package.contains("moonshot")
        || id.contains("kimi")
        || id.contains("moonshot")
    {
        Some(ImportWireFamily::Kimi)
    } else if package.contains("google") || id.contains("google") || id.contains("gemini") {
        Some(ImportWireFamily::Gemini)
    } else if package.contains("openai") || id.contains("openai") {
        Some(ImportWireFamily::OpenAiChat)
    } else {
        None
    }
}

fn explicit_wire(value: &str) -> Option<ImportWireFamily> {
    match value {
        "anthropic" => Some(ImportWireFamily::Anthropic),
        "openai" => Some(ImportWireFamily::OpenAiChat),
        "openai_responses" => Some(ImportWireFamily::OpenAiResponses),
        "kimi" => Some(ImportWireFamily::Kimi),
        "google-genai" => Some(ImportWireFamily::Gemini),
        "vertexai" => Some(ImportWireFamily::Vertex),
        _ => None,
    }
}

fn normalize_base_url(
    value: &str,
    wire: Option<ImportWireFamily>,
) -> Result<String, ProviderError> {
    let mut url = validate_url("provider base URL", value, false)?;
    if wire == Some(ImportWireFamily::Anthropic) {
        let path = url.path().trim_end_matches('/').to_owned();
        if path.ends_with("/v1") {
            let stripped = path.strip_suffix("/v1").unwrap_or_default();
            url.set_path(if stripped.is_empty() { "/" } else { stripped });
        }
    }
    Ok(normalized_url(&url))
}

fn validate_url(label: &str, value: &str, source: bool) -> Result<Url, ProviderError> {
    if value.len() > 4_096 {
        return Err(invalid_discovery(format!("{label} is too long")));
    }
    let url = Url::parse(value)
        .map_err(|_| invalid_discovery(format!("{label} is not a valid absolute URL")))?;
    let local_http =
        url.scheme() == "http" && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if url.scheme() != "https" && !local_http {
        return Err(invalid_discovery(format!(
            "{label} must use HTTPS or loopback HTTP"
        )));
    }
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid_discovery(format!(
            "{label} must not contain credentials, a query, or a fragment"
        )));
    }
    if source && !url.path().ends_with(".json") {
        return Err(invalid_discovery(format!(
            "{label} must identify an explicit JSON document"
        )));
    }
    Ok(url)
}

fn normalized_url(url: &Url) -> String {
    let mut value = url.to_string();
    if url.path() == "/" && url.query().is_none() && url.fragment().is_none() {
        let trimmed_len = value.trim_end_matches('/').len();
        value.truncate(trimmed_len);
    }
    value
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    entry: &str,
) -> Result<&'a str, ProviderError> {
    optional_nonempty_string(object, field).ok_or_else(|| {
        malformed_discovery(format!(
            "custom registry entry {entry:?} is missing non-empty {field:?}"
        ))
    })
}

fn optional_string<'a>(object: &'a Map<String, Value>, field: &str) -> Option<&'a str> {
    object.get(field).and_then(Value::as_str)
}

fn optional_nonempty_string<'a>(object: &'a Map<String, Value>, field: &str) -> Option<&'a str> {
    optional_string(object, field).filter(|value| !value.is_empty())
}

fn optional_object<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Option<&'a Map<String, Value>> {
    object.get(field).and_then(Value::as_object)
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
        .filter(|value| seen.insert((*value).to_owned()))
        .map(str::to_owned)
        .collect()
}

fn validate_provider_id(value: &str) -> Result<(), ProviderError> {
    if value.is_empty()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control() || byte == b'/')
    {
        Err(invalid_discovery("catalog contains an invalid provider id"))
    } else {
        Ok(())
    }
}

fn validate_model_id(value: &str) -> Result<(), ProviderError> {
    if value.is_empty()
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/' | b'@')
        })
    {
        Err(invalid_discovery("catalog contains an invalid model id"))
    } else {
        Ok(())
    }
}

fn catalog_status_error(status: u16) -> ProviderError {
    let kind = if matches!(status, 401 | 403) {
        ProviderErrorKind::Authentication
    } else if status == 429 {
        ProviderErrorKind::RateLimit
    } else if (400..500).contains(&status) {
        ProviderErrorKind::InvalidRequest
    } else {
        ProviderErrorKind::Other
    };
    ProviderError {
        kind,
        message: format!("catalog request failed with HTTP {status}"),
        retryable: status == 429 || (500..600).contains(&status),
        status_code: Some(status),
        retry_after_ms: None,
    }
}

fn invalid_discovery(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}

fn malformed_discovery(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::MalformedResponse, message)
}

fn cancelled_discovery() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Cancelled,
        "catalog request was cancelled",
    )
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use bytes::Bytes;
    use futures_util::stream;

    use crate::http::{ByteStream, HttpResponse, TransportError, TransportFuture};

    use super::*;

    enum FakeOutcome {
        Response {
            status: u16,
            headers: BTreeMap<String, String>,
            chunks: Vec<Vec<u8>>,
        },
        Error(TransportError),
        Pending,
    }

    #[derive(Default)]
    struct FakeTransport {
        requests: Mutex<Vec<HttpRequest>>,
        outcomes: Mutex<VecDeque<FakeOutcome>>,
    }

    impl FakeTransport {
        fn json(&self, value: Value) {
            self.raw(200, serde_json::to_vec(&value).expect("JSON"));
        }

        fn raw(&self, status: u16, body: Vec<u8>) {
            self.outcomes
                .lock()
                .expect("outcomes")
                .push_back(FakeOutcome::Response {
                    status,
                    headers: BTreeMap::new(),
                    chunks: vec![body],
                });
        }

        fn error(&self, error: TransportError) {
            self.outcomes
                .lock()
                .expect("outcomes")
                .push_back(FakeOutcome::Error(error));
        }

        fn pending(&self) {
            self.outcomes
                .lock()
                .expect("outcomes")
                .push_back(FakeOutcome::Pending);
        }
    }

    impl HttpTransport for FakeTransport {
        fn send<'a>(&'a self, request: HttpRequest) -> TransportFuture<'a> {
            self.requests.lock().expect("requests").push(request);
            match self
                .outcomes
                .lock()
                .expect("outcomes")
                .pop_front()
                .expect("fixture outcome")
            {
                FakeOutcome::Response {
                    status,
                    headers,
                    chunks,
                } => Box::pin(async move {
                    let body: ByteStream = Box::pin(stream::iter(
                        chunks.into_iter().map(|chunk| Ok(Bytes::from(chunk))),
                    ));
                    Ok(HttpResponse {
                        status,
                        headers,
                        body,
                    })
                }),
                FakeOutcome::Error(error) => Box::pin(async move { Err(error) }),
                FakeOutcome::Pending => Box::pin(pending()),
            }
        }
    }

    fn model(id: &str, context: u64) -> Value {
        serde_json::json!({
            "id":id,
            "name":format!("display {id}"),
            "limit":{"context":context,"output":8192},
            "tool_call":true,
            "reasoning":true,
            "dynamically_loaded_tools":true,
            "interleaved":{"field":"reasoning_trace"},
            "modalities":{"input":["text","image","video","audio"],"output":["text"]}
        })
    }

    fn catalog_request() -> CatalogFetchRequest {
        CatalogFetchRequest::new("https://catalog.example.test/api.json")
    }

    #[tokio::test]
    async fn models_catalog_supports_sorted_list_filter_detail_and_import_plan() {
        let transport = Arc::new(FakeTransport::default());
        transport.json(serde_json::json!({
            "vertex": {"id":"vertex-ai","name":"Vertex","type":"vertexai","api":"https://us-central1-aiplatform.googleapis.com","models":{"gemini":model("gemini-2.5-pro",1_000_000)}},
            "responses": {"id":"openai-responses","name":"Responses","type":"openai_responses","api":"https://api.openai.com/v1","models":{"gpt":model("gpt-5.5",1_000_000)}},
            "openai": {"id":"openai","name":"OpenAI","npm":"@ai-sdk/openai","api":"https://api.openai.com/v1","env":["OPENAI_API_KEY","OPENAI_API_KEY"],"models":{"gpt":model("gpt-4.1",128_000)}},
            "kimi": {"id":"moonshot-kimi","name":"Kimi","npm":"@moonshot/kimi","api":"https://api.kimi.com/coding/v1","models":{"kimi":model("kimi-k2",256_000)}},
            "google": {"id":"gemini","name":"Gemini","npm":"@ai-sdk/google","api":"https://generativelanguage.googleapis.com","models":{"gemini":model("gemini-2.5-pro",1_000_000)}},
            "anthropic": {
                "id":"anthropic","name":"Anthropic","npm":"@ai-sdk/anthropic",
                "api":"https://api.anthropic.com/v1","env":["ANTHROPIC_API_KEY"],
                "models":{
                    "main":model("claude-opus-4-7",200_000),
                    "embedding":{"id":"claude-embedding-1","limit":{"context":8192}},
                    "image":{"id":"claude-image","limit":{"context":8192},"modalities":{"output":["image"]}},
                    "missing-context":{"id":"claude-invalid"}
                }
            }
        }));
        let service = ProviderDiscoveryService::new(transport.clone());
        let catalog = service
            .fetch_models_catalog(catalog_request())
            .await
            .expect("catalog");

        let ids = catalog
            .list(None)
            .into_iter()
            .map(|provider| provider.id)
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "anthropic",
                "google",
                "kimi",
                "openai",
                "responses",
                "vertex"
            ]
        );
        assert_eq!(catalog.list(Some("OPEN")).len(), 1);
        assert_eq!(catalog.list(Some("gemini"))[0].id, "google");
        let anthropic = catalog.detail("anthropic").expect("detail");
        assert_eq!(anthropic.raw_model_count, 4);
        assert_eq!(anthropic.models.len(), 1);
        assert_eq!(
            anthropic.base_url.as_deref(),
            Some("https://api.anthropic.com")
        );
        let capability = anthropic.models[0].capability;
        assert!(
            capability.image_in
                && capability.video_in
                && capability.audio_in
                && capability.thinking
                && capability.tool_use
        );
        assert_eq!(capability.max_context_tokens, 200_000);
        assert_eq!(anthropic.models[0].max_output_tokens, Some(8_192));
        assert_eq!(
            anthropic.models[0].reasoning_key.as_deref(),
            Some("reasoning_trace")
        );

        let plan = catalog
            .plan_provider(
                "anthropic",
                SecretString::new("catalog-secret"),
                Some("claude-opus-4-7"),
            )
            .expect("plan");
        assert_eq!(plan.wire, ImportWireFamily::Anthropic);
        assert_eq!(plan.selected_model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(plan.credential_environment, ["ANTHROPIC_API_KEY"]);
        assert!(!format!("{plan:?}").contains("catalog-secret"));

        let request = &transport.requests.lock().expect("requests")[0];
        assert_eq!(request.method, "GET");
        assert_eq!(request.headers["accept"], "application/json");
        assert!(!request.headers.contains_key("authorization"));
    }

    #[tokio::test]
    async fn custom_registry_returns_multi_provider_plan_with_exact_auth() {
        let transport = Arc::new(FakeTransport::default());
        transport.json(serde_json::json!({
            "first": {
                "id":"custom-chat","name":"Custom Chat","api":"https://gateway.example.test/v1","type":"openai",
                "env":["CUSTOM_KEY"],
                "models":{
                    "gpt":{"id":"gpt-5.5","name":"GPT","limit":{"context":100000.9,"output":32000.8},"support_efforts":["low","high"],"default_effort":"high","modalities":{"input":["image"]}}
                }
            },
            "second": {
                "id":"custom-anthropic","name":"Custom Anthropic","api":"https://gateway.example.test/v1/","type":"anthropic",
                "models":{"claude":{"id":"claude-opus-4-7"}}
            }
        }));
        let service = ProviderDiscoveryService::new(transport.clone());
        let mut request = CustomRegistryFetchRequest::new(
            "https://registry.example.test/api.json",
            Some(SecretString::new("registry-secret")),
        );
        request.user_agent = Some("mycel/1.0".into());
        let plan = service
            .plan_custom_registry(request.clone())
            .await
            .expect("plan");

        assert_eq!(plan.providers.len(), 2);
        let anthropic = plan
            .providers
            .iter()
            .find(|provider| provider.id == "custom-anthropic")
            .expect("anthropic");
        assert_eq!(anthropic.wire, ImportWireFamily::Anthropic);
        assert_eq!(
            anthropic.base_url.as_deref(),
            Some("https://gateway.example.test")
        );
        assert_eq!(anthropic.models[0].capability.max_context_tokens, 131_072);
        assert!(anthropic.models[0].capability.tool_use);
        let chat = plan
            .providers
            .iter()
            .find(|provider| provider.id == "custom-chat")
            .expect("chat");
        assert_eq!(chat.models[0].capability.max_context_tokens, 100_000);
        assert_eq!(chat.models[0].max_output_tokens, Some(32_000));
        assert!(chat.models[0].capability.thinking && chat.models[0].capability.image_in);
        assert_eq!(chat.models[0].thinking_efforts, ["low", "high"]);

        let sent = &transport.requests.lock().expect("requests")[0];
        assert_eq!(sent.headers["authorization"], "Bearer registry-secret");
        assert_eq!(sent.headers["user-agent"], "mycel/1.0");
        assert!(!format!("{request:?}").contains("registry-secret"));
        assert!(!format!("{sent:?}").contains("registry-secret"));
        assert!(!format!("{plan:?}").contains("registry-secret"));
    }

    #[tokio::test]
    async fn duplicate_provider_and_model_ids_fail_closed() {
        let transport = Arc::new(FakeTransport::default());
        transport.json(serde_json::json!({
            "one":{"id":"same","name":"One","api":"https://one.example.test/v1","type":"openai","models":{"a":{"id":"a"}}},
            "two":{"id":"same","name":"Two","api":"https://two.example.test/v1","type":"openai","models":{"b":{"id":"b"}}}
        }));
        transport.json(serde_json::json!({
            "openai":{"id":"openai","type":"openai","models":{
                "one":model("gpt-4.1",128000),
                "two":model("gpt-4.1",128000)
            }}
        }));
        let service = ProviderDiscoveryService::new(transport);
        let provider_error = service
            .plan_custom_registry(CustomRegistryFetchRequest::new(
                "https://registry.example.test/api.json",
                None,
            ))
            .await
            .expect_err("duplicate provider");
        assert_eq!(provider_error.kind, ProviderErrorKind::MalformedResponse);
        let model_error = service
            .fetch_models_catalog(catalog_request())
            .await
            .expect_err("duplicate model");
        assert_eq!(model_error.kind, ProviderErrorKind::MalformedResponse);
    }

    #[tokio::test]
    async fn rejects_bad_and_massive_json_with_bounded_sanitized_errors() {
        let transport = Arc::new(FakeTransport::default());
        transport.raw(200, b"{not-json".to_vec());
        transport.raw(200, b"[]".to_vec());
        transport.raw(200, vec![b'x'; 65]);
        let service = ProviderDiscoveryService::with_limits(
            transport,
            DiscoveryLimits {
                max_response_bytes: 64,
                ..DiscoveryLimits::default()
            },
        )
        .expect("limits");
        for expected in ["invalid JSON", "JSON object", "size limit"] {
            let error = service
                .fetch_models_catalog(catalog_request())
                .await
                .expect_err("invalid fixture");
            assert_eq!(error.kind, ProviderErrorKind::MalformedResponse);
            assert!(error.message.contains(expected), "{}", error.message);
            assert!(!error.message.contains("not-json"));
        }
    }

    #[tokio::test]
    async fn rejects_unsupported_wires_unsafe_urls_and_redirects() {
        let unsupported = Arc::new(FakeTransport::default());
        unsupported.json(serde_json::json!({
            "x":{"id":"x","name":"X","api":"https://x.example.test","type":"bedrock","models":{"m":{"id":"m"}}}
        }));
        let service = ProviderDiscoveryService::new(unsupported.clone());
        let error = service
            .plan_custom_registry(CustomRegistryFetchRequest::new(
                "https://registry.example.test/api.json",
                None,
            ))
            .await
            .expect_err("unsupported wire");
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);

        let credential_url = CustomRegistryFetchRequest::new(
            "https://user:password@registry.example.test/api.json",
            None,
        );
        let error = service
            .plan_custom_registry(credential_url)
            .await
            .expect_err("URL credentials");
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert_eq!(unsupported.requests.lock().expect("requests").len(), 1);

        let redirects = Arc::new(FakeTransport::default());
        redirects.raw(302, Vec::new());
        let error = ProviderDiscoveryService::new(redirects.clone())
            .fetch_models_catalog(catalog_request())
            .await
            .expect_err("redirect");
        assert_eq!(error.status_code, Some(302));
        assert_eq!(redirects.requests.lock().expect("requests").len(), 1);

        let insecure = Arc::new(FakeTransport::default());
        insecure.json(serde_json::json!({
            "x":{"id":"x","name":"X","api":"http://remote.example.test/v1","type":"openai","models":{"m":{"id":"m"}}}
        }));
        let error = ProviderDiscoveryService::new(insecure)
            .plan_custom_registry(CustomRegistryFetchRequest::new(
                "https://registry.example.test/api.json",
                None,
            ))
            .await
            .expect_err("insecure provider URL");
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    }

    #[tokio::test]
    async fn cancellation_and_timeout_drop_transport_and_sanitize_details() {
        let cancelled_transport = Arc::new(FakeTransport::default());
        cancelled_transport.pending();
        let cancellation = DiscoveryCancellationToken::default();
        cancellation.cancel();
        let mut request = catalog_request();
        request.cancellation = Some(cancellation);
        let error = ProviderDiscoveryService::new(cancelled_transport)
            .fetch_models_catalog(request)
            .await
            .expect_err("cancelled");
        assert_eq!(error.kind, ProviderErrorKind::Cancelled);

        let timeout_transport = Arc::new(FakeTransport::default());
        timeout_transport.error(TransportError::timeout(
            "socket timeout included registry-secret",
        ));
        let error = ProviderDiscoveryService::new(timeout_transport)
            .plan_custom_registry(CustomRegistryFetchRequest::new(
                "https://registry.example.test/api.json",
                Some(SecretString::new("registry-secret")),
            ))
            .await
            .expect_err("timeout");
        assert_eq!(error.kind, ProviderErrorKind::Connection);
        assert!(error.message.contains("timed out"));
        assert!(!error.message.contains("registry-secret"));
    }

    #[tokio::test]
    async fn invalid_api_key_is_rejected_before_transport_without_echo() {
        let transport = Arc::new(FakeTransport::default());
        let request = CustomRegistryFetchRequest::new(
            "https://registry.example.test/api.json",
            Some(SecretString::new("secret\nheader")),
        );
        let error = ProviderDiscoveryService::new(transport.clone())
            .plan_custom_registry(request)
            .await
            .expect_err("invalid header");
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert!(!error.message.contains("secret"));
        assert!(transport.requests.lock().expect("requests").is_empty());
    }
}
