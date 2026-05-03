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

/// Format a Unix-epoch millisecond stamp as a UTC `YYYY-MM-DD` date string.
/// Uses Howard Hinnant's civil calendar conversion to keep dependency
/// surface small (no `chrono` / `time`).
pub fn utc_date_string(now_ms: i64) -> String {
    let days = now_ms.div_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Howard Hinnant's civil calendar conversion, adapted for days since the
/// Unix epoch. Pure: no allocation, no system calls.
fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    y += if m <= 2 { 1 } else { 0 };
    (y as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_date_string_known_dates() {
        assert_eq!(utc_date_string(0), "1970-01-01");
        // 2024-01-01 00:00:00 UTC = 1_704_067_200_000 ms
        assert_eq!(utc_date_string(1_704_067_200_000), "2024-01-01");
        // 2024-12-31 23:59:59 UTC
        assert_eq!(utc_date_string(1_735_689_599_000), "2024-12-31");
    }
}
