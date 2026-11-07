//! A delimited (CSV) importer with an explicit column mapping.
//!
//! The mapping is configuration, not inference: every column is named by the
//! caller, so nothing is guessed from a header that happens to look familiar.

use std::path::Path;

use oura_manual::MetricKind;

use crate::error::{Error, Result};
use crate::{CandidateRow, Importer, ParseOutcome, RejectedRow};

/// Which header names hold which values. A `None` metric is simply not imported.
#[derive(Debug, Clone)]
pub struct Columns {
    /// Column holding the local calendar date (`YYYY-MM-DD`).
    pub date: String,
    pub calories: Option<String>,
    pub weight_kg: Option<String>,
    pub steps: Option<String>,
    /// Optional free-text note copied onto every row from the same line.
    pub note: Option<String>,
}

/// Imports any delimited file through an explicit header mapping.
#[derive(Debug, Clone)]
pub struct CsvImporter {
    name: String,
    version: String,
    columns: Columns,
}

impl CsvImporter {
    pub fn new(name: impl Into<String>, version: impl Into<String>, columns: Columns) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            columns,
        }
    }
}

/// Cells that mean "no measurement here". They are reported, never stored as 0.
fn is_missing(cell: &str) -> bool {
    let cell = cell.trim();
    cell.is_empty()
        || matches!(
            cell.to_ascii_lowercase().as_str(),
            "-" | "na" | "n/a" | "null" | "nil" | "none"
        )
}

impl Importer for CsvImporter {
    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> &str {
        &self.version
    }

    fn parse(&self, input: &Path) -> Result<ParseOutcome> {
        if !input.is_file() {
            return Err(Error::Read {
                path: input.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no such file or directory",
                ),
            });
        }

        let mut reader = csv::ReaderBuilder::new()
            .flexible(true)
            .trim(csv::Trim::All)
            .from_path(input)?;
        let header = reader.headers()?.clone();

        let column_index = |name: &str| header.iter().position(|h| h.eq_ignore_ascii_case(name));
        let date_idx = column_index(&self.columns.date).ok_or_else(|| Error::MissingColumn {
            column: self.columns.date.clone(),
        })?;

        let mut metrics: Vec<(usize, MetricKind)> = Vec::new();
        for (column, kind) in [
            (&self.columns.calories, MetricKind::CaloriesEaten),
            (&self.columns.weight_kg, MetricKind::WeightKg),
            (&self.columns.steps, MetricKind::Steps),
        ] {
            if let Some(column) = column {
                let idx = column_index(column).ok_or_else(|| Error::MissingColumn {
                    column: column.clone(),
                })?;
                metrics.push((idx, kind));
            }
        }
        if metrics.is_empty() {
            return Err(Error::NoMetricColumns);
        }
        let note_idx = self.columns.note.as_deref().and_then(column_index);

        let mut out = ParseOutcome::default();
        for (n, record) in reader.records().enumerate() {
            let record = record?;
            let line = record
                .position()
                .map(|p| p.line() as usize)
                .unwrap_or(n + 2);
            let raw = record.iter().collect::<Vec<_>>().join(",");

            let date = record.get(date_idx).unwrap_or("").trim().to_string();
            if date.is_empty() {
                out.rejected.push(RejectedRow {
                    line,
                    raw,
                    reason: "missing date".to_string(),
                });
                continue;
            }

            let note = note_idx
                .and_then(|i| record.get(i))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);

            for (idx, kind) in &metrics {
                let cell = record.get(*idx).unwrap_or("").trim();
                if is_missing(cell) {
                    out.rejected.push(RejectedRow {
                        line,
                        raw: raw.clone(),
                        reason: format!("no value for {kind}; left empty rather than stored as 0"),
                    });
                    continue;
                }
                match cell.parse::<f64>() {
                    Ok(value) => out.rows.push(CandidateRow {
                        line,
                        local_date: date.clone(),
                        kind: *kind,
                        value,
                        note: note.clone(),
                    }),
                    Err(_) => out.rejected.push(RejectedRow {
                        line,
                        raw: raw.clone(),
                        reason: format!("{kind} value {cell:?} is not a number"),
                    }),
                }
            }
        }
        Ok(out)
    }
}
