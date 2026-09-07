"""Synthetic benchmark only; never reads a user's ring data."""
import json
import sqlite3
import sys

with sqlite3.connect(sys.argv[1]) as db:
    db.executescript('''CREATE TABLE events(id INTEGER PRIMARY KEY, ring_timestamp INTEGER, tag INTEGER, decoded_json TEXT, captured_unix INTEGER, body BLOB);
    CREATE INDEX event_capture ON events(captured_unix,id);''')
    sample = json.dumps({'ibi_ms': [810, 820, 790, 800, 805, 815, 820, 810], 'amplitude': [1, 2, 1, 3, 2, 1, 2, 1]})
    for start in range(0, 1_735_704, 10000):
        rows = []
        for i in range(start, min(start + 10000, 1_735_704)):
            epoch, offset = divmod(i, 600000)
            ds = offset * 100
            cu = 1_700_000_000 + epoch * 6_000_000 + offset * 10
            rows.append((i+1, ds, 66 if offset == 0 else 96,
                         json.dumps({'unix_time': cu}) if offset == 0 else sample, cu, bytes(16)))
        db.executemany('INSERT INTO events VALUES (?,?,?,?,?,?)', rows)
