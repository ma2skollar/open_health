//! Synthetic acceptance coverage for authenticated, immutable snapshot ingestion.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use oura_hub::{Config, Hub, HubError};
use oura_protocol_v1::{Identity, UploadMetadata};
use rusqlite::{params, Connection, OpenFlags};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const TOKEN: &str = "synthetic-collector-token-never-in-status";
const COLLECTOR: &str = "pi-01";
const EPOCH: &str = "2026-11-07T09:00:00Z/1";
const EARLIEST: i64 = 1_700_000_000;
const DEVICE_MAC: &str = "arbitrary-nonsecret-device-metadata";
const HEALTH_PAYLOAD: &str = "synthetic-health-payload-never-in-status";

// Mirrors the real ring-store tables, including the decoder-version metadata.
const SCHEMA: &str = "
    PRAGMA page_size = 4096;
    CREATE TABLE store_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    INSERT INTO store_meta VALUES ('decoder_version', '2');
    CREATE TABLE device (
        serial TEXT PRIMARY KEY, hardware_id TEXT, firmware TEXT, api_version TEXT,
        mac TEXT, updated_unix INTEGER NOT NULL
    );
    CREATE TABLE sync_state (
        serial TEXT PRIMARY KEY, next_cursor INTEGER NOT NULL, last_sync_unix INTEGER NOT NULL
    );
    CREATE TABLE events (
        id INTEGER PRIMARY KEY AUTOINCREMENT, serial TEXT NOT NULL, tag INTEGER NOT NULL,
        name TEXT NOT NULL, ring_timestamp INTEGER NOT NULL, body BLOB NOT NULL,
        decoded_json TEXT, captured_unix INTEGER NOT NULL,
        UNIQUE(serial, tag, ring_timestamp, body)
    );
    CREATE INDEX idx_events_serial_tag ON events(serial, tag);
    CREATE INDEX idx_events_capture ON events(captured_unix, id);
    CREATE INDEX idx_events_tag_time ON events(tag, ring_timestamp);
    CREATE INDEX idx_events_decoded_tag ON events(tag) WHERE decoded_json IS NOT NULL;
    CREATE TABLE readings (
        id INTEGER PRIMARY KEY AUTOINCREMENT, serial TEXT NOT NULL, kind TEXT NOT NULL,
        value REAL NOT NULL, unit TEXT, captured_unix INTEGER NOT NULL
    );
    CREATE INDEX idx_readings_serial_kind ON readings(serial, kind);
";

fn snapshot(dir: &Path, name: &str, earliest: i64) -> PathBuf {
    let path = dir.join(name);
    let connection = Connection::open(&path).expect("create synthetic ring database");
    connection
        .execute_batch(SCHEMA)
        .expect("create ring schema");
    connection
        .execute(
            "INSERT INTO device VALUES ('synthetic-ring', 'test-hardware', 'test-firmware', '1', ?1, ?2)",
            params![DEVICE_MAC, earliest],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO sync_state VALUES ('synthetic-ring', 2, ?1)",
            [earliest + 60],
        )
        .unwrap();
    // Ring timestamps use reboot-relative deciseconds, not Unix seconds.
    for (ring_timestamp, captured_unix, tag, name, decoded_json) in [
        (
            1_000_i64,
            earliest,
            66_i64,
            "time_sync",
            format!("{{\"unix_time\":{earliest}}}"),
        ),
        (
            1_600_i64,
            earliest + 60,
            1_i64,
            "synthetic-event",
            format!("{{\"synthetic_payload\":\"{HEALTH_PAYLOAD}\"}}"),
        ),
    ] {
        connection
            .execute(
                "INSERT INTO events (serial, tag, name, ring_timestamp, body, decoded_json, captured_unix)
                 VALUES ('synthetic-ring', ?1, ?2, ?3, ?4, ?5, ?6)",
                params![tag, name, ring_timestamp, HEALTH_PAYLOAD.as_bytes(), decoded_json, captured_unix],
            )
            .unwrap();
    }
    connection
        .execute(
            "INSERT INTO readings (serial, kind, value, unit, captured_unix)
             VALUES ('synthetic-ring', 'heart_rate', 61.0, 'bpm', ?1)",
            [earliest + 60],
        )
        .unwrap();
    connection.close().expect("close complete SQLite snapshot");
    path
}

