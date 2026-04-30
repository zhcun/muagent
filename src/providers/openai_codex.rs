//! OpenAI Codex/ChatGPT subscription-backed Responses adapter.
//!
//! This is deliberately separate from the normal OpenAI-compatible adapter:
//! it talks to the ChatGPT backend `/codex/responses` endpoint and authenticates
//! with OAuth credentials instead of an API key.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::core::cancel::CancelToken;
use crate::core::error::ModelError;
use crate::core::net::{check_model_status, net_err_to_model, HttpMethod, HttpReq, NetEgress};
use crate::core::prelude::{LlmCaps, ModelAdapter, ModelReply, ModelRequest, TokenUsage};
use crate::core::thinking::{ThinkingEffort, ThinkingMode, ThinkingSupport};
use crate::core::tool::{PendingCall, ToolDescriptor, TOOL_PROTOCOL_ERROR_TOOL};
use crate::core::types::{Content, ContentPart, Message, ObsKind};
use crate::oauth::OpenAiCodexAuth;

const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
const EMPTY_CONTENT_PLACEHOLDER: &str = "[empty content]";

pub struct OpenAiCodexAdapter {
    net: Arc<dyn NetEgress>,
    base_url: String,
    model: String,
    auth: OpenAiCodexAuth,
    caps: LlmCaps,
}

impl OpenAiCodexAdapter {
    pub fn new(
        net: Arc<dyn NetEgress>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        access_token_override: Option<String>,
    ) -> Self {
        let base_url = base_url.into();
        let model = model.into();
        Self {
            net,
            base_url,
            model,
            auth: OpenAiCodexAuth::new(None).with_access_token(access_token_override),
            caps: codex_caps(),
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn with_auth(mut self, auth: OpenAiCodexAuth) -> Self {
        self.auth = auth;
        self
    }

    pub fn with_caps(mut self, caps: LlmCaps) -> Self {
        self.caps = caps;
        self
    }
}

fn codex_caps() -> LlmCaps {
    LlmCaps {
        native_tool_use: true,
        json_schema_mode: true,
        vision: true,
        streaming: false,
        ctx_len: 272_000,
        prompt_cache: true,
        thinking: ThinkingSupport::NoReplay,
    }
}

#[async_trait]
impl ModelAdapter for OpenAiCodexAdapter {
    fn caps(&self) -> LlmCaps {
        self.caps.clone()
    }

    async fn turn(&self, req: ModelRequest, cancel: CancelToken) -> Result<ModelReply, ModelError> {
        let token = self.auth.resolve(self.net.clone(), cancel.child()).await?;

        let filtered = crate::core::wire::prepare_messages_for_caps(&req.messages, &self.caps);
        let mut input = Vec::new();
        if !req.runtime_context.trim().is_empty() {
            input.push(user_text_message(&req.runtime_context));
        }
        input.extend(messages_to_responses_input(&filtered));

        let mut body = json!({
            "model": self.model,
            "store": false,
            "stream": true,
            "instructions": req.system,
            "input": input,
            "text": {"verbosity": "low"},
            "include": ["reasoning.encrypted_content"],
            "tool_choice": "auto",
            "parallel_tool_calls": true,
        });
        if let Some(temp) = req.temperature {
            body["temperature"] = json!(temp);
        }
        if let Some(k) = &req.prompt_cache_key {
            if !k.is_empty() {
                body["prompt_cache_key"] = json!(k);
            }
        }
        if !req.tools.is_empty() {
            body["tools"] = Value::Array(tools_to_responses(&req.tools));
        }
        if let Some(reasoning) = codex_reasoning(&req, &self.model) {
            body["reasoning"] = reasoning;
        }

        let body_bytes = serde_json::to_vec(&body).map_err(|e| ModelError::Parse(e.to_string()))?;
        let mut headers = HashMap::new();
        headers.insert(
            "Authorization".into(),
            format!("Bearer {}", token.access_token),
        );
        headers.insert("chatgpt-account-id".into(), token.account_id);
        headers.insert(
            "originator".into(),
            std::env::var("MUAGENT_CODEX_ORIGINATOR").unwrap_or_else(|_| "codex_cli_rs".into()),
        );
        headers.insert(
            "User-Agent".into(),
            format!("muagent/{} (codex oauth)", env!("CARGO_PKG_VERSION")),
        );
        headers.insert("OpenAI-Beta".into(), "responses=experimental".into());
        headers.insert("accept".into(), "text/event-stream".into());
        headers.insert("content-type".into(), "application/json".into());
        headers.insert("Accept-Encoding".into(), "identity".into());
        if let Some(k) = &req.prompt_cache_key {
            if !k.is_empty() {
                headers.insert("session_id".into(), k.clone());
                headers.insert("x-client-request-id".into(), k.clone());
            }
        }

        let resp = self
            .net
            .http(
                HttpReq {
                    method: HttpMethod::Post,
                    url: resolve_codex_url(&self.base_url),
                    headers,
                    body: Some(body_bytes),
                },
                cancel,
            )
            .await
            .map_err(net_err_to_model)?;
        check_model_status(&resp)?;
        parse_codex_sse(&resp.body)
    }
}

fn resolve_codex_url(base_url: &str) -> String {
    let raw = if base_url.trim().is_empty() {
        DEFAULT_CODEX_BASE_URL
    } else {
        base_url.trim()
    };
    let normalized = raw.trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        normalized.to_string()
    } else if normalized.ends_with("/codex") {
        format!("{normalized}/responses")
    } else {
        format!("{normalized}/codex/responses")
    }
}

fn messages_to_responses_input(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for msg in messages {
        match msg {
            Message::System { content } => {
                let text = content_plain_text(content);
                if !text.trim().is_empty() {
                    out.push(json!({"role": "system", "content": text}));
                }
            }
            Message::User { content } => {
                if let Some(v) = user_content_message(content) {
                    out.push(v);
                }
            }
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                let text = content_plain_text(content);
                if !text.trim().is_empty() {
                    out.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text, "annotations": []}],
                        "status": "completed",
                    }));
                }
                for call in tool_calls {
                    out.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.tool_name,
                        "arguments": call.args.to_string(),
                    }));
                }
            }
            Message::ToolResult { call_id, result } => {
                out.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": tool_result_output(result),
                }));
            }
            Message::Observation { kind, text } => {
                out.push(user_text_message(&observation_text(*kind, text)));
            }
        }
    }
    out
}

