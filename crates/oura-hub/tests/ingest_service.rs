//! Synthetic wire coverage for the frozen `POST /ingest/v1/batches`
//! service: durable ack, replay protection, idempotent restart, conflict
//! and authorisation rejection, transport limits, heartbeat bookkeeping,
//! and the `events` projection with retry backoff. Every key comes from the
//! public synthetic test vector; no real credential is embedded.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::anyhow;
use axum::body::to_bytes;
use axum::http::{HeaderValue, Request, StatusCode};
use axum::Router;
use bytes::Bytes;
use http_body_util::Full;
use oura_hub::ingest::{process_pending, router, Service};
use oura_protocol_v1::ingest::{self as wire, test_vectors as tv, Ack, Batch, SigningFields};
use rusqlite::Connection;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("non-negative clock")
        .as_secs() as i64
}

fn nonce() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Owner-only JSON key registry as the production loader requires.
fn write_registry(path: &Path, revoked: bool) {
    let registry = json!({"keys": [{
        "key_id": tv::KEY_ID,
        "device_id": tv::DEVICE_ID,
        "key_hex": hex::encode(tv::KEY),
        "revoked": revoked,
    }]});
    std::fs::write(
        path,
        serde_json::to_vec(&registry).expect("registry serialises"),
    )
    .expect("write key registry");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

fn setup(revoked: bool) -> (TempDir, PathBuf, PathBuf) {
    let dir = TempDir::new().expect("temp dir");
    let keys = dir.path().join("keys.json");
    write_registry(&keys, revoked);
    let db = dir.path().join("hub.db");
    (dir, db, keys)
}

fn signed_with(
    body: Vec<u8>,
    nonce_value: u64,
    timestamp: i64,
    key: &[u8],
) -> Request<Full<Bytes>> {
    let fields = SigningFields {
        key_id: tv::KEY_ID.to_owned(),
        device_id: tv::DEVICE_ID.to_owned(),
        timestamp,
        nonce: format!("{nonce_value:032x}"),
        body_sha256: wire::body_hash(&body),
    };
    let signature = fields.sign(key).expect("synthetic key signs");
    Request::builder()
        .method(wire::METHOD)
        .uri(wire::PATH)
        .header("content-type", wire::CONTENT_TYPE)
        .header(wire::HEADER_KEY_ID, fields.key_id)
        .header(wire::HEADER_DEVICE_ID, fields.device_id)
        .header(wire::HEADER_TIMESTAMP, fields.timestamp.to_string())
        .header(wire::HEADER_NONCE, fields.nonce)
        .header(wire::HEADER_BODY_SHA256, fields.body_sha256)
        .header(wire::HEADER_SIGNATURE, signature)
        .body(Full::new(Bytes::from(body)))
        .expect("static request parts")
}

fn signed(body: Vec<u8>, nonce_value: u64, timestamp: i64) -> Request<Full<Bytes>> {
    signed_with(body, nonce_value, timestamp, &tv::KEY)
}

async fn send(app: Router, request: Request<Full<Bytes>>) -> (StatusCode, Value) {
    let response = app.oneshot(request).await.expect("router answers");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), wire::MAX_BODY_BYTES + 1)
        .await
        .expect("bounded response body");
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Same device and batch id, different bytes: a distinct upload, never a retry.
fn conflicting_body() -> Vec<u8> {
    let mut value: Value = serde_json::from_str(tv::BATCH_BODY).expect("frozen batch decodes");
    value["collector_build"] = Value::String("oura-collector 0.1.0-conflict".into());
    serde_json::to_vec(&value).expect("conflicting batch serialises")
}

fn open_db(db: &Path) -> Connection {
    Connection::open(db).expect("open committed hub db")
}

#[tokio::test]
async fn valid_batch_is_durable_acked() {
    let (_dir, db, keys) = setup(false);
    let app = router(Service::new(db, keys));
    let (status, value) =
        send(app, signed(tv::BATCH_BODY.as_bytes().to_vec(), nonce(), now())).await;
    assert_eq!(status, StatusCode::OK);
    let ack: Ack = serde_json::from_value(value).expect("ack decodes");
    let batch: Batch = serde_json::from_str(tv::BATCH_BODY).expect("frozen batch decodes");
    ack.validate(&batch, tv::BATCH_BODY.as_bytes())
        .expect("ack covers the exact sent batch");
}

#[tokio::test]
async fn same_nonce_replayed_is_rejected() {
    let (_dir, db, keys) = setup(false);
    let app = router(Service::new(db, keys));
    let reused = nonce();
    let (first, _) = send(
        app.clone(),
        signed(tv::BATCH_BODY.as_bytes().to_vec(), reused, now()),
    )
    .await;
    assert_eq!(first, StatusCode::OK);
    let (replay, _) =
        send(app, signed(tv::BATCH_BODY.as_bytes().to_vec(), reused, now())).await;
    assert_eq!(replay, StatusCode::CONFLICT);
}

