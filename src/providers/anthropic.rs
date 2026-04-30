//! Anthropic Messages API ModelAdapter(Claude)。
//!
//! 协议(与 OpenAI 的区别):
//! - Endpoint:`POST {base_url}/v1/messages`
//! - Headers:`x-api-key`、`anthropic-version: 2023-06-01`
//! - Body:`{ model, max_tokens, system, messages, tools?, cache_control? }`
//! - Content blocks:`{type:"text"|"image"|"tool_use"|"tool_result", ...}`
//! - Tool use:assistant 返回 `tool_use` block;后续 user message 带 `tool_result` block
//! - Images:`{"type":"image","source":{"type":"base64","media_type":...,"data":...}}`
//!   或 `{"type":"image","source":{"type":"url","url":...}}`(2024-10+ 支持)

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::core::cache::CachePolicy;
use crate::core::cancel::CancelToken;
use crate::core::error::ModelError;
use crate::core::net::{check_model_status, net_err_to_model, HttpMethod, HttpReq, NetEgress};
use crate::core::prelude::{LlmCaps, ModelAdapter, ModelReply, ModelRequest, TokenUsage};
use crate::core::thinking::{
    ReplayPolicy, ThinkingArtifact, ThinkingBudget, ThinkingEffort, ThinkingKind, ThinkingMode,
    ThinkingPayload, ThinkingSupport, ThinkingVisibility,
};
use crate::core::tool::{PendingCall, ToolResult};
use crate::core::types::{Content, Message};

const EMPTY_CONTENT_PLACEHOLDER: &str = "[empty content]";

pub struct AnthropicAdapter {
    net: Arc<dyn NetEgress>,
    base_url: String,
    model: String,
    api_key: String,
    anthropic_version: String,
    max_tokens: u32,
    caps: LlmCaps,
}

impl AnthropicAdapter {
    /// `base_url` e.g. `https://api.anthropic.com`
    pub fn new(
        net: Arc<dyn NetEgress>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            net,
            base_url: base_url.into(),
            model: model.into(),
            api_key: api_key.into(),
            anthropic_version: "2023-06-01".into(),
            max_tokens: 4096,
            caps: LlmCaps {
                native_tool_use: true,
                json_schema_mode: true,
                vision: true,
                streaming: false,
                ctx_len: 200_000,
                prompt_cache: true,
                thinking: ThinkingSupport::FullReplay,
            },
        }
    }

    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }
    pub fn with_caps(mut self, caps: LlmCaps) -> Self {
        self.caps = caps;
        self
    }
    pub fn with_anthropic_version(mut self, v: impl Into<String>) -> Self {
        self.anthropic_version = v.into();
        self
    }
}

#[async_trait]
impl ModelAdapter for AnthropicAdapter {
    fn caps(&self) -> LlmCaps {
        self.caps.clone()
    }

