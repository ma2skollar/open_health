# ADR 0006 — Remove the DNA and blood panels and all outbound integrations

Status: accepted (2026-11-07)

## Context

Requirement: no external AI services, no third-party health APIs, no telemetry;
serve UI assets locally. The privacy target is "health data on my own devices".

Audit of the pinned revision found runtime outbound integrations:

1. **Third-party AI service receiving lab data.** `crates/oura-cli/src/blood.rs:471-500`
   (`llm_fallback_markers`) sends blood-marker PDF text to
   `https://agent-backend.thomas.md/v1` whenever `AGENT_BACKEND_API_KEY` is set,
   with a system prompt asking the remote model to normalize lab rows. Blood
   results are health data. This is disqualifying.
2. **Third-party FTP download.** `crates/oura-cli/src/dna.rs:361,410` fetches
   PGS Catalog score files from `https://ftp.ebi.ac.uk/...` on demand.
3. **Outbound links.** `dashboard/web/dna.html:30`, `dashboard/web/dna.js:131,238`
   navigate to `www.pgscatalog.org`; `dna.js` also builds links from
   `source_url` fields in `dna/catalog.json`.

Neither panel is in scope for this project (the goal is ring measurements plus
manual calories/weight). Keeping them means keeping the audit surface, the
network egress, and the `dna/` + blood PDF handling code.

## Decision

Remove DNA and blood from the fork:

- delete `crates/oura-cli/src/blood.rs` and `crates/oura-cli/src/dna.rs`;
- delete their routes from the dashboard dispatcher
  (`dashboard.rs` handlers for `/api/blood/*`, `/api/dna/*`) and the static
  asset routes (`/dna*`, `/blood*`);
- delete `dashboard/web/dna.{html,js,css}` and `dashboard/web/blood.{html,js,css}`;
- drop `crates/oura-dna` from the workspace members and from `oura-cli`
  dependencies;
- delete the `dna/` catalog directory;
- remove the now-unused `ureq` dependency from `oura-cli`.

No compatibility shim, no feature flag, no "disabled by default" route: a route
that exists can be re-enabled by a config mistake.

## Consequences

- The only remaining network egress is the optional collector upload/control
  channel, which is authenticated and operator-owned.
- Upstream merges will conflict in `dashboard.rs` and `Cargo.toml`; that is
  expected and confined to two files.
- Offline operation can be asserted only after a deployment test (see
  `OPERATIONS.md`), not from this decision.
