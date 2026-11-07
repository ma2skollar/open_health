use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use oura_link::ble::BleTransport;
use oura_link::transport::Transport;
use oura_link::OuraClient;
use oura_protocol::protocol::{feature, feature_mode};
use tokio::task::JoinHandle;

use crate::{bounded, AuthKey, AuthKeyError, DeviceInfo, HistoryBatch, HistoryEvent, HrSample, Operation, OperationTimeouts, RingError, RingTransport};

/// Hard local ceiling even if operator-supplied timeout settings are larger.
const MAX_LIVE_HR_DURATION: Duration = Duration::from_secs(300);

#[derive(Clone, Debug)]
pub struct BleOptions {
    pub ring_name: String,
    pub address: Option<String>,
    pub scan_timeout: Duration,
    pub timeouts: OperationTimeouts,
}

impl Default for BleOptions {
    fn default() -> Self {
        Self {
            ring_name: "Oura".into(),
            address: None,
            scan_timeout: Duration::from_secs(10),
            timeouts: OperationTimeouts::default(),
        }
    }
}

/// Real ring using only pinned `oura-link::OuraClient<BleTransport>`.
///
/// Construct with `BleRing::from_key_file(options, path)` on Unix, or load an
/// `AuthKey` once and move it into `BleRing::new(options, key)`. Construction
/// does not touch Bluetooth. `connect` performs a bounded scan, connection,
/// authentication with the existing key, and upstream app-stream registration.
/// It never creates a bond, installs a key, resets, clears data, starts RData,
/// updates firmware, enters flight mode, or synchronizes the ring clock.
///
/// Keep one instance in one coordinator. Live capture always attempts an
/// AUTOMATIC-mode restore. Await `stop_live_hr` and `disconnect` before shutting
/// down Tokio. Dropping a live future schedules bounded cleanup on that runtime;
/// cleanup cannot run after the runtime or process has already stopped.
/// Debug/trace callsites in upstream are compiled out by this feature's tracing
/// dependency flags; this boundary never enables raw live-notification logging.
pub struct BleRing {
    options: BleOptions,
    key: AuthKey,
    client: Option<Arc<OuraClient<BleTransport>>>,
    live_active: Arc<AtomicBool>,
    pending_cleanup: Option<JoinHandle<Result<(), RingError>>>,
}

impl BleRing {
    pub fn new(options: BleOptions, key: AuthKey) -> Self {
        Self {
            options,
            key,
            client: None,
            live_active: Arc::new(AtomicBool::new(false)),
            pending_cleanup: None,
        }
    }

    pub fn from_key_file(options: BleOptions, path: &Path) -> Result<Self, AuthKeyError> {
        Ok(Self::new(options, AuthKey::load(path)?))
    }

    fn client(&self) -> Result<&Arc<OuraClient<BleTransport>>, RingError> {
        if self.live_active.load(Ordering::Acquire) { return Err(RingError::Busy); }
        self.client.as_ref().ok_or(RingError::Disconnected)
    }
}

fn map_error(error: oura_link::Error) -> RingError {
    match error {
        oura_link::Error::DeviceNotFound => RingError::Absent,
        oura_link::Error::ConnectTimeout => RingError::Timeout { operation: "connect" },
        oura_link::Error::Ble(_) => RingError::Disconnected,
        _ => RingError::Failed,
    }
}

/// Disconnect an authenticated/partially authenticated connection if connect's
/// future fails or is cancelled. Upstream owns scan and GATT setup before it
/// returns a transport; its API exposes no pre-return cleanup handle.
struct ConnectCleanup {
    client: Arc<OuraClient<BleTransport>>,
    timeout: Duration,
    armed: bool,
}

impl Drop for ConnectCleanup {
    fn drop(&mut self) {
        if !self.armed { return; }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let client = self.client.clone();
            let timeout = self.timeout;
            runtime.spawn(async move {
                let _ = bounded(Operation::Disconnect, timeout, async {
                    client.transport().disconnect().await.map_err(map_error)
                }).await;
            });
        }
    }
}

