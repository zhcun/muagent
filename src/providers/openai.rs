//! OpenAI-compatible ModelAdapter。走 NetEgress(reqwest 或任意实现)。
//!
//! 支持 OpenAI / Ollama(`/v1/chat/completions` 兼容)/ 各种 OpenAI-like 接口。
//! 本实现**不带 stream**(M1-P3 最小版;后续 P6 / post-MVP 加流式)。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::core::cache::CachePolicy;
use crate::core::cancel::CancelToken;
use crate::core::error::ModelError;
use crate::core::net::{check_model_status, net_err_to_model, HttpMethod, HttpReq, NetEgress};
use crate::core::prelude::{LlmCaps, ModelAdapter, ModelReply, ModelRequest, TokenUsage};
use crate::core::thinking::{ThinkingEffort, ThinkingMode, ThinkingSupport};
use crate::core::tool::{PendingCall, ToolDescriptor, TOOL_PROTOCOL_ERROR_TOOL};
use crate::core::types::{Content, ContentPart, Message};

const EMPTY_CONTENT_PLACEHOLDER: &str = "[empty content]";

pub struct OpenAiAdapter {
    net: Arc<dyn NetEgress>,
    base_url: String,
    model: String,
    api_key: Option<String>,
    caps: LlmCaps,
}

impl OpenAiAdapter {
    /// `base_url` e.g. `https://api.openai.com/v1` / `http://127.0.0.1:11434/v1`.
    ///
    /// Caps are auto-inferred from `base_url` + `model` (see `infer_caps`).
    /// Override with `.with_caps()` for custom endpoints where we can't
    /// tell what's supported.
    pub fn new(
        net: Arc<dyn NetEgress>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        let base_url = base_url.into();
        let model = model.into();
        let caps = infer_caps(&base_url, &model);
        Self {
            net,
            base_url,
            model,
            api_key,
            caps,
        }
    }

    pub fn with_caps(mut self, caps: LlmCaps) -> Self {
        self.caps = caps;
        self
    }

    fn is_openrouter(&self) -> bool {
        self.base_url.contains("openrouter.ai")
    }

    fn openrouter_manual_cache(&self, req: &ModelRequest) -> bool {
        req.cache == CachePolicy::Auto
            && self.is_openrouter()
            && self.caps.prompt_cache
            && model_is_gemini(&self.model)
    }

    fn openrouter_anthropic_auto_cache(&self, req: &ModelRequest) -> bool {
        req.cache == CachePolicy::Auto
            && self.is_openrouter()
            && self.caps.prompt_cache
            && model_is_anthropic(&self.model)
    }
}

fn model_is_gemini(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("google/gemini") || model.starts_with("gemini")
}

fn model_is_anthropic(model: &str) -> bool {
    model.to_ascii_lowercase().starts_with("anthropic/")
}

fn cache_marked_system_message(system: &str) -> Value {
    json!({
        "role": "system",
        "content": [{
            "type": "text",
            "text": system,
            "cache_control": {"type": "ephemeral"},
        }],
    })
}

/// Infer `LlmCaps` from base URL + model id. The mental model:
///
/// - Official provider (api.openai.com) OR OpenRouter routing to a known
///   first-party family/model (`openai/*`, `anthropic/*`, `google/*`,
///   `mistralai/*`, `meta-llama/*`, `cohere/*`, `moonshotai/kimi-k2*`) → trust it, enable all
///   standard caps. Vision is family-based: official OpenAI/Azure OpenAI
///   and OpenRouter `openai/*`, `anthropic/*`, `google/*` are treated as
///   image-capable; other trusted families stay conservative unless
///   overridden via `.with_caps()`.
///
/// - Anything else (raw 127.0.0.1 Ollama, vLLM self-hosted, unknown
///   cloud) → **conservative default**: native tool-use OFF, vision OFF,
///   thinking NONE, prompt_cache OFF. JSON mode stays on because many
///   OpenAI-compatible servers implement it even when they do not implement
///   `tools`. Caller overrides via `.with_caps()` when they know the model's
///   real capabilities.
pub fn infer_caps(base_url: &str, model: &str) -> LlmCaps {
    let trusted = is_trusted_endpoint(base_url, model);
    let vision = trusted && family_has_vision(base_url, model);
    let thinking = if trusted {
        ThinkingSupport::NoReplay
    } else {
        ThinkingSupport::None
    };
    LlmCaps {
        native_tool_use: trusted,
        json_schema_mode: true,
        vision,
        streaming: false,
        ctx_len: 128_000,
        prompt_cache: trusted,
        thinking,
    }
}

fn is_trusted_endpoint(base_url: &str, model: &str) -> bool {
    // Official OpenAI — always trust.
    if base_url.contains("api.openai.com") {
        return true;
    }
    // OpenRouter routing to known first-party families.
    if base_url.contains("openrouter.ai") {
        const KNOWN_PREFIXES: &[&str] = &[
            "openai/",
            "anthropic/",
            "google/",
            "mistralai/",
            "meta-llama/",
            "cohere/",
            "x-ai/",
            "deepseek/",
            "moonshotai/kimi-k2",
        ];
        return KNOWN_PREFIXES.iter().any(|p| model.starts_with(p));
    }
    // Azure OpenAI mirrors the OpenAI API; trust it.
    if base_url.contains(".openai.azure.com") {
        return true;
    }
    false
}

fn family_has_vision(base_url: &str, model: &str) -> bool {
    let base_url = base_url.to_ascii_lowercase();
    let model = model.to_ascii_lowercase();
    if base_url.contains("api.openai.com") || base_url.contains(".openai.azure.com") {
        return true;
    }
    if base_url.contains("openrouter.ai") {
        return model.starts_with("openai/")
            || model.starts_with("anthropic/")
            || model.starts_with("google/");
    }
    false
}

#[cfg(test)]
mod caps_tests {
    use super::*;

