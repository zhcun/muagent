//! Google Gemini ModelAdapter(generateContent + function calling)。
//!
//! 协议要点:
//! - Endpoint:`POST {base_url}/v1beta/models/{model}:generateContent`
//! - Header:`x-goog-api-key`(或 query `?key=`)
//! - Body:`{ contents, tools?, systemInstruction? }`
//! - Content: `{ role:"user"|"model", parts: [Part] }`
//!   - Part types:`text` / `inlineData(mimeType, data)` / `functionCall(name, args)` /
//!     `functionResponse(name, response)`
//! - Roles:`user` / `model`(**不是** "assistant")
//! - Tool use:`model` 返回 `functionCall`;后续 `user` 消息用 `functionResponse`

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

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

pub struct GoogleGeminiAdapter {
    net: Arc<dyn NetEgress>,
    base_url: String,
    model: String,
    api_key: String,
    caps: LlmCaps,
}

impl GoogleGeminiAdapter {
    /// `base_url` e.g. `https://generativelanguage.googleapis.com`
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
            caps: LlmCaps {
                native_tool_use: true,
                json_schema_mode: true,
                vision: true,
                streaming: false,
                ctx_len: 1_000_000, // Gemini 1.5+
                // Native cachedContent exists but requires a pre-create step;
                // implicit caching available on 2.5 via SDK. We declare support
                // so hosts know to request it.
                prompt_cache: true,
                // Gemini 3 function-calling requires thoughtSignature replay.
                thinking: ThinkingSupport::FullReplay,
            },
        }
    }

    pub fn with_caps(mut self, caps: LlmCaps) -> Self {
        self.caps = caps;
        self
    }
}

#[async_trait]
impl ModelAdapter for GoogleGeminiAdapter {
    fn caps(&self) -> LlmCaps {
        self.caps.clone()
    }

