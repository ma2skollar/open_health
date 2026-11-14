# Raspberry Pi deployment

Use [the history-only collector runbook](pi-collector.md) and the dedicated package files under [`deploy/pi/`](../deploy/pi/).

The current Pi path is raw SQLite buffering and signed HTTPS batches to exact `POST /ingest/v1/batches`. The Pi does not run physiological decoding or models. It has no bearer snapshot delivery, WebSocket control channel, automatic provisioning, or live-HR/workout path.

The installer defaults to dry-run. Hardware installation, pairing, real ring contact, backend/proxy activation, and any destructive operation remain separately approval-gated. Compilation and synthetic tests do not prove Pi/ring hardware acceptance. Follow the current runbook's D1–D3 gates; do not use older live-capture acceptance instructions for this collector.
