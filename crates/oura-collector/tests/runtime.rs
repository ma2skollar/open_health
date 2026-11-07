use std::{sync::{Arc, Mutex}, time::Duration};

use async_trait::async_trait;
use oura_ble::{DeviceInfo, HistoryBatch, HistoryEvent, HrSample, RingError, RingTransport};
use oura_collector::{clock::{Clock, FakeClock}, config::Config, control::ControlSession, coordinator::{Coordinator, LocalCommand, MAX_LIVE_HR_SECS}, error::CollectorError, storage::RingStore};
use oura_protocol_v1::{CommandName, ControlMessage, ReceiptState, ResultState, PROTOCOL_VERSION};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::Notify;

const NOW: i64 = 12 * 60 * 60;

#[derive(Clone, Copy, Default)]
enum ConnectMode { #[default] Ready, Absent }
#[derive(Clone, Copy, Default)]
enum HistoryMode { #[default] Complete, Partial, Pending }
#[derive(Clone, Copy, Default)]
enum LiveMode { #[default] Pending, Complete, CompleteAtDeadline, Disconnected }

struct Observations {
    calls: Vec<&'static str>,
    cursors: Vec<u64>,
    durations: Vec<Duration>,
    mode: u32,
    serial: String,
    connect: ConnectMode,
    history: HistoryMode,
    live: LiveMode,
}

struct Trace {
    state: Mutex<Observations>,
    changed: Notify,
}

impl Trace {
    fn new() -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(Observations { calls: vec![], cursors: vec![], durations: vec![], mode: 7, serial: "runtime-ring".into(), connect: ConnectMode::Ready, history: HistoryMode::Complete, live: LiveMode::Pending }), changed: Notify::new() })
    }
    fn call(&self, operation: &'static str) {
        self.state.lock().unwrap().calls.push(operation);
        self.changed.notify_waiters();
    }
    fn count(&self, operation: &str) -> usize {
        self.state.lock().unwrap().calls.iter().filter(|call| **call == operation).count()
    }
    async fn wait_for(&self, operation: &str) {
        loop {
            let notified = self.changed.notified();
            if self.count(operation) > 0 { return; }
            notified.await;
        }
    }
}

struct TestRing(Arc<Trace>);

#[async_trait]
impl RingTransport for TestRing {
    async fn connect(&mut self) -> Result<(), RingError> {
        self.0.call("connect");
        match self.0.state.lock().unwrap().connect { ConnectMode::Ready => Ok(()), ConnectMode::Absent => Err(RingError::Absent) }
    }
    async fn disconnect(&mut self) -> Result<(), RingError> { self.0.call("disconnect"); Ok(()) }
    async fn hardware_id(&mut self) -> Result<String, RingError> { self.0.call("hardware_id"); Ok("MODEL_05".into()) }
    async fn device_info(&mut self) -> Result<DeviceInfo, RingError> {
        self.0.call("device_info");
        Ok(DeviceInfo { serial: self.0.state.lock().unwrap().serial.clone(), firmware: "test".into(), hardware: "MODEL_05".into() })
    }
    async fn battery(&mut self) -> Result<u8, RingError> { self.0.call("battery"); Ok(80) }
    async fn drain_history(&mut self, cursor: u64) -> Result<HistoryBatch, RingError> {
        self.0.state.lock().unwrap().cursors.push(cursor);
        self.0.call("drain_history");
        let mode = self.0.state.lock().unwrap().history;
        if matches!(mode, HistoryMode::Pending) { return std::future::pending().await; }
        Ok(HistoryBatch { events: vec![event(cursor)], next_cursor: cursor + 1, complete: matches!(mode, HistoryMode::Complete) })
    }
    async fn feature_mode(&mut self) -> Result<u32, RingError> { self.0.call("feature_mode"); Ok(self.0.state.lock().unwrap().mode) }
    async fn set_feature_mode(&mut self, mode: u32) -> Result<(), RingError> { self.0.state.lock().unwrap().mode = mode; self.0.call("restore_feature_mode"); Ok(()) }
    async fn live_hr(&mut self, duration: Duration) -> Result<Vec<HrSample>, RingError> {
        {
            let mut state = self.0.state.lock().unwrap();
            state.durations.push(duration);
            state.mode = 999;
        }
        self.0.call("live_hr");
        let mode = self.0.state.lock().unwrap().live;
        match mode {
            LiveMode::Pending => std::future::pending().await,
            LiveMode::Complete => Ok(vec![HrSample { bpm: 62, unix: NOW }]),
            LiveMode::CompleteAtDeadline => {
                tokio::time::sleep(duration).await;
                Ok(vec![HrSample { bpm: 62, unix: NOW }])
            }
            LiveMode::Disconnected => Err(RingError::Disconnected),
        }
    }
    async fn stop_live_hr(&mut self) -> Result<(), RingError> { self.0.call("stop_live_hr"); Ok(()) }
}

