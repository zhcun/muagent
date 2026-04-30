//! Pre-wire message normalization based on target `LlmCaps`.
//!
//! # Why this exists
//!
//! Each provider adapter converts `Message` → provider-specific JSON. That
//! translation is genuinely different across providers (Anthropic uses
//! `tool_result` blocks, OpenAI uses `role=tool`, Gemini uses
//! `functionResponse`), so we can't unify it.
//!
//! But **"should this image be included at all?"** has the same answer
//! everywhere — it depends on the model's advertised `caps`, not on the
//! wire format. We used to check `caps.vision` inside every adapter's
//! `msg_to_xxx`; adding a new gate (say, `caps.supports_audio`) meant
//! patching three functions. That's a clear missing-abstraction smell.
//!
//! This module fixes it. Adapters do:
//!
//! ```ignore
//! let messages = prepare_messages_for_caps(&req.messages, &self.caps);
//! // … translate `messages` to wire format …
//! ```
//!
//! and never look at `self.caps` again. New capability gates land here,
//! once, with a test.
//!
//! # What this does (today)
//!
//! - `caps.vision == false`: drop `ContentPart::Image` attachments from
//!   `ToolResult` and append a human-readable note to `result.content`
//!   ("`[N image attachment(s) dropped: model does not support vision]`").
//!   User-message image parts are left alone — those were put there
//!   deliberately by the caller; we're only protecting automatic
//!   tool-produced images.
//! - `caps.thinking == ThinkingSupport::None`: strip `thinking` artifacts
//!   from Assistant messages. They'd just waste tokens on replay.

use crate::core::model::LlmCaps;
use crate::core::thinking::ThinkingSupport;
use crate::core::tool::ToolResult;
use crate::core::types::{Content, ContentPart, Message};

/// Clone-and-filter `msgs` so it only contains content this model can handle.
/// Always returns a new `Vec` — callers translate from that into wire format.
pub fn prepare_messages_for_caps(msgs: &[Message], caps: &LlmCaps) -> Vec<Message> {
    msgs.iter().map(|m| filter_message(m, caps)).collect()
}

fn filter_message(m: &Message, caps: &LlmCaps) -> Message {
    match m {
        Message::ToolResult { call_id, result } => {
            let result = filter_tool_result(result, caps);
            Message::ToolResult {
                call_id: call_id.clone(),
                result,
            }
        }
        Message::Assistant {
            content,
            tool_calls,
            thinking,
        } => {
            // Clear replay if the provider can't use it.
            let thinking = if matches!(caps.thinking, ThinkingSupport::None) {
                vec![]
            } else {
                thinking.clone()
            };
            Message::Assistant {
                content: content.clone(),
                tool_calls: tool_calls.clone(),
                thinking,
            }
        }
        // User / System / Observation pass through unchanged. If caller
        // supplied an image to a non-vision model, let it fail at the wire
        // level — it's an explicit user choice, not something we should
        // silently strip.
        _ => m.clone(),
    }
}

