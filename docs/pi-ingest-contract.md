# Pi raw ingest contract v1

Status: **frozen proposal for backend B1 review, not an implemented backend or permission to activate it**. The shared API is `oura_protocol_v1::ingest`. Existing root snapshot/control types remain unchanged; they do not implement this contract. No backend handler, key store, replay database, or deployment is supplied by this module.

## Transport and limits

Use TLS and exactly `POST /ingest/v1/batches`. There is no query string or trailing slash. Reject every other method/path, redirects, request body compression (`Content-Encoding`), and a `Content-Type` other than exactly `application/json` (no charset parameter). The collector must not follow redirects or transparently compress a body. There is no separate unauthenticated heartbeat endpoint.

Bound the received wire body to **4,194,304 bytes (4 MiB)** before parsing. Reject an oversized or truncated body, malformed JSON, duplicate object keys, unknown fields, missing required fields, and values outside the rules below. All structures use `deny_unknown_fields`. `Option` values serialize as `null`; serde also accepts a missing optional field on decode. All other fields are required. No schema field is silently defaulted on decode. `Heartbeat::default()` is only a caller-side construction convenience.

`Batch::validate()` checks semantic field rules. `Batch::to_bytes()` validates, serializes compact JSON in declaration order, and enforces the wire limit through a bounded writer. The writer does not materialize an arbitrarily oversized batch. A valid semantic batch may still exceed the wire limit. Backend must enforce the wire limit separately before decoding.

Never parse/reformat/re-serialize a body before authenticating it: the hash covers **exact transmitted bytes**, including whitespace if present. There is no JSON canonicalization requirement on the receiver. Retries must use the original immutable bytes, not a reconstructed JSON object.

## Batch schema

Every integer is a JSON integer, not a float or string. Numeric Rust widths are part of the contract. Identifiers (`device_id`, `collector_epoch`, `batch_id`, and signing `key_id`) are nonempty ASCII `[A-Za-z0-9._-]`, at most 128 bytes. `collector_build` and known metadata strings are nonempty printable ASCII (`0x20..0x7e`), at most 128 bytes. Use `null`, not an empty string, for unknown metadata. Build is an opaque collector software identifier, **not** a physiological decoder version.

`Batch` fields, in serializer order:

- `version: u32`: exactly `1`.
- `device_id: String`: registered collector device identity.
- `collector_epoch: String`: persistent store identity. It changes only when a new collector store is explicitly created, not on process boot.
- `batch_id: String`: immutable batch identity. It must not be derived from content deduplication.
- `collector_build: String`.
- `device: DeviceMetadata`: `serial`, `firmware`, and `hardware`, each `Option<String>`.
- `events: Vec<RawEvent>`: at most **10,000**, including zero for a heartbeat-only batch.
- `heartbeat: Heartbeat`.

`RawEvent` fields:

- `source_row_id: u64`: positive, unique within this batch. Delivery identity is `(device_id, collector_epoch, source_row_id)`.
- `tag: u32`: `0..=255`, preserved raw.
- `ring_counter: u64`: `0..=u32::MAX`, ring-local decisecond counter. Never a Unix timestamp. For legacy records this is the exact counter. For extended records it is `floor(reconstructed_absolute_milliseconds / 100)`, with no artificial `+1` adjustment. Equal counters are valid; identity does not depend on counter uniqueness.
- `body_hex: String`: exact undecoded event body, lowercase even-length hex, at most **65,536 decoded bytes (64 KiB)**. Empty body is valid.
- `captured_unix_ms: i64`: nonnegative Pi wall-clock observation time in Unix milliseconds; not the physiological event time.
- `clock_anchor: Option<ClockAnchor>`: an observation or boot/reset boundary, not an event-to-UTC mapping.
- `transport: Option<TransportProvenance>`: original transport record and raw encoded counter. Production BLE records include it; it is nullable for synthetic/other sources without transport evidence.

`ClockAnchor` has `kind: String` (`observed` or `boot_boundary` only), `ring_counter: u64` (`0..=u32::MAX`) and nonnegative `captured_unix_ms: i64`. It records the Pi's observation, not authoritative ring UTC. Preserve reset evidence; do not deduplicate repeated content after a reboot. This contract does not infer boot boundaries from data alone.

### Transport provenance

`TransportProvenance` fields:

- `encoding: String`: `legacy_event_packet` or `ext_event_record` only.
- `raw_hex: String`: nonempty, even-length lowercase hex, at most **66,560 decoded bytes** (event body bound plus 1 KiB framing). Preserve the bytes exactly; do not reconstruct them from decoded fields.
- `counter_value: u64`: counter decoded directly from the transmitted record, before conversion, accumulation, or timestamp synthesis.
- `counter_unit: String`: an encoding-dependent allowlist below.

