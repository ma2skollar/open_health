//! Bounded transport framing only. No physiological event decoder is called.
use std::time::Duration;
use oura_link::transport::Transport;
use oura_protocol::protocol;
use tokio::sync::broadcast::error::RecvError;
use crate::{HistoryBatch, HistoryEvent, HistoryTransport, RingError};

#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub events: usize,
    pub bytes: usize,
    pub pages: usize,
}

// Retain only complete, validated pages across timeout or link loss. The caller
// must commit this checkpoint and its raw records in one durable transaction.
pub(crate) async fn drain<T: Transport>(transport: &T, cursor: u64, timeout: Duration, limits: Limits) -> Result<HistoryBatch, RingError> {
    if cursor > u32::MAX as u64 { return Err(RingError::CursorRegression); }
    if limits.events == 0 || limits.events > 10_000 || limits.bytes == 0 || limits.bytes > 1024 * 1024 || limits.pages == 0 || limits.pages > 256 {
        return Err(RingError::HistoryLimit);
    }
    let mut out = HistoryBatch { events: Vec::new(), next_cursor: cursor, complete: false };
    let result = tokio::time::timeout(timeout, drain_inner(transport, cursor, limits, &mut out)).await
        .unwrap_or(Err(RingError::Timeout { operation: "drain_history" }));
    match result {
        Ok(()) => Ok(out),
        Err(_) if out.next_cursor > cursor => Ok(out),
        Err(error) => Err(error),
    }
}

async fn drain_inner<T: Transport>(transport: &T, initial: u64, limits: Limits, out: &mut HistoryBatch) -> Result<(), RingError> {
    let mut extended = true;
    let mut body_bytes = 0;
    for page_index in 0..limits.pages {
        let available = limits.events - out.events.len();
        if available == 0 || body_bytes == limits.bytes { return Ok(()); }
        let requested = available.min(255);
        let remaining = limits.bytes - body_bytes;
        let page = fetch(transport, out.next_cursor, requested, remaining, &mut extended).await?;
        if page.events.is_empty() {
            if page.bytes_left != 0 { return Err(RingError::PartialHistory); }
            // An empty incremental fetch is not proof that a pre-reboot cursor
            // remains valid. Read the last marker; never reset the cursor here.
            if out.events.is_empty() && initial > 0 {
                if page_index + 1 >= limits.pages { return Err(RingError::HistoryLimit); }
                let marker = fetch(transport, initial - 1, 1, limits.bytes, &mut extended).await?;
                if !marker.events.iter().any(|e| e.ring_timestamp == initial - 1) {
                    return Err(RingError::CursorRegression);
                }
            }
            out.complete = true;
            return Ok(());
        }
        if page.events.iter().any(|e| e.ring_timestamp < out.next_cursor)
            || page.events.windows(2).any(|pair| pair[1].ring_timestamp < pair[0].ring_timestamp) {
            return Err(RingError::CursorRegression);
        }
        let maximum = page.events.last().unwrap().ring_timestamp;
        // A truncated page may divide a decisecond group. Leave that ENTIRE
        // group on the ring and refetch it; never synthesize +1 timestamps.
        let truncated = page.events.len() == requested || page.bytes_left != 0;
        let keep = if truncated { page.events.partition_point(|e| e.ring_timestamp < maximum) } else { page.events.len() };
        if keep == 0 { return Err(RingError::HistoryLimit); }
        let next = if truncated { maximum } else { maximum.checked_add(1).ok_or(RingError::CursorRegression)? };
        if next > u32::MAX as u64 || next <= out.next_cursor { return Err(RingError::CursorRegression); }
        for event in page.events.into_iter().take(keep) {
            body_bytes += event.body.len();
            out.events.push(event);
        }
        out.next_cursor = next;
    }
    Ok(())
}

struct Page { events: Vec<HistoryEvent>, bytes_left: u32 }

