//! Single-owner ring operations. Admission rejects overlap instead of accumulating work.
//!
//! `spawn` is the service entry point. Its handle can cancel a borrowed transport
//! future without acquiring the transport, including an outstanding live-HR read.
use std::{future::Future, sync::{Arc, Mutex}, time::Duration};

use oura_ble::{RingError, RingTransport};
use oura_protocol::protocol::feature_mode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{sync::{mpsc, oneshot, watch, Notify}, task::JoinHandle};

use crate::{clock::Clock, config::{Config, private_new_file, reject_symlinks, sync_directory}, delivery::{HttpUploadSink, UploadSink}, error::CollectorError, scheduler::Scheduler, storage::{RingStore, SnapshotQueue}};

pub const MAX_LIVE_HR_SECS: u64 = 60;
const OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
// The real boundary has a 120-second drain deadline and returns checkpointed
// progress when it expires. Do not cancel it before that partial batch returns.
const HISTORY_TIMEOUT: Duration = Duration::from_secs(125);
// Capture itself remains capped at max_secs. Allow bounded SDK mode setup and
// restoration to finish so a successful full-length capture is not discarded.
const LIVE_SETUP_TEARDOWN_GRACE: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalCommand {
    SyncNow,
    RefreshDeviceInfo,
    RefreshBattery,
    CollectorStatus,
    StartLiveHr { max_secs: u64 },
    StopLiveHr,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandOutcome {
    pub detail: Value,
}

/// `None` means not observed, not false. Control connectivity never sets ring fields.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollectorStatus {
    pub pi_reachable: Option<bool>,
    pub adapter_present: Option<bool>,
    pub ring_recently_seen: Option<bool>,
    pub ring_connected: Option<bool>,
    pub operation_running: Option<bool>,
    pub ring_unreachable: Option<bool>,
    pub pending_snapshot: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelReason {
    Requested,
    StopLiveHr,
    ControlDisconnected,
    Shutdown,
}

struct ActiveOperation {
    live_hr: bool,
    cancellation: watch::Sender<Option<CancelReason>>,
}

struct RuntimeState {
    status: CollectorStatus,
    active: Option<ActiveOperation>,
    shutting_down: bool,
}

struct Shared {
    state: Mutex<RuntimeState>,
    idle: Notify,
    shutdown: Notify,
}

impl Shared {
    fn status(&self) -> CollectorStatus {
        self.state.lock().unwrap().status.clone()
    }

    fn reserve(self: &Arc<Self>, live_hr: bool) -> Result<(OperationGuard, watch::Receiver<Option<CancelReason>>), CollectorError> {
        let mut state = self.state.lock().unwrap();
        if state.shutting_down {
            return Err(CollectorError::Cancelled);
        }
        if state.active.is_some() {
            return Err(CollectorError::Busy);
        }
        let (cancellation, receiver) = watch::channel(None);
        state.active = Some(ActiveOperation { live_hr, cancellation });
        state.status.operation_running = Some(true);
        Ok((OperationGuard(self.clone()), receiver))
    }

    fn cancel(&self, reason: CancelReason, live_only: bool) -> bool {
        let state = self.state.lock().unwrap();
        if let Some(active) = &state.active {
            if !live_only || active.live_hr {
                active.cancellation.send_replace(Some(reason));
                return true;
            }
        }
        false
    }

    async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            if self.state.lock().unwrap().active.is_none() {
                return;
            }
            notified.await;
        }
    }
}

struct OperationGuard(Arc<Shared>);

impl Drop for OperationGuard {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.active = None;
        state.status.operation_running = Some(false);
        drop(state);
        self.0.idle.notify_waiters();
    }
}

struct Request {
    command: LocalCommand,
    guard: OperationGuard,
    cancellation: watch::Receiver<Option<CancelReason>>,
    result: oneshot::Sender<Result<CommandOutcome, CollectorError>>,
    expiry: Option<(i64, Arc<dyn Clock>)>,
}

/// An admitted command. Dropping this ticket does not cancel physical execution.
pub struct CommandTicket {
    result: oneshot::Receiver<Result<CommandOutcome, CollectorError>>,
}

