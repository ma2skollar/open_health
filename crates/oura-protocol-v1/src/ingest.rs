//! Version-one signed raw-batch ingest contract shared by the Pi collector
//! and the backend (`POST /ingest/v1/batches`).
//!
//! The collector serialises one [`Batch`] with [`Batch::to_bytes`], hashes the
//! exact transmitted bytes with [`body_hash`], and signs [`SigningFields`]
//! with a per-device 32-byte HMAC-SHA-256 key. The backend recomputes the
//! hash over the bytes it received, calls [`SigningFields::verify`], enforces
//! persistent replay protection for `(key_id, nonce)` itself, parses and
//! validates the batch, commits it durably and only then returns an [`Ack`].
//!
//! Ring counters are opaque ring-local values. Neither events nor clock
//! anchors claim a UTC time for the physiological event; `captured_unix_ms`
//! is the Pi wall clock when the collector observed the record.
//!
//! The full normative description lives in `docs/pi-ingest-contract.md`.
//! Public synthetic interoperability vectors are in [`test_vectors`].

use std::collections::HashSet;
use std::fmt;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Envelope version carried in [`Batch::version`] and [`Ack::version`].
pub const VERSION: u32 = 1;
/// The only accepted HTTP method.
pub const METHOD: &str = "POST";
/// The only accepted request path (no query string, no trailing slash).
pub const PATH: &str = "/ingest/v1/batches";
/// The only accepted `Content-Type` value, compared exactly.
pub const CONTENT_TYPE: &str = "application/json";
/// Maximum transmitted body size in bytes (4 MiB).
pub const MAX_BODY_BYTES: usize = 4_194_304;
/// Maximum number of events in one batch.
pub const MAX_EVENTS: usize = 10_000;
/// Maximum decoded size of one event body in bytes (64 KiB).
pub const MAX_EVENT_BODY_BYTES: usize = 65_536;
/// Accepted absolute difference between signed timestamp and verifier clock.
pub const CLOCK_WINDOW_SECONDS: i64 = 300;

/// Domain separator: first line of the canonical signing string.
pub const DOMAIN: &str = "open-health-ingest-v1";
/// Required HMAC key length in bytes.
pub const KEY_BYTES: usize = 32;
/// Nonce length in lowercase hex characters (16 random bytes).
pub const NONCE_HEX_LEN: usize = 32;
/// Maximum length of identifiers, build string and device metadata values.
pub const MAX_IDENTIFIER_LEN: usize = 128;
/// Maximum number of heartbeat error codes.
pub const MAX_ERROR_CODES: usize = 16;
/// Maximum length of one heartbeat error code.
pub const MAX_ERROR_CODE_LEN: usize = 64;
/// Maximum decoded size of [`TransportProvenance::raw_hex`]: one event body
/// plus 1 KiB of envelope framing.
pub const MAX_TRANSPORT_RAW_BYTES: usize = MAX_EVENT_BODY_BYTES + 1024;

/// Transport encoding: exact legacy history packet `tag | len | payload`,
/// where the payload starts with the u32 little-endian decisecond counter.
pub const TRANSPORT_LEGACY_EVENT_PACKET: &str = "legacy_event_packet";
/// Transport encoding: exact extended bundle record
/// `control | tag | ext_len | varint | body`, excluding chunk headers and the
/// u16 envelope length.
pub const TRANSPORT_EXT_EVENT_RECORD: &str = "ext_event_record";
/// Counter unit of [`TRANSPORT_LEGACY_EVENT_PACKET`].
pub const UNIT_DECISECOND: &str = "decisecond";
/// Extended record varint interpreted as an absolute ring millisecond counter.
pub const UNIT_MILLISECOND_ABSOLUTE: &str = "millisecond_absolute";
/// Extended record varint interpreted as a delta from the previous record.
pub const UNIT_MILLISECOND_DELTA: &str = "millisecond_delta";