For `legacy_event_packet`, `raw_hex` is the complete `tag | len | payload` packet. The payload starts with a u32 little-endian decisecond counter, followed by the unmodified event body. `counter_unit` is exactly `decisecond`; `counter_value == RawEvent.ring_counter`.

For `ext_event_record`, `raw_hex` is exactly one bundle record: `control | tag | ext_len | varint | body`. It excludes the `0x2f`/`0x43` transport chunk headers and the enclosing u16 envelope length. `counter_value` is the decoded varint, not the upstream parser's synthesized decisecond timestamp. `counter_unit` is `millisecond_absolute` or `millisecond_delta`. The BLE protocol interpretation treats a non-first bundle record with a varint below `100000000` as a delta; the first record is absolute. Preserve the original record so the backend can independently reconstruct that interpretation.

For `millisecond_absolute`, `counter_value / 100 == RawEvent.ring_counter`. For `millisecond_delta`, the relation needs preceding bundle context, so shared DTO validation does not cross-check it. The module validates encoding, unit, hex bounds and direct-counter consistency; it does not claim to parse or authenticate the BLE payload's physiological meaning. Backend reconstruction and upstream protocol changes need separate review.

### Heartbeat

Every batch carries a heartbeat. Fields in order:

- `ble_attempt_unix_ms`, `ble_success_unix_ms`, `upload_receipt_unix_ms`: `Option<i64>`, nonnegative Unix milliseconds when known.
- `queue_count`, `queue_bytes`: `u64`.
- `queue_oldest_unix_ms`: `Option<i64>`, nonnegative oldest queued observation; queue age derives from this plus the observation clock.
- `battery_percent: Option<u8>`: `0..=100` when known.
- `disk_free_bytes: Option<u64>`.
- `pi_temperature_millicelsius: Option<i64>`: signed temperature (negative values are valid).
- `clock_synchronized: bool`: truthful clock-sync health, not an inferred guarantee from recent events.
- `error_codes: Vec<String>`: at most 16 codes; each at most 64 ASCII bytes; no free text.

The only allowed error codes (`ERROR_CODES`) are:

```text
io_failed sqlite_failed json_failed config_invalid key_permissions key_invalid
state_busy state_invalid outbox_pressure disk_reserve clock_unsynchronized
upload_failed ack_invalid contract_invalid cursor_regression history_limit
ring_failed operation_timeout
```

Never insert dynamic errors, URLs, paths, identifiers, payload bytes, signatures, or key material into these codes or logs.

## Authentication

Headers (single values, no duplicate headers):

```text
x-oura-key-id
x-oura-device-id
x-oura-timestamp
x-oura-nonce
x-oura-body-sha256
x-oura-signature
```

`x-oura-timestamp` is nonnegative integer Unix **seconds**, rendered as canonical decimal (no sign, whitespace or leading zeros except `0`). `x-oura-nonce` is exactly 32 lowercase hex characters representing **16 cryptographically random bytes**, fresh per HTTP attempt. `x-oura-body-sha256` and `x-oura-signature` are each exactly 64 lowercase hex characters. Backend must enforce canonical header parsing; `SigningFields.timestamp` is already an integer and cannot retain a noncanonical header spelling.

The HMAC key is exactly **32 raw bytes**, not a hex string. On Pi it belongs in an external, permission-checked `0600` file. Do not generate real credentials from the public vector key. Per-device backend key recovery, permission protection/encryption, provisioning, revocation and overlap rotation belong to B1.

`SigningFields { key_id, device_id, timestamp, nonce, body_sha256 }` renders the following exact UTF-8 bytes. Every line, including the final line, ends in one LF (`0x0a`), not CRLF. There are no blank lines, spaces around values or escaping:

```text
open-health-ingest-v1\n
{key_id}\n
{device_id}\n
POST\n
/ingest/v1/batches\n
{timestamp in decimal Unix seconds}\n
{nonce}\n
{body_sha256}\n
```

The display above uses `\n` to show byte boundaries; the canonical string contains actual LF bytes. The signature is lowercase hex HMAC-SHA-256 over these bytes. `SigningFields::sign(&[u8])` returns it. `SigningFields::verify(key, signature, now)` validates fields and key length, rejects an absolute timestamp difference over **300 seconds**, and uses `hmac::Mac::verify_slice` for constant-time MAC comparison. Both ±300-second boundaries are accepted. Replay protection is explicitly **not** performed by this pure module.

### Required B1 validation and persistence order

