use std::{
    collections::BTreeMap,
    env,
    ffi::OsStr,
    fmt,
    fs::{self, OpenOptions},
    future::Future,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use mycel_agent_protocol::{
    ChatProvider, ModelCapability, ProviderError, ProviderErrorKind, ProviderEventStream,
    ProviderRequest, ProviderRequestAuth, SecretString,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::Mutex,
};
use url::form_urlencoded;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::{
    error::{classify_http_error, connection_error, malformed_error},
    http::{collect_body, send_with_retry, HttpRequest, HttpTransport, RetryPolicy},
};

pub const KIMI_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
pub const KIMI_OAUTH_HOST: &str = "https://auth.kimi.com";
pub const KIMI_MANAGED_BASE_URL: &str = "https://api.kimi.com/coding/v1";
pub const CODEX_SUBSCRIPTION_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

pub type AuthFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ProviderRequestAuth, ProviderError>> + Send + 'a>>;

pub trait RequestAuthProvider: Send + Sync {
    fn request_auth<'a>(&'a self, force_refresh: bool) -> AuthFuture<'a>;
}

/// Resolve request-scoped auth and replay exactly once after an HTTP 401.
/// Token acquisition failures are returned directly rather than entering the
/// model-request retry policy.
pub async fn stream_with_refresh(
    provider: &dyn ChatProvider,
    auth_provider: &dyn RequestAuthProvider,
    request: &ProviderRequest,
) -> Result<ProviderEventStream, ProviderError> {
    let auth = auth_provider.request_auth(false).await?;
    match provider.stream(request, &auth).await {
        Err(error) if error.status_code == Some(401) => {
            let refreshed = auth_provider.request_auth(true).await?;
            provider.stream(request, &refreshed).await
        }
        result => result,
    }
}

#[derive(Clone, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct OAuthToken {
    access_token: String,
    refresh_token: String,
    expires_at: u64,
    expires_in: u64,
    scope: String,
    token_type: String,
}

impl fmt::Debug for OAuthToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthToken")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .field("token_type", &self.token_type)
            .finish()
    }
}

impl OAuthToken {
    pub fn access_token(&self) -> &str {
        &self.access_token
    }
    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
    }
    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }

    fn from_response(value: &Value, now: u64) -> Result<Self, ProviderError> {
        let access_token = required_string(value, "access_token")?;
        let refresh_token = required_string(value, "refresh_token")?;
        let expires_in = value
            .get("expires_in")
            .and_then(Value::as_u64)
            .filter(|value| *value > 0)
            .ok_or_else(|| malformed_error("OAuth token response has invalid expires_in"))?;
        Ok(Self {
            access_token,
            refresh_token,
            expires_at: now.saturating_add(expires_in),
            expires_in,
            scope: value
                .get("scope")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            token_type: value
                .get("token_type")
                .and_then(Value::as_str)
                .unwrap_or("Bearer")
                .into(),
        })
    }

    fn tombstone() -> Self {
        Self {
            access_token: String::new(),
            refresh_token: String::new(),
            expires_at: 0,
            expires_in: 0,
            scope: String::new(),
            token_type: "Bearer".into(),
        }
    }

    fn needs_refresh(&self, now: u64) -> bool {
        if self.expires_at == 0 {
            return false;
        }
        let threshold = 300_u64.max(self.expires_in / 2);
        self.expires_at <= now.saturating_add(threshold)
    }
}

#[derive(Clone, Debug)]
pub struct CredentialStore {
    directory: PathBuf,
}

impl CredentialStore {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    pub fn mycel_home(mycel_home: &Path) -> Self {
        Self::new(mycel_home.join("credentials"))
    }

    pub fn load(&self, name: &str) -> Result<Option<OAuthToken>, ProviderError> {
        let path = self.path_for(name)?;
        let mut data = String::new();
        match fs::File::open(path).and_then(|mut file| file.read_to_string(&mut data)) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(connection_error(format!(
                    "could not read OAuth credentials: {error}"
                )))
            }
        }
        serde_json::from_str(&data)
            .map(Some)
            .map_err(|error| malformed_error(format!("invalid OAuth credential file: {error}")))
    }

    pub fn save(&self, name: &str, token: &OAuthToken) -> Result<(), ProviderError> {
        ensure_private_directory(&self.directory)?;
        let target = self.path_for(name)?;
        let temp = target.with_extension(format!(
            "tmp.{}.{}",
            std::process::id(),
            hex::encode(crate::random::secure_random::<4>()?)
        ));
        let data = serde_json::to_vec_pretty(token)
            .map_err(|error| malformed_error(format!("could not encode OAuth token: {error}")))?;
        let result = (|| -> std::io::Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            file.write_all(&data)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            set_private_file(&temp)?;
            fs::rename(&temp, &target)?;
            if let Ok(directory) = fs::File::open(&self.directory) {
                let _ = directory.sync_all();
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_file(&temp);
            return Err(connection_error(format!(
                "could not save OAuth credentials: {error}"
            )));
        }
        Ok(())
    }

    pub fn remove(&self, name: &str) -> Result<(), ProviderError> {
        match fs::remove_file(self.path_for(name)?) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(connection_error(format!(
                "could not remove OAuth credentials: {error}"
            ))),
        }
    }

    fn path_for(&self, name: &str) -> Result<PathBuf, ProviderError> {
        let path = Path::new(name);
        if name.is_empty()
            || name.starts_with('.')
            || path.components().count() != 1
            || !matches!(path.components().next(), Some(Component::Normal(_)))
        {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                format!("invalid credential name {name:?}"),
            ));
        }
        Ok(self.directory.join(format!("{name}.json")))
    }
}

fn ensure_private_directory(path: &Path) -> Result<(), ProviderError> {
    fs::create_dir_all(path).map_err(|error| {
        connection_error(format!("could not create credential directory: {error}"))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
            connection_error(format!("could not secure credential directory: {error}"))
        })?;
    }
    Ok(())
}

