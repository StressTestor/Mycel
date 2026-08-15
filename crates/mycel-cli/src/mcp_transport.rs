//! Production MCP transports for the Rust CLI.
//!
//! The agent runtime owns protocol negotiation and tool registration. This
//! module owns only process and HTTP I/O: argv-safe stdio children and
//! redirect-free Streamable HTTP requests with bounded JSON/SSE framing.

use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::Path,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

use futures_util::StreamExt;
use mycel_agent_protocol::SecretString;
use mycel_agent_runtime::{
    CancellationToken, McpConnectedTransport, McpConnectionPurpose, McpFuture,
    McpHttpConnectRequest, McpPeer, McpProtocolEra, McpRequest, McpRequestError,
    McpStdioConnectRequest, McpTransportConnector, McpTransportError, McpTransportEvent,
    McpTransportEvents,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{mpsc, oneshot, Mutex, RwLock},
};

use crate::mcp_oauth::{McpOAuthManager, McpOAuthSession};

const MAX_WIRE_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_HTTP_ERROR_BYTES: usize = 1024 * 1024;
const MCP_SESSION_ID: HeaderName = HeaderName::from_static("mcp-session-id");

/// Real process/HTTP connector used by the production CLI.
#[derive(Clone)]
pub struct ProcessMcpConnector {
    http: reqwest::Client,
    oauth: Arc<McpOAuthManager>,
}

impl ProcessMcpConnector {
    /// Build the production connector. OAuth credentials are persisted below
    /// `<mycel_home>/credentials/mcp-oauth`.
    pub fn new(mycel_home: impl AsRef<Path>) -> Result<Self, McpTransportError> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| {
                McpTransportError::Failed("could not initialize MCP HTTP client".into())
            })?;
        let oauth = Arc::new(McpOAuthManager::production(mycel_home.as_ref()));
        Ok(Self { http, oauth })
    }

    /// Injection seam for deterministic OAuth and connector tests. The HTTP
    /// client must reject redirects.
    pub fn with_oauth_manager(http: reqwest::Client, oauth: Arc<McpOAuthManager>) -> Self {
        Self { http, oauth }
    }
}

impl McpTransportConnector for ProcessMcpConnector {
    fn connect_stdio<'a>(
        &'a self,
        request: McpStdioConnectRequest,
        cancellation: &'a CancellationToken,
    ) -> McpFuture<'a, Result<McpConnectedTransport, McpTransportError>> {
        Box::pin(async move { connect_stdio(request, cancellation).await })
    }

    fn connect_streamable_http<'a>(
        &'a self,
        request: McpHttpConnectRequest,
        cancellation: &'a CancellationToken,
    ) -> McpFuture<'a, Result<McpConnectedTransport, McpTransportError>> {
        let client = self.http.clone();
        let oauth = Arc::clone(&self.oauth);
        Box::pin(async move { connect_http(client, oauth, request, cancellation).await })
    }
}

struct ChannelEvents {
    receiver: mpsc::UnboundedReceiver<McpTransportEvent>,
}

impl McpTransportEvents for ChannelEvents {
    fn next<'a>(&'a mut self) -> McpFuture<'a, Option<McpTransportEvent>> {
        Box::pin(self.receiver.recv())
    }
}

struct StdioPeer {
    writer: Mutex<ChildStdin>,
    child: Mutex<Child>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, McpRequestError>>>>,
    next_id: AtomicU64,
    closed: AtomicBool,
    events: mpsc::UnboundedSender<McpTransportEvent>,
}

