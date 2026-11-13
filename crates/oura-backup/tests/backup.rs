//! Acceptance tests for backup, verification and restore.

use std::fs;
use std::path::{Path, PathBuf};

use oura_backup::{create, format_utc, list, read_manifest, restore, verify, Error, Source};
use rusqlite::Connection;
use tempfile::TempDir;

/// A stand-in for `manual.db`: a real SQLite database with the meta table.
fn make_database(dir: &Path, name: &str, values: &[(&str, f64)]) -> PathBuf {
    let path = dir.join(name);
    let conn = Connection::open(&path).expect("open");
    conn.execute_batch(
        "CREATE TABLE store_manual_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO store_manual_meta (key, value) VALUES ('schema_version', '1');
         CREATE TABLE manual_entries (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             local_date TEXT NOT NULL,
             kind TEXT NOT NULL,
             value REAL NOT NULL,
             UNIQUE(local_date, kind)
         );",
    )
    .expect("schema");
    for (date, value) in values {
        conn.execute(
            "INSERT INTO manual_entries (local_date, kind, value) VALUES (?1, 'weight_kg', ?2)",
            rusqlite::params![date, value],
        )
        .expect("insert");
    }
    drop(conn);
    path
}

fn rows(path: &Path) -> Vec<(String, f64)> {
    let conn = Connection::open(path).expect("open");
    let mut stmt = conn
        .prepare("SELECT local_date, value FROM manual_entries ORDER BY local_date")
        .expect("prepare");
    let out = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect");
    out
}

const EPOCH: i64 = 0;
const A_2023_INSTANT: i64 = 1_700_000_000;
const A_LEAP_DAY_2024: i64 = 1_709_164_800;

#[test]
fn formats_a_utc_timestamp() {
    assert_eq!(format_utc(EPOCH), "19700101-000000");
    // A 2023 instant, exercising a year boundary.
    assert_eq!(format_utc(A_2023_INSTANT), "20231114-221320");
    // A leap day, to catch an off-by-one in the civil conversion.
    assert_eq!(format_utc(A_LEAP_DAY_2024), "20240229-000000");
}

#[test]
fn a_backup_round_trips_and_verifies() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let manual = make_database(&data, "manual.db", &[("2026-01-01", 70.5), ("2026-01-02", 70.2)]);

    let backups = tmp.path().join("backups");
    let dir = create(
        &backups,
        &[Source::new("manual.db", manual.clone())],
        "test run",
    )
    .expect("create");

    let manifest = read_manifest(&dir).expect("manifest");
    assert_eq!(manifest.files.len(), 1);
    assert_eq!(manifest.files[0].name, "manual.db");
    assert_eq!(manifest.files[0].schema_version.as_deref(), Some("1"));
    assert!(manifest.files[0].bytes > 0);
    assert_eq!(manifest.files[0].sha256.len(), 64);
    assert!(
        manifest.label.contains("test-run"),
        "the label is sanitised, not dropped: {}",
        manifest.label
    );

    let report = verify(&dir).expect("verify");
    assert!(report.ok);
    assert_eq!(report.files.len(), 1);

    // The copy holds the same rows as the source.
    assert_eq!(rows(&dir.join("manual.db")), rows(&manual));
}

#[test]
fn a_backup_captures_wal_state_a_plain_copy_would_lose() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let path = data.join("manual.db");

    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    conn.execute_batch(
        "CREATE TABLE store_manual_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO store_manual_meta (key, value) VALUES ('schema_version', '1');
         CREATE TABLE manual_entries (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             local_date TEXT NOT NULL,
             kind TEXT NOT NULL,
             value REAL NOT NULL,
             UNIQUE(local_date, kind)
         );
         INSERT INTO manual_entries (local_date, kind, value)
            VALUES ('2026-03-01', 'weight_kg', 68.0);",
    )
    .unwrap();

    // The writing connection is still open, so the newest row lives in the WAL.
    let backups = tmp.path().join("backups");
    let dir = create(&backups, &[Source::new("manual.db", path)], "wal").expect("create");
    assert_eq!(
        rows(&dir.join("manual.db")),
        vec![("2026-03-01".to_string(), 68.0)],
        "the online backup API includes uncommitted WAL content"
    );
    drop(conn);
}

