//! Application-owned store for **manual** metric records: calories eaten (kcal)
//! and body weight (kg).
//!
//! These records live in their own SQLite database (`manual.db`), never inside
//! the replaceable ring database, so ring snapshot replacement, re-pairing or
//! collector re-provisioning can never destroy manual history. See
//! `docs/DECISIONS/0003-separate-manual-metric-store.md`.
//!
//! Invariants:
//!
//! - Canonical units are kcal and kg. Display conversion is a separate concern.
//! - `local_date` is an explicit local calendar date (`YYYY-MM-DD`, `TEXT`).
//!   This crate never does timezone math.
//! - Timestamps are Unix seconds (`INTEGER`).
//! - A missing value is *not* zero: an absent row reads back as [`None`].
//! - One value per `(local_date, kind)`; [`ManualStore::upsert_entry`] replaces
//!   the measurement and keeps the provenance of the original row.

#![forbid(unsafe_code)]

mod batch;
mod error;
mod model;
mod store;
mod validate;

pub use batch::{ImportBatch, RollbackReport};
pub use error::{Error, Result, ValidationError};
pub use model::{ManualEntry, MetricKind, UpsertOutcome};
pub use store::{ManualStore, SCHEMA_VERSION};
pub use validate::validate_entry;
