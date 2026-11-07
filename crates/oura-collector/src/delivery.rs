//! Authenticated raw-SQLite upload. No multipart or ring secrets cross this boundary.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream;
use oura_protocol_v1::{UploadAck, UploadMetadata, PROTOCOL_VERSION};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE};
use tokio::io::AsyncReadExt;

use crate::config::check_secret_file;
use crate::{CollectorError, Config};

#[async_trait]
pub trait UploadSink: Send {
    async fn upload(&mut self, metadata: &UploadMetadata, path: &Path) -> Result<UploadAck, CollectorError>;
}

// Intentionally not Debug: request defaults contain the bearer credential.
pub struct HttpUploadSink {
    client: reqwest::Client,
    endpoint: reqwest::Url,
}

impl HttpUploadSink {
    pub fn new(config: &Config) -> Result<Self, CollectorError> { Self::build(config, None) }

    /// Add an operator CA while retaining certificate and hostname verification.
    pub fn with_ca_certificate(config: &Config, pem: &[u8]) -> Result<Self, CollectorError> {
        let certificate = reqwest::Certificate::from_pem(pem)
            .map_err(|_| CollectorError::Config("invalid hub CA certificate".into()))?;
        Self::build(config, Some(certificate))
    }

    fn build(config: &Config, certificate: Option<reqwest::Certificate>) -> Result<Self, CollectorError> {
        config.validate()?;
        check_secret_file(&config.token_file)?;
        let token = std::fs::read_to_string(&config.token_file)?;
        let token = token.trim();
        if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(CollectorError::Config("collector token must contain printable non-whitespace ASCII".into()));
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| CollectorError::Config("invalid collector credential".into()))?;
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);
        let mut builder = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            // Collector credentials must not be handed to an ambient proxy.
            .no_proxy()
            .min_tls_version(reqwest::tls::Version::TLS_1_2);
        if let Some(certificate) = certificate { builder = builder.add_root_certificate(certificate); }
        let client = builder.build().map_err(|_| CollectorError::Delivery("HTTP client initialization failed".into()))?;
        let mut endpoint = reqwest::Url::parse(&config.hub_url)
            .map_err(|_| CollectorError::Config("invalid hub URL".into()))?;
        endpoint.set_path("/api/v1/collector/snapshot");
        Ok(Self { client, endpoint })
    }
}

#[async_trait]
impl UploadSink for HttpUploadSink {
    async fn upload(&mut self, metadata: &UploadMetadata, path: &Path) -> Result<UploadAck, CollectorError> {
        if metadata.protocol_version != PROTOCOL_VERSION {
            return Err(CollectorError::Protocol("unsupported outgoing upload protocol".into()));
        }
        let encoded = serde_json::to_string(metadata)?;
        if encoded.len() > 16 * 1024 {
            return Err(CollectorError::Protocol("upload metadata exceeds header limit".into()));
        }
        let metadata_header = HeaderValue::from_str(&encoded)
            .map_err(|_| CollectorError::Protocol("upload metadata is not HTTP-header-safe".into()))?;
        let file = tokio::fs::File::open(path).await?;
        if file.metadata().await?.len() != metadata.size_bytes {
            return Err(CollectorError::Protocol("snapshot size changed before upload".into()));
        }
        let body = stream::try_unfold((file, metadata.size_bytes), |(mut file, remaining)| async move {
            if remaining == 0 { return Ok::<_, std::io::Error>(None); }
            let mut buffer = vec![0u8; remaining.min(64 * 1024) as usize];
            let count = file.read(&mut buffer).await?;
            if count == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "snapshot truncated during upload"));
            }
            buffer.truncate(count);
            Ok(Some((buffer, (file, remaining - count as u64))))
        });
        let mut response = self.client.post(self.endpoint.clone())
            .header(CONTENT_TYPE, "application/vnd.sqlite3")
            .header(CONTENT_LENGTH, metadata.size_bytes)
            .header("X-Oura-Metadata", metadata_header)
            .body(reqwest::Body::wrap_stream(body))
            .send().await.map_err(|_| CollectorError::Delivery("snapshot request failed".into()))?;
        if !response.status().is_success() {
            // Never include response bodies, request headers or URL credentials in errors.
            let message = format!("snapshot rejected with HTTP {}", response.status().as_u16());
            return Err(if response.status().is_client_error() && !matches!(response.status().as_u16(), 408 | 429) {
                CollectorError::Protocol(message)
            } else { CollectorError::Delivery(message) });
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| CollectorError::Delivery("snapshot acknowledgement read failed".into()))? {
            if bytes.len().saturating_add(chunk.len()) > 16 * 1024 {
                return Err(CollectorError::Protocol("snapshot acknowledgement exceeds limit".into()));
            }
            bytes.extend_from_slice(&chunk);
        }
        let ack: UploadAck = serde_json::from_slice(&bytes)
            .map_err(|_| CollectorError::Protocol("invalid snapshot acknowledgement".into()))?;
        // Pending-review acceptance is durable but deliberately does not activate
        // the new epoch; active-generation numbers belong to a different epoch.
        if ack.protocol_version != PROTOCOL_VERSION || !ack.accepted
            || ack.generation != metadata.generation
            || (ack.active_generation < metadata.generation && ack.message != "pending_review") {
            return Err(CollectorError::Protocol("snapshot acknowledgement does not confirm acceptance".into()));
        }
        Ok(ack)
    }
}

