# ADR 0005 — Control channel is Pi-initiated, authenticated WebSocket

Status: accepted (2026-11-07)

## Context

The browser must never reach the Pi directly; the Pi must not expose a remote
shell or an unauthenticated control service. The Pi sits behind consumer NAT and
may be offline.

## Decision

The Pi opens and maintains an outbound authenticated WebSocket to the LXC
(`/api/v1/collector/control`). The LXC queues typed, versioned, allowlisted
commands; the Pi executes them through one BLE coordinator and reports receipt
acknowledgement separately from ring execution result.

Command lifecycle: unique id, issuing user/session, created and expiry times,
states `queued|running|completed|failed|cancelled|expired`, timeouts,
cancellation, bounded duplicate-result retention. Expired interactive commands
never execute after reconnect. Exactly-once physical execution across crashes is
**not** claimed; uncertain outcomes are reported as uncertain, and non-idempotent
actions reconcile actual state before retry.

Initial allowlist: `sync_now`, `refresh_device_info`, `refresh_battery`,
`collector_status`. Later: `start_live_hr` (enforced max duration), `stop_live_hr`,
`inspect_supported_features`, and explicitly permitted feature changes.
Excluded: factory reset, firmware update, flight mode, destructive controls.

## Consequences

- No inbound port on the Pi; no Oura cloud.
- A persistent connection is required; reconnect and command-expiry behavior
  become testable surfaces and are covered in `TESTING.md`.
- Availability is reported as distinct states (Pi reachable / adapter present /
  ring seen / ring connected / operation running / ring unreachable / unknown);
  Pi connectivity never implies ring reachability.
