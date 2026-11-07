//! Acceptance tests for the manual metric store.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use oura_manual::{Error, ManualEntry, ManualStore, MetricKind, UpsertOutcome, ValidationError};

const T0: i64 = 1_700_000_000;
const T1: i64 = 1_700_003_600;

fn entry(local_date: &str, kind: MetricKind, value: f64) -> ManualEntry {
    ManualEntry {
        id: 0,
        local_date: local_date.to_string(),
        kind,
        value,
        note: None,
        source: "manual".to_string(),
        batch_id: None,
        created_at: T0,
        updated_at: T0,
    }
}

/// Temp directory that cleans itself up, so file-backed tests leave no residue.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let path = std::env::temp_dir().join(format!(
            "oura-manual-test-{}-{nanos}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }

    fn db(&self) -> PathBuf {
        self.path.join("manual.db")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// 1. insert then read back; field round-trip
#[test]
fn insert_then_read_back_round_trips_every_field() {
    let store = ManualStore::open_in_memory().unwrap();
    let mut new_entry = entry("2026-11-07", MetricKind::CaloriesEaten, 2140.5);
    new_entry.note = Some("big lunch".to_string());
    new_entry.source = "import:myfitnesspal".to_string();
    new_entry.batch_id = Some(7);
    new_entry.created_at = T0;
    new_entry.updated_at = T1;

    assert_eq!(store.upsert_entry(&new_entry).unwrap(), UpsertOutcome::Inserted);

    let read = store
        .get_entry("2026-11-07", MetricKind::CaloriesEaten)
        .unwrap()
        .expect("row exists");
    assert!(read.id > 0, "id is assigned by the store");
    assert_eq!(read.local_date, "2026-11-07");
    assert_eq!(read.kind, MetricKind::CaloriesEaten);
    assert_eq!(read.value, 2140.5);
    assert_eq!(read.note.as_deref(), Some("big lunch"));
    assert_eq!(read.source, "import:myfitnesspal");
    assert_eq!(read.batch_id, Some(7));
    assert_eq!(read.created_at, T0);
    assert_eq!(read.updated_at, T1);
}

// 2. absent row returns None, not 0.0
#[test]
fn absent_row_is_none_not_zero() {
    let store = ManualStore::open_in_memory().unwrap();
    store
        .upsert_entry(&entry("2026-11-07", MetricKind::WeightKg, 71.4))
        .unwrap();

    let missing = store.get_entry("2026-11-06", MetricKind::WeightKg).unwrap();
    assert_eq!(missing, None);
    assert!(
        store.get_entry("2026-11-07", MetricKind::CaloriesEaten).unwrap().is_none(),
        "same day, other kind is also absent"
    );
    assert!(store.list_entries(Some("2026-11-06"), Some("2026-11-06"), None).unwrap().is_empty());
    // The stored day is untouched by the misses.
    assert_eq!(
        store.get_entry("2026-11-07", MetricKind::WeightKg).unwrap().unwrap().value,
        71.4
    );
}

// 3. calorie and weight entries coexist on the same date: independent read/edit/delete
#[test]
fn calorie_and_weight_entries_coexist_on_one_date() {
    let store = ManualStore::open_in_memory().unwrap();
    let day = "2026-11-07";
    store.upsert_entry(&entry(day, MetricKind::CaloriesEaten, 1800.0)).unwrap();
    store.upsert_entry(&entry(day, MetricKind::WeightKg, 70.2)).unwrap();

    assert_eq!(store.get_entry(day, MetricKind::CaloriesEaten).unwrap().unwrap().value, 1800.0);
    assert_eq!(store.get_entry(day, MetricKind::WeightKg).unwrap().unwrap().value, 70.2);

    let mut edited = entry(day, MetricKind::WeightKg, 69.9);
    edited.updated_at = T1;
    assert_eq!(store.upsert_entry(&edited).unwrap(), UpsertOutcome::Updated);
    assert_eq!(store.get_entry(day, MetricKind::WeightKg).unwrap().unwrap().value, 69.9);
    assert_eq!(
        store.get_entry(day, MetricKind::CaloriesEaten).unwrap().unwrap().value,
        1800.0,
        "editing weight must not touch the calorie row"
    );

    assert!(store.delete_entry(day, MetricKind::CaloriesEaten).unwrap());
    assert_eq!(store.get_entry(day, MetricKind::CaloriesEaten).unwrap(), None);
    assert_eq!(store.get_entry(day, MetricKind::WeightKg).unwrap().unwrap().value, 69.9);
}

// 4. second upsert reports Updated and keeps created_at/source
#[test]
fn duplicate_day_upsert_updates_and_keeps_provenance() {
    let store = ManualStore::open_in_memory().unwrap();
    let day = "2026-11-07";
    let mut first = entry(day, MetricKind::CaloriesEaten, 1800.0);
    first.source = "import:cronometer".to_string();
    first.batch_id = Some(42);
    first.created_at = T0;
    first.updated_at = T0;
    assert_eq!(store.upsert_entry(&first).unwrap(), UpsertOutcome::Inserted);

    let second = ManualEntry {
        value: 1950.0,
        note: Some("second helping".to_string()),
        source: "manual".to_string(),
        batch_id: None,
        created_at: T1,
        updated_at: T1,
        ..entry(day, MetricKind::CaloriesEaten, 1950.0)
    };
    assert_eq!(store.upsert_entry(&second).unwrap(), UpsertOutcome::Updated);

    let rows = store.export_all().unwrap();
    assert_eq!(rows.len(), 1, "duplicate day must not add a row");
    let row = &rows[0];
    assert_eq!(row.value, 1950.0);
    assert_eq!(row.note.as_deref(), Some("second helping"));
    assert_eq!(row.updated_at, T1);
    assert_eq!(row.created_at, T0, "created_at of the original row survives");
    assert_eq!(row.source, "import:cronometer", "source of the original row survives");
    assert_eq!(row.batch_id, Some(42), "batch_id of the original row survives");
}

// 5. every validation rejection errors and writes nothing
#[test]
fn invalid_entries_are_rejected_without_writing() {
    let store = ManualStore::open_in_memory().unwrap();
    let long_note = "n".repeat(2001);

    let cases: Vec<(&str, ManualEntry, fn(&Error) -> bool)> = vec![
        (
            "NaN calories",
            entry("2026-11-07", MetricKind::CaloriesEaten, f64::NAN),
            |e| matches!(e, Error::Validation(ValidationError::ValueNotFinite { .. })),
        ),
        (
            "infinite weight",
            entry("2026-11-07", MetricKind::WeightKg, f64::INFINITY),
            |e| matches!(e, Error::Validation(ValidationError::ValueNotFinite { .. })),
        ),
        (
            "negative calories",
            entry("2026-11-07", MetricKind::CaloriesEaten, -1.0),
            |e| matches!(e, Error::Validation(ValidationError::ValueNegative { .. })),
        ),
        (
            "negative weight",
            entry("2026-11-07", MetricKind::WeightKg, -0.5),
            |e| matches!(e, Error::Validation(ValidationError::ValueNegative { .. })),
        ),
        (
            "zero weight",
            entry("2026-11-07", MetricKind::WeightKg, 0.0),
            |e| matches!(e, Error::Validation(ValidationError::WeightOutOfRange { .. })),
        ),
        (
            "weight above 500 kg",
            entry("2026-11-07", MetricKind::WeightKg, 500.1),
            |e| matches!(e, Error::Validation(ValidationError::WeightOutOfRange { .. })),
        ),
        (
            "calories above 20000 kcal",
            entry("2026-11-07", MetricKind::CaloriesEaten, 20_000.5),
            |e| matches!(e, Error::Validation(ValidationError::CaloriesOutOfRange { .. })),
        ),
        (
            "month 13",
            entry("2026-13-01", MetricKind::CaloriesEaten, 1800.0),
            |e| matches!(e, Error::Validation(ValidationError::InvalidLocalDate { .. })),
        ),
        (
            "non-leap 29 Feb",
            entry("2023-02-29", MetricKind::CaloriesEaten, 1800.0),
            |e| matches!(e, Error::Validation(ValidationError::InvalidLocalDate { .. })),
        ),
        (
            "31 April",
            entry("2026-04-31", MetricKind::CaloriesEaten, 1800.0),
            |e| matches!(e, Error::Validation(ValidationError::InvalidLocalDate { .. })),
        ),
        (
            "not zero padded",
            entry("2026-1-07", MetricKind::CaloriesEaten, 1800.0),
            |e| matches!(e, Error::Validation(ValidationError::InvalidLocalDate { .. })),
        ),
        (
            "note over 2000 characters",
            ManualEntry {
                note: Some(long_note.clone()),
                ..entry("2026-11-07", MetricKind::CaloriesEaten, 1800.0)
            },
            |e| matches!(e, Error::Validation(ValidationError::NoteTooLong { len: 2001, max: 2000 })),
        ),
    ];

    for (label, bad, matches_expected) in cases {
        let err = store
            .upsert_entry(&bad)
            .expect_err(&format!("{label} must be rejected"));
        assert!(matches_expected(&err), "{label}: unexpected error {err:?}");
    }

    assert!(
        store.export_all().unwrap().is_empty(),
        "no rejected entry may leave a row behind"
    );

    // Accepted boundaries, so the limits are not off by one.
    assert_eq!(
        store.upsert_entry(&entry("2026-11-07", MetricKind::WeightKg, 500.0)).unwrap(),
        UpsertOutcome::Inserted
    );
    assert_eq!(
        store.upsert_entry(&entry("2026-11-07", MetricKind::CaloriesEaten, 20_000.0)).unwrap(),
        UpsertOutcome::Inserted
    );
    assert_eq!(
        store.upsert_entry(&ManualEntry {
            note: Some("n".repeat(2000)),
            ..entry("2026-11-08", MetricKind::CaloriesEaten, 0.0)
        })
        .unwrap(),
        UpsertOutcome::Inserted
    );
    assert_eq!(store.export_all().unwrap().len(), 3);
}

// 6. list_entries range is inclusive on both ends and ordered
#[test]
fn list_range_is_inclusive_and_ordered() {
    let store = ManualStore::open_in_memory().unwrap();
    store.upsert_entry(&entry("2026-01-01", MetricKind::CaloriesEaten, 1900.0)).unwrap();
    store.upsert_entry(&entry("2026-01-02", MetricKind::WeightKg, 70.0)).unwrap();
    store.upsert_entry(&entry("2026-01-02", MetricKind::CaloriesEaten, 2000.0)).unwrap();
    store.upsert_entry(&entry("2026-01-03", MetricKind::CaloriesEaten, 2100.0)).unwrap();

    let all = store.list_entries(None, None, None).unwrap();
    assert_eq!(
        all.iter().map(|e| (e.local_date.as_str(), e.kind)).collect::<Vec<_>>(),
        vec![
            ("2026-01-01", MetricKind::CaloriesEaten),
            ("2026-01-02", MetricKind::CaloriesEaten),
            ("2026-01-02", MetricKind::WeightKg),
            ("2026-01-03", MetricKind::CaloriesEaten),
        ]
    );

    let window = store.list_entries(Some("2026-01-01"), Some("2026-01-02"), None).unwrap();
    assert_eq!(window.len(), 3, "both ends are inclusive");
    assert!(window.iter().all(|e| e.local_date != "2026-01-03"));

    let single = store.list_entries(Some("2026-01-02"), Some("2026-01-02"), None).unwrap();
    assert_eq!(single.len(), 2, "a single-day window keeps both kinds");

    let from_only = store.list_entries(Some("2026-01-02"), None, None).unwrap();
    assert_eq!(from_only.len(), 3);
    let to_only = store.list_entries(None, Some("2026-01-02"), None).unwrap();
    assert_eq!(to_only.len(), 3);

    let calories = store
        .list_entries(Some("2026-01-01"), Some("2026-01-03"), Some(MetricKind::CaloriesEaten))
        .unwrap();
    assert_eq!(calories.len(), 3);
    assert!(calories.iter().all(|e| e.kind == MetricKind::CaloriesEaten));

    let empty = store.list_entries(Some("2027-01-01"), None, None).unwrap();
    assert!(empty.is_empty());
}

// 7. export_all returns every row
#[test]
fn export_all_returns_every_row() {
    let store = ManualStore::open_in_memory().unwrap();
    store.upsert_entry(&entry("2026-01-01", MetricKind::CaloriesEaten, 1900.0)).unwrap();
    store.upsert_entry(&entry("2026-01-02", MetricKind::WeightKg, 70.0)).unwrap();
    store.upsert_entry(&entry("2026-02-10", MetricKind::WeightKg, 69.0)).unwrap();

    let exported = store.export_all().unwrap();
    assert_eq!(exported.len(), 3);
    assert_eq!(
        exported.iter().map(|e| e.local_date.as_str()).collect::<Vec<_>>(),
        vec!["2026-01-01", "2026-01-02", "2026-02-10"]
    );
    assert_eq!(store.list_entries(None, None, None).unwrap(), exported);
}

// 8. a newer schema_version fails clearly and is left alone
#[test]
fn newer_schema_version_is_refused() {
    let dir = TempDir::new();
    let path = dir.db();
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE store_manual_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO store_manual_meta (key, value) VALUES ('schema_version', '2')",
            [],
        )
        .unwrap();
    }

    let err = match ManualStore::open(&path) {
        Ok(_) => panic!("opening a database at schema version 2 must fail"),
        Err(e) => e,
    };
    assert!(
        matches!(&err, Error::SchemaTooNew { found: 2, supported: 1 }),
        "unexpected error: {err:?}"
    );
    let message = err.to_string();
    assert!(message.contains('2') && message.contains('1'), "message names both versions: {message}");

    // Refusing must not have migrated or touched the newer database.
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        conn.query_row::<i64, _, _>(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='manual_entries'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row::<String, _, _>(
            "SELECT value FROM store_manual_meta WHERE key='schema_version'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        "2"
    );
}