impl CommandTicket {
    pub async fn wait(self) -> Result<CommandOutcome, CollectorError> {
        self.result.await.map_err(|_| CollectorError::Cancelled)?
    }
}

#[derive(Clone)]
pub struct CoordinatorHandle {
    requests: mpsc::Sender<Request>,
    shared: Arc<Shared>,
    terminated: watch::Receiver<bool>,
}

impl CoordinatorHandle {
    /// Admission is synchronous, so a control receipt can precede the final result.
    pub fn submit(&self, command: LocalCommand) -> Result<CommandTicket, CollectorError> {
        self.submit_inner(command, None)
    }

    pub fn submit_until(&self, command: LocalCommand, expires_unix: i64, clock: Arc<dyn Clock>) -> Result<CommandTicket, CollectorError> {
        if clock.now_unix() >= expires_unix {
            return Err(CollectorError::Ring(RingError::Expired));
        }
        self.submit_inner(command, Some((expires_unix, clock)))
    }

    fn submit_inner(&self, command: LocalCommand, expiry: Option<(i64, Arc<dyn Clock>)>) -> Result<CommandTicket, CollectorError> {
        let (result, receiver) = oneshot::channel();
        match command {
            LocalCommand::CollectorStatus => {
                let _ = result.send(Ok(CommandOutcome { detail: json!(self.status()) }));
            }
            LocalCommand::StopLiveHr => {
                let stopped = self.shared.cancel(CancelReason::StopLiveHr, true);
                let shared = self.shared.clone();
                tokio::spawn(async move {
                    if stopped {
                        shared.wait_idle().await;
                    }
                    let _ = result.send(Ok(CommandOutcome { detail: json!({"stopped": stopped}) }));
                });
            }
            command => {
                if matches!(command, LocalCommand::StartLiveHr { max_secs: 0 }) {
                    return Err(CollectorError::Protocol("live HR duration must be positive".into()));
                }
                let (guard, cancellation) = self.shared.reserve(matches!(command, LocalCommand::StartLiveHr { .. }))?;
                let request = Request { command, guard, cancellation, result, expiry };
                self.requests.try_send(request).map_err(|error| match error {
                    mpsc::error::TrySendError::Full(_) => CollectorError::Busy,
                    mpsc::error::TrySendError::Closed(_) => CollectorError::Cancelled,
                })?;
            }
        }
        Ok(CommandTicket { result: receiver })
    }

    pub async fn execute(&self, command: LocalCommand) -> Result<CommandOutcome, CollectorError> {
        self.submit(command)?.wait().await
    }

    pub fn cancel(&self) -> bool {
        self.shared.cancel(CancelReason::Requested, false)
    }

    pub fn status(&self) -> CollectorStatus {
        self.shared.status()
    }

    pub fn set_control_reachable(&self, reachable: bool) {
        self.shared.state.lock().unwrap().status.pi_reachable = Some(reachable);
        if !reachable {
            self.shared.cancel(CancelReason::ControlDisconnected, true);
        }
    }

    /// Adapter observations come from the adapter layer, never from Pi reachability.
    pub fn set_adapter_present(&self, present: Option<bool>) {
        self.shared.state.lock().unwrap().status.adapter_present = present;
    }

    pub async fn shutdown(&self) -> Result<(), CollectorError> {
        self.shared.state.lock().unwrap().shutting_down = true;
        self.shared.cancel(CancelReason::Shutdown, false);
        self.shared.shutdown.notify_one();
        let mut terminated = self.terminated.clone();
        while !*terminated.borrow_and_update() {
            if terminated.changed().await.is_err() {
                return Err(CollectorError::Cancelled);
            }
        }
        Ok(())
    }
}

pub struct Coordinator<T: RingTransport> {
    transport: T,
    store: RingStore,
    queue: SnapshotQueue,
    scheduler: Scheduler,
    config: Config,
    clock: Arc<dyn Clock>,
    shared: Arc<Shared>,
    transport_used: bool,
    _lease: std::fs::File,
}

