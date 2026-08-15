//! OAuth 2.1 support for explicitly configured Streamable HTTP MCP servers.
//!
//! This module intentionally contains no marketplace, hosted broker, or
//! vendor-specific behavior. Discovery, browser presentation, loopback
//! callbacks, HTTP, time, entropy, and persistence are injectable so the
//! authorization boundary is deterministic in tests.

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    fs::{self, OpenOptions},
    future::Future,
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    pin::Pin,
    process::Command,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use futures_util::StreamExt;
use mycel_agent_protocol::SecretString;
use mycel_agent_runtime::CancellationToken;
use ring::{
    hmac,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use url::{form_urlencoded, Host, Url};
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

const MAX_OAUTH_DOCUMENT_BYTES: usize = 1024 * 1024;
const MAX_CALLBACK_BYTES: usize = 16 * 1024;
const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
const EXPIRY_SKEW_SECONDS: u64 = 60;

pub type McpOAuthFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpOAuthError {
    message: String,
}

impl McpOAuthError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn cancelled() -> Self {
        Self::new("MCP OAuth was cancelled")
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn is_cancelled(&self) -> bool {
        self.message == "MCP OAuth was cancelled"
    }
}

impl fmt::Display for McpOAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for McpOAuthError {}

#[derive(Clone)]
pub struct McpOAuthHttpRequest {
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub timeout: Duration,
}

impl fmt::Debug for McpOAuthHttpRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpOAuthHttpRequest")
            .field("method", &self.method)
            .field("url", &redacted_url(&self.url))
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("body_len", &self.body.len())
            .field("timeout", &self.timeout)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct McpOAuthHttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl fmt::Debug for McpOAuthHttpResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpOAuthHttpResponse")
            .field("status", &self.status)
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("body_len", &self.body.len())
            .finish()
    }
}

pub trait McpOAuthHttpClient: Send + Sync {
    fn send<'a>(
        &'a self,
        request: McpOAuthHttpRequest,
        cancellation: &'a CancellationToken,
    ) -> McpOAuthFuture<'a, Result<McpOAuthHttpResponse, McpOAuthError>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ReqwestMcpOAuthHttpClient;

impl ReqwestMcpOAuthHttpClient {
    pub fn new() -> Self {
        Self
    }
}

impl McpOAuthHttpClient for ReqwestMcpOAuthHttpClient {
    fn send<'a>(
        &'a self,
        request: McpOAuthHttpRequest,
        cancellation: &'a CancellationToken,
    ) -> McpOAuthFuture<'a, Result<McpOAuthHttpResponse, McpOAuthError>> {
        Box::pin(async move {
            let method = reqwest::Method::from_bytes(request.method.as_bytes())
                .map_err(|_| McpOAuthError::new("MCP OAuth HTTP method is invalid"))?;
            let client = pinned_http_client(&request.url, request.timeout, cancellation).await?;
            let mut builder = client
                .request(method, &request.url)
                .timeout(request.timeout)
                .body(request.body);
            for (name, value) in request.headers {
                builder = builder.header(name, value);
            }
            let response = tokio::select! {
                response = builder.send() => response,
                () = cancellation.cancelled() => return Err(McpOAuthError::cancelled()),
            }
            .map_err(|error| {
                if error.is_timeout() {
                    McpOAuthError::new("MCP OAuth HTTP request timed out")
                } else {
                    McpOAuthError::new("MCP OAuth HTTP request failed")
                }
            })?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_ascii_lowercase(), value.to_owned()))
                })
                .collect();
            let mut body = Vec::new();
            let mut stream = response.bytes_stream();
            loop {
                let next = tokio::select! {
                    next = stream.next() => next,
                    () = cancellation.cancelled() => return Err(McpOAuthError::cancelled()),
                };
                let Some(chunk) = next else { break };
                let chunk = chunk
                    .map_err(|_| McpOAuthError::new("MCP OAuth response body could not be read"))?;
                if body.len().saturating_add(chunk.len()) > MAX_OAUTH_DOCUMENT_BYTES {
                    return Err(McpOAuthError::new(
                        "MCP OAuth response exceeds the size limit",
                    ));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(McpOAuthHttpResponse {
                status,
                headers,
                body,
            })
        })
    }
}

async fn pinned_http_client(
    value: &str,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<reqwest::Client, McpOAuthError> {
    let url = Url::parse(value).map_err(|_| McpOAuthError::new("MCP OAuth URL is invalid"))?;
    validate_url_common(&url, true)?;
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    if let Some(Host::Domain(host)) = url.host() {
        let port = url
            .port_or_known_default()
            .ok_or_else(|| McpOAuthError::new("MCP OAuth URL has no known port"))?;
        let lookup = tokio::net::lookup_host((host, port));
        let addresses = tokio::select! {
            result = tokio::time::timeout(timeout, lookup) => {
                result
                    .map_err(|_| McpOAuthError::new("MCP OAuth DNS resolution timed out"))?
                    .map_err(|_| McpOAuthError::new("MCP OAuth DNS resolution failed"))?
                    .collect::<Vec<_>>()
            }
            () = cancellation.cancelled() => return Err(McpOAuthError::cancelled()),
        };
        validate_resolved_addresses(&url, &addresses)?;
        builder = builder.resolve_to_addrs(host, &addresses);
    }
    builder
        .build()
        .map_err(|_| McpOAuthError::new("could not initialize MCP OAuth HTTP client"))
}

fn validate_resolved_addresses(url: &Url, addresses: &[SocketAddr]) -> Result<(), McpOAuthError> {
    if addresses.is_empty() {
        return Err(McpOAuthError::new(
            "MCP OAuth DNS resolution returned no addresses",
        ));
    }
    let loopback_http = url.scheme() == "http" && url.host_str() == Some("localhost");
    if addresses.iter().any(|address| match address.ip() {
        IpAddr::V4(ip) => !public_ipv4(ip) && !(loopback_http && ip.is_loopback()),
        IpAddr::V6(ip) => !public_ipv6(ip) && !(loopback_http && ip.is_loopback()),
    }) {
        return Err(McpOAuthError::new(
            "MCP OAuth hostname resolves to a private or reserved address",
        ));
    }
    Ok(())
}

pub trait McpOAuthClock: Send + Sync {
    fn now_seconds(&self) -> u64;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemMcpOAuthClock;

impl McpOAuthClock for SystemMcpOAuthClock {
    fn now_seconds(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }
}

pub trait McpOAuthEntropy: Send + Sync {
    fn fill(&self, bytes: &mut [u8]) -> Result<(), McpOAuthError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemMcpOAuthEntropy;

impl McpOAuthEntropy for SystemMcpOAuthEntropy {
    fn fill(&self, bytes: &mut [u8]) -> Result<(), McpOAuthError> {
        SystemRandom::new()
            .fill(bytes)
            .map_err(|_| McpOAuthError::new("operating-system randomness is unavailable"))
    }
}

pub trait McpOAuthBrowser: Send + Sync {
    /// Present the authorization URL to the user. Production tries the system
    /// browser first and prints a manual-open fallback if that fails.
    fn present(&self, authorization_url: &str) -> Result<(), McpOAuthError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemMcpOAuthBrowser;

impl McpOAuthBrowser for SystemMcpOAuthBrowser {
    fn present(&self, authorization_url: &str) -> Result<(), McpOAuthError> {
        let opened = if cfg!(target_os = "macos") {
            Command::new("open").arg(authorization_url).status()
        } else if cfg!(target_os = "windows") {
            Command::new("rundll32.exe")
                .args(["url.dll,FileProtocolHandler", authorization_url])
                .status()
        } else {
            Command::new("xdg-open").arg(authorization_url).status()
        }
        .is_ok_and(|status| status.success());
        if !opened {
            // This is an explicit user-facing fallback, not a diagnostic log.
            eprintln!("Open this MCP authorization URL in a browser:\n{authorization_url}");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpOAuthCallback {
    pub code: Option<SecretString>,
    pub state: Option<SecretString>,
    pub issuer: Option<String>,
    pub error: Option<String>,
}

pub trait McpOAuthCallbackSession: Send {
    fn redirect_uri(&self) -> &str;
    fn wait<'a>(
        &'a mut self,
        cancellation: &'a CancellationToken,
        timeout: Duration,
    ) -> McpOAuthFuture<'a, Result<McpOAuthCallback, McpOAuthError>>;
}

pub trait McpOAuthCallbackListener: Send + Sync {
    fn bind<'a>(
        &'a self,
        preferred_redirect_uri: Option<&'a str>,
        cancellation: &'a CancellationToken,
    ) -> McpOAuthFuture<'a, Result<Box<dyn McpOAuthCallbackSession>, McpOAuthError>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LoopbackMcpOAuthCallbackListener;

impl McpOAuthCallbackListener for LoopbackMcpOAuthCallbackListener {
    fn bind<'a>(
        &'a self,
        preferred_redirect_uri: Option<&'a str>,
        cancellation: &'a CancellationToken,
    ) -> McpOAuthFuture<'a, Result<Box<dyn McpOAuthCallbackSession>, McpOAuthError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(McpOAuthError::cancelled());
            }
            let preferred = preferred_redirect_uri
                .map(parse_loopback_redirect)
                .transpose()?;
            let address = preferred
                .as_ref()
                .map(|(_, address)| *address)
                .unwrap_or(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0));
            let listener = TcpListener::bind(address)
                .or_else(|_| {
                    TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                })
                .map_err(|_| {
                    McpOAuthError::new(
                        "could not bind the MCP OAuth loopback callback; use an interactive environment that permits localhost callbacks",
                    )
                })?;
            listener
                .set_nonblocking(true)
                .map_err(|_| McpOAuthError::new("could not configure the MCP OAuth callback"))?;
            let local = listener
                .local_addr()
                .map_err(|_| McpOAuthError::new("could not inspect the MCP OAuth callback"))?;
            let path = preferred
                .as_ref()
                .map(|(path, _)| path.clone())
                .unwrap_or_else(|| "/oauth/callback".to_owned());
            let host = match local.ip() {
                IpAddr::V4(address) => address.to_string(),
                IpAddr::V6(address) => format!("[{address}]"),
            };
            Ok(Box::new(LoopbackCallbackSession {
                listener,
                redirect_uri: format!("http://{host}:{}{path}", local.port()),
                path,
            }) as Box<dyn McpOAuthCallbackSession>)
        })
    }
}

