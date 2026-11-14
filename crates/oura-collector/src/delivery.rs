use std::{path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use oura_protocol_v1::ingest::{self, Ack, SigningFields};
use rand::RngCore;

use crate::{clock::Clock, config::load_signing_key, storage::PendingBatch, CollectorError, Config};

const ACK_LIMIT: usize = 16 * 1024;

#[async_trait]
pub trait UploadSink: Send {
    async fn upload(&mut self, batch: &PendingBatch) -> Result<Ack, CollectorError>;
}

/// Fresh signing material is made only for transmission. It is never stored in
/// the outbox. This type deliberately has no Debug implementation.
pub struct SignedAttempt {
    pub fields: SigningFields,
    pub signature: String,
}

pub fn sign_attempt(config: &Config, pending: &PendingBatch, clock: &dyn Clock, nonce: [u8; 16]) -> Result<SignedAttempt, CollectorError> {
    let now = clock.now_unix_ms();
    if !clock.synchronized() || now < 0 { return Err(CollectorError::Clock); }
    let key = load_signing_key(&config.signing_key_file)?;
    let fields = SigningFields { key_id: config.key_id.clone(), device_id: pending.batch.device_id.clone(),
        timestamp: now / 1000, nonce: hex::encode(nonce), body_sha256: ingest::body_hash(&pending.body) };
    let signature = fields.sign(key.as_ref()).map_err(|_| CollectorError::Contract)?;
    Ok(SignedAttempt { fields, signature })
}

pub struct HttpUploadSink {
    client: reqwest::Client,
    endpoint: reqwest::Url,
    config: Config,
    clock: Arc<dyn Clock>,
}

impl HttpUploadSink {
    pub fn new(config: &Config, clock: Arc<dyn Clock>) -> Result<Self, CollectorError> {
        config.validate()?;
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy().no_gzip().no_brotli().no_deflate().no_zstd()
            .min_tls_version(reqwest::tls::Version::TLS_1_2);
        if let Some(path) = &config.ca_certificate_file {
            let pem = read_ca(path)?;
            let cert = reqwest::Certificate::from_pem(&pem).map_err(|_| CollectorError::Config)?;
            builder = builder.add_root_certificate(cert);
        }
        let client = builder.build().map_err(|_| CollectorError::Delivery)?;
        let endpoint = reqwest::Url::parse(&config.ingest_url).map_err(|_| CollectorError::Config)?;
        Ok(Self { client, endpoint, config: config.clone(), clock })
    }
}

fn read_ca(path: &PathBuf) -> Result<Vec<u8>, CollectorError> {
    use std::io::Read;
    crate::config::no_symlinks(path)?;
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > 1024 * 1024 { return Err(CollectorError::Config); }
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 { return Err(CollectorError::Config); }
    Ok(bytes)
}

#[async_trait]
impl UploadSink for HttpUploadSink {
    async fn upload(&mut self, pending: &PendingBatch) -> Result<Ack, CollectorError> {
        let mut nonce = [0u8; 16];
        rand::rngs::OsRng.try_fill_bytes(&mut nonce).map_err(|_| CollectorError::Delivery)?;
        // Re-read the active key file for every attempt, including retries.
        let signed = sign_attempt(&self.config, pending, self.clock.as_ref(), nonce)?;
        let fields = signed.fields;
        let mut response = self.client.post(self.endpoint.clone())
            .header(reqwest::header::CONTENT_TYPE, ingest::CONTENT_TYPE)
            .header(reqwest::header::ACCEPT, ingest::CONTENT_TYPE)
            .header(ingest::HEADER_KEY_ID, fields.key_id)
            .header(ingest::HEADER_DEVICE_ID, fields.device_id)
            .header(ingest::HEADER_TIMESTAMP, fields.timestamp.to_string())
            .header(ingest::HEADER_NONCE, fields.nonce)
            .header(ingest::HEADER_BODY_SHA256, fields.body_sha256)
            .header(ingest::HEADER_SIGNATURE, signed.signature)
            .body(pending.body.clone()).send().await.map_err(|_| CollectorError::Delivery)?;
        if !response.status().is_success() || response.headers().contains_key(reqwest::header::CONTENT_ENCODING)
            || response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) != Some(ingest::CONTENT_TYPE)
            || response.content_length().is_some_and(|n| n > ACK_LIMIT as u64) {
            return Err(CollectorError::Delivery);
        }
        let mut bytes = Vec::with_capacity(1024);
        while let Some(chunk) = response.chunk().await.map_err(|_| CollectorError::Delivery)? {
            if chunk.len() > ACK_LIMIT.saturating_sub(bytes.len()) { return Err(CollectorError::Ack); }
            bytes.extend_from_slice(&chunk);
        }
        let ack: Ack = serde_json::from_slice(&bytes).map_err(|_| CollectorError::Ack)?;
        ack.validate(&pending.batch, &pending.body).map_err(|_| CollectorError::Ack)?;
        Ok(ack)
    }
}