#[test]
fn a_tampered_backup_fails_its_checksum() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let manual = make_database(&data, "manual.db", &[("2026-01-01", 70.5)]);
    let dir = create(
        &tmp.path().join("backups"),
        &[Source::new("manual.db", manual)],
        "x",
    )
    .expect("create");

    let copied = dir.join("manual.db");
    let mut bytes = fs::read(&copied).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&copied, bytes).unwrap();

    match verify(&dir) {
        Err(Error::ChecksumMismatch { name, .. }) => assert_eq!(name, "manual.db"),
        other => panic!("expected a checksum mismatch, got {other:?}"),
    }
}

#[test]
fn a_truncated_backup_is_refused() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let manual = make_database(&data, "manual.db", &[("2026-01-01", 70.5)]);
    let dir = create(
        &tmp.path().join("backups"),
        &[Source::new("manual.db", manual)],
        "x",
    )
    .expect("create");

    let copied = dir.join("manual.db");
    let bytes = fs::read(&copied).unwrap();
    fs::write(&copied, &bytes[..bytes.len() / 2]).unwrap();

    assert!(
        verify(&dir).is_err(),
        "a truncated database must never verify"
    );
}

#[test]
fn a_missing_file_is_named() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let manual = make_database(&data, "manual.db", &[("2026-01-01", 70.5)]);
    let dir = create(
        &tmp.path().join("backups"),
        &[Source::new("manual.db", manual)],
        "x",
    )
    .expect("create");

    fs::remove_file(dir.join("manual.db")).unwrap();
    match verify(&dir) {
        Err(Error::Missing { name, .. }) => assert_eq!(name, "manual.db"),
        other => panic!("expected a missing-file error, got {other:?}"),
    }
}

#[test]
fn restore_needs_force_to_replace_and_then_restores_exactly() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let manual = make_database(
        &data,
        "manual.db",
        &[("2026-01-01", 70.5), ("2026-01-02", 70.2)],
    );
    let dir = create(
        &tmp.path().join("backups"),
        &[Source::new("manual.db", manual.clone())],
        "x",
    )
    .expect("create");
    let taken = rows(&manual);

    // Lose a row, as if the user deleted it by mistake.
    {
        let conn = Connection::open(&manual).unwrap();
        conn.execute(
            "DELETE FROM manual_entries WHERE local_date = '2026-01-02'",
            [],
        )
        .unwrap();
    }
    assert_eq!(rows(&manual).len(), 1);

    let target = tmp.path().join("restore-target");
    fs::create_dir_all(&target).unwrap();
    fs::copy(&manual, target.join("manual.db")).unwrap();

    match restore(&dir, &target, false) {
        Err(Error::DestinationExists { .. }) => {}
        other => panic!("expected a refusal without --force, got {other:?}"),
    }
    assert_eq!(
        rows(&target.join("manual.db")),
        rows(&manual),
        "a refused restore must not have touched anything"
    );

    let report = restore(&dir, &target, true).expect("restore");
    assert_eq!(report.restored.len(), 1);
    assert_eq!(
        rows(&target.join("manual.db")),
        taken,
        "the restored database holds what the backup held"
    );
}

#[test]
fn a_failed_restore_leaves_the_destination_intact() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let manual = make_database(&data, "manual.db", &[("2026-01-01", 70.5)]);
    let dir = create(
        &tmp.path().join("backups"),
        &[Source::new("manual.db", manual.clone())],
        "x",
    )
    .expect("create");

    let target = tmp.path().join("restore-target");
    fs::create_dir_all(&target).unwrap();
    fs::copy(&manual, target.join("manual.db")).unwrap();
    let before = fs::read(target.join("manual.db")).unwrap();

    // Corrupt the backup: verification must fail before anything is replaced.
    let copied = dir.join("manual.db");
    let mut bytes = fs::read(&copied).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&copied, bytes).unwrap();

    assert!(restore(&dir, &target, true).is_err());
    assert_eq!(
        fs::read(target.join("manual.db")).unwrap(),
        before,
        "a rejected restore must leave the previous file untouched"
    );
}

