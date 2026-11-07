//! Entries and the [`MetricKind`] discriminator.

use serde::{Deserialize, Serialize};

/// Which manual measurement an entry holds. Canonical units: kcal, kg.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    /// Calories eaten that local day, in kcal.
    CaloriesEaten,
    /// Body weight that local day, in kg.
    WeightKg,
    /// Step count recorded for that local day. Imported history only: the ring's
    /// own step figure is *estimated* from MET bands and lives with the ring data.
    Steps,
}

impl MetricKind {
    /// Stable spelling used in the database and in metadata.
    pub fn as_str(self) -> &'static str {
        match self {
            MetricKind::CaloriesEaten => "calories_eaten",
            MetricKind::WeightKg => "weight_kg",
            MetricKind::Steps => "steps",
        }
    }

    /// Parse the database spelling. `None` for anything else.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "calories_eaten" => Some(MetricKind::CaloriesEaten),
            "weight_kg" => Some(MetricKind::WeightKg),
            "steps" => Some(MetricKind::Steps),
            _ => None,
        }
    }
}

impl std::fmt::Display for MetricKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One manual measurement for a local calendar day.
///
/// `id` is assigned by the store and ignored on writes; it is `0` for entries
/// not yet read back.
#[derive(Debug, Clone, PartialEq)]
pub struct ManualEntry {
    pub id: i64,
    /// Local calendar date, `YYYY-MM-DD`.
    pub local_date: String,
    pub kind: MetricKind,
    /// kcal for [`MetricKind::CaloriesEaten`], kg for [`MetricKind::WeightKg`].
    pub value: f64,
    pub note: Option<String>,
    /// Provenance, e.g. `manual` or `import:<adapter>`.
    pub source: String,
    pub batch_id: Option<i64>,
    /// Unix seconds.
    pub created_at: i64,
    pub updated_at: i64,
}

/// Result of [`crate::ManualStore::upsert_entry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
    /// No row existed for `(local_date, kind)`; a new row was written.
    Inserted,
    /// A row existed; it was replaced in place.
    Updated,
}