async fn connect_stdio(
    request: McpStdioConnectRequest,
    cancellation: &CancellationToken,
) -> Result<McpConnectedTransport, McpTransportError> {
    if cancellation.is_cancelled() {
        return Err(McpTransportError::Cancelled);
    }
    if request.command.trim().is_empty() {
        return Err(McpTransportError::Failed(
            "MCP stdio command must not be empty".into(),
        ));
    }
    let mut command = Command::new(&request.command);
    command
        .args(&request.args)
        .envs(&request.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(cwd) = &request.cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().map_err(|error| {
        McpTransportError::Failed(format!("could not start MCP stdio server: {error}"))
    })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| McpTransportError::Failed("MCP stdio server has no stdin pipe".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| McpTransportError::Failed("MCP stdio server has no stdout pipe".into()))?;
    if cancellation.is_cancelled() {
        let _ = child.kill().await;
        return Err(McpTransportError::Cancelled);
    }

    let (events, receiver) = mpsc::unbounded_channel();
    let peer = Arc::new(StdioPeer {
        writer: Mutex::new(stdin),
        child: Mutex::new(child),
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        closed: AtomicBool::new(false),
        events,
    });
    tokio::spawn(read_stdio(peer.clone(), stdout, request.purpose));
    Ok(McpConnectedTransport {
        peer,
        events: Box::new(ChannelEvents { receiver }),
    })
}

impl StdioPeer {
    async fn send(&self, value: &Value) -> Result<(), McpTransportError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(McpTransportError::Closed);
        }
        let mut bytes = serde_json::to_vec(value)
            .map_err(|_| McpTransportError::Failed("could not encode MCP request".into()))?;
        if bytes.len() > MAX_WIRE_MESSAGE_BYTES {
            return Err(McpTransportError::Failed(
                "MCP request exceeds the wire message limit".into(),
            ));
        }
        bytes.push(b'\n');
        let mut writer = self.writer.lock().await;
        writer.write_all(&bytes).await.map_err(|error| {
            McpTransportError::Failed(format!("could not write MCP stdio request: {error}"))
        })?;
        writer.flush().await.map_err(|error| {
            McpTransportError::Failed(format!("could not flush MCP stdio request: {error}"))
        })
    }

    async fn fail_pending(&self, error: McpTransportError) {
        let pending = std::mem::take(&mut *self.pending.lock().await);
        for sender in pending.into_values() {
            let _ = sender.send(Err(McpRequestError::Transport(error.clone())));
        }
    }

    async fn mark_closed(&self, message: Option<String>) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.fail_pending(McpTransportError::Closed).await;
        let _ = self
            .events
            .send(McpTransportEvent::Closed { error: message });
    }
}

impl McpPeer for StdioPeer {
    fn request<'a>(
        &'a self,
        request: McpRequest,
        cancellation: &'a CancellationToken,
    ) -> McpFuture<'a, Result<Value, McpRequestError>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(McpRequestError::Transport(McpTransportError::Cancelled));
            }
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let (sender, receiver) = oneshot::channel();
            self.pending.lock().await.insert(id, sender);
            let value = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": request.method,
                "params": request.params,
            });
            if let Err(error) = self.send(&value).await {
                self.pending.lock().await.remove(&id);
                return Err(McpRequestError::Transport(error));
            }
            tokio::select! {
                response = receiver => response.unwrap_or({
                    Err(McpRequestError::Transport(McpTransportError::Closed))
                }),
                () = cancellation.cancelled() => {
                    self.pending.lock().await.remove(&id);
                    let _ = self.send(&json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/cancelled",
                        "params": { "requestId": id, "reason": "caller cancelled" },
                    })).await;
                    Err(McpRequestError::Transport(McpTransportError::Cancelled))
                }
            }
        })
    }

    fn notify<'a>(&'a self, request: McpRequest) -> McpFuture<'a, Result<(), McpRequestError>> {
        Box::pin(async move {
            self.send(&json!({
                "jsonrpc": "2.0",
                "method": request.method,
                "params": request.params,
            }))
            .await
            .map_err(McpRequestError::Transport)
        })
    }

    fn close<'a>(&'a self) -> McpFuture<'a, Result<(), McpTransportError>> {
        Box::pin(async move {
            if self.closed.swap(true, Ordering::AcqRel) {
                return Ok(());
            }
            self.fail_pending(McpTransportError::Closed).await;
            let mut child = self.child.lock().await;
            match child.kill().await {
                Ok(()) => {
                    let _ = child.wait().await;
                    Ok(())
                }
                Err(error) if error.kind() == io::ErrorKind::InvalidInput => Ok(()),
                Err(error) => Err(McpTransportError::Failed(format!(
                    "could not stop MCP stdio server: {error}"
                ))),
            }
        })
    }
}