fn set_private_file(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct KimiIdentity {
    pub headers: BTreeMap<String, String>,
}

impl KimiIdentity {
    pub fn load(mycel_home: &Path, version: &str) -> Result<Self, ProviderError> {
        ensure_private_directory(mycel_home)?;
        let device_id = stable_device_id(&mycel_home.join("device_id"))?;
        let hostname = ascii_header(
            env::var("HOSTNAME")
                .ok()
                .or_else(|| command_output("hostname", &[]))
                .unwrap_or_else(|| "unknown".into()),
        );
        let release = command_output("uname", &["-r"]).unwrap_or_else(|| env::consts::OS.into());
        let os_name = command_output("uname", &["-s"]).unwrap_or_else(|| env::consts::OS.into());
        let architecture = match env::consts::ARCH {
            "aarch64" => "arm64",
            architecture => architecture,
        };
        let product_version = if env::consts::OS == "macos" {
            command_output("/usr/bin/sw_vers", &["-productVersion"])
                .unwrap_or_else(|| release.clone())
        } else {
            release.clone()
        };
        let device_model = if env::consts::OS == "macos" {
            format!("macOS {product_version} {architecture}")
        } else {
            format!("{os_name} {release} {architecture}")
        };
        let version = required_ascii_header(version, "Kimi identity version")?;
        Ok(Self {
            headers: BTreeMap::from([
                ("X-Msh-Platform".into(), "kimi_code_cli".into()),
                ("X-Msh-Version".into(), version.clone()),
                ("X-Msh-Device-Name".into(), hostname),
                ("X-Msh-Device-Model".into(), ascii_header(device_model)),
                ("X-Msh-Os-Version".into(), ascii_header(release)),
                ("X-Msh-Device-Id".into(), device_id),
                ("User-Agent".into(), format!("mycel-cli/{version}")),
            ]),
        })
    }
}

fn stable_device_id(path: &Path) -> Result<String, ProviderError> {
    if let Ok(value) = fs::read_to_string(path) {
        let value = value.trim();
        if valid_uuid(value) {
            return Ok(value.into());
        }
    }
    let mut bytes = crate::random::secure_random::<16>()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let id = format!(
        "{}-{}-{}-{}-{}",
        hex::encode(&bytes[..4]),
        hex::encode(&bytes[4..6]),
        hex::encode(&bytes[6..8]),
        hex::encode(&bytes[8..10]),
        hex::encode(&bytes[10..])
    );
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| connection_error(format!("could not create Kimi device id: {error}")))?;
    file.write_all(id.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| connection_error(format!("could not persist Kimi device id: {error}")))?;
    set_private_file(path)
        .map_err(|error| connection_error(format!("could not secure Kimi device id: {error}")))?;
    Ok(id)
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.chars().enumerate().all(|(index, character)| {
            if [8, 13, 18, 23].contains(&index) {
                character == '-'
            } else {
                character.is_ascii_hexdigit()
            }
        })
}

fn ascii_header(value: String) -> String {
    let value = clean_ascii(&value);
    if value.is_empty() {
        "unknown".into()
    } else {
        value
    }
}

fn clean_ascii(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii() && !character.is_ascii_control())
        .collect::<String>()
        .trim()
        .to_owned()
}

fn required_ascii_header(value: &str, field: &str) -> Result<String, ProviderError> {
    let value = clean_ascii(value);
    if value.is_empty() {
        Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            format!("{field} must be non-empty ASCII"),
        ))
    } else {
        Ok(value)
    }
}

fn command_output(program: &str, arguments: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(arguments)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let output = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!output.is_empty()).then_some(output)
}

pub fn kimi_environment_custom_headers() -> BTreeMap<String, String> {
    parse_kimi_custom_headers(env::var("KIMI_CODE_CUSTOM_HEADERS").ok().as_deref())
}

