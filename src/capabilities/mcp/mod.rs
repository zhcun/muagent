//! Minimal MCP (Model Context Protocol) client for μAgent.
//!
//! Scope (M1):
//! - JSON-RPC 2.0 envelope
//! - `initialize` handshake
//! - `tools/list` + `tools/call`
//! - Stdio transport (spawn a child process, write framed JSON on stdin,
//!   read JSON-per-line on stdout)
//! - `register_mcp_tools`: discovers remote tools at connect time and
//!   registers them as `crate::core::Tool` objects in a `CapabilityRegistry`.
//!
//! Out of scope for this milestone:
//! - `resources/*` and `prompts/*` endpoints
//! - Server-sent notifications (we model this as request/response only)

pub mod client;
pub mod http;
pub mod jsonrpc;
pub mod sse;
pub mod stdio;
pub mod test_support;
pub mod tools;

pub use client::{McpClient, McpClientError, McpToolDescriptor, Transport};
pub use http::HttpTransport;
pub use sse::SseTransport;
pub use stdio::{StdioSpawn, StdioTransport};
pub use tools::register_mcp_tools;

pub mod prelude {
    pub use super::{
        register_mcp_tools, HttpTransport, McpClient, McpClientError, McpToolDescriptor,
        SseTransport, StdioSpawn, StdioTransport, Transport,
    };
}
