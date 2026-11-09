# Handoff D3 — manual metrics API and dashboard UI

## Objective

Make the application useful without a ring: expose `crates/oura-manual` over the
existing dashboard HTTP server and add a mobile-friendly UI for daily calories
and body weight, with charts and export.

## Context (decided — do not relitigate)

- This worktree is a fork of `Th0rgal/open_health`. `crates/oura-manual` already
  exists, is a workspace member, and owns `manual.db`
  (`docs/DECISIONS/0003-separate-manual-metric-store.md`). Read
  `crates/oura-manual/src/lib.rs` for the API; do not change that crate.
- The DNA and blood panels were removed (`docs/DECISIONS/0006`). The dashboard is
  loopback-only, plain HTTP, vanilla HTML/CSS/JS, **no CDN, no build step, no new
  JS library**. Match the existing style in `dashboard/web/app.js`.
- Calorie and weight entry must work with **no ring and no ring database**.
- Missing values are not zero. Never render a fabricated point.

## Files you own

`crates/oura-cli/src/dashboard.rs`, `crates/oura-cli/src/main.rs`,
`dashboard/web/index.html`, `dashboard/web/app.js`, `dashboard/web/styles.css`,
`scripts/smoke-dashboard.sh`, `docs/TESTING.md`.

Do not edit `crates/oura-manual/**` or any `Cargo.toml` — the workspace member,
the `oura-manual` dependency and the `rusqlite` `backup` feature are already
added by the integrator.

## HTTP API (add these routes to the existing `match (method, path)` dispatcher)

All JSON. All require the existing same-origin guard (add mutating ones to the
CSRF `forbid` list next to `/api/profile`).

| Method | Path | Behaviour |
|---|---|---|
| `GET` | `/api/manual/entries?from=&to=&kind=` | `{"entries":[ManualEntry…]}`; `from`/`to` are inclusive `YYYY-MM-DD`; `kind` optional |
| `POST` | `/api/manual/entries` | body `{"local_date","kind","value","note"?}` → upsert. `200 {"ok":true,"outcome":"inserted"\|"updated","entry":{…}}`; validation failure → `400 {"ok":false,"error":"<message>"}` |
| `DELETE` | `/api/manual/entries?date=YYYY-MM-DD&kind=` | `{"ok":true,"deleted":true\|false}` |
| `GET` | `/api/manual/series?days=N` | `{"calories":[{"date","value"}],"weight":[{"date","value"}],"weight_trend":[{"date","value"}],"unit":"kg"}` |
| `GET` | `/api/manual/export` | `text/csv` attachment `manual-entries.csv`, header `local_date,kind,value,note,source,created_at,updated_at`; RFC 4180 quoting |

- `created_at`/`updated_at` are set by the server (Unix seconds, `SystemTime`).
  `source` is `"manual"` for API writes.
- `weight_trend` is a trailing 7-entry mean over the weight series, labelled as a
  trend, computed only from stored values. The API does not interpolate or fill.
  The web calorie and weight trendlines visually bridge missing dates with linear
  interpolation between known trend endpoints, without changing stored values,
  averages, or inspection targets. Dates outside those endpoints are not filled.
- `days` defaults to 180 and is clamped to `1..=3650`.
- The API must not fail when the manual database is empty: return empty arrays.

## Server wiring

- New flag `--manual-db <PATH>` on the `dashboard` subcommand, `$OURA_MANUAL_DB`
  as the environment default, and `manual.db` beside the ring DB as the last
  fallback. `oura dashboard` must still start when the manual DB cannot be
  created: log a warning and serve every other route.
- Open the store once at startup and share it; do not open per request.
- Weight display unit: `--weight-unit kg|lb` + `$OURA_WEIGHT_UNIT`, default `kg`.
  Canonical storage stays kg; convert only for display, and label the unit
  everywhere it is shown. `kg -> lb` is `× 2.2046226218`, rounded to 1 decimal
  for display. Editing always submits the displayed unit and is converted back.

## UI

A new **Daily entry** panel in `dashboard/web/index.html`, above the ring panels,
so it is reachable with no ring data:

- Form: date (defaults to the client's local today), calories (kcal), weight (in
  the configured display unit), optional note. One save button per row, clear
  inline validation errors surfaced from the API.
- Today's values prefill when they already exist; saving again updates instead of
  duplicating.
- Recent entries list (last 30 days) with edit and delete.
- Two charts, hand-drawn SVG consistent with the existing `app.js` helpers:
  calories as bars, weight as a line **plus** a visually distinct rolling-trend
  line labelled as a trend. Empty state says there is no data yet.
- Mobile-first: usable at 375 px wide. Touch targets ≥ 40 px. No horizontal
  scroll.
- The existing no-ring ring-data error must not hide this panel.

## Acceptance / required evidence

1. `MSYS_NO_PATHCONV=1 docker run --rm -v "<this worktree>:/w" -v oura-cargo:/usr/local/cargo/registry -v oura-target:/w/target -w /w rust:1-bookworm bash -lc "cargo build --release -p oura-cli"` succeeds.
2. Extend `scripts/smoke-dashboard.sh` to also, against a temporary `--manual-db`:
   - `GET /api/manual/entries` on an empty DB → `200`, empty `entries`
   - `POST` a calorie entry, then `GET` it back with the exact value
   - `POST` the same date+kind again with a new value → `outcome":"updated"` and
     exactly one row for that date+kind
   - `POST` weight on the same date → both kinds coexist
   - `POST` an invalid value (e.g. `-5`) → `400` and no row written
   - `POST` with header `X-Oura-Dash` only inside the request **body** →
     `403` (regression guard for the CSRF parser fix)
   - `DELETE` it → `deleted":true`, then `GET` shows it gone
   - `GET /api/manual/export` → `text/csv` containing the header line
   - the script must still end with `result: PASS`
3. Paste the raw smoke output tail.

## Out of scope

Import, ring ingestion, control protocol, authentication changes, timezone
conversion of dates, food-item logging, and any change to `crates/oura-manual`.
