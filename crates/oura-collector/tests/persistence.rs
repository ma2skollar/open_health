use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use chrono::DateTime;
use oura_ble::{HistoryBatch, HistoryEvent};
use oura_collector::clock::{Clock, FakeClock};
use oura_collector::delivery::{HttpUploadSink, UploadSink};
use oura_collector::scheduler::Scheduler;
use oura_collector::storage::{DeliveryState, RingStore, SnapshotQueue};
use oura_collector::{CollectorError, Config};
use oura_protocol_v1::{Identity, UploadAck, UploadMetadata, PROTOCOL_VERSION};
use rusqlite::{params, Connection, OpenFlags};
use tempfile::TempDir;

fn fixture() -> (TempDir, Config, RingStore) {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::demo(root.path());
    config.min_free_bytes = 0;
    config.prepare_state().unwrap();
    let store = RingStore::open(&config.database_path).unwrap();
    (root, config, store)
}

fn batch(cursor: u64, complete: bool) -> HistoryBatch {
    HistoryBatch {
        events: vec![HistoryEvent { tag: 0xee, ring_timestamp: cursor, body: vec![1, 2, 3, 4, 5] }],
        next_cursor: cursor, complete,
    }
}

fn unix(value: &str) -> i64 { DateTime::parse_from_rfc3339(value).unwrap().timestamp() }

fn ack(metadata: &UploadMetadata) -> UploadAck {
    UploadAck { protocol_version: PROTOCOL_VERSION, accepted: true, generation: metadata.generation,
        active_generation: metadata.generation, duplicate: false, message: "accepted".into() }
}

struct Accept;
#[async_trait]
impl UploadSink for Accept {
    async fn upload(&mut self, metadata: &UploadMetadata, _: &Path) -> Result<UploadAck, CollectorError> { Ok(ack(metadata)) }
}

