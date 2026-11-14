use std::{sync::{Arc, atomic::{AtomicBool, AtomicI64, Ordering}}, time::{Duration, SystemTime, UNIX_EPOCH}};

pub trait Clock: Send + Sync {
    fn now_unix_ms(&self) -> i64;
    fn synchronized(&self) -> bool;
}

#[derive(Default)]
pub struct SystemClock;
impl Clock for SystemClock {
    fn now_unix_ms(&self) -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).ok()
            .and_then(|d| i64::try_from(d.as_millis()).ok()).unwrap_or(-1)
    }
    fn synchronized(&self) -> bool { os_synchronized() }
}

#[cfg(target_os = "linux")]
fn os_synchronized() -> bool {
    // modes=0 only queries the kernel clock. It does not change OS time.
    let mut state: libc::timex = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::adjtimex(&mut state) };
    result >= 0 && result != libc::TIME_ERROR && state.status & libc::STA_UNSYNC == 0
}
#[cfg(not(target_os = "linux"))]
fn os_synchronized() -> bool { false }

/// Explicit synthetic clock; never selected by production configuration or CLI.
#[derive(Clone)]
pub struct FakeClock {
    now: Arc<AtomicI64>,
    synced: Arc<AtomicBool>,
}
impl FakeClock {
    pub fn new(unix_ms: i64) -> Self { Self { now: Arc::new(AtomicI64::new(unix_ms)), synced: Arc::new(AtomicBool::new(true)) } }
    pub fn advance(&self, duration: Duration) { self.now.fetch_add(duration.as_millis() as i64, Ordering::SeqCst); }
    pub fn set_synchronized(&self, value: bool) { self.synced.store(value, Ordering::SeqCst); }
}
impl Clock for FakeClock {
    fn now_unix_ms(&self) -> i64 { self.now.load(Ordering::SeqCst) }
    fn synchronized(&self) -> bool { self.synced.load(Ordering::SeqCst) }
}