#[tokio::test]
async fn restart_replays_idempotent_receipt() {
    let (_dir, db, keys) = setup(false);
    let (_, original) = send(
        router(Service::new(db.clone(), keys.clone())),
        signed(tv::BATCH_BODY.as_bytes().to_vec(), nonce(), now()),
    )
    .await;
    // Fresh service on the same db: same batch id and bytes, fresh nonce,
    // exactly what a restarted collector retry sends.
    let (status, receipt) = send(
        router(Service::new(db, keys)),
        signed(tv::BATCH_BODY.as_bytes().to_vec(), nonce(), now()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(receipt, original, "restart must replay the original receipt");
}

#[tokio::test]
async fn same_batch_id_different_body_conflicts() {
    let (_dir, db, keys) = setup(false);
    let app = router(Service::new(db, keys));
    let (first, _) =
        send(app.clone(), signed(tv::BATCH_BODY.as_bytes().to_vec(), nonce(), now())).await;
    assert_eq!(first, StatusCode::OK);
    let (status, value) = send(app, signed(conflicting_body(), nonce(), now())).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(value["error"], "batch_conflict");
}

#[tokio::test]
async fn wrong_signature_is_unauthorized() {
    let (_dir, db, keys) = setup(false);
    let app = router(Service::new(db, keys));
    let wrong_key: Vec<u8> = tv::KEY.iter().map(|b| b ^ 0xff).collect();
    let (status, _) = send(
        app,
        signed_with(tv::BATCH_BODY.as_bytes().to_vec(), nonce(), now(), &wrong_key),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn revoked_key_is_unauthorized() {
    let (_dir, db, keys) = setup(true);
    let app = router(Service::new(db, keys));
    let (status, _) =
        send(app, signed(tv::BATCH_BODY.as_bytes().to_vec(), nonce(), now())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn future_timestamp_is_unauthorized() {
    let (_dir, db, keys) = setup(false);
    let app = router(Service::new(db, keys));
    let (status, _) = send(
        app,
        signed(
            tv::BATCH_BODY.as_bytes().to_vec(),
            nonce(),
            now() + wire::CLOCK_WINDOW_SECONDS + 1,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn declared_oversized_body_is_rejected() {
    let (_dir, db, keys) = setup(false);
    let app = router(Service::new(db, keys));
    let mut request = signed(tv::BATCH_BODY.as_bytes().to_vec(), nonce(), now());
    request.headers_mut().insert(
        "content-length",
        HeaderValue::from_str(&(wire::MAX_BODY_BYTES + 1).to_string()).expect("digits only"),
    );
    let (status, _) = send(app, request).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn wrong_content_type_is_rejected() {
    let (_dir, db, keys) = setup(false);
    let app = router(Service::new(db, keys));
    let mut request = signed(tv::BATCH_BODY.as_bytes().to_vec(), nonce(), now());
    request
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("text/plain"));
    let (status, _) = send(app, request).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn wrong_path_is_not_found() {
    let (_dir, db, keys) = setup(false);
    let app = router(Service::new(db, keys));
    let mut request = signed(tv::BATCH_BODY.as_bytes().to_vec(), nonce(), now());
    *request.uri_mut() = "/ingest/v1/batches/".parse().expect("static uri");
    let (status, _) = send(app, request).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn empty_events_heartbeat_is_accepted_and_registered() {
    let (_dir, db, keys) = setup(false);
    let app = router(Service::new(db.clone(), keys));
    let (status, ack) =
        send(app, signed(tv::HEARTBEAT_BODY.as_bytes().to_vec(), nonce(), now())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ack["batch_id"], "b-00000000000000000002");
    let c = open_db(&db);
    let (received, heartbeat, build): (i64, String, String) = c
        .query_row(
            "SELECT received_unix_ms, heartbeat, collector_build FROM ingest_collectors
             WHERE device_id=?1",
            [tv::DEVICE_ID],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("collector row exists");
    assert!(received > 0);
    assert_eq!(build, "oura-collector 0.1.0");
    let stored: Value = serde_json::from_str(&heartbeat).expect("stored heartbeat decodes");
    assert_eq!(stored["clock_synchronized"], false);
    let raw: i64 = c.query_row("SELECT COUNT(*) FROM ingest_raw", [], |r| r.get(0)).unwrap();
    assert_eq!(raw, 0, "heartbeat-only batch stores no raw events");
    let state: String = c
        .query_row("SELECT state FROM ingest_jobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(state, "done", "heartbeat needs no physiological decoding");
}

#[tokio::test]
async fn projection_projects_raw_events_with_source_identity() {
    let (_dir, db, keys) = setup(false);
    let app = router(Service::new(db.clone(), keys));
    let (status, _) =
        send(app, signed(tv::BATCH_BODY.as_bytes().to_vec(), nonce(), now())).await;
    assert_eq!(status, StatusCode::OK);
    let attempted = process_pending(&db, |_: &Path| Ok(json!({"summary": "synthetic"})))
        .expect("processing succeeds");
    assert!(attempted);
    let c = open_db(&db);
    let projected: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM events WHERE ingest_raw_id IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(projected, 2);
    let (source, epoch, row, anchored, transported): (String, String, String, i64, i64) = c
        .query_row(
            "SELECT source_device, collector_epoch, source_row_id,
                    clock_anchor_json IS NOT NULL, transport_json IS NOT NULL
             FROM events WHERE ingest_raw_id IS NOT NULL AND source_row_id='1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(source, tv::DEVICE_ID);
    assert_eq!(epoch, "0123456789abcdef0123456789abcdef");
    assert_eq!(row, "1");
    assert_eq!(anchored, 1, "observed clock anchor is carried through");
    assert_eq!(transported, 1, "legacy transport provenance is carried through");
    let anchorless: i64 = c
        .query_row(
            "SELECT clock_anchor_json IS NOT NULL
             FROM events WHERE ingest_raw_id IS NOT NULL AND source_row_id='2'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(anchorless, 0, "anchor-free event stays anchor-free");
    let (summary, completed): (String, i64) = c
        .query_row(
            "SELECT summary, completed_revision FROM ingest_processing WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(summary, r#"{"summary":"synthetic"}"#);
    assert!(completed >= 1);
    let job: String = c
        .query_row(
            "SELECT state FROM ingest_jobs WHERE revision=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(job, "done");
}

#[tokio::test]
async fn callback_failure_retains_last_good_summary_with_backoff() {
    let (_dir, db, keys) = setup(false);
    let app = router(Service::new(db.clone(), keys.clone()));
    let (status, _) =
        send(app, signed(tv::BATCH_BODY.as_bytes().to_vec(), nonce(), now())).await;
    assert_eq!(status, StatusCode::OK);
    process_pending(&db, |_: &Path| Ok(json!({"summary": "good"})))
        .expect("first analysis succeeds");
    // Second batch: new batch id, same source rows, so a fresh job is queued.
    let mut next: Value = serde_json::from_str(tv::BATCH_BODY).expect("frozen batch decodes");
    next["batch_id"] = Value::String("b-00000000000000000003".into());
    let body = serde_json::to_vec(&next).unwrap();
    let app = router(Service::new(db.clone(), keys));
    let (status, _) = send(app, signed(body, nonce(), now())).await;
    assert_eq!(status, StatusCode::OK);
    let failure = process_pending(&db, |_: &Path| Err(anyhow!("synthetic analysis failure")));
    assert!(failure.is_err(), "build callback failure must surface");
    let c = open_db(&db);
    let (state, last_error, next_attempt): (String, Option<String>, i64) = c
        .query_row(
            "SELECT state, last_error, next_attempt_unix FROM ingest_jobs WHERE revision=2",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(state, "failed");
    assert_eq!(last_error.as_deref(), Some("analysis_failed"));
    assert!(
        next_attempt > now(),
        "retry backoff must be visible in ingest_jobs.next_attempt_unix"
    );
    let (summary, completed): (String, i64) = c
        .query_row(
            "SELECT summary, completed_revision FROM ingest_processing WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(summary, r#"{"summary":"good"}"#, "last good summary is retained");
    assert_eq!(completed, 1);
    let failed_code: String = c
        .query_row(
            "SELECT code FROM ingest_failures WHERE revision=2",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(failed_code, "analysis_failed");
}

#[tokio::test]
async fn restore_holdoff_refuses_ingestion() {
    let (_dir, db, keys) = setup(false);
    let app = router(Service::new(db.clone(), keys));
    let (status, _) =
        send(app, signed(tv::BATCH_BODY.as_bytes().to_vec(), nonce(), now())).await;
    assert_eq!(status, StatusCode::OK);
    // `oura-backup` restore installs the fail-closed replay window.
    let c = open_db(&db);
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS ingest_recovery (
            id INTEGER PRIMARY KEY CHECK(id=1),
            holdoff_until_unix INTEGER NOT NULL);
         INSERT OR REPLACE INTO ingest_recovery VALUES(1, 9999999999);",
    )
    .expect("holdoff row installs");
    drop(c);
    let (status, value) = send(
        router(Service::new(db, temp_registry())),
        signed(tv::BATCH_BODY.as_bytes().to_vec(), nonce(), now()),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(value["error"], "restore_holdoff");
}

fn temp_registry() -> PathBuf {
    let dir = TempDir::new().expect("temp dir");
    let keys = dir.keep().join("keys.json");
    write_registry(&keys, false);
    keys
}