fn user_text_message(text: &str) -> Value {
    let text = if text.trim().is_empty() {
        EMPTY_CONTENT_PLACEHOLDER
    } else {
        text
    };
    json!({
        "role": "user",
        "content": [{"type": "input_text", "text": text}],
    })
}

fn user_content_message(content: &Content) -> Option<Value> {
    let parts = content_to_responses_input(content);
    if parts.is_empty() {
        None
    } else {
        Some(json!({"role": "user", "content": parts}))
    }
}

fn content_to_responses_input(content: &Content) -> Vec<Value> {
    match content {
        Content::Text(text) => {
            if text.trim().is_empty() {
                vec![json!({"type": "input_text", "text": EMPTY_CONTENT_PLACEHOLDER})]
            } else {
                vec![json!({"type": "input_text", "text": text})]
            }
        }
        Content::Parts(parts) => {
            let mut out = Vec::new();
            for part in parts {
                match part {
                    ContentPart::Text { text } if text.trim().is_empty() => {}
                    ContentPart::Text { text } => {
                        out.push(json!({"type": "input_text", "text": text}));
                    }
                    ContentPart::Image { uri, b64, mime } => {
                        if let Some(image_url) = image_url(uri, b64, mime) {
                            out.push(json!({
                                "type": "input_image",
                                "detail": "auto",
                                "image_url": image_url,
                            }));
                        }
                    }
                    ContentPart::Data { mime, b64 } => {
                        out.push(json!({
                            "type": "input_text",
                            "text": format!("[binary data omitted: {} ({}b)]", mime, b64.len()),
                        }));
                    }
                }
            }
            if out.is_empty() {
                out.push(json!({"type": "input_text", "text": EMPTY_CONTENT_PLACEHOLDER}));
            }
            out
        }
    }
}

