//! Session-scoped MCP composition for the production CLI.

use std::{path::PathBuf, sync::Arc};

use mycel_agent_protocol::McpConfigFile;
use mycel_agent_runtime::{
    CancellationToken, McpEnvironment, McpRuntime, McpRuntimeOptions, McpTransportConnector,
    SessionHandle, SessionMcpEventSink, ToolRegistry,
};

const MAX_MCP_CONFIG_BYTES: usize = 4 * 1024 * 1024;

pub fn parse_mcp_config(source: &str) -> Result<McpConfigFile, String> {
    if source.len() > MAX_MCP_CONFIG_BYTES {
        return Err("MCP configuration exceeds the 4 MiB limit".into());
    }
    let config: McpConfigFile = serde_json::from_str(source).map_err(|error| {
        format!("MCP configuration does not match the supported schema: {error}")
    })?;
    config
        .validate()
        .map_err(|error| format!("invalid MCP configuration: {error}"))?;
    Ok(config)
}

pub struct SessionMcpContext {
    pub registry: ToolRegistry,
    pub config: McpConfigFile,
    pub connector: Arc<dyn McpTransportConnector>,
    pub environment: Arc<dyn McpEnvironment>,
    pub session: SessionHandle,
    pub working_dir: PathBuf,
}

/// Connect configured servers and attach their tools to the same registry the
/// turn engine snapshots. Individual server failures remain visible as status
/// entries; cancellation and invalid runtime configuration fail the session.
pub async fn start_session_mcp(
    context: SessionMcpContext,
    cancellation: &CancellationToken,
) -> Result<McpRuntime, String> {
    let mut options = McpRuntimeOptions::new(context.connector)
        .with_event_sink(Arc::new(SessionMcpEventSink::new(context.session)))
        .with_environment(context.environment);
    options = options.with_default_stdio_cwd(context.working_dir);
    let runtime = McpRuntime::new(context.registry, options)
        .map_err(|error| format!("could not initialize MCP runtime: {error}"))?;
    if let Err(error) = runtime
        .connect_all(&context.config.mcp_servers, cancellation)
        .await
    {
        let message = format!("could not connect MCP servers: {error}");
        return match runtime.shutdown().await {
            Ok(()) => Err(message),
            Err(shutdown) => Err(format!(
                "{message}; additionally MCP shutdown failed: {shutdown}"
            )),
        };
    }
    Ok(runtime)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_template_is_valid_rust_configuration() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("config/mcp.json.template");
        let source = std::fs::read_to_string(&path).expect("read shipped MCP template");
        let config = parse_mcp_config(&source).expect("parse shipped MCP template");
        assert_eq!(config.mcp_servers.len(), 1);
    }

    #[test]
    fn config_parser_accepts_retained_transports_and_rejects_legacy_sse() {
        let config = parse_mcp_config(
            r#"{
                "mcpServers": {
                    "local": { "transport": "stdio", "command": "server", "args": [] },
                    "remote": { "transport": "http", "url": "https://mcp.example.test/rpc" }
                }
            }"#,
        )
        .expect("valid config");
        assert_eq!(config.mcp_servers.len(), 2);

        let error = parse_mcp_config(
            r#"{"mcpServers":{"legacy":{"transport":"sse","url":"https://example.test/sse"}}}"#,
        )
        .expect_err("legacy SSE must be rejected");
        assert!(error.contains("supported schema"), "{error}");
    }

    #[test]
    fn config_parser_is_bounded_before_json_allocation() {
        let oversized = " ".repeat(MAX_MCP_CONFIG_BYTES + 1);
        assert!(parse_mcp_config(&oversized)
            .expect_err("oversized")
            .contains("4 MiB"));
    }
}
