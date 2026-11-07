//! Wall-clock boundary shared by real collection and deterministic scenarios.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

#[async_trait]
pub trait Clock: Send + Sync {
    fn now_unix(&self) -> i64;
    async fn sleep(&self, duration: Duration);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

#[async_trait]
impl Clock for SystemClock {
    fn now_unix(&self) -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
            Err(error) => -i64::try_from(error.duration().as_secs()).unwrap_or(i64::MAX),
        }
    }

    async fn sleep(&self, duration: Duration) { tokio::time::sleep(duration).await; }
}

#[derive(Clone, Debug)]
pub struct FakeClock {
    instant: Arc<Mutex<(i64, u32)>>,
}

impl FakeClock {
    pub fn new(unix: i64) -> Self { Self { instant: Arc::new(Mutex::new((unix, 0))) } }

    /// Advance without wall time, retaining subsecond sleep precision.
    pub fn advance(&self, duration: Duration) {
        let mut instant = self.instant.lock().unwrap_or_else(|error| error.into_inner());
        let nanos = instant.1 as u64 + duration.subsec_nanos() as u64;
        instant.0 = instant.0.saturating_add(i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
            .saturating_add((nanos / 1_000_000_000) as i64);
        instant.1 = (nanos % 1_000_000_000) as u32;
    }

    pub fn set(&self, unix: i64) {
        *self.instant.lock().unwrap_or_else(|error| error.into_inner()) = (unix, 0);
    }
}

#[async_trait]
impl Clock for FakeClock {
    fn now_unix(&self) -> i64 {
        self.instant.lock().unwrap_or_else(|error| error.into_inner()).0
    }

    async fn sleep(&self, duration: Duration) {
        self.advance(duration);
        tokio::task::yield_now().await;
    }
}
