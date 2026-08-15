//! Model Context Protocol client and tool runtime.
//!
//! Mycel owns MCP negotiation, lifecycle, tool naming, filtering and bounded
//! result conversion here. Process spawning and Streamable HTTP I/O are
//! injected through [`McpTransportConnector`], keeping the protocol state
//! machine testable without a network and preventing a second HTTP stack from
//! leaking into the agent loop.

mod naming;
mod output;
mod runtime;
mod transport;

pub use naming::*;
pub use output::*;
pub use runtime::*;
pub use transport::*;
