use super::*;
use std::{collections::VecDeque, sync::Mutex};
use async_trait::async_trait;
use tokio::sync::broadcast;

struct Wire {
    tx: broadcast::Sender<Vec<u8>>,
    steps: Mutex<VecDeque<(Vec<u8>, Vec<Vec<u8>>)>>,
}
impl Wire {
    fn new(steps: Vec<(Vec<u8>, Vec<Vec<u8>>)>) -> Self {
        Self { tx: broadcast::channel(32).0, steps: Mutex::new(steps.into()) }
    }
    fn finished(&self) { assert!(self.steps.lock().unwrap().is_empty()); }
}
#[async_trait]
impl Transport for Wire {
    async fn write(&self, request: &[u8]) -> oura_link::Result<()> {
        let (expected, frames) = self.steps.lock().unwrap().pop_front().expect("unexpected operation in history-only transport");
        assert_eq!(request, expected);
        for frame in frames { self.tx.send(frame).unwrap(); }
        Ok(())
    }
    fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> { self.tx.subscribe() }
}
fn limits(events: usize, pages: usize) -> Limits { Limits { events, pages, bytes: 131072 } }
fn flush() -> (Vec<u8>, Vec<Vec<u8>>) { (protocol::req_data_flush(), vec![vec![0x29, 1, 0]]) }
fn summary(count: u16, remaining: u32) -> Vec<u8> {
    let mut raw = vec![0x2f, 10, 0x42];
    raw.extend(count.to_le_bytes()); raw.push(0); raw.extend(remaining.to_le_bytes()); raw.extend([0, 0]); raw
}
fn legacy_event(counter: u32, body: &[u8]) -> Vec<u8> {
    let mut raw = vec![0xee, (body.len() + 4) as u8]; raw.extend(counter.to_le_bytes()); raw.extend(body); raw
}
fn legacy_page(events: &[(u32, &[u8])], remaining: u32) -> Vec<u8> {
    let mut raw = Vec::new();
    for (counter, body) in events { raw.extend(legacy_event(*counter, body)); }
    raw.extend(summary(events.len() as u16, remaining)); raw
}
fn request(cursor: u64, count: u16) -> Vec<u8> { protocol::req_ext_get_event(cursor * 100, count, 0) }