1. Enforce exact route/method/query/content-type/compression and bounded wire size. Reject duplicate auth headers and noncanonical header values.
2. Resolve an active, authorized `(device_id, key_id)` to its recoverable per-device key. Reject unknown/revoked keys without leaking credentials.
3. Recompute `body_hash(received_bytes)` and compare it to the signed header. Verify timestamp and HMAC. Parsing a syntactically valid signed hash does not itself bind it to the received body; the backend must perform this explicit comparison.
4. Parse the bounded body, call `Batch::validate()`, and require the signed header's `device_id` to equal the batch device. Enforce source-event identity consistency across batches.
5. Atomically claim `(key_id, nonce)` in a persistent replay store. Retain replay records at least **900 seconds (15 minutes)**; include concurrent and restart replay cases. A successful signature check alone is not sufficient authorization to accept a replay.
6. In a durable transaction, accept immutable receipt identity `(device_id, batch_id)` plus body hash and raw event identities. A fresh-nonce retry with the identical batch returns the original receipt without duplicate events. Same ID with a different body conflicts; it does not authorize deletion. Atomically persist replay/receipt state so crashes do not create ambiguous acceptance.
7. Only after durable raw commit, return `Ack`. Analysis is a separate durable concern, not a prerequisite claimed by this receipt.

Retry retains the same batch ID, event identities and exact body bytes, but generates a new nonce and current timestamp/signature. No content-based deduplication `(serial, tag, counter, body)` substitutes for source identity across counter resets.

## Ack

`Ack` fields are `version: u32`, `device_id: String`, `batch_id: String`, `body_sha256: String`, `durable: bool`, and `accepted_unix_ms: i64` (nonnegative Unix milliseconds).

`Ack::validate(&batch, exact_sent_body)` requires version 1, exact device ID and batch ID, lowercase body hash of the exact sent bytes, and `durable == true`. It does not attest to analysis completion. A missing/malformed/mismatched/non-durable ack never authorizes local outbox deletion. The timestamp is server acceptance time, not a mapped physiological time.

## Public synthetic vectors

The normative fixed values are public constants under `oura_protocol_v1::ingest::test_vectors`: `KEY`, `KEY_ID`, `DEVICE_ID`, and the `BATCH_*` / `HEARTBEAT_*` bodies, timestamps, nonces, canonical strings, hashes and signatures. `BATCH_ACK` provides a matching durable receipt. These values are synthetic, contain no real ring data, and must never become production credentials. Expected hashes/signatures were computed independently with Python standard-library `hashlib`/`hmac`, not from the Rust implementation. Do not update expectations automatically from code under test.

The vectors use exact compact JSON bytes, with no BOM or trailing LF. The `legacy_event_packet` vector retains raw `4a08e80300000102a0ff`: tag `4a`, length `08`, counter `e8030000` (1000 deciseconds), body `0102a0ff`. The second event deliberately has an empty body and null transport/anchor. The heartbeat-only vector has empty events and null metadata.

Consumer-visible Rust tests are in `crates/oura-protocol-v1/tests/ingest.rs`. They cover fixed values, byte-exact DTO serialization, signature tampering, clock boundaries, malformed fields, schema rejection, validation bounds, transport rules, heartbeat rules, ack mismatch and wire size. Tests do not contact hardware or a backend. Run from the worktree after integration:

```sh
cargo test -p oura-protocol-v1
```

### Fixed vector values

Public key hex (32 raw bytes):

```text
000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f
```

#### Event batch

Exact body (1040 bytes; the code-block newline is not part of the body):

```json
{"version":1,"device_id":"pi-test-01","collector_epoch":"0123456789abcdef0123456789abcdef","batch_id":"b-00000000000000000001","collector_build":"oura-collector 0.1.0","device":{"serial":"SYNTH0001","firmware":"2.9.31","hardware":"gen3-synthetic"},"events":[{"source_row_id":1,"tag":74,"ring_counter":1000,"body_hex":"0102a0ff","captured_unix_ms":1767225590000,"clock_anchor":{"kind":"observed","ring_counter":1000,"captured_unix_ms":1767225590000},"transport":{"encoding":"legacy_event_packet","raw_hex":"4a08e80300000102a0ff","counter_value":1000,"counter_unit":"decisecond"}},{"source_row_id":2,"tag":96,"ring_counter":1003,"body_hex":"","captured_unix_ms":1767225590000,"clock_anchor":null,"transport":null}],"heartbeat":{"ble_attempt_unix_ms":1767225580000,"ble_success_unix_ms":1767225590000,"upload_receipt_unix_ms":null,"queue_count":2,"queue_bytes":4,"queue_oldest_unix_ms":1767225590000,"battery_percent":87,"disk_free_bytes":1000000000,"pi_temperature_millicelsius":48500,"clock_synchronized":true,"error_codes":["ring_failed"]}}
```

