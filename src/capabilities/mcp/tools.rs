//! Register MCP server tools directly into a `CapabilityRegistry`.
//!
//! MCP servers expose tools; they are **tools**, not skills. Earlier versions
//! of this code wrapped them as a `Skill` — that was wrong per the Anthropic
//! Skills protocol (skills are markdown documents, not tool bundles). Use
//! this function instead.
//!
//! ```ignore
//! let transport = StdioTransport::spawn(&spec).await?;
//! let client = Arc::new(McpClient::new(Box::new(transport)));
//! register_mcp_tools(&registry, client).await?;
//! // Server's tools are now callable by name in the agent's tool set.
//! ```
//!
//! Tool names are sanitized to `^[a-zA-Z0-9_-]+$` so they pass through any
//! provider's function-calling schema (e.g. Azure OpenAI).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::core::prelude::{
    CancelToken, CapabilityRegistry, Concurrency, Idempotency, SideEffects, Tool, ToolDescriptor,
    ToolErr, ToolOk,
};

use super::client::{McpClient, McpClientError};

fn sanitize_tool_name(name: &str) -> String {
    let mut s = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            s.push(c);
        } else {
            s.push('_');
        }
    }
    if s.is_empty() {
        "tool".into()
    } else {
        s
    }
}

fn unique_tool_name(name: &str, used: &mut BTreeSet<String>) -> String {
    let base = sanitize_tool_name(name);
    if used.insert(base.clone()) {
        return base;
    }
    for n in 2u32.. {
        let candidate = format!("{base}_{n}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("unbounded suffix loop must return");
}

struct McpTool {
    desc: ToolDescriptor,
    /// Remote name as the server knows it (pre-sanitization).
    remote_name: String,
    client: Arc<McpClient>,
}

#[async_trait]
impl Tool for McpTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.desc
    }

    async fn run_ctxless(&self, args: Value, cancel: CancelToken) -> Result<ToolOk, ToolErr> {
        if cancel.triggered() {
            return Err(ToolErr::deny("cancelled"));
        }
        // MCP JSON-RPC has no native cancel concept; we honor cancel at entry
        // plus whatever the transport can do underneath (HTTP can now abort
        // the outbound request; stdio still only checks at the call boundary).
        match self
            .client
            .call_tool_cancelable(&self.remote_name, args, cancel)
            .await
        {
            Ok((text, false)) => Ok(ToolOk::text(text)),
            Ok((text, true)) => Err(ToolErr::deny(text)),
            Err(McpClientError::Rpc { message, .. }) => Err(ToolErr::retry(message)),
            Err(e) => Err(ToolErr::retry(e.to_string())),
        }
    }
}

/// Connect (initialize + tools/list) and register every tool the server
/// exposes into the given `CapabilityRegistry`. Returns the list of
/// locally-registered tool names (sanitized) so the host can log them.
pub async fn register_mcp_tools(
    registry: &CapabilityRegistry,
    client: Arc<McpClient>,
) -> Result<Vec<String>, McpClientError> {
    client.initialize().await?;
    let descs = client.list_tools().await?;

    let mut registered = Vec::with_capacity(descs.len());
    let mut used_names = BTreeSet::new();
    let timeout = mcp_tool_timeout();
    let max_out_tokens = mcp_tool_max_out_tokens();
    for d in descs {
        let local_name = unique_tool_name(&d.name, &mut used_names);
        let desc = ToolDescriptor {
            name: local_name.clone(),
            description: d.description,
            schema_json: d.input_schema,
            timeout,
            max_out_tokens,
            concurrency: Concurrency::Parallel,
            side_effects: SideEffects::Mutating,
            idempotency: Idempotency::AtMostOnce,
        };
        registry.register(Arc::new(McpTool {
            desc,
            remote_name: d.name,
            client: client.clone(),
        }));
        registered.push(local_name);
    }
    Ok(registered)
}

fn mcp_tool_timeout() -> Duration {
    let secs = std::env::var("MUAGENT_MCP_TOOL_TIMEOUT_SEC")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(120);
    Duration::from_secs(secs)
}

fn mcp_tool_max_out_tokens() -> u32 {
    std::env::var("MUAGENT_MCP_TOOL_MAX_OUT_TOKENS")
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .filter(|tokens| *tokens > 0)
        .unwrap_or(8192)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitized_names_are_non_empty_and_unique() {
        let mut used = BTreeSet::new();
        assert_eq!(unique_tool_name("a.b", &mut used), "a_b");
        assert_eq!(unique_tool_name("a_b", &mut used), "a_b_2");
        assert_eq!(unique_tool_name("!!!", &mut used), "___");
        assert_eq!(unique_tool_name("", &mut used), "tool");
    }
}