#[test]
fn creating_with_no_source_is_an_error_not_an_empty_backup() {
    let tmp = TempDir::new().unwrap();
    let backups = tmp.path().join("backups");
    match create(
        &backups,
        &[Source::new("manual.db", tmp.path().join("absent.db"))],
        "x",
    ) {
        Err(Error::NoSources(_)) => {}
        other => panic!("expected a no-sources error, got {other:?}"),
    }
    assert!(
        !backups.exists(),
        "no empty backup directory is left behind"
    );
}

#[test]
fn list_reports_newest_first_and_ignores_other_directories() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let manual = make_database(&data, "manual.db", &[("2026-01-01", 70.5)]);
    let backups = tmp.path().join("backups");
    fs::create_dir_all(backups.join("not-a-backup")).unwrap();

    let first = create(
        &backups,
        &[Source::new("manual.db", manual.clone())],
        "one",
    )
    .unwrap();
    // A distinct second, so the ordering is unambiguous.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let second = create(&backups, &[Source::new("manual.db", manual)], "two").unwrap();

    let entries = list(&backups).expect("list");
    assert_eq!(entries.len(), 2, "the unrelated directory is skipped");
    assert_eq!(entries[0].dir, second, "newest first");
    assert_eq!(entries[1].dir, first);
}

/// The hub's ingest tables, verbatim enough to exercise a full round trip.
const INGEST_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS ingest_batches (
 revision INTEGER PRIMARY KEY AUTOINCREMENT, device_id TEXT NOT NULL, batch_id TEXT NOT NULL,
 body_sha256 TEXT NOT NULL, body BLOB NOT NULL, receipt TEXT NOT NULL, accepted_unix_ms INTEGER NOT NULL,
 UNIQUE(device_id,batch_id));
CREATE TABLE IF NOT EXISTS ingest_nonces (key_id TEXT NOT NULL, nonce TEXT NOT NULL, expires_unix INTEGER NOT NULL, PRIMARY KEY(key_id,nonce));
CREATE TABLE IF NOT EXISTS ingest_raw (
 id INTEGER PRIMARY KEY AUTOINCREMENT, revision INTEGER NOT NULL, device_id TEXT NOT NULL,
 collector_epoch TEXT NOT NULL, source_row_id TEXT NOT NULL, serial TEXT NOT NULL,
 tag INTEGER NOT NULL, ring_counter INTEGER NOT NULL, body BLOB NOT NULL,
 captured_unix_ms INTEGER NOT NULL, event_json TEXT NOT NULL, transport BLOB,
 UNIQUE(device_id,collector_epoch,source_row_id));
