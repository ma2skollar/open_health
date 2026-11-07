//! Entry validation and local calendar date checking.

use crate::error::{Error, Result, ValidationError};
use crate::{ManualEntry, MetricKind};

/// Maximum number of characters accepted in `note`.
pub(crate) const MAX_NOTE_CHARS: usize = 2000;
/// Maximum body weight in kg.
pub(crate) const MAX_WEIGHT_KG: f64 = 500.0;
/// Maximum calories eaten in kcal.
pub(crate) const MAX_CALORIES_KCAL: f64 = 20_000.0;
/// Maximum steps accepted for one local day.
pub(crate) const MAX_STEPS: f64 = 200_000.0;

/// Refuse an entry that would store nonsense. Called before any write, so a
/// rejected entry leaves the database untouched. Public so an importer can report
/// a bad row instead of aborting on it.
pub fn validate_entry(entry: &ManualEntry) -> Result<()> {
    if !entry.value.is_finite() {
        return Err(invalid(ValidationError::ValueNotFinite { kind: entry.kind }));
    }
    if entry.value < 0.0 {
        return Err(invalid(ValidationError::ValueNegative {
            kind: entry.kind,
            value: entry.value,
        }));
    }
    match entry.kind {
        MetricKind::WeightKg if entry.value <= 0.0 || entry.value > MAX_WEIGHT_KG => {
            return Err(invalid(ValidationError::WeightOutOfRange {
                value: entry.value,
            }));
        }
        MetricKind::CaloriesEaten if entry.value > MAX_CALORIES_KCAL => {
            return Err(invalid(ValidationError::CaloriesOutOfRange {
                value: entry.value,
            }));
        }
        MetricKind::Steps if entry.value > MAX_STEPS => {
            return Err(invalid(ValidationError::StepsOutOfRange {
                value: entry.value,
            }));
        }
        _ => {}
    }
    if !is_valid_local_date(&entry.local_date) {
        return Err(invalid(ValidationError::InvalidLocalDate {
            value: entry.local_date.clone(),
        }));
    }
    if let Some(note) = entry.note.as_deref() {
        let len = note.chars().count();
        if len > MAX_NOTE_CHARS {
            return Err(invalid(ValidationError::NoteTooLong {
                len,
                max: MAX_NOTE_CHARS,
            }));
        }
    }
    Ok(())
}

fn invalid(e: ValidationError) -> Error {
    Error::Validation(e)
}

/// `true` only for a real calendar date written exactly as `YYYY-MM-DD`.
pub(crate) fn is_valid_local_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10 {
        return false;
    }
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    if !(digits(0..4) && digits(5..7) && digits(8..10)) {
        return false;
    }
    // Safe: the slices are four/two ASCII digits, checked above.
    let (Some(year), Some(month), Some(day)) = (
        value[0..4].parse::<i32>().ok(),
        value[5..7].parse::<u32>().ok(),
        value[8..10].parse::<u32>().ok(),
    ) else {
        return false;
    };
    if !(1..=12).contains(&month) {
        return false;
    }
    (1..=days_in_month(year, month)).contains(&day)
}

/// Gregorian days in `month` of `year`. `0` for an out-of-range month.
fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_real_dates_and_rejects_the_rest() {
        for ok in [
            "2026-11-07",
            "2024-02-29",
            "2000-02-29",
            "1996-12-31",
            "2023-04-30",
        ] {
            assert!(is_valid_local_date(ok), "{ok} should be valid");
        }
        for bad in [
            "2023-02-29",
            "1900-02-29",
            "2026-13-01",
            "2026-00-10",
            "2026-04-31",
            "2026-01-32",
            "2026-01-00",
            "2026-1-07",
            "26-01-07",
            "2026/01/07",
            "2026-01-07 ",
            "",
        ] {
            assert!(!is_valid_local_date(bad), "{bad:?} should be invalid");
        }
    }
}
