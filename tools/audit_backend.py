#!/usr/bin/env python3
"""Read-only storage inventory for the LXC backend (audit task A1).

Prints schema, table row counts, per-tag counts, capture-time ranges, decode
coverage, raw counter ranges and file modes for the ring database and the
manual database. The databases are opened with `mode=ro` URIs, so this tool
can never create or alter them. Serials are masked to a prefix and suffix;
payloads, bodies and secrets are never printed.

Exit codes: 0 ok, 1 usage, 2 a database is missing, 3 a database exists but
cannot be read (permission, corruption, or a busy WAL).

Usage: audit_backend.py --db PATH --manual-db PATH [--mode ro] [--json]
"""

import argparse
import json
import os
import sqlite3
import stat
import sys
import urllib.parse

METRIC_TABLES = ("ingest_raw", "events")


def mask(value: object) -> str:
    """A serial reduced to its prefix and suffix; never the middle."""
    text = str(value)
    if len(text) <= 8:
        return "…"
    return text[:4] + "…" + text[-4:]


def file_mode(path: str) -> str:
    mode = stat.S_IMODE(os.stat(path).st_mode)
    return f"0o{mode:03o}"


def connect_ro(path: str) -> sqlite3.Connection:
    """Open `path` read-only. Never create, never write."""
    if not os.path.exists(path):
        print(f"missing: {path}", file=sys.stderr)
        sys.exit(2)
    quoted = urllib.parse.quote(os.path.abspath(path))
    try:
        return sqlite3.connect(f"file:{quoted}?mode=ro", uri=True)
    except sqlite3.Error as error:
        print(f"unreadable: {path}: {error}", file=sys.stderr)
        sys.exit(3)


def tables(conn: sqlite3.Connection) -> list:
    rows = conn.execute(
        "SELECT name, sql FROM sqlite_master"
        " WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
    ).fetchall()
    return [{"name": name, "sql": sql} for name, sql in rows]


def count(conn: sqlite3.Connection, table: str, where: str = "1=1") -> int:
    try:
        return conn.execute(f'SELECT COUNT(*) FROM "{table}" WHERE {where}').fetchone()[0]
    except sqlite3.Error:
        return 0


def first_row(conn: sqlite3.Connection, sql: str):
    try:
        return conn.execute(sql).fetchone()
    except sqlite3.Error:
        return None


def audit(path: str) -> dict:
    conn = connect_ro(path)
    conn.row_factory = sqlite3.Row
    out = {
        "path": path,
        "file_mode": file_mode(path),
        "schema": tables(conn),
        "row_counts": {},
    }
    for table in out["schema"]:
        name = table["name"]
        out["row_counts"][name] = count(conn, name)

    if "ingest_raw" in out["row_counts"]:
        per_tag = conn.execute(
            "SELECT tag, COUNT(*) AS n, MIN(ring_counter) AS lo,"
            " MAX(ring_counter) AS hi FROM ingest_raw GROUP BY tag ORDER BY tag"
        ).fetchall()
        out["raw_per_tag"] = [
            {"tag": tag, "rows": n, "counter_min": lo, "counter_max": hi}
            for tag, n, lo, hi in per_tag
        ]
        window = first_row(
            conn, "SELECT MIN(captured_unix_ms), MAX(captured_unix_ms) FROM ingest_raw"
        )
        if window:
            out["raw_capture_ms_min"], out["raw_capture_ms_max"] = window
        per_serial = conn.execute(
            "SELECT serial, COUNT(*) AS n FROM ingest_raw GROUP BY serial ORDER BY serial"
        ).fetchall()
        # Serials are masked; a count per device stays useful for health.
        out["raw_per_device_masked"] = [
            {"serial": mask(serial), "rows": n} for serial, n in per_serial
        ]
        total = out["row_counts"].get("ingest_raw", 0)
        decoded = count(conn, "events", "decoded_json IS NOT NULL")
        out["decode_coverage"] = {
            "raw_events": total,
            "decoded_events": decoded,
            "decoded_ratio": round(decoded / total, 4) if total else None,
        }

    if "events" in out["row_counts"]:
        times = first_row(
            conn, "SELECT MIN(captured_unix), MAX(captured_unix) FROM events"
        )
        if times:
            out["event_capture_min"], out["event_capture_max"] = times
        tags = conn.execute(
            "SELECT tag, COUNT(*) AS n FROM events GROUP BY tag ORDER BY tag"
        ).fetchall()
        out["event_per_tag"] = [{"tag": tag, "rows": n} for tag, n in tags]
        per_serial = conn.execute(
            "SELECT serial, COUNT(*) AS n FROM events GROUP BY serial ORDER BY serial"
        ).fetchall()
        out["event_per_device_masked"] = [
            {"serial": mask(serial), "rows": n} for serial, n in per_serial
        ]

    for meta in ("ingest_collectors", "ingest_jobs", "ingest_batches"):
        if meta in out["row_counts"]:
            out[f"{meta}_rows"] = out["row_counts"][meta]
    conn.close()
    return out


def print_human(data: dict) -> None:
    print(f"{data['path']}  mode {data['file_mode']}")
    for table in data["schema"]:
        name = table["name"]
        print(f"  table {name}: {data['row_counts'][name]} rows")
        print(f"    {table['sql']}")
    if "raw_per_tag" in data:
        for row in data["raw_per_tag"]:
            print(
                f"  raw tag {row['tag']}: {row['rows']} rows,"
                f" counters {row['counter_min']}..{row['counter_max']}"
            )
        print(
            f"  raw capture window:"
            f" {data.get('raw_capture_ms_min')}..{data.get('raw_capture_ms_max')} ms"
        )
        for row in data["raw_per_device_masked"]:
            print(f"  device {row['serial']}: {row['rows']} raw rows")
    if "decode_coverage" in data:
        cov = data["decode_coverage"]
        print(
            f"  decode coverage: {cov['decoded_events']}/{cov['raw_events']}"
            f" (ratio {cov['decoded_ratio']})"
        )
    if "event_per_tag" in data:
        for row in data["event_per_tag"]:
            print(f"  event tag {row['tag']}: {row['rows']} rows")
        print(
            f"  event capture window:"
            f" {data.get('event_capture_min')}..{data.get('event_capture_max')} unix"
        )
        for row in data["event_per_device_masked"]:
            print(f"  device {row['serial']}: {row['rows']} decoded rows")


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Read-only inventory of the ring and manual databases"
    )
    parser.add_argument("--db", help="ring database path")
    parser.add_argument("--manual-db", dest="manual_db", help="manual database path")
    parser.add_argument(
        "--mode",
        choices=["ro"],
        default="ro",
        help="only read-only mode exists (default: ro)",
    )
    parser.add_argument(
        "--json", action="store_true", help="machine-readable output instead of text"
    )
    args = parser.parse_args()

    paths = [p for p in (args.db, args.manual_db) if p]
    if not paths:
        parser.error("nothing to audit: pass --db and/or --manual-db")

    results = {p: audit(p) for p in paths}
    if args.json:
        print(json.dumps({"databases": list(results.values())}, indent=2))
    else:
        for data in results.values():
            print_human(data)
    return 0


if __name__ == "__main__":
    sys.exit(main())