async fn connect(options: &BleOptions, key: &AuthKey) -> Result<Arc<OuraClient<BleTransport>>, RingError> {
    bounded(Operation::Connect, options.timeouts.connect, async {
        let transport = BleTransport::connect(
            &options.ring_name,
            options.address.as_deref(),
            options.scan_timeout.min(options.timeouts.connect),
        ).await.map_err(map_error)?;
        let client = Arc::new(OuraClient::new(transport));
        let mut guard = ConnectCleanup { client: client.clone(), timeout: options.timeouts.teardown, armed: true };
        client.authenticate(key.bytes()).await.map_err(map_error)?;
        client.setup_app_stream().await.map_err(map_error)?;
        guard.armed = false;
        Ok(client)
    }).await
}

async fn device_info<T: Transport>(client: &OuraClient<T>, timeout: Duration) -> Result<DeviceInfo, RingError> {
    bounded(Operation::DeviceInfo, timeout, async {
        let serial = client.serial().await.map_err(map_error)?;
        let firmware = client.firmware().await.map_err(map_error)?.firmware_version;
        let hardware = client.hardware_id().await.map_err(map_error)?;
        Ok(DeviceInfo { serial, firmware, hardware })
    }).await
}

struct DrainBuffer {
    events: Vec<HistoryEvent>,
    committed_len: usize,
    next_cursor: u64,
}

async fn drain_history<T: Transport>(client: &OuraClient<T>, cursor: u64, timeout: Duration) -> Result<HistoryBatch, RingError> {
    // Upstream addresses the ring with u32 deciseconds. Never truncate a caller's
    // checkpoint and accidentally fetch an unrelated range.
    let upstream_cursor = u32::try_from(cursor).map_err(|_| RingError::Failed)?;
    let buffer = Mutex::new(DrainBuffer { events: Vec::new(), committed_len: 0, next_cursor: cursor });
    let result = bounded(Operation::DrainHistory, timeout, async {
        client.drain_events(
            upstream_cursor,
            |event| {
                let mut buffer = buffer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                buffer.events.push(HistoryEvent {
                    tag: u32::from(event.tag),
                    ring_timestamp: u64::from(event.timestamp),
                    body: event.body.clone(),
                });
                true
            },
            |progress| {
                let mut buffer = buffer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                buffer.committed_len = buffer.events.len();
                buffer.next_cursor = u64::from(progress.next_cursor);
                true
            },
        ).await.map_err(map_error)
    }).await;
    let mut buffer = buffer.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);
    buffer.events.truncate(buffer.committed_len);
    match result {
        Ok(outcome) => Ok(HistoryBatch { events: buffer.events, next_cursor: u64::from(outcome.next_cursor), complete: true }),
        // The shared contract has no error+progress envelope. Return checkpointed
        // events as an incomplete batch instead of losing them on a later error
        // or timeout. With no progress, preserve the original typed error.
        Err(_) if buffer.committed_len > 0 || buffer.next_cursor > cursor => Ok(HistoryBatch {
            events: buffer.events,
            next_cursor: buffer.next_cursor,
            complete: false,
        }),
        Err(error) => Err(error),
    }
}

async fn restore_live<T: Transport>(client: &OuraClient<T>, timeout: Duration) -> Result<(), RingError> {
    bounded(Operation::StopLiveHr, timeout, async {
        client.set_feature_mode(feature::DAYTIME_HR, feature_mode::AUTOMATIC).await.map_err(map_error)
    }).await
}

struct LiveCleanup<'a, T: Transport + 'static> {
    client: Arc<OuraClient<T>>,
    timeout: Duration,
    active: Arc<AtomicBool>,
    pending: &'a mut Option<JoinHandle<Result<(), RingError>>>,
    armed: bool,
}

impl<T: Transport + 'static> LiveCleanup<'_, T> {
    async fn finish(&mut self) -> Result<(), RingError> {
        let result = restore_live(&self.client, self.timeout).await;
        self.armed = false;
        self.active.store(false, Ordering::Release);
        result
    }
}