struct Offline { calls: usize, journal: PathBuf }
#[async_trait]
impl UploadSink for Offline {
    async fn upload(&mut self, metadata: &UploadMetadata, _: &Path) -> Result<UploadAck, CollectorError> {
        self.calls += 1;
        let connection = Connection::open(&self.journal)?;
        let (attempts, state): (u32, String) = connection.query_row(
            "SELECT attempts,state FROM snapshots WHERE epoch=?1 AND generation=?2",
            params![metadata.collector_epoch, metadata.generation], |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(attempts, self.calls as u32);
        assert_eq!(state, "delivering", "attempt must be durable before upload");
        Err(CollectorError::Delivery("synthetic offline server".into()))
    }
}

#[tokio::test]
async fn fake_clock_sleep_retains_subseconds_without_real_wait() {
    let clock = FakeClock::new(100);
    let clone = clock.clone();
    clock.sleep(Duration::from_millis(750)).await;
    clone.sleep(Duration::from_millis(250)).await;
    assert_eq!(clock.now_unix(), 101);
    clock.advance(Duration::from_secs(9));
    assert_eq!(clone.now_unix(), 110);
}

#[test]
fn wal_backup_keeps_raw_bytes_valid_decodes_and_vendor_schema() {
    let (_root, config, store) = fixture();
    store.write_batch_at("SIM-1", &batch(100, true), 1_700_000_000).unwrap();
    let writer = Connection::open(&config.database_path).unwrap();
    writer.execute_batch("PRAGMA wal_autocheckpoint=0;").unwrap();
    writer.execute("UPDATE events SET decoded_json=?1 WHERE tag=238", [r#"{"preserved":true}"#]).unwrap();
    writer.execute("INSERT INTO events(serial,tag,name,ring_timestamp,body,decoded_json,captured_unix) VALUES('SIM-1',239,'unknown',101,x'010203',NULL,?1)", [unix("2023-11-14T22:13:20Z")]).unwrap();
    // The writer stays open so committed changes remain WAL-backed.
    assert!(config.database_path.with_extension("db-wal").exists());
    store.write_batch_at("SIM-1", &batch(100, true), 1_700_000_001).unwrap();
    let mut queue = SnapshotQueue::open(&config).unwrap();
    let metadata = queue.create_snapshot(&store, 1_700_000_002).unwrap();
    let snapshot = Connection::open_with_flags(&queue.pending()[0].path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(metadata.protocol_version, 1);
    assert_eq!(metadata.schema_version, 2);
    assert_eq!(metadata.identity, Identity::Demo);
    assert_eq!(metadata.created_unix, 1_700_000_002);
    assert_eq!(metadata.size_bytes, std::fs::metadata(&queue.pending()[0].path).unwrap().len());
    assert_eq!(metadata.sha256.len(), 64);
    assert_eq!(snapshot.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0)).unwrap(), "ok");
    assert_eq!(snapshot.query_row("SELECT COUNT(*) FROM events", [], |row| row.get::<_, u64>(0)).unwrap(), 2);
    let (body, decoded): (Vec<u8>, String) = snapshot.query_row("SELECT body,decoded_json FROM events WHERE tag=238", [], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
    assert_eq!(body, vec![1, 2, 3, 4, 5]);
    assert_eq!(decoded, r#"{"preserved":true}"#);
    for name in ["store_meta", "device", "sync_state", "events", "readings"] {
        assert_eq!(snapshot.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1", [name], |row| row.get::<_, u32>(0)).unwrap(), 1);
    }
    assert_eq!(snapshot.query_row("SELECT value FROM store_meta WHERE key='decoder_version'", [], |row| row.get::<_, String>(0)).unwrap(), "2");
    assert_eq!(snapshot.query_row("SELECT captured_unix FROM events WHERE tag=238", [], |row| row.get::<_, i64>(0)).unwrap(), 1_700_000_000);
}

#[test]
fn partial_history_is_retained_without_advancing_cursor_and_replay_deduplicates() {
    let (_root, _config, store) = fixture();
    assert_eq!(store.write_batch_at("SIM-1", &batch(10, false), 1).unwrap(), 1);
    assert_eq!(store.cursor().unwrap(), 0);
    assert_eq!(store.event_count().unwrap(), 1);
    assert_eq!(store.write_batch_at("SIM-1", &batch(10, true), 2).unwrap(), 0);
    assert_eq!(store.cursor().unwrap(), 10);
    assert!(store.write_batch_at("SIM-1", &batch(9, true), 3).is_err());
    assert_eq!(store.event_count().unwrap(), 1);
}

#[test]
fn ring_replacement_uses_independent_cursor_and_persisted_serial() {
    let (_root, config, store) = fixture();
    let first = oura_ble::DeviceInfo { serial: "first-ring".into(), firmware: "1".into(), hardware: "model".into() };
    let second = oura_ble::DeviceInfo { serial: "second-ring".into(), firmware: "2".into(), hardware: "model".into() };
    store.record_device(&first, 1).unwrap();
    store.write_batch_at(&first.serial, &batch(100, true), 1).unwrap();
    assert_eq!(store.cursor_for(&first.serial).unwrap(), 100);
    assert_eq!(store.cursor_for(&second.serial).unwrap(), 0);
    store.record_device(&second, 1).unwrap();
    store.write_batch_at(&second.serial, &batch(5, true), 1).unwrap();
    assert_eq!(store.active_serial().unwrap(), Some(second.serial.clone()));
    drop(store);
    let store = RingStore::open(&config.database_path).unwrap();
    assert_eq!(store.active_serial().unwrap(), Some(second.serial.clone()));
    assert_eq!(store.cursor_for(&second.serial).unwrap(), 5);
    assert_eq!(store.cursor_for(&first.serial).unwrap(), 100);
}

#[test]
fn config_file_overrides_validate_without_leaking_values_into_debug() {
    let (root, config, _store) = fixture();
    let path = root.path().join("collector.json");
    std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    let loaded = Config::load_with_overrides(&path, [
        ("OURA_TIMEZONE".into(), "Europe/Helsinki".into()),
        ("OURA_WAKE_START".into(), "22:30".into()),
        ("OURA_WAKE_END".into(), "06:15".into()),
        ("OURA_RETRY_MIN_SECS".into(), "11".into()),
        ("OURA_EPOCH".into(), "opaque/epoch:two".into()),
    ]).unwrap();
    assert_eq!(loaded.timezone, "Europe/Helsinki");
    assert_eq!(loaded.retry_min_secs, 11);
    assert_eq!(loaded.epoch, "opaque/epoch:two");
    let debug = format!("{loaded:?}");
    assert!(!debug.contains(&loaded.hub_url));
    assert!(!debug.contains("ring.key"));
    for (name, value) in [("OURA_WAKE_START", "8:00"), ("OURA_WAKE_END", "24:00"),
        ("OURA_TIMEZONE", "not/a/timezone"), ("OURA_MAX_ATTEMPTS", "0"),
        ("OURA_RETRY_MAX_SECS", "1"), ("OURA_OUTBOX_MAX_FILES", "0"),
        ("OURA_DATABASE_PATH", "../manual.db"), ("OURA_HUB_URL", "http://example.org")] {
        assert!(Config::load_with_overrides(&path, [(name.into(), value.into())]).is_err(), "{name}");
    }
    let toml_path = root.path().join("collector.toml");
    std::fs::write(&toml_path, toml::to_string(&config).unwrap()).unwrap();
    assert_eq!(Config::load_with_overrides(&toml_path, []).unwrap().collector_id, config.collector_id);
}

#[test]
fn demo_cannot_write_or_reset_foreign_data_or_other_store() {
    let (root, mut config, store) = fixture();
    let manual = root.path().join("manual.db");
    std::fs::write(&manual, b"manual records must survive").unwrap();
    let mut queue = SnapshotQueue::open(&config).unwrap();
    queue.create_snapshot(&store, 1).unwrap();
    let (_other_root, _other_config, other_store) = fixture();
    assert!(queue.create_snapshot(&other_store, 2).is_err(), "demo must not snapshot another identity root");
    drop(queue);
    drop(store);
    std::fs::write(config.state_dir.join("foreign-data.db"), b"foreign").unwrap();
    assert!(config.reset_demo().is_err());
    assert!(config.database_path.exists(), "reset must preflight every entry before deletion");
    std::fs::remove_file(config.state_dir.join("foreign-data.db")).unwrap();
    config.reset_demo().unwrap();
    assert_eq!(std::fs::read(&manual).unwrap(), b"manual records must survive");
    assert!(!config.state_dir.exists());
    config.database_path = manual.clone();
    assert!(config.validate().is_err());
    assert!(config.reset_demo().is_err());
    assert_eq!(std::fs::read(&manual).unwrap(), b"manual records must survive");
    assert!(Config::demo(&root.path().join("production")).validate().is_err());
}

#[test]
fn demo_reset_refuses_live_collector_lease() {
    let (_root, config, store) = fixture();
    drop(store);
    let lease = std::fs::OpenOptions::new().read(true).write(true).create_new(true)
        .open(config.state_dir.join(".collector.lock")).unwrap();
    fs2::FileExt::lock_exclusive(&lease).unwrap();
    assert!(matches!(config.reset_demo(), Err(CollectorError::Busy)));
    assert!(config.database_path.exists());
    drop(lease);
    config.reset_demo().unwrap();
    assert!(!config.state_dir.exists());
}

#[cfg(unix)]
#[test]
fn demo_refuses_symlink_escape_and_production_requires_private_credentials_and_tls() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let (root, mut config, store) = fixture();
    let outside = root.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    symlink(&outside, config.state_dir.join("outbox/escape")).unwrap();
    assert!(config.reset_demo().is_err());
    assert!(outside.exists());
    std::fs::remove_file(config.state_dir.join("outbox/escape")).unwrap();
    drop(store);
    config.reset_demo().unwrap();
    config.identity = Identity::Production;
    config.state_dir = root.path().join("production");
    config.database_path = config.state_dir.join("ring.db");
    config.key_file = root.path().join("ring.key");
    config.token_file = root.path().join("collector.token");
    // Synthetic credentials stay in temporary files, never in snapshots.
    std::fs::write(&config.key_file, b"synthetic-ring-key").unwrap();
    std::fs::write(&config.token_file, b"synthetic-collector-token").unwrap();
    std::fs::set_permissions(&config.key_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(&config.token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(config.validate().is_err(), "production HTTP is forbidden");
    config.hub_url = "https://collector.example.org".into();
    config.validate().unwrap();
    assert_eq!(config.control_url().unwrap(), "wss://collector.example.org/api/v1/collector/control");
    std::fs::set_permissions(&config.key_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(config.validate().is_err());
}

#[tokio::test]
async fn queue_restart_preserves_attempts_exponential_backoff_and_generation() {
    let (_root, mut config, store) = fixture();
    config.retry_min_secs = 5;
    config.retry_max_secs = 12;
    config.max_attempts = 4;
    let mut queue = SnapshotQueue::open(&config).unwrap();
    let first = queue.create_snapshot(&store, 100).unwrap();
    let path = queue.pending()[0].path.clone();
    let mut offline = Offline { calls: 0, journal: config.state_dir.join("delivery.sqlite") };
    assert!(queue.deliver_due(&mut offline, 100).await.is_err());
    assert_eq!(queue.pending()[0].next_attempt_unix, 105);
    drop(queue);
    let mut queue = SnapshotQueue::open(&config).unwrap();
    assert_eq!(queue.pending()[0].attempts, 1);
    assert_eq!(queue.pending()[0].state, DeliveryState::RetryWait);
    assert_eq!(queue.deliver_due(&mut offline, 104).await.unwrap(), 0);
    assert!(queue.deliver_due(&mut offline, 105).await.is_err());
    assert_eq!(queue.pending()[0].next_attempt_unix, 115);
    assert!(queue.deliver_due(&mut offline, 115).await.is_err());
    assert_eq!(queue.pending()[0].next_attempt_unix, 127, "backoff must cap at maximum");
    assert!(path.exists());
    assert_eq!(queue.deliver_due(&mut Accept, 127).await.unwrap(), 1);
    assert!(!path.exists());
    assert!(queue.pending().is_empty());
    drop(queue);
    let connection = Connection::open(config.state_dir.join("delivery.sqlite")).unwrap();
    connection.execute("DELETE FROM generations", []).unwrap();
    drop(connection);
    let mut queue = SnapshotQueue::open(&config).unwrap();
    assert_eq!(queue.create_snapshot(&store, 200).unwrap().generation, first.generation + 1);
    drop(queue);
    config.epoch = "replacement/epoch".into();
    let mut queue = SnapshotQueue::open(&config).unwrap();
    assert_eq!(queue.create_snapshot(&store, 201).unwrap().generation, 1);
    assert_eq!(queue.pending().len(), 2, "reprovisioning does not discard prior queued artifacts");
}

struct Pending;
#[async_trait]
impl UploadSink for Pending {
    async fn upload(&mut self, _: &UploadMetadata, _: &Path) -> Result<UploadAck, CollectorError> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn interrupted_network_attempt_recovers_uncertain_outcome_with_backoff() {
    let (_root, config, store) = fixture();
    let mut queue = SnapshotQueue::open(&config).unwrap();
    queue.create_snapshot(&store, 100).unwrap();
    let mut sink = Pending;
    {
        let attempt = queue.deliver_due(&mut sink, 100);
        tokio::pin!(attempt);
        assert!(futures_util::poll!(attempt.as_mut()).is_pending());
    }
    drop(queue);
    let mut queue = SnapshotQueue::open(&config).unwrap();
    assert_eq!(queue.pending()[0].attempts, 1);
    assert_eq!(queue.pending()[0].next_attempt_unix, 105);
    assert_eq!(queue.pending()[0].state, DeliveryState::RetryWait);
    assert!(queue.pending()[0].last_error.as_deref().unwrap().contains("uncertain"));
    assert_eq!(queue.deliver_due(&mut Accept, 104).await.unwrap(), 0);
    assert_eq!(queue.deliver_due(&mut Accept, 105).await.unwrap(), 1);
}

struct InvalidAck { kind: u8 }
#[async_trait]
impl UploadSink for InvalidAck {
    async fn upload(&mut self, metadata: &UploadMetadata, _: &Path) -> Result<UploadAck, CollectorError> {
        let mut response = ack(metadata);
        match self.kind {
            0 => response.accepted = false,
            1 => response.protocol_version += 1,
            2 => response.generation += 1,
            _ => response.active_generation = 0,
        }
        Ok(response)
    }
}

#[tokio::test]
async fn rejected_artifacts_survive_and_transient_uploads_retry_beyond_ring_attempt_limit() {
    for kind in 0..4 {
        let (_root, config, store) = fixture();
        let mut queue = SnapshotQueue::open(&config).unwrap();
        queue.create_snapshot(&store, 1).unwrap();
        let path = queue.pending()[0].path.clone();
        assert!(queue.deliver_due(&mut InvalidAck { kind }, 1).await.is_err());
        assert_eq!(queue.pending()[0].state, DeliveryState::Rejected);
        drop(queue);
        let mut queue = SnapshotQueue::open(&config).unwrap();
        assert_eq!(queue.pending()[0].state, DeliveryState::Rejected);
        assert_eq!(queue.deliver_due(&mut Accept, 1000).await.unwrap(), 0);
        assert!(path.exists());
    }
    let (_root, mut config, store) = fixture();
    config.max_attempts = 1;
    let mut queue = SnapshotQueue::open(&config).unwrap();
    queue.create_snapshot(&store, 1).unwrap();
    let mut sink = Offline { calls: 0, journal: config.state_dir.join("delivery.sqlite") };
    for now in [1, 6, 16, 36] {
        assert!(queue.deliver_due(&mut sink, now).await.is_err());
        assert_eq!(queue.pending()[0].state, DeliveryState::RetryWait);
    }
    assert!(queue.pending()[0].attempts > config.max_attempts);
    let path = queue.pending()[0].path.clone();
    drop(queue);
    // Recover older persisted stop states without abandoning buffered history.
    let connection = Connection::open(config.state_dir.join("delivery.sqlite")).unwrap();
    connection.execute("UPDATE snapshots SET state='exhausted'", []).unwrap();
    drop(connection);
    let mut queue = SnapshotQueue::open(&config).unwrap();
    assert_eq!(queue.pending()[0].state, DeliveryState::RetryWait);
    assert_eq!(queue.deliver_due(&mut Accept, 1000).await.unwrap(), 1);
    assert!(!path.exists());
}

#[test]
fn queue_limits_and_low_disk_refuse_new_snapshots_without_deleting_old_ones() {
    let (_root, mut config, store) = fixture();
    config.outbox_max_files = 1;
    let mut queue = SnapshotQueue::open(&config).unwrap();
    queue.create_snapshot(&store, 1).unwrap();
    let path = queue.pending()[0].path.clone();
    assert!(matches!(queue.create_snapshot(&store, 2), Err(CollectorError::OutboxFull)));
    assert!(path.exists());
    drop(queue);
    config.outbox_max_files = 10;
    config.outbox_max_bytes = std::fs::metadata(&path).unwrap().len();
    let mut queue = SnapshotQueue::open(&config).unwrap();
    assert!(matches!(queue.create_snapshot(&store, 3), Err(CollectorError::OutboxFull)));
    drop(queue);
    config.outbox_max_bytes = u64::MAX;
    config.min_free_bytes = u64::MAX;
    let mut queue = SnapshotQueue::open(&config).unwrap();
    assert!(matches!(queue.create_snapshot(&store, 4), Err(CollectorError::LowDisk)));
    assert!(path.exists());
    assert_eq!(queue.pending().len(), 1);
}

#[test]
fn tampered_missing_and_untracked_artifacts_are_preserved_and_reported() {
    let (_root, config, store) = fixture();
    let mut queue = SnapshotQueue::open(&config).unwrap();
    queue.create_snapshot(&store, 1).unwrap();
    let path = queue.pending()[0].path.clone();
    drop(queue);
    std::fs::write(&path, b"corrupt artifact").unwrap();
    let queue = SnapshotQueue::open(&config).unwrap();
    assert_eq!(queue.pending()[0].state, DeliveryState::Corrupt);
    drop(queue);
    std::fs::remove_file(&path).unwrap();
    let queue = SnapshotQueue::open(&config).unwrap();
    assert_eq!(queue.pending()[0].state, DeliveryState::Missing);
    drop(queue);
    let orphan = config.outbox_dir().join("orphan.db");
    std::fs::write(&orphan, b"retain me").unwrap();
    assert!(SnapshotQueue::open(&config).is_err());
    assert_eq!(std::fs::read(&orphan).unwrap(), b"retain me");
}

#[test]
fn prepared_snapshot_finalization_recovers_without_reusing_generation() {
    let (_root, config, store) = fixture();
    let mut queue = SnapshotQueue::open(&config).unwrap();
    let first = queue.create_snapshot(&store, 1).unwrap();
    let path = queue.pending()[0].path.clone();
    drop(queue);
    let partial = path.with_extension("db.partial");
    std::fs::rename(&path, &partial).unwrap();
    let connection = Connection::open(config.state_dir.join("delivery.sqlite")).unwrap();
    connection.execute("UPDATE snapshots SET state='prepared'", []).unwrap();
    drop(connection);
    let mut queue = SnapshotQueue::open(&config).unwrap();
    assert!(path.exists());
    assert!(!partial.exists());
    assert_eq!(queue.pending()[0].metadata, first);
    assert_eq!(queue.create_snapshot(&store, 2).unwrap().generation, first.generation + 1);
}

#[test]
fn interrupted_build_keeps_artifact_and_advances_next_generation() {
    let (_root, config, store) = fixture();
    let mut queue = SnapshotQueue::open(&config).unwrap();
    let first = queue.create_snapshot(&store, 1).unwrap();
    let path = queue.pending()[0].path.clone();
    drop(queue);
    let partial = path.with_extension("db.partial");
    std::fs::rename(&path, &partial).unwrap();
    let connection = Connection::open(config.state_dir.join("delivery.sqlite")).unwrap();
    connection.execute("UPDATE snapshots SET state='building'", []).unwrap();
    drop(connection);
    let mut queue = SnapshotQueue::open(&config).unwrap();
    assert!(queue.pending().is_empty());
    assert_eq!(queue.recovery_issues().len(), 1);
    assert!(partial.exists());
    assert_eq!(queue.create_snapshot(&store, 2).unwrap().generation, first.generation + 1);
}

#[tokio::test]
async fn actor_delivery_processes_at_most_one_snapshot() {
    let (_root, config, store) = fixture();
    let mut queue = SnapshotQueue::open(&config).unwrap();
    queue.create_snapshot(&store, 1).unwrap();
    queue.create_snapshot(&store, 2).unwrap();
    assert_eq!(queue.deliver_one_due(&mut Accept, 2).await.unwrap(), 1);
    assert_eq!(queue.pending().len(), 1);
    assert_eq!(queue.deliver_one_due(&mut Accept, 2).await.unwrap(), 1);
    assert!(queue.pending().is_empty());
}

#[test]
fn scheduler_persists_bounded_reconnects_and_cursor_across_reboot() {
    let (_root, mut config, _store) = fixture();
    config.wake_start = "08:00".into();
    config.wake_end = "20:00".into();
    config.retry_min_secs = 10;
    config.retry_max_secs = 40;
    config.max_attempts = 3;
    let now = unix("2026-11-07T12:00:00Z");
    let mut scheduler = Scheduler::open(&config).unwrap();
    assert!(scheduler.due(now));
    scheduler.failure(now).unwrap();
    assert_eq!(scheduler.state.next_run, now + 10);
    assert!(!scheduler.due(now + 9));
    drop(scheduler);
    let mut scheduler = Scheduler::open(&config).unwrap();
    assert_eq!(scheduler.state.consecutive_failures, 1);
    scheduler.failure(now + 10).unwrap();
    assert_eq!(scheduler.state.next_run, now + 30);
    scheduler.failure(now + 30).unwrap();
    assert_eq!(scheduler.state.next_run, unix("2026-11-08T08:00:00Z"));
    let next = scheduler.state.next_run;
    scheduler.success(next, 77).unwrap();
    drop(scheduler);
    let scheduler = Scheduler::open(&config).unwrap();
    assert_eq!(scheduler.state.cursor, 77);
    assert_eq!(scheduler.state.consecutive_failures, 0);
    assert_eq!(scheduler.state.last_run, Some(next));
    assert_eq!(scheduler.state.next_run, next + 40);
}

#[test]
fn recovery_cursor_checkpoint_does_not_spend_reconnect_attempt_or_reschedule() {
    let (_root, config, _store) = fixture();
    let mut scheduler = Scheduler::open(&config).unwrap();
    scheduler.failure(unix("2026-11-07T12:00:00Z")).unwrap();
    let before = scheduler.state.clone();
    scheduler.checkpoint_cursor(88).unwrap();
    assert_eq!(scheduler.state.last_run, before.last_run);
    assert_eq!(scheduler.state.next_run, before.next_run);
    assert_eq!(scheduler.state.consecutive_failures, before.consecutive_failures);
    assert_eq!(scheduler.state.cursor, 88);
    drop(scheduler);
    let scheduler = Scheduler::open(&config).unwrap();
    assert_eq!(scheduler.state.last_run, before.last_run);
    assert_eq!(scheduler.state.next_run, before.next_run);
    assert_eq!(scheduler.state.consecutive_failures, before.consecutive_failures);
    assert_eq!(scheduler.state.cursor, 88);
}

#[test]
fn scheduler_uses_iana_overnight_windows_and_both_dst_transitions() {
    let (_root, mut config, _store) = fixture();
    config.timezone = "America/New_York".into();
    config.wake_start = "22:00".into();
    config.wake_end = "06:00".into();
    let scheduler = Scheduler::open(&config).unwrap();
    assert!(scheduler.due(unix("2026-07-01T03:00:00Z")));
    assert!(!scheduler.due(unix("2026-07-01T12:00:00Z")));
    drop(scheduler);
    config.wake_start = "02:15".into();
    config.wake_end = "03:15".into();
    let mut scheduler = Scheduler::open(&config).unwrap();
    assert!(!scheduler.due(unix("2026-03-08T06:59:00Z")));
    assert!(scheduler.due(unix("2026-03-08T07:00:00Z")));
    assert!(!scheduler.due(unix("2026-03-08T07:15:00Z")));
    // A retry that enters a missing local hour starts at the first real instant.
    scheduler.failure(unix("2026-03-08T06:59:58Z")).unwrap();
    assert_eq!(scheduler.state.next_run, unix("2026-03-08T07:00:03Z"));
    drop(scheduler);
    config.epoch = "fall-DST".into();
    config.wake_start = "01:00".into();
    config.wake_end = "02:00".into();
    let scheduler = Scheduler::open(&config).unwrap();
    assert!(scheduler.due(unix("2026-11-01T05:30:00Z")));
    assert!(scheduler.due(unix("2026-11-01T06:30:00Z")));
    assert!(!scheduler.due(unix("2026-11-01T07:00:00Z")));
}

#[tokio::test]
async fn http_sink_uses_authenticated_metadata_header_and_unmodified_sqlite_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (_root, mut config, store) = fixture();
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    config.hub_url = format!("http://{}", listener.local_addr().unwrap());
    std::fs::create_dir_all(config.token_file.parent().unwrap()).unwrap();
    std::fs::write(&config.token_file, b"synthetic-test-credential").unwrap();
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config.token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut queue = SnapshotQueue::open(&config).unwrap();
    let metadata = queue.create_snapshot(&store, 1).unwrap();
    let expected_bytes = std::fs::read(&queue.pending()[0].path).unwrap();
    let expected_metadata = metadata.clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let boundary = loop {
            let mut chunk = [0u8; 4096];
            let count = socket.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&chunk[..count]);
            if let Some(offset) = request.windows(4).position(|part| part == b"\r\n\r\n") { break offset + 4; }
            assert!(request.len() < 32 * 1024);
        };
        let headers = std::str::from_utf8(&request[..boundary]).unwrap();
        assert!(headers.starts_with("POST /api/v1/collector/snapshot HTTP/1.1\r\n"));
        let metadata_header = headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("x-oura-metadata").then_some(value.trim())
        }).unwrap();
        assert_eq!(serde_json::from_str::<UploadMetadata>(metadata_header).unwrap(), expected_metadata);
        assert!(headers.lines().any(|line| line.eq_ignore_ascii_case("authorization: Bearer synthetic-test-credential")));
        while request.len() - boundary < expected_bytes.len() {
            let mut chunk = [0u8; 8192];
            let count = socket.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&chunk[..count]);
        }
        assert_eq!(&request[boundary..], expected_bytes.as_slice());
        let reply = serde_json::to_vec(&ack(&expected_metadata)).unwrap();
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", reply.len()).as_bytes()).await.unwrap();
        socket.write_all(&reply).await.unwrap();
    });
    let mut sink = HttpUploadSink::new(&config).unwrap();
    assert_eq!(queue.deliver_due(&mut sink, 1).await.unwrap(), 1);
    server.await.unwrap();
}