fn event(cursor: u64) -> HistoryEvent { HistoryEvent { tag: 1, ring_timestamp: 1000 + cursor, body: vec![1, 2, 3] } }

fn fixture() -> (TempDir, Config, Arc<FakeClock>, Arc<Trace>, Coordinator<TestRing>) {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::demo(root.path());
    config.wake_start = "13:00".into();
    config.wake_end = "14:00".into();
    config.min_free_bytes = 0;
    let clock = Arc::new(FakeClock::new(NOW));
    let trace = Trace::new();
    let coordinator = Coordinator::new(config.clone(), TestRing(trace.clone()), clock.clone()).unwrap();
    (root, config, clock, trace, coordinator)
}

fn command(id: &str, name: CommandName, args: serde_json::Value, expires_unix: i64) -> ControlMessage {
    ControlMessage::Command { protocol_version: PROTOCOL_VERSION, id: id.into(), name, args, created_unix: NOW - 100, expires_unix }
}

fn assert_cleanup(trace: &Trace) {
    assert!(trace.count("stop_live_hr") >= 1);
    assert!(trace.count("restore_feature_mode") >= 1);
    assert!(trace.count("disconnect") >= 1);
    assert_eq!(trace.state.lock().unwrap().mode, 7);
}

#[tokio::test]
async fn sync_commits_then_snapshots_and_reopens_from_durable_cursor() {
    let (_root, config, clock, trace, mut coordinator) = fixture();
    let outcome = coordinator.execute(LocalCommand::SyncNow).await.unwrap();
    assert_eq!(outcome.detail["complete"], true);
    assert_eq!(coordinator.store().event_count().unwrap(), 1);
    assert_eq!(coordinator.store().cursor().unwrap(), 1);
    assert_eq!(coordinator.queue().pending().len(), 1);
    assert_eq!(trace.count("connect"), 1);
    assert_eq!(trace.count("disconnect"), 1);
    assert_eq!(coordinator.status().ring_connected, Some(false));
    drop(coordinator);
    let mut recovered = Coordinator::new(config, TestRing(trace.clone()), clock).unwrap();
    recovered.execute(LocalCommand::SyncNow).await.unwrap();
    assert_eq!(trace.state.lock().unwrap().cursors, vec![0, 1]);
    assert_eq!(recovered.store().event_count().unwrap(), 2);
}