CREATE TABLE IF NOT EXISTS ingest_clock_anchors (
 raw_id INTEGER PRIMARY KEY, kind TEXT NOT NULL, ring_counter INTEGER NOT NULL, captured_unix_ms INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS ingest_collectors (device_id TEXT PRIMARY KEY, received_unix_ms INTEGER NOT NULL, heartbeat TEXT NOT NULL, collector_build TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS ingest_jobs (
 revision INTEGER PRIMARY KEY, state TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
 next_attempt_unix INTEGER NOT NULL DEFAULT 0, last_error TEXT, stage TEXT NOT NULL DEFAULT 'decode',
 started_unix_ms INTEGER, completed_unix_ms INTEGER);
CREATE TABLE IF NOT EXISTS ingest_failures (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL, failed_unix_ms INTEGER NOT NULL, stage TEXT NOT NULL, code TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS ingest_processing (
 id INTEGER PRIMARY KEY CHECK(id=1), completed_revision INTEGER NOT NULL DEFAULT 0,
 decoded_revision INTEGER NOT NULL DEFAULT 0, summary TEXT, summary_unix_ms INTEGER);
INSERT OR IGNORE INTO ingest_processing(id) VALUES(1);
CREATE TABLE IF NOT EXISTS events (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 ingest_raw_id INTEGER NOT NULL UNIQUE,
 serial TEXT NOT NULL, tag INTEGER NOT NULL, name TEXT NOT NULL,
 ring_timestamp INTEGER NOT NULL, body BLOB NOT NULL, decoded_json TEXT,
 captured_unix INTEGER, source_device TEXT, collector_epoch TEXT, source_row_id TEXT,
 clock_anchor_json TEXT, transport_json TEXT
);
";

/// A stand-in for the hub's ring database: ingest tables, rows and projection.
fn make_ingest_database(dir: &Path) -> PathBuf {
    let path = dir.join("oura.db");
    let conn = Connection::open(&path).expect("open");
    conn.execute_batch(INGEST_SCHEMA).expect("ingest schema");
    conn.execute_batch(
        "
         INSERT INTO ingest_raw(revision, device_id, collector_epoch, source_row_id, serial,
             tag, ring_counter, body, captured_unix_ms, event_json)
             VALUES(3, 'device', 'epoch-1', 'row-1', 'AA11223344', 0x50, 12, x'0500', 1760000000000,
                 '{\"tag\":80,\"body_hex\":\"0500\",\"ring_counter\":12,\"captured_unix_ms\":1760000000000}');
         INSERT INTO ingest_raw(revision, device_id, collector_epoch, source_row_id, serial,
             tag, ring_counter, body, captured_unix_ms, event_json)
             VALUES(4, 'device', 'epoch-1', 'row-2', 'AA11223344', 0x50, 13, x'0600', 1760000600000,
                 '{\"tag\":80,\"body_hex\":\"0600\",\"ring_counter\":13,\"captured_unix_ms\":1760000600000}');
         INSERT INTO ingest_clock_anchors(raw_id, kind, ring_counter, captured_unix_ms)
             VALUES(1, 'boot', 0, 1760000000000);
         INSERT INTO ingest_clock_anchors(raw_id, kind, ring_counter, captured_unix_ms)
             VALUES(2, 'boot', 0, 1760000600000);
         INSERT INTO ingest_batches(device_id, batch_id, body_sha256, body, receipt, accepted_unix_ms)
             VALUES('device', 'batch-1', 'abc', x'7b7d', '{\"ok\":true}', 1760000700000);
         INSERT INTO ingest_nonces(key_id, nonce, expires_unix) VALUES('key-1', 'n-1', 1760000700);
         INSERT INTO ingest_nonces(key_id, nonce, expires_unix) VALUES('key-1', 'n-2', 1760000700);
         INSERT INTO ingest_collectors(device_id, received_unix_ms, heartbeat, collector_build)
             VALUES('device', 1760000700000, '{\"battery\":80}', 'collector-1');
         INSERT INTO ingest_jobs(revision, state, stage) VALUES(4, 'pending', 'decode');
         INSERT INTO ingest_failures(id, revision, failed_unix_ms, stage, code)
             VALUES(1, 2, 1760000500000, 'decode', 'bad_tag');
         INSERT INTO events(ingest_raw_id, serial, tag, name, ring_timestamp, body, decoded_json,
             captured_unix, source_device, collector_epoch, source_row_id, clock_anchor_json, transport_json)
             VALUES(1, 'AA11223344', 0x50, 'met', 12, x'0500', '{\"met\":2.0}', 1760000000,
                 'device', 'epoch-1', 'row-1', '{\"kind\":\"boot\"}', NULL);
         ",
    )
    .expect("ingest rows");
    drop(conn);
    path
}

/// One row per ingest_* table plus the projection, for exact comparisons.
fn table_counts(path: &Path) -> Vec<(String, i64)> {
    let conn = Connection::open(path).expect("open");
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .expect("prepare");
    let tables: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .expect("query")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect");
    drop(stmt);
    tables
        .into_iter()
        .filter(|table| !table.starts_with("sqlite_"))
        .map(|table| {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |row| {
                    row.get(0)
                })
                .expect("count");
            (table, count)
        })
        .collect()
}

#[test]
fn ingest_backup_round_trips_every_table_and_the_manual_db() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let manual = make_database(&data, "manual.db", &[("2026-01-01", 70.5)]);
    let ring = make_ingest_database(&data);

    let dir = create(
        &tmp.path().join("backups"),
        &[
            Source::new("manual.db", manual.clone()),
            Source::new("oura.db", ring.clone()),
        ],
        "ingest",
    )
    .expect("create");

    let target = tmp.path().join("restore-target");
    restore(&dir, &target, false).expect("restore");

    // `ingest_recovery` is added by the restore itself, so compare the rest.
    let restored_counts: Vec<(String, i64)> = table_counts(&target.join("oura.db"))
        .into_iter()
        .filter(|(table, _)| table != "ingest_recovery")
        .collect();
    assert_eq!(restored_counts, table_counts(&ring));
    assert_eq!(rows(&target.join("manual.db")), rows(&manual));
}

#[test]
fn restore_preserves_provenance_and_clock_anchors() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let ring = make_ingest_database(&data);
    let dir = create(&tmp.path().join("backups"), &[Source::new("oura.db", ring)], "p").expect("create");

    let target = tmp.path().join("restore-target");
    restore(&dir, &target, false).expect("restore");

    let conn = Connection::open(target.join("oura.db")).unwrap();
    let (device, epoch, source, serial): (String, String, String, String) = conn
        .query_row(
            "SELECT source_device, collector_epoch, source_row_id, serial FROM events WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!((device.as_str(), epoch.as_str(), source.as_str()), ("device", "epoch-1", "row-1"));
    assert_eq!(serial, "AA11223344");
    let anchor: String = conn
        .query_row("SELECT clock_anchor_json FROM events WHERE ingest_raw_id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(anchor, "{\"kind\":\"boot\"}");
}

#[test]
fn restore_preserves_jobs_and_receipts() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let ring = make_ingest_database(&data);
    let dir = create(&tmp.path().join("backups"), &[Source::new("oura.db", ring)], "j").expect("create");

    let target = tmp.path().join("restore-target");
    restore(&dir, &target, false).expect("restore");

    let conn = Connection::open(target.join("oura.db")).unwrap();
    let jobs: i64 = conn
        .query_row("SELECT COUNT(*) FROM ingest_jobs WHERE state = 'pending'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(jobs, 1, "unfinished jobs survive the restore and can resume");
    let (receipt, hash): (String, String) = conn
        .query_row(
            "SELECT receipt, body_sha256 FROM ingest_batches WHERE batch_id = 'batch-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((receipt.as_str(), hash.as_str()), ("{\"ok\":true}", "abc"));
}

#[test]
fn restoring_an_ingest_db_installs_a_replay_holdoff() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let ring = make_ingest_database(&data);
    let dir = create(&tmp.path().join("backups"), &[Source::new("oura.db", ring)], "h").expect("create");

    let target = tmp.path().join("restore-target");
    restore(&dir, &target, false).expect("restore");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let conn = Connection::open(target.join("oura.db")).unwrap();
    let holdoff: i64 = conn
        .query_row(
            "SELECT holdoff_until_unix FROM ingest_recovery WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .expect("the holdoff row exists");
    assert!(
        holdoff >= now + 590 && holdoff <= now + 610,
        "the holdoff is a 600-second window, got {holdoff} vs now {now}"
    );
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM ingest_recovery", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 1, "id=1 upsert keeps exactly one holdoff row");
}

#[test]
fn restoring_a_manual_only_backup_writes_no_holdoff() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let manual = make_database(&data, "manual.db", &[("2026-01-01", 70.5)]);
    let dir = create(&tmp.path().join("backups"), &[Source::new("manual.db", manual)], "m").expect("create");

    let target = tmp.path().join("restore-target");
    restore(&dir, &target, false).expect("restore");

    let conn = Connection::open(target.join("manual.db")).unwrap();
    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'ingest_recovery'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0, "no holdoff table appears in a manual-only restore");
}