    async fn turn(&self, req: ModelRequest, cancel: CancelToken) -> Result<ModelReply, ModelError> {
        let cache = req.cache == CachePolicy::Auto;

        // Build messages: system 字段分离;user/assistant 保持顺序
        let mut system_parts: Vec<String> = Vec::new();
        if !req.system.trim().is_empty() {
            system_parts.push(req.system.clone());
        }

        let mut messages: Vec<Value> = Vec::new();
        let mut first_user_idx: Option<usize> = None;
        // Cap-based filtering is centralized in core — translator stays dumb.
        let filtered = crate::core::wire::prepare_messages_for_caps(&req.messages, &self.caps);
        let mut i = 0;
        while i < filtered.len() {
            if matches!(&filtered[i], Message::ToolResult { .. }) {
                let mut blocks = Vec::new();
                while let Some(Message::ToolResult { call_id, result }) = filtered.get(i) {
                    blocks.push(tool_result_block_to_anthropic(call_id, result));
                    i += 1;
                }
                push_message_and_track_first_user(
                    &mut messages,
                    &mut first_user_idx,
                    json!({"role":"user", "content": blocks}),
                );
                continue;
            }

            if let Some(v) = msg_to_anthropic(&filtered[i], &mut system_parts) {
                push_message_and_track_first_user(&mut messages, &mut first_user_idx, v);
            }
            i += 1;
        }

        // Tool schemas with an optional cache_control on the LAST tool.
        //
        // Cache breakpoint placement is byte-load-bearing on Anthropic:
        // moving a `cache_control` marker between turns changes the wire
        // bytes of an already-sent message, which (depending on whether
        // Anthropic's prefix hash includes cache_control fields) can
        // invalidate the very cache the marker was supposed to extend.
        // To stay byte-stable, breakpoints are placed only at fixed
        // positions: system L0+L1, the last tool, and the FIRST user
        // message (which never moves once history is appended-to). The
        // top-level automatic breakpoint below handles the growing tail of
        // multi-turn history without moving these explicit markers.
        let mut tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.schema_json,
                })
            })
            .collect();
        if cache {
            if let Some(last) = tools.last_mut() {
                last.as_object_mut()
                    .unwrap()
                    .insert("cache_control".into(), json!({"type":"ephemeral"}));
            }
            if let Some(i) = first_user_idx {
                attach_cache_to_last_block(&mut messages[i]);
            }
        }

        let mut body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": messages,
        });
        if cache {
            // Anthropic's native automatic cache moves the breakpoint forward
            // as conversations grow. Keep the explicit static-prefix markers
            // above as stable fallback breakpoints, and let this cover the
            // append-only message history.
            body["cache_control"] = json!({"type":"ephemeral"});
        }
        if !system_parts.is_empty() || !req.runtime_context.trim().is_empty() {
            // L0+L1 = cacheable; L2 (runtime_context) = separate block, NO cache.
            let mut blocks: Vec<Value> = Vec::new();
            if !system_parts.is_empty() {
                let mut b = json!({"type":"text","text": system_parts.join("\n\n")});
                if cache {
                    b.as_object_mut()
                        .unwrap()
                        .insert("cache_control".into(), json!({"type":"ephemeral"}));
                }
                blocks.push(b);
            }
            if !req.runtime_context.trim().is_empty() {
                blocks.push(json!({"type":"text","text": req.runtime_context.clone()}));
            }
            // If caller disabled cache AND there's no L2, keep the legacy string form
            // (some pre-2024 endpoints only accept string system). Otherwise use array.
            body["system"] = if !cache && req.runtime_context.is_empty() && blocks.len() == 1 {
                Value::String(blocks[0]["text"].as_str().unwrap_or("").to_string())
            } else {
                Value::Array(blocks)
            };
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        let thinking = anthropic_thinking_config(&req, &self.model, self.max_tokens);
        if let Some(t) = req.temperature.filter(|_| thinking.is_none()) {
            body["temperature"] = json!(t);
        }

        // Extended thinking translation.
        if let Some(thinking) = thinking {
            body["thinking"] = thinking.body;
            if let Some(output_config) = thinking.output_config {
                body["output_config"] = output_config;
            }
        }

        let mut headers = HashMap::new();
        headers.insert("Content-Type".into(), "application/json".into());
        headers.insert("x-api-key".into(), self.api_key.clone());
        headers.insert("anthropic-version".into(), self.anthropic_version.clone());

        let http_req = HttpReq {
            method: HttpMethod::Post,
            url: format!("{}/v1/messages", self.base_url.trim_end_matches('/')),
            headers,
            body: Some(serde_json::to_vec(&body).map_err(|e| ModelError::Parse(e.to_string()))?),
        };

        let resp = self
            .net
            .http(http_req, cancel)
            .await
            .map_err(net_err_to_model)?;
        check_model_status(&resp)?;

        let parsed: Response = serde_json::from_slice(&resp.body).map_err(|e| {
            ModelError::Parse(format!("{}: {}", e, String::from_utf8_lossy(&resp.body)))
        })?;

        // Merge text blocks into `text`;collect tool_use blocks;
        // capture thinking blocks as Core artifacts for replay.
        let mut text = String::new();
        let mut tool_calls: Vec<PendingCall> = Vec::new();
        let mut reply_thinking: Vec<ThinkingArtifact> = Vec::new();
        let mut thinking_tokens: u32 = 0;
        for block in &parsed.content {
            match block {
                ContentBlock::Text { text: t } => text.push_str(t),
                ContentBlock::ToolUse { id, name, input } => {
                    tool_calls.push(PendingCall::new(id.clone(), name.clone(), input.clone()));
                }
                ContentBlock::Thinking {
                    thinking,
                    signature,
                } => {
                    thinking_tokens =
                        thinking_tokens.saturating_add(estimate_tokens(thinking.len()));
                    reply_thinking.push(ThinkingArtifact {
                        provider: "anthropic".into(),
                        kind: ThinkingKind::FullText,
                        replay: ReplayPolicy::MustReplayUnmodified,
                        visibility: ThinkingVisibility::Hidden,
                        payload: ThinkingPayload::Text {
                            text: thinking.clone(),
                        },
                        provider_signature: signature.clone(),
                    });
                }
                ContentBlock::RedactedThinking { data } => {
                    reply_thinking.push(ThinkingArtifact {
                        provider: "anthropic".into(),
                        kind: ThinkingKind::RedactedOpaque,
                        replay: ReplayPolicy::MustReplayUnmodified,
                        visibility: ThinkingVisibility::Hidden,
                        payload: ThinkingPayload::OpaqueBytes { b64: data.clone() },
                        provider_signature: None,
                    });
                }
                _ => {}
            }
        }

        Ok(ModelReply {
            text,
            tool_calls,
            thinking: reply_thinking,
            usage: TokenUsage {
                prompt_tokens: parsed.usage.input_tokens
                    + parsed.usage.cache_creation_input_tokens
                    + parsed.usage.cache_read_input_tokens,
                completion_tokens: parsed.usage.output_tokens,
                cost_usd: None,
                cache_read_tokens: parsed.usage.cache_read_input_tokens,
                cache_write_tokens: parsed.usage.cache_creation_input_tokens,
                thinking_tokens,
            },
        })
    }
}