// 9. file-backed store persists across reopen
#[test]
fn file_backed_store_persists_across_reopen() {
    let dir = TempDir::new();
    let path = dir.db();

    {
        let store = ManualStore::open(&path).unwrap();
        assert_eq!(
            store.upsert_entry(&entry("2026-11-07", MetricKind::WeightKg, 70.2)).unwrap(),
            UpsertOutcome::Inserted
        );
    }
    {
        let store = ManualStore::open(&path).unwrap();
        let row = store.get_entry("2026-11-07", MetricKind::WeightKg).unwrap().unwrap();
        assert_eq!(row.value, 70.2);
        assert_eq!(row.source, "manual");
        assert_eq!(
            store.upsert_entry(&entry("2026-11-07", MetricKind::WeightKg, 70.0)).unwrap(),
            UpsertOutcome::Updated,
            "the UNIQUE(local_date, kind) index survived the reopen"
        );
        assert_eq!(store.export_all().unwrap().len(), 1);
    }
}

#[cfg(unix)]
#[test]
fn database_file_is_created_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new();
    let path: &Path = &dir.db();
    let store = ManualStore::open(path).unwrap();
    store.upsert_entry(&entry("2026-11-07", MetricKind::WeightKg, 70.2)).unwrap();

    let mode = std::fs::metadata(path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "manual.db must be owner-only");
}