struct LoopbackCallbackSession {
    listener: TcpListener,
    redirect_uri: String,
    path: String,
}

impl McpOAuthCallbackSession for LoopbackCallbackSession {
    fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    fn wait<'a>(
        &'a mut self,
        cancellation: &'a CancellationToken,
        timeout: Duration,
    ) -> McpOAuthFuture<'a, Result<McpOAuthCallback, McpOAuthError>> {
        Box::pin(async move {
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                if cancellation.is_cancelled() {
                    return Err(McpOAuthError::cancelled());
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(McpOAuthError::new("MCP OAuth callback timed out"));
                }
                match self.listener.accept() {
                    Ok((stream, _)) => return read_loopback_callback(stream, &self.path).await,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        tokio::select! {
                            () = tokio::time::sleep(Duration::from_millis(25)) => {}
                            () = cancellation.cancelled() => return Err(McpOAuthError::cancelled()),
                        }
                    }
                    Err(_) => return Err(McpOAuthError::new("MCP OAuth callback failed")),
                }
            }
        })
    }
}

async fn read_loopback_callback(
    mut stream: TcpStream,
    expected_path: &str,
) -> Result<McpOAuthCallback, McpOAuthError> {
    let expected_path = expected_path.to_owned();
    tokio::task::spawn_blocking(move || {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .map_err(|_| McpOAuthError::new("MCP OAuth callback failed"))?;
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = stream
                .read(&mut buffer)
                .map_err(|_| McpOAuthError::new("MCP OAuth callback failed"))?;
            if count == 0 || bytes.len().saturating_add(count) > MAX_CALLBACK_BYTES {
                return Err(McpOAuthError::new("MCP OAuth callback is invalid"));
            }
            bytes.extend_from_slice(&buffer[..count]);
        }
        let request = std::str::from_utf8(&bytes)
            .map_err(|_| McpOAuthError::new("MCP OAuth callback is invalid"))?;
        let target = request
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("GET "))
            .and_then(|line| line.split_once(' ').map(|(target, _)| target))
            .ok_or_else(|| McpOAuthError::new("MCP OAuth callback is invalid"))?;
        let url = Url::parse(&format!("http://127.0.0.1{target}"))
            .map_err(|_| McpOAuthError::new("MCP OAuth callback is invalid"))?;
        if url.path() != expected_path {
            return Err(McpOAuthError::new("MCP OAuth callback path does not match"));
        }
        let parameters = url.query_pairs().into_owned().collect::<BTreeMap<_, _>>();
        const BODY: &str = "Authorization received. You may close this window.";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{BODY}",
            BODY.len()
        );
        let _ = stream.write_all(response.as_bytes());
        Ok(McpOAuthCallback {
            code: parameters.get("code").cloned().map(SecretString::new),
            state: parameters.get("state").cloned().map(SecretString::new),
            issuer: parameters.get("iss").cloned(),
            error: parameters.get("error").cloned(),
        })
    })
    .await
    .map_err(|_| McpOAuthError::new("MCP OAuth callback failed"))?
}

fn parse_loopback_redirect(value: &str) -> Result<(String, SocketAddr), McpOAuthError> {
    let url = Url::parse(value)
        .map_err(|_| McpOAuthError::new("stored MCP OAuth redirect URI is invalid"))?;
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(McpOAuthError::new(
            "stored MCP OAuth redirect URI is not a safe loopback URL",
        ));
    }
    let ip = match url.host() {
        Some(Host::Ipv4(address)) if address.is_loopback() => IpAddr::V4(address),
        Some(Host::Ipv6(address)) if address.is_loopback() => IpAddr::V6(address),
        Some(Host::Domain("localhost")) => IpAddr::V4(Ipv4Addr::LOCALHOST),
        _ => {
            return Err(McpOAuthError::new(
                "stored MCP OAuth redirect URI is not loopback",
            ))
        }
    };
    let port = url
        .port()
        .ok_or_else(|| McpOAuthError::new("stored MCP OAuth redirect URI has no port"))?;
    Ok((url.path().to_owned(), SocketAddr::new(ip, port)))
}

#[derive(Clone, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct StoredMcpOAuthState {
    resource: String,
    issuer: String,
    token_endpoint: String,
    client_id: String,
    client_secret: Option<String>,
    redirect_uri: String,
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<u64>,
    scopes: Vec<String>,
}

impl fmt::Debug for StoredMcpOAuthState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredMcpOAuthState")
            .field("resource", &redacted_url(&self.resource))
            .field("issuer", &redacted_url(&self.issuer))
            .field("token_endpoint", &redacted_url(&self.token_endpoint))
            .field("client_id", &"[REDACTED]")
            .field("client_secret", &"[REDACTED]")
            .field("redirect_uri", &redacted_url(&self.redirect_uri))
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .field("scopes", &self.scopes)
            .finish()
    }
}

pub trait McpOAuthTokenStore: Send + Sync {
    fn load(&self, key: &str) -> Result<Option<StoredMcpOAuthState>, McpOAuthError>;
    fn save(&self, key: &str, state: &StoredMcpOAuthState) -> Result<(), McpOAuthError>;
    fn remove(&self, key: &str) -> Result<(), McpOAuthError>;

    fn refresh_lock_path(&self, _key: &str) -> Result<Option<PathBuf>, McpOAuthError> {
        Ok(None)
    }
}

#[derive(Clone, Debug)]
pub struct FileMcpOAuthTokenStore {
    directory: PathBuf,
}

impl FileMcpOAuthTokenStore {
    pub fn under_mycel_home(mycel_home: &Path) -> Self {
        Self {
            directory: mycel_home.join("credentials").join("mcp-oauth"),
        }
    }

    fn path(&self, key: &str) -> Result<PathBuf, McpOAuthError> {
        if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(McpOAuthError::new("MCP OAuth credential key is invalid"));
        }
        Ok(self.directory.join(format!("{key}.json")))
    }

    fn ensure_directory(&self) -> Result<(), McpOAuthError> {
        let credentials = self
            .directory
            .parent()
            .ok_or_else(|| McpOAuthError::new("MCP OAuth credential path is invalid"))?;
        for directory in [credentials, self.directory.as_path()] {
            if fs::symlink_metadata(directory)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                return Err(McpOAuthError::new(
                    "MCP OAuth credential directory must not be a symlink",
                ));
            }
        }
        fs::create_dir_all(&self.directory)
            .map_err(|_| McpOAuthError::new("could not create MCP OAuth credential directory"))?;
        for directory in [credentials, self.directory.as_path()] {
            let metadata = fs::symlink_metadata(directory).map_err(|_| {
                McpOAuthError::new("could not inspect MCP OAuth credential directory")
            })?;
            if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                return Err(McpOAuthError::new(
                    "MCP OAuth credential directory is unsafe",
                ));
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for directory in [credentials, self.directory.as_path()] {
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).map_err(
                    |_| McpOAuthError::new("could not secure MCP OAuth credential directory"),
                )?;
            }
        }
        Ok(())
    }
}

impl McpOAuthTokenStore for FileMcpOAuthTokenStore {
    fn load(&self, key: &str) -> Result<Option<StoredMcpOAuthState>, McpOAuthError> {
        self.ensure_directory()?;
        let path = self.path(key)?;
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                return Err(McpOAuthError::new(
                    "could not inspect MCP OAuth credentials",
                ))
            }
        };
        if metadata.file_type().is_symlink()
            || !metadata.file_type().is_file()
            || metadata.len() > MAX_OAUTH_DOCUMENT_BYTES as u64
        {
            return Err(McpOAuthError::new("MCP OAuth credential file is unsafe"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .map_err(|_| McpOAuthError::new("could not secure MCP OAuth credentials"))?;
        }
        let bytes = fs::read(&path)
            .map_err(|_| McpOAuthError::new("could not read MCP OAuth credentials"))?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| McpOAuthError::new("MCP OAuth credential file is invalid"))
    }

    fn save(&self, key: &str, state: &StoredMcpOAuthState) -> Result<(), McpOAuthError> {
        self.ensure_directory()?;
        let target = self.path(key)?;
        if fs::symlink_metadata(&target).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(McpOAuthError::new(
                "MCP OAuth credential file must not be a symlink",
            ));
        }
        let temporary = self
            .directory
            .join(format!(".{key}.{}.tmp", Uuid::new_v4()));
        let data = Zeroizing::new(
            serde_json::to_vec(state)
                .map_err(|_| McpOAuthError::new("could not encode MCP OAuth credentials"))?,
        );
        let result = (|| -> io::Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(data.as_slice())?;
            file.sync_all()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
            }
            fs::rename(&temporary, &target)?;
            if let Ok(directory) = fs::File::open(&self.directory) {
                let _ = directory.sync_all();
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
            return Err(McpOAuthError::new("could not save MCP OAuth credentials"));
        }
        Ok(())
    }

    fn remove(&self, key: &str) -> Result<(), McpOAuthError> {
        self.ensure_directory()?;
        match fs::remove_file(self.path(key)?) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(McpOAuthError::new("could not remove MCP OAuth credentials")),
        }
    }

    fn refresh_lock_path(&self, key: &str) -> Result<Option<PathBuf>, McpOAuthError> {
        self.ensure_directory()?;
        self.path(key)?;
        Ok(Some(self.directory.join(format!(".{key}.refresh.lock"))))
    }
}

