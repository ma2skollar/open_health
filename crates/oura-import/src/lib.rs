//! Import historical manual metrics into `manual.db`.
//!
//! The framework is source-agnostic: an [`Importer`] turns a file into candidate
//! rows without touching the database, and the engine decides what would change
//! ([`dry_run`]) or writes it ([`apply`]) with full provenance.
//!
//! Rules this crate exists to enforce:
//!
//! - A missing cell is **not** zero: it is reported, never written.
//! - An ambiguous or unparseable value is rejected with a reason, never coerced.
//! - Overlapping step sources are never summed: an explicit precedence decides,
//!   and the decision is reported.
//! - Re-running the same import is a no-op and creates no second batch.
//! - Every write belongs to a batch, so a rollback can undo exactly what an
//!   import created without destroying later edits.

#![forbid(unsafe_code)]

mod csv;
mod engine;
mod error;
mod pulshealth;

pub use csv::{Columns, CsvImporter};
pub use engine::{apply, dry_run};
pub use pulshealth::PulsHealthImporter;
pub use error::{Error, Result};

use std::path::Path;

use oura_manual::MetricKind;

/// One value an importer wants to write.
///
/// `line` is the 1-based source line, so a rejection report can point at it. It is
/// `0` for a row that did not come from a line-oriented source.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateRow {
    pub line: usize,
    pub local_date: String,
    pub kind: MetricKind,
    pub value: f64,
    pub note: Option<String>,
}

/// A row the importer refused, with the reason it was refused.
#[derive(Debug, Clone, PartialEq)]
pub struct RejectedRow {
    pub line: usize,
    pub raw: String,
    pub reason: String,
}

/// What an importer read from a file. Nothing has been written.
#[derive(Debug, Clone, Default)]
pub struct ParseOutcome {
    pub rows: Vec<CandidateRow>,
    pub rejected: Vec<RejectedRow>,
}

impl ParseOutcome {
    /// Collapse duplicate `(local_date, kind)` candidates: the **last** wins and
    /// every earlier one is reported as rejected rather than silently dropped.
    pub fn dedup(&mut self) {
        use std::collections::HashMap;

        let mut last: HashMap<(String, MetricKind), usize> = HashMap::new();
        for (i, row) in self.rows.iter().enumerate() {
            last.insert((row.local_date.clone(), row.kind), i);
        }

        let mut superseded = Vec::new();
        let mut kept = Vec::new();
        for (i, row) in self.rows.iter().enumerate() {
            let key = (row.local_date.clone(), row.kind);
            if last.get(&key) == Some(&i) {
                kept.push(row.clone());
            } else {
                superseded.push(RejectedRow {
                    line: row.line,
                    raw: format!("{} {}", row.local_date, row.kind),
                    reason: format!(
                        "duplicate {} row for {}; the later row wins",
                        row.kind, row.local_date
                    ),
                });
            }
        }
        self.rows = kept;
        self.rejected.extend(superseded);
    }
}

/// A source of historical rows.
pub trait Importer {
    /// Stable source name. Stored on every row and used for step precedence.
    fn name(&self) -> &str;
    /// Adapter version, so a later re-import can be recognised.
    fn version(&self) -> &str;
    /// Read the file into candidates. Must not touch the database.
    fn parse(&self, input: &Path) -> Result<ParseOutcome>;
}

/// How a candidate differs from what is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Nothing is stored for this `(local_date, kind)`.
    New,
    /// The stored value differs.
    Changed,
}

/// One planned write, with what it replaces.
#[derive(Debug, Clone, PartialEq)]
pub struct DryRunRow {
    pub line: usize,
    pub local_date: String,
    pub kind: MetricKind,
    pub change: Change,
    pub old: Option<f64>,
    pub new: f64,
}

/// A step row that was not written because another source already owns that day.
#[derive(Debug, Clone, PartialEq)]
pub struct SkippedStep {
    pub local_date: String,
    pub skipped_source: String,
    pub kept_source: String,
    pub kept_value: f64,
}

/// What [`dry_run`] found. Writing this report changes nothing.
#[derive(Debug, Clone, Default)]
pub struct DryRunReport {
    pub rows: Vec<DryRunRow>,
    /// Candidates that already match what is stored.
    pub unchanged: usize,
    pub rejected: Vec<RejectedRow>,
    pub skipped_step_sources: Vec<SkippedStep>,
}

/// Options shared by [`dry_run`] and [`apply`].
#[derive(Debug, Clone, Default)]
pub struct ApplyOptions {
    /// Free-text note stored on the batch.
    pub note: Option<String>,
    /// Step sources in descending authority. A day already owned by a source
    /// listed **earlier** is left alone; nothing is ever summed.
    pub step_precedence: Vec<String>,
}

/// What [`apply`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct ApplySummary {
    /// `None` when the import changed nothing and therefore created no batch.
    pub batch_id: Option<i64>,
    pub inserted: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub rejected: usize,
    pub skipped_step_sources: usize,
}
