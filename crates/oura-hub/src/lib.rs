//! Immutable SQLite ingestion. Wire framing is JSON in `X-Oura-Metadata` and
//! unmodified SQLite bytes in the request body. Epochs are opaque strings: their
//! UTF-8 bytes are hex-encoded for disk paths, never interpreted as paths.

pub mod control;
pub mod http;

use fs2::FileExt;
use oura_protocol_v1::{ErrorEnvelope, Identity, UploadAck, UploadMetadata};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;
use uuid::Uuid;

pub const SUPPORTED_SCHEMA_VERSION: i64 = 2;
const SQLITE_MAGIC: &[u8] = b"SQLite format 3\0";

#[derive(Clone, Debug)]
pub struct Config {
    pub data_root: PathBuf,
    pub max_bytes: u64,
    pub request_timeout: Duration,
    pub rate_limit: u32,
    pub rate_window: Duration,
}
impl Config {
    pub fn new(data_root: PathBuf) -> Self {
        Self {
            data_root,
            max_bytes: 512 * 1024 * 1024,
            request_timeout: Duration::from_secs(120),
            rate_limit: 60,
            rate_window: Duration::from_secs(60),
        }
    }
}

#[derive(Debug)]
pub struct HubError {
    pub code: String,
    pub message: String,
}
impl HubError {
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
    pub fn status_code(&self) -> u16 {
        match self.code.as_str() {
            "unauthorized" => 401,
            "too_large" => 413,
            "request_timeout" => 408,
            "rate_limited" => 429,
            "stale_generation"
            | "generation_conflict"
            | "history_regression"
            | "pending_review" => 409,
            "storage_error" | "state_error" | "already_running" => 500,
            _ => 400,
        }
    }
    pub fn envelope(&self) -> ErrorEnvelope {
        ErrorEnvelope {
            protocol_version: 1,
            code: self.code.clone(),
            message: self.message.clone(),
        }
    }
}
impl std::fmt::Display for HubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for HubError {}
pub type Result<T, E = HubError> = std::result::Result<T, E>;
fn storage(_: impl std::fmt::Display) -> HubError {
    HubError::new("storage_error", "durable storage operation failed")
}
fn deadline_check(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        Err(HubError::new("request_timeout", "upload deadline exceeded"))
    } else {
        Ok(())
    }
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}
pub(crate) fn valid_collector(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}
fn validate_ids(collector: &str, epoch: &str) -> Result<()> {
    if !valid_collector(collector) || epoch.is_empty() || epoch.len() > 127 {
        return Err(HubError::new(
            "invalid_metadata",
            "invalid collector id or epoch",
        ));
    }
    Ok(())
}
fn private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(storage)
}
fn mkdir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(storage)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(storage)?;
    }
    Ok(())
}
fn sync_dir(path: &Path) -> Result<()> {
    File::open(path).and_then(|f| f.sync_all()).map_err(storage)
}
fn durable_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().ok_or_else(|| storage("parent"))?;
    let temp = parent.join(format!(".{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut f = private_file(&temp)?;
        serde_json::to_writer(&mut f, value).map_err(storage)?;
        f.write_all(b"\n").map_err(storage)?;
        f.sync_all().map_err(storage)?;
        fs::rename(&temp, path).map_err(storage)?;
        sync_dir(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    match File::open(path) {
        Ok(f) => serde_json::from_reader(f)
            .map(Some)
            .map_err(|_| HubError::new("state_error", "stored operational state is invalid")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(storage(e)),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AcceptedUpload {
    pub time: i64,
    pub generation: u64,
    pub bytes: u64,
    pub identity: Identity,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CollectorStatus {
    pub last_contact: i64,
    pub last_accepted_upload: Option<AcceptedUpload>,
    pub newest_measurement_unix: Option<i64>,
    pub pending_review: bool,
    pub recent_errors: VecDeque<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HubStatus {
    pub protocol_version: u32,
    pub collectors: BTreeMap<String, CollectorStatus>,
}
impl Default for HubStatus {
    fn default() -> Self {
        Self {
            protocol_version: 1,
            collectors: BTreeMap::new(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Generation {
    metadata: UploadMetadata,
    earliest: Option<i64>,
    newest: Option<i64>,
    #[serde(default)]
    row_count: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Active {
    protocol_version: u32,
    active: Generation,
    previous: Option<Generation>,
}
struct State {
    status: HubStatus,
    rates: BTreeMap<String, VecDeque<Instant>>,
}
struct Inner {
    config: Config,
    token_hash: [u8; 32],
    state: Mutex<State>,
    control: Mutex<control::ControlQueue>,
    control_notify: tokio::sync::Notify,
    control_broadcast: tokio::sync::broadcast::Sender<control::ControlEvent>,
    _lock: File,
}
#[derive(Clone)]
pub struct Hub {
    inner: Arc<Inner>,
}

impl Hub {
    pub fn new(config: Config, token: Vec<u8>) -> Result<Self> {
        if token.is_empty() {
            return Err(HubError::new(
                "configuration",
                "collector token must not be empty",
            ));
        }
        mkdir(&config.data_root)?;
        let lock_path = config.data_root.join(".hub.lock");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(lock_path).map_err(storage)?;
        lock.try_lock_exclusive()
            .map_err(|_| HubError::new("already_running", "another hub owns this data root"))?;
        mkdir(&config.data_root.join("staging"))?;
        mkdir(&config.data_root.join("ring"))?;
        sync_dir(&config.data_root)?;
        // No request survives process restart. Only this process holds the lock.
        for entry in fs::read_dir(config.data_root.join("staging")).map_err(storage)? {
            let entry = entry.map_err(storage)?;
            if entry.file_type().map_err(storage)?.is_dir() {
                fs::remove_dir_all(entry.path()).map_err(storage)?;
            } else {
                fs::remove_file(entry.path()).map_err(storage)?;
            }
        }
        let status = read_json(&config.data_root.join("hub-state.json"))?.unwrap_or_default();
        let hash: [u8; 32] = Sha256::digest(&token).into();
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                token_hash: hash,
                state: Mutex::new(State {
                    status,
                    rates: BTreeMap::new(),
                }),
                control: Mutex::new(control::ControlQueue::new()),
                control_notify: tokio::sync::Notify::new(),
                control_broadcast: tokio::sync::broadcast::channel(16).0,
                _lock: lock,
            }),
        })
    }
    pub fn from_env(config: Config) -> Result<Self> {
        let path = std::env::var_os("OURA_COLLECTOR_TOKEN_FILE").ok_or_else(|| {
            HubError::new("configuration", "OURA_COLLECTOR_TOKEN_FILE is required")
        })?;
        let mut token = fs::read(path)
            .map_err(|_| HubError::new("configuration", "collector token file cannot be read"))?;
        while token.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
            token.pop();
        }
        Self::new(config, token)
    }
    pub fn authenticate(&self, token: Option<&str>) -> Result<()> {
        let candidate: [u8; 32] = Sha256::digest(token.unwrap_or("").as_bytes()).into();
        if bool::from(candidate.ct_eq(&self.inner.token_hash)) && token.is_some() {
            Ok(())
        } else {
            Err(HubError::new(
                "unauthorized",
                "collector authentication failed",
            ))
        }
    }
    pub fn check_request(&self, metadata: &UploadMetadata) -> Result<()> {
        validate_ids(&metadata.collector_id, &metadata.collector_epoch)?;
        if metadata.protocol_version != 1 {
            return Err(HubError::new(
                "protocol_version",
                "only protocol_version 1 is supported",
            ));
        }
        if metadata.size_bytes > self.inner.config.max_bytes {
            return Err(HubError::new(
                "too_large",
                "snapshot exceeds configured byte limit",
            ));
        }
        if metadata.sha256.len() != 64
            || !metadata
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(HubError::new(
                "invalid_metadata",
                "sha256 must be lowercase hexadecimal",
            ));
        }
        let mut state = self.inner.state.lock().map_err(storage)?;
        let recent = state
            .rates
            .entry(metadata.collector_id.clone())
            .or_default();
        let time = Instant::now();
        while recent
            .front()
            .is_some_and(|t| time.duration_since(*t) >= self.inner.config.rate_window)
        {
            recent.pop_front();
        }
        if recent.len() >= self.inner.config.rate_limit as usize {
            return Err(HubError::new(
                "rate_limited",
                "collector upload rate exceeded",
            ));
        }
        recent.push_back(time);
        state
            .status
            .collectors
            .entry(metadata.collector_id.clone())
            .or_default()
            .last_contact = now();
        self.save_status(&state.status)
    }
    fn save_status(&self, status: &HubStatus) -> Result<()> {
        durable_json(&self.inner.config.data_root.join("hub-state.json"), status)
    }
    pub fn record_error(&self, collector: &str, code: &str) {
        if !valid_collector(collector) {
            return;
        }
        // Only fixed machine codes survive; never persist arbitrary error text.
        let safe = match code {
            "sha256_mismatch"
            | "size_mismatch"
            | "sqlite_truncated"
            | "sqlite_magic"
            | "sqlite_open"
            | "missing_store_meta"
            | "missing_table"
            | "unsupported_schema"
            | "integrity_check"
            | "stale_generation"
            | "history_regression"
            | "generation_conflict"
            | "request_timeout"
            | "too_large"
            | "rate_limited" => code,
            _ => "upload_failed",
        };
        if let Ok(mut state) = self.inner.state.lock() {
            let status = state.status.collectors.entry(collector.into()).or_default();
            status.last_contact = now();
            status.recent_errors.push_back(safe.into());
            while status.recent_errors.len() > 16 {
                status.recent_errors.pop_front();
            }
            let _ = self.save_status(&state.status);
        }
    }
    pub fn stage(&self, metadata: &UploadMetadata) -> Result<StagedUpload> {
        self.stage_before(metadata, Instant::now() + self.inner.config.request_timeout)
    }
    pub fn stage_before(
        &self,
        metadata: &UploadMetadata,
        deadline: Instant,
    ) -> Result<StagedUpload> {
        deadline_check(deadline)?;
        validate_ids(&metadata.collector_id, &metadata.collector_epoch)?;
        let dir = self
            .inner
            .config
            .data_root
            .join("staging")
            .join(&metadata.collector_id);
        mkdir(&dir)?;
        let path = dir.join(Uuid::new_v4().to_string());
        let file = private_file(&path)?;
        Ok(StagedUpload {
            hub: self.clone(),
            metadata: metadata.clone(),
            path,
            file: Some(file),
            bytes: 0,
            hash: Sha256::new(),
            deadline,
            failed: None,
        })
    }
    pub fn ingest(
        &self,
        token: Option<&str>,
        metadata: UploadMetadata,
        bytes: &[u8],
    ) -> Result<UploadAck> {
        self.authenticate(token)?;
        let id = metadata.collector_id.clone();
        let result = (|| {
            self.check_request(&metadata)?;
            let mut stage = self.stage(&metadata)?;
            stage.write_chunk(bytes)?;
            let validated = stage.finish()?;
            self.commit(validated)
        })();
        if let Err(e) = &result {
            self.record_error(&id, &e.code);
        }
        result
    }
    fn collector_dir(&self, id: &str) -> PathBuf {
        self.inner.config.data_root.join("ring").join(id)
    }
    fn generation_path(&self, g: &Generation) -> PathBuf {
        self.collector_dir(&g.metadata.collector_id)
            .join(hex(g.metadata.collector_epoch.as_bytes()))
            .join(format!("{}.db", g.metadata.generation))
    }
    fn active(&self, id: &str) -> Result<Option<Active>> {
        read_json(&self.collector_dir(id).join("active.json"))
    }
    pub fn active_path(&self, collector: &str) -> Result<Option<PathBuf>> {
        validate_ids(collector, "lookup")?;
        Ok(self
            .active(collector)?
            .map(|a| self.generation_path(&a.active)))
    }
    pub fn status(&self) -> Result<HubStatus> {
        Ok(self.inner.state.lock().map_err(storage)?.status.clone())
    }
    pub fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0)
    }
    pub fn control(&self) -> &Mutex<control::ControlQueue> {
        &self.inner.control
    }
    pub fn control_notify(&self) -> &tokio::sync::Notify {
        &self.inner.control_notify
    }
    pub fn control_broadcast(
        &self,
    ) -> &tokio::sync::broadcast::Sender<control::ControlEvent> {
        &self.inner.control_broadcast
    }
    fn broadcast_control(&self) {
        let _ = self
            .inner
            .control_broadcast
            .send(control::ControlEvent::Queue(self.control_snapshot()));
    }
    /// Publish a live-HR sample to dashboard subscribers. Called when a
    /// collector forwards a bounded stream message on the control channel.
    pub fn broadcast_stream(&self, message: oura_protocol_v1::ControlMessage) {
        let _ = self
            .inner
            .control_broadcast
            .send(control::ControlEvent::Stream(message));
    }
    /// Queue an allowlisted command and wake a connected collector.
    ///
    /// The queue is in-memory only. A hub restart drops queued commands rather
    /// than replaying them, because the hub cannot prove the Pi did not already
    /// receive one. The operator must re-issue after a restart.
    pub fn enqueue_command(
        &self,
        name: oura_protocol_v1::CommandName,
        args: serde_json::Value,
        ttl: Duration,
    ) -> Result<control::QueuedCommand, String> {
        let now = Self::now();
        let mut queue = self
            .inner
            .control
            .lock()
            .map_err(|_| "control queue unavailable".to_string())?;
        let id = Uuid::new_v4().to_string();
        let command = queue.enqueue(id, name, args, now, ttl)?;
        drop(queue);
        self.inner.control_notify.notify_waiters();
        self.broadcast_control();
        Ok(command)
    }
    pub fn record_control_receipt(
        &self,
        id: &str,
        state: oura_protocol_v1::ReceiptState,
        reason: Option<String>,
    ) -> Option<oura_protocol_v1::ControlMessage> {
        let message = self
            .inner
            .control
            .lock()
            .ok()
            .and_then(|mut queue| queue.record_receipt(id, state, reason));
        if message.is_some() {
            self.broadcast_control();
        }
        message
    }
    pub fn record_control_result(
        &self,
        id: &str,
        state: oura_protocol_v1::ResultState,
        detail: serde_json::Value,
    ) -> Option<oura_protocol_v1::ControlMessage> {
        let message = self
            .inner
            .control
            .lock()
            .ok()
            .and_then(|mut queue| queue.record_result(id, state, detail));
        if message.is_some() {
            self.broadcast_control();
        }
        message
    }
    pub fn control_snapshot(&self) -> Vec<control::QueuedCommand> {
        self.inner
            .control
            .lock()
            .map(|queue| queue.snapshot())
            .unwrap_or_default()
    }
    pub fn commit(&self, upload: ValidatedUpload) -> Result<UploadAck> {
        let deadline = upload.stage.deadline;
        self.commit_before(upload, deadline)
    }
    pub fn commit_before(&self, upload: ValidatedUpload, deadline: Instant) -> Result<UploadAck> {
        let mut state = self.inner.state.lock().map_err(storage)?;
        deadline_check(deadline)?;
        let generation = Generation {
            metadata: upload.stage.metadata.clone(),
            earliest: upload.earliest,
            newest: upload.newest,
            row_count: upload.row_count,
        };
        let m = &generation.metadata;
        let current = self.active(&m.collector_id)?;
        if let Some(active) = &current {
            if active.active.metadata.collector_epoch == m.collector_epoch {
                if m.generation <= active.active.metadata.generation {
                    if active.active.metadata.sha256 == m.sha256
                        && files_equal(
                            &upload.stage.path,
                            &self.generation_path(&active.active),
                            deadline,
                        )?
                    {
                        deadline_check(deadline)?;
                        let pending: BTreeMap<String, Generation> =
                            read_json(&self.collector_dir(&m.collector_id).join("pending.json"))?
                                .unwrap_or_default();
                        self.update_status(&mut state.status, &active.active, !pending.is_empty());
                        state
                            .status
                            .collectors
                            .get_mut(&m.collector_id)
                            .unwrap()
                            .newest_measurement_unix = active.active.newest;
                        self.save_status(&state.status)?;
                        return Ok(ack(
                            m.generation,
                            active.active.metadata.generation,
                            true,
                            "duplicate",
                        ));
                    }
                    return Err(HubError::new(
                        "stale_generation",
                        "generation is not newer than the active generation",
                    ));
                }
                if history_lost(generation.earliest, active.active.earliest)
                    && (generation.row_count < active.active.row_count
                        || !active_history_retained(
                            &upload.stage.path,
                            &self.generation_path(&active.active),
                            deadline,
                        )?)
                {
                    return Err(HubError::new(
                        "history_regression",
                        "snapshot starts later than active history",
                    ));
                }
            }
        }
        let target = self.generation_path(&generation);
        let parent = target.parent().ok_or_else(|| storage("parent"))?;
        let pending_path = self.collector_dir(&m.collector_id).join("pending.json");
        let mut pending: BTreeMap<String, Generation> =
            read_json(&pending_path)?.unwrap_or_default();
        let new_epoch = current
            .as_ref()
            .is_some_and(|a| a.active.metadata.collector_epoch != m.collector_epoch);
        if new_epoch
            && pending
                .get(&m.collector_epoch)
                .is_some_and(|old| m.generation < old.metadata.generation)
        {
            return Err(HubError::new(
                "stale_generation",
                "generation is older than pending review",
            ));
        }
        let exists = target.exists();
        if exists && !files_equal(&upload.stage.path, &target, deadline)? {
            return Err(HubError::new(
                "generation_conflict",
                "immutable generation already has different bytes",
            ));
        }
        deadline_check(deadline)?;
        // Once mutation starts, finish durability before returning, even if the
        // deadline expires during fsync. Never return a timed-out live commit.
        mkdir(&self.collector_dir(&m.collector_id))?;
        sync_dir(&self.inner.config.data_root.join("ring"))?;
        mkdir(parent)?;
        sync_dir(&self.collector_dir(&m.collector_id))?;
        if exists {
            File::open(&target)
                .and_then(|f| f.sync_all())
                .map_err(storage)?;
        } else {
            // A hard link is create-if-absent: unlike rename it cannot replace an
            // immutable generation. The staging file was fsynced in validation.
            fs::hard_link(&upload.stage.path, &target).map_err(storage)?;
        }
        sync_dir(parent)?;
        if new_epoch {
            let duplicate = pending
                .get(&m.collector_epoch)
                .is_some_and(|old| old.metadata.generation == m.generation);
            pending.insert(m.collector_epoch.clone(), generation.clone());
            durable_json(&pending_path, &pending)?;
            self.update_status(&mut state.status, &generation, true);
            self.save_status(&state.status)?;
            return Ok(ack(
                m.generation,
                current.as_ref().unwrap().active.metadata.generation,
                duplicate,
                "pending_review",
            ));
        }
        durable_json(
            &self.collector_dir(&m.collector_id).join("active.json"),
            &Active {
                protocol_version: 1,
                active: generation.clone(),
                previous: current.map(|a| a.active),
            },
        )?;
        self.update_status(&mut state.status, &generation, !pending.is_empty());
        state
            .status
            .collectors
            .get_mut(&m.collector_id)
            .unwrap()
            .newest_measurement_unix = generation.newest;
        self.save_status(&state.status)?;
        Ok(ack(m.generation, m.generation, false, "activated"))
    }
    fn update_status(&self, status: &mut HubStatus, g: &Generation, pending: bool) {
        let s = status
            .collectors
            .entry(g.metadata.collector_id.clone())
            .or_default();
        s.last_contact = now();
        s.last_accepted_upload = Some(AcceptedUpload {
            time: now(),
            generation: g.metadata.generation,
            bytes: g.metadata.size_bytes,
            identity: g.metadata.identity.clone(),
        });
        s.pending_review = pending;
        if !pending {
            s.newest_measurement_unix = g.newest;
        }
    }
    pub fn accept_epoch(&self, collector: &str, epoch: &str) -> Result<UploadAck> {
        validate_ids(collector, epoch)?;
        let mut state = self.inner.state.lock().map_err(storage)?;
        let path = self.collector_dir(collector).join("pending.json");
        let mut pending: BTreeMap<String, Generation> = read_json(&path)?.unwrap_or_default();
        let generation = pending
            .remove(epoch)
            .ok_or_else(|| HubError::new("pending_review", "epoch has no reviewed snapshot"))?;
        let file = self.generation_path(&generation);
        // Revalidate before operator promotion, including hash and integrity.
        validate_file(
            &file,
            &generation.metadata,
            Instant::now() + self.inner.config.request_timeout,
        )?;
        File::open(&file)
            .and_then(|f| f.sync_all())
            .map_err(storage)?;
        sync_dir(file.parent().unwrap())?;
        let current = self.active(collector)?;
        durable_json(
            &self.collector_dir(collector).join("active.json"),
            &Active {
                protocol_version: 1,
                active: generation.clone(),
                previous: current.map(|a| a.active),
            },
        )?;
        durable_json(&path, &pending)?;
        self.update_status(&mut state.status, &generation, !pending.is_empty());
        // Promotion is active even when other epochs await review.
        state
            .status
            .collectors
            .get_mut(collector)
            .unwrap()
            .newest_measurement_unix = generation.newest;
        self.save_status(&state.status)?;
        Ok(ack(
            generation.metadata.generation,
            generation.metadata.generation,
            false,
            "epoch accepted",
        ))
    }
}
fn ack(generation: u64, active_generation: u64, duplicate: bool, message: &str) -> UploadAck {
    UploadAck {
        protocol_version: 1,
        accepted: true,
        generation,
        active_generation,
        duplicate,
        message: message.into(),
    }
}
fn history_lost(candidate: Option<i64>, active: Option<i64>) -> bool {
    match (candidate, active) {
        (Some(c), Some(a)) => c > a,
        (None, Some(_)) => true,
        _ => false,
    }
}

// Resolving more clock observations can move the projected time of old events.
// A row count does not prove a superset: prove retention of every old raw event,
// including capture provenance, before overriding an apparent time regression.
// The ring-store unique index makes each candidate membership lookup bounded.
fn active_history_retained(candidate: &Path, active: &Path, deadline: Instant) -> Result<bool> {
    deadline_check(deadline)?;
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let candidate = Connection::open_with_flags(candidate, flags).map_err(storage)?;
    let active = Connection::open_with_flags(active, flags).map_err(storage)?;
    candidate.progress_handler(1000, Some(move || Instant::now() >= deadline));
    active.progress_handler(1000, Some(move || Instant::now() >= deadline));
    let mut old = active
        .prepare("SELECT serial, tag, ring_timestamp, body, captured_unix FROM events")
        .map_err(storage)?;
    let mut present = candidate.prepare("SELECT EXISTS(SELECT 1 FROM events WHERE serial=?1 AND tag=?2 AND ring_timestamp=?3 AND body=?4 AND captured_unix=?5)").map_err(storage)?;
    let mut rows = old.query([]).map_err(storage)?;
    while let Some(row) = rows.next().map_err(storage)? {
        deadline_check(deadline)?;
        let exists: bool = present
            .query_row(
                rusqlite::params![
                    rusqlite::types::ToSqlOutput::Borrowed(row.get_ref(0).map_err(storage)?),
                    rusqlite::types::ToSqlOutput::Borrowed(row.get_ref(1).map_err(storage)?),
                    rusqlite::types::ToSqlOutput::Borrowed(row.get_ref(2).map_err(storage)?),
                    rusqlite::types::ToSqlOutput::Borrowed(row.get_ref(3).map_err(storage)?),
                    rusqlite::types::ToSqlOutput::Borrowed(row.get_ref(4).map_err(storage)?),
                ],
                |row| row.get(0),
            )
            .map_err(storage)?;
        if !exists {
            return Ok(false);
        }
    }
    deadline_check(deadline)?;
    Ok(true)
}
fn files_equal(a: &Path, b: &Path, deadline: Instant) -> Result<bool> {
    let mut a = File::open(a).map_err(storage)?;
    let mut b = File::open(b).map_err(storage)?;
    if a.metadata().map_err(storage)?.len() != b.metadata().map_err(storage)?.len() {
        return Ok(false);
    }
    let mut left = [0u8; 65536];
    let mut right = [0u8; 65536];
    loop {
        deadline_check(deadline)?;
        let n = a.read(&mut left).map_err(storage)?;
        b.read_exact(&mut right[..n]).map_err(storage)?;
        if left[..n] != right[..n] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

pub struct StagedUpload {
    hub: Hub,
    metadata: UploadMetadata,
    path: PathBuf,
    file: Option<File>,
    bytes: u64,
    hash: Sha256,
    deadline: Instant,
    failed: Option<String>,
}
impl StagedUpload {
    pub fn write_chunk(&mut self, bytes: &[u8]) -> Result<()> {
        deadline_check(self.deadline)?;
        if self.failed.is_some() {
            return Err(HubError::new(
                "too_large",
                "snapshot exceeds configured byte limit",
            ));
        }
        if bytes.len() as u64 > self.hub.inner.config.max_bytes.saturating_sub(self.bytes) {
            self.failed = Some("too_large".into());
            return Err(HubError::new(
                "too_large",
                "snapshot exceeds configured byte limit",
            ));
        }
        self.file
            .as_mut()
            .ok_or_else(|| storage("closed"))?
            .write_all(bytes)
            .map_err(storage)?;
        self.bytes += bytes.len() as u64;
        self.hash.update(bytes);
        Ok(())
    }
    pub fn finish(self) -> Result<ValidatedUpload> {
        let deadline = self.deadline;
        self.finish_before(deadline)
    }
    pub fn finish_before(mut self, deadline: Instant) -> Result<ValidatedUpload> {
        self.deadline = self.deadline.min(deadline);
        deadline_check(self.deadline)?;
        if let Some(code) = &self.failed {
            return Err(HubError::new(
                code,
                "snapshot exceeds configured byte limit",
            ));
        }
        if self.bytes != self.metadata.size_bytes {
            return Err(HubError::new(
                "size_mismatch",
                "actual byte count differs from size_bytes",
            ));
        }
        if hex(&self.hash.clone().finalize()) != self.metadata.sha256 {
            return Err(HubError::new(
                "sha256_mismatch",
                "snapshot SHA-256 differs from metadata",
            ));
        }
        self.file.take().unwrap().sync_all().map_err(storage)?;
        let (earliest, newest, row_count) =
            validate_structure(&self.path, &self.metadata, self.deadline)?;
        Ok(ValidatedUpload {
            stage: self,
            earliest,
            newest,
            row_count,
        })
    }
}
impl Write for StagedUpload {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.write_chunk(buf).map_err(std::io::Error::other)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.as_mut().unwrap().flush()
    }
}
impl Drop for StagedUpload {
    fn drop(&mut self) {
        self.file.take();
        let _ = fs::remove_file(&self.path);
    }
}
pub struct ValidatedUpload {
    stage: StagedUpload,
    earliest: Option<i64>,
    newest: Option<i64>,
    row_count: u64,
}
fn validate_file(
    path: &Path,
    m: &UploadMetadata,
    deadline: Instant,
) -> Result<(Option<i64>, Option<i64>, u64)> {
    let mut f = File::open(path).map_err(storage)?;
    if f.metadata().map_err(storage)?.len() != m.size_bytes {
        return Err(HubError::new(
            "size_mismatch",
            "actual byte count differs from size_bytes",
        ));
    }
    let mut hash = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        deadline_check(deadline)?;
        let n = f.read(&mut buf).map_err(storage)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    if hex(&hash.finalize()) != m.sha256 {
        return Err(HubError::new(
            "sha256_mismatch",
            "snapshot SHA-256 differs from metadata",
        ));
    }
    validate_structure(path, m, deadline)
}
fn validate_structure(
    path: &Path,
    m: &UploadMetadata,
    deadline: Instant,
) -> Result<(Option<i64>, Option<i64>, u64)> {
    deadline_check(deadline)?;
    let mut header = [0u8; 100];
    let mut file = File::open(path).map_err(storage)?;
    let length = file.metadata().map_err(storage)?.len();
    if length < SQLITE_MAGIC.len() as u64 {
        return Err(HubError::new(
            "sqlite_truncated",
            "snapshot is shorter than the SQLite header",
        ));
    }
    file.read_exact(&mut header[..16]).map_err(storage)?;
    if &header[..16] != SQLITE_MAGIC {
        return Err(HubError::new(
            "sqlite_magic",
            "snapshot does not start with SQLite magic",
        ));
    }
    if length < 100 {
        return Err(HubError::new(
            "sqlite_truncated",
            "SQLite database header is truncated",
        ));
    }
    file.read_exact(&mut header[16..]).map_err(storage)?;
    let page = match u16::from_be_bytes([header[16], header[17]]) {
        1 => 65536,
        n => u64::from(n),
    };
    let pages = u64::from(u32::from_be_bytes(header[28..32].try_into().unwrap()));
    if page >= 512
        && (length % page != 0
            || pages
                .checked_mul(page)
                .is_none_or(|expected| expected > length))
    {
        return Err(HubError::new(
            "sqlite_truncated",
            "SQLite database pages are truncated",
        ));
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| HubError::new("sqlite_open", "snapshot cannot be opened read-only"))?;
    connection.progress_handler(1000, Some(move || Instant::now() >= deadline));
    let query_error = |code: &str, message: &str| {
        if Instant::now() >= deadline {
            HubError::new("request_timeout", "upload deadline exceeded")
        } else {
            HubError::new(code, message)
        }
    };
    let exists = |table: &str| -> Result<bool> {
        connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map(|v| v.is_some())
            .map_err(|_| query_error("sqlite_open", "SQLite schema cannot be read"))
    };
    if !exists("store_meta")? {
        return Err(HubError::new(
            "missing_store_meta",
            "store_meta table is required",
        ));
    }
    for table in ["device", "sync_state", "events", "readings"] {
        if !exists(table)? {
            return Err(HubError::new(
                "missing_table",
                &format!("ring-store table {table} is required"),
            ));
        }
    }
    let decoder = connection
        .query_row(
            "SELECT value FROM store_meta WHERE key='decoder_version'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| {
            query_error(
                "unsupported_schema",
                "store_meta decoder version cannot be read",
            )
        })?
        .and_then(|s| s.parse::<i64>().ok());
    if decoder != Some(m.schema_version) || m.schema_version != SUPPORTED_SCHEMA_VERSION {
        return Err(HubError::new(
            "unsupported_schema",
            &format!(
                "declared schema {}, stored decoder {:?}; supported schema {}",
                m.schema_version, decoder, SUPPORTED_SCHEMA_VERSION
            ),
        ));
    }
    let mut statement = connection
        .prepare("PRAGMA integrity_check")
        .map_err(|_| query_error("integrity_check", "SQLite integrity check failed"))?;
    let mut rows = statement
        .query([])
        .map_err(|_| query_error("integrity_check", "SQLite integrity check failed"))?;
    let row = rows
        .next()
        .map_err(|_| query_error("integrity_check", "SQLite integrity check failed"))?
        .ok_or_else(|| {
            query_error(
                "integrity_check",
                "SQLite integrity check returned no result",
            )
        })?;
    let result: String = row
        .get(0)
        .map_err(|_| query_error("integrity_check", "SQLite integrity check failed"))?;
    if result != "ok"
        || rows
            .next()
            .map_err(|_| query_error("integrity_check", "SQLite integrity check failed"))?
            .is_some()
    {
        return Err(query_error(
            "integrity_check",
            "SQLite integrity check failed",
        ));
    }
    deadline_check(deadline)?;
    // device.mac is ordinary device metadata in this schema, not a storage key.
    // Never inspect or publish device records or decoded event payloads.
    // Ring counters reset on reboot. Reuse the dashboard clock rather than
    // comparing counters or mistaking download time for measurement time.
    let mut events = connection.prepare("SELECT serial, ring_timestamp, tag, CASE WHEN tag IN (66,133) THEN COALESCE(decoded_json,'') ELSE '' END, captured_unix FROM events ORDER BY serial, captured_unix, id").map_err(|_| query_error("missing_table","events columns are required"))?;
    let mut rows = events
        .query([])
        .map_err(|_| query_error("integrity_check", "events cannot be read"))?;
    let mut serial = String::new();
    let mut group = Vec::new();
    let mut earliest = None;
    let mut newest = None;
    let mut row_count: u64 = 0;
    while let Some(row) = rows
        .next()
        .map_err(|_| query_error("integrity_check", "events cannot be read"))?
    {
        deadline_check(deadline)?;
        row_count += 1;
        let next: String = row
            .get(0)
            .map_err(|_| query_error("missing_table", "events columns are invalid"))?;
        if serial != next && !group.is_empty() {
            time_bounds(&group, &mut earliest, &mut newest);
            group.clear();
        }
        serial = next;
        group.push((
            row.get(1)
                .map_err(|_| query_error("missing_table", "events columns are invalid"))?,
            row.get::<_, i64>(2)
                .map_err(|_| query_error("missing_table", "events columns are invalid"))?
                as u8,
            row.get(3)
                .map_err(|_| query_error("missing_table", "events columns are invalid"))?,
            row.get(4)
                .map_err(|_| query_error("missing_table", "events columns are invalid"))?,
        ));
    }
    time_bounds(&group, &mut earliest, &mut newest);
    deadline_check(deadline)?;
    Ok((earliest, newest, row_count))
}
fn time_bounds(
    events: &[(i64, u8, String, i64)],
    earliest: &mut Option<i64>,
    newest: &mut Option<i64>,
) {
    let clock = oura_summary::ring_time::RingClock::from_events(events);
    for (counter, _, _, captured) in events {
        let unix = clock.unix_s(*counter, *captured);
        if unix.is_finite() {
            let unix = unix as i64;
            *earliest = Some(earliest.map_or(unix, |old| old.min(unix)));
            *newest = Some(newest.map_or(unix, |old| old.max(unix)));
        }
    }
}

pub mod ingest;
