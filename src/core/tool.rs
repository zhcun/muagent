//! ToolExecutor trait + 相关 data types(Core 侧)。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::cancel::CancelToken;
use crate::core::error::ToolExecutorError;
use crate::core::event::CallId;

/// Internal pseudo-tool used by model adapters when a text-based tool-call
/// fallback could not be parsed. Executors return a structured ToolResult
/// for this call so the next model turn can repair the call syntax.
pub const TOOL_PROTOCOL_ERROR_TOOL: &str = "__muagent_tool_protocol_error__";

// ============ Pending call / ToolResult ============

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PendingCall {
    pub id: CallId,
    pub tool_name: String,
    pub args: Value,
    #[serde(default)]
    pub args_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_from_llm: Option<String>,
}

/// Context passed to `ToolExecutor::execute` on every call. Tools that
/// need session-scoped state (like `session_note`) read from here; tools
/// that don't (fs_read / sh_exec / ...) ignore it. Runner fills it from
/// `RunState` before each call.
#[derive(Clone, Debug)]
pub struct ToolContext {
    pub session_id: crate::core::event::SessionId,
    pub run_id: crate::core::event::RunId,
    pub turn: crate::core::event::TurnId,
}

impl ToolContext {
    /// For unit tests / one-offs where a context isn't meaningful.
    pub fn ephemeral() -> Self {
        Self {
            session_id: uuid::Uuid::nil(),
            run_id: uuid::Uuid::nil(),
            turn: 0,
        }
    }
}

impl PendingCall {
    pub fn new(id: impl Into<String>, tool_name: impl Into<String>, args: Value) -> Self {
        let id = id.into();
        let tool_name = tool_name.into();
        let args_hash = hash_args(&args);
        Self {
            id,
            tool_name,
            args,
            args_hash,
            reason_from_llm: None,
        }
    }
}

fn hash_args(v: &Value) -> String {
    use sha2::{Digest, Sha256};
    let s = serde_json::to_string(v).unwrap_or_default();
    let digest = Sha256::digest(s.as_bytes());
    hex::encode(digest)
}

/// Result of a single tool invocation. Two concerns live here:
///
/// 1. **Status**: `ok` / `retryable` / `hint` / `detail` — metadata about the
///    call itself.
/// 2. **Output**: `content` — the model-visible bytes that go into the next
///    turn's context. Same `Content` type as `Message::User { content }` so
///    text-only and multipart (text + images) tool results share one shape.
///
/// The two were split (`content: String` + `attachments: Vec<ContentPart>`)
/// in earlier versions; that asymmetry leaked into every adapter and forced
/// each one to render the two channels separately. Now adapters read
/// `result.content` and translate it once.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    pub ok: bool,
    pub content: crate::core::types::Content,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

impl ToolResult {
    /// Text-only success. Most tools use this.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            ok: true,
            content: crate::core::types::Content::Text(content.into()),
            retryable: false,
            hint: None,
            detail: None,
        }
    }

    /// Multipart success (text + image attachments etc.).
    pub fn ok_parts(parts: Vec<crate::core::types::ContentPart>) -> Self {
        Self {
            ok: true,
            content: crate::core::types::Content::Parts(parts),
            retryable: false,
            hint: None,
            detail: None,
        }
    }

    pub fn err(content: impl Into<String>, retryable: bool, hint: Option<String>) -> Self {
        Self {
            ok: false,
            content: crate::core::types::Content::Text(content.into()),
            retryable,
            hint,
            detail: None,
        }
    }

    pub fn framework_error(e: ToolExecutorError) -> Self {
        Self::err(format!("framework: {e}"), true, None)
    }

    /// Plain-text projection of `content` — joins text parts with newlines and
    /// drops non-text parts. Used for token estimation, audit briefs, and
    /// any consumer that just needs a string.
    pub fn text(&self) -> String {
        match &self.content {
            crate::core::types::Content::Text(s) => s.clone(),
            crate::core::types::Content::Parts(parts) => {
                let mut out = String::new();
                for p in parts {
                    if let crate::core::types::ContentPart::Text { text } = p {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(text);
                    }
                }
                out
            }
        }
    }

    /// Text shown to the model for text-only tool-result transports.
    /// Successful tool output is left unchanged. Failed tool output gets the
    /// small amount of status metadata the model needs to choose a different
    /// action instead of blindly repeating the same call.
    pub fn model_text(&self) -> String {
        let text = self.text();
        if self.ok {
            return text;
        }

        let mut out = format!(
            "TOOL_RESULT ok=false error_kind=tool_error retryable={}",
            self.retryable
        );
        if let Some(hint) = &self.hint {
            if !hint.is_empty() {
                out.push_str(" hint=");
                out.push_str(&one_line_status_value(hint));
            }
        }
        out.push('\n');

        if text.is_empty() {
            out.push_str("tool error");
        } else {
            out.push_str("tool error: ");
            out.push_str(&text);
        }
        out.push_str(if self.retryable {
            "\nretryable: true"
        } else {
            "\nretryable: false"
        });
        if let Some(hint) = &self.hint {
            if !hint.is_empty() {
                out.push_str("\nhint: ");
                out.push_str(hint);
            }
        }
        out
    }

    /// Iterate non-text parts (images, data). Adapters with vision render
    /// these alongside the textual content; adapters without vision can
    /// inspect via `wire::prepare_messages_for_caps` to drop them upstream.
    pub fn attachments(&self) -> impl Iterator<Item = &crate::core::types::ContentPart> {
        let slice: &[crate::core::types::ContentPart] = match &self.content {
            crate::core::types::Content::Parts(p) => p,
            crate::core::types::Content::Text(_) => &[],
        };
        slice
            .iter()
            .filter(|p| !matches!(p, crate::core::types::ContentPart::Text { .. }))
    }

    /// Brief for events (short summary of the textual content).
    pub fn brief(&self) -> String {
        let t = self.text();
        let s = t.chars().take(120).collect::<String>();
        if t.chars().count() > 120 {
            format!("{s}…")
        } else {
            s
        }
    }
}

