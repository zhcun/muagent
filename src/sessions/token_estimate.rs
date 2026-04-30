//! Token 估算(保守版)。**不是**精确 tokenizer —— 仅用于做 compaction 预算判断。
//!
//! 为什么不用真 tokenizer:
//! - 每家 provider / 每个模型的 tokenizer 不同(tiktoken / claude tokenizer / gemini),
//!   引入 tokenizer 体积大(2-10MB BPE 表)且跨语言误差仍 10-30%
//! - 压缩决策不需要精确 —— 我们只关心"是否已接近上限"
//!
//! 策略(保守,倾向高估):
//! - 文本:`max(ceil(chars / 3.0), ceil(bytes / 4.0))`
//!   - 纯英文:chars/3 ≈ 实际 tokens
//!   - 中文:每字 ≈ 1 token,所以 chars 直接 = tokens(chars/3 比实际小,bytes/4 覆盖)
//!   - 取 max 对两种都保守
//! - 图像:固定 **1200 tokens / image**(OpenAI / Claude 都在这个量级的上界)
//! - 其它 binary(音频等):mime 长度占位 + `b64.len() / 3`(粗略)

use crate::core::tool::ToolDescriptor;
use crate::core::types::{Content, ContentPart, Message};

/// 每张图片估算 token 成本(保守,倾向提前触发 compaction)。
pub const IMAGE_TOKEN_COST: u32 = 1200;

/// 估算单条 message 的 token 数。
pub fn estimate_message_tokens(m: &Message) -> u32 {
    match m {
        Message::System { content }
        | Message::User { content }
        | Message::Assistant { content, .. } => {
            let base = estimate_content_tokens(content);
            // Assistant 还可能带 tool_calls,估算 name + args json
            if let Message::Assistant { tool_calls, .. } = m {
                let calls_tokens: u32 = tool_calls
                    .iter()
                    .map(|c| {
                        estimate_text_tokens(&c.tool_name)
                            + estimate_text_tokens(&c.args.to_string())
                            + 4 // 协议 overhead(id、类型标签等)
                    })
                    .sum();
                base + calls_tokens
            } else {
                base
            }
        }
        Message::ToolResult { result, .. } => estimate_text_tokens(&result.text()) + 4,
        Message::Observation { text, .. } => estimate_text_tokens(text) + 2,
    }
}

/// 估算整个 history。
pub fn estimate_history_tokens(history: &[Message]) -> u32 {
    history.iter().map(estimate_message_tokens).sum()
}

/// 估算 system prompt 长度。
pub fn estimate_system_tokens(s: &str) -> u32 {
    estimate_text_tokens(s)
}

/// 估算 tool descriptors(它们拼进 ModelRequest 的 tools 字段也占 tokens)。
pub fn estimate_tools_tokens(tools: &[ToolDescriptor]) -> u32 {
    tools
        .iter()
        .map(
            |t| {
                estimate_text_tokens(&t.name)
                    + estimate_text_tokens(&t.description)
                    + estimate_text_tokens(&t.schema_json.to_string())
                    + 8
            }, // JSON 包装 overhead
        )
        .sum()
}

/// 把 Content 翻成 token 估算(Parts 内 Image 按固定成本)。
pub fn estimate_content_tokens(c: &Content) -> u32 {
    match c {
        Content::Text(s) => estimate_text_tokens(s),
        Content::Parts(parts) => parts.iter().map(estimate_part_tokens).sum(),
    }
}

fn estimate_part_tokens(p: &ContentPart) -> u32 {
    match p {
        ContentPart::Text { text } => estimate_text_tokens(text),
        ContentPart::Image { .. } => IMAGE_TOKEN_COST,
        ContentPart::Data { mime, b64 } => estimate_text_tokens(mime) + (b64.len() as u32) / 3 + 4,
    }
}

/// 纯文本的保守 token 估算。
///
/// 策略:`max(ceil(chars/3), ceil(bytes/4))`。
/// - `chars/3`:1 token ≈ 3 chars(英文 ~4,中文 ~1;取中间偏低以免低估)
/// - `bytes/4`:UTF-8 bytes 约束;中文字符 3 bytes,emoji 4 bytes,这个下界避免漏估
///
/// 取 max 保证两种 mix 都不会低估。
pub fn estimate_text_tokens(s: &str) -> u32 {
    let by_chars = (s.chars().count() as u32).div_ceil(3);
    let by_bytes = (s.len() as u32).div_ceil(4);
    by_chars.max(by_bytes)
}

// =================== 完整 request 估算 helper ===================

/// 估算完整发给模型的 context token 数。
pub fn estimate_context_tokens(system: &str, history: &[Message], tools: &[ToolDescriptor]) -> u32 {
    estimate_system_tokens(system) + estimate_history_tokens(history) + estimate_tools_tokens(tools)
}