    #[test]
    fn official_openai_trusts_everything() {
        let c = infer_caps("https://api.openai.com/v1", "gpt-5.4-nano");
        assert!(c.vision);
        assert!(c.prompt_cache);
        assert!(matches!(c.thinking, ThinkingSupport::NoReplay));
    }

    #[test]
    fn official_openai_family_has_vision_even_for_legacy_model_names() {
        let c = infer_caps("https://api.openai.com/v1", "gpt-3.5-turbo");
        assert!(c.vision);
        assert!(c.prompt_cache); // still trusted endpoint
    }

    #[test]
    fn openrouter_openai_prefix_trusted() {
        let c = infer_caps("https://openrouter.ai/api/v1", "openai/gpt-5.4-nano");
        assert!(c.vision);
        assert!(c.prompt_cache);
    }

    #[test]
    fn openrouter_openai_prefix_has_vision_without_model_whitelist() {
        let c = infer_caps("https://openrouter.ai/api/v1", "openai/any-current-model");
        assert!(c.vision);
    }

    #[test]
    fn openrouter_anthropic_prefix_trusted() {
        let c = infer_caps(
            "https://openrouter.ai/api/v1",
            "anthropic/claude-sonnet-4.6",
        );
        assert!(c.vision);
        assert!(c.prompt_cache);
    }

    #[test]
    fn openrouter_google_gemini_prefix_has_vision() {
        let c = infer_caps(
            "https://openrouter.ai/api/v1",
            "google/gemini-3.1-flash-lite-preview",
        );
        assert!(c.vision);
        assert!(c.prompt_cache);
    }

    #[test]
    fn openrouter_moonshot_kimi_k2_prefix_trusted() {
        let c = infer_caps("https://openrouter.ai/api/v1", "moonshotai/kimi-k2.6");
        assert!(c.native_tool_use);
        assert!(!c.vision);
        assert!(c.prompt_cache);
        assert!(matches!(c.thinking, ThinkingSupport::NoReplay));
    }

    #[test]
    fn openrouter_unknown_prefix_conservative() {
        let c = infer_caps(
            "https://openrouter.ai/api/v1",
            "some-random-org/custom-model-v1",
        );
        assert!(!c.vision);
        assert!(!c.prompt_cache);
        assert!(!c.native_tool_use);
        assert!(matches!(c.thinking, ThinkingSupport::None));
    }

    #[test]
    fn custom_local_endpoint_conservative() {
        // Ollama default
        let c = infer_caps("http://127.0.0.1:11434/v1", "llama3");
        assert!(!c.vision);
        assert!(!c.prompt_cache);
        assert!(matches!(c.thinking, ThinkingSupport::None));
        assert!(!c.native_tool_use);
        assert!(c.json_schema_mode);
    }

    #[test]
    fn azure_openai_trusted() {
        let c = infer_caps("https://my-tenant.openai.azure.com", "gpt-5.4-nano");
        assert!(c.vision);
        assert!(c.prompt_cache);
    }
}

#[async_trait]
impl ModelAdapter for OpenAiAdapter {
    fn caps(&self) -> LlmCaps {
        self.caps.clone()
    }

    async fn turn(&self, req: ModelRequest, cancel: CancelToken) -> Result<ModelReply, ModelError> {
        // ---- Build OpenAI-style request body ----
        // Two separate system messages: the first (L0+L1) is the stable,
        // cacheable prefix; the second (L2 runtime_context) changes per
        // turn. OpenAI's automatic prompt cache matches on exact prefix,
        // so putting L2 in a later message keeps the L0+L1 prefix cached.
        let mut messages_out: Vec<Value> = Vec::new();
        let native_tool_use = self.caps.native_tool_use;
        let system = if native_tool_use || req.tools.is_empty() {
            req.system.clone()
        } else {
            system_with_fallback_tools(&req.system, &req.tools)
        };

        let openrouter_manual_cache = self.openrouter_manual_cache(&req);
        let openrouter_anthropic_auto_cache = self.openrouter_anthropic_auto_cache(&req);
        if !system.trim().is_empty() {
            if openrouter_manual_cache {
                messages_out.push(cache_marked_system_message(&system));
            } else {
                messages_out.push(json!({"role":"system","content": system}));
            }
        }
        if !req.runtime_context.trim().is_empty() {
            if openrouter_manual_cache && !system.trim().is_empty() {
                messages_out.push(json!({
                    "role": "user",
                    "content": [{"type":"text", "text": req.runtime_context}],
                }));
            } else {
                messages_out.push(json!({"role":"system","content": req.runtime_context}));
            }
        }
        // Cap-based filtering happens once upstream — msg_to_openai just
        // translates to wire format, no capability checks.
        let filtered = crate::core::wire::prepare_messages_for_caps(&req.messages, &self.caps);
        messages_out.extend(messages_to_openai(&filtered, native_tool_use));

        let tools: Vec<Value> = if native_tool_use {
            req.tools
                .iter()
                .map(|t| {
                    json!({
                        "type":"function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.schema_json,
                        }
                    })
                })
                .collect()
        } else {
            Vec::new()
        };