pub const HEADER_KEY_ID: &str = "x-oura-key-id";
pub const HEADER_DEVICE_ID: &str = "x-oura-device-id";
pub const HEADER_TIMESTAMP: &str = "x-oura-timestamp";
pub const HEADER_NONCE: &str = "x-oura-nonce";
pub const HEADER_BODY_SHA256: &str = "x-oura-body-sha256";
pub const HEADER_SIGNATURE: &str = "x-oura-signature";

/// Clock anchor kind: the collector observed `ring_counter` at `captured_unix_ms`.
pub const ANCHOR_OBSERVED: &str = "observed";
/// Clock anchor kind: evidence of a ring reboot / counter reset boundary.
pub const ANCHOR_BOOT_BOUNDARY: &str = "boot_boundary";

const SHA256_HEX_LEN: usize = 64;
const SIGNATURE_HEX_LEN: usize = 64;

type HmacSha256 = Hmac<Sha256>;

/// Contract violation. Messages name fields only and never include key
/// material, signatures or payload bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContractError {
    /// `version` is not [`VERSION`].
    Version(u32),
    /// A field violates its documented format or bound.
    Field { field: &'static str, reason: &'static str },
    /// Serialised body exceeds [`MAX_BODY_BYTES`].
    BodyTooLarge(usize),
    /// JSON (de)serialisation failed.
    Json(String),
    /// HMAC key is not exactly [`KEY_BYTES`] long.
    KeyLength(usize),
    /// Signed timestamp is outside ±[`CLOCK_WINDOW_SECONDS`] of the verifier clock.
    ClockSkew,
    /// Signature is malformed or does not match.
    BadSignature,
    /// Ack does not confirm the exact batch/body that was sent.
    AckMismatch(&'static str),
}

impl fmt::Display for ContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Version(v) => write!(f, "unsupported ingest version {v}"),
            Self::Field { field, reason } => write!(f, "invalid {field}: {reason}"),
            Self::BodyTooLarge(n) => write!(f, "body is {n} bytes, limit {MAX_BODY_BYTES}"),
            Self::Json(e) => write!(f, "json: {e}"),
            Self::KeyLength(n) => write!(f, "hmac key is {n} bytes, expected {KEY_BYTES}"),
            Self::ClockSkew => write!(f, "timestamp outside ±{CLOCK_WINDOW_SECONDS}s window"),
            Self::BadSignature => f.write_str("bad signature"),
            Self::AckMismatch(field) => write!(f, "ack {field} does not match sent batch"),
        }
    }
}

impl std::error::Error for ContractError {}

fn field(field: &'static str, reason: &'static str) -> ContractError {
    ContractError::Field { field, reason }
}

/// One upload: raw ring records plus collector health. A heartbeat-only
/// upload is a batch with an empty `events` array.
///
/// `batch_id` is immutable: a retry resends identical bytes with a fresh
/// nonce. `collector_epoch` persists until an explicit new collector store is
/// created; it never changes on process boot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    pub version: u32,
    pub device_id: String,
    pub collector_epoch: String,
    pub batch_id: String,
    pub collector_build: String,
    pub device: DeviceMetadata,
    pub events: Vec<RawEvent>,
    pub heartbeat: Heartbeat,
}

/// Ring metadata as reported by the ring, if known.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceMetadata {
    pub serial: Option<String>,
    pub firmware: Option<String>,
    pub hardware: Option<String>,
}

/// One undecoded ring history record. Delivery identity is
/// `(device_id, collector_epoch, source_row_id)`; there is no content dedup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawEvent {
    pub source_row_id: u64,
    pub tag: u32,
    pub ring_counter: u64,
    pub body_hex: String,
    pub captured_unix_ms: i64,
    pub clock_anchor: Option<ClockAnchor>,
    /// Original transport envelope, when the collector has it. The key is
    /// always present on the wire (`null` when absent).
    pub transport: Option<TransportProvenance>,
}