async fn read_stdio(peer: Arc<StdioPeer>, stdout: ChildStdout, purpose: McpConnectionPurpose) {
    let mut reader = BufReader::new(stdout);
    loop {
        let bytes = match read_bounded_line(&mut reader, MAX_WIRE_MESSAGE_BYTES).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                peer.mark_closed(Some(format!(
                    "MCP {:?} stdio process closed stdout",
                    purpose
                )))
                .await;
                return;
            }
            Err(error) => {
                peer.mark_closed(Some(error.to_string())).await;
                return;
            }
        };
        if bytes.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) => {
                peer.mark_closed(Some("MCP stdio server emitted invalid JSON".into()))
                    .await;
                return;
            }
        };
        dispatch_stdio_message(&peer, value).await;
    }
}

async fn dispatch_stdio_message(peer: &Arc<StdioPeer>, value: Value) {
    if value.get("method").is_some() {
        if let Some(id) = value.get("id").cloned() {
            let _ = peer
                .send(&json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "client method not supported" },
                }))
                .await;
            return;
        }
    }
    if let Some(id) = value.get("id").and_then(Value::as_u64) {
        if let Some(sender) = peer.pending.lock().await.remove(&id) {
            let _ = sender.send(parse_rpc_response(value, None));
        }
        return;
    }
    dispatch_notification(&peer.events, &value);
}

async fn read_bounded_line(
    reader: &mut BufReader<ChildStdout>,
    limit: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut output = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if output.is_empty() {
                Ok(None)
            } else {
                Ok(Some(output))
            };
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if output.len().saturating_add(take) > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP stdio message exceeds the wire message limit",
            ));
        }
        output.extend_from_slice(&available[..take]);
        reader.consume(take);
        if output.last() == Some(&b'\n') {
            return Ok(Some(output));
        }
    }
}

struct HttpPeer {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    oauth: Option<Arc<McpOAuthSession>>,
    session_id: RwLock<Option<HeaderValue>>,
    next_id: AtomicU64,
    closed: AtomicBool,
    events: mpsc::UnboundedSender<McpTransportEvent>,
}

async fn connect_http(
    client: reqwest::Client,
    oauth_manager: Arc<McpOAuthManager>,
    request: McpHttpConnectRequest,
    cancellation: &CancellationToken,
) -> Result<McpConnectedTransport, McpTransportError> {
    if cancellation.is_cancelled() {
        return Err(McpTransportError::Cancelled);
    }
    validate_http_endpoint(&request.url)?;
    let headers = header_map(&request.headers)?;
    if request.auth.is_some() && headers.contains_key(AUTHORIZATION) {
        return Err(McpTransportError::Failed(
            "OAuth-enabled MCP configuration must not provide an Authorization header".into(),
        ));
    }
    let oauth = if request.auth.is_some() {
        Some(
            oauth_manager
                .session(&request.server_name, &request.url, cancellation)
                .await
                .map_err(oauth_transport_error)?,
        )
    } else {
        None
    };
    let (events, receiver) = mpsc::unbounded_channel();
    let peer = Arc::new(HttpPeer {
        client,
        url: request.url,
        headers,
        oauth,
        session_id: RwLock::new(None),
        next_id: AtomicU64::new(1),
        closed: AtomicBool::new(false),
        events,
    });
    Ok(McpConnectedTransport {
        peer,
        events: Box::new(ChannelEvents { receiver }),
    })
}

