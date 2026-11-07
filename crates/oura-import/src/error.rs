//! Errors for the import framework.

use std::path::PathBuf;

/// Result alias for every fallible operation in this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("malformed delimited input: {0}")]
    Csv(#[from] csv::Error),
    #[error("column {column:?} is not in the header and cannot be mapped")]
    MissingColumn { column: String },
    #[error("no metric columns were configured; nothing could be imported")]
    NoMetricColumns,
    #[error("manual store: {0}")]
    Store(#[from] oura_manual::Error),
}
