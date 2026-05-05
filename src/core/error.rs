//! `RuntimeError` + `ErrorClass` + 子错误 classify。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("model error: {0}")]
    Model(#[from] ModelError),

    #[error("tool executor error: {0}")]
    ToolExecutor(#[from] ToolExecutorError),

    #[error("store error: {0}")]
    Store(#[from] StoreError),

    #[error("cancelled")]
    Cancelled,

    #[error("invariant violation: {0}")]
    InvariantViolation(&'static str),

    #[error("submit during run")]
    SubmitDuringRun,

    #[error("invalid resume")]
    InvalidResume,

    #[error("blocked by hook: {0}")]
    HookBlocked(String),
}

impl RuntimeError {
    pub fn classify(&self) -> ErrorClass {
        use RuntimeError::*;
        match self {
            Model(e) => e.classify(),
            ToolExecutor(e) => e.classify(),
            Store(e) => ErrorClass::Store(e.classify()),
            Cancelled => ErrorClass::Cancelled,
            InvariantViolation(_) | SubmitDuringRun | InvalidResume => ErrorClass::Bug,
            HookBlocked(_) => ErrorClass::PolicyDenied,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    ToolFailure { retryable: bool },
    ProviderTransient,
    ProviderFatal,
    ContextTooLong,
    Store(StoreErrClass),
    PolicyDenied,
    Bug,
    Cancelled,
}

// ============ Model ============

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("transient: {0}")]
    Transient(String),

    #[error("fatal: {0}")]
    Fatal(String),

    #[error("auth: {0}")]
    Auth(String),

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("context overflow")]
    ContextOverflow,

    #[error("rate limited")]
    RateLimited { retry_after_ms: Option<u32> },

    #[error("parse error: {0}")]
    Parse(String),

    #[error("cancelled")]
    Cancelled,
}

impl ModelError {
    pub fn classify(&self) -> ErrorClass {
        use ModelError::*;
        match self {
            Transient(_) | RateLimited { .. } => ErrorClass::ProviderTransient,
            Fatal(_) | Auth(_) | InvalidRequest(_) | Parse(_) => ErrorClass::ProviderFatal,
            ContextOverflow => ErrorClass::ContextTooLong,
            Cancelled => ErrorClass::Cancelled,
        }
    }
}

// ============ ToolExecutor ============

#[derive(Debug, Error)]
pub enum ToolExecutorError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),

    #[error("schema parse: {0}")]
    SchemaParse(String),

    #[error("bundle unavailable")]
    BundleUnavailable,

    #[error("internal: {0}")]
    Internal(String),
}

impl ToolExecutorError {
    pub fn classify(&self) -> ErrorClass {
        use ToolExecutorError::*;
        match self {
            UnknownTool(_) | SchemaParse(_) | Internal(_) => ErrorClass::Bug,
            BundleUnavailable => ErrorClass::ToolFailure { retryable: false },
        }
    }
}

// ============ Store ============

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("transient: {0}")]
    Transient(String),

    #[error("stale state (expected seq {expected}, got {actual})")]
    StaleState { expected: u64, actual: u64 },

    #[error("corrupt: {0}")]
    Corrupt(String),

    #[error("io: {0}")]
    Io(String),

    #[error("incompatible schema: found {found}, supported_max {supported_max}")]
    Incompatible { found: u32, supported_max: u32 },

    #[error("not found")]
    NotFound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreErrClass {
    Transient,
    Conflict,
    Fatal,
}

impl StoreError {
    pub fn classify(&self) -> StoreErrClass {
        use StoreError::*;
        match self {
            Transient(_) | Io(_) => StoreErrClass::Transient,
            StaleState { .. } => StoreErrClass::Conflict,
            Corrupt(_) | Incompatible { .. } | NotFound => StoreErrClass::Fatal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_parse_errors_are_provider_fatal() {
        assert_eq!(
            ModelError::Parse("malformed provider response".into()).classify(),
            ErrorClass::ProviderFatal
        );
    }
}
