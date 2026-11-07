# Handoff D1 — `crates/oura-manual`

## Objective

Add a new, self-contained Rust crate `crates/oura-manual`: the application-owned
store for **manual** calories-eaten and body-weight records. This is the storage
layer only; no HTTP, no UI, no import.

## Context (already decided — do not relitigate)

- This worktree is a fork of `Th0rgal/open_health` (Rust workspace, edition 2021,
  `resolver = "2"`). Root `Cargo.toml` lists `crates/*` members.
- Manual records live in their own SQLite database (`manual.db`), **never** inside
  the replaceable ring database. See `docs/DECISIONS/0003-separate-manual-metric-store.md`.
- Canonical units: calories in **kcal**, weight in **kg**. Display conversion is a
  later, separate concern.
- Dates are explicit **local calendar dates** stored as `TEXT` in `YYYY-MM-DD`
  form. No timezone math in this crate. Timestamps are Unix seconds as `INTEGER`.
- A missing value is **not** zero: an absent row returns `None`, never `0.0`.

## Files you may edit

- `crates/oura-manual/**` (create)
- the workspace `members` array in the root `Cargo.toml` (add one line)

Touch nothing else. `cargo` may update the root `Cargo.lock`; that is expected.

## Required API

```rust
pub enum MetricKind { CaloriesEaten, WeightKg }   // serde snake_case

pub struct ManualEntry {
    pub id: i64,
    pub local_date: String,      // YYYY-MM-DD
    pub kind: MetricKind,
    pub value: f64,              // kcal or kg
    pub note: Option<String>,
    pub source: String,          // e.g. "manual", "import:<adapter>"
    pub batch_id: Option<i64>,
    pub created_at: i64,         // unix seconds
    pub updated_at: i64,
}

pub struct ManualStore { /* holds a rusqlite::Connection */ }

impl ManualStore {
    pub fn open(path: &Path) -> Result<Self>;          // creates/migrates
    pub fn open_in_memory() -> Result<Self>;
    pub fn upsert_entry(&self, entry: &ManualEntry) -> Result<UpsertOutcome>;
    pub fn get_entry(&self, local_date: &str, kind: MetricKind) -> Result<Option<ManualEntry>>;
    pub fn list_entries(&self, from: Option<&str>, to: Option<&str>, kind: Option<MetricKind>)
        -> Result<Vec<ManualEntry>>;                   // inclusive range, ordered by date
    pub fn delete_entry(&self, local_date: &str, kind: MetricKind) -> Result<bool>;
    pub fn export_all(&self) -> Result<Vec<ManualEntry>>;
}

pub enum UpsertOutcome { Inserted, Updated }
```

## Schema requirements

- `manual_entries(id INTEGER PRIMARY KEY AUTOINCREMENT, local_date TEXT NOT NULL,
  kind TEXT NOT NULL, value REAL NOT NULL, note TEXT, source TEXT NOT NULL,
  batch_id INTEGER, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  UNIQUE(local_date, kind))`
- index `idx_manual_entries_date ON manual_entries(local_date)`
- `store_manual_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL)` holding
  `schema_version = 1`. Opening a database whose version is **newer** than this
  crate supports must fail with a clear, typed error naming both versions.
- Connection settings copied from `crates/oura-store/src/storage.rs`: WAL,
  `synchronous = FULL`, busy timeout 5000 ms, and on Unix the file is created
  with mode `0600` (create with the restrictive mode; if you cannot create it
  restrictively, chmod immediately **and** fail the open if chmod fails).

## Validation

Reject with a typed error (no panics, no silent clamping):

- `value` not finite, or negative.
- `MetricKind::WeightKg` with `value <= 0` or `> 500`.
- `MetricKind::CaloriesEaten` with `value > 20000`.
- `local_date` not exactly `YYYY-MM-DD`, or not a real calendar date
  (verify month-aware day validity, including leap years).
- `note` longer than 2000 characters.

## Duplicate-day semantics

`UNIQUE(local_date, kind)`. `upsert_entry` replaces `value`, `note` and
`updated_at`; it preserves `created_at`, `source` and `batch_id` of the existing
row; the returned outcome distinguishes `Inserted` from `Updated`.

## Acceptance / required evidence

Run everything **inside Docker** (the repo has no local Rust toolchain; Rust must
be ≥ 1.85 because of `clap_lex`):

```sh
MSYS_NO_PATHCONV=1 docker run --rm -v "<this worktree>:/w" \
  -v oura-cargo:/usr/local/cargo/registry -w /w rust:1-bookworm \
  bash -lc "cargo build -p oura-manual && cargo test -p oura-manual"
```

Required tests (`crates/oura-manual/tests/` or `#[cfg(test)]`):

1. insert then read back; field round-trip
2. absent row returns `None`, not `0.0`
3. calorie and weight entries coexist on the same date and are independently
   readable, editable and deletable
4. second upsert on the same (date, kind) returns `Updated`, keeps `created_at`
   and `source`, changes `value`
5. each validation rejection above returns an error and writes nothing
6. `list_entries` range filter is inclusive on both ends and ordered
7. `export_all` returns every row
8. opening a database with a newer `schema_version` fails clearly
9. reopening a file-backed store after a write returns the persisted row

Report exactly: files added, the exact command run, the raw tail of the test
output, and any deviation from this spec or any requirement you could not meet.

## Out of scope

No HTTP routes, no dashboard or web changes, no import framework, no unit
conversion, no timezone logic, no ring data, no changes to other crates.
