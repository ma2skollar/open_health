//! Planning and writing. Nothing here touches the database before the decision
//! is made, and everything written belongs to one batch.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use oura_manual::{validate_entry, ImportBatch, ManualEntry, ManualStore, MetricKind};

use crate::error::Result;
use crate::{
    ApplyOptions, ApplySummary, CandidateRow, Change, DryRunReport, DryRunRow, Importer,
    RejectedRow, SkippedStep,
};

/// The decision for one parse: what to write, what already matches, what was
/// refused, and which step rows another source already owns.
struct Evaluation {
    writes: Vec<CandidateRow>,
    unchanged: usize,
    skipped: Vec<SkippedStep>,
    rejected: Vec<RejectedRow>,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Position of `source` in the precedence list; `usize::MAX` when unlisted.
fn rank(precedence: &[String], source: &str) -> usize {
    precedence
        .iter()
        .position(|s| s == source)
        .unwrap_or(usize::MAX)
}

fn evaluate(
    rows: &[CandidateRow],
    rejected: Vec<RejectedRow>,
    source: &str,
    store: &ManualStore,
    opts: &ApplyOptions,
) -> Result<Evaluation> {
    let mut out = Evaluation {
        writes: Vec::new(),
        unchanged: 0,
        skipped: Vec::new(),
        rejected,
    };

    for row in rows {
        // Validate here, so a bad row is reported rather than aborting the run.
        let probe = ManualEntry {
            id: 0,
            local_date: row.local_date.clone(),
            kind: row.kind,
            value: row.value,
            note: row.note.clone(),
            source: source.to_string(),
            batch_id: None,
            created_at: 0,
            updated_at: 0,
        };
        if let Err(e) = validate_entry(&probe) {
            out.rejected.push(RejectedRow {
                line: row.line,
                raw: format!("{} {} {}", row.local_date, row.kind, row.value),
                reason: e.to_string(),
            });
            continue;
        }

        let existing = store.get_entry(&row.local_date, row.kind)?;

        // Overlapping step sources: never sum, never silently overwrite a source
        // with more authority. The decision is reported either way.
        if row.kind == MetricKind::Steps {
            if let Some(entry) = &existing {
                if entry.source != source {
                    let incoming = rank(&opts.step_precedence, source);
                    let held = rank(&opts.step_precedence, &entry.source);
                    if held <= incoming {
                        out.skipped.push(SkippedStep {
                            local_date: row.local_date.clone(),
                            skipped_source: source.to_string(),
                            kept_source: entry.source.clone(),
                            kept_value: entry.value,
                        });
                        continue;
                    }
                }
            }
        }

        match existing {
            Some(entry) if entry.value == row.value => out.unchanged += 1,
            _ => out.writes.push(row.clone()),
        }
    }

    Ok(out)
}

/// Read the input and report what an import would change. Writes nothing.
pub fn dry_run(
    importer: &dyn Importer,
    input: &Path,
    store: &ManualStore,
    opts: &ApplyOptions,
) -> Result<DryRunReport> {
    let mut parsed = importer.parse(input)?;
    parsed.dedup();
    let source = importer.name().to_string();
    let evaluation = evaluate(&parsed.rows, parsed.rejected, &source, store, opts)?;

    let mut rows = Vec::with_capacity(evaluation.writes.len());
    for write in &evaluation.writes {
        let old = store.get_entry(&write.local_date, write.kind)?.map(|e| e.value);
        rows.push(DryRunRow {
            line: write.line,
            local_date: write.local_date.clone(),
            kind: write.kind,
            change: if old.is_some() {
                Change::Changed
            } else {
                Change::New
            },
            old,
            new: write.value,
        });
    }

    Ok(DryRunReport {
        rows,
        unchanged: evaluation.unchanged,
        rejected: evaluation.rejected,
        skipped_step_sources: evaluation.skipped,
    })
}

/// Write the input, recording one batch. A run that changes nothing creates no
/// batch, so re-importing the same file is a true no-op.
pub fn apply(
    importer: &dyn Importer,
    input: &Path,
    store: &ManualStore,
    opts: &ApplyOptions,
) -> Result<ApplySummary> {
    let mut parsed = importer.parse(input)?;
    parsed.dedup();
    let source = importer.name().to_string();
    let evaluation = evaluate(&parsed.rows, parsed.rejected, &source, store, opts)?;

    if evaluation.writes.is_empty() {
        return Ok(ApplySummary {
            batch_id: None,
            inserted: 0,
            updated: 0,
            unchanged: evaluation.unchanged,
            rejected: evaluation.rejected.len(),
            skipped_step_sources: evaluation.skipped.len(),
        });
    }

    let now = now_unix();
    let batch_id = store.create_batch(&ImportBatch {
        id: 0,
        source: source.clone(),
        adapter_version: importer.version().to_string(),
        imported_at: now,
        row_count: (parsed.rows.len() + evaluation.rejected.len()) as i64,
        accepted: evaluation.writes.len() as i64,
        rejected: evaluation.rejected.len() as i64,
        note: opts.note.clone(),
    })?;

    let mut inserted = 0;
    let mut updated = 0;
    let mut written = Vec::with_capacity(evaluation.writes.len());
    for write in &evaluation.writes {
        let existed = store.get_entry(&write.local_date, write.kind)?.is_some();
        store.upsert_entry(&ManualEntry {
            id: 0,
            local_date: write.local_date.clone(),
            kind: write.kind,
            value: write.value,
            note: write.note.clone(),
            source: source.clone(),
            batch_id: Some(batch_id),
            created_at: now,
            updated_at: now,
        })?;
        if existed {
            updated += 1;
        } else {
            inserted += 1;
        }
        // Record what was actually stored, so a rollback compares against the
        // real value rather than the value we intended.
        if let Some(stored) = store.get_entry(&write.local_date, write.kind)? {
            written.push(stored);
        }
    }
    store.record_provenance(batch_id, &written)?;

    Ok(ApplySummary {
        batch_id: Some(batch_id),
        inserted,
        updated,
        unchanged: evaluation.unchanged,
        rejected: evaluation.rejected.len(),
        skipped_step_sources: evaluation.skipped.len(),
    })
}
