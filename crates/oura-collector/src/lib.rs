//! Raw transport buffering only. Physiological interpretation belongs on LXC.
pub mod clock;
pub mod config;
pub mod coordinator;
pub mod delivery;
pub mod error;
pub mod storage;
pub use clock::{Clock, FakeClock, SystemClock};
pub use config::Config;
pub use coordinator::Collector;
pub use error::CollectorError;
pub use storage::{Outbox, PendingBatch, QueueStats};

pub const COLLECTOR_BUILD: &str = match option_env!("OURA_COLLECTOR_BUILD") {
    Some(value) => value,
    None => env!("CARGO_PKG_VERSION"),
};