#[test]
fn the_manifest_holds_paths_and_metadata_only() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let manual = make_database(&data, "manual.db", &[("2026-01-01", 70.5)]);
    let dir = create(&tmp.path().join("backups"), &[Source::new("manual.db", manual)], "s").expect("create");

    let raw = fs::read_to_string(dir.join("manifest.json")).unwrap();
    let text = raw.to_lowercase();
    for banned in ["key_hex", "secret", "token", "password", "credential"] {
        assert!(!text.contains(&banned), "the manifest must not mention {banned}");
    }
    assert!(raw.contains("manual.db") && raw.contains("sha256"), "paths and metadata only");
}

#[test]
fn restore_refuses_an_active_wal() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let manual = make_database(&data, "manual.db", &[("2026-01-01", 70.5)]);
    let dir = create(&tmp.path().join("backups"), &[Source::new("manual.db", manual)], "w").expect("create");

    let target = tmp.path().join("restore-target");
    fs::create_dir_all(&target).unwrap();
    // A live writer in WAL mode leaves a -wal sidecar next to the database.
    let conn = Connection::open(target.join("manual.db")).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE keepme(x);").unwrap();
    assert!(target.join("manual.db-wal").exists());

    match restore(&dir, &target, true) {
        Err(Error::ActiveWal { .. }) => {}
        other => panic!("expected an active-WAL refusal, got {other:?}"),
    }
    drop(conn);
}

