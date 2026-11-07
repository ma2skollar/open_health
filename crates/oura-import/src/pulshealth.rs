//! Adapter for the PulsHealth long-format export
//! (`timestamp,metric,value,unit,source_bundle_id`).
//!
//! Unlike the wide [`crate::CsvImporter`], this source emits one row per sample.
//! Steps arrive as many per-day samples and must be summed; calories and weight
//! are one-per-day. The adapter aggregates before returning candidates so the
//! engine's `(local_date, kind)` uniqueness rules behave as designed.
//!
//! A one-time historical import only. The ring takes over steps afterwards, so
//! the imported source is deliberately named so precedence can outrank it.

use std::collections::HashMap;
use std::path::Path;

use oura_manual::MetricKind;

use crate::error::{Error, Result};
use crate::{CandidateRow, Importer, ParseOutcome, RejectedRow};

/// Header names in the PulsHealth export.
const TIMESTAMP: &str = "timestamp";
const METRIC: &str = "metric";
const VALUE: &str = "value";
const UNIT: &str = "unit";

/// HealthKit identifiers mapped to the manual metric kinds.
const STEP_COUNT: &str = "HKQuantityTypeIdentifierStepCount";
const DIETARY_ENERGY: &str = "HKQuantityTypeIdentifierDietaryEnergyConsumed";
const BODY_MASS: &str = "HKQuantityTypeIdentifierBodyMass";

/// Imports the PulsHealth long-format CSV, aggregating per-sample steps per day.
#[derive(Debug, Clone)]
pub struct PulsHealthImporter {
    name: String,
    version: String,
}

impl PulsHealthImporter {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
        }
    }
}

/// Pull the calendar date (`YYYY-MM-DD`) out of the leading timestamp text.
/// The export writes `YYYY-MM-DD HH:MM:SS[.ffffff]+00`; only the date matters.
fn local_date(timestamp: &str) -> Option<&str> {
    let date = timestamp.split(' ').next()?.trim();
    // Reject anything that is not the expected 10-character ISO date.
    if date.len() != 10 {
        return None;
    }
    let bytes = date.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    Some(date)
}

fn kind_for(metric: &str, unit: &str) -> Option<MetricKind> {
    match (metric, unit) {
        (STEP_COUNT, "count") => Some(MetricKind::Steps),
        (DIETARY_ENERGY, "kcal") => Some(MetricKind::CaloriesEaten),
        (BODY_MASS, "kg") => Some(MetricKind::WeightKg),
        _ => None,
    }
}

/// Per-day accumulator for one metric kind.
#[derive(Default)]
struct DayAccumulator {
    /// Steps are summed; calories/weight keep the last sample.
    steps: f64,
    steps_seen: bool,
    /// Last value seen for non-step kinds.
    last: f64,
    last_line: usize,
}

impl Importer for PulsHealthImporter {
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
        let timestamp_idx = column_index(TIMESTAMP).ok_or_else(|| Error::MissingColumn {
            column: TIMESTAMP.to_string(),
        })?;
        let metric_idx = column_index(METRIC).ok_or_else(|| Error::MissingColumn {
            column: METRIC.to_string(),
        })?;
        let value_idx = column_index(VALUE).ok_or_else(|| Error::MissingColumn {
            column: VALUE.to_string(),
        })?;
        let unit_idx = column_index(UNIT).ok_or_else(|| Error::MissingColumn {
            column: UNIT.to_string(),
        })?;

        let mut out = ParseOutcome::default();
        // (local_date, kind) -> accumulator. Steps sum; others keep the last.
        let mut days: HashMap<(String, MetricKind), DayAccumulator> = HashMap::new();
        // Track the first line that contributed to each day for the report.
        let mut first_line: HashMap<(String, MetricKind), usize> = HashMap::new();

        for (n, record) in reader.records().enumerate() {
            let record = record?;
            let line = record
                .position()
                .map(|p| p.line() as usize)
                .unwrap_or(n + 2);
            let raw = record.iter().collect::<Vec<_>>().join(",");

            let timestamp = record.get(timestamp_idx).unwrap_or("").trim();
            let metric = record.get(metric_idx).unwrap_or("").trim();
            let unit = record.get(unit_idx).unwrap_or("").trim();
            let value_text = record.get(value_idx).unwrap_or("").trim();

            let date = match local_date(timestamp) {
                Some(date) => date.to_string(),
                None => {
                    out.rejected.push(RejectedRow {
                        line,
                        raw,
                        reason: format!("timestamp {timestamp:?} has no leading YYYY-MM-DD date"),
                    });
                    continue;
                }
            };

            let kind = match kind_for(metric, unit) {
                Some(kind) => kind,
                None => {
                    out.rejected.push(RejectedRow {
                        line,
                        raw,
                        reason: format!(
                            "metric {metric:?} with unit {unit:?} is not an imported metric"
                        ),
                    });
                    continue;
                }
            };

            let value = match value_text.parse::<f64>() {
                Ok(value) if value.is_finite() => value,
                Ok(_) => {
                    out.rejected.push(RejectedRow {
                        line,
                        raw,
                        reason: format!("value {value_text:?} is not finite"),
                    });
                    continue;
                }
                Err(_) => {
                    out.rejected.push(RejectedRow {
                        line,
                        raw,
                        reason: format!("value {value_text:?} is not a number"),
                    });
                    continue;
                }
            };

            let key = (date, kind);
            first_line.entry(key.clone()).or_insert(line);
            let slot = days.entry(key).or_default();
            if kind == MetricKind::Steps {
                slot.steps += value;
                slot.steps_seen = true;
            } else {
                slot.last = value;
                slot.last_line = line;
            }
        }

        for ((date, kind), slot) in days {
            let value = if kind == MetricKind::Steps {
                if !slot.steps_seen {
                    continue;
                }
                slot.steps
            } else {
                slot.last
            };
            out.rows.push(CandidateRow {
                line: first_line.get(&(date.clone(), kind)).copied().unwrap_or(0),
                local_date: date,
                kind,
                value,
                note: Some("PulsHealth export".to_string()),
            });
        }

        Ok(out)
    }
}