/// Exact transport bytes and counter as received from the ring, before any
/// upstream parser synthesises timestamps (for example from varint deltas).
/// The backend reconstructs timing from these; the collector never invents a
/// UTC time or counter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransportProvenance {
    /// [`TRANSPORT_LEGACY_EVENT_PACKET`] or [`TRANSPORT_EXT_EVENT_RECORD`].
    pub encoding: String,
    /// Lowercase hex of the original envelope bytes, unmodified, non-empty.
    pub raw_hex: String,
    /// Counter exactly as decoded from the envelope, in `counter_unit`.
    pub counter_value: u64,
    /// [`UNIT_DECISECOND`] for legacy packets; [`UNIT_MILLISECOND_ABSOLUTE`]
    /// or [`UNIT_MILLISECOND_DELTA`] for extended records.
    pub counter_unit: String,
}

/// Observation linking a ring counter to the Pi wall clock. This is
/// provenance for later reconstruction, not a mapped event UTC time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockAnchor {
    /// [`ANCHOR_OBSERVED`] or [`ANCHOR_BOOT_BOUNDARY`].
    pub kind: String,
    pub ring_counter: u64,
    pub captured_unix_ms: i64,
}

/// Collector health snapshot carried by every batch.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Heartbeat {
    pub ble_attempt_unix_ms: Option<i64>,
    pub ble_success_unix_ms: Option<i64>,
    pub upload_receipt_unix_ms: Option<i64>,
    pub queue_count: u64,
    pub queue_bytes: u64,
    pub queue_oldest_unix_ms: Option<i64>,
    pub battery_percent: Option<u8>,
    pub disk_free_bytes: Option<u64>,
    pub pi_temperature_millicelsius: Option<i64>,
    pub clock_synchronized: bool,
    /// Short snake_case codes (`[a-z0-9_]`), never free text.
    pub error_codes: Vec<String>,
}

/// Backend receipt, sent only after the raw batch is durably committed.
/// It does not claim analysis completion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ack {
    pub version: u32,
    pub device_id: String,
    pub batch_id: String,
    pub body_sha256: String,
    pub durable: bool,
    pub accepted_unix_ms: i64,
}

fn check_identifier(name: &'static str, value: &str) -> Result<(), ContractError> {
    if value.is_empty() {
        return Err(field(name, "empty"));
    }
    if value.len() > MAX_IDENTIFIER_LEN {
        return Err(field(name, "longer than 128 bytes"));
    }
    if !value.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')) {
        return Err(field(name, "not [A-Za-z0-9._-]"));
    }
    Ok(())
}

fn check_printable(name: &'static str, value: &str) -> Result<(), ContractError> {
    if value.is_empty() {
        return Err(field(name, "empty"));
    }
    if value.len() > MAX_IDENTIFIER_LEN {
        return Err(field(name, "longer than 128 bytes"));
    }
    if !value.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return Err(field(name, "not printable ASCII"));
    }
    Ok(())
}

fn is_lower_hex(value: &str) -> bool {
    value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn check_lower_hex(name: &'static str, value: &str, len: usize) -> Result<(), ContractError> {
    if value.len() != len || !is_lower_hex(value) {
        return Err(field(name, "not fixed-length lowercase hex"));
    }
    Ok(())
}

fn check_time(name: &'static str, value: i64) -> Result<(), ContractError> {
    if value < 0 {
        return Err(field(name, "negative"));
    }
    Ok(())
}

fn check_opt_time(name: &'static str, value: Option<i64>) -> Result<(), ContractError> {
    value.map_or(Ok(()), |v| check_time(name, v))
}

impl Batch {
    /// Checks every documented field rule without serialising.
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.version != VERSION {
            return Err(ContractError::Version(self.version));
        }
        check_identifier("device_id", &self.device_id)?;
        check_identifier("collector_epoch", &self.collector_epoch)?;
        check_identifier("batch_id", &self.batch_id)?;
        check_printable("collector_build", &self.collector_build)?;
        self.device.validate()?;
        if self.events.len() > MAX_EVENTS {
            return Err(field("events", "more than 10000 events"));
        }
        let mut seen = HashSet::with_capacity(self.events.len());
        for event in &self.events {
            event.validate()?;
            if !seen.insert(event.source_row_id) {
                return Err(field("events.source_row_id", "duplicate within batch"));
            }
        }
        self.heartbeat.validate()
    }

    /// Validates, serialises as compact JSON and enforces [`MAX_BODY_BYTES`].
    /// The returned bytes are exactly what must be hashed and transmitted.
    pub fn to_bytes(&self) -> Result<Vec<u8>, ContractError> {
        self.validate()?;
        let mut writer = BoundedBody { bytes: Vec::new(), overflow: None };
        let result = serde_json::to_writer(&mut writer, self);
        if let Some(size) = writer.overflow {
            return Err(ContractError::BodyTooLarge(size));
        }
        result.map_err(|e| ContractError::Json(e.to_string()))?;
        Ok(writer.bytes)
    }
}

