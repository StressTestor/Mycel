use std::{collections::BTreeMap, future::Future, path::PathBuf, pin::Pin, sync::Arc};

use mycel_agent_protocol::McpAuth;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::CancellationToken;

pub type McpFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpConnectionPurpose {
    /// Disposable process used only to distinguish modern from legacy stdio.
    Probe,
    /// The process/connection that will serve actual MCP calls.
    Session,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpProtocolEra {
    Modern,
    Legacy,
}

/// A complete logical MCP request. The runtime owns the body metadata and
/// routing headers; the transport owns JSON-RPC IDs and wire framing.
#[derive(Clone, PartialEq)]
pub struct McpRequest {
    pub method: String,
    pub params: Value,
    pub era: McpProtocolEra,
    pub protocol_version: String,
    /// Streamable HTTP envelope headers. Empty for stdio. Header values may
    /// contain tool arguments and must not be logged by transport adapters.
    pub http_headers: BTreeMap<String, String>,
}

impl std::fmt::Debug for McpRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpRequest")
            .field("method", &self.method)
            .field("era", &self.era)
            .field("protocol_version", &self.protocol_version)
            .field(
                "http_header_names",
                &self.http_headers.keys().collect::<Vec<_>>(),
            )
            .field("params", &"[redacted]")
            .finish()
    }
}

/// Fully resolved stdio launch request. Implementations should inherit the
/// host environment and then apply `env` as overrides.
#[derive(Clone, PartialEq, Eq)]
pub struct McpStdioConnectRequest {
    pub server_name: String,
    pub purpose: McpConnectionPurpose,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
}

impl std::fmt::Debug for McpStdioConnectRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpStdioConnectRequest")
            .field("server_name", &self.server_name)
            .field("purpose", &self.purpose)
            .field("command", &self.command)
            .field("arg_count", &self.args.len())
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field("cwd", &self.cwd)
            .finish()
    }
}

/// Streamable HTTP request. Header values are intentionally omitted from the
/// Debug representation because this object may contain authorization data.
#[derive(Clone, PartialEq, Eq)]
pub struct McpHttpConnectRequest {
    pub server_name: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub auth: Option<McpAuth>,
}

impl std::fmt::Debug for McpHttpConnectRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpHttpConnectRequest")
            .field("server_name", &self.server_name)
            .field("url", &redacted_debug_url(&self.url))
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("auth", &self.auth)
            .finish()
    }
}

fn redacted_debug_url(url: &str) -> String {
    let end = url.find(['?', '#']).unwrap_or(url.len());
    let base = &url[..end];
    let Some(scheme_end) = base.find("://") else {
        return "[redacted endpoint]".to_owned();
    };
    let authority_start = scheme_end + 3;
    let authority_end = base[authority_start..]
        .find('/')
        .map_or(base.len(), |offset| authority_start + offset);
    let authority = &base[authority_start..authority_end];
    if let Some(userinfo_end) = authority.rfind('@') {
        format!(
            "{}{}{}",
            &base[..authority_start],
            &authority[userinfo_end + 1..],
            &base[authority_end..]
        )
    } else {
        base.to_owned()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpTransportEvent {
    ToolsListChanged,
    Closed { error: Option<String> },
}

/// Event stream allocated when the transport connects. It must buffer events
/// until consumed so a close between initialization and listener startup is
/// not lost.
pub trait McpTransportEvents: Send {
    fn next<'a>(&'a mut self) -> McpFuture<'a, Option<McpTransportEvent>>;
}

/// Request/notification seam shared by stdio and Streamable HTTP peers.
/// Implementations own JSON-RPC request IDs and must abort their underlying
/// I/O when the supplied token is cancelled.
pub trait McpPeer: Send + Sync {
    fn request<'a>(
        &'a self,
        request: McpRequest,
        cancellation: &'a CancellationToken,
    ) -> McpFuture<'a, Result<Value, McpRequestError>>;

    fn notify<'a>(&'a self, request: McpRequest) -> McpFuture<'a, Result<(), McpRequestError>>;

    fn close<'a>(&'a self) -> McpFuture<'a, Result<(), McpTransportError>>;
}

pub struct McpConnectedTransport {
    pub peer: Arc<dyn McpPeer>,
    pub events: Box<dyn McpTransportEvents>,
}

/// I/O injection boundary. A production adapter may spawn a child for stdio
/// or use an HTTP client for Streamable HTTP, while tests provide deterministic
/// in-memory peers. Standalone legacy SSE is deliberately absent.
pub trait McpTransportConnector: Send + Sync {
    fn connect_stdio<'a>(
        &'a self,
        request: McpStdioConnectRequest,
        cancellation: &'a CancellationToken,
    ) -> McpFuture<'a, Result<McpConnectedTransport, McpTransportError>>;

    fn connect_streamable_http<'a>(
        &'a self,
        request: McpHttpConnectRequest,
        cancellation: &'a CancellationToken,
    ) -> McpFuture<'a, Result<McpConnectedTransport, McpTransportError>>;
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum McpTransportError {
    #[error("MCP transport cancelled")]
    Cancelled,
    #[error("MCP transport closed")]
    Closed,
    #[error("MCP transport failed: {0}")]
    Failed(String),
}

/// Typed request failure used by era negotiation. HTTP status is kept
/// separate from a JSON-RPC error body so authentication, server faults and
/// modern version errors cannot be mistaken for legacy fallback signals.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum McpRequestError {
    #[error("MCP JSON-RPC error {code}: {message}")]
    JsonRpc {
        code: i64,
        message: String,
        data: Option<Value>,
        http_status: Option<u16>,
    },
    #[error("MCP HTTP request failed with status {status}: {message}")]
    Http { status: u16, message: String },
    #[error(transparent)]
    Transport(#[from] McpTransportError),
}

pub trait McpEnvironment: Send + Sync {
    fn get(&self, name: &str) -> Option<String>;
}

#[derive(Default)]
pub struct SystemMcpEnvironment;

impl McpEnvironment for SystemMcpEnvironment {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_debug_omits_credential_bearing_values() {
        let stdio = McpStdioConnectRequest {
            server_name: "server".to_owned(),
            purpose: McpConnectionPurpose::Session,
            command: "server-bin".to_owned(),
            args: vec!["--token".to_owned(), "stdio-super-secret".to_owned()],
            env: BTreeMap::from([("TOKEN".to_owned(), "env-super-secret".to_owned())]),
            cwd: None,
        };
        let rendered = format!("{stdio:?}");
        assert!(!rendered.contains("stdio-super-secret"));
        assert!(!rendered.contains("env-super-secret"));

        let http = McpHttpConnectRequest {
            server_name: "server".to_owned(),
            url: "https://user:userinfo-super-secret@example.test/mcp?token=http-super-secret"
                .to_owned(),
            headers: BTreeMap::from([(
                "Authorization".to_owned(),
                "Bearer header-super-secret".to_owned(),
            )]),
            auth: None,
        };
        let rendered = format!("{http:?}");
        assert!(!rendered.contains("http-super-secret"));
        assert!(!rendered.contains("userinfo-super-secret"));
        assert!(!rendered.contains("header-super-secret"));
        assert!(rendered.contains("https://example.test/mcp"));
    }
}
