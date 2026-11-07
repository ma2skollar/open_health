# ADR 0004 — Collector delivery uses consistent SQLite snapshots first

Status: accepted (2026-11-07), pending measurement

## Context

The Pi keeps its own ring store. The server needs that data without inventing a
bespoke event-transfer format, and without losing the ability to re-decode raw
events later.

## Decision

MVP delivery is an immutable, consistent snapshot of the Pi's ring store,
uploaded as a versioned artifact with metadata (protocol version, storage schema
version, collector id, collector epoch, monotonic generation, source build
revision, creation time, byte size, checksum, explicit demo/production identity).
The server validates, verifies integrity and expected structure, staging-writes,
then activates as a generation. The previous known-good generation is retained
for rollback; readers never have the database swapped beneath them.

Rules:

- Snapshot created with the SQLite backup API or `VACUUM INTO`, WAL-aware; never a
  naive copy of a live file; never over SMB/NFS.
- Ring authentication secrets are excluded from the snapshot.
- Duplicate delivery is harmless; stale generations cannot roll data backward.
- A rebuilt collector must not blindly replace complete server history with
  partial history; unproven continuity requires human intervention.

## Consequences

- Simpler server logic than incremental event streaming, and structural
  validation is strong (it is a real SQLite database).
- Cost: upload size and time. Measured with bounded synthetic datasets before
  acceptance; if snapshots prove unsuitable, evidence is recorded and incremental
  delivery is proposed as a separate ADR.
- Only one of the two designs is implemented; both are explicitly not built.