/// Stops before allocating more than the wire limit, even for a valid batch
/// containing 10,000 individually maximum-sized event bodies.
struct BoundedBody {
    bytes: Vec<u8>,
    overflow: Option<usize>,
}

impl std::io::Write for BoundedBody {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let size = self.bytes.len().saturating_add(bytes.len());
        if size > MAX_BODY_BYTES {
            self.overflow = Some(size);
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "ingest body limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl DeviceMetadata {
    fn validate(&self) -> Result<(), ContractError> {
        if let Some(v) = &self.serial {
            check_printable("device.serial", v)?;
        }
        if let Some(v) = &self.firmware {
            check_printable("device.firmware", v)?;
        }
        if let Some(v) = &self.hardware {
            check_printable("device.hardware", v)?;
        }
        Ok(())
    }
}

impl RawEvent {
    fn validate(&self) -> Result<(), ContractError> {
        if self.source_row_id == 0 {
            return Err(field("events.source_row_id", "zero"));
        }
        if self.tag > u32::from(u8::MAX) {
            return Err(field("events.tag", "greater than 255"));
        }
        if self.ring_counter > u64::from(u32::MAX) {
            return Err(field("events.ring_counter", "greater than u32::MAX"));
        }
        if self.body_hex.len() % 2 != 0 || !is_lower_hex(&self.body_hex) {
            return Err(field("events.body_hex", "not even-length lowercase hex"));
        }
        if self.body_hex.len() / 2 > MAX_EVENT_BODY_BYTES {
            return Err(field("events.body_hex", "decoded body larger than 65536 bytes"));
        }
        check_time("events.captured_unix_ms", self.captured_unix_ms)?;
        if let Some(anchor) = &self.clock_anchor {
            anchor.validate()?;
        }
        if let Some(transport) = &self.transport {
            transport.validate(self.ring_counter)?;
        }
        Ok(())
    }
}

impl TransportProvenance {
    /// `ring_counter` is the event's decisecond counter; it must be derived
    /// from the transport counter, never synthesised.
    fn validate(&self, ring_counter: u64) -> Result<(), ContractError> {
        let expected_ring_counter = match (self.encoding.as_str(), self.counter_unit.as_str()) {
            (TRANSPORT_LEGACY_EVENT_PACKET, UNIT_DECISECOND) => Some(self.counter_value),
            (TRANSPORT_EXT_EVENT_RECORD, UNIT_MILLISECOND_ABSOLUTE) => Some(self.counter_value / 100),
            (TRANSPORT_EXT_EVENT_RECORD, UNIT_MILLISECOND_DELTA) => None,
            (TRANSPORT_LEGACY_EVENT_PACKET | TRANSPORT_EXT_EVENT_RECORD, _) => {
                return Err(field("transport.counter_unit", "not allowed for encoding"));
            }
            _ => return Err(field("transport.encoding", "not legacy_event_packet or ext_event_record")),
        };
        if expected_ring_counter.is_some_and(|expected| expected != ring_counter) {
            return Err(field("transport.counter_value", "does not match events.ring_counter"));
        }
        if self.raw_hex.is_empty() || self.raw_hex.len() % 2 != 0 || !is_lower_hex(&self.raw_hex) {
            return Err(field("transport.raw_hex", "not non-empty even-length lowercase hex"));
        }
        if self.raw_hex.len() / 2 > MAX_TRANSPORT_RAW_BYTES {
            return Err(field("transport.raw_hex", "decoded envelope larger than 66560 bytes"));
        }
        Ok(())
    }
}

impl ClockAnchor {
    fn validate(&self) -> Result<(), ContractError> {
        if self.kind != ANCHOR_OBSERVED && self.kind != ANCHOR_BOOT_BOUNDARY {
            return Err(field("clock_anchor.kind", "not observed or boot_boundary"));
        }
        if self.ring_counter > u64::from(u32::MAX) {
            return Err(field("clock_anchor.ring_counter", "greater than u32::MAX"));
        }
        check_time("clock_anchor.captured_unix_ms", self.captured_unix_ms)
    }
}

/// Frozen safe heartbeat error codes. These contain no payload, identifiers,
/// paths, key material, or free-form library error messages.
pub const ERROR_CODES: &[&str] = &[
    "io_failed", "sqlite_failed", "json_failed", "config_invalid",
    "key_permissions", "key_invalid", "state_busy", "state_invalid",
    "outbox_pressure", "disk_reserve", "clock_unsynchronized", "upload_failed",
    "ack_invalid", "contract_invalid", "cursor_regression", "history_limit",
    "ring_failed", "operation_timeout",
];

impl Heartbeat {
    fn validate(&self) -> Result<(), ContractError> {
        check_opt_time("heartbeat.ble_attempt_unix_ms", self.ble_attempt_unix_ms)?;
        check_opt_time("heartbeat.ble_success_unix_ms", self.ble_success_unix_ms)?;
        check_opt_time("heartbeat.upload_receipt_unix_ms", self.upload_receipt_unix_ms)?;
        check_opt_time("heartbeat.queue_oldest_unix_ms", self.queue_oldest_unix_ms)?;
        if self.battery_percent.is_some_and(|b| b > 100) {
            return Err(field("heartbeat.battery_percent", "greater than 100"));
        }
        if self.error_codes.len() > MAX_ERROR_CODES {
            return Err(field("heartbeat.error_codes", "more than 16 codes"));
        }
        for code in &self.error_codes {
            if code.is_empty() || code.len() > MAX_ERROR_CODE_LEN {
                return Err(field("heartbeat.error_codes", "code empty or longer than 64 bytes"));
            }
            if !code.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_')) {
                return Err(field("heartbeat.error_codes", "code not [a-z0-9_]"));
            }
            if !ERROR_CODES.contains(&code.as_str()) {
                return Err(field("heartbeat.error_codes", "not an allowlisted code"));
            }
        }
        Ok(())
    }
}

