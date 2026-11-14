#![allow(dead_code)]
use oura_ble::{DeviceInfo, HistoryBatch};
use oura_collector::{Config, Outbox};
use oura_protocol_v1::ingest::{Ack, Heartbeat, VERSION, body_hash};

pub fn fixture() -> (tempfile::TempDir, Config) {
    let dir = tempfile::tempdir().unwrap();
    let config: Config = serde_json::from_value(serde_json::json!({
        "device_id":"test-device", "ring_name":"SYNTHETIC", "ring_key_file":dir.path().join("unused-ring-key"),
        "state_dir":dir.path().join("state"), "ingest_url":"https://localhost/ingest/v1/batches",
        "key_id":"test-key", "signing_key_file":dir.path().join("synthetic-key"), "min_free_bytes":0,
        "max_storage_bytes":16*1024*1024
    })).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(&config.signing_key_file, [0x42u8;32]).unwrap();
    std::fs::set_permissions(&config.signing_key_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    (dir, config)
}
pub fn history(counter: u64, complete: bool) -> HistoryBatch {
    let event = serde_json::from_value(serde_json::json!({"tag":238,"ring_timestamp":counter,"body":[0,255,17]})).unwrap();
    HistoryBatch { events: vec![event], next_cursor: counter + 1, complete }
}
pub fn info() -> DeviceInfo { DeviceInfo { serial:"SYNTHETIC".into(), firmware:"test".into(), hardware:"test".into() } }
pub fn append(store: &mut Outbox, counter: u64) {
    store.commit_history(&history(counter, false), &info(), Heartbeat::default(), 1_700_000_000_000).unwrap();
}
pub fn ack(pending: &oura_collector::PendingBatch) -> Ack {
    Ack { version:VERSION, device_id:pending.batch.device_id.clone(), batch_id:pending.batch.batch_id.clone(), body_sha256:body_hash(&pending.body), durable:true, accepted_unix_ms:1_700_000_000_001 }
}