#[tokio::test]
async fn partial_history_snapshots_events_but_replays_unchanged_cursor_after_restart() {
    let (_root, config, clock, trace, mut coordinator) = fixture();
    trace.state.lock().unwrap().history = HistoryMode::Partial;
    assert!(matches!(coordinator.execute(LocalCommand::SyncNow).await, Err(CollectorError::Ring(RingError::PartialHistory))));
    assert_eq!(coordinator.store().event_count().unwrap(), 1);
    assert_eq!(coordinator.store().cursor().unwrap(), 0);
    assert_eq!(coordinator.queue().pending().len(), 1);
    assert_eq!(coordinator.scheduler().state.consecutive_failures, 1);
    drop(coordinator);
    trace.state.lock().unwrap().history = HistoryMode::Complete;
    let mut recovered = Coordinator::new(config, TestRing(trace.clone()), clock).unwrap();
    recovered.execute(LocalCommand::SyncNow).await.unwrap();
    assert_eq!(trace.state.lock().unwrap().cursors, vec![0, 0]);
    assert_eq!(recovered.store().event_count().unwrap(), 1);
    assert_eq!(recovered.store().cursor().unwrap(), 1);
    assert_eq!(recovered.scheduler().state.consecutive_failures, 0);
}

#[tokio::test]
async fn crash_marker_recovers_partial_snapshot_without_contacting_ring() {
    let (_root, config, clock, trace, coordinator) = fixture();
    drop(coordinator);
    let store = RingStore::open(&config.database_path).unwrap();
    store.write_batch_at("runtime-ring", &HistoryBatch { events: vec![event(0)], next_cursor: 1, complete: false }, NOW).unwrap();
    drop(store);
    std::fs::write(config.state_dir.join("pending-history-snapshot"), []).unwrap();
    trace.state.lock().unwrap().connect = ConnectMode::Absent;
    let recovered = Coordinator::new(config.clone(), TestRing(trace.clone()), clock).unwrap();
    assert_eq!(recovered.store().event_count().unwrap(), 1);
    assert_eq!(recovered.store().cursor().unwrap(), 0);
    assert_eq!(recovered.queue().pending().len(), 1);
    assert!(!config.state_dir.join("pending-history-snapshot").exists());
    assert_eq!(trace.count("connect"), 0);
}