/// Appending an unanchored observation changes the download-time projection
/// without replacing or editing any earlier raw event or its capture provenance.
fn stalled_snapshot(dir: &Path, name: &str, earliest: i64) -> (Vec<u8>, Vec<u8>) {
    let path = snapshot(dir, name, earliest);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE events SET tag=1, decoded_json='{}' WHERE tag=66",
            [],
        )
        .unwrap();
    connection.close().unwrap();
    let old = fs::read(&path).unwrap();
    let connection = Connection::open(&path).unwrap();
    connection.execute("INSERT INTO events(serial,tag,name,ring_timestamp,body,decoded_json,captured_unix) VALUES('synthetic-ring',1,'synthetic-event',2200,X'03','{}',?1)", [earliest + 3600]).unwrap();
    connection.close().unwrap();
    (old, fs::read(&path).unwrap())
}

fn metadata(bytes: &[u8], epoch: &str, generation: u64) -> UploadMetadata {
    UploadMetadata {
        protocol_version: 1,
        schema_version: 2,
        collector_id: COLLECTOR.to_owned(),
        collector_epoch: epoch.to_owned(),
        generation,
        created_unix: EARLIEST + 120,
        size_bytes: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(bytes)),
        build_revision: "synthetic-revision".to_owned(),
        identity: Identity::Production,
    }
}

fn config(root: &Path) -> Config {
    let mut config = Config::new(root.to_path_buf());
    config.rate_limit = 1_000;
    config
}

fn hub(root: &Path) -> Hub {
    Hub::new(config(root), TOKEN.as_bytes().to_vec()).expect("construct hub")
}

fn files(root: &Path) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    let mut result = Vec::new();
    for entry in fs::read_dir(root).expect("list test directory") {
        let path = entry.unwrap().path();
        if path.is_dir() {
            result.extend(files(&path));
        } else {
            result.push(path);
        }
    }
    result.sort();
    result
}

fn databases(root: &Path) -> Vec<PathBuf> {
    files(&root.join("ring"))
        .into_iter()
        .filter(|path| path.extension().is_some_and(|extension| extension == "db"))
        .collect()
}

fn assert_no_staging(root: &Path) {
    assert!(
        files(&root.join("staging")).is_empty(),
        "staging leaked files"
    );
}

fn assert_rejected(error: HubError, code: &str) {
    assert_eq!(error.code, code, "unexpected error: {}", error.message);
}

fn assert_no_active(hub: &Hub) {
    assert!(hub.active_path(COLLECTOR).unwrap().is_none());
}

fn active_bytes(hub: &Hub) -> Vec<u8> {
    let path = hub
        .active_path(COLLECTOR)
        .unwrap()
        .expect("active generation");
    assert!(
        path.is_file(),
        "active pointer must reference an existing file"
    );
    fs::read(path).unwrap()
}

