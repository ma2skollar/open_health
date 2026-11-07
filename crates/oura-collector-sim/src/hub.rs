//! Fault injection around the real D5 ingestion implementation, not a second hub.
use async_trait::async_trait;
use oura_collector::{delivery::UploadSink, CollectorError};
use oura_protocol_v1::{UploadAck, UploadMetadata};
use std::path::Path;

const DEMO_TOKEN: &str = "simulator-only-demo-collector-token";

pub struct SimHub {
    pub hub: oura_hub::Hub,
    pub online: bool,
    /// Lose one response after durable acceptance to exercise harmless retries.
    pub lose_next_ack: bool,
    pub attempts: usize,
    pub acknowledgements: Vec<UploadAck>,
    pub rejections: Vec<String>,
}

impl SimHub {
    pub fn new(root: &Path) -> Result<Self, CollectorError> {
        let mut config = oura_hub::Config::new(root.to_path_buf());
        config.rate_limit = 10_000;
        let hub = oura_hub::Hub::new(config, DEMO_TOKEN.as_bytes().to_vec())
            .map_err(|error| CollectorError::Delivery(error.to_string()))?;
        Ok(Self {
            hub,
            online: true,
            lose_next_ack: false,
            attempts: 0,
            acknowledgements: Vec::new(),
            rejections: Vec::new(),
        })
    }

    pub fn ingest(&self, metadata: UploadMetadata, path: &Path) -> Result<UploadAck, CollectorError> {
        if metadata.identity != oura_protocol_v1::Identity::Demo {
            return Err(CollectorError::Protocol("simulator only accepts identity: demo".into()));
        }
        let bytes = std::fs::read(path)?;
        self.hub.ingest(Some(DEMO_TOKEN), metadata, &bytes)
            .map_err(|error| CollectorError::Delivery(error.to_string()))
    }
}

#[async_trait]
impl UploadSink for SimHub {
    async fn upload(&mut self, metadata: &UploadMetadata, path: &Path) -> Result<UploadAck, CollectorError> {
        self.attempts += 1;
        if !self.online {
            return Err(CollectorError::Delivery("simulated server offline".into()));
        }
        let ack = match self.ingest(metadata.clone(), path) {
            Ok(ack) => ack,
            Err(error) => {
                self.rejections.push(error.to_string());
                return Err(error);
            }
        };
        self.acknowledgements.push(ack.clone());
        if std::mem::take(&mut self.lose_next_ack) {
            return Err(CollectorError::Delivery("simulated lost acknowledgement".into()));
        }
        Ok(ack)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_identity_is_rejected_before_reading_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let hub = SimHub::new(&root.path().join("hub")).unwrap();
        let metadata = UploadMetadata {
            protocol_version: 1,
            schema_version: 2,
            collector_id: "demo-collector".into(),
            collector_epoch: "demo-epoch".into(),
            generation: 1,
            created_unix: 0,
            size_bytes: 0,
            sha256: String::new(),
            build_revision: "demo".into(),
            identity: oura_protocol_v1::Identity::Production,
        };
        let error = hub.ingest(metadata, &root.path().join("does-not-exist.db")).unwrap_err();
        assert!(matches!(error, CollectorError::Protocol(_)));
    }
}