struct FileMcpOAuthCredentialLock {
    path: PathBuf,
}

impl FileMcpOAuthCredentialLock {
    async fn acquire(
        path: PathBuf,
        cancellation: &CancellationToken,
    ) -> Result<Self, McpOAuthError> {
        const ATTEMPTS: usize = 120;
        const DELAY: Duration = Duration::from_millis(250);
        const STALE_AFTER: Duration = Duration::from_secs(15 * 60);
        for attempt in 0..ATTEMPTS {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(mut file) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).is_err() {
                            let _ = fs::remove_file(&path);
                            return Err(McpOAuthError::new(
                                "could not secure MCP OAuth refresh lock",
                            ));
                        }
                    }
                    if writeln!(file, "{}", std::process::id()).is_err() {
                        let _ = fs::remove_file(&path);
                        return Err(McpOAuthError::new("could not write MCP OAuth refresh lock"));
                    }
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|elapsed| elapsed >= STALE_AFTER);
                    if stale {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    if attempt + 1 == ATTEMPTS {
                        return Err(McpOAuthError::new(
                            "timed out waiting for the MCP OAuth refresh lock",
                        ));
                    }
                    tokio::select! {
                        () = tokio::time::sleep(DELAY) => {}
                        () = cancellation.cancelled() => return Err(McpOAuthError::cancelled()),
                    }
                }
                Err(_) => {
                    return Err(McpOAuthError::new(
                        "could not create MCP OAuth refresh lock",
                    ))
                }
            }
        }
        Err(McpOAuthError::new(
            "timed out waiting for the MCP OAuth refresh lock",
        ))
    }
}

impl Drop for FileMcpOAuthCredentialLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Clone, Debug)]
struct ProtectedResourceMetadata {
    issuer: String,
    scopes: Vec<String>,
}

#[derive(Clone, Debug)]
struct AuthorizationServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    scopes_supported: Vec<String>,
    authorization_response_iss_parameter_supported: bool,
}

#[derive(Clone, Debug)]
struct OAuthContext {
    resource: String,
    authorization: AuthorizationServerMetadata,
    scopes: Vec<String>,
}

pub struct McpOAuthManager {
    http: Arc<dyn McpOAuthHttpClient>,
    browser: Arc<dyn McpOAuthBrowser>,
    listener: Arc<dyn McpOAuthCallbackListener>,
    clock: Arc<dyn McpOAuthClock>,
    entropy: Arc<dyn McpOAuthEntropy>,
    store: Arc<dyn McpOAuthTokenStore>,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    http_timeout: Duration,
    callback_timeout: Duration,
}

pub struct McpOAuthDependencies {
    pub http: Arc<dyn McpOAuthHttpClient>,
    pub browser: Arc<dyn McpOAuthBrowser>,
    pub listener: Arc<dyn McpOAuthCallbackListener>,
    pub clock: Arc<dyn McpOAuthClock>,
    pub entropy: Arc<dyn McpOAuthEntropy>,
    pub store: Arc<dyn McpOAuthTokenStore>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct McpOAuthOptions {
    pub http_timeout: Duration,
    pub callback_timeout: Duration,
}

impl Default for McpOAuthOptions {
    fn default() -> Self {
        Self {
            http_timeout: DEFAULT_HTTP_TIMEOUT,
            callback_timeout: DEFAULT_CALLBACK_TIMEOUT,
        }
    }
}

impl McpOAuthManager {
    pub fn new(dependencies: McpOAuthDependencies, options: McpOAuthOptions) -> Self {
        Self {
            http: dependencies.http,
            browser: dependencies.browser,
            listener: dependencies.listener,
            clock: dependencies.clock,
            entropy: dependencies.entropy,
            store: dependencies.store,
            locks: Mutex::new(HashMap::new()),
            http_timeout: options.http_timeout,
            callback_timeout: options.callback_timeout,
        }
    }

    pub fn production(mycel_home: &Path) -> Self {
        Self::new(
            McpOAuthDependencies {
                http: Arc::new(ReqwestMcpOAuthHttpClient::new()),
                browser: Arc::new(SystemMcpOAuthBrowser),
                listener: Arc::new(LoopbackMcpOAuthCallbackListener),
                clock: Arc::new(SystemMcpOAuthClock),
                entropy: Arc::new(SystemMcpOAuthEntropy),
                store: Arc::new(FileMcpOAuthTokenStore::under_mycel_home(mycel_home)),
            },
            McpOAuthOptions::default(),
        )
    }

    pub async fn session(
        self: &Arc<Self>,
        server_name: &str,
        endpoint: &str,
        cancellation: &CancellationToken,
    ) -> Result<Arc<McpOAuthSession>, McpOAuthError> {
        if cancellation.is_cancelled() {
            return Err(McpOAuthError::cancelled());
        }
        validate_server_name(server_name)?;
        let resource = canonical_resource(endpoint)?;
        let key = credential_key(server_name, &resource);
        let context = self.discover(&resource, cancellation).await?;
        let session = Arc::new(McpOAuthSession {
            manager: Arc::clone(self),
            key,
            context,
        });
        session.access_token(cancellation).await?;
        Ok(session)
    }