impl HttpPeer {
    async fn headers_for(
        &self,
        request: &McpRequest,
        token: Option<&SecretString>,
    ) -> Result<HeaderMap, McpRequestError> {
        let mut headers = self.headers.clone();
        for (name, value) in &request.http_headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                McpRequestError::Transport(McpTransportError::Failed(
                    "MCP request contains an invalid HTTP header name".into(),
                ))
            })?;
            if self.oauth.is_some() && name == AUTHORIZATION {
                return Err(McpRequestError::Transport(McpTransportError::Failed(
                    "OAuth-enabled MCP requests must not override the Authorization header".into(),
                )));
            }
            let value = HeaderValue::from_str(value).map_err(|_| {
                McpRequestError::Transport(McpTransportError::Failed(
                    "MCP request contains an invalid HTTP header value".into(),
                ))
            })?;
            headers.insert(name, value);
        }
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        if let Some(token) = token {
            headers.insert(AUTHORIZATION, bearer_header(token)?);
        }
        if request.era == McpProtocolEra::Legacy {
            if let Some(session_id) = self.session_id.read().await.clone() {
                headers.insert(MCP_SESSION_ID, session_id);
            }
        }
        Ok(headers)
    }

    async fn send_request(
        &self,
        request: McpRequest,
        id: Option<u64>,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Option<Value>, McpRequestError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(McpRequestError::Transport(McpTransportError::Closed));
        }
        let local_cancellation = CancellationToken::new();
        let cancellation = cancellation.unwrap_or(&local_cancellation);
        let mut token = match &self.oauth {
            Some(oauth) => Some(
                oauth
                    .access_token(cancellation)
                    .await
                    .map_err(oauth_request_error)?,
            ),
            None => None,
        };
        let body = match id {
            Some(id) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": &request.method,
                "params": &request.params,
            }),
            None => json!({
                "jsonrpc": "2.0",
                "method": &request.method,
                "params": &request.params,
            }),
        };
        let body = serde_json::to_vec(&body).map_err(|_| {
            McpRequestError::Transport(McpTransportError::Failed(
                "could not encode MCP HTTP request".into(),
            ))
        })?;
        if body.len() > MAX_WIRE_MESSAGE_BYTES {
            return Err(McpRequestError::Transport(McpTransportError::Failed(
                "MCP request exceeds the wire message limit".into(),
            )));
        }
        let mut retried_unauthorized = false;
        let response = loop {
            let headers = self.headers_for(&request, token.as_ref()).await?;
            let send = self
                .client
                .post(&self.url)
                .headers(headers)
                .body(body.clone())
                .send();
            let response = tokio::select! {
                response = send => response,
                () = cancellation.cancelled() => {
                    return Err(McpRequestError::Transport(McpTransportError::Cancelled));
                }
            }
            .map_err(|error| {
                let message = if error.is_timeout() {
                    "MCP HTTP request timed out"
                } else {
                    "MCP HTTP request failed"
                };
                McpRequestError::Transport(McpTransportError::Failed(message.into()))
            })?;
            if response.status() != reqwest::StatusCode::UNAUTHORIZED
                || retried_unauthorized
                || self.oauth.is_none()
            {
                break response;
            }
            let rejected = token.as_ref().ok_or_else(|| {
                McpRequestError::Transport(McpTransportError::Failed(
                    "OAuth-enabled MCP request has no access token".into(),
                ))
            })?;
            let oauth = self.oauth.as_ref().ok_or_else(|| {
                McpRequestError::Transport(McpTransportError::Failed(
                    "OAuth-enabled MCP request has no OAuth session".into(),
                ))
            })?;
            token = Some(
                oauth
                    .refresh_after_unauthorized(rejected, cancellation)
                    .await
                    .map_err(oauth_request_error)?,
            );
            retried_unauthorized = true;
        };
        let status = response.status();
        if request.era == McpProtocolEra::Legacy {
            if let Some(session_id) = response.headers().get(MCP_SESSION_ID).cloned() {
                *self.session_id.write().await = Some(session_id);
            }
        }
        if id.is_none() && status.is_success() {
            return Ok(None);
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if content_type.starts_with("text/event-stream") {
            return self
                .read_sse(response, id, status.as_u16(), Some(cancellation))
                .await
                .map(Some);
        }
        let limit = if status.is_success() {
            MAX_WIRE_MESSAGE_BYTES
        } else {
            MAX_HTTP_ERROR_BYTES
        };
        let bytes = collect_response(response, limit, Some(cancellation)).await?;
        if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
            if let Some(id) = id {
                if value.get("id").and_then(Value::as_u64) != Some(id) {
                    return Err(McpRequestError::Transport(McpTransportError::Failed(
                        "MCP HTTP response id does not match the request".into(),
                    )));
                }
            }
            return parse_rpc_response(value, Some(status.as_u16())).map(Some);
        }
        if !status.is_success() {
            return Err(McpRequestError::Http {
                status: status.as_u16(),
                message: status
                    .canonical_reason()
                    .unwrap_or("request failed")
                    .to_owned(),
            });
        }
        Err(McpRequestError::Transport(McpTransportError::Failed(
            "MCP HTTP server returned invalid JSON".into(),
        )))
    }

    async fn read_sse(
        &self,
        response: reqwest::Response,
        expected_id: Option<u64>,
        status: u16,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Value, McpRequestError> {
        let mut stream = response.bytes_stream();
        let mut buffer = Vec::new();
        loop {
            let next = if let Some(cancellation) = cancellation {
                tokio::select! {
                    chunk = stream.next() => chunk,
                    () = cancellation.cancelled() => {
                        return Err(McpRequestError::Transport(McpTransportError::Cancelled));
                    }
                }
            } else {
                stream.next().await
            };
            let Some(chunk) = next else {
                break;
            };
            let chunk = chunk.map_err(|_| {
                McpRequestError::Transport(McpTransportError::Failed(
                    "MCP HTTP event stream failed".into(),
                ))
            })?;
            if buffer.len().saturating_add(chunk.len()) > MAX_WIRE_MESSAGE_BYTES {
                return Err(McpRequestError::Transport(McpTransportError::Failed(
                    "MCP SSE event exceeds the wire message limit".into(),
                )));
            }
            buffer.extend_from_slice(&chunk);
            while let Some(event) = take_sse_event(&mut buffer) {
                let Some(value) = parse_sse_data(&event)? else {
                    continue;
                };
                if value.get("method").is_some() && value.get("id").is_some() {
                    self.reject_reverse_request(value["id"].clone()).await;
                    continue;
                }
                if value.get("method").is_some() && value.get("id").is_none() {
                    dispatch_notification(&self.events, &value);
                    continue;
                }
                if expected_id.is_none() || value.get("id").and_then(Value::as_u64) == expected_id {
                    return parse_rpc_response(value, Some(status));
                }
            }
        }
        Err(McpRequestError::Transport(McpTransportError::Closed))
    }

    async fn reject_reverse_request(&self, id: Value) {
        let cancellation = CancellationToken::new();
        let mut headers = self.headers.clone();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        if let Some(oauth) = &self.oauth {
            let Ok(token) = oauth.access_token(&cancellation).await else {
                return;
            };
            let Ok(value) = bearer_header_transport(&token) else {
                return;
            };
            headers.insert(AUTHORIZATION, value);
        }
        if let Some(session_id) = self.session_id.read().await.clone() {
            headers.insert(MCP_SESSION_ID, session_id);
        }
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": "client method not supported" },
        });
        if let Ok(body) = serde_json::to_vec(&body) {
            let _ = self
                .client
                .post(&self.url)
                .headers(headers)
                .body(body)
                .send()
                .await;
        }
    }
}

