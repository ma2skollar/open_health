//! Synthetic durable-receipt consumer, not a production backend implementation.
use std::{collections::BTreeMap, sync::{Arc, Mutex}};
use async_trait::async_trait;
use oura_collector::{CollectorError, PendingBatch, delivery::UploadSink};
use oura_protocol_v1::ingest::{Ack, body_hash, VERSION};

#[derive(Default)]
pub struct Consumer {
    pub offline: bool,
    pub lose_next_ack: bool,
    pub conflicting_ack: bool,
    pub receipts: BTreeMap<String, Ack>,
    pub rows: BTreeMap<(String, String, u64), String>,
}
#[derive(Clone, Default)]
pub struct SyntheticSink(pub Arc<Mutex<Consumer>>);
#[async_trait]
impl UploadSink for SyntheticSink {
    async fn upload(&mut self, pending: &PendingBatch) -> Result<Ack, CollectorError> {
        let mut consumer = self.0.lock().unwrap();
        if consumer.offline { return Err(CollectorError::Delivery); }
        let hash = body_hash(&pending.body);
        let id = pending.batch.batch_id.clone();
        if let Some(old) = consumer.receipts.get(&id) {
            if old.body_sha256 != hash { return Err(CollectorError::Ack); }
            return Ok(old.clone());
        }
        let ack = Ack { version: VERSION, device_id: pending.batch.device_id.clone(), batch_id: id.clone(), body_sha256: hash, durable: true, accepted_unix_ms: 1_700_000_000_000 };
        if consumer.conflicting_ack {
            let mut wrong = ack;
            wrong.body_sha256 = "00".repeat(32);
            return Ok(wrong);
        }
        for event in &pending.batch.events {
            consumer.rows.insert((pending.batch.device_id.clone(), pending.batch.collector_epoch.clone(), event.source_row_id), event.body_hex.clone());
        }
        consumer.receipts.insert(id, ack.clone());
        if consumer.lose_next_ack { consumer.lose_next_ack = false; return Err(CollectorError::Delivery); }
        Ok(ack)
    }
}
