//! Consumer-visible tests for `oura_protocol_v1::ingest`. Expected hashes and
//! signatures in `test_vectors` were computed independently (Python
//! `hashlib`/`hmac`), so these tests check the Rust implementation against
//! frozen external values rather than against itself.

use oura_protocol_v1::ingest::{
    body_hash, test_vectors as tv, Ack, Batch, ClockAnchor, ContractError, DeviceMetadata, Heartbeat, RawEvent,
    SigningFields, TransportProvenance, CLOCK_WINDOW_SECONDS, MAX_BODY_BYTES, MAX_EVENTS, MAX_EVENT_BODY_BYTES,
    VERSION,
};

fn batch_fields() -> SigningFields {
    SigningFields {
        key_id: tv::KEY_ID.into(),
        device_id: tv::DEVICE_ID.into(),
        timestamp: tv::BATCH_TIMESTAMP,
        nonce: tv::BATCH_NONCE.into(),
        body_sha256: tv::BATCH_BODY_SHA256.into(),
    }
}

fn vector_batch() -> Batch {
    serde_json::from_str(tv::BATCH_BODY).unwrap()
}

fn event(id: u64) -> RawEvent {
    RawEvent {
        source_row_id: id,
        tag: 0x4a,
        ring_counter: 1,
        body_hex: "00".into(),
        captured_unix_ms: 0,
        clock_anchor: None,
        transport: None,
    }
}

fn minimal_batch() -> Batch {
    Batch {
        version: VERSION,
        device_id: "pi-test-01".into(),
        collector_epoch: "epoch".into(),
        batch_id: "b-1".into(),
        collector_build: "build".into(),
        device: DeviceMetadata::default(),
        events: vec![event(1)],
        heartbeat: Heartbeat::default(),
    }
}

fn rejects(batch: &Batch, expected_field: &str) {
    match batch.validate() {
        Err(ContractError::Field { field, .. }) => assert_eq!(field, expected_field),
        other => panic!("expected field error on {expected_field}, got {other:?}"),
    }
}

#[test]
fn batch_vector_hash_canonical_and_signature_match_frozen_values() {
    assert_eq!(body_hash(tv::BATCH_BODY.as_bytes()), tv::BATCH_BODY_SHA256);
    let fields = batch_fields();
    assert_eq!(fields.canonical().unwrap(), tv::BATCH_CANONICAL.as_bytes());
    assert_eq!(fields.sign(&tv::KEY).unwrap(), tv::BATCH_SIGNATURE);
    fields.verify(&tv::KEY, tv::BATCH_SIGNATURE, tv::BATCH_TIMESTAMP).unwrap();
}

#[test]
fn heartbeat_vector_hash_canonical_and_signature_match_frozen_values() {
    assert_eq!(body_hash(tv::HEARTBEAT_BODY.as_bytes()), tv::HEARTBEAT_BODY_SHA256);
    let fields = SigningFields {
        key_id: tv::KEY_ID.into(),
        device_id: tv::DEVICE_ID.into(),
        timestamp: tv::HEARTBEAT_TIMESTAMP,
        nonce: tv::HEARTBEAT_NONCE.into(),
        body_sha256: tv::HEARTBEAT_BODY_SHA256.into(),
    };
    assert_eq!(fields.canonical().unwrap(), tv::HEARTBEAT_CANONICAL.as_bytes());
    assert_eq!(fields.sign(&tv::KEY).unwrap(), tv::HEARTBEAT_SIGNATURE);
    fields.verify(&tv::KEY, tv::HEARTBEAT_SIGNATURE, tv::HEARTBEAT_TIMESTAMP).unwrap();
}

