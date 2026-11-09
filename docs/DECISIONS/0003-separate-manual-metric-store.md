# ADR 0003 — Manual and imported metrics live in an application-owned store

Status: accepted (2026-11-07)

## Context

Requirement: manual calorie and weight records must survive ring snapshot
replacement, ring re-pairing, and collector re-provisioning. Ring data arrives as
whole-database snapshots that are validated and activated as generations, so
anything stored inside the ring database is replaceable by design.

Upstream stores the user profile (age, sex, height, weight, ring size) in
`profile.json` beside `oura.db` (`dashboard/README.md`), not in SQLite, and it
holds only a single current weight.

## Decision

Manual and imported metrics get their own SQLite database, `manual.db`, owned by
the application and never replaced by ring ingestion. Ring snapshots are written
to separate, generation-addressed files.

Schema (kcal / kg canonical, explicit local calendar date, configurable IANA
timezone):

- `manual_entries(id, local_date, kind, value, note, created_at, updated_at,
  source, batch_id)` — `kind` in `calories_eaten|weight_kg`
- `import_batches(id, source, adapter_version, imported_at, row_count,
  accepted, rejected, note)`
- `settings(key, value)`

Rules: missing != zero; one value per (local_date, kind) with explicit duplicate
semantics; provenance on every write; no silent rewrite of history.

## Consequences

- Ring ingestion can never destroy manual history.
- Backups must cover `manual.db` and the active ring generation; restore order is
  documented in `OPERATIONS.md`.
- The relationship between `profile.json` weight and dated `weight_kg` entries is
  explicit: dated entries win for derived calculations; the profile value is a
  seed/default only.
  The web dashboard resolves the arithmetic mean of the latest 28 dated weight
  observations within the preceding 180 calendar days (including today, excluding
  future dates). Resolution uses canonical kg from the configured manual store,
  not rounded display values. This effective weight feeds the summary and models
  without replacing the saved fallback. Profile GET/POST expose its source and
  observation count even when the ring database is absent.
