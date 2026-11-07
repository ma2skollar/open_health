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