fn filter_tool_result(r: &ToolResult, caps: &LlmCaps) -> ToolResult {
    if caps.vision {
        return r.clone();
    }
    let parts = match &r.content {
        Content::Text(_) => return r.clone(),
        Content::Parts(p) => p,
    };
    let n_img = parts
        .iter()
        .filter(|a| matches!(a, ContentPart::Image { .. }))
        .count();
    if n_img == 0 {
        return r.clone();
    }
    // Drop image parts; keep text + other (data) parts, then append a note
    // so the model still sees something happened.
    let mut kept: Vec<ContentPart> = parts
        .iter()
        .filter(|a| !matches!(a, ContentPart::Image { .. }))
        .cloned()
        .collect();
    kept.push(ContentPart::Text {
        text: format!("[{n_img} image attachment(s) dropped: model does not support vision]"),
    });
    ToolResult {
        ok: r.ok,
        content: Content::Parts(kept),
        retryable: r.retryable,
        hint: r.hint.clone(),
        detail: r.detail.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{Content, ContentPart};

    fn vision_caps(on: bool) -> LlmCaps {
        LlmCaps {
            vision: on,
            ..Default::default()
        }
    }

    fn tool_result_with_image(text: &str) -> ToolResult {
        ToolResult::ok_parts(vec![
            ContentPart::Text { text: text.into() },
            ContentPart::Image {
                uri: None,
                b64: Some("AAAA".into()),
                mime: "image/png".into(),
            },
        ])
    }

    #[test]
    fn tool_result_images_dropped_when_no_vision() {
        let msgs = vec![Message::ToolResult {
            call_id: "c1".into(),
            result: tool_result_with_image("screenshot ready"),
        }];
        let out = prepare_messages_for_caps(&msgs, &vision_caps(false));
        match &out[0] {
            Message::ToolResult { result, .. } => {
                assert_eq!(result.attachments().count(), 0);
                assert!(result.text().contains("dropped"));
                assert!(result.text().contains("screenshot ready"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn tool_result_images_kept_when_vision_on() {
        let msgs = vec![Message::ToolResult {
            call_id: "c1".into(),
            result: tool_result_with_image("ok"),
        }];
        let out = prepare_messages_for_caps(&msgs, &vision_caps(true));
        match &out[0] {
            Message::ToolResult { result, .. } => {
                assert_eq!(result.attachments().count(), 1);
                assert!(!result.text().contains("dropped"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn assistant_thinking_stripped_when_provider_has_no_thinking() {
        use crate::core::thinking::{
            ReplayPolicy, ThinkingArtifact, ThinkingKind, ThinkingPayload, ThinkingVisibility,
        };
        let msgs = vec![Message::Assistant {
            content: Content::text("ok"),
            tool_calls: vec![],
            thinking: vec![ThinkingArtifact {
                provider: "x".into(),
                kind: ThinkingKind::FullText,
                replay: ReplayPolicy::MustReplayUnmodified,
                visibility: ThinkingVisibility::Hidden,
                payload: ThinkingPayload::Text {
                    text: "reasoning...".into(),
                },
                provider_signature: None,
            }],
        }];
        let caps = LlmCaps {
            thinking: ThinkingSupport::None,
            ..Default::default()
        };
        let out = prepare_messages_for_caps(&msgs, &caps);
        match &out[0] {
            Message::Assistant { thinking, .. } => assert!(thinking.is_empty()),
            _ => panic!(),
        }
    }

    #[test]
    fn assistant_thinking_kept_when_replay_supported() {
        use crate::core::thinking::{
            ReplayPolicy, ThinkingArtifact, ThinkingKind, ThinkingPayload, ThinkingVisibility,
        };
        let msgs = vec![Message::Assistant {
            content: Content::text("ok"),
            tool_calls: vec![],
            thinking: vec![ThinkingArtifact {
                provider: "x".into(),
                kind: ThinkingKind::FullText,
                replay: ReplayPolicy::MustReplayUnmodified,
                visibility: ThinkingVisibility::Hidden,
                payload: ThinkingPayload::Text {
                    text: "reasoning...".into(),
                },
                provider_signature: None,
            }],
        }];
        let caps = LlmCaps {
            thinking: ThinkingSupport::FullReplay,
            ..Default::default()
        };
        let out = prepare_messages_for_caps(&msgs, &caps);
        match &out[0] {
            Message::Assistant { thinking, .. } => assert_eq!(thinking.len(), 1),
            _ => panic!(),
        }
    }

    #[test]
    fn user_image_untouched_even_without_vision() {
        // User-supplied images are a deliberate choice; let the provider
        // reject at wire level if it wants, don't silently strip.
        let msgs = vec![Message::User {
            content: Content::Parts(vec![
                ContentPart::Text {
                    text: "see this".into(),
                },
                ContentPart::Image {
                    uri: None,
                    b64: Some("X".into()),
                    mime: "image/png".into(),
                },
            ]),
        }];
        let out = prepare_messages_for_caps(&msgs, &vision_caps(false));
        match &out[0] {
            Message::User {
                content: Content::Parts(p),
            } => assert_eq!(p.len(), 2),
            _ => panic!(),
        }
    }
}
