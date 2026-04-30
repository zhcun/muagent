//! DefaultToolExecutor:tool filters → resolve → guard → sandbox + timeout + catch_unwind。
//!
//! 所有 tool(built-in / MCP / host 注册的)都**直接进 CapabilityRegistry**,
//! 执行器只从 registry 解析。Skills 不再贡献 tools(Anthropic Skills 协议),
//! 所以执行器不再需要 SkillManager 引用。
//!
//! 被 allowlist 过滤或解析不到的调用,都**返结构化的 ToolResult**
//! (ok=false, retryable=false, hint=可用工具列表),而不是 framework
//! error —— 这样 LLM 把结果看成普通的 tool_result,改路线。

use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use async_trait::async_trait;
use futures::FutureExt;
use tokio::time::timeout as tk_timeout;

use crate::core::cancel::CancelToken;
use crate::core::error::ToolExecutorError;
use crate::core::prelude::{
    CapabilityRegistry, GuardOutcome, Idempotency, PendingCall, SideEffects, Tool, ToolContext,
    ToolErr, ToolExecutor, ToolOk, ToolResult, TOOL_PROTOCOL_ERROR_TOOL,
};
use crate::core::types::{Content, ContentPart};
use serde_json::{json, Value};

const FRAMEWORK_MAX_OUT_TOKENS: u32 = 4096;
const MAX_HINT_CHARS: usize = 1024;
const MAX_DETAIL_CHARS: usize = 4096;

pub struct DefaultToolExecutor {
    registry: Arc<CapabilityRegistry>,
    /// 可选 host 级 allowlist。`None` = 全开;`Some(list)` = 只放 list
    /// 里的名字过。Provider 也应设同一份,两头一致。
    tool_allowlist: Option<Arc<Vec<String>>>,
    /// 可选 host 级 denylist。无论 allowlist 是否命中,denylist 都优先拒绝。
    tool_denylist: Arc<Vec<String>>,
}

impl DefaultToolExecutor {
    pub fn new(registry: Arc<CapabilityRegistry>) -> Self {
        Self {
            registry,
            tool_allowlist: None,
            tool_denylist: Arc::new(Vec::new()),
        }
    }

    /// 设定执行期 tool allowlist。不在 list 里的调用会被拒(返回
    /// structured ToolResult,不执行)。
    pub fn with_tool_allowlist(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.tool_allowlist = Some(Arc::new(names.into_iter().collect()));
        self
    }

    /// 设定执行期 tool denylist。不在 provider 暴露,也不允许执行。
    pub fn with_tool_denylist(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.tool_denylist = Arc::new(names.into_iter().collect());
        self
    }

    fn is_allowed(&self, name: &str) -> bool {
        if self.tool_denylist.iter().any(|x| x == name) {
            return false;
        }
        match &self.tool_allowlist {
            None => true,
            Some(list) => list.iter().any(|x| x == name),
        }
    }

    fn resolve(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.registry.resolve(name)
    }

    /// 当前**实际可用**的工具名集合,被 allowlist 过滤后。
    fn available_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .registry
            .list()
            .iter()
            .map(|t| t.descriptor().name.clone())
            .filter(|n| self.is_allowed(n))
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

#[async_trait]
impl ToolExecutor for DefaultToolExecutor {
    async fn execute(
        &self,
        call: &PendingCall,
        ctx: &ToolContext,
        cancel: CancelToken,
    ) -> Result<ToolResult, ToolExecutorError> {
        if call.tool_name == TOOL_PROTOCOL_ERROR_TOOL {
            let message = call
                .args
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Tool call could not be parsed.");
            let hint = call
                .args
                .get("hint")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let mut result = ToolResult::err(message, true, hint);
            result.detail = call.args.get("errors").cloned();
            return Ok(enforce_max_out(result, FRAMEWORK_MAX_OUT_TOKENS));
        }

        // 1) allowlist 过滤 — 不在 list 里直接返 ToolResult 给 LLM 看。
        if !self.is_allowed(&call.tool_name) {
            let result = ToolResult::err(
                format!(
                    "Tool `{}` is not available in this session. \
                     You may have seen it in earlier turns, but it's \
                     currently restricted. Try a different approach.",
                    call.tool_name
                ),
                false,
                Some(format!(
                    "Available tools: {}",
                    self.available_names().join(", ")
                )),
            );
            return Ok(enforce_max_out(result, FRAMEWORK_MAX_OUT_TOKENS));
        }

        // 2) resolve — 找不到也返 ToolResult(LLM 可能幻觉/拼错)。
        let tool = match self.resolve(&call.tool_name) {
            Some(t) => t,
            None => {
                let result = ToolResult::err(
                    format!(
                        "Tool `{}` does not exist (not registered). \
                         Likely a typo or a tool that was never available.",
                        call.tool_name
                    ),
                    false,
                    Some(format!("Use one of: {}", self.available_names().join(", "))),
                );
                return Ok(enforce_max_out(result, FRAMEWORK_MAX_OUT_TOKENS));
            }
        };

        // 3) guard
        match tool.guard(&call.args) {
            GuardOutcome::Allow => {}
            GuardOutcome::Deny { reason, hint } => {
                let result = ToolResult::err(reason, false, hint);
                return Ok(enforce_max_out(result, tool.descriptor().max_out_tokens));
            }
        }

        // 4) sandbox 执行:cancel(透传)+ timeout + catch_unwind
        let fut = tool.run(call.args.clone(), ctx, cancel);
        let run = tk_timeout(tool.descriptor().timeout, fut);
        let outcome = AssertUnwindSafe(run).catch_unwind().await;

        let result = match outcome {
            Ok(Ok(Ok(ok))) => to_result_ok(ok),
            Ok(Ok(Err(e))) => to_result_err(e),
            Ok(Err(_elapsed)) => ToolResult::err("timeout", true, Some("try smaller input".into())),
            Err(panic) => {
                let msg = sanitize_panic(panic);
                ToolResult::err(format!("internal: {msg}"), true, None)
            }
        };
        Ok(enforce_max_out(result, tool.descriptor().max_out_tokens))
    }