async fn fetch<T: Transport>(transport: &T, cursor: u64, count: usize, bytes: usize, extended: &mut bool) -> Result<Page, RingError> {
    let cap = bytes.saturating_add(count.saturating_mul(64)).saturating_add(4096);
    packets(transport, &protocol::req_data_flush(), 4096, |raw| raw[0] == 0x29).await?;
    let terminal = |raw: &[u8]| raw[0] == 0x11 || (raw[0] == 0x2f && matches!(raw.get(2), Some(0x42) | Some(0x00)));
    let mut raw = if *extended {
        packets(transport, &protocol::req_ext_get_event(cursor * 100, count as u16, 0), cap, terminal).await?
    } else {
        packets(transport, &protocol::req_get_event(cursor as u32, count as u8, -1), cap, terminal).await?
    };
    if *extended && raw.iter().any(|p| p[0] == 0x2f && p.get(2) == Some(&0)) {
        if raw.iter().any(|p| p[0] >= protocol::HISTORY_EVENT_PREFIX || (p[0] == 0x2f && p.get(2) == Some(&0x43))) {
            return Err(RingError::Failed);
        }
        *extended = false;
        raw = packets(transport, &protocol::req_get_event(cursor as u32, count as u8, -1), cap, terminal).await?;
    }
    decode_page(raw, count, bytes)
}

// Subscribe BEFORE writing. Bound received bytes, reject lag and incomplete
// framing; unlike upstream transact_until, never accumulate an unbounded burst.
async fn packets<T: Transport>(transport: &T, request: &[u8], cap: usize, terminal: impl Fn(&[u8]) -> bool) -> Result<Vec<Vec<u8>>, RingError> {
    let mut receiver = transport.subscribe();
    transport.write(request).await.map_err(|_| RingError::Disconnected)?;
    let mut buffer = Vec::new();
    let mut result = Vec::new();
    let mut received = 0usize;
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(6), receiver.recv()).await
            .map_err(|_| RingError::Timeout { operation: "drain_history" })?
            .map_err(|error| match error { RecvError::Closed => RingError::Disconnected, RecvError::Lagged(_) => RingError::PartialHistory })?;
        received = received.checked_add(frame.len()).filter(|n| *n <= cap).ok_or(RingError::HistoryLimit)?;
        buffer.extend_from_slice(&frame);
        let mut position = 0;
        let mut done = false;
        while buffer.len() - position >= 2 {
            let end = position + 2 + buffer[position + 1] as usize;
            if end > buffer.len() { break; }
            let raw = &buffer[position..end];
            if done { return Err(RingError::PartialHistory); }
            done = terminal(raw);
            result.push(raw.to_vec());
            position = end;
        }
        if done {
            if position != buffer.len() { return Err(RingError::PartialHistory); }
            return Ok(result);
        }
        buffer.drain(..position);
    }
}

