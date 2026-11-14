//! Synthetic credentials and temp stores only. No backend, Bluetooth or network.
pub mod hub;
use std::{sync::Arc, time::Duration};
use oura_ble::{FakeRing, HistoryBatch, ScriptOutcome};
use oura_collector::{Collector, CollectorError, Config, FakeClock, Clock, Outbox, delivery::sign_attempt};
use hub::SyntheticSink;

pub const SCENARIOS: &[&str] = &["history-only", "outage-restart", "uncertain-ack", "conflicting-receipt", "partial-drain", "disk-pressure", "nonce-rotation", "clock-gate", "key-permissions"];

pub fn synthetic_config(root: &std::path::Path) -> Config {
    Config { device_id: "synthetic-device".into(), ring_name: "SYNTHETIC".into(), ring_key_file: root.join("unused-ring-key"), state_dir: root.join("state"), ingest_url: "https://localhost/ingest/v1/batches".into(), key_id: "synthetic-key".into(), signing_key_file: root.join("synthetic-signing-key"), ca_certificate_file: None, max_storage_bytes: 16 * 1024 * 1024, min_free_bytes: 0, history_max_events: 256, history_max_bytes: 128 * 1024, history_max_pages: 16, collection_interval_secs: 300, heartbeat_interval_secs: 900, operation_timeout_secs: 1, retry_min_secs: 5, retry_max_secs: 60 }
}
fn credential(config: &Config) -> Result<(), CollectorError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(&config.signing_key_file, [0x5au8; 32])?;
    std::fs::set_permissions(&config.signing_key_file, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}
pub async fn run_scenario(name: &str) -> Result<String, CollectorError> {
    if !SCENARIOS.contains(&name) { return Err(CollectorError::Config); }
    let sandbox = tempfile::tempdir()?;
    let mut config = synthetic_config(sandbox.path());
    credential(&config)?;
    if name == "disk-pressure" { config.min_free_bytes = u64::MAX; }
    let clock = Arc::new(FakeClock::new(1_700_000_000_000));
    let (ring, trace) = FakeRing::new();
    if name == "partial-drain" {
        let event = serde_json::from_value(serde_json::json!({"tag":238,"ring_timestamp":12,"body":[0,255]}))?;
        trace.push("drain_history", ScriptOutcome::PartialHistory(HistoryBatch { events: vec![event], next_cursor: 13, complete: false })).unwrap();
    }
    let sink = SyntheticSink::default();
    if name == "disk-pressure" {
        assert!(matches!(Outbox::open(&config), Err(CollectorError::LowDisk)));
        assert!(trace.calls().is_empty());
        return Ok(format!("synthetic: {name} passed\n"));
    }
    let mut collector = Collector::new(config.clone(), ring, sink.clone(), clock.clone())?;
    collector.collect_once().await?;
    let cursor = collector.outbox.cursor()?;
    let epoch = collector.outbox.epoch()?;
    let pending = collector.outbox.next_pending(clock.now_unix_ms())?.unwrap();
    let body = pending.body.clone();
    match name {
        "outage-restart" | "uncertain-ack" => {
            if name == "outage-restart" { sink.0.lock().unwrap().offline = true; }
            else { sink.0.lock().unwrap().lose_next_ack = true; }
            assert!(collector.upload_once().await.is_err());
            assert_eq!(collector.outbox.stats()?.batches, 1);
            drop(collector);
            clock.advance(Duration::from_secs(120));
            sink.0.lock().unwrap().offline = false;
            let (ring, _) = FakeRing::new();
            collector = Collector::new(config.clone(), ring, sink.clone(), clock.clone())?;
            assert_eq!(collector.outbox.cursor()?, cursor);
            assert_eq!(collector.outbox.epoch()?, epoch);
            let retry = collector.outbox.next_pending(clock.now_unix_ms())?.unwrap();
            assert_eq!(retry.body, body);
            assert_eq!(retry.attempts, 1);
            assert!(collector.upload_once().await?);
            assert_eq!(sink.0.lock().unwrap().receipts.len(), 1);
            assert_eq!(sink.0.lock().unwrap().rows.len(), pending.batch.events.len());
        }
        "conflicting-receipt" => {
            sink.0.lock().unwrap().conflicting_ack = true;
            assert!(collector.upload_once().await.is_err());
            assert_eq!(collector.outbox.stats()?.batches, 1);
            assert_eq!(collector.outbox.receipt_count()?, 0);
        }
        "partial-drain" => {
            assert_eq!(cursor, 13);
            drop(collector);
            let reopened = Outbox::open(&config)?;
            assert_eq!(reopened.cursor()?, 13);
            assert_eq!(reopened.next_pending(clock.now_unix_ms())?.unwrap().body, body);
            return Ok(format!("synthetic: {name} passed\n"));
        }
        "nonce-rotation" => {
            let first = sign_attempt(&config, &pending, clock.as_ref(), [1; 16])?;
            let second = sign_attempt(&config, &pending, clock.as_ref(), [2; 16])?;
            assert_ne!(first.fields.nonce, second.fields.nonce);
            assert_ne!(first.signature, second.signature);
            assert_eq!(first.fields.body_sha256, second.fields.body_sha256);
            std::fs::write(&config.signing_key_file, [0x33; 32])?;
            let rotated = sign_attempt(&config, &pending, clock.as_ref(), [2; 16])?;
            assert_ne!(rotated.signature, second.signature);
        }
        "clock-gate" => {
            clock.set_synchronized(false);
            assert!(matches!(collector.upload_once().await, Err(CollectorError::Clock)));
            assert_eq!(collector.outbox.stats()?.batches, 1);
        }
        "key-permissions" => {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&config.signing_key_file, std::fs::Permissions::from_mode(0o644))?;
            assert!(matches!(sign_attempt(&config, &pending, clock.as_ref(), [1; 16]), Err(CollectorError::Permissions)));
        }
        "history-only" => {
            assert_eq!(trace.calls(), vec!["connect", "device_info", "battery", "drain_history", "disconnect"]);
            assert!(collector.upload_once().await?);
            let first_id = pending.batch.events.last().unwrap().source_row_id;
            collector.collect_once().await?;
            let next = collector.outbox.next_pending(clock.now_unix_ms())?.unwrap();
            assert!(next.batch.events[0].source_row_id > first_id);
            assert_eq!(collector.outbox.epoch()?, epoch);
            assert!(next.batch.events.iter().all(|e| e.clock_anchor.is_none()));
        }
        _ => unreachable!(),
    }
    Ok(format!("synthetic: {name} passed\n"))
}
