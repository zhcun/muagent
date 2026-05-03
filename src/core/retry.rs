//! Retry policy for transient model errors.
//!
//! Only **model** calls get retried at the Runner level. Tool retries would
//! risk duplicating side effects and are the caller's concern (agent-level:
//! an agent can re-call a tool; framework-level: `Idempotency::AtMostOnce`
//! protects against silent duplication across crashes).
//!
//! Policy semantics:
//! - Applies only to `ModelError::Transient` and `ModelError::RateLimited`.
//! - Other variants (`Auth`, `Fatal`, `InvalidRequest`, `Parse`,
//!   `ContextOverflow`) surface immediately — retrying won't help.
//! - `RateLimited.retry_after_ms` overrides the computed backoff when set
//!   (capped at `max_backoff_ms`).
//! - Exponential backoff with the given multiplier, clamped at `max_backoff_ms`.

use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    /// Total attempts including the first. `1` means no retry.
    pub max_attempts: u32,
    /// Backoff before attempt #2.
    pub initial_backoff_ms: u64,
    /// Backoff ceiling per wait.
    pub max_backoff_ms: u64,
    /// Multiplier applied after each failed attempt.
    pub multiplier: f32,
}

impl Default for RetryPolicy {
    /// 3 attempts total (two retries), 1s → 2s → 4s exponential,
    /// max single wait 10s. Matches the common Anthropic / OpenAI SDK
    /// default. Rate-limit responses with `Retry-After` override the
    /// computed backoff.
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff_ms: 1_000,
            max_backoff_ms: 10_000,
            multiplier: 2.0,
        }
    }
}

impl RetryPolicy {
    /// No retries — fail fast. Useful for unit tests and strict callers.
    pub fn never() -> Self {
        Self {
            max_attempts: 1,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
            multiplier: 1.0,
        }
    }

    /// Build from `MUAGENT_MODEL_RETRY_*` environment variables, falling back
    /// to the [`Default`] schedule for any field that is unset or empty.
    /// Hosts that wire their own runtime call this once during setup.
    pub fn from_env() -> Result<Self, String> {
        fn parse_u<T: std::str::FromStr>(name: &str) -> Result<Option<T>, String> {
            match std::env::var(name) {
                Ok(raw) if raw.trim().is_empty() => Ok(None),
                Ok(raw) => raw
                    .parse::<T>()
                    .map(Some)
                    .map_err(|_| format!("{name} must be an unsigned integer")),
                Err(std::env::VarError::NotPresent) => Ok(None),
                Err(e) => Err(format!("{name}: {e}")),
            }
        }
        let mut policy = Self::default();
        if let Some(value) = parse_u::<u32>("MUAGENT_MODEL_RETRY_ATTEMPTS")? {
            policy.max_attempts = value.max(1);
        }
        if let Some(value) = parse_u::<u64>("MUAGENT_MODEL_RETRY_INITIAL_MS")? {
            policy.initial_backoff_ms = value;
        }
        if let Some(value) = parse_u::<u64>("MUAGENT_MODEL_RETRY_MAX_MS")? {
            policy.max_backoff_ms = value;
        }
        if policy.max_backoff_ms < policy.initial_backoff_ms {
            policy.max_backoff_ms = policy.initial_backoff_ms;
        }
        Ok(policy)
    }

    /// Wait duration before the `attempt`-th try (1-indexed: no wait before
    /// attempt 1, `initial_backoff_ms` before attempt 2, etc.). Honors the
    /// optional `retry_after_ms` hint from the provider when present.
    pub fn backoff_for(&self, attempt: u32, retry_after_ms: Option<u32>) -> Duration {
        if attempt <= 1 {
            return Duration::ZERO;
        }
        let factor = (self.multiplier as f64).powi((attempt - 2) as i32);
        let computed = (self.initial_backoff_ms as f64 * factor) as u64;
        let base = computed.min(self.max_backoff_ms);
        let effective = match retry_after_ms {
            Some(ra) => (ra as u64).max(base).min(self.max_backoff_ms),
            None => base,
        };
        Duration::from_millis(effective)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_schedule() {
        let p = RetryPolicy::default();
        assert_eq!(p.backoff_for(1, None), Duration::ZERO);
        assert_eq!(p.backoff_for(2, None), Duration::from_millis(1000));
        assert_eq!(p.backoff_for(3, None), Duration::from_millis(2000));
        assert_eq!(p.backoff_for(4, None), Duration::from_millis(4000));
    }

    #[test]
    fn retry_after_overrides_and_caps() {
        let p = RetryPolicy::default();
        // retry_after > computed → use retry_after (capped at max_backoff).
        assert_eq!(p.backoff_for(2, Some(5_000)), Duration::from_millis(5_000));
        assert_eq!(
            p.backoff_for(2, Some(30_000)),
            Duration::from_millis(10_000)
        );
        // retry_after < computed → use computed.
        assert_eq!(p.backoff_for(3, Some(100)), Duration::from_millis(2000));
    }

    #[test]
    fn never_policy() {
        let p = RetryPolicy::never();
        assert_eq!(p.max_attempts, 1);
    }
}
