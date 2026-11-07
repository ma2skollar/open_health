use std::collections::{HashMap, VecDeque};
use std::future::pending;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::watch;

use crate::{bounded, DeviceInfo, HistoryBatch, HistoryEvent, HrSample, Operation, OperationTimeouts, RingError, RingTransport};

/// One outcome consumed by one call of the named operation. `Slow` never
/// completes by itself, so it deterministically exercises the outer timeout.
#[derive(Clone, Debug)]
pub enum ScriptOutcome {
    Present,
    Absent,
    OutOfRange,
    Busy,
    Failed,
    Cancelled,
    Expired,
    Disconnected,
    Slow,
    PartialHistory(HistoryBatch),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown ring operation: {0}")]
pub struct UnknownOperation(pub String);

#[derive(Clone, Copy, PartialEq, Eq)]
enum LiveSignal {
    Running,
    Stopped,
    Disconnected,
}

struct State {
    scripts: HashMap<Operation, VecDeque<ScriptOutcome>>,
    calls: Vec<String>,
    connected: bool,
    live_active: bool,
    mode: u32,
    history: Option<Vec<HistoryEvent>>,
    device_info: DeviceInfo,
    battery: u8,
    samples: Vec<HrSample>,
}

struct Shared {
    state: Mutex<State>,
    live_signal: watch::Sender<LiveSignal>,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Cloneable controls that do not borrow the ring or own a Bluetooth client.
#[derive(Clone)]
pub struct FakeRingHandle {
    shared: Arc<Shared>,
}

impl FakeRingHandle {
    /// Queue an outcome for a trait method, for example `"drain_history"`.
    /// Unscripted calls succeed with deterministic demo data after connection.
    pub fn push(&self, operation: &str, outcome: ScriptOutcome) -> Result<(), UnknownOperation> {
        let operation = Operation::parse(operation)
            .ok_or_else(|| UnknownOperation(operation.to_owned()))?;
        self.push_operation(operation, outcome);
        Ok(())
    }

    pub fn push_operation(&self, operation: Operation, outcome: ScriptOutcome) {
        self.shared.state().scripts.entry(operation).or_default().push_back(outcome);
    }

    /// Ordered method names, including automatic `stop_live_hr` teardown calls.
    pub fn calls(&self) -> Vec<String> {
        self.shared.state().calls.clone()
    }

    pub fn live_active(&self) -> bool {
        self.shared.state().live_active
    }

    pub fn connected(&self) -> bool {
        self.shared.state().connected
    }

    pub fn set_history(&self, events: Vec<HistoryEvent>) {
        self.shared.state().history = Some(events);
    }

    pub fn set_device_info(&self, info: DeviceInfo) {
        self.shared.state().device_info = info;
    }

    pub fn set_battery(&self, percent: u8) {
        self.shared.state().battery = percent;
    }

    pub fn set_samples(&self, samples: Vec<HrSample>) {
        self.shared.state().samples = samples;
    }

    /// Interrupt a pending fake live session without borrowing its owner.
    pub fn stop_live_hr(&self) {
        self.shared.live_signal.send_replace(LiveSignal::Stopped);
    }

    /// Simulate physical link loss during a pending fake live session.
    pub fn disconnect(&self) {
        self.shared.state().connected = false;
        self.shared.live_signal.send_replace(LiveSignal::Disconnected);
    }
}

/// Always available, including with `default-features = false`.
///
/// `new` uses five-millisecond deadlines and never sleeps for successful calls.
/// Use `with_timeouts` with Tokio's paused clock for longer deterministic cases.
/// By default each drain returns two new demo events relative to the supplied
/// cursor. `set_history` switches to a finite raw-event fixture instead.
pub struct FakeRing {
    shared: Arc<Shared>,
    timeouts: OperationTimeouts,
}

impl Default for FakeRing {
    fn default() -> Self {
        Self::new().0
    }
}

impl FakeRing {
    pub fn new() -> (Self, FakeRingHandle) {
        let short = Duration::from_millis(5);
        Self::with_timeouts(OperationTimeouts {
            connect: short,
            operation: short,
            history: short,
            teardown: short,
            ..OperationTimeouts::default()
        })
    }