#[test]
fn vector_bodies_round_trip_byte_exact() {
    let batch = vector_batch();
    assert_eq!(batch.to_bytes().unwrap(), tv::BATCH_BODY.as_bytes());
    assert_eq!(batch.events.len(), 2);
    assert_eq!(batch.events[0].transport.as_ref().unwrap().raw_hex, "4a08e80300000102a0ff");
    assert_eq!(batch.events[1].transport, None);

    let heartbeat: Batch = serde_json::from_str(tv::HEARTBEAT_BODY).unwrap();
    assert!(heartbeat.events.is_empty());
    assert_eq!(heartbeat.to_bytes().unwrap(), tv::HEARTBEAT_BODY.as_bytes());
}

#[test]
fn verify_enforces_clock_window_inclusive() {
    let fields = batch_fields();
    let ts = tv::BATCH_TIMESTAMP;
    fields.verify(&tv::KEY, tv::BATCH_SIGNATURE, ts + CLOCK_WINDOW_SECONDS).unwrap();
    fields.verify(&tv::KEY, tv::BATCH_SIGNATURE, ts - CLOCK_WINDOW_SECONDS).unwrap();
    assert_eq!(fields.verify(&tv::KEY, tv::BATCH_SIGNATURE, ts + 301), Err(ContractError::ClockSkew));
    assert_eq!(fields.verify(&tv::KEY, tv::BATCH_SIGNATURE, ts - 301), Err(ContractError::ClockSkew));
}

#[test]
fn verify_rejects_tampering_and_bad_keys() {
    let fields = batch_fields();
    let now = tv::BATCH_TIMESTAMP;

    let mut flipped = tv::BATCH_SIGNATURE.to_owned();
    flipped.replace_range(0..1, if flipped.starts_with('0') { "1" } else { "0" });
    assert_eq!(fields.verify(&tv::KEY, &flipped, now), Err(ContractError::BadSignature));
    assert_eq!(fields.verify(&tv::KEY, &tv::BATCH_SIGNATURE.to_uppercase(), now), Err(ContractError::BadSignature));
    assert_eq!(fields.verify(&tv::KEY, &tv::BATCH_SIGNATURE[..62], now), Err(ContractError::BadSignature));
    assert_eq!(fields.verify(&tv::KEY, "", now), Err(ContractError::BadSignature));

    let mut other_key = tv::KEY;
    other_key[31] ^= 1;
    assert_eq!(fields.verify(&other_key, tv::BATCH_SIGNATURE, now), Err(ContractError::BadSignature));
    assert_eq!(fields.sign(&tv::KEY[..31]), Err(ContractError::KeyLength(31)));
    assert_eq!(fields.verify(&[0u8; 33], tv::BATCH_SIGNATURE, now), Err(ContractError::KeyLength(33)));

    for tamper in [
        SigningFields { key_id: "pi-key-2".into(), ..batch_fields() },
        SigningFields { device_id: "pi-test-02".into(), ..batch_fields() },
        SigningFields { timestamp: tv::BATCH_TIMESTAMP + 1, ..batch_fields() },
        SigningFields { nonce: "000102030405060708090a0b0c0d0e0e".into(), ..batch_fields() },
        SigningFields { body_sha256: tv::HEARTBEAT_BODY_SHA256.into(), ..batch_fields() },
    ] {
        assert_eq!(tamper.verify(&tv::KEY, tv::BATCH_SIGNATURE, now), Err(ContractError::BadSignature));
    }
}

#[test]
fn canonical_rejects_ambiguous_fields() {
    for (fields, name) in [
        (SigningFields { key_id: "pi\nkey".into(), ..batch_fields() }, "key_id"),
        (SigningFields { key_id: String::new(), ..batch_fields() }, "key_id"),
        (SigningFields { device_id: "a b".into(), ..batch_fields() }, "device_id"),
        (SigningFields { timestamp: -1, ..batch_fields() }, "timestamp"),
        (SigningFields { nonce: "000102030405060708090A0B0C0D0E0F".into(), ..batch_fields() }, "nonce"),
        (SigningFields { nonce: "0001020304050607".into(), ..batch_fields() }, "nonce"),
        (SigningFields { body_sha256: tv::BATCH_BODY_SHA256.to_uppercase(), ..batch_fields() }, "body_sha256"),
    ] {
        match fields.canonical() {
            Err(ContractError::Field { field, .. }) => assert_eq!(field, name),
            other => panic!("expected {name} rejection, got {other:?}"),
        }
    }
}

