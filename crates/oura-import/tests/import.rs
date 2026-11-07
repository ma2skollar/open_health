//! Acceptance tests for the import framework.

use std::path::PathBuf;

use oura_import::{apply, dry_run, ApplyOptions, Change, Columns, CsvImporter, Importer};
use oura_manual::{ManualEntry, ManualStore, MetricKind};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn history_importer() -> CsvImporter {
    CsvImporter::new(
        "history-csv",
        "1",
        Columns {
            date: "date".to_string(),
            calories: Some("calories".to_string()),
            weight_kg: Some("weight_kg".to_string()),
            steps: Some("steps".to_string()),
            note: Some("note".to_string()),
        },
    )
}

fn steps_importer(name: &str) -> CsvImporter {
    CsvImporter::new(
        name,
        "1",
        Columns {
            date: "date".to_string(),
            calories: None,
            weight_kg: None,
            steps: Some("steps".to_string()),
            note: None,
        },
    )
}

fn manual_entry(local_date: &str, kind: MetricKind, value: f64, updated_at: i64) -> ManualEntry {
    ManualEntry {
        id: 0,
        local_date: local_date.to_string(),
        kind,
        value,
        note: None,
        source: "manual".to_string(),
        batch_id: None,
        created_at: 1,
        updated_at,
    }
}

#[test]
fn parsing_reports_candidates_and_refusals() {
    let outcome = history_importer().parse(&fixture("history.csv")).unwrap();

    // 2026-01-01 has three values; 01-02 has no calories; 01-03 has neither
    // weight nor steps; 01-04 has an unparseable calorie cell; 01-32 is a date
    // the parser passes through (the engine refuses it) but still has three cells.
    assert_eq!(outcome.rows.len(), 11);

    // 01-02 calories, 01-03 weight, 01-03 steps, 01-04 calories.
    assert_eq!(outcome.rejected.len(), 4);
    assert!(
        outcome
            .rejected
            .iter()
            .any(|r| r.reason.contains("is not a number")),
        "an unparseable cell is reported as such"
    );
    assert!(
        outcome
            .rejected
            .iter()
            .any(|r| r.reason.contains("rather than stored as 0")),
        "a missing cell is reported, not coerced to zero"
    );
    assert!(
        outcome.rejected.iter().all(|r| r.line >= 2),
        "refusals carry a source line"
    );
}

#[test]
fn dry_run_reports_new_rows_and_writes_nothing() {
    let store = ManualStore::open_in_memory().unwrap();
    let report = dry_run(
        &history_importer(),
        &fixture("history.csv"),
        &store,
        &ApplyOptions::default(),
    )
    .unwrap();

    assert_eq!(report.rows.len(), 8, "the impossible date is not planned");
    assert!(report.rows.iter().all(|r| r.change == Change::New));
    assert_eq!(report.unchanged, 0);
    assert_eq!(report.rejected.len(), 7, "4 refusals plus 3 rows for 2026-01-32");
    assert!(
        report
            .rejected
            .iter()
            .any(|r| r.reason.contains("2026-01-32")),
        "the impossible date is named"
    );
    assert!(store.export_all().unwrap().is_empty(), "a dry run writes nothing");
    assert!(
        store.list_batches().unwrap().is_empty(),
        "a dry run creates no batch"
    );
}

#[test]
fn apply_then_reimport_is_a_no_op() {
    let store = ManualStore::open_in_memory().unwrap();
    let first = apply(
        &history_importer(),
        &fixture("history.csv"),
        &store,
        &ApplyOptions::default(),
    )
    .unwrap();
    assert_eq!(first.inserted, 8);
    assert_eq!(first.updated, 0);
    assert_eq!(first.rejected, 7);
    assert_eq!(first.batch_id, Some(1));

    let second = apply(
        &history_importer(),
        &fixture("history.csv"),
        &store,
        &ApplyOptions::default(),
    )
    .unwrap();
    assert_eq!(second.batch_id, None, "a no-op import creates no batch");
    assert_eq!(second.unchanged, 8);
    assert_eq!(second.inserted + second.updated, 0);
    assert_eq!(store.list_batches().unwrap().len(), 1);
}

#[test]
fn refused_rows_are_never_written() {
    let store = ManualStore::open_in_memory().unwrap();
    apply(
        &history_importer(),
        &fixture("history.csv"),
        &store,
        &ApplyOptions::default(),
    )
    .unwrap();

    assert!(
        store
            .get_entry("2026-01-32", MetricKind::CaloriesEaten)
            .unwrap()
            .is_none(),
        "an impossible date is refused"
    );
    assert!(
        store
            .get_entry("2026-01-02", MetricKind::CaloriesEaten)
            .unwrap()
            .is_none(),
        "a blank cell is not a zero"
    );
    assert!(
        store
            .get_entry("2026-01-03", MetricKind::WeightKg)
            .unwrap()
            .is_none(),
        "'-' is not a zero"
    );
    assert!(
        store
            .get_entry("2026-01-03", MetricKind::Steps)
            .unwrap()
            .is_none(),
        "'NA' is not a zero"
    );

    // The cells that did carry values are stored.
    assert_eq!(
        store
            .get_entry("2026-01-01", MetricKind::CaloriesEaten)
            .unwrap()
            .unwrap()
            .value,
        2100.0
    );
    assert_eq!(
        store
            .get_entry("2026-01-01", MetricKind::Steps)
            .unwrap()
            .unwrap()
            .value,
        8500.0
    );
    assert_eq!(
        store
            .get_entry("2026-01-03", MetricKind::CaloriesEaten)
            .unwrap()
            .unwrap()
            .value,
        1850.0
    );
}