    async fn lock_for(&self, key: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        Arc::clone(
            locks
                .entry(key.to_owned())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    async fn discover(
        &self,
        resource: &str,
        cancellation: &CancellationToken,
    ) -> Result<OAuthContext, McpOAuthError> {
        let challenge = self.probe_challenge(resource, cancellation).await?;
        let (protected_url, challenge_scope) = if let Some(challenge) = challenge {
            (challenge.resource_metadata, challenge.scope)
        } else {
            (None, None)
        };
        let protected = self
            .discover_protected_resource(resource, protected_url.as_deref(), cancellation)
            .await?;
        let authorization = self
            .discover_authorization_server(&protected.issuer, cancellation)
            .await?;
        let mut scopes = challenge_scope
            .map(|scope| split_scopes(&scope))
            .unwrap_or_else(|| protected.scopes.clone());
        if authorization
            .scopes_supported
            .iter()
            .any(|scope| scope == "offline_access")
            && !scopes.iter().any(|scope| scope == "offline_access")
        {
            scopes.push("offline_access".to_owned());
        }
        scopes.sort();
        scopes.dedup();
        Ok(OAuthContext {
            resource: resource.to_owned(),
            authorization,
            scopes,
        })
    }

    async fn probe_challenge(
        &self,
        resource: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<BearerChallenge>, McpOAuthError> {
        let response = self
            .send("GET", resource, BTreeMap::new(), Vec::new(), cancellation)
            .await?;
        if response.status != 401 {
            return Ok(None);
        }
        let Some(header) = response.headers.get("www-authenticate") else {
            return Ok(None);
        };
        Ok(parse_bearer_challenge(header))
    }

    async fn discover_protected_resource(
        &self,
        resource: &str,
        challenge_url: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<ProtectedResourceMetadata, McpOAuthError> {
        let mut candidates = Vec::new();
        if let Some(challenge_url) = challenge_url {
            validate_resource_metadata_url(resource, challenge_url)?;
            candidates.push(challenge_url.to_owned());
        } else {
            candidates.extend(protected_resource_metadata_urls(resource)?);
        }
        for candidate in candidates {
            let response = self
                .send("GET", &candidate, BTreeMap::new(), Vec::new(), cancellation)
                .await?;
            if response.status == 404 {
                continue;
            }
            if response.status != 200 {
                return Err(McpOAuthError::new(format!(
                    "MCP protected-resource discovery failed with HTTP {}",
                    response.status
                )));
            }
            let value = parse_json_object(&response.body, "protected-resource metadata")?;
            let declared_resource = required_string(&value, "resource")?;
            if canonical_resource(declared_resource)? != resource {
                return Err(McpOAuthError::new(
                    "MCP protected-resource metadata does not match the configured server",
                ));
            }
            let issuers = string_array(value.get("authorization_servers"));
            if issuers.is_empty() {
                return Err(McpOAuthError::new(
                    "MCP protected-resource metadata has no authorization server",
                ));
            }
            let issuer = validate_oauth_url("authorization server issuer", &issuers[0], true)?;
            let scopes = string_array(value.get("scopes_supported"));
            return Ok(ProtectedResourceMetadata { issuer, scopes });
        }
        Err(McpOAuthError::new(
            "MCP protected-resource metadata was not found",
        ))
    }

    async fn discover_authorization_server(
        &self,
        issuer: &str,
        cancellation: &CancellationToken,
    ) -> Result<AuthorizationServerMetadata, McpOAuthError> {
        for candidate in authorization_metadata_urls(issuer)? {
            let response = self
                .send("GET", &candidate, BTreeMap::new(), Vec::new(), cancellation)
                .await?;
            if matches!(response.status, 404 | 400) {
                continue;
            }
            if response.status != 200 {
                return Err(McpOAuthError::new(format!(
                    "MCP authorization-server discovery failed with HTTP {}",
                    response.status
                )));
            }
            let value = parse_json_object(&response.body, "authorization-server metadata")?;
            let declared_issuer = required_string(&value, "issuer")?;
            if declared_issuer != issuer {
                return Err(McpOAuthError::new(
                    "MCP authorization-server metadata issuer does not match",
                ));
            }
            let methods = string_array(value.get("code_challenge_methods_supported"));
            if !methods.iter().any(|method| method == "S256") {
                return Err(McpOAuthError::new(
                    "MCP authorization server does not advertise S256 PKCE",
                ));
            }
            let authorization_endpoint = validate_oauth_url(
                "authorization endpoint",
                required_string(&value, "authorization_endpoint")?,
                false,
            )?;
            let token_endpoint = validate_oauth_url(
                "token endpoint",
                required_string(&value, "token_endpoint")?,
                false,
            )?;
            let registration_endpoint = value
                .get("registration_endpoint")
                .and_then(serde_json::Value::as_str)
                .map(|value| validate_oauth_url("registration endpoint", value, false))
                .transpose()?;
            return Ok(AuthorizationServerMetadata {
                issuer: issuer.to_owned(),
                authorization_endpoint,
                token_endpoint,
                registration_endpoint,
                scopes_supported: string_array(value.get("scopes_supported")),
                authorization_response_iss_parameter_supported: value
                    .get("authorization_response_iss_parameter_supported")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            });
        }
        Err(McpOAuthError::new(
            "MCP authorization-server metadata was not found",
        ))
    }

    async fn send(
        &self,
        method: &str,
        url: &str,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
        cancellation: &CancellationToken,
    ) -> Result<McpOAuthHttpResponse, McpOAuthError> {
        if cancellation.is_cancelled() {
            return Err(McpOAuthError::cancelled());
        }
        let response = self
            .http
            .send(
                McpOAuthHttpRequest {
                    method: method.to_owned(),
                    url: url.to_owned(),
                    headers,
                    body,
                    timeout: self.http_timeout,
                },
                cancellation,
            )
            .await?;
        if (300..400).contains(&response.status) {
            return Err(McpOAuthError::new("MCP OAuth redirects are not allowed"));
        }
        Ok(response)
    }
}

pub struct McpOAuthSession {
    manager: Arc<McpOAuthManager>,
    key: String,
    context: OAuthContext,
}

impl McpOAuthSession {
    pub async fn access_token(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<SecretString, McpOAuthError> {
        self.access_token_inner(None, cancellation).await
    }

    /// Refresh after a resource server rejects `rejected_token`. Concurrent
    /// requests that observed the same 401 coordinate through the credential
    /// lock; followers reuse the token already refreshed by the first caller.
    pub async fn refresh_after_unauthorized(
        &self,
        rejected_token: &SecretString,
        cancellation: &CancellationToken,
    ) -> Result<SecretString, McpOAuthError> {
        self.access_token_inner(Some(rejected_token.expose()), cancellation)
            .await
    }

    async fn access_token_inner(
        &self,
        rejected_token: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<SecretString, McpOAuthError> {
        let lock = self.manager.lock_for(&self.key).await;
        let _guard = tokio::select! {
            guard = lock.lock() => guard,
            () = cancellation.cancelled() => return Err(McpOAuthError::cancelled()),
        };
        if cancellation.is_cancelled() {
            return Err(McpOAuthError::cancelled());
        }
        let mut stored = self.load_compatible_state()?;
        if let Some(state) = stored.as_ref() {
            if !token_needs_refresh(state, rejected_token, self.manager.clock.now_seconds()) {
                return Ok(SecretString::new(state.access_token.clone()));
            }
        }
        let _file_lock = match self.manager.store.refresh_lock_path(&self.key)? {
            Some(path) => Some(FileMcpOAuthCredentialLock::acquire(path, cancellation).await?),
            None => None,
        };
        stored = self.load_compatible_state()?;
        if let Some(state) = stored.as_ref() {
            if !token_needs_refresh(state, rejected_token, self.manager.clock.now_seconds()) {
                return Ok(SecretString::new(state.access_token.clone()));
            }
            if state.refresh_token.is_some() {
                match self.refresh(state, cancellation).await {
                    Ok(refreshed) => {
                        let token = SecretString::new(refreshed.access_token.clone());
                        self.manager.store.save(&self.key, &refreshed)?;
                        return Ok(token);
                    }
                    Err(RefreshFailure::Reauthorize) => {
                        self.manager.store.remove(&self.key)?;
                    }
                    Err(RefreshFailure::Fatal(error)) => return Err(error),
                }
            }
        }
        let authorized = self.authorize(stored.as_ref(), cancellation).await?;
        let token = SecretString::new(authorized.access_token.clone());
        self.manager.store.save(&self.key, &authorized)?;
        Ok(token)
    }

    fn load_compatible_state(&self) -> Result<Option<StoredMcpOAuthState>, McpOAuthError> {
        let mut stored = self.manager.store.load(&self.key)?;
        if stored.as_ref().is_some_and(|state| {
            state.resource != self.context.resource
                || state.issuer != self.context.authorization.issuer
                || state.token_endpoint != self.context.authorization.token_endpoint
                || validate_persisted_state(state).is_err()
        }) {
            self.manager.store.remove(&self.key)?;
            stored = None;
        }
        Ok(stored)
    }

    async fn authorize(
        &self,
        previous: Option<&StoredMcpOAuthState>,
        cancellation: &CancellationToken,
    ) -> Result<StoredMcpOAuthState, McpOAuthError> {
        let preferred_redirect = previous
            .filter(|state| state.issuer == self.context.authorization.issuer)
            .map(|state| state.redirect_uri.as_str());
        let mut callback = self
            .manager
            .listener
            .bind(preferred_redirect, cancellation)
            .await?;
        let redirect_uri = callback.redirect_uri().to_owned();
        let (client_id, client_secret) = match previous.filter(|state| {
            state.issuer == self.context.authorization.issuer
                && state.redirect_uri == redirect_uri
                && !state.client_id.is_empty()
        }) {
            Some(state) => (state.client_id.clone(), state.client_secret.clone()),
            None => self.register_client(&redirect_uri, cancellation).await?,
        };
        let verifier = random_url_token(self.manager.entropy.as_ref())?;
        let state = random_url_token(self.manager.entropy.as_ref())?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut authorization_url = Url::parse(&self.context.authorization.authorization_endpoint)
            .map_err(|_| McpOAuthError::new("MCP authorization endpoint is invalid"))?;
        {
            let mut query = authorization_url.query_pairs_mut();
            query
                .append_pair("response_type", "code")
                .append_pair("client_id", &client_id)
                .append_pair("redirect_uri", &redirect_uri)
                .append_pair("state", &state)
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("resource", &self.context.resource);
            if !self.context.scopes.is_empty() {
                query.append_pair("scope", &self.context.scopes.join(" "));
            }
        }
        self.manager.browser.present(authorization_url.as_str())?;
        let response = callback
            .wait(cancellation, self.manager.callback_timeout)
            .await?;
        let returned_state = response
            .state
            .as_ref()
            .ok_or_else(|| McpOAuthError::new("MCP OAuth callback omitted state"))?;
        let comparison_key = hmac::Key::new(hmac::HMAC_SHA256, b"mycel MCP OAuth state comparison");
        let expected_state = hmac::sign(&comparison_key, state.as_bytes());
        hmac::verify(
            &comparison_key,
            returned_state.expose().as_bytes(),
            expected_state.as_ref(),
        )
        .map_err(|_| McpOAuthError::new("MCP OAuth callback state does not match"))?;
        validate_callback_issuer(&self.context.authorization, response.issuer.as_deref())?;
        if response.error.is_some() {
            return Err(McpOAuthError::new(
                "MCP authorization server denied the authorization request",
            ));
        }
        let code = response
            .code
            .ok_or_else(|| McpOAuthError::new("MCP OAuth callback omitted the code"))?;
        let token_body = {
            let mut form = form_urlencoded::Serializer::new(String::new());
            form.append_pair("grant_type", "authorization_code")
                .append_pair("code", code.expose())
                .append_pair("client_id", &client_id)
                .append_pair("redirect_uri", &redirect_uri)
                .append_pair("code_verifier", &verifier)
                .append_pair("resource", &self.context.resource);
            if let Some(secret) = &client_secret {
                form.append_pair("client_secret", secret);
            }
            form.finish()
        };
        let token = self
            .token_request(
                &self.context.authorization.token_endpoint,
                token_body,
                None,
                cancellation,
            )
            .await?;
        Ok(StoredMcpOAuthState {
            resource: self.context.resource.clone(),
            issuer: self.context.authorization.issuer.clone(),
            token_endpoint: self.context.authorization.token_endpoint.clone(),
            client_id,
            client_secret,
            redirect_uri,
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            expires_at: token.expires_at,
            scopes: if token.scopes.is_empty() {
                self.context.scopes.clone()
            } else {
                token.scopes
            },
        })
    }

    async fn register_client(
        &self,
        redirect_uri: &str,
        cancellation: &CancellationToken,
    ) -> Result<(String, Option<String>), McpOAuthError> {
        let endpoint = self
            .context
            .authorization
            .registration_endpoint
            .as_deref()
            .ok_or_else(|| {
                McpOAuthError::new(
                    "MCP authorization server requires a pre-registered client and does not offer dynamic registration",
                )
            })?;
        let body = serde_json::to_vec(&serde_json::json!({
            "client_name": "Mycel CLI",
            "application_type": "native",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none"
        }))
        .map_err(|_| McpOAuthError::new("could not encode MCP client registration"))?;
        let response = self
            .manager
            .send(
                "POST",
                endpoint,
                BTreeMap::from([
                    ("accept".to_owned(), "application/json".to_owned()),
                    ("content-type".to_owned(), "application/json".to_owned()),
                ]),
                body,
                cancellation,
            )
            .await?;
        if !matches!(response.status, 200 | 201) {
            return Err(McpOAuthError::new(format!(
                "MCP client registration failed with HTTP {}",
                response.status
            )));
        }
        let value = parse_json_object(&response.body, "client registration")?;
        let client_id = required_string(&value, "client_id")?.to_owned();
        if client_id.is_empty() || client_id.chars().any(char::is_control) {
            return Err(McpOAuthError::new(
                "MCP client registration returned an invalid client id",
            ));
        }
        let client_secret = value
            .get("client_secret")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
            .map(str::to_owned);
        Ok((client_id, client_secret))
    }

    async fn refresh(
        &self,
        state: &StoredMcpOAuthState,
        cancellation: &CancellationToken,
    ) -> Result<StoredMcpOAuthState, RefreshFailure> {
        let refresh_token = state.refresh_token.as_deref().ok_or_else(|| {
            RefreshFailure::Fatal(McpOAuthError::new("MCP OAuth refresh token is missing"))
        })?;
        let token_body = {
            let mut form = form_urlencoded::Serializer::new(String::new());
            form.append_pair("grant_type", "refresh_token")
                .append_pair("refresh_token", refresh_token)
                .append_pair("client_id", &state.client_id)
                .append_pair("resource", &state.resource);
            if !state.scopes.is_empty() {
                form.append_pair("scope", &state.scopes.join(" "));
            }
            if let Some(secret) = &state.client_secret {
                form.append_pair("client_secret", secret);
            }
            form.finish()
        };
        let token = self
            .token_request(
                &state.token_endpoint,
                token_body,
                state.refresh_token.clone(),
                cancellation,
            )
            .await
            .map_err(|error| {
                if error.message() == "MCP OAuth refresh was rejected" {
                    RefreshFailure::Reauthorize
                } else {
                    RefreshFailure::Fatal(error)
                }
            })?;
        let mut refreshed = state.clone();
        refreshed.access_token = token.access_token;
        refreshed.refresh_token = token.refresh_token.or_else(|| state.refresh_token.clone());
        refreshed.expires_at = token.expires_at;
        if !token.scopes.is_empty() {
            refreshed.scopes = token.scopes;
        }
        Ok(refreshed)
    }

    async fn token_request(
        &self,
        endpoint: &str,
        body: String,
        existing_refresh: Option<String>,
        cancellation: &CancellationToken,
    ) -> Result<TokenResponse, McpOAuthError> {
        let response = self
            .manager
            .send(
                "POST",
                endpoint,
                BTreeMap::from([
                    ("accept".to_owned(), "application/json".to_owned()),
                    (
                        "content-type".to_owned(),
                        "application/x-www-form-urlencoded".to_owned(),
                    ),
                ]),
                body.into_bytes(),
                cancellation,
            )
            .await?;
        if response.status != 200 {
            let invalid_grant = serde_json::from_slice::<serde_json::Value>(&response.body)
                .ok()
                .and_then(|value| {
                    value
                        .get("error")
                        .and_then(serde_json::Value::as_str)
                        .map(|error| error == "invalid_grant")
                })
                .unwrap_or(false);
            if invalid_grant && existing_refresh.is_some() {
                return Err(McpOAuthError::new("MCP OAuth refresh was rejected"));
            }
            return Err(McpOAuthError::new(format!(
                "MCP OAuth token request failed with HTTP {}",
                response.status
            )));
        }
        let value = parse_json_object(&response.body, "token response")?;
        let access_token = required_string(&value, "access_token")?.to_owned();
        if access_token.is_empty() || access_token.chars().any(char::is_control) {
            return Err(McpOAuthError::new(
                "MCP OAuth token response contains an invalid access token",
            ));
        }
        let token_type = value
            .get("token_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Bearer");
        if !token_type.eq_ignore_ascii_case("bearer") {
            return Err(McpOAuthError::new(
                "MCP OAuth token response is not a bearer token",
            ));
        }
        let refresh_token = value
            .get("refresh_token")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
            .map(str::to_owned);
        let expires_at = value
            .get("expires_in")
            .and_then(serde_json::Value::as_u64)
            .filter(|value| *value > 0)
            .map(|expires_in| self.manager.clock.now_seconds().saturating_add(expires_in));
        let scopes = value
            .get("scope")
            .and_then(serde_json::Value::as_str)
            .map(split_scopes)
            .unwrap_or_default();
        Ok(TokenResponse {
            access_token,
            refresh_token,
            expires_at,
            scopes,
        })
    }
}

enum RefreshFailure {
    Reauthorize,
    Fatal(McpOAuthError),
}

struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<u64>,
    scopes: Vec<String>,
}

fn token_needs_refresh(
    state: &StoredMcpOAuthState,
    rejected_token: Option<&str>,
    now: u64,
) -> bool {
    rejected_token.is_some_and(|rejected| rejected == state.access_token)
        || state
            .expires_at
            .is_some_and(|expires_at| expires_at <= now.saturating_add(EXPIRY_SKEW_SECONDS))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct BearerChallenge {
    resource_metadata: Option<String>,
    scope: Option<String>,
}

fn parse_bearer_challenge(value: &str) -> Option<BearerChallenge> {
    let lowercase = value.to_ascii_lowercase();
    let bearer = lowercase.match_indices("bearer").find_map(|(index, _)| {
        let starts_at_boundary = index == 0
            || value[..index]
                .chars()
                .next_back()
                .is_some_and(|character| character == ',' || character.is_ascii_whitespace());
        let after = index + "bearer".len();
        let ends_at_boundary = value[after..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace);
        (starts_at_boundary && ends_at_boundary).then_some(index)
    })?;
    let mut parameters = BTreeMap::new();
    let mut input = &value[bearer + "bearer".len()..];
    while !input.trim_start().is_empty() {
        input = input.trim_start_matches([' ', ',']);
        let (name, rest) = input.split_once('=')?;
        let name = name.trim().to_ascii_lowercase();
        input = rest;
        let (parsed, rest) = if let Some(quoted) = input.strip_prefix('"') {
            let mut escaped = false;
            let mut end = None;
            for (index, character) in quoted.char_indices() {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    end = Some(index);
                    break;
                }
            }
            let end = end?;
            (quoted[..end].replace("\\\"", "\""), &quoted[end + 1..])
        } else {
            let end = input.find(',').unwrap_or(input.len());
            (input[..end].trim().to_owned(), &input[end..])
        };
        parameters.insert(name, parsed);
        input = rest;
    }
    Some(BearerChallenge {
        resource_metadata: parameters.remove("resource_metadata"),
        scope: parameters.remove("scope"),
    })
}

fn protected_resource_metadata_urls(resource: &str) -> Result<Vec<String>, McpOAuthError> {
    let url =
        Url::parse(resource).map_err(|_| McpOAuthError::new("MCP resource URL is invalid"))?;
    let origin = origin(&url)?;
    let path = url.path().trim_start_matches('/');
    let mut urls = Vec::new();
    if !path.is_empty() {
        urls.push(format!(
            "{origin}/.well-known/oauth-protected-resource/{path}"
        ));
    }
    urls.push(format!("{origin}/.well-known/oauth-protected-resource"));
    urls.dedup();
    Ok(urls)
}

fn authorization_metadata_urls(issuer: &str) -> Result<Vec<String>, McpOAuthError> {
    let url = Url::parse(issuer)
        .map_err(|_| McpOAuthError::new("MCP authorization issuer is invalid"))?;
    let origin = origin(&url)?;
    let path = url.path().trim_matches('/');
    if path.is_empty() {
        Ok(vec![
            format!("{origin}/.well-known/oauth-authorization-server"),
            format!("{origin}/.well-known/openid-configuration"),
        ])
    } else {
        Ok(vec![
            format!("{origin}/.well-known/oauth-authorization-server/{path}"),
            format!("{origin}/.well-known/openid-configuration/{path}"),
            format!("{origin}/{path}/.well-known/openid-configuration"),
        ])
    }
}

fn validate_resource_metadata_url(resource: &str, metadata: &str) -> Result<(), McpOAuthError> {
    let resource =
        Url::parse(resource).map_err(|_| McpOAuthError::new("MCP resource URL is invalid"))?;
    let metadata = Url::parse(metadata)
        .map_err(|_| McpOAuthError::new("MCP resource metadata URL is invalid"))?;
    validate_url_common(&metadata, true)?;
    if origin(&resource)? != origin(&metadata)? {
        return Err(McpOAuthError::new(
            "MCP resource metadata URL must share the configured server origin",
        ));
    }
    Ok(())
}

fn validate_oauth_url(label: &str, value: &str, issuer: bool) -> Result<String, McpOAuthError> {
    if value.len() > 4096 {
        return Err(McpOAuthError::new(format!("MCP OAuth {label} is too long")));
    }
    let url = Url::parse(value)
        .map_err(|_| McpOAuthError::new(format!("MCP OAuth {label} is invalid")))?;
    validate_url_common(&url, false)?;
    if url.scheme() != "https" {
        return Err(McpOAuthError::new(format!(
            "MCP OAuth {label} must use HTTPS"
        )));
    }
    if issuer && (url.query().is_some() || url.fragment().is_some()) {
        return Err(McpOAuthError::new(
            "MCP OAuth issuer must not contain a query or fragment",
        ));
    }
    Ok(if issuer {
        value.to_owned()
    } else {
        url.to_string()
    })
}

fn validate_url_common(url: &Url, allow_loopback_http: bool) -> Result<(), McpOAuthError> {
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(McpOAuthError::new(
            "MCP OAuth URL must not contain credentials or a fragment",
        ));
    }
    let host = url
        .host()
        .ok_or_else(|| McpOAuthError::new("MCP OAuth URL has no host"))?;
    if let Host::Ipv4(address) = &host {
        if !public_ipv4(*address) && !(allow_loopback_http && address.is_loopback()) {
            return Err(McpOAuthError::new(
                "MCP OAuth URL targets a private or reserved address",
            ));
        }
    }
    if let Host::Ipv6(address) = &host {
        if !public_ipv6(*address) && !(allow_loopback_http && address.is_loopback()) {
            return Err(McpOAuthError::new(
                "MCP OAuth URL targets a private or reserved address",
            ));
        }
    }
    let loopback_domain = matches!(&host, Host::Domain("localhost"));
    if url.scheme() != "https"
        && !(allow_loopback_http
            && url.scheme() == "http"
            && (loopback_domain
                || matches!(&host, Host::Ipv4(address) if address.is_loopback())
                || matches!(&host, Host::Ipv6(address) if address.is_loopback())))
    {
        return Err(McpOAuthError::new(
            "MCP OAuth URL must use HTTPS or loopback HTTP",
        ));
    }
    Ok(())
}

fn public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224)
}

fn public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return public_ipv4(mapped);
    }
    let segments = address.segments();
    !(address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}

fn canonical_resource(value: &str) -> Result<String, McpOAuthError> {
    let url = Url::parse(value).map_err(|_| McpOAuthError::new("MCP resource URL is invalid"))?;
    validate_url_common(&url, true)?;
    if url.query().is_some() {
        return Err(McpOAuthError::new(
            "OAuth-enabled MCP resource URL must not contain a query",
        ));
    }
    let mut value = url.to_string();
    if url.path() == "/" {
        value.truncate(value.trim_end_matches('/').len());
    }
    Ok(value)
}

fn validate_persisted_state(state: &StoredMcpOAuthState) -> Result<(), McpOAuthError> {
    canonical_resource(&state.resource)?;
    validate_oauth_url("stored issuer", &state.issuer, true)?;
    validate_oauth_url("stored token endpoint", &state.token_endpoint, false)?;
    parse_loopback_redirect(&state.redirect_uri)?;
    if state.client_id.is_empty()
        || state.access_token.is_empty()
        || state.access_token.chars().any(char::is_control)
    {
        return Err(McpOAuthError::new(
            "stored MCP OAuth credentials are invalid",
        ));
    }
    Ok(())
}

fn validate_callback_issuer(
    metadata: &AuthorizationServerMetadata,
    returned: Option<&str>,
) -> Result<(), McpOAuthError> {
    match (
        metadata.authorization_response_iss_parameter_supported,
        returned,
    ) {
        (true, None) => Err(McpOAuthError::new("MCP OAuth callback omitted issuer")),
        (_, Some(returned)) if returned != metadata.issuer => Err(McpOAuthError::new(
            "MCP OAuth callback issuer does not match",
        )),
        _ => Ok(()),
    }
}

fn validate_server_name(value: &str) -> Result<(), McpOAuthError> {
    if value.trim().is_empty()
        || value.len() > 256
        || value.chars().any(|character| character.is_control())
    {
        Err(McpOAuthError::new("MCP OAuth server name is invalid"))
    } else {
        Ok(())
    }
}

fn credential_key(server_name: &str, resource: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(server_name.as_bytes());
    digest.update([0]);
    digest.update(resource.as_bytes());
    format!("{:x}", digest.finalize())
}

fn random_url_token(entropy: &dyn McpOAuthEntropy) -> Result<String, McpOAuthError> {
    let mut bytes = [0_u8; 32];
    entropy.fill(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn parse_json_object(
    bytes: &[u8],
    label: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, McpOAuthError> {
    if bytes.len() > MAX_OAUTH_DOCUMENT_BYTES {
        return Err(McpOAuthError::new(format!(
            "MCP OAuth {label} exceeds the size limit"
        )));
    }
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| McpOAuthError::new(format!("MCP OAuth {label} is invalid JSON")))?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| McpOAuthError::new(format!("MCP OAuth {label} must be an object")))
}

fn required_string<'a>(
    value: &'a serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> Result<&'a str, McpOAuthError> {
    value
        .get(name)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
        .ok_or_else(|| McpOAuthError::new(format!("MCP OAuth metadata field {name:?} is invalid")))
}

fn string_array(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
        .map(str::to_owned)
        .collect()
}

fn split_scopes(value: &str) -> Vec<String> {
    value
        .split_ascii_whitespace()
        .filter(|scope| !scope.is_empty() && !scope.chars().any(char::is_control))
        .map(str::to_owned)
        .collect()
}

fn origin(url: &Url) -> Result<String, McpOAuthError> {
    let host = url
        .host_str()
        .ok_or_else(|| McpOAuthError::new("MCP OAuth URL has no host"))?;
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    Ok(match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    })
}

fn redacted_url(value: &str) -> String {
    Url::parse(value)
        .ok()
        .and_then(|url| origin(&url).ok())
        .unwrap_or_else(|| "[redacted URL]".to_owned())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicU64, AtomicU8, Ordering},
            Mutex as StdMutex,
        },
    };

    use super::*;

    struct FakeHttp {
        responses: StdMutex<VecDeque<Result<McpOAuthHttpResponse, McpOAuthError>>>,
        requests: StdMutex<Vec<McpOAuthHttpRequest>>,
    }

    impl FakeHttp {
        fn new(responses: Vec<McpOAuthHttpResponse>) -> Self {
            Self {
                responses: StdMutex::new(responses.into_iter().map(Ok).collect()),
                requests: StdMutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> Vec<McpOAuthHttpRequest> {
            self.requests.lock().expect("requests lock").clone()
        }
    }

    impl McpOAuthHttpClient for FakeHttp {
        fn send<'a>(
            &'a self,
            request: McpOAuthHttpRequest,
            cancellation: &'a CancellationToken,
        ) -> McpOAuthFuture<'a, Result<McpOAuthHttpResponse, McpOAuthError>> {
            Box::pin(async move {
                if cancellation.is_cancelled() {
                    return Err(McpOAuthError::cancelled());
                }
                self.requests.lock().expect("requests lock").push(request);
                self.responses
                    .lock()
                    .expect("responses lock")
                    .pop_front()
                    .expect("fake response")
            })
        }
    }

    #[derive(Default)]
    struct FakeBrowser {
        urls: StdMutex<Vec<String>>,
    }

    impl McpOAuthBrowser for FakeBrowser {
        fn present(&self, authorization_url: &str) -> Result<(), McpOAuthError> {
            self.urls
                .lock()
                .expect("browser lock")
                .push(authorization_url.to_owned());
            Ok(())
        }
    }

    struct FakeCallbackListener {
        response: McpOAuthCallback,
        preferred: StdMutex<Vec<Option<String>>>,
        observed_timeout: Arc<StdMutex<Option<Duration>>>,
    }

    impl FakeCallbackListener {
        fn new(response: McpOAuthCallback) -> Self {
            Self {
                response,
                preferred: StdMutex::new(Vec::new()),
                observed_timeout: Arc::new(StdMutex::new(None)),
            }
        }
    }

    struct FakeCallbackSession {
        response: Option<McpOAuthCallback>,
        observed_timeout: Arc<StdMutex<Option<Duration>>>,
    }

    impl McpOAuthCallbackSession for FakeCallbackSession {
        fn redirect_uri(&self) -> &str {
            "http://127.0.0.1:43117/oauth/callback"
        }

        fn wait<'a>(
            &'a mut self,
            cancellation: &'a CancellationToken,
            timeout: Duration,
        ) -> McpOAuthFuture<'a, Result<McpOAuthCallback, McpOAuthError>> {
            Box::pin(async move {
                if cancellation.is_cancelled() {
                    return Err(McpOAuthError::cancelled());
                }
                *self.observed_timeout.lock().expect("timeout lock") = Some(timeout);
                self.response
                    .take()
                    .ok_or_else(|| McpOAuthError::new("fake callback was reused"))
            })
        }
    }

    impl McpOAuthCallbackListener for FakeCallbackListener {
        fn bind<'a>(
            &'a self,
            preferred_redirect_uri: Option<&'a str>,
            cancellation: &'a CancellationToken,
        ) -> McpOAuthFuture<'a, Result<Box<dyn McpOAuthCallbackSession>, McpOAuthError>> {
            Box::pin(async move {
                if cancellation.is_cancelled() {
                    return Err(McpOAuthError::cancelled());
                }
                self.preferred
                    .lock()
                    .expect("preferred lock")
                    .push(preferred_redirect_uri.map(str::to_owned));
                Ok(Box::new(FakeCallbackSession {
                    response: Some(self.response.clone()),
                    observed_timeout: Arc::clone(&self.observed_timeout),
                }) as Box<dyn McpOAuthCallbackSession>)
            })
        }
    }

    struct FakeClock(AtomicU64);

    impl McpOAuthClock for FakeClock {
        fn now_seconds(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    struct FakeEntropy(AtomicU8);

    impl McpOAuthEntropy for FakeEntropy {
        fn fill(&self, bytes: &mut [u8]) -> Result<(), McpOAuthError> {
            let value = self.0.fetch_add(1, Ordering::SeqCst);
            bytes.fill(value);
            Ok(())
        }
    }

    #[derive(Default)]
    struct MemoryStore(StdMutex<HashMap<String, StoredMcpOAuthState>>);

    impl McpOAuthTokenStore for MemoryStore {
        fn load(&self, key: &str) -> Result<Option<StoredMcpOAuthState>, McpOAuthError> {
            Ok(self.0.lock().expect("store lock").get(key).cloned())
        }

        fn save(&self, key: &str, state: &StoredMcpOAuthState) -> Result<(), McpOAuthError> {
            self.0
                .lock()
                .expect("store lock")
                .insert(key.to_owned(), state.clone());
            Ok(())
        }

        fn remove(&self, key: &str) -> Result<(), McpOAuthError> {
            self.0.lock().expect("store lock").remove(key);
            Ok(())
        }
    }

    fn response(
        status: u16,
        headers: impl IntoIterator<Item = (&'static str, &'static str)>,
        body: serde_json::Value,
    ) -> McpOAuthHttpResponse {
        McpOAuthHttpResponse {
            status,
            headers: headers
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value.to_owned()))
                .collect(),
            body: serde_json::to_vec(&body).expect("JSON fixture"),
        }
    }

    fn authorization_metadata() -> serde_json::Value {
        serde_json::json!({
            "issuer": "https://auth.example.test/tenant",
            "authorization_endpoint": "https://auth.example.test/tenant/authorize",
            "token_endpoint": "https://auth.example.test/tenant/token",
            "registration_endpoint": "https://auth.example.test/tenant/register",
            "code_challenge_methods_supported": ["S256"],
            "scopes_supported": ["mcp.read", "offline_access"],
            "authorization_response_iss_parameter_supported": true
        })
    }

    fn manager(
        http: Arc<dyn McpOAuthHttpClient>,
        browser: Arc<dyn McpOAuthBrowser>,
        listener: Arc<dyn McpOAuthCallbackListener>,
        store: Arc<dyn McpOAuthTokenStore>,
    ) -> Arc<McpOAuthManager> {
        Arc::new(McpOAuthManager::new(
            McpOAuthDependencies {
                http,
                browser,
                listener,
                clock: Arc::new(FakeClock(AtomicU64::new(1_000))),
                entropy: Arc::new(FakeEntropy(AtomicU8::new(1))),
                store,
            },
            McpOAuthOptions {
                http_timeout: Duration::from_secs(7),
                callback_timeout: Duration::from_secs(29),
            },
        ))
    }

    fn callback() -> McpOAuthCallback {
        McpOAuthCallback {
            code: Some(SecretString::new("authorization-code")),
            state: Some(SecretString::new(URL_SAFE_NO_PAD.encode([2_u8; 32]))),
            issuer: Some("https://auth.example.test/tenant".to_owned()),
            error: None,
        }
    }

    #[tokio::test]
    async fn full_flow_uses_discovery_pkce_resource_state_and_private_persistence() {
        let http = Arc::new(FakeHttp::new(vec![
            response(
                401,
                [("www-authenticate", "BEARER resource_metadata=\"https://mcp.example.test/oauth-resource\", scope=\"mcp.read\"")],
                serde_json::json!({}),
            ),
            response(
                200,
                [],
                serde_json::json!({
                    "resource": "https://mcp.example.test/rpc",
                    "authorization_servers": ["https://auth.example.test/tenant"],
                    "scopes_supported": ["metadata.scope"]
                }),
            ),
            response(200, [], authorization_metadata()),
            response(
                201,
                [],
                serde_json::json!({"client_id":"dynamic-client"}),
            ),
            response(
                200,
                [],
                serde_json::json!({
                    "access_token":"access-secret",
                    "refresh_token":"refresh-secret",
                    "token_type":"Bearer",
                    "expires_in":3600,
                    "scope":"mcp.read offline_access"
                }),
            ),
        ]));
        let browser = Arc::new(FakeBrowser::default());
        let listener = Arc::new(FakeCallbackListener::new(callback()));
        let store = Arc::new(MemoryStore::default());
        let oauth_manager = manager(
            http.clone(),
            browser.clone(),
            listener.clone(),
            store.clone(),
        );
        let cancellation = CancellationToken::new();

        let session = oauth_manager
            .session("docs", "https://mcp.example.test/rpc", &cancellation)
            .await
            .expect("OAuth session");
        assert_eq!(
            session
                .access_token(&cancellation)
                .await
                .expect("stored token")
                .expose(),
            "access-secret"
        );

        let requests = http.requests();
        assert_eq!(
            requests
                .iter()
                .map(|request| (request.method.as_str(), request.url.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("GET", "https://mcp.example.test/rpc"),
                ("GET", "https://mcp.example.test/oauth-resource"),
                (
                    "GET",
                    "https://auth.example.test/.well-known/oauth-authorization-server/tenant"
                ),
                ("POST", "https://auth.example.test/tenant/register"),
                ("POST", "https://auth.example.test/tenant/token"),
            ]
        );
        assert!(requests
            .iter()
            .all(|request| request.timeout == Duration::from_secs(7)));

        let registration: serde_json::Value =
            serde_json::from_slice(&requests[3].body).expect("registration JSON");
        assert_eq!(registration["application_type"], "native");
        assert_eq!(registration["token_endpoint_auth_method"], "none");
        assert_eq!(
            registration["redirect_uris"][0],
            "http://127.0.0.1:43117/oauth/callback"
        );

        let authorization = browser.urls.lock().expect("browser lock")[0].clone();
        let authorization = Url::parse(&authorization).expect("authorization URL");
        let query = authorization
            .query_pairs()
            .into_owned()
            .collect::<BTreeMap<_, _>>();
        assert_eq!(query["resource"], "https://mcp.example.test/rpc");
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["state"], URL_SAFE_NO_PAD.encode([2_u8; 32]));
        let verifier = URL_SAFE_NO_PAD.encode([1_u8; 32]);
        assert_eq!(
            query["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
        );
        assert_eq!(query["scope"], "mcp.read offline_access");

        let token_form = form_urlencoded::parse(&requests[4].body)
            .into_owned()
            .collect::<BTreeMap<_, _>>();
        assert_eq!(token_form["resource"], "https://mcp.example.test/rpc");
        assert_eq!(token_form["code_verifier"], verifier);
        assert_eq!(token_form["client_id"], "dynamic-client");
        assert_eq!(
            *listener.observed_timeout.lock().expect("timeout lock"),
            Some(Duration::from_secs(29))
        );

        let key = credential_key("docs", "https://mcp.example.test/rpc");
        let saved = store.load(&key).expect("store").expect("saved state");
        let debug = format!("{saved:?}");
        assert!(!debug.contains("access-secret"));
        assert!(!debug.contains("refresh-secret"));
        assert!(!format!("{:?}", requests[4]).contains("authorization-code"));
    }

    fn stored_state(access_token: &str, expires_at: u64) -> StoredMcpOAuthState {
        StoredMcpOAuthState {
            resource: "https://mcp.example.test/rpc".to_owned(),
            issuer: "https://auth.example.test/tenant".to_owned(),
            token_endpoint: "https://auth.example.test/tenant/token".to_owned(),
            client_id: "client".to_owned(),
            client_secret: None,
            redirect_uri: "http://127.0.0.1:43117/oauth/callback".to_owned(),
            access_token: access_token.to_owned(),
            refresh_token: Some("refresh-secret".to_owned()),
            expires_at: Some(expires_at),
            scopes: vec!["mcp.read".to_owned()],
        }
    }

    fn direct_session(manager: Arc<McpOAuthManager>, key: &str) -> Arc<McpOAuthSession> {
        Arc::new(McpOAuthSession {
            manager,
            key: key.to_owned(),
            context: OAuthContext {
                resource: "https://mcp.example.test/rpc".to_owned(),
                authorization: AuthorizationServerMetadata {
                    issuer: "https://auth.example.test/tenant".to_owned(),
                    authorization_endpoint: "https://auth.example.test/tenant/authorize".to_owned(),
                    token_endpoint: "https://auth.example.test/tenant/token".to_owned(),
                    registration_endpoint: None,
                    scopes_supported: Vec::new(),
                    authorization_response_iss_parameter_supported: false,
                },
                scopes: vec!["mcp.read".to_owned()],
            },
        })
    }

    #[tokio::test]
    async fn expiry_and_unauthorized_races_refresh_exactly_once() {
        let http = Arc::new(FakeHttp::new(vec![response(
            200,
            [],
            serde_json::json!({
                "access_token":"fresh-token",
                "token_type":"Bearer",
                "expires_in":3600
            }),
        )]));
        let store = Arc::new(MemoryStore::default());
        let key = credential_key("docs", "https://mcp.example.test/rpc");
        store
            .save(&key, &stored_state("stale-token", 1_000))
            .expect("seed state");
        let refresh_manager = manager(
            http.clone(),
            Arc::new(FakeBrowser::default()),
            Arc::new(FakeCallbackListener::new(callback())),
            store,
        );
        let session = direct_session(refresh_manager, &key);
        let cancellation = CancellationToken::new();

        let (first, second) = tokio::join!(
            session.access_token(&cancellation),
            session.access_token(&cancellation)
        );
        assert_eq!(first.expect("first").expose(), "fresh-token");
        assert_eq!(second.expect("second").expose(), "fresh-token");
        let rejected = SecretString::new("stale-token");
        let (first, second) = tokio::join!(
            session.refresh_after_unauthorized(&rejected, &cancellation),
            session.refresh_after_unauthorized(&rejected, &cancellation)
        );
        assert_eq!(first.expect("first 401").expose(), "fresh-token");
        assert_eq!(second.expect("second 401").expose(), "fresh-token");
        assert_eq!(http.requests().len(), 1);
        let refresh_form = form_urlencoded::parse(&http.requests()[0].body)
            .into_owned()
            .collect::<BTreeMap<_, _>>();
        assert_eq!(refresh_form["grant_type"], "refresh_token");
        assert_eq!(refresh_form["resource"], "https://mcp.example.test/rpc");
    }

    #[tokio::test]
    async fn callback_state_and_issuer_are_verified_before_token_exchange() {
        for callback in [
            McpOAuthCallback {
                code: Some(SecretString::new("authorization-code")),
                state: Some(SecretString::new("wrong-state")),
                issuer: Some("https://auth.example.test/tenant".to_owned()),
                error: None,
            },
            McpOAuthCallback {
                code: Some(SecretString::new("authorization-code")),
                state: Some(SecretString::new(URL_SAFE_NO_PAD.encode([2_u8; 32]))),
                issuer: Some("https://attacker.example.test".to_owned()),
                error: None,
            },
        ] {
            let http = Arc::new(FakeHttp::new(Vec::new()));
            let oauth_manager = manager(
                http.clone(),
                Arc::new(FakeBrowser::default()),
                Arc::new(FakeCallbackListener::new(callback)),
                Arc::new(MemoryStore::default()),
            );
            let session = direct_session(oauth_manager, "state-test");
            let previous = stored_state("access-token", 2_000);
            let error = session
                .authorize(Some(&previous), &CancellationToken::new())
                .await
                .expect_err("callback binding must be verified");
            assert!(
                error.message().contains("state does not match")
                    || error.message().contains("issuer does not match")
            );
            assert!(http.requests().is_empty());
        }
    }

    #[tokio::test]
    async fn cancellation_and_discovery_fail_closed_without_fallback() {
        let http = Arc::new(FakeHttp::new(Vec::new()));
        let cancelled_manager = manager(
            http.clone(),
            Arc::new(FakeBrowser::default()),
            Arc::new(FakeCallbackListener::new(callback())),
            Arc::new(MemoryStore::default()),
        );
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = cancelled_manager
            .session("docs", "https://mcp.example.test/rpc", &cancellation)
            .await
            .err()
            .expect("cancelled");
        assert!(error.is_cancelled());
        assert!(http.requests().is_empty());

        let redirect = Arc::new(FakeHttp::new(vec![response(
            302,
            [("location", "https://elsewhere.example.test")],
            serde_json::json!({}),
        )]));
        let redirect_manager = manager(
            redirect,
            Arc::new(FakeBrowser::default()),
            Arc::new(FakeCallbackListener::new(callback())),
            Arc::new(MemoryStore::default()),
        );
        let error = redirect_manager
            .session(
                "docs",
                "https://mcp.example.test/rpc",
                &CancellationToken::new(),
            )
            .await
            .err()
            .expect("discovery must not follow or accept redirects");
        assert_eq!(error.message(), "MCP OAuth redirects are not allowed");
    }

    #[test]
    fn discovery_paths_challenges_and_url_policy_match_the_wire_contract() {
        assert_eq!(
            protected_resource_metadata_urls("https://mcp.example.test/a/b").expect("paths"),
            vec![
                "https://mcp.example.test/.well-known/oauth-protected-resource/a/b",
                "https://mcp.example.test/.well-known/oauth-protected-resource",
            ]
        );
        assert_eq!(
            authorization_metadata_urls("https://auth.example.test/tenant").expect("paths"),
            vec![
                "https://auth.example.test/.well-known/oauth-authorization-server/tenant",
                "https://auth.example.test/.well-known/openid-configuration/tenant",
                "https://auth.example.test/tenant/.well-known/openid-configuration",
            ]
        );
        let challenge = parse_bearer_challenge(
            "Basic realm=\"legacy\", BeArEr resource_metadata=\"https://mcp.example.test/meta\", scope=\"a b\"",
        )
        .expect("Bearer challenge");
        assert_eq!(
            challenge.resource_metadata.as_deref(),
            Some("https://mcp.example.test/meta")
        );
        assert_eq!(challenge.scope.as_deref(), Some("a b"));

        for blocked in [
            "https://127.0.0.1/meta",
            "https://10.0.0.1/meta",
            "https://100.64.0.1/meta",
            "https://169.254.169.254/meta",
            "https://192.0.2.1/meta",
            "https://[::1]/meta",
            "https://[::ffff:127.0.0.1]/meta",
            "https://[2001:db8::1]/meta",
        ] {
            assert!(
                validate_oauth_url("endpoint", blocked, false).is_err(),
                "{blocked}"
            );
        }
        assert!(validate_oauth_url("endpoint", "https://auth.example.test/token", false).is_ok());
        assert!(validate_resource_metadata_url(
            "https://mcp.example.test/rpc",
            "https://attacker.example.test/meta"
        )
        .is_err());
        let remote = Url::parse("https://auth.example.test/token").expect("remote URL");
        assert!(validate_resolved_addresses(
            &remote,
            &["93.184.216.34:443".parse().expect("public address")]
        )
        .is_ok());
        assert!(validate_resolved_addresses(
            &remote,
            &["169.254.169.254:443".parse().expect("metadata address")]
        )
        .is_err());
        let loopback = Url::parse("http://localhost:43117/callback").expect("loopback URL");
        assert!(validate_resolved_addresses(
            &loopback,
            &["127.0.0.1:43117".parse().expect("loopback address")]
        )
        .is_ok());
        assert_eq!(
            validate_oauth_url("issuer", "https://auth.example.test/", true).expect("issuer"),
            "https://auth.example.test/"
        );
    }

    #[tokio::test]
    async fn discovery_fallback_is_ordered_and_metadata_is_strict() {
        let fallback_http = Arc::new(FakeHttp::new(vec![
            response(404, [], serde_json::json!({})),
            response(
                200,
                [],
                serde_json::json!({
                    "resource":"https://mcp.example.test/rpc",
                    "authorization_servers":["https://auth.example.test/tenant"]
                }),
            ),
        ]));
        let oauth_manager = manager(
            fallback_http.clone(),
            Arc::new(FakeBrowser::default()),
            Arc::new(FakeCallbackListener::new(callback())),
            Arc::new(MemoryStore::default()),
        );
        let metadata = oauth_manager
            .discover_protected_resource(
                "https://mcp.example.test/rpc",
                None,
                &CancellationToken::new(),
            )
            .await
            .expect("root fallback");
        assert_eq!(metadata.issuer, "https://auth.example.test/tenant");
        assert_eq!(
            fallback_http
                .requests()
                .iter()
                .map(|request| request.url.as_str())
                .collect::<Vec<_>>(),
            vec![
                "https://mcp.example.test/.well-known/oauth-protected-resource/rpc",
                "https://mcp.example.test/.well-known/oauth-protected-resource",
            ]
        );

        for (metadata, expected) in [
            (
                serde_json::json!({
                    "issuer":"https://other.example.test/tenant",
                    "authorization_endpoint":"https://auth.example.test/authorize",
                    "token_endpoint":"https://auth.example.test/token",
                    "code_challenge_methods_supported":["S256"]
                }),
                "issuer does not match",
            ),
            (
                serde_json::json!({
                    "issuer":"https://auth.example.test/tenant",
                    "authorization_endpoint":"https://auth.example.test/authorize",
                    "token_endpoint":"https://auth.example.test/token",
                    "code_challenge_methods_supported":["plain"]
                }),
                "does not advertise S256 PKCE",
            ),
        ] {
            let strict_http = Arc::new(FakeHttp::new(vec![response(200, [], metadata)]));
            let oauth_manager = manager(
                strict_http,
                Arc::new(FakeBrowser::default()),
                Arc::new(FakeCallbackListener::new(callback())),
                Arc::new(MemoryStore::default()),
            );
            let error = oauth_manager
                .discover_authorization_server(
                    "https://auth.example.test/tenant",
                    &CancellationToken::new(),
                )
                .await
                .expect_err("strict metadata rejection");
            assert!(error.message().contains(expected), "{error}");
        }
    }

    #[test]
    fn parsers_are_bounded_and_debug_output_redacts_credentials() {
        let too_large = vec![b' '; MAX_OAUTH_DOCUMENT_BYTES + 1];
        assert!(parse_json_object(&too_large, "metadata").is_err());
        assert!(parse_json_object(b"[]", "metadata").is_err());
        let request = McpOAuthHttpRequest {
            method: "POST".to_owned(),
            url: "https://user:password@auth.example.test/token?code=secret".to_owned(),
            headers: BTreeMap::from([("authorization".to_owned(), "Bearer secret".to_owned())]),
            body: b"refresh_token=secret".to_vec(),
            timeout: Duration::from_secs(5),
        };
        let debug = format!("{request:?}");
        assert!(!debug.contains("password"));
        assert!(!debug.contains("code=secret"));
        assert!(!debug.contains("Bearer secret"));
        assert!(!debug.contains("refresh_token=secret"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_store_is_atomic_private_and_rejects_symlinks() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let home = tempfile::tempdir().expect("home");
        let store = FileMcpOAuthTokenStore::under_mycel_home(home.path());
        let key = credential_key("docs", "https://mcp.example.test/rpc");
        store
            .save(&key, &stored_state("access-secret", 2_000))
            .expect("save");
        let directory = home.path().join("credentials/mcp-oauth");
        let file = directory.join(format!("{key}.json"));
        assert_eq!(
            fs::metadata(&directory)
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&file)
                .expect("file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            store.load(&key).expect("load").expect("state").access_token,
            "access-secret"
        );

        fs::remove_file(&file).expect("remove credential");
        symlink(home.path().join("outside"), &file).expect("credential symlink");
        assert!(store.load(&key).is_err());

        let second_home = tempfile::tempdir().expect("second home");
        fs::create_dir(second_home.path().join("credentials")).expect("credentials parent");
        fs::remove_dir(second_home.path().join("credentials")).expect("remove parent");
        symlink(
            home.path().join("outside"),
            second_home.path().join("credentials"),
        )
        .expect("parent symlink");
        let unsafe_store = FileMcpOAuthTokenStore::under_mycel_home(second_home.path());
        assert!(unsafe_store
            .save(&key, &stored_state("access-secret", 2_000))
            .is_err());

        let lock_path = store
            .refresh_lock_path(&key)
            .expect("lock path")
            .expect("file lock");
        let first =
            FileMcpOAuthCredentialLock::acquire(lock_path.clone(), &CancellationToken::new())
                .await
                .expect("first lock");
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let error = FileMcpOAuthCredentialLock::acquire(lock_path.clone(), &cancelled)
            .await
            .err()
            .expect("cancelled waiter");
        assert!(error.is_cancelled());
        drop(first);
        let second =
            FileMcpOAuthCredentialLock::acquire(lock_path.clone(), &CancellationToken::new())
                .await
                .expect("second lock");
        drop(second);
        assert!(!lock_path.exists());
    }

    #[test]
    fn per_server_credentials_are_isolated() {
        let resource = "https://mcp.example.test/rpc";
        assert_ne!(
            credential_key("docs", resource),
            credential_key("source", resource)
        );
        assert_ne!(
            credential_key("docs", resource),
            credential_key("docs", "https://other.example.test/rpc")
        );
    }
}
