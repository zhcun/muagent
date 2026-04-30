//! `Clock` helper:wallclock + budget hint + async sleep。

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
    fn budget_hint(&self) -> BudgetHint {
        BudgetHint::Unlimited
    }

    /// Async sleep for `dur`. Used by Runner's retry backoff.
    fn sleep<'a>(&'a self, dur: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            tokio::time::sleep(dur).await;
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BudgetHint {
    Unlimited,
    IosBackground { remaining_ms: i64 },
    AndroidWorker { total_cap_ms: i64 },
    AndroidForegroundService,
    Custom { remaining_ms: i64, source: String },
}

impl BudgetHint {
    pub fn soft_floor_breached(&self, floor_ms: i64) -> bool {
        match self {
            Self::Unlimited | Self::AndroidForegroundService => false,
            Self::IosBackground { remaining_ms }
            | Self::AndroidWorker {
                total_cap_ms: remaining_ms,
            }
            | Self::Custom { remaining_ms, .. } => *remaining_ms < floor_ms,
        }
    }
}

/// Default `Clock` impl backed by `std::time::SystemTime`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}