impl Ack {
    /// Confirms this ack covers exactly `batch` sent as `body`, durably.
    /// A mismatch never authorises the collector to delete local data.
    pub fn validate(&self, batch: &Batch, body: &[u8]) -> Result<(), ContractError> {
        if self.version != VERSION {
            return Err(ContractError::Version(self.version));
        }
        if self.device_id != batch.device_id {
            return Err(ContractError::AckMismatch("device_id"));
        }
        if self.batch_id != batch.batch_id {
            return Err(ContractError::AckMismatch("batch_id"));
        }
        if self.body_sha256 != body_hash(body) {
            return Err(ContractError::AckMismatch("body_sha256"));
        }
        if !self.durable {
            return Err(ContractError::AckMismatch("durable"));
        }
        check_time("ack.accepted_unix_ms", self.accepted_unix_ms)
    }
}

/// Lowercase hex SHA-256 of the exact transmitted body bytes.
pub fn body_hash(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))
}

/// Values carried in the signing headers. The method and path are fixed
/// constants and are part of the canonical string, not of this struct.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SigningFields {
    pub key_id: String,
    pub device_id: String,
    /// Integer UTC unix seconds.
    pub timestamp: i64,
    /// 32 lowercase hex characters (16 random bytes), fresh per request.
    pub nonce: String,
    /// [`body_hash`] of the exact transmitted body.
    pub body_sha256: String,
}