fn decode_page(packets: Vec<Vec<u8>>, count: usize, limit: usize) -> Result<Page, RingError> {
    let mut events = Vec::new();
    let mut envelopes = Vec::new();
    let mut summary = None;
    let mut body_bytes = 0usize;
    for raw in packets {
        match (raw[0], raw.get(2).copied()) {
            (0x2f, Some(0x43)) => envelopes.extend_from_slice(&raw[3..]),
            (0x2f, Some(0x42)) => {
                if raw.len() < 12 || summary.is_some() { return Err(RingError::PartialHistory); }
                if raw[11] == 0xff { return Err(RingError::CursorRegression); }
                if raw[11] != 0 || raw[10] != 0 { return Err(RingError::Failed); }
                summary = Some((u16::from_le_bytes([raw[3], raw[4]]) as usize, u32::from_le_bytes(raw[6..10].try_into().unwrap())));
            }
            (0x11, _) => {
                if raw.len() < 8 || summary.is_some() { return Err(RingError::PartialHistory); }
                summary = Some((raw[2] as usize, u32::from_le_bytes(raw[4..8].try_into().unwrap())));
            }
            (tag, _) if tag >= protocol::HISTORY_EVENT_PREFIX => {
                if raw.len() < 6 { return Err(RingError::PartialHistory); }
                let counter = u32::from_le_bytes(raw[2..6].try_into().unwrap()) as u64;
                let body = raw[6..].to_vec();
                let event = HistoryEvent { tag: tag as u32, ring_timestamp: counter, body, transport: Some(HistoryTransport {
                    encoding: "legacy_event_packet".into(), counter_value: counter, counter_unit: "decisecond".into(), raw,
                }) };
                push(&mut events, event, &mut body_bytes, count, limit)?;
            }
            _ => return Err(RingError::PartialHistory),
        }
    }
    if !envelopes.is_empty() {
        if !events.is_empty() { return Err(RingError::PartialHistory); }
        let mut offset = 0;
        while offset < envelopes.len() {
            let header = envelopes.get(offset..offset + 2).ok_or(RingError::PartialHistory)?;
            let length = u16::from_le_bytes(header.try_into().unwrap()) as usize;
            offset += 2;
            let bundle = envelopes.get(offset..offset + length).ok_or(RingError::PartialHistory)?;
            decode_bundle(bundle, &mut events, &mut body_bytes, count, limit)?;
            offset += length;
        }
    }
    let (reported, bytes_left) = summary.ok_or(RingError::PartialHistory)?;
    if reported != events.len() { return Err(RingError::PartialHistory); }
    Ok(Page { events, bytes_left })
}

fn decode_bundle(bundle: &[u8], events: &mut Vec<HistoryEvent>, bytes: &mut usize, count: usize, limit: usize) -> Result<(), RingError> {
    let mut offset = 0;
    let mut previous_ms: Option<u64> = None;
    while offset < bundle.len() {
        let header = bundle.get(offset..offset + 3).ok_or(RingError::PartialHistory)?;
        let tag = header[1];
        if tag < protocol::HISTORY_EVENT_PREFIX { return Err(RingError::PartialHistory); }
        let end = offset + 3 + header[2] as usize;
        let payload = bundle.get(offset + 3..end).ok_or(RingError::PartialHistory)?;
        let (encoded, used) = varint(payload)?;
        let delta = previous_ms.is_some() && encoded < 100_000_000;
        let ms = if delta { previous_ms.unwrap().checked_add(encoded).ok_or(RingError::CursorRegression)? } else { encoded };
        let counter = ms / 100;
        if counter > u32::MAX as u64 { return Err(RingError::CursorRegression); }
        previous_ms = Some(ms);
        let event = HistoryEvent { tag: tag as u32, ring_timestamp: counter, body: payload[used..].to_vec(), transport: Some(HistoryTransport {
            encoding: "ext_event_record".into(), raw: bundle[offset..end].to_vec(), counter_value: encoded,
            counter_unit: if delta { "millisecond_delta" } else { "millisecond_absolute" }.into(),
        }) };
        push(events, event, bytes, count, limit)?;
        offset = end;
    }
    Ok(())
}

fn varint(bytes: &[u8]) -> Result<(u64, usize), RingError> {
    let mut value = 0u64;
    for (index, byte) in bytes.iter().copied().take(10).enumerate() {
        if index == 9 && byte > 1 { return Err(RingError::PartialHistory); }
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 { return Ok((value, index + 1)); }
    }
    Err(RingError::PartialHistory)
}
fn push(events: &mut Vec<HistoryEvent>, event: HistoryEvent, bytes: &mut usize, count: usize, limit: usize) -> Result<(), RingError> {
    *bytes = bytes.checked_add(event.body.len()).filter(|n| *n <= limit).ok_or(RingError::HistoryLimit)?;
    if events.len() >= count { return Err(RingError::HistoryLimit); }
    events.push(event);
    Ok(())
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
