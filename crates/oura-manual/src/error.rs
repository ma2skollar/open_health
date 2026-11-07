//! Typed errors. Nothing in this crate panics on bad input.

use std::fmt;

use crate::MetricKind;

/// Result alias for every fallible operation in this crate. The error type can
/// be overridden for callers that want to reuse this alias precisely.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Why a well-formed entry was still refused. The store never clamps silently.
#[derive(Debug, Clone, PartialEq)]
pub enum ValidationError {
    /// `value` was NaN or infinite.
    ValueNotFinite { kind: MetricKind },
    /// `value` was negative.
    ValueNegative { kind: MetricKind, value: f64 },
    /// Weight must be `> 0` and `<= 500` kg.
    WeightOutOfRange { value: f64 },
    /// Calories eaten must be `<= 20000` kcal.
    CaloriesOutOfRange { value: f64 },
    /// A day's step count must be `<= 200000`.
    StepsOutOfRange { value: f64 },
    /// `local_date` is not a real calendar date in `YYYY-MM-DD` form.
    InvalidLocalDate { value: String },
    /// `note` exceeds the character limit.
    NoteTooLong { len: usize, max: usize },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidationError::ValueNotFinite { kind } => {
                write!(f, "{kind} value must be a finite number")
            }
            ValidationError::ValueNegative { kind, value } => {
                write!(f, "{kind} value must not be negative (got {value})")
            }
            ValidationError::WeightOutOfRange { value } => write!(
                f,
                "weight must be greater than 0 kg and at most 500 kg (got {value})"
            ),
            ValidationError::CaloriesOutOfRange { value } => write!(
                f,
                "calories eaten must be at most 20000 kcal (got {value})"
            ),
            ValidationError::StepsOutOfRange { value } => {
                write!(f, "steps must be at most 200000 for one day (got {value})")
            }
            ValidationError::InvalidLocalDate { value } => write!(
                f,
                "local_date {value:?} is not a real calendar date in YYYY-MM-DD form"
            ),
            ValidationError::NoteTooLong { len, max } => {
                write!(f, "note is {len} characters long, the maximum is {max}")
            }
        }
    }
}

impl std::error::Error for ValidationError {}

/// Every failure mode of this crate.
#[derive(Debug)]
pub enum Error {
    /// The entry was refused; nothing was written.
    Validation(ValidationError),
    /// The database was written by a newer version of this crate.
    SchemaTooNew { found: i64, supported: i64 },
    /// The database contains data this crate cannot interpret.
    CorruptData(String),
    /// Connection, pragma, or file-permission failure.
    Storage(String),
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Validation(e) => write!(f, "invalid manual entry: {e}"),
            Error::SchemaTooNew { found, supported } => write!(
                f,
                "manual database schema version {found} is newer than the supported version {supported}; upgrade the application"
            ),
            Error::CorruptData(msg) => write!(f, "manual database is corrupt: {msg}"),
            Error::Storage(msg) => write!(f, "manual store error: {msg}"),
            Error::Sqlite(e) => write!(f, "sqlite error: {e}"),
            Error::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Validation(e) => Some(e),
            Error::Sqlite(e) => Some(e),
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Sqlite(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