fn epoch_hex(epoch: &str) -> String {
    epoch
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn valid_snapshot_is_accepted_activated_and_readable() {
    let temp = TempDir::new().unwrap();
    let bytes = fs::read(snapshot(temp.path(), "snapshot.db", EARLIEST)).unwrap();
    let root = temp.path().join("hub");
    let hub = hub(&root);

    let ack = hub
        .ingest(Some(TOKEN), metadata(&bytes, EPOCH, 42), &bytes)
        .unwrap();

    assert_eq!(ack.protocol_version, 1);
    assert!(ack.accepted);
    assert!(!ack.duplicate);
    assert_eq!(ack.generation, 42);
    assert_eq!(ack.active_generation, 42);
    let path = hub.active_path(COLLECTOR).unwrap().unwrap();
    assert_eq!(
        path,
        root.join("ring")
            .join(COLLECTOR)
            .join(epoch_hex(EPOCH))
            .join("42.db")
    );
    let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
    let mac: String = connection
        .query_row("SELECT mac FROM device", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        mac, DEVICE_MAC,
        "nonsecret device metadata must remain intact"
    );
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert_no_staging(&root);
}

#[test]
fn wrong_sha256_is_rejected_without_activation() {
    let temp = TempDir::new().unwrap();
    let bytes = fs::read(snapshot(temp.path(), "snapshot.db", EARLIEST)).unwrap();
    let root = temp.path().join("hub");
    let hub = hub(&root);
    let mut request = metadata(&bytes, EPOCH, 1);
    request.sha256 = "0".repeat(64);

    assert_rejected(
        hub.ingest(Some(TOKEN), request, &bytes).unwrap_err(),
        "sha256_mismatch",
    );
    assert_no_active(&hub);
    assert!(databases(&root).is_empty());
    assert_no_staging(&root);
}

#[test]
fn declared_size_smaller_or_larger_than_body_is_rejected() {
    let temp = TempDir::new().unwrap();
    let bytes = fs::read(snapshot(temp.path(), "snapshot.db", EARLIEST)).unwrap();
    for (name, size) in [
        ("smaller", bytes.len() as u64 - 1),
        ("larger", bytes.len() as u64 + 1),
    ] {
        let root = temp.path().join(name);
        let hub = hub(&root);
        let mut request = metadata(&bytes, EPOCH, 1);
        request.size_bytes = size;

        assert_rejected(
            hub.ingest(Some(TOKEN), request, &bytes).unwrap_err(),
            "size_mismatch",
        );
        assert_no_active(&hub);
        assert!(databases(&root).is_empty());
        assert_no_staging(&root);
    }
}

#[test]
fn truncated_sqlite_and_non_sqlite_have_distinct_error_codes() {
    let temp = TempDir::new().unwrap();
    let bytes = fs::read(snapshot(temp.path(), "snapshot.db", EARLIEST)).unwrap();
    let truncated = bytes[..bytes.len() - 1].to_vec();
    let not_sqlite = vec![b'x'; bytes.len()];
    for (name, body, code) in [
        ("truncated", truncated, "sqlite_truncated"),
        ("not-sqlite", not_sqlite, "sqlite_magic"),
    ] {
        let root = temp.path().join(name);
        let hub = hub(&root);
        assert_rejected(
            hub.ingest(Some(TOKEN), metadata(&body, EPOCH, 1), &body)
                .unwrap_err(),
            code,
        );
        assert_no_active(&hub);
        assert_no_staging(&root);
    }
}

#[test]
fn missing_events_table_is_rejected() {
    let temp = TempDir::new().unwrap();
    let path = snapshot(temp.path(), "snapshot.db", EARLIEST);
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch("DROP TABLE events;").unwrap();
    connection.close().unwrap();
    let bytes = fs::read(path).unwrap();
    let root = temp.path().join("hub");
    let hub = hub(&root);

    assert_rejected(
        hub.ingest(Some(TOKEN), metadata(&bytes, EPOCH, 1), &bytes)
            .unwrap_err(),
        "missing_table",
    );
    assert_no_active(&hub);
    assert_no_staging(&root);
}

#[test]
fn unsupported_schema_names_requested_and_supported_versions() {
    let temp = TempDir::new().unwrap();
    let path = snapshot(temp.path(), "snapshot.db", EARLIEST);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE store_meta SET value = '999' WHERE key = 'decoder_version'",
            [],
        )
        .unwrap();
    connection.close().unwrap();
    let bytes = fs::read(path).unwrap();
    let root = temp.path().join("hub");
    let hub = hub(&root);
    let mut request = metadata(&bytes, EPOCH, 1);
    request.schema_version = 999;

    let error = hub.ingest(Some(TOKEN), request, &bytes).unwrap_err();
    assert_eq!(error.code, "unsupported_schema");
    assert!(
        error.message.contains("999"),
        "missing requested version: {}",
        error.message
    );
    assert!(
        error.message.contains('2'),
        "missing supported version: {}",
        error.message
    );
    assert_no_active(&hub);
    assert_no_staging(&root);
}