fn one_line_status_value(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ============ Descriptor ============

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub schema_json: Value,
    #[serde(with = "humantime_serde_opt", default = "default_timeout")]
    pub timeout: std::time::Duration,
    #[serde(default = "default_max_out")]
    pub max_out_tokens: u32,
    #[serde(default)]
    pub concurrency: Concurrency,
    #[serde(default)]
    pub side_effects: SideEffects,
    #[serde(default)]
    pub idempotency: Idempotency,
}

fn default_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(10)
}
fn default_max_out() -> u32 {
    4096
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Concurrency {
    #[default]
    Exclusive,
    Parallel,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SideEffects {
    ReadOnly,
    #[default]
    Mutating,
    Destructive,
    /// 只改 capability 激活状态,不接触外部世界(cap.* meta-tools)
    CapabilityMutation,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Idempotency {
    #[default]
    Idempotent,
    AtMostOnce,
    AtLeastOnce,
}

// serde helper:ser/de Duration as humantime-like string;keep simple — just seconds.
mod humantime_serde_opt {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_millis() as u64)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let ms = u64::deserialize(d)?;
        Ok(Duration::from_millis(ms))
    }
}

// ============ Trait ============

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Execute one call. Implementations **must not panic** — catch panics internally
    /// and wrap them into `ToolResult { ok: false, retryable: true, ... }`.
    async fn execute(
        &self,
        call: &PendingCall,
        ctx: &ToolContext,
        cancel: CancelToken,
    ) -> Result<ToolResult, ToolExecutorError>;

    /// Dynamic idempotency judgment per-call (may depend on args).
    fn idempotency_for(&self, call: &PendingCall) -> Idempotency;

    /// Static side-effect class for the named tool. The Runner records this
    /// in the audit log. Default `Mutating` is the safe pessimistic value
    /// when the executor doesn't know the tool — that way audit reports
    /// "could have done something" rather than the misleading "unknown".
    fn side_effects_for(&self, _call: &PendingCall) -> SideEffects {
        SideEffects::Mutating
    }
}

// ============ Tool trait + CapabilityRegistry ============
//
// `Tool` is the per-tool unit registered into a `CapabilityRegistry` — the
// registry plus an executor adapter (in muagent-shell) make up the host's
// `ToolExecutor` impl. These types live in core (rather than shell) so MCP,
// model adapters, and embedded hosts can produce / consume tools without
// pulling in the shell runtime.

/// The atomic unit of capability — one callable function.
#[async_trait]
pub trait Tool: Send + Sync {
    fn descriptor(&self) -> &ToolDescriptor;

    /// 按 args 动态判定幂等性。默认返回 `descriptor().idempotency`。
    fn idempotency_for_args(&self, _args: &Value) -> Idempotency {
        self.descriptor().idempotency
    }

    /// guard 只做同步纯函数式检查;不可弹对话框、不做 I/O、不 await
    fn guard(&self, _args: &Value) -> GuardOutcome {
        GuardOutcome::Allow
    }

    /// Session-aware execution. Tools that don't need `ctx` can implement
    /// `run_ctxless` and ignore this entry point. The default forwards to
    /// `run_ctxless` so legacy tools keep compiling.
    ///
    /// `cancel` is the runner's per-step cancel token — pass it down to any
    /// long-running adapter call (process exec, HTTP, sleeps). A tool that
    /// silently uses `CancelToken::never()` makes the whole runtime's cancel
    /// guarantee a lie. For pure-sync work that's fast enough to not need
    /// mid-call cancellation (e.g. small file reads), check
    /// `cancel.triggered()` at entry and return early if set.
    ///
    /// Implementation **不应 panic**;executor 会用 `catch_unwind` 兜底。
    async fn run(
        &self,
        args: Value,
        _ctx: &ToolContext,
        cancel: CancelToken,
    ) -> Result<ToolOk, ToolErr> {
        self.run_ctxless(args, cancel).await
    }