pub fn parse_kimi_custom_headers(raw: Option<&str>) -> BTreeMap<String, String> {
    raw.into_iter()
        .flat_map(str::lines)
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            let name = name.trim();
            (!name.is_empty()).then(|| (name.to_owned(), value.trim().to_owned()))
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct KimiOAuthConfig {
    pub oauth_host: String,
    pub client_id: String,
    pub api_base_url: String,
    pub storage_name: String,
}

impl KimiOAuthConfig {
    pub fn standard() -> Self {
        Self {
            oauth_host: env::var("KIMI_CODE_OAUTH_HOST")
                .or_else(|_| env::var("KIMI_OAUTH_HOST"))
                .unwrap_or_else(|_| KIMI_OAUTH_HOST.into()),
            client_id: KIMI_CLIENT_ID.into(),
            api_base_url: KIMI_MANAGED_BASE_URL.into(),
            storage_name: "kimi-code".into(),
        }
    }

    pub fn scoped(mut self) -> Self {
        if self.oauth_host.trim_end_matches('/') != KIMI_OAUTH_HOST
            || self.api_base_url.trim_end_matches('/') != KIMI_MANAGED_BASE_URL
        {
            let oauth_host = self.oauth_host.trim_end_matches('/');
            let base_url = self.api_base_url.trim_end_matches('/');
            let source = format!(
                "{{\"oauthHost\":{},\"baseUrl\":{}}}",
                serde_json::to_string(oauth_host).expect("string serialization cannot fail"),
                serde_json::to_string(base_url).expect("string serialization cannot fail")
            );
            self.storage_name = format!(
                "kimi-code-env-{}",
                &hex::encode(Sha256::digest(source.as_bytes()))[..16]
            );
        }
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceAuthorization {
    pub user_code: String,
    pub device_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub expires_in: Option<u64>,
    pub interval_seconds: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DevicePoll {
    Pending {
        slow_down: bool,
        description: String,
    },
    Expired,
    Denied(String),
    Token,
}

#[derive(Clone)]
pub struct KimiOAuthClient {
    config: KimiOAuthConfig,
    identity: KimiIdentity,
    transport: Arc<dyn HttpTransport>,
}

impl KimiOAuthClient {
    pub fn new(
        config: KimiOAuthConfig,
        identity: KimiIdentity,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            config: config.scoped(),
            identity,
            transport,
        }
    }

    pub fn config(&self) -> &KimiOAuthConfig {
        &self.config
    }

    pub fn identity_headers(&self) -> &BTreeMap<String, String> {
        &self.identity.headers
    }

    pub async fn begin_device_authorization(&self) -> Result<DeviceAuthorization, ProviderError> {
        let (status, value) = self
            .post_form(
                "/api/oauth/device_authorization",
                &[("client_id", self.config.client_id.as_str())],
            )
            .await?;
        if status != 200 {
            return Err(oauth_status_error(status, &value));
        }
        Ok(DeviceAuthorization {
            user_code: required_string(&value, "user_code")?,
            device_code: required_string(&value, "device_code")?,
            verification_uri: value
                .get("verification_uri")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            verification_uri_complete: required_string(&value, "verification_uri_complete")?,
            expires_in: value.get("expires_in").and_then(Value::as_u64),
            interval_seconds: value
                .get("interval")
                .and_then(Value::as_u64)
                .unwrap_or(5)
                .max(1),
        })
    }

    pub async fn poll_once(
        &self,
        device_code: &str,
    ) -> Result<(DevicePoll, Option<OAuthToken>), ProviderError> {
        let (status, value) = self
            .post_form(
                "/api/oauth/token",
                &[
                    ("client_id", self.config.client_id.as_str()),
                    ("device_code", device_code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ],
            )
            .await?;
        if status == 200 {
            return Ok((
                DevicePoll::Token,
                Some(OAuthToken::from_response(&value, now_seconds())?),
            ));
        }
        if status >= 500 {
            return Err(oauth_status_error(status, &value));
        }
        let code = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown_error");
        let description = oauth_detail(&value);
        Ok(match code {
            "authorization_pending" => (
                DevicePoll::Pending {
                    slow_down: false,
                    description,
                },
                None,
            ),
            "slow_down" => (
                DevicePoll::Pending {
                    slow_down: true,
                    description,
                },
                None,
            ),
            "expired_token" => (DevicePoll::Expired, None),
            "access_denied" => (DevicePoll::Denied(description), None),
            _ => return Err(oauth_status_error(status, &value)),
        })
    }

    pub async fn poll_until_token(
        &self,
        authorization: &DeviceAuthorization,
    ) -> Result<OAuthToken, ProviderError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15 * 60);
        let mut interval = authorization.interval_seconds.max(1);
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(connection_error("Kimi device authorization timed out"));
            }
            let (state, token) = self.poll_once(&authorization.device_code).await?;
            match state {
                DevicePoll::Token => {
                    return token
                        .ok_or_else(|| malformed_error("successful OAuth poll has no token"))
                }
                DevicePoll::Pending { slow_down, .. } => {
                    if slow_down {
                        interval = interval.saturating_add(5);
                    }
                    tokio::time::sleep(Duration::from_secs(interval)).await;
                }
                DevicePoll::Expired => {
                    return Err(ProviderError::new(
                        ProviderErrorKind::Authentication,
                        "Kimi device code expired",
                    ))
                }
                DevicePoll::Denied(description) => {
                    return Err(ProviderError::new(
                        ProviderErrorKind::Authentication,
                        format!("Kimi authorization denied: {description}"),
                    ))
                }
            }
        }
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<OAuthToken, ProviderError> {
        let mut final_error = None;
        for attempt in 0..3 {
            match self
                .post_form(
                    "/api/oauth/token",
                    &[
                        ("client_id", self.config.client_id.as_str()),
                        ("grant_type", "refresh_token"),
                        ("refresh_token", refresh_token),
                    ],
                )
                .await
            {
                Ok((200, value)) => return OAuthToken::from_response(&value, now_seconds()),
                Ok((status, value))
                    if status == 401
                        || status == 403
                        || value.get("error").and_then(Value::as_str) == Some("invalid_grant") =>
                {
                    return Err(ProviderError::new(
                        ProviderErrorKind::Authentication,
                        format!("Kimi refresh token revoked: {}", oauth_detail(&value)),
                    ));
                }
                Ok((status, value)) => {
                    let error = oauth_status_error(status, &value);
                    if !matches!(status, 429 | 500 | 502 | 503 | 504) {
                        return Err(error);
                    }
                    final_error = Some(error);
                }
                Err(error) => final_error = Some(error),
            }
            if attempt < 2 {
                tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
            }
        }
        Err(final_error.unwrap_or_else(|| connection_error("Kimi token refresh failed")))
    }

    async fn post_form(
        &self,
        path: &str,
        fields: &[(&str, &str)],
    ) -> Result<(u16, Value), ProviderError> {
        let encoded = {
            let mut serializer = form_urlencoded::Serializer::new(String::new());
            for (name, value) in fields {
                serializer.append_pair(name, value);
            }
            serializer.finish()
        };
        let mut headers = self.identity.headers.clone();
        headers.insert(
            "content-type".into(),
            "application/x-www-form-urlencoded".into(),
        );
        headers.insert("accept".into(), "application/json".into());
        let response = self
            .transport
            .send(HttpRequest {
                method: "POST".into(),
                url: format!("{}{}", self.config.oauth_host.trim_end_matches('/'), path),
                headers,
                body: encoded.into_bytes(),
                timeout: Duration::from_secs(30),
            })
            .await
            .map_err(|error| connection_error(error.message))?;
        let status = response.status;
        let body = collect_body(response.body, 1_048_576).await?;
        let value = serde_json::from_slice(&body)
            .unwrap_or_else(|_| json_error(String::from_utf8_lossy(&body).into_owned()));
        Ok((status, value))
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ManagedKimiModel {
    pub id: String,
    pub context_length: u64,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub supports_reasoning: bool,
    #[serde(default)]
    pub supports_image_in: bool,
    #[serde(default)]
    pub supports_video_in: bool,
    #[serde(default = "default_true")]
    pub supports_tool_use: bool,
    #[serde(default, deserialize_with = "deserialize_kimi_thinking_type")]
    pub supports_thinking_type: Option<KimiThinkingType>,
    #[serde(default)]
    pub think_efforts: Option<KimiThinkEfforts>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum KimiThinkingType {
    Only,
    No,
    Both,
}

fn deserialize_kimi_thinking_type<'de, D>(
    deserializer: D,
) -> Result<Option<KimiThinkingType>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(match value.as_deref() {
        Some("only") => Some(KimiThinkingType::Only),
        Some("no") => Some(KimiThinkingType::No),
        Some("both") => Some(KimiThinkingType::Both),
        _ => None,
    })
}

#[derive(Clone, Debug, Deserialize)]
pub struct KimiThinkEfforts {
    #[serde(default)]
    pub support: bool,
    #[serde(default)]
    pub valid_efforts: Vec<String>,
    #[serde(default)]
    pub default_effort: Option<String>,
}

impl ManagedKimiModel {
    pub fn capability(&self) -> ModelCapability {
        let thinking = match self.supports_thinking_type {
            Some(KimiThinkingType::Only | KimiThinkingType::Both) => true,
            Some(KimiThinkingType::No) => false,
            None => self.supports_reasoning,
        };
        ModelCapability {
            image_in: self.supports_image_in,
            video_in: self.supports_video_in,
            audio_in: false,
            thinking,
            tool_use: self.supports_tool_use,
            max_context_tokens: self.context_length,
            dynamically_loaded_tools: Some(false),
        }
    }

    pub fn always_thinking(&self) -> bool {
        self.supports_thinking_type == Some(KimiThinkingType::Only)
    }

    pub fn valid_thinking_efforts(&self) -> &[String] {
        self.think_efforts
            .as_ref()
            .filter(|efforts| efforts.support)
            .map_or(&[], |efforts| efforts.valid_efforts.as_slice())
    }
}

fn default_true() -> bool {
    true
}

pub async fn discover_kimi_models(
    client: &KimiOAuthClient,
    auth: &ProviderRequestAuth,
) -> Result<Vec<ManagedKimiModel>, ProviderError> {
    let token = auth
        .api_key
        .as_ref()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Authentication,
                "Kimi access token is missing",
            )
        })?;
    let mut headers = client.identity.headers.clone();
    headers.insert("authorization".into(), format!("Bearer {}", token.expose()));
    headers.insert("accept".into(), "application/json".into());
    for (name, value) in &auth.headers {
        headers.insert(name.clone(), value.expose().into());
    }
    let response = send_with_retry(
        &client.transport,
        HttpRequest {
            method: "GET".into(),
            url: format!(
                "{}/models",
                client.config.api_base_url.trim_end_matches('/')
            ),
            headers,
            body: Vec::new(),
            timeout: Duration::from_secs(30),
        },
        RetryPolicy {
            max_attempts: 3,
            ..RetryPolicy::default()
        },
    )
    .await?;
    let body = collect_body(response.body, 4_194_304).await?;
    let value: Value = serde_json::from_slice(&body)
        .map_err(|error| malformed_error(format!("invalid Kimi model catalog: {error}")))?;
    let models: Vec<ManagedKimiModel> = serde_json::from_value(
        value
            .get("data")
            .cloned()
            .ok_or_else(|| malformed_error("Kimi model catalog is missing data"))?,
    )
    .map_err(|error| malformed_error(format!("invalid Kimi model catalog: {error}")))?;
    if models
        .iter()
        .any(|model| model.id.is_empty() || model.context_length == 0)
    {
        return Err(malformed_error(
            "Kimi model catalog contains an invalid model",
        ));
    }
    Ok(models)
}

