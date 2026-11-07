# SECURITY

Findings from the pinned-revision audit plus the decisions they force.
Source-inspection results only; no exploit was executed.

## Required fixes in this fork

| # | Finding | Evidence | Action |
|---|---|---|---|
| S1 | Blood-marker fallback uploads lab rows to a third-party service | `crates/oura-cli/src/blood.rs:471-500` posts to `https://agent-backend.thomas.md/v1` when `AGENT_BACKEND_API_KEY` is set | **Remove the blood module** (ADR 0006) |
| S2 | CSRF guard can be satisfied from the request body | `crates/oura-cli/src/dashboard.rs:445-449,480-488,513-529`: the header helper scans every line of the first socket read, including body lines; `/api/sync` ignores the body | **Fixed** — `same_origin_write_ok` splits the request once and parses the header block only, and requires `Origin`, when sent, to equal `Host`. 8 unit tests in `dashboard::request_guard_tests`. Real user authentication still lands with D3/D8 |
| S3 | No rate limiting, connection cap, or read timeout | `dashboard.rs:383-388,480-481,532-550` | Add limits when the new listener/auth middleware lands |
| S4 | Ring key written before `chmod 0600`; `chmod` failure ignored | `crates/oura-cli/src/main.rs:364-370`; `dashboard.rs:45-63` | **Fixed** — `crates/oura-cli/src/secret_file.rs::write_private` creates the file with mode `0600` in the creating syscall and returns an error if the mode cannot be enforced; now used by `save_key` and `write_ring_key`. 3 unit tests |
| S5 | Debug tracing prints the ring key as TX hex | `open_oura` pinned `crates/oura-link/src/client.rs:175,352` — `set_auth_key()` goes through a generic `tx = %hex::encode(bytes)` logger | Never run the collector with debug tracing on the key path; redact key packets |
| S6 | DNA download from a third-party FTP endpoint | `crates/oura-cli/src/dna.rs:361,410` | **Remove the DNA module** (ADR 0006) |
| S7 | Blood import accepts any local PDF path (`..`, absolute) | `dashboard.rs:653-660`, `blood.rs:354-356` | Removed with the blood module |
| S8 | Health payloads printed to stdout by CLI commands | `crates/oura-cli/src/main.rs:1137-1154` and the `tools/*` runners | Keep service stdout out of long-term journals; redact by default |

## Inherited, documented, not fixable by us

| # | Finding | Detail |
|---|---|---|
| S9 | A **real-value ring test key is committed** upstream | `open_oura` `docs/horizon-ring3-protocol-cheatsheet.md:48-53`, duplicated in `crates/oura-protocol/src/auth.rs:70` and `crates/oura-link/src/client.rs:943`; the same value appears in `open_health` history (`crates/oura-core/src/auth.rs:70` at `dc1799c`). It is upstream's published test fixture, not this project's secret, and it is already public. This fork does not reprint it and must never add a real key of its own. Whether the value still authenticates against any ring is **unknown**. |
| S10 | Optional research tools mishandle secrets | `tools/oura_token_sniff.py` writes full tokens (no `0600` in open_health), `tools/oura_protocol.py` prints generated keys, `tools/oura_firmware_download.py` forwards a bearer to a manifest-supplied URL with a `startswith` host check | These are local research tools, not part of the deployed service. They are **not** installed or run by this deployment. |

## Deployment rules that follow

- Never place secrets or real health payloads in git, example configuration,
  test fixtures, routine logs, or agent handoffs.
- Ring key stays on the Pi, `0600`, created with a restrictive mode.
- Collector credentials and browser sessions are separate; rotating one must
  not disturb the other.
- Debug/verbose tracing is off in service units.
- Logs are structured and redacted; health payload bodies are not logged.
- Nothing listens on the public internet by default.