impl McpPeer for HttpPeer {
    fn request<'a>(
        &'a self,
        request: McpRequest,
        cancellation: &'a CancellationToken,
    ) -> McpFuture<'a, Result<Value, McpRequestError>> {
        Box::pin(async move {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            self.send_request(request, Some(id), Some(cancellation))
                .await?
                .ok_or(McpRequestError::Transport(McpTransportError::Closed))
        })
    }

    fn notify<'a>(&'a self, request: McpRequest) -> McpFuture<'a, Result<(), McpRequestError>> {
        Box::pin(async move {
            self.send_request(request, None, None).await?;
            Ok(())
        })
    }

    fn close<'a>(&'a self) -> McpFuture<'a, Result<(), McpTransportError>> {
        Box::pin(async move {
            if self.closed.swap(true, Ordering::AcqRel) {
                return Ok(());
            }
            let session_id = self.session_id.write().await.take();
            if let Some(session_id) = session_id {
                let mut headers = self.headers.clone();
                headers.insert(MCP_SESSION_ID, session_id);
                if let Some(oauth) = &self.oauth {
                    let cancellation = CancellationToken::new();
                    let Ok(token) = oauth.access_token(&cancellation).await else {
                        return Ok(());
                    };
                    let Ok(value) = bearer_header_transport(&token) else {
                        return Ok(());
                    };
                    headers.insert(AUTHORIZATION, value);
                }
                let _ = self.client.delete(&self.url).headers(headers).send().await;
            }
            Ok(())
        })
    }
}

fn bearer_header(token: &SecretString) -> Result<HeaderValue, McpRequestError> {
    bearer_header_transport(token).map_err(McpRequestError::Transport)
}