pub struct KimiTokenProvider {
    oauth: KimiOAuthClient,
    store: CredentialStore,
    lock_directory: PathBuf,
    in_process: Mutex<()>,
}

impl KimiTokenProvider {
    pub fn new(oauth: KimiOAuthClient, store: CredentialStore, lock_directory: PathBuf) -> Self {
        Self {
            oauth,
            store,
            lock_directory,
            in_process: Mutex::new(()),
        }
    }

    pub fn save_login(&self, token: &OAuthToken) -> Result<(), ProviderError> {
        self.store.save(&self.oauth.config.storage_name, token)
    }

    pub fn logout(&self) -> Result<(), ProviderError> {
        self.store.remove(&self.oauth.config.storage_name)
    }

    async fn resolve(&self, force: bool) -> Result<OAuthToken, ProviderError> {
        // Read before the in-process gate. A concurrent waiter therefore
        // remembers the token that triggered its refresh request and can use
        // a token rotated by the leader instead of refreshing it again.
        let observed = self
            .store
            .load(&self.oauth.config.storage_name)?
            .filter(|token| !token.access_token.is_empty())
            .ok_or_else(|| {
                ProviderError::new(ProviderErrorKind::Authentication, "Kimi login is required")
            })?;
        let _guard = self.in_process.lock().await;
        let current = self
            .store
            .load(&self.oauth.config.storage_name)?
            .filter(|token| !token.access_token.is_empty())
            .ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "Kimi credentials were revoked by another process",
                )
            })?;
        if token_changed(&observed, &current) {
            return Ok(current);
        }
        if !force && !current.needs_refresh(now_seconds()) {
            return Ok(current);
        }
        let lock_path = self
            .lock_directory
            .join(format!("{}.lock", self.oauth.config.storage_name));
        let _file_lock = FileRefreshLock::acquire(
            lock_path,
            120,
            Duration::from_millis(500),
            Duration::from_secs(5),
        )
        .await?;
        let active = self
            .store
            .load(&self.oauth.config.storage_name)?
            .unwrap_or_else(|| current.clone());
        if token_changed(&current, &active) {
            if !active.access_token.is_empty() {
                return Ok(active);
            }
            return Err(ProviderError::new(
                ProviderErrorKind::Authentication,
                "Kimi credentials were revoked by another process",
            ));
        }
        if !force && !active.needs_refresh(now_seconds()) {
            return Ok(active);
        }
        if active.refresh_token.is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::Authentication,
                "Kimi refresh token is unavailable",
            ));
        }
        match self.oauth.refresh(&active.refresh_token).await {
            Ok(refreshed) => {
                self.store
                    .save(&self.oauth.config.storage_name, &refreshed)?;
                Ok(refreshed)
            }
            Err(error) if error.kind == ProviderErrorKind::Authentication => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if let Some(peer) = self.store.load(&self.oauth.config.storage_name)? {
                    if peer.refresh_token != active.refresh_token && !peer.access_token.is_empty() {
                        return Ok(peer);
                    }
                }
                self.store
                    .save(&self.oauth.config.storage_name, &OAuthToken::tombstone())?;
                Err(error)
            }
            Err(error) => Err(error),
        }
    }
}

fn token_changed(before: &OAuthToken, after: &OAuthToken) -> bool {
    before.refresh_token != after.refresh_token
        || before.access_token != after.access_token
        || before.expires_at != after.expires_at
}

impl RequestAuthProvider for KimiTokenProvider {
    fn request_auth<'a>(&'a self, force_refresh: bool) -> AuthFuture<'a> {
        Box::pin(async move {
            let token = self.resolve(force_refresh).await?;
            Ok(ProviderRequestAuth {
                api_key: Some(SecretString::new(token.access_token.clone())),
                headers: BTreeMap::new(),
            })
        })
    }
}

struct FileRefreshLock {
    path: PathBuf,
}

impl FileRefreshLock {
    async fn acquire(
        path: PathBuf,
        attempts: u32,
        delay: Duration,
        stale: Duration,
    ) -> Result<Self, ProviderError> {
        if let Some(parent) = path.parent() {
            ensure_private_directory(parent)?;
        }
        for attempt in 0..attempts.max(1) {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(mut file) => {
                    if let Err(error) = set_private_file(&path) {
                        let _ = fs::remove_file(&path);
                        return Err(connection_error(format!(
                            "could not secure OAuth refresh lock: {error}"
                        )));
                    }
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale_lock = fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|elapsed| elapsed >= stale);
                    if stale_lock {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    if attempt + 1 < attempts {
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(connection_error(format!(
                        "could not acquire OAuth refresh lock {}",
                        path.display()
                    )));
                }
                Err(error) => {
                    return Err(connection_error(format!(
                        "could not create OAuth refresh lock: {error}"
                    )))
                }
            }
        }
        Err(connection_error("could not acquire OAuth refresh lock"))
    }
}

impl Drop for FileRefreshLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Clone, Debug)]
pub struct CodexAuthStatus {
    pub auth_method: Option<String>,
    pub auth_token: Option<String>,
    pub requires_openai_auth: Option<bool>,
}

pub type CodexStatusFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CodexAuthStatus, ProviderError>> + Send + 'a>>;
pub type CodexVersionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<String, ProviderError>> + Send + 'a>>;

pub trait CodexStatusSource: Send + Sync {
    fn read<'a>(&'a self, force_refresh: bool) -> CodexStatusFuture<'a>;
    fn version<'a>(&'a self) -> CodexVersionFuture<'a>;
}

#[derive(Clone, Debug)]
pub struct ProcessCodexStatusSource {
    executable: PathBuf,
}

impl ProcessCodexStatusSource {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
        }
    }
}

impl CodexStatusSource for ProcessCodexStatusSource {
    fn read<'a>(&'a self, force_refresh: bool) -> CodexStatusFuture<'a> {
        Box::pin(async move { read_codex_status(self.executable.as_os_str(), force_refresh).await })
    }

    fn version<'a>(&'a self) -> CodexVersionFuture<'a> {
        Box::pin(async move {
            let output = Command::new(&self.executable)
                .arg("--version")
                .output()
                .await
                .map_err(|error| {
                    connection_error(format!("could not run codex --version: {error}"))
                })?;
            if !output.status.success() {
                return Err(connection_error("codex --version failed"));
            }
            let text = String::from_utf8_lossy(&output.stdout);
            let version = text
                .split_whitespace()
                .find(|word| word.chars().next().is_some_and(|c| c.is_ascii_digit()))
                .ok_or_else(|| malformed_error("could not parse Codex version"))?;
            Ok(version.into())
        })
    }
}