        let mut body = json!({
            "model": self.model,
            "messages": messages_out,
        });
        if openrouter_anthropic_auto_cache {
            // OpenRouter's Anthropic route can advance an automatic cache
            // breakpoint through growing multi-turn history. Marking only the
            // system block leaves cache hits artificially short.
            body["cache_control"] = json!({ "type": "ephemeral" });
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        if let Some(temp) = req.temperature {
            body["temperature"] = json!(temp);
        }
        if let Some(effort) = reasoning_effort_str(&req, self.is_openrouter()) {
            if self.is_openrouter() {
                body["reasoning"] = json!({ "effort": effort });
            } else {
                body["reasoning_effort"] = json!(effort);
            }
        }
        // Routing-affinity hint for OpenAI's prompt-cache backend (Chat
        // Completions + Responses). Per OpenAI Cookbook *Prompt Caching 201*,
        // setting a stable key per session lifts real hit-rates from ~60%
        // to ~87% by keeping subsequent same-prefix requests on the
        // cache-warm replica. We pass it through verbatim; providers that
        // don't recognize the field will ignore it (it's a top-level body
        // hint, not a routing parameter), so this is safe for OpenRouter
        // and OpenAI-compatible local endpoints.
        if let Some(k) = &req.prompt_cache_key {
            if !k.is_empty() {
                body["prompt_cache_key"] = json!(k);
            }
        }

        let body_bytes = serde_json::to_vec(&body).map_err(|e| ModelError::Parse(e.to_string()))?;

        let mut headers = HashMap::new();
        headers.insert("Content-Type".into(), "application/json".into());
        headers.insert("Accept-Encoding".into(), "identity".into());
        if let Some(k) = &self.api_key {
            headers.insert("Authorization".into(), format!("Bearer {k}"));
        }

        let http_req = HttpReq {
            method: HttpMethod::Post,
            url: format!("{}/chat/completions", self.base_url.trim_end_matches('/')),
            headers,
            body: Some(body_bytes),
        };

        let resp = self
            .net
            .http(http_req, cancel)
            .await
            .map_err(net_err_to_model)?;
        check_model_status(&resp)?;

        let parsed: OpenAiResponse = serde_json::from_slice(&resp.body).map_err(|e| {
            let msg = format!("{}: {}", e, String::from_utf8_lossy(&resp.body));
            if self.is_openrouter() {
                ModelError::Transient(format!("malformed OpenRouter response: {msg}"))
            } else {
                ModelError::Parse(msg)
            }
        })?;

        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ModelError::Parse("no choices".into()))?;

        let finish_reason = choice.finish_reason.clone();
        let message = choice.message;
        let mut text = message_content_to_text(message.content);
        if text.trim().is_empty() {
            if let Some(refusal) = message.refusal {
                text = refusal;
            }
        }
        let tool_calls: Vec<PendingCall> = if native_tool_use {
            let mut calls = Vec::new();
            for tc in message.tool_calls.unwrap_or_default() {
                if !tc.ty.is_empty() && tc.ty != "function" {
                    continue;
                }
                let call_id = if tc.id.trim().is_empty() {
                    format!("native_tool_call_{}", calls.len() + 1)
                } else {
                    tc.id
                };
                let Some(f) = tc.function else {
                    calls.push(native_tool_protocol_error_call(
                        &call_id,
                        "",
                        "tool call is missing the function payload",
                        calls.len(),
                    ));
                    continue;
                };
                if f.name.trim().is_empty() {
                    calls.push(native_tool_protocol_error_call(
                        &call_id,
                        "",
                        "tool call function is missing the name",
                        calls.len(),
                    ));
                    continue;
                }
                match native_tool_arguments_to_value(f.arguments) {
                    Ok(args @ Value::Object(_)) => {
                        calls.push(PendingCall::new(call_id, f.name, args))
                    }
                    Ok(other) => calls.push(native_tool_protocol_error_call(
                        &call_id,
                        &f.name,
                        &format!(
                            "arguments must decode to a JSON object; got {}",
                            value_kind(&other)
                        ),
                        calls.len(),
                    )),
                    Err(e) => calls.push(native_tool_protocol_error_call(
                        &call_id,
                        &f.name,
                        &e,
                        calls.len(),
                    )),
                }
            }
            calls
        } else {
            let parsed = parse_fallback_tool_calls(&text, &req.tools);
            let mut calls = parsed.calls;
            if !calls.is_empty() || !parsed.errors.is_empty() {
                text = strip_fallback_tool_call_blocks(&text).trim().to_string();
            }
            if !parsed.errors.is_empty() {
                calls.push(fallback_protocol_error_call(
                    parsed.errors,
                    calls.len(),
                    &req.tools,
                ));
            }
            calls
        };

        if text.trim().is_empty()
            && tool_calls.is_empty()
            && finish_reason.as_deref() == Some("content_filter")
        {
            return Err(ModelError::Fatal(
                "OpenAI content filter omitted the assistant response".into(),
            ));
        }

        Ok(ModelReply {
            text,
            tool_calls,
            thinking: vec![], // Chat Completions does not return replayable reasoning items
            usage: TokenUsage {
                prompt_tokens: parsed.usage.as_ref().map(|u| u.prompt_tokens).unwrap_or(0),
                completion_tokens: parsed
                    .usage
                    .as_ref()
                    .map(|u| u.completion_tokens)
                    .unwrap_or(0),
                cost_usd: None,
                cache_read_tokens: parsed
                    .usage
                    .as_ref()
                    .and_then(|u| u.prompt_tokens_details.as_ref())
                    .map(|d| d.cached_tokens)
                    .unwrap_or(0),
                cache_write_tokens: parsed
                    .usage
                    .as_ref()
                    .and_then(|u| u.prompt_tokens_details.as_ref())
                    .map(|d| d.cache_write_tokens)
                    .unwrap_or(0),
                thinking_tokens: parsed
                    .usage
                    .as_ref()
                    .and_then(|u| u.completion_tokens_details.as_ref())
                    .map(|d| d.reasoning_tokens)
                    .unwrap_or(0),
            },
        })
    }
}

/// Pure wire-format translation. No capability checks — the caller has
/// already run `prepare_messages_for_caps` upstream, so any image parts in
/// `result.content` are either all-dropped (non-vision model) or all-keepable
/// (vision model); we just render what we see.
fn messages_to_openai(messages: &[Message], native_tool_use: bool) -> Vec<Value> {
    if !native_tool_use {
        return messages.iter().filter_map(msg_to_openai_fallback).collect();
    }

    let mut out = Vec::with_capacity(messages.len());
    let mut i = 0;
    while i < messages.len() {
        if !matches!(messages[i], Message::ToolResult { .. }) {
            if let Some(v) = msg_to_openai(&messages[i]) {
                out.push(v);
            }
            i += 1;
            continue;
        }

        let mut bridge_parts = Vec::new();
        while let Some(Message::ToolResult { call_id, result }) = messages.get(i) {
            out.push(tool_result_to_openai(call_id, result));
            append_tool_image_bridge_parts(&mut bridge_parts, call_id, result);
            i += 1;
        }
        if !bridge_parts.is_empty() {
            out.push(json!({
                "role": "user",
                "content": bridge_parts,
            }));
        }
    }
    out
}