#[test]
fn corrupt_readings_leaf_fails_integrity_check() {
    let temp = TempDir::new().unwrap();
    let path = snapshot(temp.path(), "snapshot.db", EARLIEST);
    let connection = Connection::open(&path).unwrap();
    let page_size: usize = connection
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .unwrap();
    let readings_page: usize = connection
        .query_row(
            "SELECT rootpage FROM sqlite_master WHERE type = 'table' AND name = 'readings'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    connection.close().unwrap();
    let mut bytes = fs::read(path).unwrap();
    let offset = (readings_page - 1) * page_size;
    assert_eq!(
        bytes[offset], 0x0d,
        "small readings table must use a table B-tree leaf"
    );
    // Invalid leaf type leaves sqlite_master, store_meta and event timestamps readable.
    bytes[offset] = 0xff;
    let root = temp.path().join("hub");
    let hub = hub(&root);

    assert_rejected(
        hub.ingest(Some(TOKEN), metadata(&bytes, EPOCH, 1), &bytes)
            .unwrap_err(),
        "integrity_check",
    );
    assert_no_active(&hub);
    assert_no_staging(&root);
}

#[test]
fn duplicate_active_generation_is_a_successful_noop() {
    let temp = TempDir::new().unwrap();
    let bytes = fs::read(snapshot(temp.path(), "snapshot.db", EARLIEST)).unwrap();
    let root = temp.path().join("hub");
    let hub = hub(&root);
    hub.ingest(Some(TOKEN), metadata(&bytes, EPOCH, 7), &bytes)
        .unwrap();
    let before = databases(&root);
    let active = hub.active_path(COLLECTOR).unwrap();

    let ack = hub
        .ingest(Some(TOKEN), metadata(&bytes, EPOCH, 7), &bytes)
        .unwrap();

    assert!(ack.accepted);
    assert!(ack.duplicate);
    assert_eq!(ack.generation, 7);
    assert_eq!(ack.active_generation, 7);
    assert_eq!(databases(&root), before);
    assert_eq!(hub.active_path(COLLECTOR).unwrap(), active);
    assert_eq!(active_bytes(&hub), bytes);
    assert_no_staging(&root);
}

#[test]
fn stale_generation_cannot_replace_active_snapshot() {
    let temp = TempDir::new().unwrap();
    let bytes = fs::read(snapshot(temp.path(), "snapshot.db", EARLIEST)).unwrap();
    let different = fs::read(snapshot(temp.path(), "different.db", EARLIEST - 60)).unwrap();
    let root = temp.path().join("hub");
    let hub = hub(&root);
    hub.ingest(Some(TOKEN), metadata(&bytes, EPOCH, 7), &bytes)
        .unwrap();
    let active = hub.active_path(COLLECTOR).unwrap();

    // Cover older generations and a conflicting payload for the active generation.
    for generation in [6, 7] {
        assert_rejected(
            hub.ingest(
                Some(TOKEN),
                metadata(&different, EPOCH, generation),
                &different,
            )
            .unwrap_err(),
            "stale_generation",
        );
        assert_eq!(hub.active_path(COLLECTOR).unwrap(), active);
        assert_eq!(active_bytes(&hub), bytes);
    }
    assert_eq!(databases(&root).len(), 1);
    assert_no_staging(&root);
}

#[test]
fn restart_reuses_orphan_generation_and_commits_only_existing_paths() {
    let temp = TempDir::new().unwrap();
    let bytes = fs::read(snapshot(temp.path(), "snapshot.db", EARLIEST)).unwrap();
    let root = temp.path().join("hub");
    let orphan = root
        .join("ring")
        .join(COLLECTOR)
        .join(epoch_hex(EPOCH))
        .join("12.db");
    fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    fs::write(&orphan, &bytes).unwrap();
    let abandoned = root
        .join("staging")
        .join(COLLECTOR)
        .join("abandoned-upload");
    fs::create_dir_all(abandoned.parent().unwrap()).unwrap();
    fs::write(&abandoned, b"partial snapshot").unwrap();
    // Simulate a durable generation rename followed by a crash before pointer commit.
    let hub = hub(&root);
    assert_no_staging(&root);
    if let Some(path) = hub.active_path(COLLECTOR).unwrap() {
        assert!(
            path.is_file(),
            "recovery must not publish a nonexistent database"
        );
    }

    let ack = hub
        .ingest(Some(TOKEN), metadata(&bytes, EPOCH, 12), &bytes)
        .unwrap();
    assert!(ack.accepted);
    assert_eq!(ack.active_generation, 12);
    assert_eq!(active_bytes(&hub), bytes);
    assert_eq!(databases(&root), vec![orphan]);
    drop(hub);
    let restarted = self::hub(&root);
    assert_eq!(active_bytes(&restarted), bytes);
    let duplicate = restarted
        .ingest(Some(TOKEN), metadata(&bytes, EPOCH, 12), &bytes)
        .unwrap();
    assert!(duplicate.duplicate);
    assert_no_staging(&root);
}

#[test]
fn new_epoch_with_earlier_history_requires_explicit_promotion() {
    let temp = TempDir::new().unwrap();
    let old = fs::read(snapshot(temp.path(), "old.db", EARLIEST)).unwrap();
    let new = fs::read(snapshot(temp.path(), "new.db", EARLIEST - 86_400)).unwrap();
    let root = temp.path().join("hub");
    let hub = hub(&root);
    hub.ingest(Some(TOKEN), metadata(&old, EPOCH, 9), &old)
        .unwrap();
    let old_path = hub.active_path(COLLECTOR).unwrap().unwrap();
    let next_epoch = "reprovisioned/epoch/2";

    let pending = hub
        .ingest(Some(TOKEN), metadata(&new, next_epoch, 1), &new)
        .unwrap();
    assert!(pending.accepted);
    assert_eq!(pending.message, "pending_review");
    assert_eq!(pending.active_generation, 9);
    assert_eq!(
        hub.active_path(COLLECTOR).unwrap().as_ref(),
        Some(&old_path)
    );
    assert_eq!(active_bytes(&hub), old);
    assert!(hub.status().unwrap().collectors[COLLECTOR].pending_review);
    drop(hub);
    let hub = self::hub(&root);
    assert_eq!(active_bytes(&hub), old);
    assert!(hub.status().unwrap().collectors[COLLECTOR].pending_review);

    let promoted = hub.accept_epoch(COLLECTOR, next_epoch).unwrap();
    assert!(promoted.accepted);
    assert_eq!(promoted.active_generation, 1);
    assert!(!hub.status().unwrap().collectors[COLLECTOR].pending_review);
    assert_eq!(active_bytes(&hub), new);
    assert!(
        old_path.is_file(),
        "promotion must retain previous known-good generation"
    );
    assert_no_staging(&root);
}

#[test]
fn wrong_or_missing_token_is_rejected_before_writing_files() {
    let temp = TempDir::new().unwrap();
    let bytes = fs::read(snapshot(temp.path(), "snapshot.db", EARLIEST)).unwrap();
    let root = temp.path().join("hub");
    let hub = hub(&root);
    let before = files(&root);
    for token in [None, Some("wrong-token"), Some("")] {
        // Malformed and oversized metadata must not mask authentication failure.
        let mut request = metadata(&bytes, EPOCH, 1);
        request.collector_id = "../escape".to_owned();
        request.size_bytes = u64::MAX;
        assert_rejected(
            hub.ingest(token, request, &bytes).unwrap_err(),
            "unauthorized",
        );
        assert_eq!(files(&root), before, "unauthenticated request wrote a file");
        assert_no_active(&hub);
    }
}

#[test]
fn declared_and_actual_over_limit_uploads_leave_no_staging() {
    let temp = TempDir::new().unwrap();
    let bytes = fs::read(snapshot(temp.path(), "snapshot.db", EARLIEST)).unwrap();
    for declared_over_limit in [true, false] {
        let root = temp.path().join(if declared_over_limit {
            "declared"
        } else {
            "actual"
        });
        let mut config = config(&root);
        config.max_bytes = bytes.len() as u64 - 1;
        let hub = Hub::new(config, TOKEN.as_bytes().to_vec()).unwrap();
        let mut request = metadata(&bytes, EPOCH, 1);
        if !declared_over_limit {
            request.size_bytes = bytes.len() as u64 - 2;
        }

        assert_rejected(
            hub.ingest(Some(TOKEN), request, &bytes).unwrap_err(),
            "too_large",
        );
        assert_no_active(&hub);
        assert!(databases(&root).is_empty());
        assert_no_staging(&root);
    }
}

#[test]
fn status_redacts_token_device_metadata_and_health_payloads() {
    let temp = TempDir::new().unwrap();
    let bytes = fs::read(snapshot(temp.path(), "snapshot.db", EARLIEST)).unwrap();
    let root = temp.path().join("hub");
    let hub = hub(&root);
    hub.ingest(Some(TOKEN), metadata(&bytes, EPOCH, 3), &bytes)
        .unwrap();
    // Error recording must not echo attacker-controlled secrets into status.
    let mut invalid = metadata(&bytes, EPOCH, 4);
    invalid.collector_id = format!("../{TOKEN}/{HEALTH_PAYLOAD}");
    assert!(hub.ingest(Some(TOKEN), invalid, &bytes).is_err());

    let status = serde_json::to_string(&hub.status().unwrap()).unwrap();
    for forbidden in [
        TOKEN,
        DEVICE_MAC,
        HEALTH_PAYLOAD,
        "decoded_json",
        "hardware_id",
        "data_root",
    ] {
        assert!(!status.contains(forbidden), "status leaked {forbidden}");
    }
    assert!(
        !status.contains(&root.to_string_lossy().to_string()),
        "status leaked a storage path"
    );
    assert!(
        !status.contains(".db"),
        "status leaked a generation database path"
    );
    assert!(
        status.contains(COLLECTOR),
        "status omitted operational collector identity"
    );
    assert_no_staging(&root);
}

#[test]
fn newer_generation_preserves_previous_immutable_database() {
    let temp = TempDir::new().unwrap();
    let old = fs::read(snapshot(temp.path(), "old.db", EARLIEST)).unwrap();
    let new = fs::read(snapshot(temp.path(), "new.db", EARLIEST - 60)).unwrap();
    let root = temp.path().join("hub");
    let hub = hub(&root);
    hub.ingest(Some(TOKEN), metadata(&old, EPOCH, 1), &old)
        .unwrap();
    let old_path = hub.active_path(COLLECTOR).unwrap().unwrap();

    let ack = hub
        .ingest(Some(TOKEN), metadata(&new, EPOCH, 2), &new)
        .unwrap();

    assert_eq!(ack.active_generation, 2);
    assert_eq!(active_bytes(&hub), new);
    assert_eq!(fs::read(old_path).unwrap(), old);
    assert_eq!(databases(&root).len(), 2);
    assert_no_staging(&root);
}

#[test]
fn dropping_unfinished_staging_removes_partial_file() {
    let temp = TempDir::new().unwrap();
    let bytes = fs::read(snapshot(temp.path(), "snapshot.db", EARLIEST)).unwrap();
    let root = temp.path().join("hub");
    let hub = hub(&root);
    let request = metadata(&bytes, EPOCH, 1);
    hub.check_request(&request).unwrap();
    let mut staging = hub.stage(&request).unwrap();
    staging.write_all(&bytes[..100]).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let staged_files = files(&root.join("staging"));
        assert_eq!(staged_files.len(), 1);
        assert_eq!(
            fs::metadata(&staged_files[0]).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    drop(staging);

    assert_no_active(&hub);
    assert_no_staging(&root);
}

#[test]
fn rate_limit_is_per_collector_without_sleeping() {
    let temp = TempDir::new().unwrap();
    let bytes = fs::read(snapshot(temp.path(), "snapshot.db", EARLIEST)).unwrap();
    let root = temp.path().join("hub");
    let mut config = config(&root);
    config.rate_limit = 1;
    config.rate_window = Duration::from_secs(3_600);
    let hub = Hub::new(config, TOKEN.as_bytes().to_vec()).unwrap();
    let request = metadata(&bytes, EPOCH, 1);
    hub.check_request(&request).unwrap();
    let error = hub.check_request(&request).unwrap_err();
    assert_eq!(error.status_code(), 429);
    assert_eq!(error.code, "rate_limited");
    let mut other = request;
    other.collector_id = "pi-02".to_owned();
    hub.check_request(&other).unwrap();
    assert_no_staging(&root);
}

#[test]
fn collector_paths_cannot_escape_data_root() {
    let temp = TempDir::new().unwrap();
    let bytes = fs::read(snapshot(temp.path(), "snapshot.db", EARLIEST)).unwrap();
    let root = temp.path().join("hub");
    let hub = hub(&root);
    for collector in [
        "",
        ".",
        "..",
        "../escape",
        "a/b",
        "a\\b",
        "a:b",
        "pi\u{00e9}",
    ] {
        let mut request = metadata(&bytes, EPOCH, 1);
        request.collector_id = collector.to_owned();
        assert_rejected(
            hub.ingest(Some(TOKEN), request, &bytes).unwrap_err(),
            "invalid_metadata",
        );
        assert!(
            hub.active_path(collector).is_err(),
            "unsafe active-path lookup accepted {collector:?}"
        );
    }
    assert!(databases(&root).is_empty());
    assert!(!temp.path().join("escape").exists());
    assert_no_staging(&root);
}

#[test]
fn later_history_cannot_replace_same_epoch_without_review() {
    let temp = TempDir::new().unwrap();
    let old = fs::read(snapshot(temp.path(), "old.db", EARLIEST)).unwrap();
    let later = fs::read(snapshot(temp.path(), "later.db", EARLIEST + 86_400)).unwrap();
    let root = temp.path().join("hub");
    let hub = hub(&root);
    hub.ingest(Some(TOKEN), metadata(&old, EPOCH, 1), &old)
        .unwrap();
    let before = hub.active_path(COLLECTOR).unwrap();

    assert_rejected(
        hub.ingest(Some(TOKEN), metadata(&later, EPOCH, 2), &later)
            .unwrap_err(),
        "history_regression",
    );
    assert_eq!(hub.active_path(COLLECTOR).unwrap(), before);
    assert_eq!(active_bytes(&hub), old);
    assert_eq!(databases(&root).len(), 1);

    // Explicit review of a new epoch is the only way to replace complete history.
    let pending = hub
        .ingest(
            Some(TOKEN),
            metadata(&later, "rebuilt/partial-history", 1),
            &later,
        )
        .unwrap();
    assert!(pending.accepted);
    assert_eq!(pending.message, "pending_review");
    assert_eq!(active_bytes(&hub), old);
    hub.accept_epoch(COLLECTOR, "rebuilt/partial-history")
        .unwrap();
    assert_eq!(active_bytes(&hub), later);
    assert_no_staging(&root);
}

#[test]
fn expired_request_deadline_rejects_without_staging() {
    let temp = TempDir::new().unwrap();
    let bytes = fs::read(snapshot(temp.path(), "snapshot.db", EARLIEST)).unwrap();
    let root = temp.path().join("hub");
    let mut config = config(&root);
    config.request_timeout = Duration::ZERO;
    let hub = Hub::new(config, TOKEN.as_bytes().to_vec()).unwrap();

    assert_rejected(
        hub.ingest(Some(TOKEN), metadata(&bytes, EPOCH, 1), &bytes)
            .unwrap_err(),
        "request_timeout",
    );
    assert_no_active(&hub);
    assert_no_staging(&root);
}

async fn response_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).expect("versioned JSON response")
}