impl SigningFields {
    /// Canonical signing string: eight `\n`-terminated lines (domain, key ID,
    /// device ID, method, path, timestamp, nonce, body hash). Fields are
    /// validated first, so no escaping is needed.
    pub fn canonical(&self) -> Result<Vec<u8>, ContractError> {
        check_identifier("key_id", &self.key_id)?;
        check_identifier("device_id", &self.device_id)?;
        check_time("timestamp", self.timestamp)?;
        check_lower_hex("nonce", &self.nonce, NONCE_HEX_LEN)?;
        check_lower_hex("body_sha256", &self.body_sha256, SHA256_HEX_LEN)?;
        Ok(format!(
            "{DOMAIN}\n{}\n{}\n{METHOD}\n{PATH}\n{}\n{}\n{}\n",
            self.key_id, self.device_id, self.timestamp, self.nonce, self.body_sha256
        )
        .into_bytes())
    }

    fn mac(&self, key: &[u8]) -> Result<HmacSha256, ContractError> {
        if key.len() != KEY_BYTES {
            return Err(ContractError::KeyLength(key.len()));
        }
        let canonical = self.canonical()?;
        let mut mac = HmacSha256::new_from_slice(key).map_err(|_| ContractError::KeyLength(key.len()))?;
        mac.update(&canonical);
        Ok(mac)
    }

    /// Lowercase hex HMAC-SHA-256 of [`Self::canonical`] under a 32-byte key.
    pub fn sign(&self, key: &[u8]) -> Result<String, ContractError> {
        Ok(hex::encode(self.mac(key)?.finalize().into_bytes()))
    }

    /// Checks field format, `|now - timestamp| <= 300` seconds and the HMAC
    /// in constant time. Nonce replay tracking is the caller's (backend's)
    /// responsibility and must be persistent and atomic.
    pub fn verify(&self, key: &[u8], signature: &str, now: i64) -> Result<(), ContractError> {
        let mac = self.mac(key)?;
        if now.abs_diff(self.timestamp) > CLOCK_WINDOW_SECONDS.unsigned_abs() {
            return Err(ContractError::ClockSkew);
        }
        if signature.len() != SIGNATURE_HEX_LEN || !is_lower_hex(signature) {
            return Err(ContractError::BadSignature);
        }
        let expected = hex::decode(signature).map_err(|_| ContractError::BadSignature)?;
        mac.verify_slice(&expected).map_err(|_| ContractError::BadSignature)
    }
}

