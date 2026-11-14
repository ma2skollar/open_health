use std::{fs::{self, File, OpenOptions}, io::Read, path::{Component, Path, PathBuf}};

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::CollectorError;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub device_id: String,
    pub ring_name: String,
    pub ring_key_file: PathBuf,
    pub state_dir: PathBuf,
    pub ingest_url: String,
    pub key_id: String,
    pub signing_key_file: PathBuf,
    pub ca_certificate_file: Option<PathBuf>,
    #[serde(default = "storage_default")]
    pub max_storage_bytes: u64,
    #[serde(default = "reserve_default")]
    pub min_free_bytes: u64,
    #[serde(default = "events_default")]
    pub history_max_events: usize,
    #[serde(default = "bytes_default")]
    pub history_max_bytes: usize,
    #[serde(default = "pages_default")]
    pub history_max_pages: usize,
    #[serde(default = "collection_default")]
    pub collection_interval_secs: u64,
    #[serde(default = "heartbeat_default")]
    pub heartbeat_interval_secs: u64,
    #[serde(default = "operation_default")]
    pub operation_timeout_secs: u64,
    #[serde(default = "retry_default")]
    pub retry_min_secs: u64,
    #[serde(default = "retry_max_default")]
    pub retry_max_secs: u64,
}
fn storage_default() -> u64 { 1024 * 1024 * 1024 }
fn reserve_default() -> u64 { 256 * 1024 * 1024 }
fn events_default() -> usize { 256 }
fn bytes_default() -> usize { 128 * 1024 }
fn pages_default() -> usize { 16 }
fn collection_default() -> u64 { 300 }
fn heartbeat_default() -> u64 { 900 }
fn operation_default() -> u64 { 30 }
fn retry_default() -> u64 { 5 }
fn retry_max_default() -> u64 { 900 }

impl Config {
    /// Loading and validating configuration never opens credentials or state.
    pub fn load(path: &Path) -> Result<Self, CollectorError> {
        let mut bytes = Vec::new();
        File::open(path)?.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 64 * 1024 { return Err(CollectorError::Config); }
        let text = std::str::from_utf8(&bytes).map_err(|_| CollectorError::Config)?;
        let config: Self = toml::from_str(text).map_err(|_| CollectorError::Config)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), CollectorError> {
        let id = |s: &str| !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
        if !id(&self.device_id) || !id(&self.key_id) || self.ring_name.is_empty() || self.ring_name.len() > 128 {
            return Err(CollectorError::Config);
        }
        for path in [&self.state_dir, &self.ring_key_file, &self.signing_key_file] { absolute_path(path)?; }
        if let Some(path) = &self.ca_certificate_file { absolute_path(path)?; }
        let url = reqwest::Url::parse(&self.ingest_url).map_err(|_| CollectorError::Config)?;
        if url.scheme() != "https" || url.host_str().is_none() || !url.username().is_empty() || url.password().is_some()
            || url.query().is_some() || url.fragment().is_some() || url.path() != oura_protocol_v1::ingest::PATH
            || self.ingest_url != url.as_str() {
            return Err(CollectorError::Config);
        }
        if self.history_max_events == 0 || self.history_max_events > 10_000 || self.history_max_bytes == 0
            || self.history_max_bytes > 1024 * 1024 || self.history_max_pages == 0 || self.history_max_pages > 256
            || self.wire_budget() > oura_protocol_v1::ingest::MAX_BODY_BYTES as u64
            || self.max_storage_bytes < 16 * 1024 * 1024 || self.max_storage_bytes > 1024 * 1024 * 1024 * 1024
            || self.collection_interval_secs == 0 || self.heartbeat_interval_secs == 0 || self.operation_timeout_secs == 0
            || self.operation_timeout_secs > 300 || self.retry_min_secs == 0 || self.retry_max_secs < self.retry_min_secs
            || self.retry_max_secs > 86400 || self.collection_interval_secs > 86400 || self.heartbeat_interval_secs > 86400 {
            return Err(CollectorError::Config);
        }
        Ok(())
    }

    pub fn database_path(&self) -> PathBuf { self.state_dir.join("outbox.sqlite3") }
    /// Conservative upper bound on one serialized drain, including envelope and anchors.
    pub fn wire_budget(&self) -> u64 {
        (self.history_max_bytes as u64).saturating_mul(6)
            .saturating_add((self.history_max_events as u64).saturating_mul(1024)).saturating_add(16 * 1024)
    }
}

fn absolute_path(path: &Path) -> Result<(), CollectorError> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        return Err(CollectorError::Config);
    }
    Ok(())
}

/// Reject symlink ancestors as well as symlink leaf names.
pub(crate) fn no_symlinks(path: &Path) -> Result<(), CollectorError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => return Err(CollectorError::Permissions),
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn private_file(path: &Path) -> Result<File, CollectorError> {
    use std::os::unix::{fs::{MetadataExt, OpenOptionsExt}, io::AsRawFd};
    no_symlinks(path)?;
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o7777 != 0o600 {
        return Err(CollectorError::Permissions);
    }
    // Keep the descriptor open across validation and read; no path re-open race.
    let _ = file.as_raw_fd();
    Ok(file)
}

#[cfg(not(unix))]
pub(crate) fn private_file(_: &Path) -> Result<File, CollectorError> { Err(CollectorError::Permissions) }

pub fn load_signing_key(path: &Path) -> Result<Zeroizing<[u8; 32]>, CollectorError> {
    let file = private_file(path)?;
    if file.metadata()?.len() != 32 { return Err(CollectorError::Key); }
    let mut bytes = Zeroizing::new([0u8; 32]);
    file.take(33).read_exact(bytes.as_mut())?;
    Ok(bytes)
}