impl<T: Transport + 'static> Drop for LiveCleanup<'_, T> {
    fn drop(&mut self) {
        if !self.armed { return; }
        let client = self.client.clone();
        let active = self.active.clone();
        let timeout = self.timeout;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            *self.pending = Some(runtime.spawn(async move {
                let result = restore_live(&client, timeout).await;
                active.store(false, Ordering::Release);
                result
            }));
        } else {
            // No async cleanup can outlive the runtime. Callers must await
            // shutdown while the runtime remains available.
            active.store(false, Ordering::Release);
        }
    }
}

async fn live_hr<T: Transport + 'static>(
    client: Arc<OuraClient<T>>,
    timeouts: &OperationTimeouts,
    active: Arc<AtomicBool>,
    pending: &mut Option<JoinHandle<Result<(), RingError>>>,
    requested: Duration,
) -> Result<Vec<HrSample>, RingError> {
    let duration = requested.min(timeouts.live_hr_max).min(MAX_LIVE_HR_DURATION);
    if duration.is_zero() { return Ok(Vec::new()); }
    if active.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return Err(RingError::Busy);
    }
    let mut guard = LiveCleanup {
        client: client.clone(), timeout: timeouts.teardown, active, pending, armed: true,
    };
    let mut samples = Vec::new();
    let started = tokio::time::Instant::now();
    let deadline = duration.saturating_add(timeouts.operation);
    let result = bounded(Operation::LiveHr, deadline, async {
        // Upstream's false flag is essential: true prints raw notification bytes.
        client.live_heart_rate(duration, false, |sample| {
            let unix = SystemTime::now().duration_since(UNIX_EPOCH)
                .map(|value| value.as_secs().min(i64::MAX as u64) as i64).unwrap_or(0);
            samples.push(HrSample { bpm: sample.bpm, unix });
        }).await.map_err(map_error)?;
        // Upstream returns Ok when its notification channel closes early.
        if started.elapsed() < duration { return Err(RingError::Disconnected); }
        Ok(())
    }).await;
    let cleanup = guard.finish().await;
    result?;
    cleanup?;
    Ok(samples)
}

#[async_trait]
impl RingTransport for BleRing {
    async fn connect(&mut self) -> Result<(), RingError> {
        if self.live_active.load(Ordering::Acquire) { return Err(RingError::Busy); }
        if self.client.is_some() { return Ok(()); }
        self.client = Some(connect(&self.options, &self.key).await?);
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), RingError> {
        let cleanup = self.stop_live_hr().await;
        let Some(client) = self.client.take() else { return cleanup; };
        let result = bounded(Operation::Disconnect, self.options.timeouts.teardown, async {
            client.transport().disconnect().await.map_err(map_error)
        }).await;
        result.and(cleanup)
    }

    async fn hardware_id(&mut self) -> Result<String, RingError> {
        let client = self.client()?;
        bounded(Operation::HardwareId, self.options.timeouts.operation, async {
            client.hardware_id().await.map_err(map_error)
        }).await
    }

    async fn device_info(&mut self) -> Result<DeviceInfo, RingError> {
        device_info(self.client()?, self.options.timeouts.operation).await
    }

    async fn battery(&mut self) -> Result<u8, RingError> {
        let client = self.client()?;
        bounded(Operation::Battery, self.options.timeouts.operation, async {
            Ok(client.battery().await.map_err(map_error)?.percent)
        }).await
    }

    async fn drain_history(&mut self, cursor: u64) -> Result<HistoryBatch, RingError> {
        drain_history(self.client()?, cursor, self.options.timeouts.history).await
    }

    async fn feature_mode(&mut self) -> Result<u32, RingError> {
        let client = self.client()?;
        bounded(Operation::FeatureMode, self.options.timeouts.operation, async {
            Ok(u32::from(client.feature_status(feature::DAYTIME_HR).await.map_err(map_error)?.mode))
        }).await
    }

    async fn set_feature_mode(&mut self, mode: u32) -> Result<(), RingError> {
        if mode > u32::from(feature_mode::CONNECTED_LIVE) { return Err(RingError::Failed); }
        let client = self.client()?;
        bounded(Operation::SetFeatureMode, self.options.timeouts.operation, async {
            client.set_feature_mode(feature::DAYTIME_HR, mode as u8).await.map_err(map_error)
        }).await
    }