#[test]
fn ack_vector_validates_and_mismatches_are_rejected() {
    let batch = vector_batch();
    let body = tv::BATCH_BODY.as_bytes();
    let ack: Ack = serde_json::from_str(tv::BATCH_ACK).unwrap();
    ack.validate(&batch, body).unwrap();

    let bad = |a: Ack| a.validate(&batch, body).unwrap_err();
    assert_eq!(bad(Ack { device_id: "pi-test-02".into(), ..ack.clone() }), ContractError::AckMismatch("device_id"));
    assert_eq!(bad(Ack { batch_id: "b-2".into(), ..ack.clone() }), ContractError::AckMismatch("batch_id"));
    assert_eq!(
        bad(Ack { body_sha256: tv::HEARTBEAT_BODY_SHA256.into(), ..ack.clone() }),
        ContractError::AckMismatch("body_sha256")
    );
    assert_eq!(bad(Ack { durable: false, ..ack.clone() }), ContractError::AckMismatch("durable"));
    assert_eq!(bad(Ack { version: 2, ..ack.clone() }), ContractError::Version(2));
    assert_eq!(ack.validate(&batch, tv::HEARTBEAT_BODY.as_bytes()), Err(ContractError::AckMismatch("body_sha256")));
}

#[test]
fn unknown_fields_are_rejected() {
    assert!(serde_json::from_str::<Batch>(&tv::BATCH_BODY[..tv::BATCH_BODY.len() - 1]).is_err());
    let duplicate = tv::BATCH_BODY.replacen("{\"version\":1,", "{\"version\":1,\"version\":1,", 1);
    assert!(serde_json::from_str::<Batch>(&duplicate).is_err());
    let missing_required = tv::BATCH_BODY.replacen("\"version\":1,", "", 1);
    assert!(serde_json::from_str::<Batch>(&missing_required).is_err());
    let extra_top = tv::BATCH_BODY.replacen("{\"version\":1,", "{\"version\":1,\"extra\":0,", 1);
    assert!(serde_json::from_str::<Batch>(&extra_top).is_err());
    let extra_event = tv::BATCH_BODY.replacen("\"source_row_id\":1,", "\"source_row_id\":1,\"utc\":0,", 1);
    assert!(serde_json::from_str::<Batch>(&extra_event).is_err());
    let extra_heartbeat = tv::BATCH_BODY.replacen("\"queue_count\":2,", "\"queue_count\":2,\"x\":1,", 1);
    assert!(serde_json::from_str::<Batch>(&extra_heartbeat).is_err());
    let extra_ack = tv::BATCH_ACK.replacen("\"durable\":true,", "\"durable\":true,\"analysed\":true,", 1);
    assert!(serde_json::from_str::<Ack>(&extra_ack).is_err());
}