#[test]
fn restore_refuses_a_corrupt_source_before_writing() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let manual = make_database(&data, "manual.db", &[("2026-01-01", 70.5)]);
    let dir = create(&tmp.path().join("backups"), &[Source::new("manual.db", manual)], "c").expect("create");
    fs::write(dir.join("manual.db"), b"not a database at all").unwrap();

    let target = tmp.path().join("restore-target");
    fs::create_dir_all(&target).unwrap();
    assert!(restore(&dir, &target, true).is_err(), "a corrupt source is never trusted");
    assert!(!target.join("manual.db").exists(), "nothing was written");
}

#[test]
fn restore_refuses_a_symlink_target() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let manual = make_database(&data, "manual.db", &[("2026-01-01", 70.5)]);
    let dir = create(&tmp.path().join("backups"), &[Source::new("manual.db", manual)], "y").expect("create");

    let target = tmp.path().join("restore-target");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("elsewhere.db"), b"away").unwrap();
    std::os::unix::fs::symlink(target.join("elsewhere.db"), target.join("manual.db")).unwrap();

    match restore(&dir, &target, true) {
        Err(Error::Symlink { .. }) => {}
        other => panic!("expected a symlink refusal, got {other:?}"),
    }
    assert_eq!(fs::read(target.join("elsewhere.db")).unwrap(), b"away");
}

#[test]
fn a_write_failure_errors_loudly_and_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let manual = make_database(&data, "manual.db", &[("2026-01-01", 70.5)]);
    let dir = create(&tmp.path().join("backups"), &[Source::new("manual.db", manual)], "f").expect("create");

    // A disk-full equivalent: the temporary file name is occupied by a
    // directory, so the copy cannot write and must surface an error.
    let target = tmp.path().join("restore-target");
    fs::create_dir_all(target.join(".manual.db.restoring")).unwrap();
    assert!(restore(&dir, &target, true).is_err(), "the failure is not silent");
    assert!(!target.join("manual.db").exists(), "no database was replaced");
}

#[test]
fn restored_files_are_owner_only() {
    let tmp = TempDir::new().unwrap();
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let manual = make_database(&data, "manual.db", &[("2026-01-01", 70.5)]);
    let dir = create(&tmp.path().join("backups"), &[Source::new("manual.db", manual)], "p").expect("create");

    let target = tmp.path().join("restore-target");
    restore(&dir, &target, false).expect("restore");

    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(target.join("manual.db")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}