    async fn live_hr(&mut self, duration: Duration) -> Result<Vec<HrSample>, RingError> {
        let client = self.client()?.clone();
        // A completed cancellation cleanup may remain in its slot. Join it so
        // no previous teardown can race a newly enabled session.
        if let Some(task) = self.pending_cleanup.take() {
            task.await.map_err(|_| RingError::Failed)??;
        }
        live_hr(client, &self.options.timeouts, self.live_active.clone(), &mut self.pending_cleanup, duration).await
    }

    async fn stop_live_hr(&mut self) -> Result<(), RingError> {
        if let Some(task) = self.pending_cleanup.take() {
            return task.await.map_err(|_| RingError::Failed)?;
        }
        if !self.live_active.load(Ordering::Acquire) { return Ok(()); }
        let Some(client) = self.client.as_ref() else {
            self.live_active.store(false, Ordering::Release);
            return Err(RingError::Disconnected);
        };
        let result = restore_live(client, self.options.timeouts.teardown).await;
        self.live_active.store(false, Ordering::Release);
        result
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::future::pending;
    use tokio::sync::broadcast;
    use super::*;

    /// Exercises the actual pinned client without constructing a BLE adapter.
    struct Wire {
        tx: broadcast::Sender<Vec<u8>>,
        writes: Mutex<Vec<Vec<u8>>>,
        responses: HashMap<Vec<u8>, Vec<Vec<u8>>>,
        hang_live_start: bool,
        fail_live_start: bool,
        closed_notifications: bool,
    }

    impl Wire {
        fn new() -> Self {
            let (tx, _) = broadcast::channel(32);
            let restore = oura_protocol::protocol::req_set_feature_mode(feature::DAYTIME_HR, feature_mode::AUTOMATIC);
            let mut responses = HashMap::new();
            responses.insert(restore, vec![vec![0x2f, 0x03, 0x23, feature::DAYTIME_HR, 0]]);
            Self {
                tx, writes: Mutex::new(Vec::new()), responses,
                hang_live_start: false, fail_live_start: false, closed_notifications: false,
            }
        }
    }

    #[async_trait]
    impl Transport for Wire {
        async fn write(&self, data: &[u8]) -> oura_link::Result<()> {
            self.writes.lock().unwrap().push(data.to_vec());
            if self.hang_live_start && data == oura_protocol::protocol::req_set_feature_mode(feature::DAYTIME_HR, feature_mode::CONNECTED_LIVE) {
                pending::<()>().await;
            }
            if self.fail_live_start && data == oura_protocol::protocol::req_set_feature_mode(feature::DAYTIME_HR, feature_mode::CONNECTED_LIVE) {
                return Err(oura_link::Error::Ble("scripted link loss".into()));
            }
            if let Some(frames) = self.responses.get(data) {
                for frame in frames { let _ = self.tx.send(frame.clone()); }
            }
            Ok(())
        }

        fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
            if self.closed_notifications {
                let (sender, receiver) = broadcast::channel(1);
                drop(sender);
                return receiver;
            }
            self.tx.subscribe()
        }
    }

    fn short() -> OperationTimeouts {
        OperationTimeouts {
            operation: Duration::from_millis(5),
            teardown: Duration::from_millis(5),
            history: Duration::from_millis(20),
            ..OperationTimeouts::default()
        }
    }

    fn restored(client: &OuraClient<Wire>) -> bool {
        let restore = oura_protocol::protocol::req_set_feature_mode(feature::DAYTIME_HR, feature_mode::AUTOMATIC);
        client.transport().writes.lock().unwrap().iter().any(|write| write == &restore)
    }