    pub fn with_timeouts(timeouts: OperationTimeouts) -> (Self, FakeRingHandle) {
        let (live_signal, _) = watch::channel(LiveSignal::Stopped);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                scripts: HashMap::new(),
                calls: Vec::new(),
                connected: false,
                live_active: false,
                mode: 1,
                history: None,
                device_info: DeviceInfo {
                    serial: "DEMO-RING-0001".into(),
                    firmware: "demo-1.0".into(),
                    hardware: "DEMO_05".into(),
                },
                battery: 82,
                samples: vec![HrSample { bpm: 62, unix: 1_700_000_000 }],
            }),
            live_signal,
        });
        (Self { shared: shared.clone(), timeouts }, FakeRingHandle { shared })
    }

    pub fn handle(&self) -> FakeRingHandle {
        FakeRingHandle { shared: self.shared.clone() }
    }

    fn begin(&self, operation: Operation) -> ScriptOutcome {
        let mut state = self.shared.state();
        state.calls.push(operation.as_str().into());
        state.scripts.get_mut(&operation).and_then(VecDeque::pop_front)
            .unwrap_or(ScriptOutcome::Present)
    }

    fn require_connected(&self) -> Result<(), RingError> {
        if self.shared.state().connected { Ok(()) } else { Err(RingError::Disconnected) }
    }

    async fn outcome(&self, operation: Operation, outcome: ScriptOutcome) -> Result<(), RingError> {
        let duration = self.timeouts.for_operation(operation);
        bounded(operation, duration, async {
            match outcome {
                ScriptOutcome::Present => Ok(()),
                ScriptOutcome::Absent => Err(RingError::Absent),
                ScriptOutcome::OutOfRange => Err(RingError::OutOfRange),
                ScriptOutcome::Busy => Err(RingError::Busy),
                ScriptOutcome::Failed => Err(RingError::Failed),
                ScriptOutcome::Cancelled => Err(RingError::Cancelled),
                ScriptOutcome::Expired => Err(RingError::Expired),
                ScriptOutcome::Disconnected => Err(RingError::Disconnected),
                ScriptOutcome::PartialHistory(_) => Err(RingError::PartialHistory),
                ScriptOutcome::Slow => pending().await,
            }
        }).await
    }
}

/// Synchronous fake cleanup also covers cancellation by dropping the future.
struct LiveGuard {
    shared: Arc<Shared>,
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        let mut state = self.shared.state();
        state.calls.push(Operation::StopLiveHr.as_str().into());
        state.live_active = false;
        state.mode = 1;
    }
}

#[async_trait]
impl RingTransport for FakeRing {
    async fn connect(&mut self) -> Result<(), RingError> {
        let outcome = self.begin(Operation::Connect);
        let result = self.outcome(Operation::Connect, outcome).await;
        self.shared.state().connected = result.is_ok();
        result
    }

    async fn disconnect(&mut self) -> Result<(), RingError> {
        let outcome = self.begin(Operation::Disconnect);
        let live_active = self.shared.state().live_active;
        let cleanup = if live_active { self.stop_live_hr().await } else { Ok(()) };
        let result = self.outcome(Operation::Disconnect, outcome).await;
        self.shared.state().connected = false;
        self.shared.live_signal.send_replace(LiveSignal::Disconnected);
        result.and(cleanup)
    }

    async fn hardware_id(&mut self) -> Result<String, RingError> {
        let outcome = self.begin(Operation::HardwareId);
        self.outcome(Operation::HardwareId, outcome).await?;
        self.require_connected()?;
        Ok(self.shared.state().device_info.hardware.clone())
    }

    async fn device_info(&mut self) -> Result<DeviceInfo, RingError> {
        let outcome = self.begin(Operation::DeviceInfo);
        self.outcome(Operation::DeviceInfo, outcome).await?;
        self.require_connected()?;
        Ok(self.shared.state().device_info.clone())
    }

    async fn battery(&mut self) -> Result<u8, RingError> {
        let outcome = self.begin(Operation::Battery);
        self.outcome(Operation::Battery, outcome).await?;
        self.require_connected()?;
        Ok(self.shared.state().battery)
    }

