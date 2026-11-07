//! Deterministic scenarios using the production collector and real D5 ingestion.
//! Every scenario owns a private temporary demo namespace; no production paths
//! are accepted by the simulator API or command-line program.
mod hub;

use hub::SimHub;
use oura_ble::{FakeRing, FakeRingHandle, HistoryBatch, HistoryEvent, OperationTimeouts, ScriptOutcome};
use oura_collector::{Clock, CollectorError, Config, Coordinator, FakeClock, LocalCommand, RingStore, SnapshotQueue};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

pub const SCENARIOS: &[&str] = &[
    "normal-sync", "absent-backoff", "offline-recovery", "duplicate-delivery",
    "stale-generation", "new-epoch", "incomplete-history", "live-stop",
    "live-timeout", "live-disconnect", "live-shutdown", "busy-cancelled",
    "expired-reconnect",
];
const START: i64 = 1_794_052_800; // fixed UTC instant, not wall-clock dependent

type SimCollector = Coordinator<FakeRing>;

struct Fixture {
    config: Config,
    clock: Arc<FakeClock>,
    ring: FakeRingHandle,
    collector: SimCollector,
    hub: SimHub,
    _sandbox: TempDir,
}

impl Fixture {
    fn new() -> Result<Self, CollectorError> {
        let sandbox = tempfile::Builder::new().prefix("oura-d6-demo-").tempdir()?;
        let mut config = Config::demo(sandbox.path());
        config.wake_start = "00:00".into();
        config.wake_end = "23:59".into();
        config.timezone = "UTC".into();
        config.retry_min_secs = 2;
        config.retry_max_secs = 16;
        config.min_free_bytes = 0;
        let clock = Arc::new(FakeClock::new(START));
        let (transport, ring) = FakeRing::with_timeouts(OperationTimeouts {
            operation: Duration::from_secs(60),
            ..OperationTimeouts::default()
        });
        let collector = Coordinator::new(config.clone(), transport, clock.clone())?;
        let hub = SimHub::new(&sandbox.path().join("simulated-hub"))?;
        Ok(Self { _sandbox: sandbox, config, clock, ring, collector, hub })
    }

    async fn sync(&mut self) -> Result<(), CollectorError> {
        self.collector.execute(LocalCommand::SyncNow).await?;
        Ok(())
    }

    async fn deliver(&mut self) -> Result<usize, CollectorError> {
        self.collector.queue_mut().deliver_due(&mut self.hub, self.clock.now_unix()).await
    }

    fn advance_retry(&self) {
        self.clock.advance(Duration::from_secs(self.config.retry_max_secs + 1));
    }
}