    #[tokio::test(start_paused = true)]
    async fn real_client_timeout_tears_down_without_bluetooth() {
        let mut wire = Wire::new();
        wire.hang_live_start = true;
        let client = Arc::new(OuraClient::new(wire).with_quiet(Duration::from_millis(1)));
        let active = Arc::new(AtomicBool::new(false));
        let mut cleanup = None;
        let result = live_hr(client.clone(), &short(), active.clone(), &mut cleanup, Duration::from_secs(1)).await;
        assert_eq!(result, Err(RingError::Timeout { operation: "live_hr" }));
        assert!(restored(&client));
        assert!(!active.load(Ordering::Acquire));
        assert!(cleanup.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn real_client_cancelled_future_schedules_awaitable_teardown() {
        let client = Arc::new(OuraClient::new(Wire::new()).with_quiet(Duration::from_millis(1)));
        let active = Arc::new(AtomicBool::new(false));
        let mut cleanup = None;
        let timeouts = short();
        let mut future = Box::pin(live_hr(client.clone(), &timeouts, active.clone(), &mut cleanup, Duration::from_secs(30)));
        tokio::select! {
            biased;
            result = &mut future => panic!("capture ended unexpectedly: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        assert!(active.load(Ordering::Acquire));
        drop(future);
        cleanup.take().unwrap().await.unwrap().unwrap();
        assert!(restored(&client));
        assert!(!active.load(Ordering::Acquire));
    }

    #[tokio::test(start_paused = true)]
    async fn real_client_success_still_restores_and_hard_caps_duration() {
        let client = Arc::new(OuraClient::new(Wire::new()).with_quiet(Duration::from_millis(1)));
        let active = Arc::new(AtomicBool::new(false));
        let mut cleanup = None;
        let start = tokio::time::Instant::now();
        let mut timeouts = short();
        timeouts.live_hr_max = Duration::from_secs(3_600);
        live_hr(client.clone(), &timeouts, active.clone(), &mut cleanup, Duration::from_secs(3_600)).await.unwrap();
        assert!(start.elapsed() <= MAX_LIVE_HR_DURATION + timeouts.operation + timeouts.teardown);
        assert!(restored(&client));
        assert!(!active.load(Ordering::Acquire));
    }

    #[tokio::test(start_paused = true)]
    async fn real_client_failure_and_disconnect_still_attempt_teardown() {
        for closed_notifications in [false, true] {
            let mut wire = Wire::new();
            wire.fail_live_start = !closed_notifications;
            wire.closed_notifications = closed_notifications;
            let client = Arc::new(OuraClient::new(wire).with_quiet(Duration::from_millis(1)));
            let active = Arc::new(AtomicBool::new(false));
            let mut cleanup = None;
            let result = live_hr(client.clone(), &short(), active.clone(), &mut cleanup, Duration::from_secs(1)).await;
            assert_eq!(result, Err(RingError::Disconnected));
            assert!(restored(&client));
            assert!(!active.load(Ordering::Acquire));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn real_history_timeout_preserves_only_completed_batches() {
        let mut wire = Wire::new();
        let protocol = oura_protocol::protocol::req_ext_get_event(0, 4096, 0);
        wire.responses.insert(oura_protocol::protocol::req_data_flush(), vec![vec![0x29, 1, 0]]);
        wire.responses.insert(protocol, vec![vec![0x43, 8, 1, 0, 0, 0, b't', b'e', b's', b't', 0x2f, 10, 0x42, 1, 0, 0, 0, 100, 0, 0, 0, 0]]);
        let client = OuraClient::new(wire).with_quiet(Duration::from_millis(1));
        let batch = drain_history(&client, 0, Duration::from_millis(2)).await.unwrap();
        assert!(!batch.complete);
        assert_eq!(batch.next_cursor, 2);
        assert_eq!(batch.events, vec![HistoryEvent { tag: 0x43, ring_timestamp: 1, body: b"test".to_vec() }]);
    }

    #[tokio::test(start_paused = true)]
    async fn real_history_no_progress_returns_typed_timeout() {
        let client = OuraClient::new(Wire::new()).with_quiet(Duration::from_secs(1));
        assert_eq!(drain_history(&client, 0, Duration::from_millis(1)).await, Err(RingError::Timeout { operation: "drain_history" }));
        assert_eq!(drain_history(&client, u64::MAX, Duration::from_millis(1)).await, Err(RingError::Failed));
    }
}