impl<T: RingTransport> Coordinator<T> {
    pub fn new(config: Config, transport: T, clock: Arc<dyn Clock>) -> Result<Self, CollectorError> {
        config.prepare_state()?;
        let lease_path = config.state_dir.join(".collector.lock");
        reject_symlinks(&lease_path)?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)] {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lease = options.open(&lease_path)?;
        fs2::FileExt::try_lock_exclusive(&lease).map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
                CollectorError::Busy
            } else {
                CollectorError::Io(error)
            }
        })?;
        let store = RingStore::open(&config.database_path)?;
        let mut queue = SnapshotQueue::open(&config)?;
        let mut scheduler = Scheduler::open(&config)?;
        let cursor = match store.active_serial()? {
            Some(serial) => store.cursor_for(&serial)?,
            None => 0,
        };
        // A crash after committing history but before creating its snapshot must
        // not strand that durable progress until another successful ring contact.
        let marker = config.state_dir.join("pending-history-snapshot");
        reject_symlinks(&marker)?;
        let mut pending_snapshot = cursor > scheduler.state.cursor || marker.exists();
        if pending_snapshot {
            if !marker.exists() {
                private_new_file(&marker)?.sync_all()?;
                sync_directory(&config.state_dir)?;
            }
            match queue.create_snapshot(&store, clock.now_unix()) {
                Ok(_) => {
                    scheduler.checkpoint_cursor(cursor)?;
                    std::fs::remove_file(&marker)?;
                    sync_directory(&config.state_dir)?;
                    pending_snapshot = false;
                }
                Err(CollectorError::OutboxFull | CollectorError::LowDisk) => {},
                Err(error) => return Err(error),
            }
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(RuntimeState {
                status: CollectorStatus { operation_running: Some(false), pending_snapshot, ..CollectorStatus::default() },
                active: None,
                shutting_down: false,
            }),
            idle: Notify::new(),
            shutdown: Notify::new(),
        });
        Ok(Self { transport, store, queue, scheduler, config, clock, shared, transport_used: false, _lease: lease })
    }

    pub fn status(&self) -> CollectorStatus { self.shared.status() }
    pub fn queue(&self) -> &SnapshotQueue { &self.queue }
    pub fn queue_mut(&mut self) -> &mut SnapshotQueue { &mut self.queue }
    pub fn store(&self) -> &RingStore { &self.store }
    pub fn scheduler(&self) -> &Scheduler { &self.scheduler }
    pub fn config(&self) -> &Config { &self.config }

    pub async fn execute(&mut self, command: LocalCommand) -> Result<CommandOutcome, CollectorError> {
        match command {
            LocalCommand::CollectorStatus => return Ok(CommandOutcome { detail: json!(self.status()) }),
            LocalCommand::StopLiveHr => return Ok(CommandOutcome { detail: json!({"stopped": self.shared.cancel(CancelReason::StopLiveHr, true)}) }),
            LocalCommand::StartLiveHr { max_secs: 0 } => return Err(CollectorError::Protocol("live HR duration must be positive".into())),
            _ => {}
        }
        let (guard, cancellation) = self.shared.reserve(matches!(command, LocalCommand::StartLiveHr { .. }))?;
        self.execute_reserved(command, guard, cancellation).await
    }

    pub async fn tick(&mut self) -> Result<Option<CommandOutcome>, CollectorError> {
        if !self.scheduler.due(self.clock.now_unix()) {
            return Ok(None);
        }
        match self.execute(LocalCommand::SyncNow).await {
            Ok(result) => Ok(Some(result)),
            Err(CollectorError::Busy) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn recover_snapshot(&mut self) -> Result<(), CollectorError> {
        let marker = self.config.state_dir.join("pending-history-snapshot");
        reject_symlinks(&marker)?;
        if !marker.exists() { return Ok(()); }
        self.queue.create_snapshot(&self.store, self.clock.now_unix())?;
        let cursor = match self.store.active_serial()? {
            Some(serial) => self.store.cursor_for(&serial)?,
            None => 0,
        };
        self.scheduler.checkpoint_cursor(cursor)?;
        std::fs::remove_file(&marker)?;
        sync_directory(&self.config.state_dir)?;
        self.shared.state.lock().unwrap().status.pending_snapshot = false;
        Ok(())
    }

    async fn execute_reserved(&mut self, command: LocalCommand, _guard: OperationGuard, mut cancellation: watch::Receiver<Option<CancelReason>>) -> Result<CommandOutcome, CollectorError> {
        let prepared = if matches!(command, LocalCommand::SyncNow) { self.recover_snapshot() } else { Ok(()) };
        let connected = match prepared {
            Ok(()) => {
                self.transport_used = true;
                ring_call(&mut cancellation, "connect", CONNECT_TIMEOUT, self.transport.connect()).await
            }
            Err(error) => Err(error),
        };
        if connected.is_ok() {
            let mut state = self.shared.state.lock().unwrap();
            state.status.adapter_present = Some(true);
            state.status.ring_recently_seen = Some(true);
            state.status.ring_connected = Some(true);
            state.status.ring_unreachable = Some(false);
        }
        let mut result = match connected {
            Err(error) => {
                if matches!(command, LocalCommand::SyncNow) {
                    self.scheduler.failure(self.clock.now_unix()).and(Err(error))
                } else {
                    Err(error)
                }
            }
            Ok(()) => match command {
                LocalCommand::SyncNow => self.sync(&mut cancellation).await,
                LocalCommand::RefreshDeviceInfo => self.refresh_device_info(&mut cancellation).await,
                LocalCommand::RefreshBattery => ring_call(&mut cancellation, "battery", OPERATION_TIMEOUT, self.transport.battery()).await.map(|battery| CommandOutcome { detail: json!({"battery_percent": battery}) }),
                LocalCommand::StartLiveHr { max_secs } => self.live_hr(max_secs, &mut cancellation).await,
                LocalCommand::CollectorStatus | LocalCommand::StopLiveHr => unreachable!("non-ring commands do not reserve transport"),
            },
        };
        if let Err(error) = &result {
            self.observe_error(error);
        }
        // Do not let cancellation or a broken connection suppress teardown.
        let disconnected = cleanup_call("disconnect", self.transport.disconnect()).await;
        self.shared.state.lock().unwrap().status.ring_connected = if disconnected.is_ok() { Some(false) } else { None };
        if result.is_ok() {
            if let Err(error) = disconnected { result = Err(error); }
        }
        result
    }

    async fn refresh_device_info(&mut self, cancellation: &mut watch::Receiver<Option<CancelReason>>) -> Result<CommandOutcome, CollectorError> {
        let device = ring_call(cancellation, "device_info", OPERATION_TIMEOUT, self.transport.device_info()).await?;
        self.store.record_device(&device, self.clock.now_unix())?;
        Ok(CommandOutcome { detail: json!({"device": device}) })
    }

    async fn sync(&mut self, cancellation: &mut watch::Receiver<Option<CancelReason>>) -> Result<CommandOutcome, CollectorError> {
        let now = self.clock.now_unix();
        let progress = self.sync_batch(cancellation, now).await;
        match &progress {
            Ok(_) => self.scheduler.success(now, self.scheduler.state.cursor)?,
            Err(_) => self.scheduler.failure(now)?,
        }
        progress
    }

    async fn sync_batch(&mut self, cancellation: &mut watch::Receiver<Option<CancelReason>>, now: i64) -> Result<CommandOutcome, CollectorError> {
        let hardware = ring_call(cancellation, "hardware_id", OPERATION_TIMEOUT, self.transport.hardware_id()).await?;
        let mut device = ring_call(cancellation, "device_info", OPERATION_TIMEOUT, self.transport.device_info()).await?;
        device.hardware = hardware;
        self.store.record_device(&device, now)?;
        let serial = device.serial;
        let cursor = self.store.cursor_for(&serial)?;
        let batch = ring_call(cancellation, "drain_history", HISTORY_TIMEOUT, self.transport.drain_history(cursor)).await?;
        if batch.next_cursor < cursor {
            return Err(CollectorError::Protocol("ring history cursor regressed".into()));
        }
        let marker = self.config.state_dir.join("pending-history-snapshot");
        reject_symlinks(&marker)?;
        if !marker.exists() {
            private_new_file(&marker)?.sync_all()?;
            sync_directory(&self.config.state_dir)?;
        }
        self.shared.state.lock().unwrap().status.pending_snapshot = true;
        let inserted = self.store.write_batch_at(&serial, &batch, now)?;
        let metadata = self.queue.create_snapshot(&self.store, now)?;
        std::fs::remove_file(&marker)?;
        sync_directory(&self.config.state_dir)?;
        self.shared.state.lock().unwrap().status.pending_snapshot = false;
        self.scheduler.state.cursor = self.store.cursor_for(&serial)?;
        if !batch.complete {
            return Err(CollectorError::Ring(RingError::PartialHistory));
        }
        Ok(CommandOutcome { detail: json!({"inserted_events": inserted, "cursor": batch.next_cursor, "complete": true, "snapshot_generation": metadata.generation}) })
    }

    async fn live_hr(&mut self, max_secs: u64, cancellation: &mut watch::Receiver<Option<CancelReason>>) -> Result<CommandOutcome, CollectorError> {
        let duration = Duration::from_secs(max_secs.min(MAX_LIVE_HR_SECS));
        let watchdog = duration.saturating_add(LIVE_SETUP_TEARDOWN_GRACE);
        let (original_mode, result) = match ring_call(cancellation, "feature_mode", OPERATION_TIMEOUT, self.transport.feature_mode()).await {
            Ok(mode) => {
                // A crash can leave CONNECTED_LIVE on the ring without a local
                // session. Reconcile that actual state before another start;
                // restoring CONNECTED_LIVE would re-enable unbounded capture.
                let restore_mode = if mode == u32::from(feature_mode::CONNECTED_LIVE) {
                    u32::from(feature_mode::AUTOMATIC)
                } else { mode };
                let reconciled = if restore_mode != mode {
                    ring_call(cancellation, "reconcile_live_mode", OPERATION_TIMEOUT, self.transport.set_feature_mode(restore_mode)).await
                } else { Ok(()) };
                let capture = match reconciled {
                    Ok(()) => ring_call(cancellation, "live_hr", watchdog, self.transport.live_hr(duration)).await
                        .map(|samples| CommandOutcome { detail: json!({"duration_secs": duration.as_secs(), "samples": samples}) }),
                    Err(error) => Err(error),
                };
                (Some(restore_mode), capture)
            }
            Err(error) => (None, Err(error)),
        };
        let stopped = cleanup_call("stop_live_hr", self.transport.stop_live_hr()).await;
        let restored = match original_mode {
            Some(mode) => cleanup_call("restore_feature_mode", self.transport.set_feature_mode(mode)).await,
            None => Ok(()),
        };
        match result {
            Err(error) => Err(error),
            Ok(outcome) => { stopped?; restored?; Ok(outcome) }
        }
    }

    fn observe_error(&self, error: &CollectorError) {
        let mut state = self.shared.state.lock().unwrap();
        if matches!(error, CollectorError::Ring(RingError::Absent | RingError::OutOfRange)) {
            state.status.ring_recently_seen = Some(false);
            state.status.ring_unreachable = Some(true);
        } else if matches!(error, CollectorError::Ring(RingError::Disconnected)) {
            state.status.ring_connected = Some(false);
            state.status.ring_unreachable = Some(true);
        }
    }

    pub async fn shutdown(&mut self) -> Result<(), CollectorError> {
        self.shared.state.lock().unwrap().shutting_down = true;
        self.shared.cancel(CancelReason::Shutdown, false);
        if !self.transport_used || self.status().ring_connected == Some(false) { return Ok(()); }
        let stopped = cleanup_call("stop_live_hr", self.transport.stop_live_hr()).await;
        let disconnected = cleanup_call("disconnect", self.transport.disconnect()).await;
        self.shared.state.lock().unwrap().status.ring_connected = if disconnected.is_ok() { Some(false) } else { None };
        stopped?;
        disconnected
    }
}

impl<T: RingTransport + 'static> Coordinator<T> {
    /// Runs collection on its own task. WebSocket reconnects cannot stall `tick`.
    pub fn spawn(self) -> (CoordinatorHandle, JoinHandle<Result<(), CollectorError>>) {
        self.spawn_inner::<HttpUploadSink>(None)
    }

    /// Delivery shares queue ownership; one upload delays this actor at most 30 seconds.
    pub fn spawn_with_delivery<S: UploadSink + 'static>(self, sink: S) -> (CoordinatorHandle, JoinHandle<Result<(), CollectorError>>) {
        self.spawn_inner(Some(sink))
    }

    fn spawn_inner<S: UploadSink + 'static>(mut self, sink: Option<S>) -> (CoordinatorHandle, JoinHandle<Result<(), CollectorError>>) {
        let mut sink = sink.map(BoundedSink);
        let (requests, mut receiver) = mpsc::channel::<Request>(1);
        let (terminated, closed) = watch::channel(false);
        let shared = self.shared.clone();
        let handle = CoordinatorHandle { requests, shared: shared.clone(), terminated: closed };
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                if shared.state.lock().unwrap().shutting_down { break; }
                tokio::select! {
                    biased;
                    _ = shared.shutdown.notified() => break,
                    request = receiver.recv() => match request {
                        Some(request) => {
                            let expired = request.expiry.as_ref().is_some_and(|(deadline, clock)| clock.now_unix() >= *deadline);
                            let result = if expired {
                                drop(request.guard);
                                Err(CollectorError::Ring(RingError::Expired))
                            } else {
                                self.execute_reserved(request.command, request.guard, request.cancellation).await
                            };
                            let _ = request.result.send(result);
                        }
                        None => break,
                    },
                    _ = interval.tick() => {
                        // Ring and storage failures are persisted by sync; the next
                        // due time, not a WebSocket event, drives the next attempt.
                        let _ = self.tick().await;
                        if let Some(sink) = sink.as_mut() {
                            let _ = self.queue.deliver_one_due(sink, self.clock.now_unix()).await;
                        }
                        if self.shared.state.lock().unwrap().status.pending_snapshot {
                            let _ = self.recover_snapshot();
                        }
                    }
                }
            }
            receiver.close();
            while let Ok(request) = receiver.try_recv() {
                let _ = request.result.send(Err(CollectorError::Cancelled));
            }
            let result = self.shutdown().await;
            terminated.send_replace(true);
            result
        });
        (handle, task)
    }
}