fn msg_to_openai(m: &Message) -> Option<Value> {
    match m {
        Message::System { content } => Some(json!({
            "role":"system",
            "content": content_to_openai_input(content, /*system=*/ true),
        })),
        Message::User { content } => Some(json!({
            "role":"user",
            "content": content_to_openai_input(content, false),
        })),
        Message::Assistant {
            content,
            tool_calls,
            ..
        } => {
            if assistant_message_is_empty(content, tool_calls) {
                return None;
            }
            let mut v = json!({
                "role":"assistant",
                "content": content_to_openai(content, /*system=*/ true),
            });
            if !tool_calls.is_empty() {
                v["tool_calls"] = Value::Array(
                    tool_calls
                        .iter()
                        .map(|c| {
                            json!({
                                "id": c.id,
                                "type": "function",
                                "function": {
                                    "name": c.tool_name,
                                    "arguments": c.args.to_string(),
                                }
                            })
                        })
                        .collect(),
                );
            }
            Some(v)
        }
        Message::ToolResult { call_id, result } => Some(tool_result_to_openai(call_id, result)),
        Message::Observation { kind, text } => {
            let (role, content) = observation_role_and_content(*kind, text);
            Some(json!({
                "role": role,
                "content": content,
            }))
        }
    }
}

fn msg_to_openai_fallback(m: &Message) -> Option<Value> {
    match m {
        Message::System { content } => Some(json!({
            "role":"system",
            "content": content_to_openai_input(content, /*system=*/ true),
        })),
        Message::User { content } => Some(json!({
            "role":"user",
            "content": content_to_openai_input(content, false),
        })),
        Message::Assistant {
            content,
            tool_calls,
            ..
        } => {
            if assistant_message_is_empty(content, tool_calls) {
                return None;
            }
            Some(json!({
                "role":"assistant",
                "content": assistant_text_with_fallback_tool_calls(content, tool_calls),
            }))
        }
        Message::ToolResult { call_id, result } => {
            Some(tool_result_to_openai_fallback(call_id, result))
        }
        Message::Observation { kind, text } => {
            let (role, content) = observation_role_and_content(*kind, text);
            Some(json!({
                "role": role,
                "content": content,
            }))
        }
    }
}

fn observation_role_and_content(
    kind: crate::core::types::ObsKind,
    text: &str,
) -> (&'static str, String) {
    match kind {
        crate::core::types::ObsKind::System => ("system", format!("[system observation] {text}")),
        crate::core::types::ObsKind::Summary => (
            "user",
            format!(
                "[conversation summary]\n\
                 Authoritative prior conversation memory; facts here are visible \
                 context for later requests unless corrected.\n\n{text}"
            ),
        ),
        crate::core::types::ObsKind::Steering => ("user", format!("[user steering] {text}")),
        crate::core::types::ObsKind::User => ("user", format!("[user observation] {text}")),
    }
}

fn tool_result_to_openai(call_id: &str, result: &crate::core::tool::ToolResult) -> Value {
    json!({
        "role":"tool",
        "tool_call_id": call_id,
        "content": result.model_text(),
    })
}

fn tool_result_to_openai_fallback(call_id: &str, result: &crate::core::tool::ToolResult) -> Value {
    let text = format!("[tool_result id={call_id}]\n{}", result.model_text());
    let Content::Parts(parts) = &result.content else {
        return json!({
            "role": "user",
            "content": text,
        });
    };

    let images: Vec<Value> = parts.iter().filter_map(image_part_to_openai).collect();
    if images.is_empty() {
        return json!({
            "role": "user",
            "content": text,
        });
    }

    let mut content = vec![json!({"type": "text", "text": text})];
    content.extend(images);
    json!({
        "role": "user",
        "content": content,
    })
}

fn assistant_text_with_fallback_tool_calls(
    content: &Content,
    tool_calls: &[PendingCall],
) -> String {
    let mut out = content_plain_text(content);
    for call in tool_calls {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("<tool_call>");
        out.push_str(
            &json!({
                "name": call.tool_name,
                "arguments": call.args,
            })
            .to_string(),
        );
        out.push_str("</tool_call>");
    }
    out
}