async fn read_codex_status(
    executable: &OsStr,
    force_refresh: bool,
) -> Result<CodexAuthStatus, ProviderError> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut command = Command::new(executable);
        command
            .args(["app-server", "--stdio"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| connection_error(format!("could not start Codex app-server: {error}")))?;
        let mut stdin = child.stdin.take().ok_or_else(|| connection_error("Codex app-server stdin unavailable"))?;
        let stdout = child.stdout.take().ok_or_else(|| connection_error("Codex app-server stdout unavailable"))?;
        let mut lines = BufReader::new(stdout).lines();
        write_json_line(&mut stdin, &serde_json::json!({"method":"initialize","id":1,"params":{"clientInfo":{"name":"mycel","title":"Mycel","version":"1"}}})).await?;
        read_rpc_id(&mut lines, 1).await?;
        write_json_line(&mut stdin, &serde_json::json!({"method":"initialized"})).await?;
        write_json_line(&mut stdin, &serde_json::json!({"method":"getAuthStatus","id":2,"params":{"includeToken":true,"refreshToken":force_refresh}})).await?;
        let result = read_rpc_id(&mut lines, 2).await?;
        let _ = child.kill().await;
        let result = result.get("result").ok_or_else(|| malformed_error("Codex app-server auth response is missing result"))?;
        Ok(CodexAuthStatus {
            auth_method: result.get("authMethod").and_then(Value::as_str).map(str::to_owned),
            auth_token: result.get("authToken").and_then(Value::as_str).map(str::to_owned),
            requires_openai_auth: result.get("requiresOpenaiAuth").and_then(Value::as_bool),
        })
    }).await.map_err(|_| connection_error("timed out waiting for Codex app-server auth"))?
}

async fn write_json_line(
    stdin: &mut tokio::process::ChildStdin,
    value: &Value,
) -> Result<(), ProviderError> {
    let mut data = serde_json::to_vec(value).map_err(|error| malformed_error(error.to_string()))?;
    data.push(b'\n');
    stdin
        .write_all(&data)
        .await
        .map_err(|error| connection_error(format!("could not write Codex RPC: {error}")))
}

async fn read_rpc_id(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    id: u64,
) -> Result<Value, ProviderError> {
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|error| connection_error(format!("could not read Codex RPC: {error}")))?
    {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if value.get("error").is_some() {
            return Err(connection_error(if id == 2 {
                "Codex app-server rejected deprecated getAuthStatus token export"
            } else {
                "Codex app-server rejected initialization"
            }));
        }
        return Ok(value);
    }
    Err(connection_error(
        "Codex app-server exited before returning auth",
    ))
}

#[derive(Clone)]
struct CachedCodexAuth {
    token: String,
    account_id: String,
    fedramp: bool,
    expires_at: Option<u64>,
    fetched_at: u64,
}

pub struct CodexSubscriptionAuth {
    source: Arc<dyn CodexStatusSource>,
    cache: Mutex<Option<CachedCodexAuth>>,
    refresh_generation: AtomicU64,
}

impl CodexSubscriptionAuth {
    pub fn new(source: Arc<dyn CodexStatusSource>) -> Self {
        Self {
            source,
            cache: Mutex::new(None),
            refresh_generation: AtomicU64::new(0),
        }
    }

    async fn resolve(&self, force: bool) -> Result<CachedCodexAuth, ProviderError> {
        let observed_generation = self.refresh_generation.load(Ordering::SeqCst);
        let mut cache = self.cache.lock().await;
        let now = now_seconds();
        if force && self.refresh_generation.load(Ordering::SeqCst) != observed_generation {
            if let Some(cached) = cache.as_ref() {
                return Ok(cached.clone());
            }
        }
        if !force {
            if let Some(cached) = cache.as_ref().filter(|cached| {
                now.saturating_sub(cached.fetched_at) < 30
                    && cached
                        .expires_at
                        .is_none_or(|expiry| expiry > now.saturating_add(300))
            }) {
                return Ok(cached.clone());
            }
        }
        let status = self.source.read(force).await?;
        if status.requires_openai_auth == Some(false)
            || status.auth_method.as_deref() != Some("chatgpt")
        {
            return Err(ProviderError::new(
                ProviderErrorKind::Authentication,
                "Codex is not logged in with ChatGPT",
            ));
        }
        let token = status
            .auth_token
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "Codex did not return an auth token",
                )
            })?;
        let claims = decode_codex_claims(&token)?;
        let auth = CachedCodexAuth {
            token,
            account_id: claims.0,
            fedramp: claims.1,
            expires_at: claims.2,
            fetched_at: now,
        };
        *cache = Some(auth.clone());
        self.refresh_generation.fetch_add(1, Ordering::SeqCst);
        Ok(auth)
    }
}

impl RequestAuthProvider for CodexSubscriptionAuth {
    fn request_auth<'a>(&'a self, force_refresh: bool) -> AuthFuture<'a> {
        Box::pin(async move {
            let (auth, version) =
                tokio::try_join!(self.resolve(force_refresh), self.source.version())?;
            let mut headers = BTreeMap::from([
                (
                    "ChatGPT-Account-ID".into(),
                    SecretString::new(auth.account_id),
                ),
                ("originator".into(), SecretString::new("mycel")),
                ("version".into(), SecretString::new(version)),
            ]);
            if auth.fedramp {
                headers.insert("X-OpenAI-Fedramp".into(), SecretString::new("true"));
            }
            Ok(ProviderRequestAuth {
                api_key: Some(SecretString::new(auth.token)),
                headers,
            })
        })
    }
}

fn decode_codex_claims(token: &str) -> Result<(String, bool, Option<u64>), ProviderError> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| malformed_error("Codex auth token is not a JWT"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|error| malformed_error(format!("invalid Codex JWT payload: {error}")))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| malformed_error(format!("invalid Codex JWT claims: {error}")))?;
    let auth = value
        .get("https://api.openai.com/auth")
        .ok_or_else(|| malformed_error("Codex JWT is missing OpenAI auth claims"))?;
    let account = required_string(auth, "chatgpt_account_id")?;
    let fedramp = auth
        .get("chatgpt_account_is_fedramp")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok((account, fedramp, value.get("exp").and_then(Value::as_u64)))
}

pub fn is_codex_subscription_base_url(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str() == Some("chatgpt.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path().trim_end_matches('/') == "/backend-api/codex"
        && url.query().is_none()
        && url.fragment().is_none()
}

fn required_string(value: &Value, field: &str) -> Result<String, ProviderError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| malformed_error(format!("response is missing {field}")))
}

