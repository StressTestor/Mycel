//! Provider and login management independent of terminal presentation.
//!
//! The command service uses injected transport, input, output, environment,
//! clock, and config-store boundaries. Production callers can therefore share
//! the same behavior with interactive and headless frontends while tests never
//! open a network connection or browser.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    fs::{self, OpenOptions},
    future::Future,
    io::{self, Write},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use mycel_agent_protocol::{
    CredentialStorage, ModelConfig, MycelConfig, ProviderEntryConfig, ProviderError, ProviderType,
    SecretString,
};
use mycel_providers::{
    CatalogFetchRequest, CredentialStore, CustomRegistryFetchRequest, CustomRegistryImportPlan,
    DevicePoll, DiscoveryCancellationToken, HttpTransport, ImportWireFamily, KimiIdentity,
    KimiOAuthClient, KimiOAuthConfig, ProviderCatalogListItem, ProviderDiscoveryService,
    ProviderImportPlan, KIMI_CLIENT_ID, KIMI_MANAGED_BASE_URL, KIMI_OAUTH_HOST,
};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::production::parse_config;

const MAX_CONFIG_BYTES: usize = 4 * 1024 * 1024;
const KIMI_STORAGE_NAME: &str = "kimi-code";

pub type ProviderCommandFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

#[derive(Debug)]
pub enum ProviderCommandError {
    Invalid(String),
    Provider(ProviderError),
    Io(String),
    Cancelled,
}

impl fmt::Display for ProviderCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) | Self::Io(message) => formatter.write_str(message),
            Self::Provider(error) => error.fmt(formatter),
            Self::Cancelled => formatter.write_str("provider command cancelled"),
        }
    }
}

impl Error for ProviderCommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Provider(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ProviderError> for ProviderCommandError {
    fn from(value: ProviderError) -> Self {
        Self::Provider(value)
    }
}

pub trait ProviderConfigStore: Send + Sync {
    fn path(&self) -> &Path;
    fn load(&self) -> Result<String, ProviderCommandError>;
    fn replace(&self, source: &str) -> Result<(), ProviderCommandError>;
}

#[derive(Clone, Debug)]
pub struct AtomicTomlConfigStore {
    path: PathBuf,
}

impl AtomicTomlConfigStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl ProviderConfigStore for AtomicTomlConfigStore {
    fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> Result<String, ProviderCommandError> {
        match fs::read(&self.path) {
            Ok(bytes) if bytes.len() <= MAX_CONFIG_BYTES => {
                String::from_utf8(bytes).map_err(|_| {
                    ProviderCommandError::Invalid(format!(
                        "provider config {} is not UTF-8",
                        self.path.display()
                    ))
                })
            }
            Ok(_) => Err(ProviderCommandError::Invalid(format!(
                "provider config {} exceeds {} bytes",
                self.path.display(),
                MAX_CONFIG_BYTES
            ))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(ProviderCommandError::Io(format!(
                "could not read provider config {}: {error}",
                self.path.display()
            ))),
        }
    }

    fn replace(&self, source: &str) -> Result<(), ProviderCommandError> {
        if source.len() > MAX_CONFIG_BYTES {
            return Err(ProviderCommandError::Invalid(format!(
                "provider config exceeds {MAX_CONFIG_BYTES} bytes"
            )));
        }
        if fs::symlink_metadata(&self.path).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(ProviderCommandError::Invalid(format!(
                "refusing to replace symlinked provider config {}",
                self.path.display()
            )));
        }
        let parent = self.path.parent().ok_or_else(|| {
            ProviderCommandError::Invalid("provider config has no parent directory".to_owned())
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            ProviderCommandError::Io(format!(
                "could not create provider config directory {}: {error}",
                parent.display()
            ))
        })?;
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                ProviderCommandError::Invalid("provider config filename is invalid".to_owned())
            })?;
        let temporary = parent.join(format!(
            ".{file_name}.tmp.{}.{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        let result = (|| -> io::Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(source.as_bytes())?;
            file.sync_all()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
            }
            fs::rename(&temporary, &self.path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
            }
            if let Ok(directory) = fs::File::open(parent) {
                let _ = directory.sync_all();
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_file(&temporary);
            return Err(ProviderCommandError::Io(format!(
                "could not atomically replace provider config {}: {error}",
                self.path.display()
            )));
        }
        Ok(())
    }
}

pub trait ProviderCommandEnvironment: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessProviderEnvironment;

