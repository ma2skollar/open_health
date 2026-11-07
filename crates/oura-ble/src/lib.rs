//! Single-owner, bounded ring operations for the collector.
//!
//! The default build has no BLE dependency. [`FakeRing::new`] supplies a ring
//! and a cloneable script/call-log handle. Enable `ble` for [`BleRing`], which
//! uses the pinned `oura-link` client and its existing BLE transport exclusively.
//! No constructor installs a key, pairs, resets, updates firmware, clears ring
//! data, enters flight mode, or changes the ring clock.
//!
//! Cursors and `ring_timestamp` retain upstream's raw decisecond units, not Unix
//! seconds. A cursor is the next timestamp to fetch. Persist every returned
//! batch before advancing its cursor, including a batch with `complete == false`.
//! An incomplete batch means reconnect/resume, not an empty successful sync.

use std::future::Future;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod fake;
mod key;
#[cfg(feature = "ble")]
mod real;

pub use fake::{FakeRing, FakeRingHandle, ScriptOutcome, UnknownOperation};
pub use key::{AuthKey, AuthKeyError};
#[cfg(feature = "ble")]
pub use real::{BleOptions, BleRing};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub serial: String,
    pub firmware: String,
    pub hardware: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEvent {
    pub tag: u32,
    pub ring_timestamp: u64,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryBatch {
    pub events: Vec<HistoryEvent>,
    pub next_cursor: u64,
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HrSample {
    pub bpm: u16,
    pub unix: i64,
}

/// Deliberately excludes upstream diagnostic text and authentication material.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum RingError {
    #[error("ring absent")]
    Absent,
    #[error("ring out of range")]
    OutOfRange,
    #[error("ring busy")]
    Busy,
    #[error("ring operation failed")]
    Failed,
    #[error("ring operation cancelled")]
    Cancelled,
    #[error("ring operation expired")]
    Expired,
    #[error("ring operation timed out: {operation}")]
    Timeout { operation: &'static str },
    #[error("history incomplete without a usable checkpoint")]
    PartialHistory,
    #[error("ring disconnected")]
    Disconnected,
}

/// Stable operation names also used by fake scripts and timeout errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Connect,
    Disconnect,
    HardwareId,
    DeviceInfo,
    Battery,
    DrainHistory,
    FeatureMode,
    SetFeatureMode,
    LiveHr,
    StopLiveHr,
}

impl Operation {
    pub const ALL: [Self; 10] = [
        Self::Connect,
        Self::Disconnect,
        Self::HardwareId,
        Self::DeviceInfo,
        Self::Battery,
        Self::DrainHistory,
        Self::FeatureMode,
        Self::SetFeatureMode,
        Self::LiveHr,
        Self::StopLiveHr,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Disconnect => "disconnect",
            Self::HardwareId => "hardware_id",
            Self::DeviceInfo => "device_info",
            Self::Battery => "battery",
            Self::DrainHistory => "drain_history",
            Self::FeatureMode => "feature_mode",
            Self::SetFeatureMode => "set_feature_mode",
            Self::LiveHr => "live_hr",
            Self::StopLiveHr => "stop_live_hr",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|operation| operation.as_str() == name)
    }
}

/// Every network wait has an outer deadline. Live capture is capped separately;
/// setup may take `operation`, and teardown may take `teardown` after capture.
#[derive(Clone, Debug)]
pub struct OperationTimeouts {
    pub connect: Duration,
    pub operation: Duration,
    pub history: Duration,
    pub live_hr_max: Duration,
    pub teardown: Duration,
}

impl Default for OperationTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(45),
            operation: Duration::from_secs(10),
            history: Duration::from_secs(120),
            live_hr_max: Duration::from_secs(60),
            teardown: Duration::from_secs(5),
        }
    }
}

impl OperationTimeouts {
    pub fn for_operation(&self, operation: Operation) -> Duration {
        match operation {
            Operation::Connect => self.connect,
            Operation::Disconnect | Operation::StopLiveHr => self.teardown,
            Operation::DrainHistory => self.history,
            _ => self.operation,
        }
    }
}

/// One coordinator must own this value. Dropping a live-HR future initiates
/// bounded best-effort teardown; call `stop_live_hr` and `disconnect` on shutdown
/// while the Tokio runtime is still alive so cleanup can be awaited.
#[async_trait]
pub trait RingTransport: Send {
    async fn connect(&mut self) -> Result<(), RingError>;
    async fn disconnect(&mut self) -> Result<(), RingError>;
    async fn hardware_id(&mut self) -> Result<String, RingError>;
    async fn device_info(&mut self) -> Result<DeviceInfo, RingError>;
    async fn battery(&mut self) -> Result<u8, RingError>;
    async fn drain_history(&mut self, cursor: u64) -> Result<HistoryBatch, RingError>;
    /// Daytime-HR feature mode, widened from upstream's byte to `u32`.
    async fn feature_mode(&mut self) -> Result<u32, RingError>;
    /// Changes only daytime HR. Valid modes: OFF=0, AUTOMATIC=1, REQUESTED=2,
    /// CONNECTED_LIVE=3. Never interpreted as a feature bit mask.
    async fn set_feature_mode(&mut self, mode: u32) -> Result<(), RingError>;
    async fn live_hr(&mut self, duration: Duration) -> Result<Vec<HrSample>, RingError>;
    async fn stop_live_hr(&mut self) -> Result<(), RingError>;
}

pub(crate) async fn bounded<T>(
    operation: Operation,
    timeout: Duration,
    future: impl Future<Output = Result<T, RingError>>,
) -> Result<T, RingError> {
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| RingError::Timeout { operation: operation.as_str() })?
}
