//! `NetEgress` adapter trait + supporting types and helpers.
//!
//! Lives in core because model adapters and MCP HTTP transports need the
//! abstract trait, but none of them want a runtime IO dependency. The concrete
//! impl is selected by the host.

use async_trait::async_trait;

use crate::core::cancel::CancelToken;
use crate::core::error::ModelError;

pub type HeaderMap = std::collections::HashMap<String, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
    Head,
    Options,
}

impl HttpMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
        }
    }
}

#[derive(Clone, Debug)]
pub struct HttpReq {
    pub method: HttpMethod,
    pub url: String,
    pub headers: HeaderMap,
    pub body: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct HttpResp {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum NetErr {
    #[error("denied: {0}")]
    Denied(String),

    #[error("dns: {0}")]
    Dns(String),

    #[error("connect: {0}")]
    Connect(String),

    #[error("tls: {0}")]
    Tls(String),

    #[error("timeout")]
    Timeout,

    #[error("http status {status}: {reason}")]
    HttpStatus { status: u16, reason: String },

    #[error("io: {0}")]
    Io(String),

    #[error("cancelled")]
    Cancelled,
}

#[async_trait]
pub trait NetEgress: Send + Sync {
    async fn http(&self, req: HttpReq, cancel: CancelToken) -> Result<HttpResp, NetErr>;
}

// =================== Shared helpers for ModelAdapter impls ===================

/// NetErr → ModelError classification used by every `ModelAdapter` impl.
///
/// - Network layer errors (DNS, connect, io, timeout) → `Transient`.
/// - Cancel → `Cancelled` (not retryable).
/// - Denial, TLS → `Fatal`.
/// - `HttpStatus` is classified by code: 408/429/5xx → `Transient`, else `Fatal`.
pub fn net_err_to_model(e: NetErr) -> ModelError {
    use NetErr::*;
    match e {
        Denied(s) => ModelError::Fatal(format!("net denied: {s}")),
        Dns(s) | Connect(s) => ModelError::Transient(s),
        Tls(s) => ModelError::Fatal(format!("tls: {s}")),
        Timeout => ModelError::Transient("timeout".into()),
        HttpStatus { status, reason } if matches!(status, 408 | 429 | 500..=599) => {
            ModelError::Transient(format!("{status}: {reason}"))
        }
        HttpStatus { status, reason } => ModelError::Fatal(format!("{status}: {reason}")),
        Io(s) => ModelError::Transient(s),
        Cancelled => ModelError::Cancelled,
    }
}

/// Map a successful HTTP response's status to `Ok(())` (200) or a typed
/// `ModelError`. Common classification used by OpenAI / Anthropic / Google
/// JSON endpoints.
///
/// - 200 → Ok
/// - 401/403 → Auth
/// - 400 → InvalidRequest (or ContextOverflow when the body clearly says so)
/// - 413 → ContextOverflow
/// - 408/429/5xx → Transient
/// - other → Fatal
pub fn check_model_status(resp: &HttpResp) -> Result<(), ModelError> {
    match resp.status {
        200 => Ok(()),
        401 | 403 => Err(ModelError::Auth(fmt_body(resp.status, &resp.body))),
        400 if looks_like_context_overflow(&resp.body) => Err(ModelError::ContextOverflow),
        400 => Err(ModelError::InvalidRequest(fmt_body(
            resp.status,
            &resp.body,
        ))),
        413 => Err(ModelError::ContextOverflow),
        408 | 429 | 500..=599 => Err(ModelError::Transient(fmt_body(resp.status, &resp.body))),
        _ => Err(ModelError::Fatal(fmt_body(resp.status, &resp.body))),
    }
}

fn fmt_body(status: u16, body: &[u8]) -> String {
    format!("status {}: {}", status, String::from_utf8_lossy(body))
}

fn looks_like_context_overflow(body: &[u8]) -> bool {
    let s = String::from_utf8_lossy(body).to_ascii_lowercase();
    [
        "context_length_exceeded",
        "context window",
        "maximum context",
        "too many tokens",
        "prompt is too long",
        "exceeds the maximum number of tokens",
    ]
    .iter()
    .any(|needle| s.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(status: u16, body: &str) -> HttpResp {
        HttpResp {
            status,
            headers: Default::default(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn request_too_large_is_context_overflow() {
        assert!(matches!(
            check_model_status(&resp(413, "request too large")),
            Err(ModelError::ContextOverflow)
        ));
    }

    #[test]
    fn provider_context_400_is_context_overflow() {
        assert!(matches!(
            check_model_status(&resp(
                400,
                r#"{"error":{"code":"context_length_exceeded"}}"#
            )),
            Err(ModelError::ContextOverflow)
        ));
    }

    #[test]
    fn ordinary_400_stays_invalid_request() {
        assert!(matches!(
            check_model_status(&resp(400, "bad tool schema")),
            Err(ModelError::InvalidRequest(_))
        ));
    }
}
