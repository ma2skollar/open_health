mod common;
use common::*;
use oura_collector::{CollectorError, Outbox};
use oura_protocol_v1::ingest::Heartbeat;

#[test]
fn restart_partial_cursor_frozen_body_sequence_and_exact_receipt() {
    let (_dir, config) = fixture();
    let mut store = Outbox::open(&config).unwrap();
    append(&mut store, 12);
    let epoch = store.epoch().unwrap();
    let pending = store.next_pending(i64::MAX).unwrap().unwrap();
    let first = pending.batch.events[0].source_row_id;
    assert!(pending.batch.events[0].clock_anchor.is_none());
    let body = pending.body.clone();
    store.record_upload_failure(&pending.batch.batch_id, 1000, 0).unwrap();
    drop(store);
    let mut store = Outbox::open(&config).unwrap();
    assert_eq!(store.cursor().unwrap(), 13);
    assert_eq!(store.epoch().unwrap(), epoch);
    let retry = store.next_pending(i64::MAX).unwrap().unwrap();
    assert_eq!(retry.body, body);
    assert_eq!(retry.attempts, 1);
    for mutate in 0..5 {
        let mut wrong = ack(&retry);
        match mutate { 0 => wrong.body_sha256="00".repeat(32), 1 => wrong.batch_id="wrong".into(), 2 => wrong.device_id="wrong".into(), 3 => wrong.durable=false, _ => wrong.version=2 }
        assert!(store.acknowledge(&wrong, 2000).is_err());
        assert_eq!(store.stats().unwrap().batches, 1);
        assert_eq!(store.receipt_count().unwrap(), 0);
    }
    store.acknowledge(&ack(&retry), 2000).unwrap();
    assert_eq!(store.receipt_count().unwrap(), 1);
    append(&mut store, 13);
    let next = store.next_pending(i64::MAX).unwrap().unwrap();
    assert!(next.batch.events[0].source_row_id > first);
    assert_ne!(next.batch.batch_id, retry.batch.batch_id);
    assert_eq!(next.batch.events[0].body_hex, retry.batch.events[0].body_hex);
}
#[test]
fn owner_lease_regression_gate_and_bounded_receipts() {
    let (_dir, config) = fixture();
    let mut store = Outbox::open(&config).unwrap();
    assert!(matches!(Outbox::open(&config), Err(CollectorError::Busy)));
    for counter in 0..140 {
        append(&mut store, counter);
        let batch = store.next_pending(i64::MAX).unwrap().unwrap();
        store.acknowledge(&ack(&batch), counter as i64).unwrap();
    }
    assert_eq!(store.receipt_count().unwrap(), 128);
    assert!(matches!(store.commit_history(&history(1,true), &info(), Heartbeat::default(), 2000), Err(CollectorError::CursorRegression)));
    assert_eq!(store.cursor().unwrap(), 140);
    drop(store);
    let mut reopened = Outbox::open(&config).unwrap();
    assert!(reopened.blocked().unwrap());
    assert!(reopened.preflight_history().is_err());
}
#[test]
fn actual_sqlite_full_preserves_cursor_and_can_acknowledge() {
    let (_dir, config) = fixture();
    let mut store = Outbox::open(&config).unwrap();
    let mut counter=0;
    loop {
        let mut batch=history(counter,false);
        batch.events[0].body=vec![0x55;65536];
        match store.commit_history(&batch,&info(),Heartbeat::default(),1000) {
            Ok(_) => counter+=1,
            Err(CollectorError::Sqlite(error)) => {
                assert_eq!(error.sqlite_error_code(),Some(rusqlite::ErrorCode::DiskFull));
                break;
            }
            Err(error) => panic!("unexpected pressure code: {}",error.code()),
        }
        assert!(counter<200);
    }
    assert!(counter>0);
    assert_eq!(store.cursor().unwrap(),counter);
    assert_eq!(store.stats().unwrap().batches,counter);
    assert!(store.physical_bytes().unwrap()<=config.max_storage_bytes);
    let pending=store.next_pending(i64::MAX).unwrap().unwrap();
    store.acknowledge(&ack(&pending),2000).unwrap();
    assert_eq!(store.stats().unwrap().batches,counter-1);
}
#[test]
fn heartbeat_does_not_make_backlog() {
    let (_dir, config)=fixture();
    let mut store=Outbox::open(&config).unwrap();
    assert!(store.enqueue_heartbeat(Heartbeat::default(),1_700_000_000_000).unwrap());
    assert!(!store.enqueue_heartbeat(Heartbeat::default(),1_800_000_000_000).unwrap());
    assert!(store.next_pending(i64::MAX).unwrap().unwrap().batch.events.is_empty());
    assert_eq!(store.stats().unwrap().batches,1);
}
#[test]
fn existing_queue_opens_below_free_reserve_and_drains_but_never_collects() {
    let (_dir, mut config) = fixture();
    let mut store = Outbox::open(&config).unwrap();
    append(&mut store, 1);
    append(&mut store, 2);
    let epoch = store.epoch().unwrap();
    drop(store);
    config.min_free_bytes = u64::MAX;
    let mut store = Outbox::open(&config).unwrap();
    assert_eq!(store.epoch().unwrap(), epoch);
    assert!(matches!(store.preflight_history(), Err(CollectorError::LowDisk)));
    assert!(matches!(store.commit_history(&history(3,false), &info(), Heartbeat::default(), 1_700_000_000_000), Err(CollectorError::LowDisk)));
    assert_eq!(store.cursor().unwrap(), 3);
    while let Some(pending) = store.next_pending(i64::MAX).unwrap() {
        store.acknowledge(&ack(&pending), 2000).unwrap();
    }
    assert_eq!(store.stats().unwrap().batches, 0);
    assert_eq!(store.receipt_count().unwrap(), 2);
    assert!(matches!(store.enqueue_heartbeat(Heartbeat::default(), 1_800_000_000_000), Err(CollectorError::LowDisk)));
    assert_eq!(store.stats().unwrap().batches, 0);
    let (_fresh_dir, mut fresh) = fixture();
    fresh.min_free_bytes = u64::MAX;
    assert!(matches!(Outbox::open(&fresh), Err(CollectorError::LowDisk)));
}
#[test]
fn backward_clock_correction_rebases_deadlines_without_touching_bodies() {
    let (_dir, config) = fixture();
    let future = 2_000_000_000_000i64;
    let past = 1_700_000_000_000i64;
    let mut store = Outbox::open(&config).unwrap();
    assert!(!store.observe_clock(future).unwrap());
    store.commit_history(&history(1,false), &info(), Heartbeat::default(), future).unwrap();
    let pending = store.next_pending(future).unwrap().unwrap();
    store.record_upload_failure(&pending.batch.batch_id, future, u64::MAX).unwrap();
    let body = pending.body.clone();
    drop(store);
    let mut store = Outbox::open(&config).unwrap();
    // Before observing the correction, both deadlines sit years ahead of `past`.
    assert!(!store.collection_due(past + 86_400_000).unwrap());
    assert!(store.next_pending(past + 86_400_000).unwrap().is_none());
    assert!(store.observe_clock(past).unwrap());
    assert!(!store.observe_clock(past).unwrap());
    let bound = past + (config.collection_interval_secs.max(config.retry_max_secs) * 1000) as i64;
    assert!(!store.collection_due(past).unwrap());
    assert!(store.collection_due(bound).unwrap());
    assert!(store.next_pending(past).unwrap().is_none());
    let retry = store.next_pending(past + (config.retry_max_secs * 1000) as i64).unwrap().unwrap();
    assert_eq!(retry.body, body);
    assert_eq!(retry.batch.batch_id, pending.batch.batch_id);
    assert_eq!(retry.attempts, 1);
    assert_eq!(retry.batch.events[0].captured_unix_ms, future);
    // Ordinary backoff after the correction survives a normal restart unchanged.
    store.record_upload_failure(&retry.batch.batch_id, past + 1000, u64::MAX).unwrap();
    drop(store);
    let mut store = Outbox::open(&config).unwrap();
    assert!(!store.observe_clock(past + 1000).unwrap());
    assert!(store.next_pending(past + 1000).unwrap().is_none());
    let ceiling = (config.retry_min_secs * 2).min(config.retry_max_secs) as i64;
    assert!(store.next_pending(past + 1000 + ceiling * 1000).unwrap().is_some());
}
fn private_state_dir(config: &oura_collector::Config) {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new().mode(0o700).create(&config.state_dir).unwrap();
    let path = config.database_path();
    std::fs::write(&path, []).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
}
#[test]
fn interrupted_first_init_bootstraps_but_payload_without_identity_fails_closed() {
    let (_dir, config) = fixture();
    private_state_dir(&config);
    let mut store = Outbox::open(&config).unwrap();
    assert_eq!(store.cursor().unwrap(), 0);
    append(&mut store, 1);
    let epoch = store.epoch().unwrap();
    let source = store.next_pending(i64::MAX).unwrap().unwrap().batch.events[0].source_row_id;
    drop(store);
    // Established identity reopens unchanged and keeps counting.
    let mut store = Outbox::open(&config).unwrap();
    assert_eq!(store.epoch().unwrap(), epoch);
    let pending = store.next_pending(i64::MAX).unwrap().unwrap();
    assert_eq!(pending.batch.events[0].source_row_id, source);
    store.acknowledge(&ack(&pending), 2000).unwrap();
    append(&mut store, 2);
    assert_eq!(store.next_pending(i64::MAX).unwrap().unwrap().batch.events[0].source_row_id, source + 1);
    drop(store);
    // Singleton missing while payload exists: fail closed, keep every row.
    let sql = rusqlite::Connection::open(config.database_path()).unwrap();
    sql.execute("DELETE FROM collector_state", []).unwrap();
    drop(sql);
    assert!(matches!(Outbox::open(&config), Err(CollectorError::State)));
    let sql = rusqlite::Connection::open(config.database_path()).unwrap();
    let rows: (u64, u64) = sql.query_row("SELECT (SELECT count(*) FROM batches),(SELECT count(*) FROM receipts)", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(rows, (1, 1));
    // Schema-only database with no payload or receipt bootstraps a new identity.
    sql.execute_batch("DELETE FROM batches; DELETE FROM receipts;").unwrap();
    drop(sql);
    let store = Outbox::open(&config).unwrap();
    assert_ne!(store.epoch().unwrap(), epoch);
    assert_eq!(store.cursor().unwrap(), 0);
}