    /// Context-free variant. Session-unaware tools override this.
    /// (fs_read / sh_exec / fs_list etc.)
    async fn run_ctxless(&self, _args: Value, _cancel: CancelToken) -> Result<ToolOk, ToolErr> {
        Err(ToolErr::deny("tool must override `run` or `run_ctxless`"))
    }
}

#[derive(Clone, Debug)]
pub enum GuardOutcome {
    Allow,
    Deny {
        reason: String,
        hint: Option<String>,
    },
}

/// Tool-author–facing success type. `content` follows the same `Content`
/// shape as `ToolResult` and `Message::User`. Use `ToolOk::text("…")` for
/// the common case; `with_attachment` upgrades to multipart when needed.
#[derive(Clone, Debug)]
pub struct ToolOk {
    pub content: crate::core::types::Content,
    pub detail: Option<Value>,
}

impl ToolOk {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: crate::core::types::Content::Text(s.into()),
            detail: None,
        }
    }

    pub fn parts(parts: Vec<crate::core::types::ContentPart>) -> Self {
        Self {
            content: crate::core::types::Content::Parts(parts),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: Value) -> Self {
        self.detail = Some(detail);
        self
    }

    /// Append a non-text attachment. Promotes a `Text(s)` content into a
    /// `Parts(vec![Text{s}, part])` so a single API works for either shape.
    pub fn with_attachment(mut self, part: crate::core::types::ContentPart) -> Self {
        self.content = match self.content {
            crate::core::types::Content::Text(s) if s.is_empty() => {
                crate::core::types::Content::Parts(vec![part])
            }
            crate::core::types::Content::Text(s) => crate::core::types::Content::Parts(vec![
                crate::core::types::ContentPart::Text { text: s },
                part,
            ]),
            crate::core::types::Content::Parts(mut p) => {
                p.push(part);
                crate::core::types::Content::Parts(p)
            }
        };
        self
    }
}

#[derive(Clone, Debug)]
pub struct ToolErr {
    pub msg: String,
    pub retryable: bool,
    pub hint: Option<String>,
}

impl ToolErr {
    pub fn retry(msg: impl Into<String>) -> Self {
        Self {
            msg: msg.into(),
            retryable: true,
            hint: None,
        }
    }
    pub fn deny(msg: impl Into<String>) -> Self {
        Self {
            msg: msg.into(),
            retryable: false,
            hint: None,
        }
    }
    pub fn with_hint(mut self, h: impl Into<String>) -> Self {
        self.hint = Some(h.into());
        self
    }
}

/// Parse the JSON `args` blob into a typed args struct, mapping any serde
/// failure to a `ToolErr::deny` with the field-level diagnostic verbatim
/// (which is the most useful thing a tool author can show the model).
///
/// Tools should define a `#[derive(Deserialize)]` args struct and call this
/// at the top of `run_ctxless` / `guard`, instead of walking `Value` by hand
/// — that prevents the schema_json declaration from drifting away from
/// what the implementation actually reads.
pub fn parse_args<T: serde::de::DeserializeOwned>(args: &Value) -> Result<T, ToolErr> {
    serde_json::from_value(args.clone()).map_err(|e| ToolErr::deny(format!("invalid args: {e}")))
}

/// Central registry mapping tool name → `Arc<dyn Tool>`. Hosts populate it
/// at startup and pass it to the executor and `ActiveToolSetProvider`.
#[derive(Default)]
pub struct CapabilityRegistry {
    tools: std::sync::RwLock<std::collections::HashMap<String, std::sync::Arc<dyn Tool>>>,
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, t: std::sync::Arc<dyn Tool>) {
        let name = t.descriptor().name.clone();
        self.tools.write().unwrap().insert(name, t);
    }

    pub fn resolve(&self, name: &str) -> Option<std::sync::Arc<dyn Tool>> {
        self.tools.read().unwrap().get(name).cloned()
    }

    /// List all registered tools, sorted by `descriptor().name`.
    ///
    /// Sorted output is load-bearing for prompt-cache hit rates:
    /// providers (OpenAI, Anthropic, Gemini) include the tool array
    /// verbatim in the cacheable prefix, so any ordering change between
    /// requests — including across separate process restarts — busts the
    /// cache. `HashMap::values()` iteration is non-deterministic across
    /// processes, so we sort here once. Within a process iteration is
    /// already stable, but cross-session cache reuse (1h retention,
    /// `prompt_cache_key` routing) requires cross-process determinism.
    /// See `developers.openai.com/cookbook/examples/prompt_caching_201`.
    pub fn list(&self) -> Vec<std::sync::Arc<dyn Tool>> {
        let mut out: Vec<_> = self.tools.read().unwrap().values().cloned().collect();
        out.sort_by(|a, b| a.descriptor().name.cmp(&b.descriptor().name));
        out
    }
}