impl ProviderCommandEnvironment for ProcessProviderEnvironment {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

pub trait ProviderCommandInput: Send + Sync {
    fn api_key(&self, provider_id: &str) -> Result<Option<SecretString>, ProviderCommandError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoProviderCommandInput;

impl ProviderCommandInput for NoProviderCommandInput {
    fn api_key(&self, _provider_id: &str) -> Result<Option<SecretString>, ProviderCommandError> {
        Ok(None)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderCommandEvent {
    DeviceAuthorization {
        user_code: String,
        verification_uri: String,
        expires_in: u64,
    },
    LoginComplete {
        provider: String,
    },
    LogoutComplete {
        provider: String,
    },
    ConfigUpdated {
        path: PathBuf,
        provider_count: usize,
        model_count: usize,
    },
}

pub trait ProviderCommandOutput: Send + Sync {
    fn emit(&self, event: ProviderCommandEvent) -> Result<(), ProviderCommandError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct IgnoreProviderCommandOutput;

impl ProviderCommandOutput for IgnoreProviderCommandOutput {
    fn emit(&self, _event: ProviderCommandEvent) -> Result<(), ProviderCommandError> {
        Ok(())
    }
}

pub trait ProviderCommandClock: Send + Sync {
    fn now_seconds(&self) -> u64;
    fn sleep<'a>(&'a self, duration: Duration) -> ProviderCommandFuture<'a>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TokioProviderCommandClock;

impl ProviderCommandClock for TokioProviderCommandClock {
    fn now_seconds(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn sleep<'a>(&'a self, duration: Duration) -> ProviderCommandFuture<'a> {
        Box::pin(tokio::time::sleep(duration))
    }
}

struct ProviderCancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

#[derive(Clone)]
pub struct ProviderCommandCancellation(Arc<ProviderCancellationState>);

impl Default for ProviderCommandCancellation {
    fn default() -> Self {
        Self(Arc::new(ProviderCancellationState {
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
        }))
    }
}

impl fmt::Debug for ProviderCommandCancellation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderCommandCancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl ProviderCommandCancellation {
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    pub(crate) async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let notified = self.0.notify.notified();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialStatus {
    Configured,
    Codex,
    Environment(String),
    Missing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfiguredProviderView {
    pub id: String,
    pub provider_type: ProviderType,
    pub base_url: Option<String>,
    pub model_count: usize,
    pub credential: CredentialStatus,
    pub is_default: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogListResult {
    Providers(Vec<ProviderCatalogListItem>),
    Models {
        provider_id: String,
        display_name: String,
        models: Vec<String>,
    },
}

#[derive(Clone, Debug)]
pub struct CatalogAddRequest {
    pub url: String,
    pub provider_id: String,
    pub api_key: Option<SecretString>,
    pub default_model: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CustomRegistryRequest {
    pub url: String,
    pub api_key: Option<SecretString>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderMutationSummary {
    pub providers: Vec<String>,
    pub models: Vec<String>,
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
}

pub struct ProviderCommandService {
    discovery: ProviderDiscoveryService,
    transport: Arc<dyn HttpTransport>,
    config_store: Arc<dyn ProviderConfigStore>,
    environment: Arc<dyn ProviderCommandEnvironment>,
    input: Arc<dyn ProviderCommandInput>,
    output: Arc<dyn ProviderCommandOutput>,
    clock: Arc<dyn ProviderCommandClock>,
    mycel_home: PathBuf,
    version: String,
}

pub struct ProviderCommandDependencies {
    pub transport: Arc<dyn HttpTransport>,
    pub config_store: Arc<dyn ProviderConfigStore>,
    pub environment: Arc<dyn ProviderCommandEnvironment>,
    pub input: Arc<dyn ProviderCommandInput>,
    pub output: Arc<dyn ProviderCommandOutput>,
    pub clock: Arc<dyn ProviderCommandClock>,
}

impl ProviderCommandService {
    pub fn new(
        dependencies: ProviderCommandDependencies,
        mycel_home: PathBuf,
        version: impl Into<String>,
    ) -> Self {
        Self {
            discovery: ProviderDiscoveryService::new(Arc::clone(&dependencies.transport)),
            transport: dependencies.transport,
            config_store: dependencies.config_store,
            environment: dependencies.environment,
            input: dependencies.input,
            output: dependencies.output,
            clock: dependencies.clock,
            mycel_home,
            version: version.into(),
        }
    }

    pub async fn login_kimi(
        &self,
        cancellation: &ProviderCommandCancellation,
    ) -> Result<(), ProviderCommandError> {
        if cancellation.is_cancelled() {
            return Err(ProviderCommandError::Cancelled);
        }
        let identity = KimiIdentity::load(&self.mycel_home, &self.version)?;
        let oauth = KimiOAuthClient::new(
            KimiOAuthConfig {
                oauth_host: KIMI_OAUTH_HOST.to_owned(),
                client_id: KIMI_CLIENT_ID.to_owned(),
                api_base_url: KIMI_MANAGED_BASE_URL.to_owned(),
                storage_name: KIMI_STORAGE_NAME.to_owned(),
            },
            identity,
            Arc::clone(&self.transport),
        );
        let begin = oauth.begin_device_authorization();
        let authorization = tokio::select! {
            result = begin => result?,
            () = cancellation.cancelled() => return Err(ProviderCommandError::Cancelled),
        };
        let expires_in = authorization.expires_in.unwrap_or(15 * 60).max(1);
        let verification_uri = if authorization.verification_uri_complete.is_empty() {
            authorization.verification_uri.clone()
        } else {
            authorization.verification_uri_complete.clone()
        };
        self.output
            .emit(ProviderCommandEvent::DeviceAuthorization {
                user_code: authorization.user_code.clone(),
                verification_uri,
                expires_in,
            })?;
        let deadline = self.clock.now_seconds().saturating_add(expires_in);
        let mut interval = authorization.interval_seconds.max(1);
        let token = loop {
            if cancellation.is_cancelled() {
                return Err(ProviderCommandError::Cancelled);
            }
            if self.clock.now_seconds() >= deadline {
                return Err(ProviderCommandError::Invalid(
                    "Kimi device authorization expired".to_owned(),
                ));
            }
            let poll = oauth.poll_once(&authorization.device_code);
            let (state, token) = tokio::select! {
                result = poll => result?,
                () = cancellation.cancelled() => return Err(ProviderCommandError::Cancelled),
            };
            match state {
                DevicePoll::Token => {
                    break token.ok_or_else(|| {
                        ProviderCommandError::Invalid(
                            "successful Kimi login returned no token".to_owned(),
                        )
                    })?
                }
                DevicePoll::Pending { slow_down, .. } => {
                    if slow_down {
                        interval = interval.saturating_add(5);
                    }
                    tokio::select! {
                        () = self.clock.sleep(Duration::from_secs(interval)) => {}
                        () = cancellation.cancelled() => return Err(ProviderCommandError::Cancelled),
                    }
                }
                DevicePoll::Expired => {
                    return Err(ProviderCommandError::Invalid(
                        "Kimi device authorization expired".to_owned(),
                    ))
                }
                DevicePoll::Denied(description) => {
                    return Err(ProviderCommandError::Invalid(format!(
                        "Kimi authorization denied: {description}"
                    )))
                }
            }
        };
        if cancellation.is_cancelled() {
            return Err(ProviderCommandError::Cancelled);
        }
        CredentialStore::mycel_home(&self.mycel_home).save(KIMI_STORAGE_NAME, &token)?;
        self.output.emit(ProviderCommandEvent::LoginComplete {
            provider: "kimi".to_owned(),
        })?;
        Ok(())
    }

    pub fn logout_kimi(&self) -> Result<(), ProviderCommandError> {
        CredentialStore::mycel_home(&self.mycel_home).remove(KIMI_STORAGE_NAME)?;
        self.output.emit(ProviderCommandEvent::LogoutComplete {
            provider: "kimi".to_owned(),
        })?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<ConfiguredProviderView>, ProviderCommandError> {
        let config = self.load_config()?;
        Ok(config
            .providers
            .iter()
            .map(|(id, provider)| ConfiguredProviderView {
                id: id.clone(),
                provider_type: provider.provider_type,
                base_url: provider.base_url.clone(),
                model_count: config
                    .models
                    .values()
                    .filter(|model| model.provider == *id)
                    .count(),
                credential: credential_status(provider, self.environment.as_ref()),
                is_default: config.default_provider.as_deref() == Some(id)
                    || config
                        .default_model
                        .as_ref()
                        .and_then(|model| config.models.get(model))
                        .is_some_and(|model| model.provider == *id),
            })
            .collect())
    }

    pub fn remove(
        &self,
        provider_id: &str,
    ) -> Result<ProviderMutationSummary, ProviderCommandError> {
        require_provider_id(provider_id)?;
        self.mutate_config(|config| {
            if config.providers.remove(provider_id).is_none() {
                return Err(ProviderCommandError::Invalid(format!(
                    "provider {provider_id:?} is not configured"
                )));
            }
            config
                .models
                .retain(|_, model| model.provider != provider_id);
            normalize_defaults(config);
            Ok(())
        })
    }

    pub async fn catalog_list(
        &self,
        url: &str,
        provider_id: Option<&str>,
        filter: Option<&str>,
        cancellation: Option<DiscoveryCancellationToken>,
    ) -> Result<CatalogListResult, ProviderCommandError> {
        require_explicit_url(url)?;
        let mut request = CatalogFetchRequest::new(url);
        request.cancellation = cancellation;
        let catalog = self.discovery.fetch_models_catalog(request).await?;
        match provider_id {
            None => Ok(CatalogListResult::Providers(catalog.list(filter))),
            Some(provider_id) => {
                let provider = catalog.detail(provider_id).ok_or_else(|| {
                    ProviderCommandError::Invalid(format!(
                        "catalog provider {provider_id:?} was not found"
                    ))
                })?;
                let needle = filter.map(str::to_ascii_lowercase);
                let models = provider
                    .models
                    .iter()
                    .filter(|model| {
                        needle.as_ref().is_none_or(|needle| {
                            model.id.to_ascii_lowercase().contains(needle)
                                || model
                                    .display_name
                                    .as_ref()
                                    .is_some_and(|name| name.to_ascii_lowercase().contains(needle))
                        })
                    })
                    .map(|model| model.id.clone())
                    .collect();
                Ok(CatalogListResult::Models {
                    provider_id: provider.id.clone(),
                    display_name: provider.display_name.clone(),
                    models,
                })
            }
        }
    }

    pub async fn catalog_add(
        &self,
        request: CatalogAddRequest,
        cancellation: Option<DiscoveryCancellationToken>,
    ) -> Result<ProviderMutationSummary, ProviderCommandError> {
        require_explicit_url(&request.url)?;
        require_provider_id(&request.provider_id)?;
        let mut fetch = CatalogFetchRequest::new(&request.url);
        fetch.cancellation = cancellation;
        let catalog = self.discovery.fetch_models_catalog(fetch).await?;
        let config = self.load_config()?;
        let provider = catalog.detail(&request.provider_id).ok_or_else(|| {
            ProviderCommandError::Invalid(format!(
                "catalog provider {:?} was not found",
                request.provider_id
            ))
        })?;
        let key = self.resolve_api_key(
            &request.provider_id,
            request.api_key,
            config.providers.get(&request.provider_id),
            &provider.credential_environment,
        )?;
        let plan =
            catalog.plan_provider(&request.provider_id, key, request.default_model.as_deref())?;
        self.apply_plans(vec![plan], request.default_model.is_some())
    }

    pub async fn custom_registry_plan(
        &self,
        request: CustomRegistryRequest,
        cancellation: Option<DiscoveryCancellationToken>,
    ) -> Result<CustomRegistryImportPlan, ProviderCommandError> {
        require_explicit_url(&request.url)?;
        let mut fetch = CustomRegistryFetchRequest::new(request.url, request.api_key);
        fetch.cancellation = cancellation;
        Ok(self.discovery.plan_custom_registry(fetch).await?)
    }

    pub async fn custom_registry_add(
        &self,
        request: CustomRegistryRequest,
        cancellation: Option<DiscoveryCancellationToken>,
    ) -> Result<ProviderMutationSummary, ProviderCommandError> {
        let supplied_key = request.api_key.clone();
        let mut plan = self.custom_registry_plan(request, cancellation).await?;
        let config = self.load_config()?;
        for provider in &mut plan.providers {
            let key = self.resolve_api_key(
                &provider.id,
                supplied_key.clone().or_else(|| provider.api_key.clone()),
                config.providers.get(&provider.id),
                &provider.credential_environment,
            )?;
            provider.api_key = Some(key);
        }
        self.apply_plans(plan.providers, false)
    }

    fn resolve_api_key(
        &self,
        provider_id: &str,
        explicit: Option<SecretString>,
        configured: Option<&ProviderEntryConfig>,
        environment_names: &[String],
    ) -> Result<SecretString, ProviderCommandError> {
        if let Some(key) = explicit.filter(|key| !key.is_empty()) {
            return Ok(key);
        }
        // Persisted configuration intentionally precedes ambient environment.
        if let Some(key) = configured
            .and_then(|provider| provider.api_key.as_deref())
            .filter(|key| !key.is_empty())
        {
            return Ok(SecretString::new(key.to_owned()));
        }
        if let Some(key) = configured.and_then(|provider| {
            environment_names.iter().find_map(|name| {
                provider
                    .env
                    .get(name)
                    .filter(|value| !value.trim().is_empty())
            })
        }) {
            return Ok(SecretString::new(key.to_owned()));
        }
        if let Some(key) = environment_names.iter().find_map(|name| {
            self.environment
                .get(name)
                .filter(|value| !value.trim().is_empty())
        }) {
            return Ok(SecretString::new(key));
        }
        if let Some(key) = self
            .input
            .api_key(provider_id)?
            .filter(|key| !key.is_empty())
        {
            return Ok(key);
        }
        Err(ProviderCommandError::Invalid(format!(
            "provider {provider_id:?} requires an API key"
        )))
    }

    fn apply_plans(
        &self,
        mut plans: Vec<ProviderImportPlan>,
        force_default: bool,
    ) -> Result<ProviderMutationSummary, ProviderCommandError> {
        plans.sort_by(|left, right| left.id.cmp(&right.id));
        self.mutate_config(move |config| {
            let had_default = config
                .default_model
                .as_ref()
                .is_some_and(|alias| config.models.contains_key(alias));
            let mut first_added = None;
            let mut forced_default = None;
            for plan in plans {
                let selected = install_plan(config, plan)?;
                first_added.get_or_insert_with(|| selected.clone());
                if force_default {
                    forced_default = Some(selected);
                }
            }
            let selected_default = forced_default.or(if had_default { None } else { first_added });
            if let Some(alias) = selected_default {
                config.default_model = Some(alias.clone());
                config.default_provider = config
                    .models
                    .get(&alias)
                    .map(|model| model.provider.clone());
            }
            normalize_defaults(config);
            Ok(())
        })
    }

    fn load_config(&self) -> Result<MycelConfig, ProviderCommandError> {
        let source = self.config_store.load()?;
        let config = parse_config(&source).map_err(|_| {
            // TOML diagnostics may include the source line. Configuration can
            // contain API keys, so command-facing errors only identify the
            // invalid file and never echo parser excerpts.
            ProviderCommandError::Invalid(format!(
                "invalid provider config {}",
                self.config_store.path().display()
            ))
        })?;
        validate_configured_urls(&config)?;
        Ok(config)
    }

    fn mutate_config(
        &self,
        mutation: impl FnOnce(&mut MycelConfig) -> Result<(), ProviderCommandError>,
    ) -> Result<ProviderMutationSummary, ProviderCommandError> {
        let mut config = self.load_config()?;
        mutation(&mut config)?;
        config
            .validate_runtime()
            .map_err(|error| ProviderCommandError::Invalid(error.to_string()))?;
        let source = toml::to_string_pretty(&config).map_err(|error| {
            ProviderCommandError::Invalid(format!("could not encode provider config: {error}"))
        })?;
        parse_config(&source).map_err(|_| {
            ProviderCommandError::Invalid("encoded provider config is invalid".to_owned())
        })?;
        self.config_store.replace(&source)?;
        self.output.emit(ProviderCommandEvent::ConfigUpdated {
            path: self.config_store.path().to_path_buf(),
            provider_count: config.providers.len(),
            model_count: config.models.len(),
        })?;
        Ok(summary(&config))
    }
}

fn install_plan(
    config: &mut MycelConfig,
    mut plan: ProviderImportPlan,
) -> Result<String, ProviderCommandError> {
    require_provider_id(&plan.id)?;
    if plan.models.is_empty() {
        return Err(ProviderCommandError::Invalid(format!(
            "provider {:?} has no usable models",
            plan.id
        )));
    }
    let provider_type = provider_type(plan.wire);
    let api_key = plan
        .api_key
        .take()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| {
            ProviderCommandError::Invalid(format!("provider {:?} requires an API key", plan.id))
        })?;
    if let Some(existing) = config.providers.get(&plan.id) {
        if existing.provider_type != provider_type {
            return Err(ProviderCommandError::Invalid(format!(
                "provider {:?} is already configured with a different wire family",
                plan.id
            )));
        }
        if existing.oauth.is_some() {
            return Err(ProviderCommandError::Invalid(format!(
                "provider {:?} already uses OAuth credentials",
                plan.id
            )));
        }
    }
    plan.models.sort_by(|left, right| left.id.cmp(&right.id));
    let selected_model = plan
        .selected_model
        .clone()
        .unwrap_or_else(|| plan.models[0].id.clone());
    let mut aliases = BTreeMap::new();
    for model in plan.models {
        let alias = available_alias(config, &plan.id, &model.id)?;
        let capabilities = capability_names(model.capability);
        config.models.insert(
            alias.clone(),
            ModelConfig {
                provider: plan.id.clone(),
                model: model.id.clone(),
                max_context_size: model.capability.max_context_tokens,
                max_output_size: model.max_output_tokens,
                capabilities,
                display_name: model.display_name,
                reasoning_key: model.reasoning_key,
                protocol: None,
                adaptive_thinking: None,
                support_efforts: model.thinking_efforts,
                default_effort: model.default_thinking_effort,
                beta_api: None,
                overrides: None,
            },
        );
        aliases.insert(model.id, alias);
    }
    let selected_alias = aliases.get(&selected_model).cloned().ok_or_else(|| {
        ProviderCommandError::Invalid(format!(
            "selected model {selected_model:?} is not in provider {:?}",
            plan.id
        ))
    })?;
    config.providers.insert(
        plan.id.clone(),
        ProviderEntryConfig {
            provider_type,
            api_key: Some(api_key.expose().to_owned()),
            base_url: plan.base_url,
            default_model: Some(selected_model),
            oauth: None,
            env: BTreeMap::new(),
            custom_headers: BTreeMap::new(),
            source: BTreeMap::from([(
                "catalog_url".to_owned(),
                serde_json::Value::String(plan.source_url),
            )]),
        },
    );
    Ok(selected_alias)
}

fn available_alias(
    config: &MycelConfig,
    provider_id: &str,
    model_id: &str,
) -> Result<String, ProviderCommandError> {
    match config.models.get(model_id) {
        None => Ok(model_id.to_owned()),
        Some(existing) if existing.provider == provider_id && existing.model == model_id => {
            Ok(model_id.to_owned())
        }
        Some(_) => {
            let qualified = format!("{provider_id}:{model_id}");
            match config.models.get(&qualified) {
                None => Ok(qualified),
                Some(existing)
                    if existing.provider == provider_id && existing.model == model_id =>
                {
                    Ok(qualified)
                }
                Some(_) => Err(ProviderCommandError::Invalid(format!(
                    "model alias collision for {model_id:?} and {qualified:?}"
                ))),
            }
        }
    }
}

fn normalize_defaults(config: &mut MycelConfig) {
    if config
        .default_model
        .as_ref()
        .is_none_or(|alias| !config.models.contains_key(alias))
    {
        config.default_model = config.models.keys().next().cloned();
    }
    config.default_provider = config
        .default_model
        .as_ref()
        .and_then(|alias| config.models.get(alias))
        .map(|model| model.provider.clone())
        .or_else(|| config.providers.keys().next().cloned());
}

fn summary(config: &MycelConfig) -> ProviderMutationSummary {
    ProviderMutationSummary {
        providers: config.providers.keys().cloned().collect(),
        models: config.models.keys().cloned().collect(),
        default_provider: config.default_provider.clone(),
        default_model: config.default_model.clone(),
    }
}

fn provider_type(wire: ImportWireFamily) -> ProviderType {
    match wire {
        ImportWireFamily::Anthropic => ProviderType::Anthropic,
        ImportWireFamily::OpenAiChat => ProviderType::OpenAi,
        ImportWireFamily::OpenAiResponses => ProviderType::OpenAiResponses,
        ImportWireFamily::Kimi => ProviderType::Kimi,
        ImportWireFamily::Gemini => ProviderType::GoogleGenAi,
        ImportWireFamily::Vertex => ProviderType::VertexAi,
    }
}

fn capability_names(capability: mycel_agent_protocol::ModelCapability) -> Vec<String> {
    let mut capabilities = Vec::new();
    if capability.thinking {
        capabilities.push("thinking".to_owned());
    }
    if capability.image_in {
        capabilities.push("image_in".to_owned());
    }
    if capability.video_in {
        capabilities.push("video_in".to_owned());
    }
    if capability.audio_in {
        capabilities.push("audio_in".to_owned());
    }
    if capability.tool_use {
        capabilities.push("tool_use".to_owned());
    }
    capabilities
}

fn credential_status(
    provider: &ProviderEntryConfig,
    environment: &dyn ProviderCommandEnvironment,
) -> CredentialStatus {
    if provider
        .api_key
        .as_deref()
        .is_some_and(|key| !key.is_empty())
        || default_environment_names(provider.provider_type)
            .iter()
            .any(|name| {
                provider
                    .env
                    .get(*name)
                    .is_some_and(|value| !value.trim().is_empty())
            })
    {
        return CredentialStatus::Configured;
    }
    if let Some(oauth) = provider
        .oauth
        .as_ref()
        .filter(|oauth| !oauth.key.is_empty())
    {
        return match oauth.storage {
            CredentialStorage::Codex => CredentialStatus::Codex,
        };
    }
    default_environment_names(provider.provider_type)
        .iter()
        .find(|name| environment.get(name).is_some_and(|value| !value.is_empty()))
        .map_or(CredentialStatus::Missing, |name| {
            CredentialStatus::Environment((*name).to_owned())
        })
}

fn default_environment_names(provider_type: ProviderType) -> &'static [&'static str] {
    match provider_type {
        ProviderType::Anthropic => &["ANTHROPIC_API_KEY"],
        ProviderType::OpenAi | ProviderType::OpenAiResponses => &["OPENAI_API_KEY"],
        ProviderType::Kimi => &["KIMI_API_KEY"],
        ProviderType::GoogleGenAi => &["GOOGLE_API_KEY", "GEMINI_API_KEY"],
        ProviderType::VertexAi => &[
            "GOOGLE_APPLICATION_CREDENTIALS",
            "VERTEXAI_API_KEY",
            "GOOGLE_API_KEY",
        ],
    }
}

fn require_explicit_url(url: &str) -> Result<(), ProviderCommandError> {
    if url.trim().is_empty() {
        Err(ProviderCommandError::Invalid(
            "an explicit catalog URL is required".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn validate_configured_urls(config: &MycelConfig) -> Result<(), ProviderCommandError> {
    for provider in config.providers.values() {
        if let Some(base_url) = provider.base_url.as_deref() {
            validate_configured_url(base_url)?;
        }
        for (name, value) in &provider.env {
            if name.ends_with("_BASE_URL") {
                validate_configured_url(value)?;
            }
        }
    }
    Ok(())
}

fn validate_configured_url(value: &str) -> Result<(), ProviderCommandError> {
    let invalid = || {
        ProviderCommandError::Invalid(
            "configured provider base URL must use HTTPS or loopback HTTP and contain no credentials, query, or fragment"
                .to_owned(),
        )
    };
    if value.len() > 4_096 {
        return Err(invalid());
    }
    let url = reqwest::Url::parse(value).map_err(|_| invalid())?;
    let local_http =
        url.scheme() == "http" && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if (url.scheme() != "https" && !local_http)
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    Ok(())
}

fn require_provider_id(provider_id: &str) -> Result<(), ProviderCommandError> {
    if !provider_id.is_empty()
        && provider_id.len() <= 128
        && provider_id
            .bytes()
            .enumerate()
            .all(|(index, byte)| match byte {
                b'a'..=b'z' | b'0'..=b'9' => true,
                b'_' | b'-' => index > 0,
                _ => false,
            })
    {
        Ok(())
    } else {
        Err(ProviderCommandError::Invalid(format!(
            "invalid provider id {provider_id:?}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures_util::stream;
    use mycel_providers::{HttpRequest, HttpResponse, TransportError, TransportFuture};
    use std::collections::VecDeque;
    use std::sync::{atomic::AtomicU64, Mutex};
    use tempfile::TempDir;

    #[derive(Default)]
    struct FakeTransport {
        responses: Mutex<VecDeque<Result<(u16, serde_json::Value), TransportError>>>,
        requests: Mutex<Vec<HttpRequest>>,
    }

    impl FakeTransport {
        fn response(&self, status: u16, value: serde_json::Value) {
            self.responses
                .lock()
                .expect("responses")
                .push_back(Ok((status, value)));
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
                .expect("fake response");
            Box::pin(async move {
                let (status, value) = response?;
                let body = serde_json::to_vec(&value).expect("JSON");
                Ok(HttpResponse {
                    status,
                    headers: BTreeMap::new(),
                    body: Box::pin(stream::iter([Ok(Bytes::from(body))])),
                })
            })
        }
    }

    struct MemoryConfigStore {
        path: PathBuf,
        source: Mutex<String>,
    }

    impl MemoryConfigStore {
        fn new(source: impl Into<String>) -> Self {
            Self {
                path: PathBuf::from("/memory/config.toml"),
                source: Mutex::new(source.into()),
            }
        }
    }

    impl ProviderConfigStore for MemoryConfigStore {
        fn path(&self) -> &Path {
            &self.path
        }

        fn load(&self) -> Result<String, ProviderCommandError> {
            Ok(self.source.lock().expect("source").clone())
        }

        fn replace(&self, source: &str) -> Result<(), ProviderCommandError> {
            *self.source.lock().expect("source") = source.to_owned();
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeEnvironment(Mutex<BTreeMap<String, String>>);

    impl ProviderCommandEnvironment for FakeEnvironment {
        fn get(&self, key: &str) -> Option<String> {
            self.0.lock().expect("environment").get(key).cloned()
        }
    }

    struct FakeInput(Option<SecretString>);

    impl ProviderCommandInput for FakeInput {
        fn api_key(
            &self,
            _provider_id: &str,
        ) -> Result<Option<SecretString>, ProviderCommandError> {
            Ok(self.0.clone())
        }
    }

    #[derive(Default)]
    struct RecordingOutput(Mutex<Vec<ProviderCommandEvent>>);

    impl ProviderCommandOutput for RecordingOutput {
        fn emit(&self, event: ProviderCommandEvent) -> Result<(), ProviderCommandError> {
            self.0.lock().expect("events").push(event);
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeClock(AtomicU64);

    impl ProviderCommandClock for FakeClock {
        fn now_seconds(&self) -> u64 {
            self.0.load(Ordering::Acquire)
        }

        fn sleep<'a>(&'a self, duration: Duration) -> ProviderCommandFuture<'a> {
            self.0.fetch_add(duration.as_secs(), Ordering::AcqRel);
            Box::pin(async {})
        }
    }

    fn service(
        transport: Arc<FakeTransport>,
        store: Arc<MemoryConfigStore>,
        environment: Arc<FakeEnvironment>,
        input: Arc<FakeInput>,
        output: Arc<RecordingOutput>,
        home: &Path,
    ) -> ProviderCommandService {
        ProviderCommandService::new(
            ProviderCommandDependencies {
                transport,
                config_store: store,
                environment,
                input,
                output,
                clock: Arc::new(FakeClock::default()),
            },
            home.to_path_buf(),
            "0.2.0",
        )
    }

    fn model(id: &str, context: u64) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "name": format!("display {id}"),
            "limit": {"context": context, "output": 4096},
            "tool_call": true,
            "reasoning": true,
            "modalities": {"input": ["text", "image"], "output": ["text"]}
        })
    }

    fn base_config() -> &'static str {
        r#"
default_model = "old"

[providers.old]
type = "openai"
api_key = "old-key"

[models.old]
provider = "old"
model = "old-model"
max_context_size = 8192
"#
    }

    #[tokio::test]
    async fn kimi_login_is_cancellable_and_persists_private_credentials() {
        let temp = TempDir::new().unwrap();
        let transport = Arc::new(FakeTransport::default());
        transport.response(
            200,
            serde_json::json!({
                "user_code":"ABCD",
                "device_code":"device",
                "verification_uri":"https://auth.kimi.com/device",
                "verification_uri_complete":"https://auth.kimi.com/device?code=ABCD",
                "expires_in":60,
                "interval":1
            }),
        );
        transport.response(400, serde_json::json!({"error":"authorization_pending"}));
        transport.response(
            200,
            serde_json::json!({
                "access_token":"access-secret",
                "refresh_token":"refresh-secret",
                "expires_in":3600,
                "token_type":"Bearer"
            }),
        );
        let output = Arc::new(RecordingOutput::default());
        let manager = service(
            transport,
            Arc::new(MemoryConfigStore::new("")),
            Arc::new(FakeEnvironment::default()),
            Arc::new(FakeInput(None)),
            Arc::clone(&output),
            temp.path(),
        );
        manager
            .login_kimi(&ProviderCommandCancellation::default())
            .await
            .unwrap();
        let credential = temp.path().join("credentials/kimi-code.json");
        assert!(credential.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&credential).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let events = output.0.lock().unwrap();
        assert!(matches!(
            events[0],
            ProviderCommandEvent::DeviceAuthorization { .. }
        ));
        assert!(matches!(
            events[1],
            ProviderCommandEvent::LoginComplete { .. }
        ));
        drop(events);
        manager.logout_kimi().unwrap();
        assert!(!credential.exists());
    }

    #[tokio::test]
    async fn cancelled_login_never_polls_or_writes_credentials() {
        let temp = TempDir::new().unwrap();
        let transport = Arc::new(FakeTransport::default());
        let cancellation = ProviderCommandCancellation::default();
        cancellation.cancel();
        let manager = service(
            Arc::clone(&transport),
            Arc::new(MemoryConfigStore::new("")),
            Arc::new(FakeEnvironment::default()),
            Arc::new(FakeInput(None)),
            Arc::new(RecordingOutput::default()),
            temp.path(),
        );
        assert!(matches!(
            manager.login_kimi(&cancellation).await,
            Err(ProviderCommandError::Cancelled)
        ));
        assert!(transport.requests.lock().unwrap().is_empty());
        assert!(!temp.path().join("credentials/kimi-code.json").exists());
    }

    #[test]
    fn provider_list_and_remove_are_deterministic_and_secret_free() {
        let temp = TempDir::new().unwrap();
        let store = Arc::new(MemoryConfigStore::new(base_config()));
        let manager = service(
            Arc::new(FakeTransport::default()),
            Arc::clone(&store),
            Arc::new(FakeEnvironment::default()),
            Arc::new(FakeInput(None)),
            Arc::new(RecordingOutput::default()),
            temp.path(),
        );
        let listed = manager.list().unwrap();
        assert_eq!(listed[0].id, "old");
        assert_eq!(listed[0].credential, CredentialStatus::Configured);
        assert!(!format!("{listed:?}").contains("old-key"));
        let removed = manager.remove("old").unwrap();
        assert!(removed.providers.is_empty());
        assert!(removed.models.is_empty());
        assert_eq!(removed.default_model, None);
        assert!(manager.remove("old").is_err());
    }

    #[tokio::test]
    async fn catalog_list_add_uses_explicit_url_and_configured_key_before_env() {
        let temp = TempDir::new().unwrap();
        let transport = Arc::new(FakeTransport::default());
        let catalog = serde_json::json!({
            "anthropic": {
                "id":"anthropic",
                "name":"Anthropic",
                "npm":"@ai-sdk/anthropic",
                "api":"https://api.anthropic.com/v1",
                "env":["ANTHROPIC_API_KEY"],
                "models": {
                    "z": model("claude-z", 200_000),
                    "a": model("claude-a", 100_000)
                }
            }
        });
        transport.response(200, catalog.clone());
        transport.response(200, catalog);
        let source = r#"
[providers.anthropic]
type = "anthropic"

[providers.anthropic.env]
ANTHROPIC_API_KEY = "configured-key"
"#;
        let store = Arc::new(MemoryConfigStore::new(source));
        let environment = Arc::new(FakeEnvironment::default());
        environment
            .0
            .lock()
            .unwrap()
            .insert("ANTHROPIC_API_KEY".to_owned(), "environment-key".to_owned());
        let manager = service(
            Arc::clone(&transport),
            Arc::clone(&store),
            environment,
            Arc::new(FakeInput(Some(SecretString::new("input-key")))),
            Arc::new(RecordingOutput::default()),
            temp.path(),
        );
        assert_eq!(
            manager.list().unwrap()[0].credential,
            CredentialStatus::Configured
        );
        assert!(manager.catalog_list("", None, None, None).await.is_err());
        let listed = manager
            .catalog_list("https://catalog.example.test/api.json", None, None, None)
            .await
            .unwrap();
        let CatalogListResult::Providers(listed) = listed else {
            panic!("providers")
        };
        assert_eq!(listed[0].id, "anthropic");
        let result = manager
            .catalog_add(
                CatalogAddRequest {
                    url: "https://catalog.example.test/api.json".to_owned(),
                    provider_id: "anthropic".to_owned(),
                    api_key: None,
                    default_model: Some("claude-z".to_owned()),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.default_model.as_deref(), Some("claude-z"));
        let parsed = parse_config(&store.source.lock().unwrap()).unwrap();
        assert_eq!(
            parsed.providers["anthropic"].api_key.as_deref(),
            Some("configured-key")
        );
        assert_eq!(parsed.models["claude-a"].max_context_size, 100_000);
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests
            .iter()
            .all(|request| request.url == "https://catalog.example.test/api.json"));
    }

    #[tokio::test]
    async fn custom_registry_plan_and_add_are_multi_provider_and_fail_without_keys() {
        let temp = TempDir::new().unwrap();
        let transport = Arc::new(FakeTransport::default());
        let registry = serde_json::json!({
            "one": {
                "id":"custom-one", "name":"One", "type":"openai",
                "api":"https://one.example.test/v1", "env":["ONE_KEY"],
                "models":{"one":model("model-one", 32000)}
            },
            "two": {
                "id":"custom-two", "name":"Two", "type":"anthropic",
                "api":"https://two.example.test/v1", "env":["TWO_KEY"],
                "models":{"two":model("model-two", 64000)}
            }
        });
        transport.response(200, registry.clone());
        transport.response(200, registry);
        let store = Arc::new(MemoryConfigStore::new(""));
        let manager = service(
            transport,
            Arc::clone(&store),
            Arc::new(FakeEnvironment::default()),
            Arc::new(FakeInput(None)),
            Arc::new(RecordingOutput::default()),
            temp.path(),
        );
        let plan = manager
            .custom_registry_plan(
                CustomRegistryRequest {
                    url: "https://registry.example.test/api.json".to_owned(),
                    api_key: None,
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            plan.providers
                .iter()
                .map(|provider| provider.id.as_str())
                .collect::<Vec<_>>(),
            ["custom-one", "custom-two"]
        );
        let error = manager
            .custom_registry_add(
                CustomRegistryRequest {
                    url: "https://registry.example.test/api.json".to_owned(),
                    api_key: None,
                },
                None,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("requires an API key"));
        assert!(store.source.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn custom_registry_add_uses_per_provider_environment_keys_and_stable_defaults() {
        let temp = TempDir::new().unwrap();
        let transport = Arc::new(FakeTransport::default());
        transport.response(
            200,
            serde_json::json!({
                "two": {
                    "id":"custom-two", "name":"Two", "type":"anthropic",
                    "api":"https://two.example.test/v1", "env":["TWO_KEY"],
                    "models":{"two":model("model-two", 64000)}
                },
                "one": {
                    "id":"custom-one", "name":"One", "type":"openai",
                    "api":"https://one.example.test/v1", "env":["ONE_KEY"],
                    "models":{"one":model("model-one", 32000)}
                }
            }),
        );
        let store = Arc::new(MemoryConfigStore::new(""));
        let environment = Arc::new(FakeEnvironment::default());
        environment.0.lock().unwrap().extend(BTreeMap::from([
            ("ONE_KEY".to_owned(), "one-secret".to_owned()),
            ("TWO_KEY".to_owned(), "two-secret".to_owned()),
        ]));
        let manager = service(
            transport,
            Arc::clone(&store),
            environment,
            Arc::new(FakeInput(None)),
            Arc::new(RecordingOutput::default()),
            temp.path(),
        );
        let result = manager
            .custom_registry_add(
                CustomRegistryRequest {
                    url: "https://registry.example.test/api.json".to_owned(),
                    api_key: None,
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            result.providers,
            ["custom-one".to_owned(), "custom-two".to_owned()]
        );
        assert_eq!(result.default_provider.as_deref(), Some("custom-one"));
        assert_eq!(result.default_model.as_deref(), Some("model-one"));
        let parsed = parse_config(&store.source.lock().unwrap()).unwrap();
        assert_eq!(
            parsed.providers["custom-one"].api_key.as_deref(),
            Some("one-secret")
        );
        assert_eq!(
            parsed.providers["custom-two"].api_key.as_deref(),
            Some("two-secret")
        );
    }

    #[tokio::test]
    async fn malformed_urls_and_configs_fail_without_secret_echo_or_transport() {
        let temp = TempDir::new().unwrap();
        let transport = Arc::new(FakeTransport::default());
        let store = Arc::new(MemoryConfigStore::new(
            "[providers.bad]\ntype = \"openai\"\napi_key = \"super-secret\n",
        ));
        let manager = service(
            Arc::clone(&transport),
            store,
            Arc::new(FakeEnvironment::default()),
            Arc::new(FakeInput(None)),
            Arc::new(RecordingOutput::default()),
            temp.path(),
        );
        let config_error = manager.list().unwrap_err().to_string();
        assert!(config_error.contains("invalid provider config"));
        assert!(!config_error.contains("super-secret"));

        let credential_url_manager = service(
            Arc::clone(&transport),
            Arc::new(MemoryConfigStore::new(
                r#"
[providers.bad]
type = "openai"
api_key = "key"
base_url = "https://user:url-secret@example.test/v1"
"#,
            )),
            Arc::new(FakeEnvironment::default()),
            Arc::new(FakeInput(None)),
            Arc::new(RecordingOutput::default()),
            temp.path(),
        );
        let base_url_error = credential_url_manager.list().unwrap_err().to_string();
        assert!(base_url_error.contains("configured provider base URL"));
        assert!(!base_url_error.contains("url-secret"));

        let url_error = manager
            .custom_registry_plan(
                CustomRegistryRequest {
                    url: "file:///tmp/api.json".to_owned(),
                    api_key: Some(SecretString::new("registry-secret")),
                },
                None,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(!url_error.contains("registry-secret"));
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn atomic_store_uses_private_mode_and_rejects_symlink_target() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("config.toml");
        let store = AtomicTomlConfigStore::new(&path);
        store.replace("default_model = \"x\"\n").unwrap();
        assert_eq!(store.load().unwrap(), "default_model = \"x\"\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::{symlink, PermissionsExt};
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            fs::remove_file(&path).unwrap();
            let other = temp.path().join("other.toml");
            fs::write(&other, "safe").unwrap();
            symlink(&other, &path).unwrap();
            assert!(store.replace("changed").is_err());
            assert_eq!(fs::read_to_string(other).unwrap(), "safe");
        }
    }

    #[test]
    fn unsupported_provider_ids_and_missing_credentials_fail_loudly() {
        let temp = TempDir::new().unwrap();
        let manager = service(
            Arc::new(FakeTransport::default()),
            Arc::new(MemoryConfigStore::new("")),
            Arc::new(FakeEnvironment::default()),
            Arc::new(FakeInput(None)),
            Arc::new(RecordingOutput::default()),
            temp.path(),
        );
        assert!(manager.remove("../escape").is_err());
        assert!(manager.resolve_api_key("missing", None, None, &[]).is_err());
    }
}