struct BoundedSink<S>(S);

#[async_trait::async_trait]
impl<S: UploadSink> UploadSink for BoundedSink<S> {
    async fn upload(&mut self, metadata: &oura_protocol_v1::UploadMetadata, path: &std::path::Path) -> Result<oura_protocol_v1::UploadAck, CollectorError> {
        tokio::time::timeout(Duration::from_secs(30), self.0.upload(metadata, path)).await
            .map_err(|_| CollectorError::Delivery("upload timed out; snapshot retained".into()))?
    }
}

async fn cancelled(receiver: &mut watch::Receiver<Option<CancelReason>>) {
    loop {
        if receiver.borrow_and_update().is_some() { return; }
        if receiver.changed().await.is_err() { return; }
    }
}

async fn ring_call<R>(cancellation: &mut watch::Receiver<Option<CancelReason>>, operation: &'static str, timeout: Duration, future: impl Future<Output = Result<R, RingError>>) -> Result<R, CollectorError> {
    tokio::select! {
        biased;
        _ = cancelled(cancellation) => Err(CollectorError::Cancelled),
        result = tokio::time::timeout(timeout, future) => result.map_err(|_| RingError::Timeout { operation })?.map_err(CollectorError::from),
    }
}

async fn cleanup_call<R>(operation: &'static str, future: impl Future<Output = Result<R, RingError>>) -> Result<R, CollectorError> {
    tokio::time::timeout(CLEANUP_TIMEOUT, future).await.map_err(|_| RingError::Timeout { operation })?.map_err(CollectorError::from)
}