/// Fixed public synthetic interoperability vectors. The key is the public
/// sequence `00..1f`; it is not, and must never be used as, a real credential.
/// Expected hashes and signatures were computed independently of this crate
/// (Python `hashlib`/`hmac`) and are frozen.
pub mod test_vectors {
    /// Public synthetic HMAC key: bytes `0x00..=0x1f`.
    pub const KEY: [u8; 32] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11,
        0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
    ];
    pub const KEY_ID: &str = "pi-key-1";
    pub const DEVICE_ID: &str = "pi-test-01";

    /// Batch with two events: one with an observed clock anchor and its synthetic
    /// legacy transport packet, one with an empty body and neither.
    pub const BATCH_BODY: &str = concat!(
        r#"{"version":1,"device_id":"pi-test-01","collector_epoch":"0123456789abcdef0123456789abcdef","#,
        r#""batch_id":"b-00000000000000000001","collector_build":"oura-collector 0.1.0","#,
        r#""device":{"serial":"SYNTH0001","firmware":"2.9.31","hardware":"gen3-synthetic"},"#,
        r#""events":[{"source_row_id":1,"tag":74,"ring_counter":1000,"body_hex":"0102a0ff","#,
        r#""captured_unix_ms":1767225590000,"clock_anchor":{"kind":"observed","ring_counter":1000,"#,
        r#""captured_unix_ms":1767225590000},"transport":{"encoding":"legacy_event_packet","#,
        r#""raw_hex":"4a08e80300000102a0ff","counter_value":1000,"counter_unit":"decisecond"}},"#,
        r#"{"source_row_id":2,"tag":96,"ring_counter":1003,"body_hex":"","captured_unix_ms":1767225590000,"#,
        r#""clock_anchor":null,"transport":null}],"heartbeat":{"ble_attempt_unix_ms":1767225580000,"#,
        r#""ble_success_unix_ms":1767225590000,"upload_receipt_unix_ms":null,"queue_count":2,"#,
        r#""queue_bytes":4,"queue_oldest_unix_ms":1767225590000,"battery_percent":87,"#,
        r#""disk_free_bytes":1000000000,"pi_temperature_millicelsius":48500,"clock_synchronized":true,"#,
        r#""error_codes":["ring_failed"]}}"#,
    );
    pub const BATCH_BODY_SHA256: &str = "1a7d6fc4d003c94ff24713e6a15e5fbbc5b06bf8d2491ce46f21f9c84caa9d4a";
    pub const BATCH_TIMESTAMP: i64 = 1_767_225_600;
    pub const BATCH_NONCE: &str = "000102030405060708090a0b0c0d0e0f";
    pub const BATCH_CANONICAL: &str = concat!(
        "open-health-ingest-v1\n",
        "pi-key-1\n",
        "pi-test-01\n",
        "POST\n",
        "/ingest/v1/batches\n",
        "1767225600\n",
        "000102030405060708090a0b0c0d0e0f\n",
        "1a7d6fc4d003c94ff24713e6a15e5fbbc5b06bf8d2491ce46f21f9c84caa9d4a\n",
    );
    pub const BATCH_SIGNATURE: &str = "a9ad3257e7351c7211a9bbb4b1c4e5543fdce73a66c17da20c6a9f4842ddf342";

    /// Heartbeat-only batch (empty events, unknown metadata).
    pub const HEARTBEAT_BODY: &str = concat!(
        r#"{"version":1,"device_id":"pi-test-01","collector_epoch":"0123456789abcdef0123456789abcdef","#,
        r#""batch_id":"b-00000000000000000002","collector_build":"oura-collector 0.1.0","#,
        r#""device":{"serial":null,"firmware":null,"hardware":null},"events":[],"#,
        r#""heartbeat":{"ble_attempt_unix_ms":1767225580000,"ble_success_unix_ms":null,"#,
        r#""upload_receipt_unix_ms":1767225500000,"queue_count":0,"queue_bytes":0,"#,
        r#""queue_oldest_unix_ms":null,"battery_percent":null,"disk_free_bytes":null,"#,
        r#""pi_temperature_millicelsius":null,"clock_synchronized":false,"#,
        r#""error_codes":["operation_timeout","clock_unsynchronized"]}}"#,
    );
    pub const HEARTBEAT_BODY_SHA256: &str = "01056534793a81e9def527d0c87798ef76b926732ef600046a554e44bece8579";
    pub const HEARTBEAT_TIMESTAMP: i64 = 1_767_225_660;
    pub const HEARTBEAT_NONCE: &str = "f0e1d2c3b4a5968778695a4b3c2d1e0f";
    pub const HEARTBEAT_CANONICAL: &str = concat!(
        "open-health-ingest-v1\n",
        "pi-key-1\n",
        "pi-test-01\n",
        "POST\n",
        "/ingest/v1/batches\n",
        "1767225660\n",
        "f0e1d2c3b4a5968778695a4b3c2d1e0f\n",
        "01056534793a81e9def527d0c87798ef76b926732ef600046a554e44bece8579\n",
    );
    pub const HEARTBEAT_SIGNATURE: &str = "92543346947a3e6ffa892c964efeab20fa3c1eea6e1dc991d6c28dbdbb5e9f67";

    /// Durable ack for [`BATCH_BODY`].
    pub const BATCH_ACK: &str = concat!(
        r#"{"version":1,"device_id":"pi-test-01","batch_id":"b-00000000000000000001","#,
        r#""body_sha256":"1a7d6fc4d003c94ff24713e6a15e5fbbc5b06bf8d2491ce46f21f9c84caa9d4a","durable":true,"#,
        r#""accepted_unix_ms":1767225601000}"#,
    );
}