    fn idempotency_for(&self, call: &PendingCall) -> Idempotency {
        if call.tool_name == TOOL_PROTOCOL_ERROR_TOOL {
            return Idempotency::Idempotent;
        }
        self.resolve(&call.tool_name)
            .map(|t| t.idempotency_for_args(&call.args))
            .unwrap_or(Idempotency::AtMostOnce)
    }

    fn side_effects_for(&self, call: &PendingCall) -> SideEffects {
        if call.tool_name == TOOL_PROTOCOL_ERROR_TOOL {
            return SideEffects::ReadOnly;
        }
        self.resolve(&call.tool_name)
            .map(|t| t.descriptor().side_effects)
            // Unknown tool → trait default (Mutating) is the safe pessimistic
            // audit classification. (The execution path itself returns a
            // structured error for unknown tools, never actually invokes one.)
            .unwrap_or(SideEffects::Mutating)
    }
}

fn to_result_ok(o: ToolOk) -> ToolResult {
    ToolResult {
        ok: true,
        content: o.content,
        retryable: false,
        hint: None,
        detail: o.detail,
    }
}

fn to_result_err(e: ToolErr) -> ToolResult {
    ToolResult {
        ok: false,
        content: crate::core::types::Content::Text(e.msg),
        retryable: e.retryable,
        hint: e.hint,
        detail: None,
    }
}

fn enforce_max_out(mut result: ToolResult, max_out_tokens: u32) -> ToolResult {
    let max_chars = (max_out_tokens.max(1) as usize).saturating_mul(4);
    let marker = output_truncated_marker(max_out_tokens);

    result.content = match result.content {
        Content::Text(text) => {
            let (mut text, truncated, _) = take_chars(text, max_chars);
            if truncated {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&marker);
            }
            Content::Text(text)
        }
        Content::Parts(parts) => {
            let mut remaining = max_chars;
            let mut truncated = false;
            let mut out = Vec::with_capacity(parts.len() + 1);
            for part in parts {
                match part {
                    ContentPart::Text { text } => {
                        if remaining == 0 {
                            truncated = true;
                            continue;
                        }
                        let (text, was_truncated, used) = take_chars(text, remaining);
                        remaining = remaining.saturating_sub(used);
                        truncated |= was_truncated;
                        if !text.is_empty() {
                            out.push(ContentPart::Text { text });
                        }
                    }
                    other => out.push(other),
                }
            }
            if truncated {
                out.push(ContentPart::Text { text: marker });
            }
            Content::Parts(out)
        }
    };
    if let Some(hint) = result.hint.take() {
        result.hint = Some(truncate_hint(hint));
    }
    if let Some(detail) = result.detail.take() {
        result.detail = Some(truncate_detail(detail));
    }
    result
}

fn output_truncated_marker(max_out_tokens: u32) -> String {
    format!(
        "-- (truncated to ~{max_out_tokens} tokens; prefix only. Rerun narrower. \
         Files: fs_read offset=0/head, offset=<next>/more, from_end=true/tail.) --"
    )
}

fn take_chars(text: String, max_chars: usize) -> (String, bool, usize) {
    let mut out = String::new();
    let mut used = 0_usize;
    let mut chars = text.chars();
    while used < max_chars {
        let Some(ch) = chars.next() else {
            return (out, false, used);
        };
        out.push(ch);
        used += 1;
    }
    (out, chars.next().is_some(), used)
}

fn truncate_hint(hint: String) -> String {
    let (mut hint, truncated, _) = take_chars(hint, MAX_HINT_CHARS);
    if truncated {
        hint.push_str(" ... (hint truncated)");
    }
    hint
}

fn truncate_detail(detail: Value) -> Value {
    let Ok(serialized) = serde_json::to_string(&detail) else {
        return json!({
            "truncated": true,
            "reason": "tool detail could not be serialized"
        });
    };
    if serialized.chars().count() <= MAX_DETAIL_CHARS {
        return detail;
    }

    let (preview, _, _) = take_chars(serialized, MAX_DETAIL_CHARS);
    json!({
        "truncated": true,
        "reason": "tool detail exceeded the model-visible detail cap",
        "max_chars": MAX_DETAIL_CHARS,
        "preview_json_prefix": preview,
    })
}

fn sanitize_panic(p: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = p.downcast_ref::<String>() {
        return s.clone();
    }
    "(panic payload)".into()
}