#[test]
fn batch_validation_rules() {
    minimal_batch().validate().unwrap();

    assert_eq!(Batch { version: 2, ..minimal_batch() }.validate(), Err(ContractError::Version(2)));
    rejects(&Batch { device_id: String::new(), ..minimal_batch() }, "device_id");
    rejects(&Batch { collector_epoch: "a/b".into(), ..minimal_batch() }, "collector_epoch");
    rejects(&Batch { batch_id: "x".repeat(129), ..minimal_batch() }, "batch_id");
    Batch { batch_id: "x".repeat(128), ..minimal_batch() }.validate().unwrap();
    rejects(&Batch { collector_build: "a\nb".into(), ..minimal_batch() }, "collector_build");
    rejects(
        &Batch { device: DeviceMetadata { firmware: Some("é".into()), ..Default::default() }, ..minimal_batch() },
        "device.firmware",
    );
    rejects(&Batch { events: vec![event(1), event(1)], ..minimal_batch() }, "events.source_row_id");
    rejects(&Batch { events: vec![event(0)], ..minimal_batch() }, "events.source_row_id");
    rejects(&Batch { events: (1..=MAX_EVENTS as u64 + 1).map(event).collect(), ..minimal_batch() }, "events");
    Batch { events: (1..=MAX_EVENTS as u64).map(event).collect(), ..minimal_batch() }.validate().unwrap();
    Batch { events: Vec::new(), ..minimal_batch() }.validate().unwrap();

    let with = |e: RawEvent| Batch { events: vec![e], ..minimal_batch() };
    rejects(&with(RawEvent { tag: 256, ..event(1) }), "events.tag");
    with(RawEvent { tag: 255, ring_counter: u64::from(u32::MAX), ..event(1) }).validate().unwrap();
    rejects(&with(RawEvent { ring_counter: u64::from(u32::MAX) + 1, ..event(1) }), "events.ring_counter");
    rejects(&with(RawEvent { body_hex: "AB".into(), ..event(1) }), "events.body_hex");
    rejects(&with(RawEvent { body_hex: "abc".into(), ..event(1) }), "events.body_hex");
    with(RawEvent { body_hex: "00".repeat(MAX_EVENT_BODY_BYTES), ..event(1) }).validate().unwrap();
    rejects(&with(RawEvent { body_hex: "00".repeat(MAX_EVENT_BODY_BYTES + 1), ..event(1) }), "events.body_hex");
    rejects(&with(RawEvent { captured_unix_ms: -1, ..event(1) }), "events.captured_unix_ms");

    let anchor = ClockAnchor { kind: "boot_boundary".into(), ring_counter: 0, captured_unix_ms: 0 };
    with(RawEvent { clock_anchor: Some(anchor.clone()), ..event(1) }).validate().unwrap();
    rejects(
        &with(RawEvent { clock_anchor: Some(ClockAnchor { kind: "utc".into(), ..anchor.clone() }), ..event(1) }),
        "clock_anchor.kind",
    );
    rejects(
        &with(RawEvent { clock_anchor: Some(ClockAnchor { captured_unix_ms: -1, ..anchor }), ..event(1) }),
        "clock_anchor.captured_unix_ms",
    );

    let transport =
        TransportProvenance { encoding: "legacy_event_packet".into(), raw_hex: "4a00".into(), counter_value: 1, counter_unit: "decisecond".into() };
    with(RawEvent { transport: Some(transport.clone()), ..event(1) }).validate().unwrap();
    rejects(
        &with(RawEvent { transport: Some(TransportProvenance { encoding: "Legacy".into(), ..transport.clone() }), ..event(1) }),
        "transport.encoding",
    );
    rejects(
        &with(RawEvent { transport: Some(TransportProvenance { counter_unit: String::new(), ..transport.clone() }), ..event(1) }),
        "transport.counter_unit",
    );
    rejects(
        &with(RawEvent { transport: Some(TransportProvenance { raw_hex: String::new(), ..transport }), ..event(1) }),
        "transport.raw_hex",
    );
}

#[test]
fn heartbeat_validation_rules() {
    let with = |h: Heartbeat| Batch { events: Vec::new(), heartbeat: h, ..minimal_batch() };
    with(Heartbeat { error_codes: vec!["ring_failed".into(); 16], battery_percent: Some(100), ..Default::default() })
        .validate()
        .unwrap();
    rejects(&with(Heartbeat { error_codes: vec!["ring_failed".into(); 17], ..Default::default() }), "heartbeat.error_codes");
    rejects(&with(Heartbeat { error_codes: vec!["a".repeat(65)], ..Default::default() }), "heartbeat.error_codes");
    rejects(&with(Heartbeat { error_codes: vec!["Free text".into()], ..Default::default() }), "heartbeat.error_codes");
    rejects(&with(Heartbeat { error_codes: vec![String::new()], ..Default::default() }), "heartbeat.error_codes");
    rejects(&with(Heartbeat { battery_percent: Some(101), ..Default::default() }), "heartbeat.battery_percent");
    rejects(&with(Heartbeat { ble_attempt_unix_ms: Some(-1), ..Default::default() }), "heartbeat.ble_attempt_unix_ms");
}