#[tokio::test]
async fn http_authentication_precedes_metadata_parsing_and_body_work() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let temp = TempDir::new().unwrap();
    let bytes = fs::read(snapshot(temp.path(), "snapshot.db", EARLIEST)).unwrap();
    let root = temp.path().join("hub");
    let hub = hub(&root);
    let app = oura_hub::http::router(hub.clone(), Duration::from_secs(30));
    let before = files(&root);
    for token in [None, Some("wrong-token")] {
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/v1/collector/snapshot")
            .header("X-Oura-Metadata", "not-json")
            .header("Content-Length", u64::MAX.to_string());
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::from(bytes.clone())).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let error = response_json(response).await;
        assert_eq!(error["protocol_version"], 1);
        assert_eq!(error["code"], "unauthorized");
        assert_eq!(files(&root), before);
        assert_no_active(&hub);
    }
}

#[tokio::test]
async fn http_enforces_declared_and_actual_upload_limits() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let temp = TempDir::new().unwrap();
    let bytes = fs::read(snapshot(temp.path(), "snapshot.db", EARLIEST)).unwrap();
    for declared_over_limit in [true, false] {
        let root = temp.path().join(if declared_over_limit {
            "declared"
        } else {
            "actual"
        });
        let mut config = config(&root);
        config.max_bytes = bytes.len() as u64 - 1;
        let hub = Hub::new(config, TOKEN.as_bytes().to_vec()).unwrap();
        let app = oura_hub::http::router(hub.clone(), Duration::from_secs(30));
        let mut metadata = metadata(&bytes, EPOCH, 1);
        if !declared_over_limit {
            metadata.size_bytes = bytes.len() as u64 - 2;
        }
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/collector/snapshot")
            .header("Authorization", format!("Bearer {TOKEN}"))
            .header("X-Oura-Metadata", serde_json::to_string(&metadata).unwrap())
            .body(Body::from(bytes.clone()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let error = response_json(response).await;
        assert_eq!(error["protocol_version"], 1);
        assert_eq!(error["code"], "too_large");
        assert_no_active(&hub);
        assert!(databases(&root).is_empty());
        assert_no_staging(&root);
    }
}

#[tokio::test]
async fn http_status_requires_authentication() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let temp = TempDir::new().unwrap();
    let bytes = fs::read(snapshot(temp.path(), "snapshot.db", EARLIEST)).unwrap();
    let root = temp.path().join("hub");
    let hub = hub(&root);
    hub.ingest(Some(TOKEN), metadata(&bytes, EPOCH, 1), &bytes)
        .unwrap();
    let app = oura_hub::http::router(hub, Duration::from_secs(30));
    for token in [None, Some("wrong-token")] {
        let mut request = Request::builder().uri("/api/v1/hub/status");
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let error = response_json(response).await;
        assert_eq!(error["protocol_version"], 1);
        assert_eq!(error["code"], "unauthorized");
        assert!(!error.to_string().contains(COLLECTOR));
    }
}