    async fn turn(&self, req: ModelRequest, cancel: CancelToken) -> Result<ModelReply, ModelError> {
        let mut system_parts: Vec<String> = Vec::new();
        if !req.system.trim().is_empty() {
            system_parts.push(req.system.clone());
        }

        // Build `contents`
        let mut contents: Vec<Value> = Vec::new();
        let mut tool_names_by_call_id: HashMap<String, String> = HashMap::new();
        let requires_thought_signatures =
            model_requires_function_call_thought_signatures(&self.model);
        // Cap-based filtering centralized in core.
        let filtered = crate::core::wire::prepare_messages_for_caps(&req.messages, &self.caps);
        let mut i = 0;
        while i < filtered.len() {
            if matches!(&filtered[i], Message::ToolResult { .. }) {
                let mut parts = Vec::new();
                while let Some(Message::ToolResult { call_id, result }) = filtered.get(i) {
                    parts.extend(tool_result_parts_to_gemini(
                        call_id,
                        result,
                        &tool_names_by_call_id,
                    ));
                    i += 1;
                }
                contents.push(json!({ "role": "user", "parts": parts }));
                continue;
            }

            match msg_to_gemini(
                &filtered[i],
                &mut system_parts,
                &tool_names_by_call_id,
                requires_thought_signatures,
            ) {
                MsgOut::Skip => {}
                MsgOut::One(v) => contents.push(v),
            }
            if let Message::Assistant { tool_calls, .. } = &filtered[i] {
                for call in tool_calls {
                    tool_names_by_call_id.insert(call.id.clone(), call.tool_name.clone());
                }
            }
            i += 1;
        }

        // Tools: Gemini REST uses `[{functionDeclarations: [...]}]`.
        let func_decls: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.schema_json,
                })
            })
            .collect();
        let mut tools = Vec::new();
        if !func_decls.is_empty() {
            tools.push(json!({"functionDeclarations": func_decls}));
        }

        let mut body = json!({ "contents": contents });
        if !system_parts.is_empty() || !req.runtime_context.trim().is_empty() {
            // Gemini's systemInstruction takes multiple parts — put L0+L1
            // first, then L2 runtime_context as a separate part.
            let mut parts: Vec<Value> = Vec::new();
            if !system_parts.is_empty() {
                parts.push(json!({"text": system_parts.join("\n\n")}));
            }
            if !req.runtime_context.trim().is_empty() {
                parts.push(json!({"text": req.runtime_context.clone()}));
            }
            body["systemInstruction"] = json!({ "parts": parts });
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        if let Some(temp) = req.temperature {
            body["generationConfig"] = json!({"temperature": temp});
        }
        if let Some(thinking_config) = gemini_thinking_config(&req, &self.model) {
            let cfg = body
                .get_mut("generationConfig")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let mut cfg_obj = cfg.as_object().cloned().unwrap_or_default();
            cfg_obj.insert("thinkingConfig".into(), thinking_config);
            body["generationConfig"] = json!(cfg_obj);
        }

        let mut headers = HashMap::new();
        headers.insert("Content-Type".into(), "application/json".into());
        headers.insert("x-goog-api-key".into(), self.api_key.clone());

        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.base_url.trim_end_matches('/'),
            self.model,
        );
        let http_req = HttpReq {
            method: HttpMethod::Post,
            url,
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

        let candidate = parsed.candidates.into_iter().next().ok_or_else(|| {
            if let Some(feedback) = &parsed.prompt_feedback {
                if let Some(reason) = &feedback.block_reason {
                    return ModelError::Fatal(format!("Gemini prompt blocked: {reason}"));
                }
            }
            ModelError::Parse("no candidates".into())
        })?;

        let mut text = String::new();
        let mut tool_calls: Vec<PendingCall> = Vec::new();
        let mut reply_thinking: Vec<ThinkingArtifact> = Vec::new();
        let finish_reason = candidate.finish_reason.clone();
        let content = candidate.content.ok_or_else(|| {
            ModelError::Fatal(format!(
                "Gemini response omitted content{}",
                finish_reason
                    .as_deref()
                    .map(|r| format!("; finish_reason={r}"))
                    .unwrap_or_default()
            ))
        })?;
        for part in &content.parts {
            if let Some(t) = &part.text {
                if part.thought {
                    reply_thinking.push(google_thought_text_artifact(t.clone()));
                    continue;
                }
                text.push_str(t);
            }
            if let Some(fc) = &part.function_call {
                let index = tool_calls.len();
                let id = fc
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("fc_{}_{}", fc.name, tool_calls.len() + 1));
                if let Some(signature) = &part.thought_signature {
                    reply_thinking.push(google_tool_signature_artifact(index, signature.clone()));
                }
                tool_calls.push(PendingCall::new(id, fc.name.clone(), fc.args.clone()));
            }
        }
        if text.trim().is_empty()
            && tool_calls.is_empty()
            && finish_reason
                .as_deref()
                .is_some_and(gemini_finish_reason_is_not_retryable_empty)
        {
            return Err(ModelError::Fatal(format!(
                "Gemini stopped without usable content; finish_reason={}",
                finish_reason.unwrap_or_else(|| "unknown".into())
            )));
        }

        Ok(ModelReply {
            text,
            tool_calls,
            thinking: reply_thinking,
            usage: TokenUsage {
                prompt_tokens: parsed
                    .usage_metadata
                    .as_ref()
                    .map(|u| u.prompt_token_count)
                    .unwrap_or(0),
                completion_tokens: parsed
                    .usage_metadata
                    .as_ref()
                    .map(|u| u.candidates_token_count)
                    .unwrap_or(0),
                cost_usd: None,
                cache_read_tokens: parsed
                    .usage_metadata
                    .as_ref()
                    .map(|u| u.cached_content_token_count)
                    .unwrap_or(0),
                cache_write_tokens: 0,
                thinking_tokens: parsed
                    .usage_metadata
                    .as_ref()
                    .map(|u| u.thoughts_token_count)
                    .unwrap_or(0),
            },
        })
    }
}

// =================== message conversion ===================

enum MsgOut {
    Skip,
    One(Value),
}