#[test]
fn transport_provenance_preserves_raw_counter_units_and_bounds() {
    let legacy = TransportProvenance {
        encoding: "legacy_event_packet".into(),
        raw_hex: "4a0401000000".into(),
        counter_value: 1,
        counter_unit: "decisecond".into(),
    };
    let with = |t: TransportProvenance| Batch {
        events: vec![RawEvent { transport: Some(t), ..event(1) }],
        ..minimal_batch()
    };
    rejects(&with(TransportProvenance { counter_value: 2, ..legacy.clone() }), "transport.counter_value");
    rejects(
        &with(TransportProvenance { counter_unit: "millisecond_delta".into(), ..legacy.clone() }),
        "transport.counter_unit",
    );
    rejects(&with(TransportProvenance { raw_hex: "AB".into(), ..legacy.clone() }), "transport.raw_hex");
    rejects(&with(TransportProvenance { raw_hex: "a".into(), ..legacy.clone() }), "transport.raw_hex");
    let limit = oura_protocol_v1::ingest::MAX_TRANSPORT_RAW_BYTES;
    with(TransportProvenance { raw_hex: "00".repeat(limit), ..legacy.clone() }).validate().unwrap();
    rejects(&with(TransportProvenance { raw_hex: "00".repeat(limit + 1), ..legacy.clone() }), "transport.raw_hex");

    let extended = TransportProvenance {
        encoding: "ext_event_record".into(),
        counter_value: 100,
        counter_unit: "millisecond_absolute".into(),
        ..legacy
    };
    with(extended.clone()).validate().unwrap();
    rejects(&with(TransportProvenance { counter_value: 200, ..extended.clone() }), "transport.counter_value");
    // Delta counters require previous bundle context. Preserve all u64 bits;
    // never reinterpret a delta as an absolute timestamp in this DTO layer.
    with(TransportProvenance { counter_value: u64::MAX, counter_unit: "millisecond_delta".into(), ..extended })
        .validate()
        .unwrap();
}

#[test]
fn hash_binds_exact_bytes_and_replay_storage_is_not_implied() {
    let mut with_newline = tv::BATCH_BODY.as_bytes().to_vec();
    with_newline.push(b'\n');
    assert_ne!(body_hash(&with_newline), tv::BATCH_BODY_SHA256);
    let fields = batch_fields();
    // Verification is deliberately stateless. Backend must persist the nonce
    // atomically; a second successful call here does not authorize a replay.
    fields.verify(&tv::KEY, tv::BATCH_SIGNATURE, tv::BATCH_TIMESTAMP).unwrap();
    fields.verify(&tv::KEY, tv::BATCH_SIGNATURE, tv::BATCH_TIMESTAMP).unwrap();
    assert_eq!(fields.verify(&tv::KEY, tv::BATCH_SIGNATURE, i64::MIN), Err(ContractError::ClockSkew));
    assert_eq!(fields.verify(&tv::KEY, tv::BATCH_SIGNATURE, i64::MAX), Err(ContractError::ClockSkew));
}

#[test]
fn to_bytes_enforces_wire_limit() {
    let big = |id| RawEvent { body_hex: "00".repeat(MAX_EVENT_BODY_BYTES), ..event(id) };
    let batch = Batch { events: (1..=40).map(big).collect(), ..minimal_batch() };
    batch.validate().unwrap();
    match batch.to_bytes() {
        Err(ContractError::BodyTooLarge(n)) => assert!(n > MAX_BODY_BYTES),
        other => panic!("expected BodyTooLarge, got {other:?}"),
    }
    let fits = Batch { events: (1..=31).map(big).collect(), ..minimal_batch() };
    assert!(fits.to_bytes().unwrap().len() <= MAX_BODY_BYTES);
}
