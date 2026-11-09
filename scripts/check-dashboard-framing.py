#!/usr/bin/env python3
"""Behavioral framing and origin probes for a disposable dashboard.

Usage:
  python3 scripts/check-dashboard-framing.py [host] [port] [public_origin]

Run only against a dashboard started with an isolated --manual-db. Successful
probes write one synthetic weight entry for today (upsert; no other rows).
public_origin, e.g. https://localhost:18443, is optional; when given, a write
carrying that Origin must succeed, proving the configured public origin.
"""
from datetime import date
import json
import socket
import sys
import time

host = sys.argv[1] if len(sys.argv) > 1 else "localhost"
port = int(sys.argv[2]) if len(sys.argv) > 2 else 8092
public_origin = sys.argv[3] if len(sys.argv) > 3 else ""

body = json.dumps({"local_date": date.today().isoformat(), "kind": "weight_kg", "value": 75.5}).encode()


def request(chunks, delay=0, shutdown=False):
    """Send byte chunks with a pause between them; return the full response."""
    with socket.create_connection((host, port), timeout=15) as conn:
        for i, chunk in enumerate(chunks):
            if i and delay:
                time.sleep(delay)
            try:
                conn.sendall(chunk)
            except (BrokenPipeError, ConnectionResetError):
                break  # server answered and closed early; response is read below
        if shutdown:
            conn.shutdown(socket.SHUT_WR)
        received = bytearray()
        while True:
            try:
                part = conn.recv(65536)
            except ConnectionResetError:
                break
            if not part:
                break
            received.extend(part)
    return bytes(received)


def check(name, chunks, expected, delay=0, shutdown=False, want_body=None):
    response = request(chunks, delay, shutdown)
    assert response, (name, "empty response")
    head, _, content = response.partition(b"\r\n\r\n")
    first = head.split(b"\r\n", 1)[0].decode()
    status = int(first.split()[1])
    assert status == expected, (name, first, content[-200:])
    if b"application/json" in head.lower():
        parsed = json.loads(content)
        if status >= 400:
            assert isinstance(parsed.get("error"), str), (name, parsed)
        if want_body is not None:
            assert want_body(parsed), (name, parsed)
    elif status == 403:
        assert content == b"forbidden", (name, content[-200:])
    print(f"PASS {name}: {first}")


def write_head(extra="", length=None):
    """POST head for the manual endpoint; Content-Length from body unless given."""
    n = len(body) if length is None else length
    return (
        f"POST /api/manual/entries HTTP/1.1\r\nHost: {host}:{port}\r\n"
        f"X-Oura-Dash: 1\r\nContent-Type: application/json\r\n{extra}"
        f"Content-Length: {n}\r\n\r\n"
    ).encode()


def stored_ok(parsed):
    return parsed.get("ok") is True and parsed.get("entry", {}).get("value") == 75.5


# ── valid writes: framing must reassemble across reads ──────────────────────
head = write_head()
check("delayed body", [head, body], 200, delay=0.15, want_body=stored_ok)
# Whitespace keeps JSON valid while proving body reads exceed one read.
large = body + b" " * 65536
check("large fragmented body", [write_head(length=len(large)), large[:20], large[20:]], 200, delay=0.15, want_body=stored_ok)
chunked = f"{len(body):x}\r\n".encode() + body + b"\r\n0\r\n\r\n"
check(
    "chunked body split",
    [write_head("Transfer-Encoding: chunked\r\n").replace(b"Content-Length: " + str(len(body)).encode() + b"\r\n", b""),
     chunked[:6], chunked[6:]],
    200, delay=0.15, want_body=stored_ok,
)
check("headers split mid-line", [head[:20], head[20:], body], 200, delay=0.15, want_body=stored_ok)

# ── refused framing: each maps to one status, never a guess ─────────────────
check("truncated body", [write_head(length=100), body[:10]], 400, shutdown=True)
check(
    "duplicate Content-Length",
    [write_head(extra=f"Content-Length: {len(body) + 1}\r\n")],
    400,
)
check(
    "Content-Length with chunked",
    [write_head("Transfer-Encoding: chunked\r\n")],
    400,
)
check(
    "gzip coding",
    [write_head().replace(b"Content-Length: " + str(len(body)).encode(), b"Transfer-Encoding: gzip, chunked")],
    501,
)
check(
    "oversized Content-Length",
    [write_head(length=1024 * 1024 + 1)],
    413,
)
check(
    "bad chunk size",
    [write_head("Transfer-Encoding: chunked\r\n").replace(b"Content-Length: " + str(len(body)).encode() + b"\r\n", b"") + b"zz\r\n"],
    400,
)
check(
    "oversized head",
    [b"POST /api/manual/entries HTTP/1.1\r\nHost: localhost:%d\r\n" % port
     + b"".join(b"X-Pad: " + b"a" * 200 + b"\r\n" for _ in range(100))
     + b"\r\n"],
    431,
)

# ── origin and host guard ───────────────────────────────────────────────────
check(
    "foreign origin",
    [write_head(extra="Origin: https://evil.example\r\n"), body],
    403,
)
check(
    "bad Host",
    [write_head().replace(f"Host: {host}:{port}".encode(), b"Host: evil.example"), body],
    403,
)
if public_origin:
    check(
        "configured public origin",
        [write_head(extra=f"Origin: {public_origin}\r\n"), body],
        200,
        want_body=stored_ok,
    )
    check(
        "public origin with foreign Host",
        [write_head(extra=f"Origin: {public_origin}\r\n").replace(f"Host: {host}:{port}".encode(), b"Host: evil.example"), body],
        403,
    )
print("PASS all framing probes")