/// Pure wire-format translation. No capability checks — caller ran
/// `prepare_messages_for_caps` upstream.
fn msg_to_gemini(
    m: &Message,
    system_parts: &mut Vec<String>,
    tool_names_by_call_id: &HashMap<String, String>,
    requires_thought_signatures: bool,
) -> MsgOut {
    match m {
        Message::System { content } => {
            let text = content_to_text(content);
            if !text.trim().is_empty() {
                system_parts.push(text);
            }
            MsgOut::Skip
        }
        Message::User { content } => MsgOut::One(json!({
            "role": "user",
            "parts": content_to_gemini(content),
        })),
        Message::Assistant {
            content,
            tool_calls,
            thinking,
        } => {
            let mut parts: Vec<Value> = Vec::new();
            let t = content_to_text(content);
            if !t.is_empty() {
                parts.push(json!({"text": t}));
            }
            for (idx, c) in tool_calls.iter().enumerate() {
                let mut part = json!({
                    "functionCall": { "name": c.tool_name, "args": c.args }
                });
                if let Some(signature) = google_thought_signature_for_tool(thinking, idx) {
                    part["thoughtSignature"] = json!(signature);
                } else if requires_thought_signatures && idx == 0 {
                    // Existing sessions created before signature capture look like
                    // transferred histories to Gemini 3's validator.
                    part["thoughtSignature"] = json!("skip_thought_signature_validator");
                }
                parts.push(part);
            }
            if parts.is_empty() {
                return MsgOut::Skip;
            }
            MsgOut::One(json!({"role":"model","parts":parts}))
        }
        Message::ToolResult { call_id, result } => MsgOut::One(json!({
            "role": "user",
            "parts": tool_result_parts_to_gemini(call_id, result, tool_names_by_call_id),
        })),
        Message::Observation { kind, text } => match kind {
            crate::core::types::ObsKind::System => {
                system_parts.push(format!("[system observation] {text}"));
                MsgOut::Skip
            }
            _ => MsgOut::One(json!({
                "role": "user",
                "parts": [{"text": observation_content(*kind, text)}],
            })),
        },
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

fn tool_result_parts_to_gemini(
    call_id: &str,
    result: &ToolResult,
    tool_names_by_call_id: &HashMap<String, String>,
) -> Vec<Value> {
    if let Some(name) = tool_names_by_call_id.get(call_id) {
        let media_parts = tool_result_media_parts(result);
        let mut function_response = json!({
            "id": call_id,
            "name": name,
            "response": tool_result_response(result),
        });
        if !media_parts.is_empty() {
            function_response["parts"] = Value::Array(media_parts);
        }
        vec![json!({ "functionResponse": function_response })]
    } else {
        let mut parts = vec![json!({
            "text": format!("[tool_result] {}", result.text())
        })];
        parts.extend(tool_result_media_parts(result));
        parts
    }
}

fn tool_result_media_parts(result: &ToolResult) -> Vec<Value> {
    let mut parts = Vec::new();
    for (idx, att) in result.attachments().enumerate() {
        if let crate::core::types::ContentPart::Image {
            b64: Some(data),
            mime,
            ..
        } = att
        {
            let display_name = format!("tool_image_{}.{}", idx + 1, extension_for_mime(mime));
            parts.push(json!({
                "inlineData": {
                    "mimeType": mime,
                    "displayName": display_name,
                    "data": data
                }
            }));
        } else if let crate::core::types::ContentPart::Data { mime, b64 } = att {
            let display_name = format!("tool_data_{}.{}", idx + 1, extension_for_mime(mime));
            parts.push(json!({
                "inlineData": {
                    "mimeType": mime,
                    "displayName": display_name,
                    "data": b64
                }
            }));
        }
    }
    parts
}

fn extension_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        _ => "bin",
    }
}

fn tool_result_response(result: &ToolResult) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("ok".into(), json!(result.ok));
    obj.insert("content".into(), json!(result.text()));
    obj.insert("retryable".into(), json!(result.retryable));
    if let Some(hint) = &result.hint {
        obj.insert("hint".into(), json!(hint));
    }
    if let Some(detail) = &result.detail {
        obj.insert("detail".into(), detail.clone());
    }
    Value::Object(obj)
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