#[tokio::test]
async fn http_upload_status_and_rate_limit_use_versioned_redacted_json() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let temp = TempDir::new().unwrap();
    let bytes = fs::read(snapshot(temp.path(), "snapshot.db", EARLIEST)).unwrap();
    let root = temp.path().join("hub");
    let mut config = config(&root);
    config.rate_limit = 1;
    config.rate_window = Duration::from_secs(3_600);
    let hub = Hub::new(config, TOKEN.as_bytes().to_vec()).unwrap();
    let app = oura_hub::http::router(hub.clone(), Duration::from_secs(30));
    let request = || {
        Request::builder()
            .method("POST")
            .uri("/api/v1/collector/snapshot")
            .header("Authorization", format!("Bearer {TOKEN}"))
            .header(
                "X-Oura-Metadata",
                serde_json::to_string(&metadata(&bytes, EPOCH, 5)).unwrap(),
            )
            .body(Body::from(bytes.clone()))
            .unwrap()
    };

    let response = app.clone().oneshot(request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let ack = response_json(response).await;
    assert_eq!(ack["protocol_version"], 1);
    assert_eq!(ack["accepted"], true);
    assert_eq!(ack["active_generation"], 5);
    assert_eq!(active_bytes(&hub), bytes);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/hub/status")
                .header("Authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let status = response_json(response).await;
    assert_eq!(status["protocol_version"], 1);
    assert_eq!(
        status["collectors"][COLLECTOR]["last_accepted_upload"]["generation"],
        5
    );
    assert_eq!(
        status["collectors"][COLLECTOR]["last_accepted_upload"]["bytes"],
        bytes.len() as u64
    );
    assert_eq!(
        status["collectors"][COLLECTOR]["last_accepted_upload"]["identity"],
        "production"
    );
    assert_eq!(
        status["collectors"][COLLECTOR]["newest_measurement_unix"],
        EARLIEST + 60
    );
    let encoded = status.to_string();
    for forbidden in [
        TOKEN,
        DEVICE_MAC,
        HEALTH_PAYLOAD,
        "decoded_json",
        "data_root",
        ".db",
    ] {
        assert!(
            !encoded.contains(forbidden),
            "HTTP status leaked {forbidden}"
        );
    }

    let response = app.oneshot(request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let error = response_json(response).await;
    assert_eq!(error["protocol_version"], 1);
    assert_eq!(error["code"], "rate_limited");
    assert_no_staging(&root);
}

#[test]
fn appended_unanchored_history_retains_old_events_despite_later_projection() {
    let temp = TempDir::new().unwrap();
    let (old, new) = stalled_snapshot(temp.path(), "snapshot.db", EARLIEST);
    let old_clock = oura_summary::ring_time::RingClock::from_events(&[
        (1000, 1, String::new(), EARLIEST),
        (1600, 1, String::new(), EARLIEST + 60),
    ]);
    let new_clock = oura_summary::ring_time::RingClock::from_events(&[
        (1000, 1, String::new(), EARLIEST),
        (1600, 1, String::new(), EARLIEST + 60),
        (2200, 1, String::new(), EARLIEST + 3600),
    ]);
    assert!(new_clock.unix_s(1000, EARLIEST) > old_clock.unix_s(1000, EARLIEST));
    let root = temp.path().join("hub");
    let hub = hub(&root);
    hub.ingest(Some(TOKEN), metadata(&old, EPOCH, 1), &old)
        .unwrap();
    let ack = hub
        .ingest(Some(TOKEN), metadata(&new, EPOCH, 2), &new)
        .unwrap();
    assert_eq!(ack.active_generation, 2);
    assert_eq!(active_bytes(&hub), new);
    assert_no_staging(&root);
}

#[test]
fn larger_candidate_dropping_an_old_raw_event_is_not_a_superset() {
    let temp = TempDir::new().unwrap();
    let (old, _) = stalled_snapshot(temp.path(), "snapshot.db", EARLIEST);
    let path = temp.path().join("snapshot.db");
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch("DELETE FROM events WHERE ring_timestamp=1000;
        INSERT INTO events(serial,tag,name,ring_timestamp,body,decoded_json,captured_unix)
        SELECT serial,tag,name,2800,X'04',decoded_json,captured_unix+60 FROM events WHERE ring_timestamp=2200;").unwrap();
    connection.close().unwrap();
    let new = fs::read(path).unwrap();
    let root = temp.path().join("hub");
    let hub = hub(&root);
    hub.ingest(Some(TOKEN), metadata(&old, EPOCH, 1), &old)
        .unwrap();
    assert_rejected(
        hub.ingest(Some(TOKEN), metadata(&new, EPOCH, 2), &new)
            .unwrap_err(),
        "history_regression",
    );
    assert_eq!(active_bytes(&hub), old);
    assert_no_staging(&root);
}