/// Push a message JSON value and, if it's the first user-role message we've
/// seen, record its position so a fixed cache breakpoint can be attached to
/// it. Fixed (not rolling) breakpoints keep the wire body byte-stable
/// across turns — a precondition for the cache to actually hit.
fn push_message_and_track_first_user(
    messages: &mut Vec<Value>,
    first_user_idx: &mut Option<usize>,
    v: Value,
) {
    if first_user_idx.is_none() && v.get("role").and_then(|r| r.as_str()) == Some("user") {
        *first_user_idx = Some(messages.len());
    }
    messages.push(v);
}

fn attach_cache_to_last_block(msg: &mut Value) {
    let content = match msg.get_mut("content") {
        Some(c) => c,
        None => return,
    };
    // Normalize string → single text block.
    if let Some(s) = content.as_str() {
        *content = json!([{"type":"text","text": s}]);
    }
    if let Some(arr) = content.as_array_mut() {
        if let Some(last) = arr.last_mut() {
            if let Some(obj) = last.as_object_mut() {
                obj.insert("cache_control".into(), json!({"type":"ephemeral"}));
            }
        }
    }
}

// =================== message conversion ===================

/// Pure wire-format translation. No capability checks — caller ran
/// `prepare_messages_for_caps` upstream.
fn msg_to_anthropic(m: &Message, system_parts: &mut Vec<String>) -> Option<Value> {
    match m {
        Message::System { content } => {
            // Anthropic 的 system 是顶层字段,不放 messages 里。累积到 system_parts。
            let text = content_to_text(content);
            if !text.trim().is_empty() {
                system_parts.push(text);
            }
            None
        }
        Message::User { content } => Some(json!({
            "role":"user",
            "content": content_to_anthropic(content),
        })),
        Message::Assistant {
            content,
            tool_calls,
            thinking,
        } => {
            let mut blocks: Vec<Value> = Vec::new();
            // Replay thinking artifacts FIRST — Anthropic requires them
            // to appear before text/tool_use blocks on tool-use turns.
            for t in thinking {
                if t.provider != "anthropic" {
                    continue;
                }
                match &t.kind {
                    ThinkingKind::RedactedOpaque => {
                        if let ThinkingPayload::OpaqueBytes { b64 } = &t.payload {
                            blocks.push(json!({
                                "type": "redacted_thinking",
                                "data": b64,
                            }));
                        }
                    }
                    _ => {
                        if let ThinkingPayload::Text { text: s } = &t.payload {
                            let mut b = json!({
                                "type": "thinking",
                                "thinking": s,
                            });
                            if let Some(sig) = &t.provider_signature {
                                b["signature"] = json!(sig);
                            }
                            blocks.push(b);
                        }
                    }
                }
            }
            let text = content_to_text(content);
            if !text.is_empty() {
                blocks.push(json!({"type":"text", "text": text}));
            }
            for c in tool_calls {
                blocks.push(json!({
                    "type":"tool_use",
                    "id": c.id,
                    "name": c.tool_name,
                    "input": c.args,
                }));
            }
            if blocks.is_empty() {
                return None;
            }
            Some(json!({"role":"assistant", "content": blocks}))
        }
        Message::ToolResult { call_id, result } => Some(json!({
            "role":"user",
            "content": [tool_result_block_to_anthropic(call_id, result)],
        })),
        Message::Observation { kind, text } => match kind {
            crate::core::types::ObsKind::System => {
                system_parts.push(format!("[system observation] {text}"));
                None
            }
            _ => Some(json!({
                "role": "user",
                "content": observation_content(*kind, text),
            })),
        },
    }
}

