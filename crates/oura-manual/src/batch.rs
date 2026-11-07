//! Import batch provenance.
//!
//! An import writes entries and then records, per `(local_date, kind)`, the
//! `updated_at` it left behind. A rollback deletes a row only while it still
//! holds that value — if the user edited the entry afterwards, the edit wins and
//! the row is kept. Without this record a rollback could not tell an imported
//! value from a later correction.

/// One import run: who produced it, how much it wrote, and when.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportBatch {
    /// Assigned by the store; `0` before the batch is created.
    pub id: i64,
    /// Source name, e.g. `myfitnesspal-csv`.
    pub source: String,
    /// Adapter version, so a later re-import can be recognised.
    pub adapter_version: String,
    /// Unix seconds.
    pub imported_at: i64,
    /// Rows seen in the source.
    pub row_count: i64,
    /// Rows written.
    pub accepted: i64,
    /// Rows refused, with reasons reported separately by the caller.
    pub rejected: i64,
    pub note: Option<String>,
}

/// What a rollback actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RollbackReport {
    /// Entries deleted: created by this batch and unchanged since.
    pub removed: usize,
    /// Entries kept because they were edited after the import.
    pub kept_edited_after_import: usize,
    /// Entries kept because they existed before the batch and the batch only
    /// overwrote them. A rollback must not delete data the import merely replaced.
    pub kept_not_created_by_this_batch: usize,
    /// Provenance rows whose entry no longer exists.
    pub missing: usize,
}
