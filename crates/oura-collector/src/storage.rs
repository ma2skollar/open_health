use std::{fs::{self, File, OpenOptions}, path::PathBuf};

use bytes::Bytes;
use fs2::FileExt;
use oura_ble::{DeviceInfo, HistoryBatch};
use oura_protocol_v1::ingest::{Ack, Batch, DeviceMetadata, Heartbeat, RawEvent, TransportProvenance, VERSION};
use rand::RngCore;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::{config::no_symlinks, CollectorError, Config};

const RECEIPT_LIMIT: usize = 128;
const SIDECAR_ALLOWANCE: u64 = 1024 * 1024;

pub struct PendingBatch {
    pub batch: Batch,
    pub body: Bytes,
    pub attempts: u32,
    pub next_attempt_unix_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueStats {
    pub batches: u64,
    pub events: u64,
    pub bytes: u64,
    pub oldest_unix_ms: Option<i64>,
}

/// One owner, one SQLite database. A transaction commits frozen body, row IDs and
/// safe transport cursor together. There is no physiological decoding or dedup.
pub struct Outbox {
    connection: Connection,
    config: Config,
    _lease: File,
}

impl Outbox {
    pub fn open(config: &Config) -> Result<Self, CollectorError> {
        config.validate()?;
        prepare_directory(config)?;
        let lock_path = config.state_dir.join("collector.lock");
        no_symlinks(&lock_path)?;
        #[cfg(unix)]
        let lease = {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600)
                .custom_flags(libc::O_NOFOLLOW).open(&lock_path)?
        };
        #[cfg(not(unix))]
        return Err(CollectorError::Permissions);
        lease.try_lock_exclusive().map_err(|_| CollectorError::Busy)?;
        let path = config.database_path();
        for file in [path.clone(), suffix(&path, "-wal"), suffix(&path, "-shm")] { no_symlinks(&file)?; }
        let is_new = !path.try_exists()?;
        if !is_new { crate::config::private_file(&path)?; }
        let mut connection = Connection::open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        connection.busy_timeout(std::time::Duration::from_secs(2))?;
        // A zero-page file (new, or left empty by an interrupted first init) has
        // no schema yet, so auto_vacuum can still be chosen before WAL.
        let virgin = connection.pragma_query_value::<u64, _>(None, "page_count", |r| r.get(0))? == 0;
        if virgin { connection.pragma_update(None, "auto_vacuum", "FULL")?; }
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "wal_autocheckpoint", 1)?;
        connection.pragma_update(None, "journal_size_limit", 0)?;
        let page_size: u64 = connection.pragma_query_value(None, "page_size", |r| r.get(0))?;
        // A WAL transaction cannot exceed the DB page cap. Reserve another DB's
        // worth for WAL plus a third share for checkpoints and filesystem slack.
        let pages = (config.max_storage_bytes.saturating_sub(SIDECAR_ALLOWANCE) / 3 / page_size).max(1);
        let current_pages: u64 = connection.pragma_query_value(None, "page_count", |r| r.get(0))?;
        if current_pages > pages { return Err(CollectorError::OutboxFull); }
        connection.pragma_update(None, "max_page_count", pages)?;
        // Schema and singleton commit together, so a crash leaves either nothing
        // or a complete identity. A missing singleton is only initialized when no
        // payload or receipt exists; otherwise fail closed and keep every row.
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch("
            CREATE TABLE IF NOT EXISTS collector_state (
                singleton INTEGER PRIMARY KEY CHECK(singleton=1), format INTEGER NOT NULL,
                device_id TEXT NOT NULL, epoch TEXT NOT NULL, next_source_id INTEGER NOT NULL,
                next_batch_id INTEGER NOT NULL, cursor INTEGER NOT NULL, device_json TEXT NOT NULL,
                heartbeat_json TEXT NOT NULL, next_collection_ms INTEGER NOT NULL,
                collection_attempts INTEGER NOT NULL, last_heartbeat_ms INTEGER NOT NULL,
                blocked_code TEXT, clock_ms INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS batches (
                batch_id TEXT PRIMARY KEY, body BLOB NOT NULL, event_count INTEGER NOT NULL,
                captured_ms INTEGER NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
                next_attempt_ms INTEGER NOT NULL DEFAULT 0
            );
            CREATE TRIGGER IF NOT EXISTS immutable_batch BEFORE UPDATE OF batch_id,body,event_count,captured_ms ON batches
                BEGIN SELECT RAISE(ABORT, 'immutable_batch'); END;
            CREATE TABLE IF NOT EXISTS receipts (
                batch_id TEXT PRIMARY KEY, body_sha256 TEXT NOT NULL, ack BLOB NOT NULL
            );
        ")?;
        // Stores created before the scheduler clock column gain it with 0, which
        // never reads as a rollback.
        let has_clock: bool = tx.query_row("SELECT count(*) FROM pragma_table_info('collector_state') WHERE name='clock_ms'", [], |r| r.get(0))?;
        if !has_clock { tx.execute_batch("ALTER TABLE collector_state ADD COLUMN clock_ms INTEGER NOT NULL DEFAULT 0")?; }
        let state_rows: u64 = tx.query_row("SELECT count(*) FROM collector_state", [], |r| r.get(0))?;
        let initialize = state_rows == 0;
        if initialize {
            let rows: u64 = tx.query_row("SELECT (SELECT count(*) FROM batches)+(SELECT count(*) FROM receipts)", [], |r| r.get(0))?;
            if rows != 0 { return Err(CollectorError::State); }
            // Only a store without identity must meet the free-space reserve, and
            // before anything commits. An existing queue always opens so uploads
            // and acks can drain it under disk pressure; collection and heartbeat
            // enqueue stay gated by physical_check.
            if fs2::available_space(&config.state_dir)? < config.min_free_bytes { return Err(CollectorError::LowDisk); }
            let epoch = random_id()?;
            let device = DeviceMetadata { serial: None, firmware: None, hardware: None };
            tx.execute("INSERT INTO collector_state(singleton,format,device_id,epoch,next_source_id,next_batch_id,cursor,device_json,heartbeat_json,next_collection_ms,collection_attempts,last_heartbeat_ms,blocked_code,clock_ms) VALUES(1,1,?1,?2,1,1,0,?3,?4,0,0,0,NULL,0)",
                params![config.device_id, epoch, serde_json::to_string(&device)?, serde_json::to_string(&Heartbeat::default())?])?;
        }
        tx.commit()?;
        if initialize { File::open(&config.state_dir)?.sync_all()?; }
        let identity: (u32, String) = connection.query_row("SELECT format,device_id FROM collector_state WHERE singleton=1", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if identity != (1, config.device_id.clone()) { return Err(CollectorError::State); }
        let mut store = Self { connection, config: config.clone(), _lease: lease };
        store.checkpoint()?;
        Ok(store)
    }

    pub fn epoch(&self) -> Result<String, CollectorError> {
        Ok(self.connection.query_row("SELECT epoch FROM collector_state WHERE singleton=1", [], |r| r.get(0))?)
    }
    pub fn cursor(&self) -> Result<u64, CollectorError> {
        Ok(self.connection.query_row("SELECT cursor FROM collector_state WHERE singleton=1", [], |r| r.get(0))?)
    }
    pub fn blocked(&self) -> Result<bool, CollectorError> {
        Ok(self.connection.query_row("SELECT blocked_code IS NOT NULL FROM collector_state WHERE singleton=1", [], |r| r.get(0))?)
    }
    pub fn block_history(&mut self) -> Result<(), CollectorError> {
        self.connection.execute("UPDATE collector_state SET blocked_code='cursor_regression' WHERE singleton=1", [])?;
        self.checkpoint()
    }
    /// Scheduler clock check; call once per tick before any scheduling decision.
    /// Reads only, unless OS wall time is behind the last scheduler time persisted
    /// by a deadline/state commit (backward NTP/RTC correction). Then saved
    /// collection, upload-retry and heartbeat deadlines are bounded to their
    /// configured maximum delay from `now` in one commit. Frozen batch bodies
    /// and captured times are never touched. Returns true when it rebased.
    pub fn observe_clock(&mut self, now: i64) -> Result<bool, CollectorError> {
        if now < 0 { return Err(CollectorError::Clock); }
        let last: i64 = self.connection.query_row("SELECT clock_ms FROM collector_state WHERE singleton=1", [], |r| r.get(0))?;
        if now >= last { return Ok(false); }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let secs = |value: u64| now.saturating_add(value.saturating_mul(1000).min(i64::MAX as u64) as i64);
        let collection = secs(self.config.collection_interval_secs.max(self.config.retry_max_secs));
        let upload = secs(self.config.retry_max_secs);
        tx.execute("UPDATE collector_state SET next_collection_ms=min(next_collection_ms,?1),last_heartbeat_ms=min(last_heartbeat_ms,?2),clock_ms=?2 WHERE singleton=1", [collection, now])?;
        tx.execute("UPDATE batches SET next_attempt_ms=?1 WHERE next_attempt_ms>?1", [upload])?;
        tx.commit()?;
        self.checkpoint()?;
        Ok(true)
    }
    pub fn collection_due(&self, now: i64) -> Result<bool, CollectorError> {
        Ok(!self.blocked()? && self.connection.query_row("SELECT next_collection_ms <= ?1 FROM collector_state WHERE singleton=1", [now], |r| r.get(0))?)
    }
    pub fn stats(&self) -> Result<QueueStats, CollectorError> {
        Ok(self.connection.query_row("SELECT count(*),coalesce(sum(event_count),0),coalesce(sum(length(body)),0),min(captured_ms) FROM batches", [], |r|
            Ok(QueueStats { batches: r.get(0)?, events: r.get(1)?, bytes: r.get(2)?, oldest_unix_ms: r.get(3)? }))?)
    }
    pub fn heartbeat(&self) -> Result<Heartbeat, CollectorError> {
        let json: String = self.connection.query_row("SELECT heartbeat_json FROM collector_state WHERE singleton=1", [], |r| r.get(0))?;
        Ok(serde_json::from_str(&json)?)
    }
    pub fn save_heartbeat(&mut self, value: &Heartbeat) -> Result<(), CollectorError> {
        self.connection.execute("UPDATE collector_state SET heartbeat_json=?1 WHERE singleton=1", [serde_json::to_string(value)?])?;
        self.checkpoint()
    }

    /// Admission happens before BLE. No acknowledged or unacknowledged payload
    /// is ever evicted to make room. FULL leaves the old cursor intact.
    pub fn preflight_history(&mut self) -> Result<(), CollectorError> {
        self.checkpoint()?;
        if self.blocked()? { return Err(CollectorError::CursorRegression); }
        self.physical_check(self.config.wire_budget().saturating_mul(4))
    }
    pub fn physical_bytes(&self) -> Result<u64, CollectorError> {
        let path = self.config.database_path();
        let mut bytes = 0u64;
        for file in [path.clone(), suffix(&path, "-wal"), suffix(&path, "-shm"), self.config.state_dir.join("collector.lock")] {
            match fs::metadata(file) {
                Ok(meta) => bytes = bytes.saturating_add(meta.len()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(bytes)
    }
    fn physical_check(&self, additional: u64) -> Result<(), CollectorError> {
        if self.physical_bytes()?.saturating_add(additional).saturating_add(SIDECAR_ALLOWANCE) > self.config.max_storage_bytes {
            return Err(CollectorError::OutboxFull);
        }
        if fs2::available_space(&self.config.state_dir)? < self.config.min_free_bytes.saturating_add(additional) {
            return Err(CollectorError::LowDisk);
        }
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), CollectorError> {
        let (busy, _, _): (u32, u32, u32) = self.connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        if busy != 0 { return Err(CollectorError::Busy); }
        Ok(())
    }

    pub fn commit_history(&mut self, history: &HistoryBatch, info: &DeviceInfo, mut heartbeat: Heartbeat, captured_ms: i64) -> Result<Option<String>, CollectorError> {
        if captured_ms < 0 { return Err(CollectorError::Clock); }
        let cursor = self.cursor()?;
        if history.next_cursor < cursor || history.events.iter().any(|e| e.ring_timestamp < cursor) {
            self.connection.execute("UPDATE collector_state SET blocked_code='cursor_regression' WHERE singleton=1", [])?;
            self.checkpoint()?;
            return Err(CollectorError::CursorRegression);
        }
        let bytes = history.events.iter().fold(0usize, |total, event| total.saturating_add(event.body.len())
            .saturating_add(event.transport.as_ref().map_or(0, |value| value.raw.len())));
        if history.events.len() > self.config.history_max_events || bytes > self.config.history_max_bytes
            || (!history.events.is_empty() && history.next_cursor <= cursor)
            || history.events.iter().any(|e| e.ring_timestamp >= history.next_cursor) {
            return Err(CollectorError::HistoryLimit);
        }
        self.physical_check(self.config.wire_budget().saturating_mul(4))?;
        let device = DeviceMetadata { serial: Some(info.serial.clone()), firmware: Some(info.firmware.clone()), hardware: Some(info.hardware.clone()) };
        let old_json: String = self.connection.query_row("SELECT device_json FROM collector_state WHERE singleton=1", [], |r| r.get(0))?;
        let old: DeviceMetadata = serde_json::from_str(&old_json)?;
        if old.serial.is_some() && old.serial != device.serial { return Err(CollectorError::State); }
        heartbeat.ble_success_unix_ms = Some(captured_ms);
        let stats = self.stats()?;
        heartbeat.queue_count = stats.batches;
        heartbeat.queue_bytes = stats.bytes;
        heartbeat.queue_oldest_unix_ms = stats.oldest_unix_ms;
        let transaction = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (epoch, first, sequence): (String, u64, u64) = transaction.query_row("SELECT epoch,next_source_id,next_batch_id FROM collector_state WHERE singleton=1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let mut events = Vec::with_capacity(history.events.len());
        for (index, event) in history.events.iter().enumerate() {
            let source_row_id = first.checked_add(index as u64).filter(|id| *id <= i64::MAX as u64).ok_or(CollectorError::State)?;
            events.push(RawEvent { source_row_id, tag: event.tag, ring_counter: event.ring_timestamp,
                body_hex: hex::encode(&event.body), captured_unix_ms: captured_ms,
                clock_anchor: None, transport: event.transport.as_ref().map(|value| TransportProvenance {
                    encoding: value.encoding.clone(), raw_hex: hex::encode(&value.raw),
                    counter_value: value.counter_value, counter_unit: value.counter_unit.clone(),
                }) });
        }
        let id = if events.is_empty() { None } else {
            let id = format!("{epoch}-{sequence}");
            let batch = Batch { version: VERSION, device_id: self.config.device_id.clone(), collector_epoch: epoch,
                batch_id: id.clone(), collector_build: crate::COLLECTOR_BUILD.into(), device: device.clone(), events, heartbeat: heartbeat.clone() };
            let body = batch.to_bytes().map_err(|_| CollectorError::Contract)?;
            transaction.execute("INSERT INTO batches(batch_id,body,event_count,captured_ms) VALUES(?1,?2,?3,?4)", params![id, body, history.events.len(), captured_ms])?;
            Some(id)
        };
        // A safe partial checkpoint is durable progress, not a discarded error.
        transaction.execute("UPDATE collector_state SET cursor=?1,next_source_id=?2,next_batch_id=?3,device_json=?4,heartbeat_json=?5,next_collection_ms=?6,collection_attempts=0,clock_ms=?7 WHERE singleton=1",
            params![history.next_cursor, first.checked_add(history.events.len() as u64).ok_or(CollectorError::State)?, sequence + u64::from(id.is_some()), serde_json::to_string(&device)?, serde_json::to_string(&heartbeat)?, captured_ms.saturating_add((self.config.collection_interval_secs * 1000) as i64), captured_ms])?;
        transaction.commit()?;
        self.checkpoint()?;
        Ok(id)
    }

    /// At most one queued empty heartbeat; never build a heartbeat backlog.
    pub fn enqueue_heartbeat(&mut self, mut heartbeat: Heartbeat, now: i64) -> Result<bool, CollectorError> {
        let stats = self.stats()?;
        if stats.batches != 0 { return Ok(false); }
        let last: i64 = self.connection.query_row("SELECT last_heartbeat_ms FROM collector_state WHERE singleton=1", [], |r| r.get(0))?;
        if now < last.saturating_add((self.config.heartbeat_interval_secs * 1000) as i64) { return Ok(false); }
        self.physical_check(64 * 1024)?;
        heartbeat.queue_count = 0;
        heartbeat.queue_bytes = 0;
        heartbeat.queue_oldest_unix_ms = None;
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (epoch, sequence, device): (String, u64, String) = tx.query_row("SELECT epoch,next_batch_id,device_json FROM collector_state WHERE singleton=1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let batch = Batch { version: VERSION, device_id: self.config.device_id.clone(), collector_epoch: epoch.clone(), batch_id: format!("{epoch}-{sequence}"), collector_build: crate::COLLECTOR_BUILD.into(), device: serde_json::from_str(&device)?, events: Vec::new(), heartbeat };
        let body = batch.to_bytes().map_err(|_| CollectorError::Contract)?;
        tx.execute("INSERT INTO batches(batch_id,body,event_count,captured_ms) VALUES(?1,?2,0,?3)", params![batch.batch_id, body, now])?;
        tx.execute("UPDATE collector_state SET next_batch_id=next_batch_id+1,last_heartbeat_ms=?1,clock_ms=?1 WHERE singleton=1", [now])?;
        tx.commit()?;
        self.checkpoint()?;
        Ok(true)
    }

    pub fn next_pending(&self, now: i64) -> Result<Option<PendingBatch>, CollectorError> {
        // Head-of-line only: a deferred oldest row yields None without loading its body.
        let value: Option<(Vec<u8>, u32, i64)> = self.connection.query_row("SELECT body,attempts,next_attempt_ms FROM batches WHERE rowid=(SELECT min(rowid) FROM batches) AND next_attempt_ms<=?1", [now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
        match value {
            Some((body, attempts, due)) => {
                let batch: Batch = serde_json::from_slice(&body)?;
                batch.validate().map_err(|_| CollectorError::Contract)?;
                Ok(Some(PendingBatch { batch, body: body.into(), attempts, next_attempt_unix_ms: due }))
            }
            None => Ok(None),
        }
    }
    pub fn record_upload_failure(&mut self, id: &str, now: i64, jitter: u64) -> Result<(), CollectorError> {
        let attempts: u32 = self.connection.query_row("SELECT attempts FROM batches WHERE batch_id=?1", [id], |r| r.get(0))?;
        let attempts = attempts.saturating_add(1);
        let next = retry_at(&self.config, attempts, now, jitter);
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("UPDATE batches SET attempts=?1,next_attempt_ms=?2 WHERE batch_id=?3", params![attempts, next, id])?;
        tx.execute("UPDATE collector_state SET clock_ms=?1 WHERE singleton=1", [now])?;
        tx.commit()?;
        self.checkpoint()
    }
    pub fn record_collection_failure(&mut self, now: i64, jitter: u64) -> Result<(), CollectorError> {
        let attempts: u32 = self.connection.query_row("SELECT collection_attempts FROM collector_state WHERE singleton=1", [], |r| r.get(0))?;
        let attempts = attempts.saturating_add(1);
        self.connection.execute("UPDATE collector_state SET collection_attempts=?1,next_collection_ms=?2,clock_ms=?3 WHERE singleton=1", params![attempts, retry_at(&self.config, attempts, now, jitter), now])?;
        self.checkpoint()
    }

    /// Validate against the bytes in SQLite, not caller-supplied replacement data.
    /// Receipt insert and exact batch deletion share one FULL SQLite commit.
    pub fn acknowledge(&mut self, ack: &Ack, now: i64) -> Result<(), CollectorError> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let bytes: Option<Vec<u8>> = tx.query_row("SELECT body FROM batches WHERE batch_id=?1", [&ack.batch_id], |r| r.get(0)).optional()?;
        let bytes = bytes.ok_or(CollectorError::Ack)?;
        let batch: Batch = serde_json::from_slice(&bytes)?;
        ack.validate(&batch, &bytes).map_err(|_| CollectorError::Ack)?;
        // Free the exact acknowledged blob's pages first, inside the same
        // transaction. Receipt failure rolls this deletion back, even at FULL.
        tx.execute("DELETE FROM batches WHERE batch_id=?1", [&ack.batch_id])?;
        tx.execute("INSERT INTO receipts(batch_id,body_sha256,ack) VALUES(?1,?2,?3)", params![ack.batch_id, ack.body_sha256, serde_json::to_vec(ack)?])?;
        tx.execute("DELETE FROM receipts WHERE rowid NOT IN (SELECT rowid FROM receipts ORDER BY rowid DESC LIMIT ?1)", [RECEIPT_LIMIT])?;
        let json: String = tx.query_row("SELECT heartbeat_json FROM collector_state WHERE singleton=1", [], |r| r.get(0))?;
        let mut heartbeat: Heartbeat = serde_json::from_str(&json)?;
        heartbeat.upload_receipt_unix_ms = Some(now);
        tx.execute("UPDATE collector_state SET heartbeat_json=?1,clock_ms=?2 WHERE singleton=1", params![serde_json::to_string(&heartbeat)?, now])?;
        tx.commit()?;
        self.checkpoint()
    }
    pub fn receipt_count(&self) -> Result<u64, CollectorError> {
        Ok(self.connection.query_row("SELECT count(*) FROM receipts", [], |r| r.get(0))?)
    }
}

fn random_id() -> Result<String, CollectorError> {
    let mut random = [0u8; 16];
    rand::rngs::OsRng.try_fill_bytes(&mut random).map_err(|_| CollectorError::State)?;
    Ok(hex::encode(random))
}
fn suffix(path: &std::path::Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    name.into()
}
fn prepare_directory(config: &Config) -> Result<(), CollectorError> {
    no_symlinks(&config.state_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        if !config.state_dir.try_exists()? {
            fs::DirBuilder::new().mode(0o700).create(&config.state_dir)?;
        }
        let meta = fs::metadata(&config.state_dir)?;
        if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o7777 != 0o700 {
            return Err(CollectorError::Permissions);
        }
        Ok(())
    }
    #[cfg(not(unix))]
    Err(CollectorError::Permissions)
}
fn retry_at(config: &Config, attempts: u32, now: i64, jitter: u64) -> i64 {
    let ceiling = config.retry_min_secs.saturating_mul(1u64 << attempts.saturating_sub(1).min(32)).min(config.retry_max_secs);
    let delay = ceiling / 2 + jitter % (ceiling - ceiling / 2 + 1);
    now.saturating_add((delay.max(1).saturating_mul(1000)).min(i64::MAX as u64) as i64)
}