#[tokio::test]
async fn busy_rejects_second_command_and_external_cancel_releases_owner() {
    let (_root, _config, _clock, trace, coordinator) = fixture();
    let (handle, task) = coordinator.spawn();
    let live = handle.submit(LocalCommand::StartLiveHr { max_secs: 60 }).unwrap();
    trace.wait_for("live_hr").await;
    assert!(matches!(handle.submit(LocalCommand::RefreshBattery), Err(CollectorError::Busy)));
    assert_eq!(trace.count("battery"), 0);
    assert!(handle.cancel());
    assert!(matches!(live.wait().await, Err(CollectorError::Cancelled)));
    assert_cleanup(&trace);
    handle.execute(LocalCommand::RefreshBattery).await.unwrap();
    assert_eq!(trace.count("battery"), 1);
    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn stop_interrupts_awaited_live_read_and_enforces_local_cap() {
    let (_root, _config, _clock, trace, coordinator) = fixture();
    let (handle, task) = coordinator.spawn();
    let live = handle.submit(LocalCommand::StartLiveHr { max_secs: 10_000 }).unwrap();
    trace.wait_for("live_hr").await;
    let stopped = handle.execute(LocalCommand::StopLiveHr).await.unwrap();
    assert_eq!(stopped.detail["stopped"], true);
    assert!(matches!(live.wait().await, Err(CollectorError::Cancelled)));
    assert_eq!(trace.state.lock().unwrap().durations, vec![Duration::from_secs(MAX_LIVE_HR_SECS)]);
    assert_cleanup(&trace);
    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn live_timeout_tears_down_and_normal_collection_can_resume() {
    let (_root, _config, _clock, trace, coordinator) = fixture();
    let (handle, task) = coordinator.spawn();
    let live = handle.submit(LocalCommand::StartLiveHr { max_secs: 2 }).unwrap();
    trace.wait_for("live_hr").await;
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(matches!(live.wait().await, Err(CollectorError::Ring(RingError::Timeout { operation: "live_hr" }))));
    assert_cleanup(&trace);
    handle.execute(LocalCommand::SyncNow).await.unwrap();
    assert_eq!(trace.count("drain_history"), 1);
    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn live_completion_and_transport_loss_both_restore_original_mode() {
    for live_mode in [LiveMode::Complete, LiveMode::Disconnected] {
        let (_root, _config, _clock, trace, mut coordinator) = fixture();
        trace.state.lock().unwrap().live = live_mode;
        let outcome = coordinator.execute(LocalCommand::StartLiveHr { max_secs: 1 }).await;
        match live_mode {
            LiveMode::Complete => assert_eq!(outcome.unwrap().detail["samples"][0]["bpm"], 62),
            LiveMode::Disconnected => {
                assert!(matches!(outcome, Err(CollectorError::Ring(RingError::Disconnected))));
                assert_eq!(coordinator.status().ring_unreachable, Some(true));
            }
            LiveMode::Pending | LiveMode::CompleteAtDeadline => unreachable!(),
        }
        assert_cleanup(&trace);
    }
}

#[tokio::test]
async fn control_disconnect_and_shutdown_cancel_live_without_blocking_actor() {
    for shutdown in [false, true] {
        let (_root, _config, _clock, trace, coordinator) = fixture();
        let (handle, task) = coordinator.spawn();
        let live = handle.submit(LocalCommand::StartLiveHr { max_secs: 60 }).unwrap();
        trace.wait_for("live_hr").await;
        if shutdown { handle.shutdown().await.unwrap(); }
        else { handle.set_control_reachable(false); }
        assert!(matches!(live.wait().await, Err(CollectorError::Cancelled)));
        assert_cleanup(&trace);
        if !shutdown {
            assert_eq!(handle.status().pi_reachable, Some(false));
            handle.execute(LocalCommand::SyncNow).await.unwrap();
            handle.shutdown().await.unwrap();
        }
        task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn cancellation_during_history_disconnects_and_persists_scheduler_failure() {
    let (_root, config, clock, trace, coordinator) = fixture();
    trace.state.lock().unwrap().history = HistoryMode::Pending;
    let (handle, task) = coordinator.spawn();
    let sync = handle.submit(LocalCommand::SyncNow).unwrap();
    trace.wait_for("drain_history").await;
    assert!(handle.cancel());
    assert!(matches!(sync.wait().await, Err(CollectorError::Cancelled)));
    assert_eq!(trace.count("disconnect"), 1);
    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
    let recovered = Coordinator::new(config, TestRing(Trace::new()), clock).unwrap();
    assert_eq!(recovered.scheduler().state.consecutive_failures, 1);
    assert_eq!(recovered.store().event_count().unwrap(), 0);
    assert!(recovered.queue().pending().is_empty());
}

#[tokio::test]
async fn pi_reachability_never_implies_adapter_or_ring_reachability() {
    let (_root, _config, _clock, trace, coordinator) = fixture();
    let (handle, task) = coordinator.spawn();
    handle.set_control_reachable(true);
    let status = handle.status();
    assert_eq!(status.pi_reachable, Some(true));
    assert_eq!(status.adapter_present, None);
    assert_eq!(status.ring_recently_seen, None);
    assert_eq!(status.ring_connected, None);
    assert_eq!(status.ring_unreachable, None);
    trace.state.lock().unwrap().connect = ConnectMode::Absent;
    assert!(matches!(handle.execute(LocalCommand::RefreshBattery).await, Err(CollectorError::Ring(RingError::Absent))));
    assert_eq!(handle.status().adapter_present, None);
    assert_eq!(handle.status().ring_unreachable, Some(true));
    assert_eq!(handle.status().pi_reachable, Some(true));
    assert_eq!(trace.count("disconnect"), 1);
    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn receipt_is_separate_and_duplicate_results_never_repeat_execution() {
    let (_root, config, clock, trace, coordinator) = fixture();
    let (handle, task) = coordinator.spawn();
    let mut session = ControlSession::open(&config, clock, handle.clone()).unwrap();
    let command = command("battery-once", CommandName::RefreshBattery, json!({}), NOW + 60);
    let admitted = session.receive(command.clone()).await.unwrap();
    assert!(matches!(admitted.as_slice(), [ControlMessage::Receipt { state: ReceiptState::Accepted, .. }]));
    let duplicate = session.receive(command.clone()).await.unwrap();
    assert_eq!(duplicate.len(), 1);
    let result = session.next_result().await.unwrap();
    assert!(matches!(result, ControlMessage::CommandResult { state: ResultState::Completed, .. }));
    assert_eq!(session.receive(command).await.unwrap().len(), 2);
    assert_eq!(trace.count("battery"), 1);
    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn expired_reconnect_command_never_runs_and_survives_session_restart() {
    let (_root, config, clock, trace, coordinator) = fixture();
    let (handle, task) = coordinator.spawn();
    let mut session = ControlSession::open(&config, clock.clone(), handle.clone()).unwrap();
    session.disconnect();
    clock.advance(Duration::from_secs(100));
    session.connected();
    let expired = command("old-command", CommandName::RefreshBattery, json!({}), NOW + 50);
    let frames = session.receive(expired.clone()).await.unwrap();
    assert!(matches!(frames.last(), Some(ControlMessage::CommandResult { state: ResultState::Expired, .. })));
    drop(session);
    let mut recovered = ControlSession::open(&config, clock, handle.clone()).unwrap();
    assert_eq!(recovered.receive(expired).await.unwrap(), frames);
    assert_eq!(trace.count("battery"), 0);
    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn command_expiring_after_admission_does_not_connect() {
    let (_root, _config, clock, trace, coordinator) = fixture();
    let (handle, task) = coordinator.spawn();
    let ticket = handle.submit_until(LocalCommand::RefreshBattery, NOW + 1, clock.clone()).unwrap();
    clock.advance(Duration::from_secs(2));
    assert!(matches!(ticket.wait().await, Err(CollectorError::Ring(RingError::Expired))));
    assert_eq!(trace.count("connect"), 0);
    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn crash_inflight_is_uncertain_and_never_automatically_reexecuted() {
    let (_root, config, clock, trace, coordinator) = fixture();
    let (handle, task) = coordinator.spawn();
    let mut session = ControlSession::open(&config, clock.clone(), handle.clone()).unwrap();
    let command = command("interrupted", CommandName::StartLiveHr, json!({"max_secs":60}), NOW + 100);
    session.receive(command.clone()).await.unwrap();
    trace.wait_for("live_hr").await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    drop(session);
    drop(handle);
    let fresh = Trace::new();
    let recovered = Coordinator::new(config.clone(), TestRing(fresh.clone()), clock.clone()).unwrap();
    let (handle, task) = recovered.spawn();
    let mut session = ControlSession::open(&config, clock, handle.clone()).unwrap();
    let frames = session.receive(command).await.unwrap();
    assert!(matches!(frames.last(), Some(ControlMessage::CommandResult { state: ResultState::Failed, detail, .. }) if detail["uncertain"] == true));
    assert_eq!(fresh.count("live_hr"), 0);
    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn duplicate_retention_is_bounded_and_only_six_commands_are_enabled() {
    let (_root, config, clock, trace, coordinator) = fixture();
    let (handle, task) = coordinator.spawn();
    let mut session = ControlSession::open(&config, clock, handle.clone()).unwrap();
    for index in 0..270 {
        let frames = session.receive(command(&format!("expired-{index}"), CommandName::SyncNow, json!({}), NOW - 1)).await.unwrap();
        assert!(matches!(frames.last(), Some(ControlMessage::CommandResult { state: ResultState::Expired, .. })));
    }
    assert_eq!(session.replay_results().unwrap().len(), 256);
    for name in [CommandName::InspectSupportedFeatures, CommandName::SetFeatureMode] {
        let frames = session.receive(command(&format!("unsupported-{name:?}"), name, json!({"mode":1}), NOW + 60)).await.unwrap();
        assert!(matches!(frames.first(), Some(ControlMessage::Receipt { state: ReceiptState::Rejected, .. })));
    }
    assert_eq!(trace.count("connect"), 0);
    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn tick_backoff_and_success_persist_without_any_control_session() {
    let (_root, config, clock, trace, mut coordinator) = fixture();
    clock.set(13 * 60 * 60);
    trace.state.lock().unwrap().connect = ConnectMode::Absent;
    assert!(coordinator.tick().await.is_err());
    assert_eq!(coordinator.scheduler().state.consecutive_failures, 1);
    let next = coordinator.scheduler().state.next_run;
    assert!(next > clock.now_unix());
    assert!(coordinator.tick().await.unwrap().is_none());
    drop(coordinator);
    trace.state.lock().unwrap().connect = ConnectMode::Ready;
    let mut recovered = Coordinator::new(config, TestRing(trace.clone()), clock.clone()).unwrap();
    assert_eq!(recovered.scheduler().state.next_run, next);
    clock.set(next);
    assert!(recovered.tick().await.unwrap().is_some());
    assert_eq!(recovered.scheduler().state.consecutive_failures, 0);
    assert_eq!(recovered.scheduler().state.cursor, 1);
}

#[tokio::test]
async fn ring_swap_uses_serial_cursor_not_shared_model_or_previous_ring_cursor() {
    let (_root, config, clock, trace, mut coordinator) = fixture();
    coordinator.execute(LocalCommand::SyncNow).await.unwrap();
    trace.state.lock().unwrap().serial = "replacement-ring".into();
    coordinator.execute(LocalCommand::SyncNow).await.unwrap();
    assert_eq!(trace.state.lock().unwrap().cursors, vec![0, 0]);
    assert_eq!(coordinator.store().cursor_for("runtime-ring").unwrap(), 1);
    assert_eq!(coordinator.store().cursor_for("replacement-ring").unwrap(), 1);
    assert_eq!(coordinator.store().cursor_for("MODEL_05").unwrap(), 0);
    assert_eq!(coordinator.store().event_count().unwrap(), 2);
    drop(coordinator);
    let recovered = Coordinator::new(config, TestRing(trace), clock).unwrap();
    assert_eq!(recovered.store().active_serial().unwrap().as_deref(), Some("replacement-ring"));
}

#[test]
fn process_lease_rejects_second_owner_and_releases_after_drop() {
    let (_root, config, clock, trace, coordinator) = fixture();
    assert!(matches!(Coordinator::new(config.clone(), TestRing(trace.clone()), clock.clone()), Err(CollectorError::Busy)));
    assert_eq!(trace.count("connect"), 0);
    drop(coordinator);
    let recovered = Coordinator::new(config, TestRing(trace), clock).unwrap();
    drop(recovered);
}

struct HangingUpload(Option<tokio::sync::oneshot::Sender<()>>);

#[async_trait]
impl oura_collector::delivery::UploadSink for HangingUpload {
    async fn upload(&mut self, _metadata: &oura_protocol_v1::UploadMetadata, _path: &std::path::Path) -> Result<oura_protocol_v1::UploadAck, CollectorError> {
        if let Some(started) = self.0.take() { let _ = started.send(()); }
        std::future::pending().await
    }
}

#[tokio::test(start_paused = true)]
async fn delivery_is_one_bounded_attempt_and_retains_timed_out_snapshot() {
    let (_root, config, clock, trace, mut coordinator) = fixture();
    coordinator.execute(LocalCommand::SyncNow).await.unwrap();
    let (started, entered) = tokio::sync::oneshot::channel();
    let (handle, task) = coordinator.spawn_with_delivery(HangingUpload(Some(started)));
    entered.await.unwrap();
    let battery = handle.submit(LocalCommand::RefreshBattery).unwrap();
    tokio::time::advance(Duration::from_secs(30)).await;
    battery.wait().await.unwrap();
    assert_eq!(trace.count("battery"), 1);
    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
    let recovered = Coordinator::new(config, TestRing(trace), clock).unwrap();
    assert_eq!(recovered.queue().pending().len(), 1);
    assert_eq!(recovered.queue().pending()[0].attempts, 1);
}

#[tokio::test]
async fn outbound_websocket_authenticates_then_sends_receipt_before_result() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{handshake::server::{Request, Response}, Message};
    let (_root, mut config, clock, trace, coordinator) = fixture();
    let listener = tokio::net::TcpListener::bind(std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), 0)).await.unwrap();
    config.hub_url = format!("http://{}", listener.local_addr().unwrap());
    std::fs::create_dir_all(config.token_file.parent().unwrap()).unwrap();
    std::fs::write(&config.token_file, "runtime-test-credential").unwrap();
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config.token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_hdr_async(stream, |request: &Request, response: Response| {
            assert_eq!(request.uri().path(), "/api/v1/collector/control");
            assert!(request.headers().get("authorization").and_then(|header| header.to_str().ok()).is_some_and(|header| header == "Bearer runtime-test-credential"));
            Ok(response)
        }).await.unwrap();
        let hello = socket.next().await.unwrap().unwrap();
        let Message::Text(hello) = hello else { panic!("expected hello frame") };
        assert!(matches!(serde_json::from_str::<ControlMessage>(&hello).unwrap(), ControlMessage::Hello { protocol_version: PROTOCOL_VERSION, .. }));
        let status = socket.next().await.unwrap().unwrap();
        let Message::Text(status) = status else { panic!("expected status frame") };
        assert!(matches!(serde_json::from_str::<ControlMessage>(&status).unwrap(), ControlMessage::Status { collector_state, .. } if collector_state.pi_reachable && collector_state.unknown && !collector_state.ring_connected));
        socket.send(Message::Text(serde_json::to_string(&command("socket-battery", CommandName::RefreshBattery, json!({}), NOW + 60)).unwrap().into())).await.unwrap();
        let mut receipt_seen = false;
        loop {
            let frame = socket.next().await.unwrap().unwrap();
            let Message::Text(text) = frame else { continue };
            match serde_json::from_str::<ControlMessage>(&text).unwrap() {
                ControlMessage::Receipt { state: ReceiptState::Accepted, .. } => receipt_seen = true,
                ControlMessage::CommandResult { state: ResultState::Completed, .. } => { assert!(receipt_seen); break; }
                ControlMessage::Status { .. } => {},
                _ => panic!("unexpected control response"),
            }
        }
        socket.close(None).await.unwrap();
    });
    let (handle, actor) = coordinator.spawn();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let control = tokio::spawn(oura_collector::control::run_control(config, clock, handle.clone(), shutdown.clone()));
    server.await.unwrap();
    shutdown.cancel();
    control.await.unwrap().unwrap();
    assert_eq!(trace.count("battery"), 1);
    handle.shutdown().await.unwrap();
    actor.await.unwrap().unwrap();
}

struct AcceptingUpload(Option<tokio::sync::oneshot::Sender<()>>);

#[async_trait]
impl oura_collector::delivery::UploadSink for AcceptingUpload {
    async fn upload(&mut self, metadata: &oura_protocol_v1::UploadMetadata, _path: &std::path::Path) -> Result<oura_protocol_v1::UploadAck, CollectorError> {
        if let Some(delivered) = self.0.take() { let _ = delivered.send(()); }
        Ok(oura_protocol_v1::UploadAck { protocol_version: PROTOCOL_VERSION, accepted: true, generation: metadata.generation, active_generation: metadata.generation, duplicate: false, message: "accepted".into() })
    }
}

#[tokio::test(start_paused = true)]
async fn full_outbox_keeps_recovery_marker_until_delivery_frees_capacity() {
    let (_root, mut config, clock, trace, coordinator) = fixture();
    drop(coordinator);
    config.outbox_max_files = 1;
    let mut coordinator = Coordinator::new(config.clone(), TestRing(trace.clone()), clock.clone()).unwrap();
    coordinator.execute(LocalCommand::SyncNow).await.unwrap();
    assert!(matches!(coordinator.execute(LocalCommand::SyncNow).await, Err(CollectorError::OutboxFull)));
    assert!(coordinator.status().pending_snapshot);
    assert_eq!(coordinator.store().event_count().unwrap(), 2);
    drop(coordinator);
    let recovered = Coordinator::new(config.clone(), TestRing(trace.clone()), clock.clone()).unwrap();
    assert!(recovered.status().pending_snapshot);
    let (delivered, accepted) = tokio::sync::oneshot::channel();
    let (handle, task) = recovered.spawn_with_delivery(AcceptingUpload(Some(delivered)));
    accepted.await.unwrap();
    assert!(!handle.status().pending_snapshot);
    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
    let recovered = Coordinator::new(config.clone(), TestRing(trace), clock).unwrap();
    assert_eq!(recovered.store().event_count().unwrap(), 2);
    assert!(!config.state_dir.join("pending-history-snapshot").exists());
}

#[tokio::test]
async fn control_loss_does_not_cancel_normal_history_collection() {
    let (_root, config, clock, trace, coordinator) = fixture();
    trace.state.lock().unwrap().history = HistoryMode::Pending;
    let (handle, task) = coordinator.spawn();
    let session = ControlSession::open(&config, clock, handle.clone()).unwrap();
    let sync = handle.submit(LocalCommand::SyncNow).unwrap();
    trace.wait_for("drain_history").await;
    session.disconnect();
    assert_eq!(handle.status().pi_reachable, Some(false));
    assert_eq!(handle.status().operation_running, Some(true));
    assert_eq!(trace.count("disconnect"), 0);
    assert!(handle.cancel());
    assert!(matches!(sync.wait().await, Err(CollectorError::Cancelled)));
    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn actor_schedules_collection_without_starting_any_network_task() {
    let (_root, config, clock, trace, coordinator) = fixture();
    clock.set(13 * 60 * 60);
    let (handle, task) = coordinator.spawn();
    trace.wait_for("drain_history").await;
    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
    let recovered = Coordinator::new(config, TestRing(trace.clone()), clock).unwrap();
    assert_eq!(recovered.store().event_count().unwrap(), 1);
    assert_eq!(recovered.scheduler().state.cursor, 1);
    assert_eq!(trace.count("connect"), 1);
}

#[tokio::test(start_paused = true)]
async fn full_length_live_capture_returns_samples_before_bounded_watchdog() {
    let (_root, _config, _clock, trace, mut coordinator) = fixture();
    trace.state.lock().unwrap().live = LiveMode::CompleteAtDeadline;
    let started = tokio::time::Instant::now();
    let outcome = coordinator.execute(LocalCommand::StartLiveHr { max_secs: 2 }).await.unwrap();
    assert_eq!(outcome.detail["samples"][0]["bpm"], 62);
    assert_eq!(started.elapsed(), Duration::from_secs(2));
    assert_cleanup(&trace);
}

#[tokio::test]
async fn orphaned_live_mode_is_reconciled_before_start_and_never_restored() {
    use oura_protocol::protocol::feature_mode;
    let (_root, _config, _clock, trace, mut coordinator) = fixture();
    {
        let mut state = trace.state.lock().unwrap();
        state.mode = u32::from(feature_mode::CONNECTED_LIVE);
        state.live = LiveMode::Complete;
    }
    coordinator.execute(LocalCommand::StartLiveHr { max_secs: 2 }).await.unwrap();
    let state = trace.state.lock().unwrap();
    assert_eq!(state.mode, u32::from(feature_mode::AUTOMATIC));
    let reconciled = state.calls.iter().position(|call| *call == "restore_feature_mode").unwrap();
    let started = state.calls.iter().position(|call| *call == "live_hr").unwrap();
    assert!(reconciled < started);
    assert_eq!(state.calls.iter().filter(|call| **call == "restore_feature_mode").count(), 2);
}