fn tool_result_block_to_anthropic(call_id: &str, result: &ToolResult) -> Value {
    json!({
        "type":"tool_result",
        "tool_use_id": call_id,
        "content": tool_result_content_to_anthropic(result),
        "is_error": !result.ok,
    })
}

fn tool_result_content_to_anthropic(result: &ToolResult) -> Value {
    // Anthropic tool_result.content may be a string OR an array of blocks.
    // Use the simpler string form for text-only results and errors.
    match &result.content {
        crate::core::types::Content::Text(_) => Value::String(result.model_text()),
        crate::core::types::Content::Parts(parts) => {
            if !result.ok {
                return Value::String(result.model_text());
            }
            let blocks: Vec<Value> = parts
                .iter()
                .filter_map(|p| match p {
                    crate::core::types::ContentPart::Text { text } if !text.is_empty() => {
                        Some(json!({"type":"text", "text": text}))
                    }
                    crate::core::types::ContentPart::Image { uri, b64, mime } => {
                        if let Some(data) = b64 {
                            Some(json!({
                                "type":"image",
                                "source": {
                                    "type":"base64",
                                    "media_type": mime,
                                    "data": data,
                                }
                            }))
                        } else {
                            uri.as_ref().map(|u| {
                                json!({
                                    "type":"image",
                                    "source": {"type":"url", "url": u},
                                })
                            })
                        }
                    }
                    _ => None,
                })
                .collect();
            Value::Array(blocks)
        }
    }
}

fn observation_content(kind: crate::core::types::ObsKind, text: &str) -> String {
    match kind {
        crate::core::types::ObsKind::System => format!("[system observation] {text}"),
        crate::core::types::ObsKind::Summary => format!(
            "[conversation summary]\n\
             Authoritative prior conversation memory; facts here are visible \
             context for later requests unless corrected.\n\n{text}"
        ),
        crate::core::types::ObsKind::Steering => format!("[user steering] {text}"),
        crate::core::types::ObsKind::User => format!("[user observation] {text}"),
    }
}

