# Handoff D5 — collector ingestion: protocol, generations, continuity

## Objective

The server side of collector delivery: an authenticated upload endpoint, an
immutable generation store, structural validation of the uploaded SQLite
snapshot, safe activation, continuity protection, and the operational state the
dashboard will later show.

## Context (decided — read before starting)

- `docs/DECISIONS/0004-snapshot-delivery.md` fixes the delivery model: a
  consistent SQLite snapshot with metadata, never a naive file copy, never both a
  snapshot and an incremental format.
- `docs/ARCHITECTURE.md` freezes the v1 upload metadata and the server's order of
  operations. Implement that contract; if reality forces a change, change the
  contract in the doc and say so explicitly in your report.
- Ring storage keys are **not** part of a snapshot. Validation must fail a
  snapshot whose `device.mac` is populated **only** when you can also show the
  column carries secrets — otherwise leave device metadata alone and say why.
- The dashboard is a **separate process** and opens ring databases read-only.
  Activation must therefore never rewrite a database in place.

## Files you own

`crates/oura-protocol-v1/**` (new), `crates/oura-hub/**` (new), the workspace
`members` array in the root `Cargo.toml`, `docs/TESTING.md`,
`docs/ARCHITECTURE.md`, `docs/OPERATIONS.md` (only a new "ingestion" section).

Do not edit `crates/oura-cli/**`, `crates/oura-manual/**`, or `vendor/**`.

## Part 1 — `oura-protocol-v1`

Serde types shared by hub, collector and simulator. `protocol_version: 1`.
Rename nothing after this task; additive optional fields only.

```rust
pub struct UploadMetadata {
    pub protocol_version: u32,
    pub schema_version: i64,        // ring store schema version
    pub collector_id: String,
    pub collector_epoch: String,    // opaque, stable per provisioning
    pub generation: u64,            // monotonic within the epoch
    pub created_unix: i64,
    pub size_bytes: u64,
    pub sha256: String,             // lowercase hex
    pub build_revision: String,
    pub identity: Identity,         // Demo | Production
}
```

Plus `UploadAck { accepted: bool, generation: u64, active_generation: u64,
duplicate: bool, message: String }` and an error envelope with a machine-readable
`code`.

## Part 2 — `oura-hub`

Library first, thin binary second.

1. **Authenticate before any expensive work.** A collector token compared in
   constant time. Read it from a file whose path comes from the environment; the
   token is never logged, echoed, or written into a status response.
2. **Limits.** Reject over `max_bytes` (default 512 MiB) using the declared size
   *and* the actual bytes read; a wall-clock limit on the request; a rate limit
   per collector id.
3. **Staging.** Write to `staging/<collector_id>/<uuid>` with mode 0600 inside the
   data root only. Never trust a path from the request.
4. **Structural validation**, in this order, failing closed with a specific code:
   byte count equals `size_bytes`; SHA-256 matches; the file starts with the
   SQLite header magic; it opens read-only; `store_meta` exists; the ring-store
   tables exist; `store_meta` decoder/schema version matches `schema_version` and
   is one this build supports; `PRAGMA integrity_check` returns `ok`.
5. **Generation store.** `ring/<collector_id>/<epoch>/<generation>.db`, plus
   `active.json` holding the active and previous known-good generation. Activation
   writes the new pointer to a temp file and renames it, so no reader ever sees a
   half-written pointer, and no database file is ever replaced in place.
6. **Monotonicity.** Same collector and epoch: a generation `<= active` is
   rejected as stale **unless** it is byte-identical to the active one (duplicate
   delivery), which is a successful no-op with `duplicate: true`.
7. **Continuity.** A **new epoch** for a known collector id must not silently
   become active: this is the rebuilt-Pi / re-provisioned case, and partial
   history must never replace complete history. Stage it, record it as
   `pending_review`, keep the old epoch active, and require an explicit operator
   action (`oura-hub accept-epoch --collector <id> --epoch <epoch>`) to promote
   it. Refuse a snapshot whose earliest event is *later* than the active
   generation's earliest event unless that promotion flag is given.
8. **Idempotence.** Re-upload of an accepted generation is a no-op success;
   acknowledge only after the file is durable (fsync the file and its directory)
   and the pointer is committed. A crash between the two leaves an orphan file
   that a later run reuses or removes — never a pointer to a missing file.
9. **Operational state.** `hub-state.json`: per collector — last contact, last
   accepted upload (time, generation, bytes, identity), newest measurement
   timestamp, pending review flag, recent errors (bounded, redacted).
10. **HTTP surface**, loopback bind, no TLS (a proxy terminates):
    `POST /api/v1/collector/snapshot` (JSON metadata in a header or a leading
    length-prefixed block — pick one, document it, and keep it trivially
    parseable) and `GET /api/v1/hub/status`.

## Acceptance / required evidence

`cargo test -p oura-hub -p oura-protocol-v1` passes with these cases, each
deterministic and using synthetic fixtures only:

1. valid snapshot is accepted, activated, and readable at the returned generation
2. wrong `sha256` → rejected, nothing activated
3. `size_bytes` smaller or larger than the body → rejected
4. truncated file and a non-SQLite file → rejected with distinct codes
5. SQLite file with a missing `events` table → rejected
6. unsupported `schema_version` → rejected, message names both versions
7. `PRAGMA integrity_check` failure (corrupt page) → rejected
8. duplicate upload of the accepted generation → `duplicate: true`, no new file
9. stale generation → rejected, active generation unchanged
10. restart between staging and pointer commit → no pointer points at a missing
    file; a re-run converges
11. new epoch with earlier history → staged as `pending_review`, old epoch stays
    active, and promotion only happens through the explicit call
12. wrong or missing collector token → rejected before any file is written
13. over-limit body → rejected and nothing left in staging
14. status output contains no token, no key material, and no health payload

Run in Docker (`rust:1-bookworm`, plus `build-essential pkg-config libsqlite3-dev`).
Report the exact command and the raw test tail.

## Out of scope

The collector and the BLE layer (D6), the control channel and live HR (Phase 5),
dashboard UI for the status panel, and any change to `oura-cli` or `oura-manual`.
