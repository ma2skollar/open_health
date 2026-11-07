# Handoff D8 — deployment: systemd units, installer, health checks

## Objective

Make the one-container deployment reproducible and reviewable: systemd units, an
idempotent installer, placeholder configuration, a readiness/health probe, and a
documented upgrade and rollback. No secrets in any file.

## Context (decided — do not relitigate)

- One **unprivileged** Proxmox LXC, native systemd, SQLite, no Docker nesting, no
  Redis (`docs/ARCHITECTURE.md`).
- The dashboard binds **loopback only**; browser and collector traffic terminates
  at a reverse proxy that is **not chosen yet**. Do not pick one, do not add a
  proxy config, and do not change the bind address. Assume loopback plus a proxy.
- `docs/OPERATIONS.md` already lists the runtime dependency (`libdbus-1-3`), the
  backup/restore ordering, and the redaction rules. Implement; do not restate.
- Backup and restore are D4. If `oura backup` is not merged yet, the timer must
  call it anyway and the unit must be verified syntactically; note the dependency
  in the file header rather than inventing a different command.

## Files you own

`deploy/**` (new), `docs/OPERATIONS.md`, `docs/TESTING.md`,
`docs/HARDWARE_ACCEPTANCE.md` (only the collector-runbook section, if you touch it).

Do not edit `crates/**`.

## Deliverables

1. `deploy/systemd/oura-dashboard.service`
   - `User=oura`, `Group=oura`, `WorkingDirectory=/srv/oura`, `Restart=on-failure`
   - `EnvironmentFile=-/etc/oura/oura.env`
   - `NoNewPrivileges=yes`, `ProtectSystem=strict`, `ProtectHome=yes`,
     `PrivateTmp=yes`, `ReadWritePaths=/srv/oura`, `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`
   - hardening that does **not** break SQLite or D-Bus; if something must be
     relaxed, say which directive and why in a comment
   - `ExecStart=` the installed binary with explicit data paths
2. `deploy/systemd/oura-backup.service` and `oura-backup.timer`
   - oneshot, runs `oura backup --dest /srv/oura/backups`, a daily timer with a
     random delay; `Persistent=true`
3. `deploy/oura.env.example` — every value a placeholder (`__RING_DB__`,
   `__MANUAL_DB__`, `__TZ_OFFSET__`, …). No real paths, no keys, no tokens. Comment
   that the ring key is **never** stored here.
4. `deploy/install.sh` — idempotent, POSIX-safe shell with `set -euo pipefail`
   where the shell supports it, and supports `--dry-run`.
   - creates the `oura` system user/group and `/srv/oura/{data,backups,bin}` with
     explicit modes, only when missing
   - installs the binary and units, then `systemctl daemon-reload` and
     `enable --now`
   - every command that needs root is listed in one clearly marked section, and
     the script refuses to run as root for the non-privileged steps
   - re-running reports "already present / unchanged" instead of failing
5. `deploy/health.sh` — readiness probe: unit active, the dashboard answers on its
   loopback port, free disk above a floor, manual database readable, and (once
   D4/D5 exist) the age of the newest accepted upload. Non-zero exit with a
   human-readable reason on failure. Redacted output only.
6. `deploy/README.md` — LXC preparation, the privileged steps, the ARM64 collector
   runbook, upgrade, rollback.

## Acceptance / required evidence

1. `systemd-analyze verify` passes for every unit file (run inside a
   `debian:bookworm` container with systemd available; paste the command).
2. `shellcheck` is clean for `install.sh` and `health.sh`, or every remaining
   warning is explained inline.
3. `install.sh --dry-run` prints the actions and changes nothing; a second
   `--dry-run` prints the same list.
4. Secret scan finds nothing: run
   `grep -rnE '(BEGIN [A-Z ]*PRIVATE KEY|api[_-]?key *[:=] *[^_]|token *[:=] *[^_]|[0-9a-fA-F]{32})' deploy/`
   and paste its empty output.
5. Every unit binds loopback only: no unit listens on a non-loopback address and
   none contains a public hostname.

## Out of scope

Choosing or configuring a reverse proxy, TLS certificates, firewall rules,
collector/Pi units, ARM64 **build** automation (document the command; the build
itself is verified separately), and any `crates/**` change.