fn content_to_gemini(c: &Content) -> Vec<Value> {
    match c {
        Content::Text(s) if s.trim().is_empty() => {
            vec![json!({"text": EMPTY_CONTENT_PLACEHOLDER})]
        }
        Content::Text(s) => vec![json!({"text": s})],
        Content::Parts(parts) => {
            let out: Vec<Value> = parts
                .iter()
                .filter_map(|p| match p {
                    crate::core::types::ContentPart::Text { text } if text.is_empty() => None,
                    crate::core::types::ContentPart::Text { text } => Some(json!({"text": text})),
                    crate::core::types::ContentPart::Image { b64, uri, mime } => {
                        if let Some(data) = b64 {
                            Some(json!({
                                "inlineData": {"mimeType": mime, "data": data},
                            }))
                        } else if let Some(u) = uri {
                            // Gemini fileData supports Google Cloud Storage URIs;http(s) URLs generally
                            // need base64 conversion client-side. We emit text indicating the URI.
                            Some(json!({"text": format!("[image:{u}]")}))
                        } else {
                            Some(json!({"text": "[image omitted]"}))
                        }
                    }
                    crate::core::types::ContentPart::Data { mime, b64 } => {
                        Some(json!({"inlineData": {"mimeType": mime, "data": b64}}))
                    }
                })
                .collect();
            if out.is_empty() {
                vec![json!({"text": EMPTY_CONTENT_PLACEHOLDER})]
            } else {
                out
            }
        }
    }
}

// =================== Gemini response types ===================

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    candidates: Vec<Candidate>,
    #[serde(default, rename = "promptFeedback")]
    prompt_feedback: Option<PromptFeedback>,
    #[serde(default, rename = "usageMetadata")]
    usage_metadata: Option<UsageMetadata>,
}

#[derive(Deserialize)]
struct Candidate {
    #[serde(default)]
    content: Option<CandidateContent>,
    #[serde(default, rename = "finishReason", alias = "finish_reason")]
    #[allow(dead_code)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct CandidateContent {
    #[serde(default)]
    parts: Vec<Part>,
    #[serde(default)]
    #[allow(dead_code)]
    role: Option<String>,
}

/// Gemini 的 part:每个字段独立可选。实际响应里一条 part 通常只命中一个字段。
#[derive(Deserialize, Default)]
struct Part {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thought: bool,
    #[serde(default, rename = "functionCall")]
    function_call: Option<FunctionCall>,
    #[serde(default, rename = "thoughtSignature", alias = "thought_signature")]
    thought_signature: Option<String>,
    // inlineData / functionResponse etc. 此处忽略(我们只处理 text + functionCall)
}

#[derive(Deserialize)]
struct FunctionCall {
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default)]
    args: Value,
}

#[derive(Deserialize)]
struct UsageMetadata {
    #[serde(default, rename = "promptTokenCount")]
    prompt_token_count: u32,
    #[serde(default, rename = "candidatesTokenCount")]
    candidates_token_count: u32,
    /// Tokens served from a pre-created `cachedContent`. Zero unless
    /// the caller explicitly set `cachedContent` (not wired in M1).
    #[serde(default, rename = "cachedContentTokenCount")]
    cached_content_token_count: u32,
    /// Reasoning tokens on Gemini 2.5+ thinking models.
    #[serde(default, rename = "thoughtsTokenCount")]
    thoughts_token_count: u32,
}

#[derive(Deserialize)]
struct PromptFeedback {
    #[serde(default, rename = "blockReason")]
    block_reason: Option<String>,
}

fn gemini_finish_reason_is_not_retryable_empty(reason: &str) -> bool {
    matches!(
        reason,
        "SAFETY"
            | "RECITATION"
            | "LANGUAGE"
            | "BLOCKLIST"
            | "PROHIBITED_CONTENT"
            | "SPII"
            | "MALFORMED_FUNCTION_CALL"
            | "UNEXPECTED_TOOL_CALL"
            | "TOO_MANY_TOOL_CALLS"
            | "MISSING_THOUGHT_SIGNATURE"
            | "IMAGE_SAFETY"
            | "IMAGE_PROHIBITED_CONTENT"
            | "NO_IMAGE"
            | "IMAGE_RECITATION"
            | "IMAGE_OTHER"
    )
}