fn bearer_header_transport(token: &SecretString) -> Result<HeaderValue, McpTransportError> {
    HeaderValue::from_str(&format!("Bearer {}", token.expose()))
        .map_err(|_| McpTransportError::Failed("MCP OAuth returned an invalid bearer token".into()))
}

fn oauth_transport_error(error: crate::mcp_oauth::McpOAuthError) -> McpTransportError {
    if error.is_cancelled() {
        McpTransportError::Cancelled
    } else {
        McpTransportError::Failed(error.to_string())
    }
}

fn oauth_request_error(error: crate::mcp_oauth::McpOAuthError) -> McpRequestError {
    McpRequestError::Transport(oauth_transport_error(error))
}

fn validate_http_endpoint(value: &str) -> Result<(), McpTransportError> {
    let url = reqwest::Url::parse(value)
        .map_err(|_| McpTransportError::Failed("MCP HTTP URL is invalid".into()))?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(McpTransportError::Failed(
            "MCP HTTP URL must not contain credentials or a fragment".into(),
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| McpTransportError::Failed("MCP HTTP URL has no host".into()))?;
    match url.scheme() {
        "https" => Ok(()),
        "http" if matches!(host, "localhost" | "127.0.0.1" | "::1") => Ok(()),
        "http" => Err(McpTransportError::Failed(
            "plaintext MCP HTTP is restricted to loopback".into(),
        )),
        _ => Err(McpTransportError::Failed(
            "MCP endpoint must use HTTP or HTTPS".into(),
        )),
    }
}

fn header_map(values: &BTreeMap<String, String>) -> Result<HeaderMap, McpTransportError> {
    let mut headers = HeaderMap::new();
    for (name, value) in values {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            McpTransportError::Failed("MCP configuration has an invalid HTTP header name".into())
        })?;
        let value = HeaderValue::from_str(value).map_err(|_| {
            McpTransportError::Failed("MCP configuration has an invalid HTTP header value".into())
        })?;
        headers.insert(name, value);
    }
    Ok(headers)
}