/// Run a named scenario and return its complete deterministic evidence log.
/// Assertions intentionally fail the process when a contract regresses.
pub async fn run_scenario(name: &str) -> Result<String, CollectorError> {
    if !SCENARIOS.contains(&name) {
        return Err(CollectorError::Config(format!("unknown scenario: {name}")));
    }
    let mut log = format!("identity: demo\nscenario: {name}\nclock: {START}\n");
    match name {
        "normal-sync" => {
            let mut f = Fixture::new()?;
            f.sync().await?;
            let count = f.collector.store().event_count()?;
            assert_eq!(count, 2);
            assert_eq!(f.collector.queue().pending().len(), 1);
            assert_eq!(f.collector.queue().pending()[0].metadata.identity, oura_protocol_v1::Identity::Demo);
            assert_eq!(f.deliver().await?, 1);
            assert!(f.collector.queue().pending().is_empty());
            let ack = &f.hub.acknowledgements[0];
            assert!(ack.accepted);
            assert!(!ack.duplicate);
            log.push_str(&format!("history: {count} events committed\nsnapshot: generation {} identity demo\nupload: accepted generation {}\noutbox: 0 pending\n", ack.generation, ack.active_generation));
        }
        "absent-backoff" => {
            let mut f = Fixture::new()?;
            f.ring.push("connect", ScriptOutcome::Absent).unwrap();
            assert!(f.collector.tick().await.is_err());
            let first_calls = f.ring.calls().len();
            assert!(f.collector.tick().await?.is_none());
            assert_eq!(f.ring.calls().len(), first_calls);
            f.advance_retry();
            assert!(f.collector.tick().await?.is_some());
            assert_eq!(f.collector.store().event_count()?, 2);
            log.push_str("ring: absent\nscheduler: next run persisted; no scan before backoff\nclock: advanced without wall-clock sleep\nring: recovered; history committed\n");
        }
        "offline-recovery" => {
            let mut f = Fixture::new()?;
            f.hub.online = false;
            f.sync().await?;
            let _ = f.deliver().await;
            assert_eq!(f.collector.queue().pending().len(), 1);
            f.advance_retry();
            f.sync().await?;
            let _ = f.deliver().await;
            assert_eq!(f.collector.queue().pending().len(), 2);
            for _ in 0..=f.config.max_attempts {
                f.advance_retry();
                let _ = f.deliver().await;
                assert_eq!(f.collector.queue().pending().len(), 2);
            }
            let mut restarted = SnapshotQueue::open(&f.config)?;
            assert_eq!(restarted.pending().len(), 2);
            f.hub.online = true;
            f.advance_retry();
            let delivered = restarted.deliver_due(&mut f.hub, f.clock.now_unix()).await
                .map_err(|error| CollectorError::Delivery(format!("{error}; hub_rejections={:?}", f.hub.rejections)))?;
            assert_eq!(delivered, 2);
            assert!(restarted.pending().is_empty());
            log.push_str("server: offline beyond reconnect-attempt limit\noutbox: 2 snapshots durably buffered\nreboot: queue recovered\nserver: online\nupload: both snapshots accepted\noutbox: 0 pending; nothing discarded\n");
        }
        "duplicate-delivery" => {
            let mut f = Fixture::new()?;
            f.sync().await?;
            f.hub.lose_next_ack = true;
            let _ = f.deliver().await;
            assert_eq!(f.collector.queue().pending().len(), 1);
            f.advance_retry();
            assert_eq!(f.deliver().await?, 1);
            assert_eq!(f.hub.acknowledgements.len(), 2);
            assert!(f.hub.acknowledgements[1].duplicate);
            log.push_str("upload: durably accepted; response lost\nretry: identical generation\nhub: duplicate acknowledged; active data unchanged\noutbox: 0 pending\n");
        }
        "stale-generation" => {
            let mut f = Fixture::new()?;
            f.sync().await?;
            f.sync().await?;
            let entries = f.collector.queue().pending();
            let old = entries[0].clone();
            let newer = entries[1].clone();
            let ack = f.hub.ingest(newer.metadata.clone(), &newer.path)?;
            assert_eq!(ack.active_generation, newer.metadata.generation);
            let error = f.hub.ingest(old.metadata, &old.path).unwrap_err();
            assert!(error.to_string().contains("stale_generation"));
            assert_eq!(f.collector.queue().pending().len(), 2);
            log.push_str("hub: newer generation activated\nolder upload: stale_generation rejected\nactive generation: unchanged\noutbox: rejected snapshot retained for operator review\n");
        }
        "new-epoch" => {
            let mut f = Fixture::new()?;
            f.sync().await?;
            assert_eq!(f.deliver().await?, 1);
            let old_path = f.hub.hub.active_path(&f.config.collector_id).map_err(|e| CollectorError::Delivery(e.to_string()))?.unwrap();
            let mut reprovisioned = f.config.clone();
            reprovisioned.epoch = "reprovisioned-demo-epoch".into();
            let mut queue = SnapshotQueue::open(&reprovisioned)?;
            let metadata = queue.create_snapshot(f.collector.store(), f.clock.now_unix())?;
            assert_eq!(metadata.generation, 1);
            let entry = &queue.pending()[0];
            let ack = f.hub.ingest(entry.metadata.clone(), &entry.path)?;
            assert!(ack.accepted);
            assert_eq!(ack.message, "pending_review");
            queue.create_snapshot(f.collector.store(), f.clock.now_unix())?;
            let newest = queue.create_snapshot(f.collector.store(), f.clock.now_unix())?;
            assert_eq!(queue.deliver_due(&mut f.hub, f.clock.now_unix()).await?, 3);
            assert!(queue.pending().is_empty());
            assert!(f.hub.acknowledgements.last().unwrap().generation > ack.active_generation);
            assert_eq!(f.hub.hub.active_path(&f.config.collector_id).map_err(|e| CollectorError::Delivery(e.to_string()))?.unwrap(), old_path);
            let promoted = f.hub.hub.accept_epoch(&f.config.collector_id, &reprovisioned.epoch).map_err(|e| CollectorError::Delivery(e.to_string()))?;
            assert!(promoted.accepted);
            assert_eq!(promoted.active_generation, newest.generation);
            assert_ne!(f.hub.hub.active_path(&f.config.collector_id).map_err(|e| CollectorError::Delivery(e.to_string()))?.unwrap(), old_path);
            log.push_str("collector: new epoch; generation restarted at 1\nhub: pending_review; old active generation retained\noperator: explicit continuity promotion\n");
        }
        "incomplete-history" => {
            let mut f = Fixture::new()?;
            f.ring.push("drain_history", ScriptOutcome::PartialHistory(HistoryBatch {
                events: vec![HistoryEvent { tag: 1, ring_timestamp: 1, body: vec![1] }],
                next_cursor: 1, complete: false,
            })).unwrap();
            let result = f.collector.execute(LocalCommand::SyncNow).await;
            assert_eq!(f.collector.store().event_count()?, 1);
            assert_eq!(f.collector.store().cursor()?, 0);
            assert!(!f.collector.queue().pending().is_empty());
            drop(result); // partial progress is reported, never rolled back or presented as complete
            f.sync().await?;
            assert!(f.collector.store().event_count()? >= 2);
            log.push_str("history: incomplete batch\npartial progress: committed with durable snapshot; cursor unchanged\nrecovery: safely replayed from last complete cursor\n");
        }
        other => {
            log.push_str(&runtime_scenario(other).await?);
        }
    }
    log.push_str("result: PASS\n");
    Ok(log)
}

/// Reset a freshly owned demo namespace. No external root or production path
/// can enter this API; Config also validates ownership before deleting files.
pub fn reset_demo() -> Result<String, CollectorError> {
    let sandbox = tempfile::Builder::new().prefix("oura-d6-demo-reset-").tempdir()?;
    let config = Config::demo(sandbox.path());
    config.prepare_state()?;
    let store = RingStore::open(&config.database_path)?;
    drop(store);
    config.reset_demo()?;
    assert!(!config.database_path.exists());
    Ok("identity: demo\nreset: private simulator sandbox only\nproduction paths: not accepted\nresult: PASS\n".into())
}

async fn runtime_scenario(name: &str) -> Result<String, CollectorError> {
    crate::runtime::run(name).await
}

mod runtime;