    async fn drain_history(&mut self, cursor: u64) -> Result<HistoryBatch, RingError> {
        let outcome = self.begin(Operation::DrainHistory);
        if let ScriptOutcome::PartialHistory(mut batch) = outcome {
            self.require_connected()?;
            batch.complete = false;
            return Ok(batch);
        }
        self.outcome(Operation::DrainHistory, outcome).await?;
        self.require_connected()?;
        let state = self.shared.state();
        let events = match &state.history {
            Some(events) => events.iter().filter(|event| event.ring_timestamp >= cursor).cloned().collect(),
            None => vec![
                HistoryEvent { tag: 0x43, ring_timestamp: cursor.saturating_add(1), body: b"demo-event-1".to_vec() },
                HistoryEvent { tag: 0x43, ring_timestamp: cursor.saturating_add(2), body: b"demo-event-2".to_vec() },
            ],
        };
        let next_cursor = events.iter().map(|event| event.ring_timestamp.saturating_add(1)).max().unwrap_or(cursor).max(cursor);
        Ok(HistoryBatch { events, next_cursor, complete: true })
    }

    async fn feature_mode(&mut self) -> Result<u32, RingError> {
        let outcome = self.begin(Operation::FeatureMode);
        self.outcome(Operation::FeatureMode, outcome).await?;
        self.require_connected()?;
        Ok(self.shared.state().mode)
    }

    async fn set_feature_mode(&mut self, mode: u32) -> Result<(), RingError> {
        let outcome = self.begin(Operation::SetFeatureMode);
        self.outcome(Operation::SetFeatureMode, outcome).await?;
        self.require_connected()?;
        if mode > 3 { return Err(RingError::Failed); }
        self.shared.state().mode = mode;
        Ok(())
    }

    async fn live_hr(&mut self, duration: Duration) -> Result<Vec<HrSample>, RingError> {
        let outcome = self.begin(Operation::LiveHr);
        self.require_connected()?;
        if self.shared.state().live_active { return Err(RingError::Busy); }
        if duration.is_zero() { return Ok(Vec::new()); }
        self.shared.state().live_active = true;
        self.shared.state().mode = 3;
        self.shared.live_signal.send_replace(LiveSignal::Running);
        let _guard = LiveGuard { shared: self.shared.clone() };
        if matches!(outcome, ScriptOutcome::Slow) {
            let mut signal = self.shared.live_signal.subscribe();
            let duration = duration.min(self.timeouts.live_hr_max).min(self.timeouts.operation);
            bounded(Operation::LiveHr, duration, async {
                loop {
                    match *signal.borrow_and_update() {
                        LiveSignal::Stopped => return Err(RingError::Cancelled),
                        LiveSignal::Disconnected => return Err(RingError::Disconnected),
                        LiveSignal::Running => {}
                    }
                    signal.changed().await.map_err(|_| RingError::Disconnected)?;
                }
            }).await?;
        } else {
            self.outcome(Operation::LiveHr, outcome).await?;
        }
        self.require_connected()?;
        let samples = self.shared.state().samples.clone();
        Ok(samples)
    }