fn oauth_detail(value: &Value) -> String {
    value
        .get("error_description")
        .or_else(|| value.pointer("/error/message"))
        .or_else(|| value.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("OAuth request failed")
        .into()
}

fn oauth_status_error(status: u16, value: &Value) -> ProviderError {
    classify_http_error(
        status,
        &BTreeMap::new(),
        serde_json::to_string(value).unwrap_or_default().as_bytes(),
    )
}

fn json_error(message: String) -> Value {
    serde_json::json!({"error":{"message":message}})
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex as StdMutex,
        },
    };

    use bytes::Bytes;
    use futures_util::{stream, FutureExt, StreamExt};
    use mycel_agent_protocol::{OptionalNullable, ProviderFuture, ProviderStreamEvent};
    use tempfile::TempDir;
    use tokio::sync::{Barrier, Notify};

    use super::*;
    use crate::http::{ByteStream, HttpResponse, TransportFuture};

    #[derive(Default)]
    struct FakeTransport {
        requests: StdMutex<Vec<HttpRequest>>,
        responses: StdMutex<VecDeque<(u16, Value)>>,
    }

    impl FakeTransport {
        fn response(&self, status: u16, value: Value) {
            self.responses
                .lock()
                .expect("responses")
                .push_back((status, value));
        }
    }

    impl HttpTransport for FakeTransport {
        fn send<'a>(&'a self, request: HttpRequest) -> TransportFuture<'a> {
            self.requests.lock().expect("requests").push(request);
            let (status, value) = self
                .responses
                .lock()
                .expect("responses")
                .pop_front()
                .expect("fixture response");
            async move {
                let body: ByteStream = Box::pin(stream::iter([Ok(Bytes::from(
                    serde_json::to_vec(&value).expect("json"),
                ))]));
                Ok(HttpResponse {
                    status,
                    headers: BTreeMap::new(),
                    body,
                })
            }
            .boxed()
        }
    }

    #[test]
    fn credential_store_is_atomic_private_and_rejects_traversal() {
        let temp = TempDir::new().expect("temp");
        let store = CredentialStore::new(temp.path().join("credentials"));
        let token = OAuthToken {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: 9,
            expires_in: 8,
            scope: String::new(),
            token_type: "Bearer".into(),
        };
        store.save("kimi-code", &token).expect("save");
        assert_eq!(
            store
                .load("kimi-code")
                .expect("load")
                .expect("token")
                .access_token(),
            "a"
        );
        assert!(store.save("../escape", &token).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(temp.path().join("credentials"))
                    .expect("directory metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(temp.path().join("credentials/kimi-code.json"))
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        store.remove("kimi-code").expect("logout");
        assert!(store
            .load("kimi-code")
            .expect("load after logout")
            .is_none());
        fs::write(temp.path().join("credentials/kimi-code.json"), b"not-json")
            .expect("corrupt fixture");
        assert_eq!(
            store
                .load("kimi-code")
                .expect_err("corrupt credentials")
                .kind,
            ProviderErrorKind::MalformedResponse
        );
    }

    #[test]
    fn custom_kimi_hosts_use_the_stable_credential_scope() {
        let config = KimiOAuthConfig {
            oauth_host: "https://auth.test/".into(),
            client_id: KIMI_CLIENT_ID.into(),
            api_base_url: "https://api.test/v1/".into(),
            storage_name: "kimi-code".into(),
        }
        .scoped();
        assert_eq!(config.storage_name, "kimi-code-env-0e66b80ad983099f");
    }

    #[tokio::test]
    async fn kimi_device_and_refresh_wire_are_form_encoded() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(FakeTransport::default());
        transport.response(200, serde_json::json!({"user_code":"U","device_code":"D","verification_uri_complete":"https://verify","interval":1}));
        transport.response(
            400,
            serde_json::json!({"error":"authorization_pending","error_description":"wait"}),
        );
        transport.response(
            200,
            serde_json::json!({"access_token":"a","refresh_token":"r2","expires_in":3600}),
        );
        let identity = KimiIdentity::load(temp.path(), "1.0").expect("identity");
        let client = KimiOAuthClient::new(
            KimiOAuthConfig {
                oauth_host: "https://auth.test".into(),
                client_id: KIMI_CLIENT_ID.into(),
                api_base_url: "https://api.test/v1".into(),
                storage_name: "test".into(),
            },
            identity,
            transport.clone(),
        );
        let auth = client
            .begin_device_authorization()
            .await
            .expect("device auth");
        assert_eq!(auth.device_code, "D");
        let (poll, polled_token) = client.poll_once("D").await.expect("poll");
        assert_eq!(
            poll,
            DevicePoll::Pending {
                slow_down: false,
                description: "wait".into(),
            }
        );
        assert!(polled_token.is_none());
        let token = client.refresh("old").await.expect("refresh");
        assert_eq!(token.refresh_token(), "r2");
        let requests = transport.requests.lock().expect("requests");
        assert_eq!(
            requests[0].url,
            "https://auth.test/api/oauth/device_authorization"
        );
        assert_eq!(
            String::from_utf8_lossy(&requests[1].body),
            format!("client_id={KIMI_CLIENT_ID}&device_code=D&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code")
        );
        assert_eq!(
            String::from_utf8_lossy(&requests[2].body),
            format!("client_id={KIMI_CLIENT_ID}&grant_type=refresh_token&refresh_token=old")
        );
        assert_eq!(requests[0].headers["X-Msh-Platform"], "kimi_code_cli");
        assert_eq!(requests[0].headers["User-Agent"], "mycel-cli/1.0");
    }

    #[tokio::test]
    async fn managed_kimi_model_discovery_maps_runtime_capabilities() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(FakeTransport::default());
        transport.response(
            200,
            serde_json::json!({"data":[{
                "id":"kimi-k2","context_length":131072,"supports_reasoning":true,
                "supports_image_in":true,"supports_video_in":false,"supports_tool_use":true,
                "supports_thinking_type":"only",
                "think_efforts":{"support":true,"valid_efforts":["low","high"],"default_effort":"high"}
            }]}),
        );
        let identity = KimiIdentity::load(temp.path(), "1.0").expect("identity");
        let client = KimiOAuthClient::new(
            KimiOAuthConfig {
                oauth_host: "https://auth.test".into(),
                client_id: KIMI_CLIENT_ID.into(),
                api_base_url: "https://api.test/v1".into(),
                storage_name: "test".into(),
            },
            identity,
            transport.clone(),
        );
        let models = discover_kimi_models(
            &client,
            &ProviderRequestAuth {
                api_key: Some(SecretString::new("access")),
                headers: BTreeMap::new(),
            },
        )
        .await
        .expect("models");
        assert_eq!(models[0].id, "kimi-k2");
        assert_eq!(models[0].capability().max_context_tokens, 131_072);
        assert!(models[0].capability().thinking);
        assert!(models[0].always_thinking());
        assert_eq!(models[0].valid_thinking_efforts(), ["low", "high"]);
        let requests = transport.requests.lock().expect("requests");
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].url, "https://api.test/v1/models");
        assert_eq!(requests[0].headers["authorization"], "Bearer access");
        assert_eq!(requests[0].headers["accept"], "application/json");
        assert_eq!(requests[0].headers["X-Msh-Platform"], "kimi_code_cli");
        assert!(requests[0].body.is_empty());
    }

    #[test]
    fn managed_kimi_capability_edges_preserve_catalog_semantics() {
        let models: Vec<ManagedKimiModel> = serde_json::from_value(serde_json::json!([
            {"id":"only","context_length":1,"supports_reasoning":false,"supports_thinking_type":"only"},
            {"id":"both","context_length":2,"supports_thinking_type":"both","supports_tool_use":false},
            {"id":"no","context_length":3,"supports_reasoning":true,"supports_thinking_type":"no"},
            {"id":"legacy","context_length":4,"supports_reasoning":true,"supports_thinking_type":"future","supports_image_in":true,"supports_video_in":true,
             "think_efforts":{"support":false,"valid_efforts":["low"],"default_effort":"low"}}
        ])).expect("catalog models");
        assert!(models[0].capability().thinking);
        assert!(models[0].always_thinking());
        assert!(models[0].capability().tool_use);
        assert!(models[1].capability().thinking);
        assert!(!models[1].always_thinking());
        assert!(!models[1].capability().tool_use);
        assert!(!models[2].capability().thinking);
        let legacy = models[3].capability();
        assert!(legacy.thinking && legacy.image_in && legacy.video_in);
        assert!(models[3].valid_thinking_efforts().is_empty());
    }

    struct BlockingRefreshTransport {
        attempts: AtomicUsize,
        entered: Notify,
        release: Notify,
        status: u16,
        value: Value,
    }

    impl HttpTransport for BlockingRefreshTransport {
        fn send<'a>(&'a self, request: HttpRequest) -> TransportFuture<'a> {
            assert_eq!(request.method, "POST");
            assert!(request.url.ends_with("/api/oauth/token"));
            assert!(String::from_utf8_lossy(&request.body).contains("refresh_token=old"));
            self.attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                self.entered.notify_waiters();
                self.release.notified().await;
                let body: ByteStream = Box::pin(stream::iter([Ok(Bytes::from(
                    serde_json::to_vec(&self.value).expect("json"),
                ))]));
                Ok(HttpResponse {
                    status: self.status,
                    headers: BTreeMap::new(),
                    body,
                })
            }
            .boxed()
        }
    }

    fn saved_oauth_token(access: &str, refresh: &str) -> OAuthToken {
        OAuthToken {
            access_token: access.into(),
            refresh_token: refresh.into(),
            expires_at: 1,
            expires_in: 1,
            scope: String::new(),
            token_type: "Bearer".into(),
        }
    }

    fn kimi_token_provider(temp: &TempDir, transport: Arc<dyn HttpTransport>) -> KimiTokenProvider {
        let identity = KimiIdentity::load(temp.path(), "1.0").expect("identity");
        let oauth = KimiOAuthClient::new(
            KimiOAuthConfig {
                oauth_host: KIMI_OAUTH_HOST.into(),
                client_id: KIMI_CLIENT_ID.into(),
                api_base_url: KIMI_MANAGED_BASE_URL.into(),
                storage_name: "kimi-code".into(),
            },
            identity,
            transport,
        );
        KimiTokenProvider::new(
            oauth,
            CredentialStore::new(temp.path().join("credentials")),
            temp.path().join("locks"),
        )
    }

    #[tokio::test]
    async fn concurrent_forced_kimi_refresh_rotates_once() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(BlockingRefreshTransport {
            attempts: AtomicUsize::new(0),
            entered: Notify::new(),
            release: Notify::new(),
            status: 200,
            value: serde_json::json!({
                "access_token":"fresh","refresh_token":"rotated","expires_in":3600
            }),
        });
        let provider = Arc::new(kimi_token_provider(&temp, transport.clone()));
        provider
            .save_login(&saved_oauth_token("stale", "old"))
            .expect("save old token");

        let gate = Arc::new(Barrier::new(3));
        let entered = transport.entered.notified();
        tokio::pin!(entered);
        let first = {
            let provider = provider.clone();
            let gate = gate.clone();
            tokio::spawn(async move {
                gate.wait().await;
                provider.request_auth(true).await
            })
        };
        let second = {
            let provider = provider.clone();
            let gate = gate.clone();
            tokio::spawn(async move {
                gate.wait().await;
                provider.request_auth(true).await
            })
        };
        gate.wait().await;
        entered.await;
        tokio::task::yield_now().await;
        transport.release.notify_waiters();

        for result in [
            first.await.expect("first task"),
            second.await.expect("second task"),
        ] {
            assert_eq!(
                result.expect("auth").api_key.expect("token").expose(),
                "fresh"
            );
        }
        assert_eq!(transport.attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn revoked_kimi_refresh_is_tombstoned_and_unlocks() {
        let temp = TempDir::new().expect("temp");
        let transport = Arc::new(FakeTransport::default());
        transport.response(401, serde_json::json!({"error":"invalid_grant"}));
        let provider = kimi_token_provider(&temp, transport);
        provider
            .save_login(&saved_oauth_token("stale", "old"))
            .expect("save old token");
        let error = provider.request_auth(true).await.expect_err("revoked");
        assert_eq!(error.kind, ProviderErrorKind::Authentication);
        let tombstone = provider
            .store
            .load(&provider.oauth.config.storage_name)
            .expect("load tombstone")
            .expect("tombstone");
        assert!(tombstone.access_token().is_empty());
        assert!(tombstone.refresh_token().is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(
                    temp.path()
                        .join("credentials")
                        .join(format!("{}.json", provider.oauth.config.storage_name)),
                )
                .expect("tombstone metadata")
                .permissions()
                .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(!temp
            .path()
            .join("locks")
            .join(format!("{}.lock", provider.oauth.config.storage_name))
            .exists());
    }

    #[tokio::test]
    async fn active_refresh_lock_fails_closed_without_deleting_peer_lock() {
        let temp = TempDir::new().expect("temp");
        let path = temp.path().join("locks/kimi-code.lock");
        fs::create_dir_all(path.parent().expect("parent")).expect("lock directory");
        fs::write(&path, b"peer").expect("peer lock");
        let error = match FileRefreshLock::acquire(
            path.clone(),
            1,
            Duration::ZERO,
            Duration::from_secs(3600),
        )
        .await
        {
            Ok(_) => panic!("active peer lock must not be stolen"),
            Err(error) => error,
        };
        assert_eq!(error.kind, ProviderErrorKind::Connection);
        assert_eq!(fs::read(&path).expect("peer lock remains"), b"peer");
    }

    #[tokio::test]
    async fn refresh_lock_is_private_and_removed_on_drop() {
        let temp = TempDir::new().expect("temp");
        let path = temp.path().join("locks/kimi-code.lock");
        let lock =
            FileRefreshLock::acquire(path.clone(), 1, Duration::ZERO, Duration::from_secs(3600))
                .await
                .expect("lock");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path)
                    .expect("lock metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        drop(lock);
        assert!(!path.exists());
    }

    struct RotatingAuth {
        forced: AtomicUsize,
    }

    impl RequestAuthProvider for RotatingAuth {
        fn request_auth<'a>(&'a self, force_refresh: bool) -> AuthFuture<'a> {
            Box::pin(async move {
                if force_refresh {
                    self.forced.fetch_add(1, Ordering::SeqCst);
                }
                Ok(ProviderRequestAuth {
                    api_key: Some(SecretString::new(if force_refresh {
                        "fresh"
                    } else {
                        "stale"
                    })),
                    headers: BTreeMap::new(),
                })
            })
        }
    }

    struct AuthCheckingProvider;

    impl ChatProvider for AuthCheckingProvider {
        fn name(&self) -> &str {
            "test"
        }
        fn model(&self) -> &str {
            "test-model"
        }
        fn stream<'a>(
            &'a self,
            _request: &'a ProviderRequest,
            auth: &'a ProviderRequestAuth,
        ) -> ProviderFuture<'a> {
            Box::pin(async move {
                if auth.api_key.as_ref().map(SecretString::expose) != Some("fresh") {
                    return Err(ProviderError {
                        kind: ProviderErrorKind::Authentication,
                        message: "unauthorized".into(),
                        retryable: false,
                        status_code: Some(401),
                        retry_after_ms: None,
                    });
                }
                Ok(Box::pin(stream::iter([
                    Ok(ProviderStreamEvent::ResponseStart {
                        id: Some("r".into()),
                        trace_id: OptionalNullable::Missing,
                    }),
                    Ok(ProviderStreamEvent::ResponseEnd),
                ])) as ProviderEventStream)
            })
        }
    }

    #[tokio::test]
    async fn request_auth_replays_exactly_once_after_401() {
        let auth = RotatingAuth {
            forced: AtomicUsize::new(0),
        };
        let request = ProviderRequest {
            provider: "test".into(),
            model: "test-model".into(),
            system_prompt: String::new(),
            tools: Vec::new(),
            history: Vec::new(),
            thinking_effort: None,
            max_completion_tokens: None,
            response_format: None,
            metadata: BTreeMap::new(),
        };
        let mut events = stream_with_refresh(&AuthCheckingProvider, &auth, &request)
            .await
            .expect("stream");
        assert!(events.next().await.is_some());
        assert_eq!(auth.forced.load(Ordering::SeqCst), 1);
    }

    struct FakeCodex {
        status: CodexAuthStatus,
        version: String,
    }
    impl CodexStatusSource for FakeCodex {
        fn read<'a>(&'a self, _force_refresh: bool) -> CodexStatusFuture<'a> {
            async move { Ok(self.status.clone()) }.boxed()
        }
        fn version<'a>(&'a self) -> CodexVersionFuture<'a> {
            async move { Ok(self.version.clone()) }.boxed()
        }
    }

    fn codex_test_token(account: &str) -> String {
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&serde_json::json!({
                "https://api.openai.com/auth":{"chatgpt_account_id":account},
                "exp":now_seconds()+3600
            }))
            .expect("json"),
        );
        format!("x.{payload}.y")
    }

    struct BlockingCodex {
        status: CodexAuthStatus,
        reads: AtomicUsize,
        entered: Notify,
        release: Notify,
    }

    impl CodexStatusSource for BlockingCodex {
        fn read<'a>(&'a self, force_refresh: bool) -> CodexStatusFuture<'a> {
            assert!(force_refresh);
            let read = self.reads.fetch_add(1, Ordering::SeqCst);
            async move {
                if read == 0 {
                    self.entered.notify_waiters();
                    self.release.notified().await;
                }
                Ok(self.status.clone())
            }
            .boxed()
        }

        fn version<'a>(&'a self) -> CodexVersionFuture<'a> {
            async move { Ok("1.2.3".into()) }.boxed()
        }
    }

    #[tokio::test]
    async fn codex_auth_extracts_account_and_fedramp_headers() {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&serde_json::json!({
            "https://api.openai.com/auth":{"chatgpt_account_id":"acct","chatgpt_account_is_fedramp":true},
            "exp":now_seconds()+3600
        })).expect("json"));
        let token = format!("x.{payload}.y");
        let auth = CodexSubscriptionAuth::new(Arc::new(FakeCodex {
            status: CodexAuthStatus {
                auth_method: Some("chatgpt".into()),
                auth_token: Some(token.clone()),
                requires_openai_auth: Some(true),
            },
            version: "1.2.3".into(),
        }));
        let request = auth.request_auth(false).await.expect("auth");
        assert_eq!(request.api_key.expect("token").expose(), token);
        assert_eq!(request.headers["ChatGPT-Account-ID"].expose(), "acct");
        assert_eq!(request.headers["X-OpenAI-Fedramp"].expose(), "true");
    }

    #[tokio::test]
    async fn concurrent_forced_codex_refresh_reads_app_server_once() {
        let source = Arc::new(BlockingCodex {
            status: CodexAuthStatus {
                auth_method: Some("chatgpt".into()),
                auth_token: Some(codex_test_token("acct-race")),
                requires_openai_auth: Some(true),
            },
            reads: AtomicUsize::new(0),
            entered: Notify::new(),
            release: Notify::new(),
        });
        let auth = Arc::new(CodexSubscriptionAuth::new(source.clone()));
        let gate = Arc::new(Barrier::new(3));
        let entered = source.entered.notified();
        tokio::pin!(entered);
        let first = {
            let auth = auth.clone();
            let gate = gate.clone();
            tokio::spawn(async move {
                gate.wait().await;
                auth.request_auth(true).await
            })
        };
        let second = {
            let auth = auth.clone();
            let gate = gate.clone();
            tokio::spawn(async move {
                gate.wait().await;
                auth.request_auth(true).await
            })
        };
        gate.wait().await;
        entered.await;
        tokio::task::yield_now().await;
        source.release.notify_waiters();

        for result in [
            first.await.expect("first task"),
            second.await.expect("second task"),
        ] {
            assert_eq!(
                result.expect("auth").headers["ChatGPT-Account-ID"].expose(),
                "acct-race"
            );
        }
        assert_eq!(source.reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn codex_base_url_is_strict() {
        assert!(is_codex_subscription_base_url(CODEX_SUBSCRIPTION_BASE_URL));
        assert!(!is_codex_subscription_base_url(
            "https://chatgpt.com/backend-api/codex?x=1"
        ));
        assert!(!is_codex_subscription_base_url(
            "https://example.com/backend-api/codex"
        ));
    }

    #[test]
    fn kimi_custom_headers_skip_malformed_lines_and_trim_values() {
        let headers =
            parse_kimi_custom_headers(Some(" X-Test : one\ninvalid\n: empty\nX-Two: two:three"));
        assert_eq!(headers["X-Test"], "one");
        assert_eq!(headers["X-Two"], "two:three");
        assert_eq!(headers.len(), 2);
    }
}