fn tool_result_output(result: &crate::core::tool::ToolResult) -> Value {
    let Content::Parts(parts) = &result.content else {
        return Value::String(result.model_text());
    };
    let has_image = parts.iter().any(|p| matches!(p, ContentPart::Image { .. }));
    if !has_image || !result.ok {
        return Value::String(result.model_text());
    }

    let mut out = Vec::new();
    let text = result.text();
    if !text.trim().is_empty() {
        out.push(json!({"type": "input_text", "text": text}));
    }
    for part in parts {
        if let ContentPart::Image { uri, b64, mime } = part {
            if let Some(image_url) = image_url(uri, b64, mime) {
                out.push(json!({
                    "type": "input_image",
                    "detail": "auto",
                    "image_url": image_url,
                }));
            }
        }
    }
    if out.is_empty() {
        Value::String("(see attached image)".into())
    } else {
        Value::Array(out)
    }
}

fn image_url(uri: &Option<String>, b64: &Option<String>, mime: &str) -> Option<String> {
    if let Some(data) = b64 {
        Some(format!("data:{mime};base64,{data}"))
    } else {
        uri.clone()
    }
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

fn observation_text(kind: ObsKind, text: &str) -> String {
    match kind {
        ObsKind::System => format!("[system observation] {text}"),
        ObsKind::Summary => format!(
            "[conversation summary]\n\
             Authoritative prior conversation memory; facts here are visible \
             context for later requests unless corrected.\n\n{text}"
        ),
        ObsKind::Steering => format!("[user steering] {text}"),
        ObsKind::User => format!("[user observation] {text}"),
    }
}

fn tools_to_responses(tools: &[ToolDescriptor]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.schema_json,
                "strict": false,
            })
        })
        .collect()
}

fn codex_reasoning(req: &ModelRequest, model: &str) -> Option<Value> {
    let effort = match req.thinking.mode {
        ThinkingMode::Off | ThinkingMode::Auto => return None,
        ThinkingMode::Enabled => match req.thinking.effort {
            Some(ThinkingEffort::Minimal) => "minimal",
            Some(ThinkingEffort::Low) => "low",
            Some(ThinkingEffort::Medium) => "medium",
            Some(ThinkingEffort::High) => "high",
            Some(ThinkingEffort::Max) => "xhigh",
            None => "medium",
        },
    };
    Some(json!({
        "effort": clamp_codex_effort(model, effort),
        "summary": "auto",
    }))
}

