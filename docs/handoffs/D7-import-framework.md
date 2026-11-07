# Handoff D7 — historical import framework

## Objective

Build the import framework that brings the existing daily calorie / weight /
step history into `manual.db` — dry run, explicit mapping, rejected-row report,
rerun safety, batch provenance, and a rollback that preserves later edits.

The **source adapter** is deliberately not part of this task: the export format
has not been supplied yet. Build the framework plus one adapter against a
synthetic CSV fixture that the framework's own tests define.

## Context (decided — read before starting)

- `crates/oura-manual` owns `manual.db`; see `docs/DECISIONS/0003`. Read
  `crates/oura-manual/src/lib.rs` for the current API.
- Canonical units: calories **kcal**, weight **kg**. Dates are explicit local
  calendar dates (`YYYY-MM-DD`). Missing is not zero.
- Imported iPhone steps must be preserved even if ongoing iPhone collection is
  later removed. Steps therefore need a home in `manual.db` as a third metric
  kind, with an explicit precedence rule between step sources.
- Never guess ambiguous dates or units. An ambiguous or unparseable row is
  **rejected and reported**, never coerced.

## Files you own

`crates/oura-import/**` (new crate), `crates/oura-manual/**` (additive changes
only — the API D3 uses must keep working), the workspace `members` array in the
root `Cargo.toml`, `docs/TESTING.md`, `docs/OPERATIONS.md` (only the import
section), and fixtures under `crates/oura-import/tests/fixtures/**`.

Do not edit `crates/oura-cli/**` — wiring the import into a CLI subcommand is a
separate task.

## Part 1 — `oura-manual`, additive

1. Add `MetricKind::Steps` with the database spelling `steps`. Unit is a count.
   Validation: finite, `>= 0`, `<= 200000` per day.
2. Add the batch tables from ADR 0003, created with `IF NOT EXISTS` under the
   existing `SCHEMA_VERSION` (nothing is deployed yet, so `IF NOT EXISTS` is the
   migration):

```sql
CREATE TABLE IF NOT EXISTS import_batches (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    source          TEXT    NOT NULL,
    adapter_version TEXT    NOT NULL,
    imported_at     INTEGER NOT NULL,
    row_count       INTEGER NOT NULL,
    accepted        INTEGER NOT NULL,
    rejected        INTEGER NOT NULL,
    note            TEXT
);
CREATE TABLE IF NOT EXISTS import_provenance (
    batch_id   INTEGER NOT NULL REFERENCES import_batches(id),
    local_date TEXT    NOT NULL,
    kind       TEXT    NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (batch_id, local_date, kind)
);
```

3. Add:
   - `create_batch(&self, batch: &ImportBatch) -> Result<i64>`
   - `get_batch(&self, id: i64) -> Result<Option<ImportBatch>>`
   - `list_batches(&self) -> Result<Vec<ImportBatch>>`
   - `record_provenance(&self, batch_id: i64, entries: &[ManualEntry]) -> Result<()>`
   - `rollback_batch(&self, batch_id: i64) -> Result<RollbackReport>`
   - `batch_entries(&self, batch_id: i64) -> Result<Vec<ManualEntry>>`

   `RollbackReport { removed: usize, kept_edited_after_import: usize, missing: usize }`.

4. **Rollback rule (the point of the design).** For each provenance row, delete
   the entry only when the stored `updated_at` still equals the recorded
   `updated_at`. If it differs, the user edited that entry after the import:
   keep it and count it in `kept_edited_after_import`. If it is gone, count it in
   `missing`. Delete the batch's provenance rows and the batch row; other batches
   are untouched. The whole rollback is one transaction.

## Part 2 — `oura-import`

A source-agnostic framework:

```rust
pub trait Importer {
    fn name(&self) -> &str;
    fn version(&self) -> &str;
    /// Parse the source into candidate rows without touching the database.
    fn parse(&self, input: &Path) -> Result<ParseOutcome>;
}

pub struct ParseOutcome { pub rows: Vec<CandidateRow>, pub rejected: Vec<RejectedRow> }
pub struct CandidateRow { pub local_date: String, pub kind: MetricKind, pub value: f64, pub note: Option<String> }
pub struct RejectedRow { pub line: usize, pub raw: String, pub reason: String }
```

- `dry_run(importer, input, &ManualStore) -> Result<DryRunReport>` with, per
  (date, kind): `new`, `same`, `changed` (old → new), and the full rejected-row
  list. Dry run must not write anything.
- `apply(importer, input, &ManualStore, &ApplyOptions) -> Result<ApplySummary>`
  creating one batch, writing entries, recording provenance, and returning the
  counts. `ApplyOptions { note: Option<String> }`.
- Blank/`-`/`NA` cells in a numeric column are **missing**, not zero: the row is
  skipped for that metric and reported, never written as `0`.
- Overlapping step sources: the framework never sums two sources for one day.
  `ApplyOptions` takes an explicit `step_precedence: Vec<String>`; for a day
  already written by `steps` with a source listed earlier, the later source is
  skipped and reported. State this in the dry-run report.
- Rerun safety: importing the same file twice produces `same` for every row,
  writes nothing new, and creates no second batch.

## Acceptance / required evidence

`cargo test -p oura-import -p oura-manual` passes, covering:

1. parse → dry run → apply round trip on a synthetic fixture
2. dry run writes nothing (row count and `updated_at` unchanged)
3. re-running the same import is a no-op and creates no extra batch
4. a rejected row is reported with line number and reason and is not written
5. missing cells are skipped, never written as zero
6. rollback removes the batch's rows and reports nothing edited since
7. rollback **keeps** entries the user edited after the import and counts them
8. rollback of one batch leaves another batch's rows intact
9. two step sources on one day respect `step_precedence` and are not summed
10. the additive `oura-manual` changes did not break the existing 11 tests

Run in Docker:

```sh
MSYS_NO_PATHCONV=1 docker run --rm -v "<this worktree>:/w" \
  -v oura-cargo:/usr/local/cargo/registry -v oura-target:/w/target \
  -w /w rust:1-bookworm bash -lc \
  "apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq build-essential pkg-config libsqlite3-dev libdbus-1-dev && cargo test -p oura-import -p oura-manual"
```

Report: files added or changed, the exact command, the raw test tail, and any
requirement you could not meet.

## Out of scope

A real source adapter for the user's export (blocked on an anonymized sample),
any CLI or HTTP wiring, scheduling, and changes to `crates/oura-cli/**`.