- Timestamp: `1767225600`
- Nonce: `000102030405060708090a0b0c0d0e0f`
- Body SHA-256: `1a7d6fc4d003c94ff24713e6a15e5fbbc5b06bf8d2491ce46f21f9c84caa9d4a`
- HMAC-SHA-256: `a9ad3257e7351c7211a9bbb4b1c4e5543fdce73a66c17da20c6a9f4842ddf342`

Exact canonical bytes (the final newline shown inside this block is part of the input):

```text
open-health-ingest-v1
pi-key-1
pi-test-01
POST
/ingest/v1/batches
1767225600
000102030405060708090a0b0c0d0e0f
1a7d6fc4d003c94ff24713e6a15e5fbbc5b06bf8d2491ce46f21f9c84caa9d4a
```

#### Heartbeat batch

Exact body (580 bytes; the code-block newline is not part of the body):

```json
{"version":1,"device_id":"pi-test-01","collector_epoch":"0123456789abcdef0123456789abcdef","batch_id":"b-00000000000000000002","collector_build":"oura-collector 0.1.0","device":{"serial":null,"firmware":null,"hardware":null},"events":[],"heartbeat":{"ble_attempt_unix_ms":1767225580000,"ble_success_unix_ms":null,"upload_receipt_unix_ms":1767225500000,"queue_count":0,"queue_bytes":0,"queue_oldest_unix_ms":null,"battery_percent":null,"disk_free_bytes":null,"pi_temperature_millicelsius":null,"clock_synchronized":false,"error_codes":["operation_timeout","clock_unsynchronized"]}}
```

- Timestamp: `1767225660`
- Nonce: `f0e1d2c3b4a5968778695a4b3c2d1e0f`
- Body SHA-256: `01056534793a81e9def527d0c87798ef76b926732ef600046a554e44bece8579`
- HMAC-SHA-256: `92543346947a3e6ffa892c964efeab20fa3c1eea6e1dc991d6c28dbdbb5e9f67`

Exact canonical bytes (the final newline shown inside this block is part of the input):

```text
open-health-ingest-v1
pi-key-1
pi-test-01
POST
/ingest/v1/batches
1767225660
f0e1d2c3b4a5968778695a4b3c2d1e0f
01056534793a81e9def527d0c87798ef76b926732ef600046a554e44bece8579
```

Matching event-batch ack:

```json
{"version":1,"device_id":"pi-test-01","batch_id":"b-00000000000000000001","body_sha256":"1a7d6fc4d003c94ff24713e6a15e5fbbc5b06bf8d2491ce46f21f9c84caa9d4a","durable":true,"accepted_unix_ms":1767225601000}
```

Independent reproduction of the event-batch vector with the Python standard library:

```python
import hashlib
import hmac

key = bytes(range(32))  # Public synthetic key only.
body = b'{"version":1,"device_id":"pi-test-01","collector_epoch":"0123456789abcdef0123456789abcdef","batch_id":"b-00000000000000000001","collector_build":"oura-collector 0.1.0","device":{"serial":"SYNTH0001","firmware":"2.9.31","hardware":"gen3-synthetic"},"events":[{"source_row_id":1,"tag":74,"ring_counter":1000,"body_hex":"0102a0ff","captured_unix_ms":1767225590000,"clock_anchor":{"kind":"observed","ring_counter":1000,"captured_unix_ms":1767225590000},"transport":{"encoding":"legacy_event_packet","raw_hex":"4a08e80300000102a0ff","counter_value":1000,"counter_unit":"decisecond"}},{"source_row_id":2,"tag":96,"ring_counter":1003,"body_hex":"","captured_unix_ms":1767225590000,"clock_anchor":null,"transport":null}],"heartbeat":{"ble_attempt_unix_ms":1767225580000,"ble_success_unix_ms":1767225590000,"upload_receipt_unix_ms":null,"queue_count":2,"queue_bytes":4,"queue_oldest_unix_ms":1767225590000,"battery_percent":87,"disk_free_bytes":1000000000,"pi_temperature_millicelsius":48500,"clock_synchronized":true,"error_codes":["ring_failed"]}}'  # Event-batch vector, no trailing newline.
timestamp = 1_767_225_600  # Event batch; use 1_767_225_660 for heartbeat.
nonce = "000102030405060708090a0b0c0d0e0f"  # Heartbeat: f0e1d2c3b4a5968778695a4b3c2d1e0f.
body_sha256 = hashlib.sha256(body).hexdigest()
canonical = (
    "open-health-ingest-v1\npi-key-1\npi-test-01\nPOST\n"
    "/ingest/v1/batches\n"
    f"{timestamp}\n{nonce}\n{body_sha256}\n"
).encode("utf-8")
print(body_sha256)
print(hmac.new(key, canonical, hashlib.sha256).hexdigest())
```