fn model_requires_function_call_thought_signatures(model: &str) -> bool {
    model.to_ascii_lowercase().contains("gemini-3")
}

fn google_tool_signature_artifact(tool_call_index: usize, signature: String) -> ThinkingArtifact {
    ThinkingArtifact {
        provider: "google".into(),
        kind: ThinkingKind::ProviderOpaque,
        replay: ReplayPolicy::MustReplayUnmodified,
        visibility: ThinkingVisibility::Hidden,
        payload: ThinkingPayload::Json {
            value: json!({ "tool_call_index": tool_call_index }),
        },
        provider_signature: Some(signature),
    }
}

fn google_thought_text_artifact(text: String) -> ThinkingArtifact {
    ThinkingArtifact {
        provider: "google".into(),
        kind: ThinkingKind::SummaryText,
        replay: ReplayPolicy::Never,
        visibility: ThinkingVisibility::Hidden,
        payload: ThinkingPayload::Text { text },
        provider_signature: None,
    }
}

fn google_thought_signature_for_tool(
    thinking: &[ThinkingArtifact],
    tool_call_index: usize,
) -> Option<&str> {
    thinking.iter().find_map(|artifact| {
        if artifact.provider != "google" {
            return None;
        }
        let ThinkingPayload::Json { value } = &artifact.payload else {
            return None;
        };
        let index = value
            .get("tool_call_index")
            .and_then(|v| v.as_u64())
            .and_then(|n| usize::try_from(n).ok())?;
        (index == tool_call_index)
            .then(|| artifact.provider_signature.as_deref())
            .flatten()
    })
}

fn gemini_thinking_config(req: &ModelRequest, model: &str) -> Option<Value> {
    if model.to_ascii_lowercase().contains("gemini-3") {
        return gemini3_thinking_config(req);
    }
    gemini25_thinking_budget(req).map(|budget| {
        json!({
            "thinkingBudget": budget,
            "includeThoughts": false,
        })
    })
}

fn gemini3_thinking_config(req: &ModelRequest) -> Option<Value> {
    let level = match req.thinking.mode {
        ThinkingMode::Auto => return None,
        // Gemini 3 cannot fully disable thinking; use the lowest level instead
        // of sending the Gemini 2.5-only `thinkingBudget: 0` knob.
        ThinkingMode::Off => "low",
        ThinkingMode::Enabled => match (req.thinking.budget, req.thinking.effort) {
            (Some(ThinkingBudget::Relative(p)), _) if p <= 40 => "low",
            (Some(ThinkingBudget::Relative(_)), _) => "high",
            (Some(ThinkingBudget::Tokens(n)), _) if n <= 4_000 => "low",
            (Some(ThinkingBudget::Tokens(_)), _) => "high",
            (None, Some(ThinkingEffort::Minimal)) => "minimal",
            (None, Some(ThinkingEffort::Low)) => "low",
            (None, _) => "high",
        },
    };
    Some(json!({ "thinkingLevel": level }))
}

fn gemini25_thinking_budget(req: &ModelRequest) -> Option<u32> {
    match req.thinking.mode {
        ThinkingMode::Off => Some(0), // explicitly disable on 2.5 Flash/Pro
        ThinkingMode::Auto => None,   // let server default
        ThinkingMode::Enabled => Some(match (req.thinking.budget, req.thinking.effort) {
            (Some(ThinkingBudget::Tokens(n)), _) => n,
            (Some(ThinkingBudget::Relative(p)), _) => ((p as f32 / 100.0) * 24_000.0) as u32,
            (None, Some(ThinkingEffort::Minimal)) => 512,
            (None, Some(ThinkingEffort::Low)) => 2_000,
            (None, Some(ThinkingEffort::Medium)) => 8_000,
            (None, Some(ThinkingEffort::High)) => 16_000,
            (None, Some(ThinkingEffort::Max)) => 24_000,
            (None, None) => 2_000,
        }),
    }
}
