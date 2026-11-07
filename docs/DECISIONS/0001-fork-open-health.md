# ADR 0001 — Fork open_health instead of building a new application

Status: accepted (2026-11-07)

## Context

`Th0rgal/open_health` already provides a Rust HTTP dashboard with a stable
JSON API, a vanilla web UI with no build step and no CDN, local-only assets,
SQLite ring storage, and model-free fallbacks for every scored metric.
`dashboard/README.md` confirms: port 8090, loopback-only, `GET /api/summary`,
`GET/POST /api/profile`, `POST /api/sync`, `POST /api/feature`,
`GET/POST /api/ring-key`, same-origin `X-Oura-Dash` CSRF header, Host-header
check, icons vendored from Phosphor (MIT).

## Decision

Fork `open_health` at `8f616ec5bdf569e7faecbcefa2b8eff6f69ae2bc` and extend it.
Do not rewrite the dashboard, the ring decoders, or the metrics layer.

## Consequences

- Preserves upstream metric integrity rules (clock correction, model-free
  labelling) instead of re-deriving them.
- Local modifications are small and reviewable; upstream can be re-merged.
- The iOS application is not required and stays unbuilt.
- MIT license and attribution preserved (see `LICENSE`, `NOTICE`).
