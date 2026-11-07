//! Acceptance tests for import provenance and rollback.
//!
//! The property under test: a rollback undoes exactly what the import wrote and
//! never destroys a correction the user made afterwards.

use oura_manual::{
    Error, ImportBatch, ManualEntry, ManualStore, MetricKind, RollbackReport, UpsertOutcome,
    ValidationError,
};

const T0: i64 = 1_700_000_000;

fn entry(local_date: &str, kind: MetricKind, value: f64, updated_at: i64) -> ManualEntry {
    ManualEntry {
        id: 0,
        local_date: local_date.to_string(),
        kind,
        value,
        note: None,
        source: "import:test".to_string(),
        batch_id: None,
        created_at: T0,
        updated_at,
    }
}

fn batch(source: &str) -> ImportBatch {
    ImportBatch {
        id: 0,
        source: source.to_string(),
        adapter_version: "1".to_string(),
        imported_at: T0,
        row_count: 3,
        accepted: 2,
        rejected: 1,
        note: Some("fixture".to_string()),
    }
}

/// Write entries and record them under a new batch, the way the import framework
/// does: write first, then record what was actually stored.
fn import(store: &ManualStore, source: &str, rows: &[(ManualEntry, f64)]) -> i64 {
    let id = store.create_batch(&batch(source)).expect("create batch");
    let mut written = Vec::new();
    for (template, value) in rows {
        let mut e = template.clone();
        e.value = *value;
        e.source = source.to_string();
        e.batch_id = Some(id);
        store.upsert_entry(&e).expect("upsert");
        written.push(
            store
                .get_entry(&e.local_date, e.kind)
                .expect("read back")
                .expect("row exists"),
        );
    }
    store.record_provenance(id, &written).expect("provenance");
    id
}

#[test]
fn batch_round_trips_and_lists_newest_first() {
    let store = ManualStore::open_in_memory().unwrap();
    let first = store.create_batch(&batch("a")).unwrap();
    let mut second = batch("b");
    second.imported_at = T0 + 60;
    let second = store.create_batch(&second).unwrap();
    assert!(first > 0 && second > first, "ids are assigned and increasing");

    let read = store.get_batch(first).unwrap().expect("batch exists");
    assert_eq!(read.source, "a");
    assert_eq!(read.adapter_version, "1");
    assert_eq!(read.row_count, 3);
    assert_eq!(read.accepted, 2);
    assert_eq!(read.rejected, 1);
    assert_eq!(read.note.as_deref(), Some("fixture"));

    let listed = store.list_batches().unwrap();
    assert_eq!(
        listed.iter().map(|b| b.source.as_str()).collect::<Vec<_>>(),
        vec!["b", "a"],
        "newest import first"
    );
    assert_eq!(store.get_batch(9999).unwrap(), None);
}

#[test]
fn batch_entries_reports_exactly_what_the_batch_wrote() {
    let store = ManualStore::open_in_memory().unwrap();
    store
        .upsert_entry(&entry("2026-01-01", MetricKind::CaloriesEaten, 100.0, T0))
        .unwrap(); // not from any batch

    let id = import(
        &store,
        "a",
        &[
            (entry("2026-01-02", MetricKind::CaloriesEaten, 0.0, T0), 1900.0),
            (entry("2026-01-02", MetricKind::WeightKg, 0.0, T0), 70.0),
        ],
    );

    let touched = store.batch_entries(id).unwrap();
    assert_eq!(touched.len(), 2);
    assert_eq!(
        touched
            .iter()
            .map(|e| (e.local_date.as_str(), e.kind))
            .collect::<Vec<_>>(),
        vec![
            ("2026-01-02", MetricKind::CaloriesEaten),
            ("2026-01-02", MetricKind::WeightKg),
        ],
        "the hand-written row must not appear"
    );
}

#[test]
fn rollback_removes_untouched_entries_and_the_batch() {
    let store = ManualStore::open_in_memory().unwrap();
    let id = import(
        &store,
        "a",
        &[
            (entry("2026-01-02", MetricKind::CaloriesEaten, 0.0, T0), 1900.0),
            (entry("2026-01-03", MetricKind::WeightKg, 0.0, T0), 70.0),
        ],
    );

    let report = store.rollback_batch(id).unwrap();
    assert_eq!(
        report,
        RollbackReport {
            removed: 2,
            kept_edited_after_import: 0,
            kept_not_created_by_this_batch: 0,
            missing: 0
        }
    );
    assert!(store.export_all().unwrap().is_empty());
    assert_eq!(store.get_batch(id).unwrap(), None, "the batch row is gone");
    assert!(store.batch_entries(id).unwrap().is_empty());
}

#[test]
fn rollback_keeps_an_entry_edited_after_the_import() {
    let store = ManualStore::open_in_memory().unwrap();
    let id = import(
        &store,
        "a",
        &[
            (entry("2026-01-02", MetricKind::CaloriesEaten, 0.0, T0), 1900.0),
            (entry("2026-01-03", MetricKind::WeightKg, 0.0, T0), 70.0),
        ],
    );

    // The user corrects the calorie total after the import.
    store
        .upsert_entry(&entry("2026-01-02", MetricKind::CaloriesEaten, 1950.0, T0 + 500))
        .unwrap();

    let report = store.rollback_batch(id).unwrap();
    assert_eq!(report.removed, 1);
    assert_eq!(report.kept_edited_after_import, 1);
    assert_eq!(report.missing, 0);

    let kept = store
        .get_entry("2026-01-02", MetricKind::CaloriesEaten)
        .unwrap()
        .expect("the corrected row survives");
    assert_eq!(kept.value, 1950.0);
    assert_eq!(kept.updated_at, T0 + 500);
    assert!(
        store
            .get_entry("2026-01-03", MetricKind::WeightKg)
            .unwrap()
            .is_none(),
        "the untouched row was rolled back"
    );
}