async fn collect_response(
    response: reqwest::Response,
    limit: usize,
    cancellation: Option<&CancellationToken>,
) -> Result<Vec<u8>, McpRequestError> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    loop {
        let next = if let Some(cancellation) = cancellation {
            tokio::select! {
                chunk = stream.next() => chunk,
                () = cancellation.cancelled() => {
                    return Err(McpRequestError::Transport(McpTransportError::Cancelled));
                }
            }
        } else {
            stream.next().await
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.map_err(|_| {
            McpRequestError::Transport(McpTransportError::Failed(
                "MCP HTTP response body failed".into(),
            ))
        })?;
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(McpRequestError::Transport(McpTransportError::Failed(
                "MCP HTTP response exceeds the wire message limit".into(),
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn parse_rpc_response(value: Value, http_status: Option<u16>) -> Result<Value, McpRequestError> {
    if let Some(error) = value.get("error").and_then(Value::as_object) {
        return Err(McpRequestError::JsonRpc {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(-32603),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("MCP server returned an error")
                .to_owned(),
            data: error.get("data").cloned(),
            http_status,
        });
    }
    value.get("result").cloned().ok_or_else(|| {
        McpRequestError::Transport(McpTransportError::Failed(
            "MCP response contains neither result nor error".into(),
        ))
    })
}

fn dispatch_notification(sender: &mpsc::UnboundedSender<McpTransportEvent>, value: &Value) {
    if matches!(
        value.get("method").and_then(Value::as_str),
        Some("notifications/tools/list_changed" | "notifications/tools/listChanged")
    ) {
        let _ = sender.send(McpTransportEvent::ToolsListChanged);
    }
}

fn take_sse_event(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let (position, delimiter) = buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|position| (position, 2))
        .or_else(|| {
            buffer
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|position| (position, 4))
        })?;
    let event = buffer[..position].to_vec();
    buffer.drain(..position + delimiter);
    Some(event)
}

fn parse_sse_data(event: &[u8]) -> Result<Option<Value>, McpRequestError> {
    let text = std::str::from_utf8(event).map_err(|_| {
        McpRequestError::Transport(McpTransportError::Failed(
            "MCP SSE event is not UTF-8".into(),
        ))
    })?;
    let data = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&data).map(Some).map_err(|_| {
        McpRequestError::Transport(McpTransportError::Failed(
            "MCP SSE data is not valid JSON".into(),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn endpoint_policy_is_explicit_and_credential_safe() {
        assert!(validate_http_endpoint("https://mcp.example.test/rpc").is_ok());
        assert!(validate_http_endpoint("http://127.0.0.1:3118/rpc").is_ok());
        assert!(validate_http_endpoint("http://remote.example.test/rpc").is_err());
        assert!(validate_http_endpoint("https://user:secret@example.test/rpc").is_err());
        assert!(validate_http_endpoint("https://example.test/rpc#secret").is_err());
    }

    #[test]
    fn sse_parser_handles_notifications_and_results_without_loggable_headers() {
        let mut buffer = b"event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\r\n\r\nrest".to_vec();
        let event = take_sse_event(&mut buffer).expect("event");
        let value = parse_sse_data(&event).expect("valid SSE").expect("data");
        assert_eq!(
            parse_rpc_response(value, Some(200)).expect("result")["ok"],
            true
        );
        assert_eq!(buffer, b"rest");
    }

    #[test]
    fn rpc_errors_preserve_codes_without_echoing_request_data() {
        let error = parse_rpc_response(
            json!({"jsonrpc":"2.0","id":1,"error":{"code":-32022,"message":"unsupported","data":{"supported":["2026-07-28"]}}}),
            Some(400),
        )
        .expect_err("error response");
        assert!(matches!(
            error,
            McpRequestError::JsonRpc {
                code: -32022,
                http_status: Some(400),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn oauth_configuration_rejects_ambiguous_authorization_headers_before_network() {
        let directory = tempfile::tempdir().expect("tempdir");
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("HTTP client");
        let oauth = Arc::new(McpOAuthManager::production(directory.path()));
        let cancellation = CancellationToken::new();
        let error = connect_http(
            client,
            oauth,
            McpHttpConnectRequest {
                server_name: "docs".to_owned(),
                url: "https://mcp.example.test/rpc".to_owned(),
                headers: BTreeMap::from([(
                    "authorization".to_owned(),
                    "Bearer configured-secret".to_owned(),
                )]),
                auth: Some(mycel_agent_protocol::McpAuth::Oauth),
            },
            &cancellation,
        )
        .await
        .err()
        .expect("ambiguous credentials must fail");
        assert!(matches!(error, McpTransportError::Failed(_)));
        assert!(!error.to_string().contains("configured-secret"));
    }

    #[test]
    fn bearer_header_is_exact_and_invalid_values_do_not_echo_secrets() {
        let token = SecretString::new("token-value");
        assert_eq!(
            bearer_header_transport(&token)
                .expect("bearer header")
                .to_str()
                .expect("ASCII header"),
            "Bearer token-value"
        );
        let invalid = SecretString::new("secret\nvalue");
        let error = bearer_header_transport(&invalid).expect_err("invalid header");
        assert!(!error.to_string().contains("secret"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_connector_round_trips_without_a_shell_command_string() {
        let directory = tempfile::tempdir().expect("tempdir");
        let script = directory.path().join("server.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nIFS= read -r request\nprintf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}'\n",
        )
        .expect("write fixture");
        let mut permissions = std::fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script, permissions).expect("chmod fixture");

        let connector = ProcessMcpConnector::new(directory.path()).expect("connector");
        let cancellation = CancellationToken::new();
        let connected = connector
            .connect_stdio(
                McpStdioConnectRequest {
                    server_name: "fixture".into(),
                    purpose: McpConnectionPurpose::Session,
                    command: script.to_string_lossy().into_owned(),
                    args: Vec::new(),
                    env: BTreeMap::new(),
                    cwd: Some(directory.path().to_owned()),
                },
                &cancellation,
            )
            .await
            .expect("connect");
        let result = connected
            .peer
            .request(
                McpRequest {
                    method: "ping".into(),
                    params: json!({}),
                    era: McpProtocolEra::Modern,
                    protocol_version: "2026-07-28".into(),
                    http_headers: BTreeMap::new(),
                },
                &cancellation,
            )
            .await
            .expect("request");
        assert_eq!(result["ok"], true);
        connected.peer.close().await.expect("close");
    }
}