    async fn stop_live_hr(&mut self) -> Result<(), RingError> {
        let outcome = self.begin(Operation::StopLiveHr);
        self.shared.live_signal.send_replace(LiveSignal::Stopped);
        let result = self.outcome(Operation::StopLiveHr, outcome).await;
        let mut state = self.shared.state();
        state.live_active = false;
        state.mode = 1;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn scripts_are_per_operation_and_all_failures_are_typed() {
        let cases = [
            (ScriptOutcome::Absent, RingError::Absent),
            (ScriptOutcome::OutOfRange, RingError::OutOfRange),
            (ScriptOutcome::Busy, RingError::Busy),
            (ScriptOutcome::Failed, RingError::Failed),
            (ScriptOutcome::Cancelled, RingError::Cancelled),
            (ScriptOutcome::Expired, RingError::Expired),
            (ScriptOutcome::Disconnected, RingError::Disconnected),
        ];
        let (mut ring, handle) = FakeRing::new();
        handle.push("battery", ScriptOutcome::Failed).unwrap();
        for (outcome, expected) in cases {
            handle.push("connect", outcome).unwrap();
            assert_eq!(ring.connect().await, Err(expected));
        }
        handle.push("connect", ScriptOutcome::Present).unwrap();
        ring.connect().await.unwrap();
        assert_eq!(ring.battery().await, Err(RingError::Failed));
        assert_eq!(ring.battery().await.unwrap(), 82);
        assert_eq!(handle.calls().last().unwrap(), "battery");
        assert!(handle.push("factory_reset", ScriptOutcome::Present).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn every_operation_has_a_typed_timeout() {
        for operation in Operation::ALL {
            let (mut ring, handle) = FakeRing::new();
            ring.connect().await.unwrap();
            handle.push_operation(operation, ScriptOutcome::Slow);
            let error = match operation {
                Operation::Connect => ring.connect().await.unwrap_err(),
                Operation::Disconnect => ring.disconnect().await.unwrap_err(),
                Operation::HardwareId => ring.hardware_id().await.unwrap_err(),
                Operation::DeviceInfo => ring.device_info().await.unwrap_err(),
                Operation::Battery => ring.battery().await.unwrap_err(),
                Operation::DrainHistory => ring.drain_history(0).await.unwrap_err(),
                Operation::FeatureMode => ring.feature_mode().await.unwrap_err(),
                Operation::SetFeatureMode => ring.set_feature_mode(1).await.unwrap_err(),
                Operation::LiveHr => ring.live_hr(Duration::from_secs(30)).await.unwrap_err(),
                Operation::StopLiveHr => ring.stop_live_hr().await.unwrap_err(),
            };
            assert_eq!(error, RingError::Timeout { operation: operation.as_str() });
            assert!(!handle.live_active());
        }
    }

    #[tokio::test]
    async fn partial_history_keeps_raw_events_and_checkpoint() {
        let (mut ring, handle) = FakeRing::new();
        ring.connect().await.unwrap();
        let batch = HistoryBatch {
            events: vec![HistoryEvent { tag: 0x80, ring_timestamp: 12, body: vec![0, 255] }],
            next_cursor: 13,
            complete: false,
        };
        handle.push("drain_history", ScriptOutcome::PartialHistory(batch.clone())).unwrap();
        assert_eq!(ring.drain_history(10).await.unwrap(), batch);
        handle.set_history(batch.events.clone());
        assert_eq!(ring.drain_history(13).await.unwrap(), HistoryBatch { events: vec![], next_cursor: 13, complete: true });
    }

    #[tokio::test(start_paused = true)]
    async fn live_teardown_covers_success_errors_timeout_and_dropped_future() {
        let (mut ring, handle) = FakeRing::new();
        ring.connect().await.unwrap();
        assert!(!ring.live_hr(Duration::from_secs(1)).await.unwrap().is_empty());
        for outcome in [ScriptOutcome::Failed, ScriptOutcome::Cancelled, ScriptOutcome::Expired, ScriptOutcome::Slow] {
            handle.push("live_hr", outcome).unwrap();
            assert!(ring.live_hr(Duration::from_secs(1)).await.is_err());
            assert!(!handle.live_active());
        }
        handle.push("live_hr", ScriptOutcome::Slow).unwrap();
        let mut future = Box::pin(ring.live_hr(Duration::from_secs(30)));
        tokio::select! {
            biased;
            result = &mut future => panic!("pending script completed unexpectedly: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        assert!(handle.live_active());
        drop(future);
        assert!(!handle.live_active());
        assert_eq!(ring.feature_mode().await.unwrap(), 1);
        assert_eq!(handle.calls().iter().filter(|call| call.as_str() == "stop_live_hr").count(), 6);
    }

    #[tokio::test(start_paused = true)]
    async fn handle_stop_and_disconnect_interrupt_live_capture() {
        for disconnect in [false, true] {
            let (mut ring, handle) = FakeRing::new();
            ring.connect().await.unwrap();
            handle.push("live_hr", ScriptOutcome::Slow).unwrap();
            let control = async {
                tokio::task::yield_now().await;
                if disconnect { handle.disconnect(); } else { handle.stop_live_hr(); }
            };
            let (result, ()) = tokio::join!(ring.live_hr(Duration::from_secs(30)), control);
            assert_eq!(result, Err(if disconnect { RingError::Disconnected } else { RingError::Cancelled }));
            assert!(!handle.live_active());
            ring.stop_live_hr().await.unwrap();
            ring.disconnect().await.unwrap();
            assert!(!handle.connected());
        }
    }
}