#[test]
fn rollback_leaves_other_batches_alone() {
    let store = ManualStore::open_in_memory().unwrap();
    let first = import(
        &store,
        "a",
        &[(entry("2026-01-02", MetricKind::CaloriesEaten, 0.0, T0), 1900.0)],
    );
    let second = import(
        &store,
        "b",
        &[(entry("2026-01-03", MetricKind::WeightKg, 0.0, T0), 70.0)],
    );

    let report = store.rollback_batch(first).unwrap();
    assert_eq!(report.removed, 1);

    assert!(store
        .get_entry("2026-01-02", MetricKind::CaloriesEaten)
        .unwrap()
        .is_none());
    assert_eq!(
        store
            .get_entry("2026-01-03", MetricKind::WeightKg)
            .unwrap()
            .expect("other batch survives")
            .value,
        70.0
    );
    assert_eq!(store.batch_entries(second).unwrap().len(), 1);
}

#[test]
fn rollback_counts_entries_that_disappeared() {
    let store = ManualStore::open_in_memory().unwrap();
    let id = import(
        &store,
        "a",
        &[(entry("2026-01-02", MetricKind::CaloriesEaten, 0.0, T0), 1900.0)],
    );
    store
        .delete_entry("2026-01-02", MetricKind::CaloriesEaten)
        .unwrap();

    let report = store.rollback_batch(id).unwrap();
    assert_eq!(
        report,
        RollbackReport {
            removed: 0,
            kept_edited_after_import: 0,
            kept_not_created_by_this_batch: 0,
            missing: 1
        }
    );
}

#[test]
fn a_batch_with_no_provenance_rolls_back_to_nothing() {
    let store = ManualStore::open_in_memory().unwrap();
    store
        .upsert_entry(&entry("2026-01-02", MetricKind::CaloriesEaten, 1900.0, T0))
        .unwrap();
    let id = store.create_batch(&batch("empty")).unwrap();

    let report = store.rollback_batch(id).unwrap();
    assert_eq!(report, RollbackReport::default());
    assert_eq!(
        store.export_all().unwrap().len(),
        1,
        "an unrelated entry is untouched"
    );
}

#[test]
fn rollback_keeps_a_row_the_import_only_overwrote() {
    let store = ManualStore::open_in_memory().unwrap();
    // A row the user entered by hand: no batch, source "manual".
    let mut hand = entry("2026-01-02", MetricKind::WeightKg, 71.0, T0);
    hand.source = "manual".to_string();
    store.upsert_entry(&hand).unwrap();

    // An import replaces its value.
    let id = import(
        &store,
        "scale-csv",
        &[(entry("2026-01-02", MetricKind::WeightKg, 0.0, T0 + 10), 70.4)],
    );
    assert_eq!(
        store
            .get_entry("2026-01-02", MetricKind::WeightKg)
            .unwrap()
            .unwrap()
            .value,
        70.4
    );

    let report = store.rollback_batch(id).unwrap();
    assert_eq!(
        report,
        RollbackReport {
            removed: 0,
            kept_edited_after_import: 0,
            kept_not_created_by_this_batch: 1,
            missing: 0
        },
        "a rollback must not delete a row the import merely replaced"
    );
    let kept = store
        .get_entry("2026-01-02", MetricKind::WeightKg)
        .unwrap()
        .expect("the overwritten row survives");
    assert_eq!(kept.value, 70.4);
    assert_eq!(kept.source, "manual", "the original provenance is preserved");
}

#[test]
fn steps_entries_round_trip_and_validate() {
    let store = ManualStore::open_in_memory().unwrap();

    let mut zero = entry("2026-01-02", MetricKind::Steps, 0.0, T0);
    assert_eq!(
        store.upsert_entry(&zero).unwrap(),
        UpsertOutcome::Inserted,
        "zero steps is a real value, not a missing one"
    );
    assert_eq!(
        store
            .get_entry("2026-01-02", MetricKind::Steps)
            .unwrap()
            .expect("row exists")
            .value,
        0.0
    );

    zero.value = 12_345.0;
    assert_eq!(store.upsert_entry(&zero).unwrap(), UpsertOutcome::Updated);
    assert_eq!(
        store.get_entry("2026-01-02", MetricKind::Steps).unwrap().unwrap().value,
        12_345.0
    );

    zero.value = 200_001.0;
    match store.upsert_entry(&zero) {
        Err(Error::Validation(ValidationError::StepsOutOfRange { value })) => {
            assert_eq!(value, 200_001.0)
        }
        other => panic!("expected a steps range error, got {other:?}"),
    }
    assert_eq!(
        store.get_entry("2026-01-02", MetricKind::Steps).unwrap().unwrap().value,
        12_345.0,
        "a rejected write leaves the previous value in place"
    );

    // The three kinds coexist on one day.
    store
        .upsert_entry(&entry("2026-01-02", MetricKind::CaloriesEaten, 1800.0, T0))
        .unwrap();
    store
        .upsert_entry(&entry("2026-01-02", MetricKind::WeightKg, 70.0, T0))
        .unwrap();
    assert_eq!(store.list_entries(None, None, None).unwrap().len(), 3);
}