fn content_plain_text(content: &Content) -> String {
    match content {
        Content::Text(s) => s.clone(),
        Content::Parts(parts) => parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn assistant_message_is_empty(content: &Content, tool_calls: &[PendingCall]) -> bool {
    tool_calls.is_empty() && content_plain_text(content).trim().is_empty()
}

fn system_with_fallback_tools(system: &str, tools: &[ToolDescriptor]) -> String {
    let fallback = fallback_tool_prompt(tools);
    if system.trim().is_empty() {
        fallback
    } else {
        format!("{system}\n\n{fallback}")
    }
}

fn fallback_tool_prompt(tools: &[ToolDescriptor]) -> String {
    let mut tools_sorted: Vec<_> = tools.iter().collect();
    tools_sorted.sort_by(|a, b| a.name.cmp(&b.name));

    let mut out = String::from(
        "Tool call transport fallback:\n\
         This endpoint does not expose native tool calling, so tool requests must be encoded in assistant text.\n\
         When a tool is needed, output only tool-call blocks and no surrounding prose, markdown, or commentary.\n\
         One block calls one tool:\n\
         <tool_call>{\"name\":\"tool_name\",\"arguments\":{},\"reason\":\"short optional reason\"}</tool_call>\n\
         For multiple calls, repeat one <tool_call> block per call in the desired order.\n\
         `name` must exactly match an available tool. `arguments` must be a JSON object matching that tool's schema; do not use comments, trailing commas, or non-JSON values.\n\
         If no tool is needed, answer normally without any tool_call block.\n\
         Tool results will be returned later as user-visible text beginning with [tool_result id=...].\n\
         If a tool-call block cannot be parsed, the host will return a retryable protocol-error tool result; fix the syntax and retry instead of repeating the same malformed block.\n\
         Available tools:\n",
    );

    for tool in tools_sorted {
        let description = tool.description.replace('\n', " ");
        let schema = serde_json::to_string(&tool.schema_json).unwrap_or_else(|_| "{}".into());
        out.push_str("- ");
        out.push_str(&tool.name);
        if !description.trim().is_empty() {
            out.push_str(": ");
            out.push_str(description.trim());
        }
        out.push_str("\n  schema: ");
        out.push_str(&schema);
        out.push('\n');
    }
    out
}

#[derive(Debug, Default)]
struct FallbackToolParse {
    calls: Vec<PendingCall>,
    errors: Vec<FallbackParseError>,
}

#[derive(Debug)]
struct FallbackParseError {
    message: String,
    raw: String,
}

#[derive(Debug)]
struct FallbackCandidate {
    raw: String,
    strict: bool,
    label: &'static str,
}

const FALLBACK_TAGS: &[(&str, &str, &str)] = &[
    ("<tool_call>", "</tool_call>", "tool_call tag"),
    ("<tool_calls>", "</tool_calls>", "tool_calls tag"),
    ("<function_call>", "</function_call>", "function_call tag"),
    (
        "<function_calls>",
        "</function_calls>",
        "function_calls tag",
    ),
];

fn parse_fallback_tool_calls(text: &str, tools: &[ToolDescriptor]) -> FallbackToolParse {
    let allowed: HashSet<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    let mut parsed = FallbackToolParse::default();
    let mut candidates = collect_fallback_candidates(text, &mut parsed.errors);

    if candidates.is_empty() && looks_like_standalone_tool_json(text.trim()) {
        candidates.push(FallbackCandidate {
            raw: text.trim().to_string(),
            strict: false,
            label: "standalone json",
        });
    }

    for candidate in candidates {
        parse_fallback_candidate(&candidate, &allowed, &mut parsed);
    }

    parsed
}

fn collect_fallback_candidates(
    text: &str,
    errors: &mut Vec<FallbackParseError>,
) -> Vec<FallbackCandidate> {
    let mut candidates = Vec::new();
    collect_tagged_fallback_candidates(text, &mut candidates, errors);
    collect_fenced_fallback_candidates(text, &mut candidates, errors);
    candidates
}

fn collect_tagged_fallback_candidates(
    text: &str,
    candidates: &mut Vec<FallbackCandidate>,
    errors: &mut Vec<FallbackParseError>,
) {
    for (start_tag, end_tag, label) in FALLBACK_TAGS {
        let mut cursor = 0;
        while let Some(start_rel) = text[cursor..].find(start_tag) {
            let start = cursor + start_rel + start_tag.len();
            let Some(end_rel) = text[start..].find(end_tag) else {
                errors.push(FallbackParseError {
                    message: format!("Unclosed {label}; missing `{end_tag}`."),
                    raw: clip_for_feedback(&text[cursor + start_rel..]),
                });
                break;
            };
            let end = start + end_rel;
            candidates.push(FallbackCandidate {
                raw: text[start..end].trim().to_string(),
                strict: true,
                label,
            });
            cursor = end + end_tag.len();
        }
    }
}

fn collect_fenced_fallback_candidates(
    text: &str,
    candidates: &mut Vec<FallbackCandidate>,
    errors: &mut Vec<FallbackParseError>,
) {
    let mut cursor = 0;
    while let Some(start_rel) = text[cursor..].find("```") {
        let fence_start = cursor + start_rel;
        let info_start = fence_start + 3;
        let Some(line_end_rel) = text[info_start..].find('\n') else {
            break;
        };
        let line_end = info_start + line_end_rel;
        let info = text[info_start..line_end].trim().to_ascii_lowercase();
        let content_start = line_end + 1;
        let Some(end_rel) = text[content_start..].find("```") else {
            if is_tool_fence_info(&info) {
                errors.push(FallbackParseError {
                    message: "Unclosed tool-call code fence; missing closing ```.".into(),
                    raw: clip_for_feedback(&text[fence_start..]),
                });
            }
            break;
        };
        let fence_end = content_start + end_rel;
        let raw = text[content_start..fence_end].trim();
        if is_tool_fence_info(&info) {
            candidates.push(FallbackCandidate {
                raw: raw.to_string(),
                strict: true,
                label: "tool_call code fence",
            });
        } else if info == "json" && looks_like_standalone_tool_json(raw) {
            candidates.push(FallbackCandidate {
                raw: raw.to_string(),
                strict: false,
                label: "json code fence",
            });
        }
        cursor = fence_end + 3;
    }
}

fn is_tool_fence_info(info: &str) -> bool {
    matches!(
        info,
        "tool_call" | "tool_calls" | "function_call" | "function_calls" | "tool-use"
    )
}

fn looks_like_standalone_tool_json(text: &str) -> bool {
    let trimmed = text.trim();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    lower.contains("tool_call")
        || lower.contains("tool_calls")
        || lower.contains("\"function\"")
        || ((lower.contains("\"name\"")
            || lower.contains("\"tool\"")
            || lower.contains("\"tool_name\""))
            && (lower.contains("\"arguments\"")
                || lower.contains("\"args\"")
                || lower.contains("\"input\"")
                || lower.contains("\"parameters\"")))
}

fn parse_fallback_candidate(
    candidate: &FallbackCandidate,
    allowed: &HashSet<&str>,
    parsed: &mut FallbackToolParse,
) {
    let value = match parse_candidate_json(&candidate.raw) {
        Ok(v) => v,
        Err(message) => {
            parsed.errors.push(FallbackParseError {
                message: format!("Could not parse {} as JSON: {message}.", candidate.label),
                raw: clip_for_feedback(&candidate.raw),
            });
            return;
        }
    };
    parse_tool_value(value, candidate.strict, allowed, parsed);
}

fn parse_candidate_json(raw: &str) -> Result<Value, String> {
    match serde_json::from_str::<Value>(raw) {
        Ok(v) => Ok(v),
        Err(first_err) => {
            if let Some(span) = first_json_span(raw) {
                let span = span.trim();
                if span != raw.trim() {
                    return serde_json::from_str::<Value>(span).map_err(|e| e.to_string());
                }
            }
            Err(first_err.to_string())
        }
    }
}

fn first_json_span(s: &str) -> Option<&str> {
    let start = s.find(['{', '['])?;
    let open = s[start..].chars().next()?;
    let close = if open == '{' { '}' } else { ']' };
    let mut stack = vec![close];
    let mut in_string = false;
    let mut escaped = false;

    for (offset, ch) in s[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' if offset != 0 => {
                stack.push('}');
            }
            '[' if offset != 0 => {
                stack.push(']');
            }
            '{' | '[' => {}
            '}' | ']' => {
                if Some(ch) != stack.pop() {
                    return None;
                }
                if stack.is_empty() {
                    let end = start + offset + ch.len_utf8();
                    return Some(&s[start..end]);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_tool_value(
    value: Value,
    strict: bool,
    allowed: &HashSet<&str>,
    parsed: &mut FallbackToolParse,
) {
    match value {
        Value::Array(items) => {
            if strict && items.is_empty() {
                parsed.errors.push(FallbackParseError {
                    message: "Tool-call array is empty.".into(),
                    raw: "[]".into(),
                });
            }
            for item in items {
                parse_tool_value(item, true, allowed, parsed);
            }
        }
        Value::Object(map) => {
            if let Some(v) = get_any(&map, &["tool_calls", "calls"]) {
                parse_tool_value(v.clone(), true, allowed, parsed);
                return;
            }
            if let Some(v) = get_any(&map, &["tool_call", "call"]) {
                parse_tool_value(v.clone(), true, allowed, parsed);
                return;
            }
            if is_tool_call_object(&map, allowed) {
                parse_one_tool_call(&map, allowed, parsed);
            } else if strict {
                parsed.errors.push(FallbackParseError {
                    message: "Tool-call object is missing a tool name.".into(),
                    raw: clip_for_feedback(&Value::Object(map).to_string()),
                });
            }
        }
        other => {
            if strict {
                parsed.errors.push(FallbackParseError {
                    message: "Tool-call payload must be a JSON object or array.".into(),
                    raw: clip_for_feedback(&other.to_string()),
                });
            }
        }
    }
}

fn get_any<'a>(map: &'a serde_json::Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| map.get(*k))
}

fn get_str_any(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| map.get(*k).and_then(|v| v.as_str()))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn is_tool_call_object(map: &serde_json::Map<String, Value>, allowed: &HashSet<&str>) -> bool {
    get_str_any(map, &["name", "tool", "tool_name"]).is_some()
        || map.get("function").is_some()
        || single_allowed_tool_key(map, allowed).is_some()
}

fn single_allowed_tool_key(
    map: &serde_json::Map<String, Value>,
    allowed: &HashSet<&str>,
) -> Option<String> {
    if map.len() != 1 {
        return None;
    }
    let name = map.keys().next()?;
    allowed.contains(name.as_str()).then(|| name.clone())
}

fn parse_one_tool_call(
    map: &serde_json::Map<String, Value>,
    allowed: &HashSet<&str>,
    parsed: &mut FallbackToolParse,
) {
    let function_obj = map.get("function").and_then(|v| v.as_object());
    let name = function_obj
        .and_then(|m| get_str_any(m, &["name"]))
        .or_else(|| get_str_any(map, &["name", "tool", "tool_name"]))
        .or_else(|| single_allowed_tool_key(map, allowed));

    let Some(name) = name else {
        parsed.errors.push(FallbackParseError {
            message: "Tool-call object is missing `name`.".into(),
            raw: clip_for_feedback(&Value::Object(map.clone()).to_string()),
        });
        return;
    };

    if !allowed.contains(name.as_str()) {
        parsed.errors.push(FallbackParseError {
            message: format!(
                "Tool `{name}` is not available. Available tools: {}.",
                allowed_names_for_feedback(allowed)
            ),
            raw: clip_for_feedback(&Value::Object(map.clone()).to_string()),
        });
        return;
    }

    let args_value = function_obj
        .and_then(|m| get_any(m, &["arguments", "args", "input", "parameters"]))
        .or_else(|| get_any(map, &["arguments", "args", "input", "parameters"]))
        .or_else(|| map.get(&name));

    let args = match normalize_fallback_args(args_value) {
        Ok(args) => args,
        Err(message) => {
            parsed.errors.push(FallbackParseError {
                message: format!("Invalid arguments for `{name}`: {message}."),
                raw: clip_for_feedback(&Value::Object(map.clone()).to_string()),
            });
            return;
        }
    };

    let mut call = PendingCall::new(
        format!("fallback_tool_call_{}", parsed.calls.len() + 1),
        name,
        args,
    );
    call.reason_from_llm = get_str_any(map, &["reason", "thought", "rationale"]);
    parsed.calls.push(call);
}

fn normalize_fallback_args(value: Option<&Value>) -> Result<Value, String> {
    match value {
        None | Some(Value::Null) => Ok(json!({})),
        Some(Value::Object(_)) => Ok(value.cloned().unwrap_or_else(|| json!({}))),
        Some(Value::String(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Ok(json!({}));
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(Value::Object(map)) => Ok(Value::Object(map)),
                Ok(_) => Err("arguments string must decode to a JSON object".into()),
                Err(e) => Err(format!("arguments string is not valid JSON: {e}")),
            }
        }
        Some(other) => Err(format!(
            "arguments must be a JSON object, null, or a JSON-object string; got {}",
            value_kind(other)
        )),
    }
}

fn strip_fallback_tool_call_blocks(text: &str) -> String {
    strip_fallback_fences(&strip_fallback_tags(text))
}

fn strip_fallback_tags(text: &str) -> String {
    let mut out = text.to_string();
    for (start_tag, end_tag, _) in FALLBACK_TAGS {
        out = strip_tag_pair(&out, start_tag, end_tag);
    }
    out
}

fn strip_tag_pair(text: &str, start_tag: &str, end_tag: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(start_rel) = text[cursor..].find(start_tag) {
        let start = cursor + start_rel;
        out.push_str(&text[cursor..start]);
        let content_start = start + start_tag.len();
        let Some(end_rel) = text[content_start..].find(end_tag) else {
            cursor = text.len();
            break;
        };
        cursor = content_start + end_rel + end_tag.len();
    }
    out.push_str(&text[cursor..]);
    out
}

fn strip_fallback_fences(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(start_rel) = text[cursor..].find("```") {
        let fence_start = cursor + start_rel;
        let info_start = fence_start + 3;
        let Some(line_end_rel) = text[info_start..].find('\n') else {
            break;
        };
        let line_end = info_start + line_end_rel;
        let info = text[info_start..line_end].trim().to_ascii_lowercase();
        let content_start = line_end + 1;
        let Some(end_rel) = text[content_start..].find("```") else {
            break;
        };
        let fence_end = content_start + end_rel + 3;
        if is_tool_fence_info(&info) {
            out.push_str(&text[cursor..fence_start]);
        } else {
            out.push_str(&text[cursor..fence_end]);
        }
        cursor = fence_end;
    }
    out.push_str(&text[cursor..]);
    out
}

fn fallback_protocol_error_call(
    errors: Vec<FallbackParseError>,
    index: usize,
    tools: &[ToolDescriptor],
) -> PendingCall {
    let available = tools
        .iter()
        .map(|t| t.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let message = if errors.len() == 1 {
        format!("Tool-call protocol error: {}", errors[0].message)
    } else {
        format!("Tool-call protocol error: {} parse errors.", errors.len())
    };
    let hint = format!(
        "Retry with exactly: <tool_call>{{\"name\":\"tool_name\",\"arguments\":{{}}}}</tool_call>. Available tools: {available}"
    );
    PendingCall::new(
        format!("fallback_tool_protocol_error_{}", index + 1),
        TOOL_PROTOCOL_ERROR_TOOL,
        json!({
            "message": message,
            "hint": hint,
            "errors": errors
                .into_iter()
                .map(|e| json!({"message": e.message, "raw": e.raw}))
                .collect::<Vec<_>>(),
        }),
    )
}

fn native_tool_protocol_error_call(
    call_id: &str,
    tool_name: &str,
    message: &str,
    index: usize,
) -> PendingCall {
    PendingCall::new(
        format!("native_tool_protocol_error_{}", index + 1),
        TOOL_PROTOCOL_ERROR_TOOL,
        json!({
            "message": format!(
                "Tool-call protocol error in `{tool_name}` (`{call_id}`): {message}."
            ),
            "hint": "Retry the tool call with valid JSON object arguments matching the tool schema.",
            "errors": [{
                "tool_call_id": call_id,
                "tool_name": tool_name,
                "message": message,
            }],
        }),
    )
}

fn value_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn allowed_names_for_feedback(allowed: &HashSet<&str>) -> String {
    let mut names: Vec<&str> = allowed.iter().copied().collect();
    names.sort_unstable();
    names.join(", ")
}

fn clip_for_feedback(s: &str) -> String {
    const MAX: usize = 500;
    let mut out: String = s.chars().take(MAX).collect();
    if s.chars().count() > MAX {
        out.push_str("...");
    }
    out
}

fn append_tool_image_bridge_parts(
    bridge_parts: &mut Vec<Value>,
    call_id: &str,
    result: &crate::core::tool::ToolResult,
) {
    if !result.ok {
        return;
    }
    let Content::Parts(parts) = &result.content else {
        return;
    };
    let images: Vec<&ContentPart> = parts
        .iter()
        .filter(|p| matches!(p, ContentPart::Image { .. }))
        .collect();
    if images.is_empty() {
        return;
    }

    let text = result.text();
    let header = if text.is_empty() {
        format!("Tool result `{call_id}` returned the following image attachment(s).")
    } else {
        format!(
            "Tool result `{call_id}` text:\n{text}\nThe following image attachment(s) are part of that tool output."
        )
    };
    bridge_parts.push(json!({"type": "text", "text": header}));
    bridge_parts.extend(images.into_iter().filter_map(image_part_to_openai));
}

fn image_part_to_openai(part: &ContentPart) -> Option<Value> {
    match part {
        ContentPart::Image { uri, b64, mime } => {
            if let Some(data) = b64 {
                Some(json!({
                    "type": "image_url",
                    "image_url": { "url": format!("data:{mime};base64,{data}") },
                }))
            } else {
                uri.as_ref().map(|u| {
                    json!({
                        "type": "image_url",
                        "image_url": { "url": u },
                    })
                })
            }
        }
        _ => None,
    }
}

/// 把 `Content` 转成 OpenAI 的 content 字段:
/// - 纯文本或仅 Text parts → string
/// - 含 image / data → `[{type,text|image_url,...}]` 数组
///
/// `system=true` 时(system / assistant 消息)OpenAI 不支持 image 数组,强制 flatten。
fn content_to_openai(c: &Content, system: bool) -> Value {
    match c {
        Content::Text(s) => Value::String(s.clone()),
        Content::Parts(parts) => {
            // system/assistant 不支持 image → 拍平
            if system {
                return Value::String(
                    parts
                        .iter()
                        .filter_map(|p| match p {
                            crate::core::types::ContentPart::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
            // 若全是 Text 也退化为 string(更省 token)
            let has_non_text = parts
                .iter()
                .any(|p| !matches!(p, crate::core::types::ContentPart::Text { .. }));
            if !has_non_text {
                return Value::String(
                    parts
                        .iter()
                        .filter_map(|p| match p {
                            crate::core::types::ContentPart::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
            let arr: Vec<Value> = parts
                .iter()
                .filter_map(|p| match p {
                    crate::core::types::ContentPart::Text { text } if text.is_empty() => None,
                    crate::core::types::ContentPart::Text { text } => Some(json!({
                        "type":"text", "text": text,
                    })),
                    crate::core::types::ContentPart::Image { uri, b64, mime } => {
                        let url = match (uri.as_deref(), b64.as_deref()) {
                            (_, Some(data)) => format!("data:{mime};base64,{data}"),
                            (Some(u), None) => u.to_string(),
                            _ => String::new(),
                        };
                        Some(json!({
                            "type":"image_url",
                            "image_url": {"url": url},
                        }))
                    }
                    crate::core::types::ContentPart::Data { mime, b64 } => Some(json!({
                        "type":"text",
                        "text": format!("[binary data omitted: {} ({}b)]", mime, b64.len()),
                    })),
                })
                .collect();
            Value::Array(arr)
        }
    }
}

fn content_to_openai_input(c: &Content, system: bool) -> Value {
    let v = content_to_openai(c, system);
    match &v {
        Value::String(s) if s.trim().is_empty() => Value::String(EMPTY_CONTENT_PLACEHOLDER.into()),
        Value::Array(parts) if parts.is_empty() => Value::String(EMPTY_CONTENT_PLACEHOLDER.into()),
        _ => v,
    }
}

// =================== OpenAI response types ===================

#[derive(Deserialize)]
struct OpenAiResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<UsageDto>,
}

#[derive(Deserialize)]
struct Choice {
    message: MessageOut,
    #[serde(default)]
    #[allow(dead_code)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct MessageOut {
    content: Option<Value>,
    #[serde(default)]
    refusal: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallOut>>,
}

#[derive(Deserialize)]
struct ToolCallOut {
    #[serde(default)]
    id: String,
    #[serde(rename = "type")]
    #[serde(default)]
    ty: String,
    #[serde(default)]
    function: Option<FunctionOut>,
}

#[derive(Deserialize, Serialize)]
struct FunctionOut {
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: Value,
}

fn message_content_to_text(content: Option<Value>) -> String {
    match content {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s,
        Some(Value::Array(parts)) => parts
            .into_iter()
            .filter_map(|part| match part {
                Value::String(s) => Some(s),
                Value::Object(map) => map
                    .get("text")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
    }
}

fn native_tool_arguments_to_value(arguments: Value) -> Result<Value, String> {
    match arguments {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Ok(json!({}));
            }
            serde_json::from_str::<Value>(trimmed)
                .map_err(|e| format!("arguments string is not valid JSON: {e}"))
        }
        Value::Object(_) => Ok(arguments),
        Value::Null => Ok(json!({})),
        other => Err(format!(
            "arguments must be a JSON object or JSON-object string; got {}",
            value_kind(&other)
        )),
    }
}

#[derive(Deserialize)]
struct UsageDto {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
    #[serde(default)]
    cache_write_tokens: u32,
}

#[derive(Deserialize)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: u32,
}

#[cfg(test)]
mod native_response_tests {
    use super::*;

    #[test]
    fn accepts_native_tool_arguments_as_object() {
        let body = br##"{
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "lookup_order",
                            "arguments": {"order_id": "#W2378156"}
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }"##;

        let parsed: OpenAiResponse = serde_json::from_slice(body).unwrap();
        let call = &parsed.choices[0].message.tool_calls.as_ref().unwrap()[0];
        let function = call.function.as_ref().unwrap();
        let args = native_tool_arguments_to_value(function.arguments.clone()).unwrap();

        assert_eq!(function.name, "lookup_order");
        assert_eq!(args["order_id"], "#W2378156");
    }

    #[test]
    fn accepts_native_tool_arguments_as_json_string() {
        let args =
            native_tool_arguments_to_value(Value::String(r##"{"order_id":"#W2378156"}"##.into()))
                .unwrap();

        assert_eq!(args["order_id"], "#W2378156");
    }

    #[test]
    fn accepts_message_content_as_text_parts() {
        let content = Some(json!([
            {"type": "text", "text": "first"},
            {"type": "text", "text": "second"}
        ]));

        assert_eq!(message_content_to_text(content), "first\nsecond");
    }
}

fn reasoning_effort_str(req: &ModelRequest, supports_xhigh: bool) -> Option<&'static str> {
    match req.thinking.mode {
        ThinkingMode::Off => None,
        ThinkingMode::Auto => None, // don't force reasoning on non-reasoning models
        ThinkingMode::Enabled => Some(match req.thinking.effort {
            Some(ThinkingEffort::Minimal) => "minimal",
            Some(ThinkingEffort::Low) => "low",
            Some(ThinkingEffort::Medium) => "medium",
            Some(ThinkingEffort::High) => "high",
            Some(ThinkingEffort::Max) if supports_xhigh => "xhigh",
            Some(ThinkingEffort::Max) => "high",
            None => "medium",
        }),
    }
}

#[cfg(test)]
mod image_content_tests {
    use super::*;
    use crate::core::types::{Content, ContentPart};

    #[test]
    fn image_with_uri_and_b64_prefers_inline_data_url() {
        let content = Content::Parts(vec![ContentPart::Image {
            uri: Some("/tmp/local.png".into()),
            b64: Some("AQID".into()),
            mime: "image/png".into(),
        }]);

        let value = content_to_openai(&content, false);
        assert_eq!(value[0]["image_url"]["url"], "data:image/png;base64,AQID");
    }
}