fn clamp_codex_effort(model: &str, effort: &'static str) -> &'static str {
    let id = model.rsplit('/').next().unwrap_or(model);
    if matches!(id, m if (m.starts_with("gpt-5.2") || m.starts_with("gpt-5.3") || m.starts_with("gpt-5.4") || m.starts_with("gpt-5.5")) && effort == "minimal")
    {
        return "low";
    }
    effort
}

#[derive(Default)]
struct MessageAccum {
    text: String,
    saw_delta: bool,
}

#[derive(Default)]
struct ToolAccum {
    name: String,
    arguments: String,
}

fn parse_codex_sse(body: &[u8]) -> Result<ModelReply, ModelError> {
    let events = sse_events(body);
    if events.is_empty() {
        return Err(ModelError::Parse(format!(
            "empty Codex SSE response: {}",
            String::from_utf8_lossy(body)
        )));
    }

    let mut text = String::new();
    let mut active_message: Option<MessageAccum> = None;
    let mut active_tool_call_id: Option<String> = None;
    let mut tool_accums: HashMap<String, ToolAccum> = HashMap::new();
    let mut finalized_tool_ids: HashSet<String> = HashSet::new();
    let mut tool_calls = Vec::new();
    let mut usage = TokenUsage::default();

    for event in events {
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
        match event_type {
            "error" => {
                return Err(ModelError::Fatal(event_error_message(&event)));
            }
            "response.failed" => {
                return Err(ModelError::Fatal(response_failed_message(&event)));
            }
            "response.output_item.added" => {
                let item = event.get("item").unwrap_or(&Value::Null);
                match item.get("type").and_then(Value::as_str) {
                    Some("message") => active_message = Some(MessageAccum::default()),
                    Some("function_call") => {
                        let call_id = item_str(item, "call_id")
                            .or_else(|| item_str(item, "id"))
                            .unwrap_or("codex_tool_call")
                            .to_string();
                        active_tool_call_id = Some(call_id.clone());
                        tool_accums.insert(
                            call_id,
                            ToolAccum {
                                name: item_str(item, "name").unwrap_or("").to_string(),
                                arguments: item_str(item, "arguments").unwrap_or("").to_string(),
                            },
                        );
                    }
                    _ => {}
                }
            }
            "response.content_part.added" => {
                if active_message.is_none() {
                    active_message = Some(MessageAccum::default());
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
                if delta.is_empty() {
                    continue;
                }
                if let Some(accum) = active_message.as_mut() {
                    accum.text.push_str(delta);
                    accum.saw_delta = true;
                } else {
                    append_text_block(&mut text, delta);
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(call_id) = active_tool_call_id.as_deref() {
                    let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
                    if let Some(accum) = tool_accums.get_mut(call_id) {
                        accum.arguments.push_str(delta);
                    }
                }
            }
            "response.function_call_arguments.done" => {
                if let Some(call_id) = active_tool_call_id.as_deref() {
                    if let Some(args) = event.get("arguments").and_then(Value::as_str) {
                        tool_accums
                            .entry(call_id.to_string())
                            .or_default()
                            .arguments = args.to_string();
                    }
                }
            }
            "response.output_item.done" => {
                let item = event.get("item").unwrap_or(&Value::Null);
                match item.get("type").and_then(Value::as_str) {
                    Some("message") => {
                        let final_text = message_item_text(item);
                        let block = match active_message.take() {
                            Some(accum) if accum.saw_delta => accum.text,
                            Some(_) | None => final_text,
                        };
                        append_text_block(&mut text, &block);
                    }
                    Some("function_call") => {
                        let call_id = item_str(item, "call_id")
                            .or_else(|| active_tool_call_id.as_deref())
                            .unwrap_or("codex_tool_call")
                            .to_string();
                        let mut accum = tool_accums.remove(&call_id).unwrap_or_default();
                        if accum.name.is_empty() {
                            accum.name = item_str(item, "name").unwrap_or("").to_string();
                        }
                        if let Some(args) = item_str(item, "arguments") {
                            accum.arguments = args.to_string();
                        }
                        finalize_tool_call(
                            &call_id,
                            &accum.name,
                            &accum.arguments,
                            &mut finalized_tool_ids,
                            &mut tool_calls,
                        );
                        active_tool_call_id = None;
                    }
                    _ => {}
                }
            }
            "response.completed" | "response.done" | "response.incomplete" => {
                if let Some(response) = event.get("response") {
                    usage = usage_from_response(response);
                    if matches!(
                        response.get("status").and_then(Value::as_str),
                        Some("failed" | "cancelled")
                    ) {
                        return Err(ModelError::Fatal(response_status_message(response)));
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(accum) = active_message.take() {
        append_text_block(&mut text, &accum.text);
    }

    for (call_id, accum) in tool_accums {
        finalize_tool_call(
            &call_id,
            &accum.name,
            &accum.arguments,
            &mut finalized_tool_ids,
            &mut tool_calls,
        );
    }

    Ok(ModelReply {
        text,
        tool_calls,
        usage,
        thinking: vec![],
    })
}

fn sse_events(body: &[u8]) -> Vec<Value> {
    let text = String::from_utf8_lossy(body).replace("\r\n", "\n");
    let mut out = Vec::new();
    for chunk in text.split("\n\n") {
        let data = chunk
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("\n");
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(data) {
            out.push(value);
        }
    }
    out
}

fn message_item_text(item: &Value) -> String {
    item.get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                    Some("output_text") => item_str(part, "text").map(ToOwned::to_owned),
                    Some("refusal") => item_str(part, "refusal").map(ToOwned::to_owned),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

fn append_text_block(out: &mut String, block: &str) {
    if block.is_empty() {
        return;
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(block);
}

fn finalize_tool_call(
    call_id: &str,
    name: &str,
    arguments: &str,
    finalized_tool_ids: &mut HashSet<String>,
    tool_calls: &mut Vec<PendingCall>,
) {
    if !finalized_tool_ids.insert(call_id.to_string()) {
        return;
    }
    if name.trim().is_empty() {
        tool_calls.push(tool_protocol_error_call(
            call_id,
            name,
            "tool call function is missing the name",
            arguments,
            tool_calls.len(),
        ));
        return;
    }
    match parse_tool_arguments(arguments) {
        Ok(args @ Value::Object(_)) => tool_calls.push(PendingCall::new(call_id, name, args)),
        Ok(other) => tool_calls.push(tool_protocol_error_call(
            call_id,
            name,
            &format!(
                "arguments must decode to a JSON object; got {}",
                value_kind(&other)
            ),
            arguments,
            tool_calls.len(),
        )),
        Err(e) => tool_calls.push(tool_protocol_error_call(
            call_id,
            name,
            &e,
            arguments,
            tool_calls.len(),
        )),
    }
}

fn parse_tool_arguments(arguments: &str) -> Result<Value, String> {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str::<Value>(trimmed)
        .map_err(|e| format!("arguments string is not valid JSON: {e}"))
}

fn tool_protocol_error_call(
    call_id: &str,
    tool_name: &str,
    message: &str,
    raw_arguments: &str,
    index: usize,
) -> PendingCall {
    PendingCall::new(
        format!("codex_tool_protocol_error_{}", index + 1),
        TOOL_PROTOCOL_ERROR_TOOL,
        json!({
            "provider": "openai-codex",
            "call_id": call_id,
            "tool_name": tool_name,
            "message": message,
            "raw_arguments": raw_arguments,
        }),
    )
}

fn usage_from_response(response: &Value) -> TokenUsage {
    let usage = response.get("usage").unwrap_or(&Value::Null);
    TokenUsage {
        prompt_tokens: u32_field(usage, "input_tokens"),
        completion_tokens: u32_field(usage, "output_tokens"),
        cost_usd: None,
        cache_read_tokens: usage
            .get("input_tokens_details")
            .map(|details| u32_field(details, "cached_tokens"))
            .unwrap_or(0),
        cache_write_tokens: 0,
        thinking_tokens: usage
            .get("output_tokens_details")
            .map(|details| u32_field(details, "reasoning_tokens"))
            .unwrap_or(0),
    }
}

fn event_error_message(event: &Value) -> String {
    item_str(event, "message")
        .or_else(|| item_str(event, "code"))
        .map(|s| format!("Codex error: {s}"))
        .unwrap_or_else(|| format!("Codex error: {event}"))
}

fn response_failed_message(event: &Value) -> String {
    event
        .get("response")
        .and_then(|response| response.get("error"))
        .and_then(|error| item_str(error, "message"))
        .map(|s| format!("Codex response failed: {s}"))
        .unwrap_or_else(|| format!("Codex response failed: {event}"))
}

fn response_status_message(response: &Value) -> String {
    response
        .get("error")
        .and_then(|error| item_str(error, "message"))
        .or_else(|| {
            response
                .get("incomplete_details")
                .and_then(|details| item_str(details, "reason"))
        })
        .map(|s| format!("Codex response failed: {s}"))
        .unwrap_or_else(|| format!("Codex response failed: {response}"))
}

fn item_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn u32_field(value: &Value, key: &str) -> u32 {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_codex_url_variants() {
        assert_eq!(
            resolve_codex_url("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_url("https://chatgpt.com/backend-api/codex"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_url("https://chatgpt.com/backend-api/codex/responses"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[test]
    fn parses_codex_sse_text_and_usage() {
        let body = br#"data: {"type":"response.output_item.added","item":{"type":"message"}}

data: {"type":"response.content_part.added","part":{"type":"output_text","text":""}}

data: {"type":"response.output_text.delta","delta":"hello"}

data: {"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"hello"}]}}

data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12,"input_tokens_details":{"cached_tokens":3},"output_tokens_details":{"reasoning_tokens":1}}}}

data: [DONE]

"#;
        let reply = parse_codex_sse(body).unwrap();
        assert_eq!(reply.text, "hello");
        assert_eq!(reply.usage.prompt_tokens, 10);
        assert_eq!(reply.usage.cache_read_tokens, 3);
        assert_eq!(reply.usage.thinking_tokens, 1);
    }

    #[test]
    fn parses_codex_sse_tool_call() {
        let body = br#"data: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1","id":"fc_1","name":"fs_read","arguments":""}}

data: {"type":"response.function_call_arguments.delta","delta":"{\"path\""}

data: {"type":"response.function_call_arguments.done","arguments":"{\"path\":\"/tmp/a\"}"}

data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","id":"fc_1","name":"fs_read","arguments":"{\"path\":\"/tmp/a\"}"}}

data: {"type":"response.completed","response":{"status":"completed"}}

"#;
        let reply = parse_codex_sse(body).unwrap();
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].id, "call_1");
        assert_eq!(reply.tool_calls[0].tool_name, "fs_read");
        assert_eq!(reply.tool_calls[0].args["path"], "/tmp/a");
    }
}
