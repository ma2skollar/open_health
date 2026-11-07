//! Pi-side collection, durable snapshots and outbound control.
//!
//! One coordinator owns the ring. Network delivery never owns a Bluetooth link.
pub mod clock;
pub mod config;
pub mod control;
pub mod coordinator;
pub mod delivery;
pub mod error;
pub mod scheduler;
pub mod storage;

pub use clock::{Clock, FakeClock, SystemClock};
pub use config::Config;
pub use coordinator::{CommandOutcome, Coordinator, LocalCommand};
pub use error::CollectorError;
pub use storage::{RingStore, SnapshotQueue};
