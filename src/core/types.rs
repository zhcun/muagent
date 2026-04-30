//! Message / Content / ContentPart 等 serde 基础类型。

use serde::{Deserialize, Serialize};

use crate::core::event::CallId;
use crate::core::thinking::ThinkingArtifact;
use crate::core::tool::{PendingCall, ToolResult};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    System {
        content: Content,
    },
    User {
        content: Content,
    },
    Assistant {
        content: Content,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<PendingCall>,
        /// Reasoning artifacts attached to this assistant turn.
        /// Some providers (Anthropic) require these to be round-tripped
        /// verbatim on the next tool-use turn; see `ReplayPolicy`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        thinking: Vec<ThinkingArtifact>,
    },
    ToolResult {
        call_id: CallId,
        result: ToolResult,
    },
    Observation {
        kind: ObsKind,
        text: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl Content {
    pub fn text<S: Into<String>>(s: S) -> Self {
        Self::Text(s.into())
    }
}

/// Validation: an `Image` with neither `uri` nor `b64` set is structurally
/// valid JSON but semantically dead — every adapter would silently drop it,
/// the model would never see the image, and the host would have no signal
/// that anything went wrong. Catching it at the deserialize boundary turns
/// a silent data-loss bug into a loud parse error.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    Image {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        b64: Option<String>,
        mime: String,
    },
    Data {
        mime: String,
        b64: String,
    },
}

// Manual `Deserialize` via a permissive proxy that we then validate. We
// can't use `#[serde(try_from = ...)]` on the enum directly because each
// variant needs its own check.
impl<'de> Deserialize<'de> for ContentPart {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum Raw {
            Text {
                text: String,
            },
            Image {
                #[serde(default)]
                uri: Option<String>,
                #[serde(default)]
                b64: Option<String>,
                mime: String,
            },
            Data {
                mime: String,
                b64: String,
            },
        }
        let raw = Raw::deserialize(d)?;
        Ok(match raw {
            Raw::Text { text } => ContentPart::Text { text },
            Raw::Image { uri, b64, mime } => {
                if uri.is_none() && b64.is_none() {
                    return Err(serde::de::Error::custom(
                        "ContentPart::Image must have either `uri` or `b64` (mime alone is not a valid image)",
                    ));
                }
                ContentPart::Image { uri, b64, mime }
            }
            Raw::Data { mime, b64 } => ContentPart::Data { mime, b64 },
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObsKind {
    System,
    Steering,
    Summary,
    User,
}

#[cfg(test)]
mod content_part_tests {
    use super::*;

    #[test]
    fn image_with_uri_or_b64_deserializes() {
        let with_uri =
            serde_json::json!({"kind":"image","uri":"https://x/y.png","mime":"image/png"});
        let p: ContentPart = serde_json::from_value(with_uri).unwrap();
        assert!(matches!(p, ContentPart::Image { uri: Some(_), .. }));

        let with_b64 = serde_json::json!({"kind":"image","b64":"AAAA","mime":"image/png"});
        let p: ContentPart = serde_json::from_value(with_b64).unwrap();
        assert!(matches!(p, ContentPart::Image { b64: Some(_), .. }));
    }

    #[test]
    fn image_with_neither_uri_nor_b64_is_rejected() {
        // Pre-fix this used to silently deserialize, then get dropped at
        // wire time — the model never saw the image and the host got no
        // signal of the failure.
        let bad = serde_json::json!({"kind":"image","mime":"image/png"});
        let err = serde_json::from_value::<ContentPart>(bad).unwrap_err();
        assert!(
            err.to_string().contains("uri") || err.to_string().contains("b64"),
            "error should name the missing fields; got: {err}"
        );
    }

    #[test]
    fn other_variants_still_parse_normally() {
        let t = serde_json::json!({"kind":"text","text":"hi"});
        assert!(matches!(
            serde_json::from_value::<ContentPart>(t).unwrap(),
            ContentPart::Text { .. }
        ));

        let d = serde_json::json!({"kind":"data","mime":"application/pdf","b64":"AAAA"});
        assert!(matches!(
            serde_json::from_value::<ContentPart>(d).unwrap(),
            ContentPart::Data { .. }
        ));
    }
}