#[tokio::test(start_paused = true)]
async fn split_decisecond_group_is_refetched_without_loss_or_duplicate_prefix() {
    let first = Wire::new(vec![flush(), (request(0, 3), vec![legacy_page(&[(1, b"a"), (2, b"b"), (2, b"c")], 1)])]);
    let one = drain(&first, 0, Duration::from_secs(1), limits(3, 1)).await.unwrap();
    assert_eq!(one.next_cursor, 2);
    assert!(!one.complete);
    assert_eq!(one.events.iter().map(|e| e.body.clone()).collect::<Vec<_>>(), vec![b"a".to_vec()]);
    first.finished();
    let next = Wire::new(vec![flush(), (request(2, 4), vec![legacy_page(&[(2, b"b"), (2, b"c"), (3, b"d")], 0)]), flush(), (request(4, 1), vec![summary(0, 0)])]);
    let two = drain(&next, one.next_cursor, Duration::from_secs(1), limits(4, 3)).await.unwrap();
    assert!(two.complete);
    assert_eq!(two.next_cursor, 4);
    let bodies: Vec<_> = one.events.iter().chain(&two.events).map(|e| e.body.as_slice()).collect();
    assert_eq!(bodies, vec![b"a".as_slice(), b"b".as_slice(), b"c".as_slice(), b"d".as_slice()]);
    next.finished();
}
#[tokio::test(start_paused = true)]
async fn original_extended_varints_and_unknown_bodies_survive_without_synthetic_ticks() {
    // First absolute 1200ms, then delta30ms (same decisecond), then delta80ms.
    let records = vec![0, 0xee, 3, 0xb0, 0x09, 0xff, 0, 0xee, 2, 30, 0xfe, 0, 0xee, 2, 80, 0xfd];
    let mut envelope = (records.len() as u16).to_le_bytes().to_vec(); envelope.extend(&records);
    let mut frame = vec![0x2f, (envelope.len() + 1) as u8, 0x43]; frame.extend(envelope); frame.extend(summary(3, 1));
    let wire = Wire::new(vec![flush(), (request(0, 3), vec![frame])]);
    let result = drain(&wire, 0, Duration::from_secs(1), limits(3, 1)).await.unwrap();
    assert_eq!(result.next_cursor, 13);
    assert_eq!(result.events.iter().map(|e| e.ring_timestamp).collect::<Vec<_>>(), vec![12, 12]);
    assert_eq!(result.events[0].body, vec![0xff]);
    assert_eq!(result.events[1].body, vec![0xfe]);
    let first = result.events[0].transport.as_ref().unwrap();
    assert_eq!(first.raw, records[..6]); assert_eq!(first.counter_value, 1200); assert_eq!(first.counter_unit, "millisecond_absolute");
    let second = result.events[1].transport.as_ref().unwrap();
    assert_eq!(second.raw, records[6..11]); assert_eq!(second.counter_value, 30); assert_eq!(second.counter_unit, "millisecond_delta");
    wire.finished();
}
#[tokio::test(start_paused = true)]
async fn oversized_or_interrupted_page_never_advances_but_prior_checkpoint_survives() {
    let wire = Wire::new(vec![flush(), (request(0, 3), vec![legacy_page(&[(1, b"a")], 0)]), flush(), (request(2, 2), vec![legacy_page(&[(2, b"too big")], 0)])]);
    let result = drain(&wire, 0, Duration::from_secs(1), Limits { events: 3, bytes: 2, pages: 3 }).await.unwrap();
    assert_eq!(result.next_cursor, 2); assert_eq!(result.events[0].body, b"a"); assert!(!result.complete);
    wire.finished();
    let wire = Wire::new(vec![flush(), (request(0, 3), vec![legacy_page(&[(1, b"too big")], 0)])]);
    assert_eq!(drain(&wire, 0, Duration::from_secs(1), Limits { events: 3, bytes: 2, pages: 3 }).await, Err(RingError::HistoryLimit));
    let wire = Wire::new(vec![flush(), (request(0, 3), vec![legacy_event(1, b"uncheckpointed")])]);
    assert_eq!(drain(&wire, 0, Duration::from_millis(5), limits(3, 3)).await, Err(RingError::Timeout { operation: "drain_history" }));
}
#[tokio::test(start_paused = true)]
async fn rejected_cursor_and_missing_reboot_marker_never_trigger_reset() {
    let mut rejected = summary(0, 0); rejected[11] = 0xff;
    let wire = Wire::new(vec![flush(), (request(12, 4), vec![rejected])]);
    assert_eq!(drain(&wire, 12, Duration::from_secs(1), limits(4, 3)).await, Err(RingError::CursorRegression)); wire.finished();
    let wire = Wire::new(vec![flush(), (request(12, 4), vec![summary(0, 0)]), flush(), (request(11, 1), vec![summary(0, 0)])]);
    assert_eq!(drain(&wire, 12, Duration::from_secs(1), limits(4, 3)).await, Err(RingError::CursorRegression)); wire.finished();
}
#[tokio::test(start_paused = true)]
async fn incomplete_summary_lag_and_ambiguous_full_group_fail_closed() {
    let wire = Wire::new(vec![flush(), (request(0, 3), vec![legacy_page(&[(1, b"a"), (1, b"b"), (1, b"c")], 1)])]);
    assert_eq!(drain(&wire, 0, Duration::from_secs(1), limits(3, 1)).await, Err(RingError::HistoryLimit));
    let mut broken = legacy_page(&[(1, b"a")], 0); let offset = broken.len() - 9; broken[offset] = 2;
    let wire = Wire::new(vec![flush(), (request(0, 3), vec![broken])]);
    assert_eq!(drain(&wire, 0, Duration::from_secs(1), limits(3, 1)).await, Err(RingError::PartialHistory));
    let wire = Wire::new(vec![flush(), (request(0, 3), vec![vec![0x29, 1, 0]; 40])]);
    assert_eq!(drain(&wire, 0, Duration::from_secs(1), limits(3, 1)).await, Err(RingError::PartialHistory));
}
#[tokio::test(start_paused = true)]
async fn fallback_is_explicit_and_only_normal_flush_and_history_requests_are_sent() {
    let mut page = legacy_event(1, b"raw"); page.extend([0x11, 6, 1, 0, 0, 0, 0, 0]);
    let wire = Wire::new(vec![flush(), (request(0, 4), vec![vec![0x2f, 1, 0]]),
        (protocol::req_get_event(0, 4, -1), vec![page]), flush(), (protocol::req_get_event(2, 3, -1), vec![vec![0x11, 6, 0, 0, 0, 0, 0, 0]])]);
    let result = drain(&wire, 0, Duration::from_secs(1), limits(4, 3)).await.unwrap();
    assert!(result.complete); assert_eq!(result.events[0].transport.as_ref().unwrap().raw, legacy_event(1, b"raw"));
    wire.finished();
}

#[tokio::test(start_paused = true)]
async fn reboot_probe_respects_page_budget() {
    let wire = Wire::new(vec![flush(), (request(12, 4), vec![summary(0, 0)])]);
    assert_eq!(drain(&wire, 12, Duration::from_secs(1), limits(4, 1)).await, Err(RingError::HistoryLimit));
    wire.finished();
}