#[test]
fn dry_run_reports_changed_rows_against_stored_values() {
    let store = ManualStore::open_in_memory().unwrap();
    apply(
        &history_importer(),
        &fixture("history.csv"),
        &store,
        &ApplyOptions::default(),
    )
    .unwrap();
    store
        .upsert_entry(&manual_entry(
            "2026-01-01",
            MetricKind::CaloriesEaten,
            999.0,
            5,
        ))
        .unwrap();

    let report = dry_run(
        &history_importer(),
        &fixture("history.csv"),
        &store,
        &ApplyOptions::default(),
    )
    .unwrap();

    let changed: Vec<_> = report
        .rows
        .iter()
        .filter(|r| r.change == Change::Changed)
        .collect();
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].local_date, "2026-01-01");
    assert_eq!(changed[0].old, Some(999.0));
    assert_eq!(changed[0].new, 2100.0);
    assert_eq!(report.unchanged, 7);
}

#[test]
fn rollback_undoes_the_import_and_keeps_a_later_edit() {
    let store = ManualStore::open_in_memory().unwrap();
    let summary = apply(
        &history_importer(),
        &fixture("history.csv"),
        &store,
        &ApplyOptions::default(),
    )
    .unwrap();
    let batch = summary.batch_id.expect("the import wrote rows");

    // A correction made after the import.
    store
        .upsert_entry(&manual_entry(
            "2026-01-01",
            MetricKind::CaloriesEaten,
            2200.0,
            2,
        ))
        .unwrap();

    let report = store.rollback_batch(batch).unwrap();
    assert_eq!(report.removed, 7, "everything the import created goes");
    assert_eq!(report.kept_edited_after_import, 1, "the correction stays");
    assert_eq!(report.kept_not_created_by_this_batch, 0);
    assert_eq!(report.missing, 0);

    let kept = store
        .get_entry("2026-01-01", MetricKind::CaloriesEaten)
        .unwrap()
        .expect("the corrected row survives");
    assert_eq!(kept.value, 2200.0);
    assert_eq!(
        kept.source, "history-csv",
        "upsert preserves the provenance of the first write; only the value changes"
    );
    assert!(
        store
            .get_entry("2026-01-01", MetricKind::WeightKg)
            .unwrap()
            .is_none(),
        "an imported row with no later edit is rolled back"
    );
}

#[test]
fn step_sources_respect_precedence_and_are_never_summed() {
    let phone = steps_importer("phone-export");
    let watch = steps_importer("watch-export");

    // The phone is the authority.
    let store = ManualStore::open_in_memory().unwrap();
    let opts = ApplyOptions {
        note: None,
        step_precedence: vec!["phone-export".to_string(), "watch-export".to_string()],
    };
    let first = apply(&phone, &fixture("steps_phone.csv"), &store, &opts).unwrap();
    assert_eq!(first.inserted, 2);

    let second = apply(&watch, &fixture("steps_watch.csv"), &store, &opts).unwrap();
    assert_eq!(second.skipped_step_sources, 1, "2026-02-01 is already owned");
    assert_eq!(second.batch_id, None, "nothing was written");
    assert_eq!(
        store
            .get_entry("2026-02-01", MetricKind::Steps)
            .unwrap()
            .unwrap()
            .value,
        8000.0,
        "the two sources are never summed"
    );
    assert_eq!(
        store
            .get_entry("2026-02-01", MetricKind::Steps)
            .unwrap()
            .unwrap()
            .source,
        "phone-export"
    );

    // Reverse the authority and the watch takes the day.
    let store2 = ManualStore::open_in_memory().unwrap();
    let opts2 = ApplyOptions {
        note: None,
        step_precedence: vec!["watch-export".to_string(), "phone-export".to_string()],
    };
    apply(&phone, &fixture("steps_phone.csv"), &store2, &opts2).unwrap();
    let third = apply(&watch, &fixture("steps_watch.csv"), &store2, &opts2).unwrap();
    assert_eq!(third.updated, 1, "the watch now owns 2026-02-01");
    assert_eq!(third.skipped_step_sources, 0);
    assert_eq!(
        store2
            .get_entry("2026-02-01", MetricKind::Steps)
            .unwrap()
            .unwrap()
            .value,
        12000.0
    );
}

#[test]
fn an_unmapped_column_is_an_error_not_a_guess() {
    let store = ManualStore::open_in_memory().unwrap();
    let importer = CsvImporter::new(
        "guessing-is-not-allowed",
        "1",
        Columns {
            date: "date".to_string(),
            calories: Some("kcal".to_string()),
            weight_kg: None,
            steps: None,
            note: None,
        },
    );
    let err = dry_run(
        &importer,
        &fixture("steps_phone.csv"),
        &store,
        &ApplyOptions::default(),
    )
    .expect_err("the header has no kcal column");
    assert!(err.to_string().contains("kcal"), "{err}");
}

#[test]
fn a_missing_file_is_reported_with_its_path() {
    let store = ManualStore::open_in_memory().unwrap();
    let err = dry_run(
        &history_importer(),
        &fixture("no-such-file.csv"),
        &store,
        &ApplyOptions::default(),
    )
    .expect_err("the file does not exist");
    assert!(err.to_string().contains("no-such-file.csv"), "{err}");
}