fn content_to_text(c: &Content) -> String {
    match c {
        Content::Text(s) => s.clone(),
        Content::Parts(parts) => parts
            .iter()
            .filter_map(|p| match p {
                crate::core::types::ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn content_to_anthropic(c: &Content) -> Value {
    match c {
        Content::Text(s) if s.trim().is_empty() => Value::String(EMPTY_CONTENT_PLACEHOLDER.into()),
        Content::Text(s) => Value::String(s.clone()),
        Content::Parts(parts) => {
            // Anthropic 对 user content 支持 text + image blocks
            let arr: Vec<Value> = parts
                .iter()
                .filter_map(|p| match p {
                    crate::core::types::ContentPart::Text { text } if text.is_empty() => None,
                    crate::core::types::ContentPart::Text { text } => Some(json!({
                        "type":"text", "text": text,
                    })),
                    crate::core::types::ContentPart::Image { uri, b64, mime } => {
                        if let Some(data) = b64 {
                            Some(json!({
                                "type":"image",
                                "source": {
                                    "type":"base64",
                                    "media_type": mime,
                                    "data": data,
                                }
                            }))
                        } else if let Some(u) = uri {
                            Some(json!({
                                "type":"image",
                                "source": {"type":"url", "url": u},
                            }))
                        } else {
                            Some(json!({"type":"text","text":"[image omitted]"}))
                        }
                    }
                    crate::core::types::ContentPart::Data { mime, b64 } => Some(json!({
                        "type":"text",
                        "text": format!("[binary data omitted: {} ({}b)]", mime, b64.len()),
                    })),
                })
                .collect();
            if arr.is_empty() {
                return json!([{"type":"text", "text": EMPTY_CONTENT_PLACEHOLDER}]);
            }
            Value::Array(arr)
        }
    }
}

struct AnthropicThinkingWire {
    body: Value,
    output_config: Option<Value>,
}

fn anthropic_thinking_config(
    req: &ModelRequest,
    model: &str,
    max_tokens: u32,
) -> Option<AnthropicThinkingWire> {
    match req.thinking.mode {
        ThinkingMode::Off | ThinkingMode::Auto => None,
        ThinkingMode::Enabled
            if req.thinking.budget.is_none() && anthropic_prefers_adaptive_thinking(model) =>
        {
            Some(AnthropicThinkingWire {
                body: json!({"type": "adaptive"}),
                output_config: Some(json!({
                    "effort": anthropic_adaptive_effort(model, req.thinking.effort),
                })),
            })
        }
        ThinkingMode::Enabled => {
            let budget = thinking_budget(req, max_tokens)?;
            Some(AnthropicThinkingWire {
                body: json!({
                    "type": "enabled",
                    "budget_tokens": budget,
                }),
                output_config: None,
            })
        }
    }
}

fn anthropic_prefers_adaptive_thinking(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("claude-opus-4-6")
        || model.starts_with("claude-sonnet-4-6")
        || model.starts_with("claude-opus-4-7")
        || model.starts_with("claude-mythos")
}

fn anthropic_adaptive_effort(model: &str, effort: Option<ThinkingEffort>) -> &'static str {
    match effort {
        Some(ThinkingEffort::Minimal | ThinkingEffort::Low) => "low",
        Some(ThinkingEffort::Medium) => "medium",
        Some(ThinkingEffort::Max) if anthropic_model_supports_max_effort(model) => "max",
        Some(ThinkingEffort::Max | ThinkingEffort::High) | None => "high",
    }
}

fn anthropic_model_supports_max_effort(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("claude-opus-4-6") || model.starts_with("claude-sonnet-4-6")
}

/// Resolve Anthropic's `budget_tokens` from Core's ThinkingConfig.
fn thinking_budget(req: &ModelRequest, max_tokens: u32) -> Option<u32> {
    let raw = match req.thinking.mode {
        ThinkingMode::Off | ThinkingMode::Auto => return None,
        ThinkingMode::Enabled => match req.thinking.budget {
            Some(ThinkingBudget::Tokens(n)) => n,
            Some(ThinkingBudget::Relative(p)) => {
                (1024f32 + (p as f32 / 100.0) * (32_000f32 - 1024f32)) as u32
            }
            None => match req.thinking.effort {
                Some(ThinkingEffort::Minimal) => 1024,
                Some(ThinkingEffort::Low) => 4_000,
                Some(ThinkingEffort::Medium) => 12_000,
                Some(ThinkingEffort::High) => 24_000,
                Some(ThinkingEffort::Max) => 32_000,
                None => 4_000,
            },
        },
    };
    clamp_anthropic_budget(raw, max_tokens)
}

fn clamp_anthropic_budget(raw: u32, max_tokens: u32) -> Option<u32> {
    if max_tokens <= 1024 {
        return None;
    }
    let cap = (((max_tokens as f32) * 0.8).floor() as u32)
        .max(1024)
        .min(max_tokens - 1);
    Some(raw.max(1024).min(cap))
}

/// Very rough token estimate used when Anthropic doesn't break thinking
/// tokens out separately (it's part of output_tokens already).
fn estimate_tokens(char_len: usize) -> u32 {
    char_len.div_ceil(3) as u32
}

// =================== Anthropic response types ===================

#[derive(Deserialize)]
struct Response {
    content: Vec<ContentBlock>,
    #[serde(default)]
    #[allow(dead_code)]
    stop_reason: Option<String>,
    usage: Usage,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    Thinking {
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    cache_creation_input_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: u32,
}
