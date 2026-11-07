//! Vendor-compatible ring storage and a durable, non-discarding snapshot outbox.
//!
//! Snapshot filenames use SHA-256(epoch), not the literal opaque epoch: v1 epoch
//! strings may contain path separators. Upload metadata preserves the exact epoch.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use oura_ble::{DeviceInfo, HistoryBatch};
use oura_protocol_v1::{UploadMetadata, PROTOCOL_VERSION};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{private_directory, private_new_file, reject_symlinks, sync_directory};
use crate::delivery::UploadSink;
use crate::{CollectorError, Config};

pub struct RingStore {
    connection: Connection,
    path: PathBuf,
}

impl RingStore {
    pub fn open(path: &Path) -> Result<Self, CollectorError> {
        reject_symlinks(path)?;
        if path.file_name().and_then(|name| name.to_str()).map(|name| name.eq_ignore_ascii_case("manual.db")).unwrap_or(false) {
            return Err(CollectorError::Config("cannot open manual data as ring storage".into()));
        }
        if let Some(parent) = path.parent() { private_directory(parent)?; }
        if path.exists() {
            let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            check_ring_tables(&connection)?;
        }
        // Use the pinned vendor migration and decoder, not a competing schema.
        drop(oura_store::Store::open(path).map_err(|_| CollectorError::Protocol("vendor ring store initialization failed".into()))?);
        let connection = open_private_database(path)?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
        Ok(Self { connection, path: path.to_path_buf() })
    }

    /// Raw bodies, decoded rows and cursor commit atomically. Partial batches do
    /// not advance the cursor; replay de-duplicates their already retained rows.
    pub fn write_batch(&self, serial: &str, batch: &HistoryBatch) -> Result<usize, CollectorError> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs().min(i64::MAX as u64) as i64).unwrap_or(0);
        self.write_batch_at(serial, batch, now)
    }

    pub fn write_batch_at(&self, serial: &str, batch: &HistoryBatch, now: i64) -> Result<usize, CollectorError> {
        if serial.is_empty() || batch.next_cursor > i64::MAX as u64 {
            return Err(CollectorError::Protocol("invalid serial or history cursor".into()));
        }
        let current: u64 = self.connection.query_row(
            "SELECT COALESCE((SELECT next_cursor FROM sync_state WHERE serial=?1),0)",
            [serial], |row| row.get(0),
        )?;
        if batch.complete && batch.next_cursor < current {
            return Err(CollectorError::Protocol("history cursor cannot move backwards".into()));
        }
        let tx = self.connection.unchecked_transaction()?;
        let mut inserted = 0;
        {
            let mut statement = tx.prepare_cached(
                "INSERT OR IGNORE INTO events(serial,tag,name,ring_timestamp,body,decoded_json,captured_unix) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            )?;
            for event in &batch.events {
                let tag = u8::try_from(event.tag).map_err(|_| CollectorError::Protocol("history tag exceeds vendor schema".into()))?;
                let timestamp = i64::try_from(event.ring_timestamp).map_err(|_| CollectorError::Protocol("ring timestamp exceeds SQLite range".into()))?;
                let decoded = oura_store::storage::decode_stored_event(tag, &event.body, None);
                inserted += statement.execute(params![serial, event.tag, oura_protocol::events::event_name(tag), timestamp, event.body, decoded, now])?;
            }
        }
        if batch.complete {
            tx.execute("INSERT INTO sync_state(serial,next_cursor,last_sync_unix) VALUES(?1,?2,?3) ON CONFLICT(serial) DO UPDATE SET next_cursor=excluded.next_cursor,last_sync_unix=excluded.last_sync_unix",
                params![serial, batch.next_cursor, now])?;
        }
        tx.commit()?;
        Ok(inserted)
    }

    pub fn event_count(&self) -> Result<u64, CollectorError> {
        Ok(self.connection.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?)
    }

    pub fn cursor(&self) -> Result<u64, CollectorError> {
        Ok(self.connection.query_row("SELECT COALESCE(MAX(next_cursor),0) FROM sync_state", [], |row| row.get(0))?)
    }

    pub fn cursor_for(&self, serial: &str) -> Result<u64, CollectorError> {
        Ok(self.connection.query_row("SELECT next_cursor FROM sync_state WHERE serial=?1", [serial], |row| row.get(0)).optional()?.unwrap_or(0))
    }

    pub fn record_device(&self, info: &DeviceInfo, now: i64) -> Result<(), CollectorError> {
        if info.serial.is_empty() { return Err(CollectorError::Protocol("device serial is empty".into())); }
        let tx = self.connection.unchecked_transaction()?;
        tx.execute("INSERT INTO device(serial,hardware_id,firmware,updated_unix) VALUES(?1,?2,?3,?4) ON CONFLICT(serial) DO UPDATE SET hardware_id=excluded.hardware_id,firmware=excluded.firmware,updated_unix=excluded.updated_unix",
            params![info.serial, info.hardware, info.firmware, now])?;
        tx.execute("INSERT INTO store_meta(key,value) VALUES('collector_active_serial',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [&info.serial])?;
        tx.commit()?;
        Ok(())
    }

    pub fn active_serial(&self) -> Result<Option<String>, CollectorError> {
        Ok(self.connection.query_row("SELECT value FROM store_meta WHERE key='collector_active_serial'", [], |row| row.get(0)).optional()?)
    }

    fn backup(&self, destination: &Path) -> Result<(), CollectorError> {
        check_ring_tables(&self.connection)?;
        drop(private_new_file(destination)?);
        let mut target = Connection::open(destination)?;
        {
            let backup = rusqlite::backup::Backup::new(&self.connection, &mut target)?;
            // A single SQLite read snapshot includes committed WAL pages.
            if !matches!(backup.step(-1)?, rusqlite::backup::StepResult::Done) {
                return Err(CollectorError::Protocol("snapshot source was busy".into()));
            }
        }
        target.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
        let integrity: String = target.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if integrity != "ok" { return Err(CollectorError::Protocol("snapshot integrity check failed".into())); }
        target.close().map_err(|(_, error)| error)?;
        fs::OpenOptions::new().read(true).write(true).open(destination)?.sync_all()?;
        Ok(())
    }
}

fn check_ring_tables(connection: &Connection) -> Result<(), CollectorError> {
    let mut query = connection.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")?;
    for name in query.query_map([], |row| row.get::<_, String>(0))? {
        if !matches!(name?.as_str(), "store_meta" | "device" | "sync_state" | "events" | "readings") {
            return Err(CollectorError::Protocol("ring database contains a non-ring table; snapshot refused".into()));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState { Queued, RetryWait, Delivering, Rejected, Missing, Corrupt }

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QueuedSnapshot {
    pub metadata: UploadMetadata,
    pub path: PathBuf,
    pub attempts: u32,
    pub next_attempt_unix: i64,
    pub state: DeliveryState,
    pub last_error: Option<String>,
}

pub struct SnapshotQueue {
    connection: Connection,
    config: Config,
    pending: Vec<QueuedSnapshot>,
    recovery_issues: Vec<String>,
}

impl SnapshotQueue {
    pub fn open(config: &Config) -> Result<Self, CollectorError> {
        config.prepare_state()?;
        let connection = open_private_database(&config.state_dir.join("delivery.sqlite"))?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS generations(epoch TEXT PRIMARY KEY,last_generation INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS snapshots(
                epoch TEXT NOT NULL,generation INTEGER NOT NULL,metadata TEXT NOT NULL,
                filename TEXT NOT NULL UNIQUE,attempts INTEGER NOT NULL DEFAULT 0,
                next_attempt INTEGER NOT NULL,state TEXT NOT NULL,last_error TEXT,
                PRIMARY KEY(epoch,generation));")?;
        let mut queue = Self { connection, config: config.clone(), pending: Vec::new(), recovery_issues: Vec::new() };
        queue.recover()?;
        queue.refresh()?;
        Ok(queue)
    }

    pub fn pending(&self) -> &[QueuedSnapshot] { &self.pending }
    pub fn recovery_issues(&self) -> &[String] { &self.recovery_issues }

    /// A generation is reserved durably before touching a snapshot file. Gaps are
    /// allowed; a generation is never reused, including after an interrupted build.
    pub fn create_snapshot(&mut self, store: &RingStore, now: i64) -> Result<UploadMetadata, CollectorError> {
        self.config.validate()?;
        if store.path != self.config.database_path {
            return Err(CollectorError::Config("snapshot store is outside the configured identity root".into()));
        }
        let estimate: u64 = store.connection.query_row("SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()", [], |row| row.get(0))?;
        self.check_capacity(estimate)?;
        let epoch_prefix = hex::encode(Sha256::digest(self.config.epoch.as_bytes()));
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: u64 = tx.query_row("SELECT last_generation FROM generations WHERE epoch=?1", [&self.config.epoch], |row| row.get(0)).optional()?.unwrap_or(0);
        let generation = previous.checked_add(1).filter(|n| *n <= i64::MAX as u64)
            .ok_or_else(|| CollectorError::Protocol("snapshot generation exhausted".into()))?;
        let filename = format!("{epoch_prefix}-{generation}.db");
        let path = self.config.outbox_dir().join(&filename);
        if path.exists() || partial_path(&path).exists() {
            return Err(CollectorError::Protocol("snapshot generation path already exists".into()));
        }
        let mut metadata = UploadMetadata {
            protocol_version: PROTOCOL_VERSION,
            schema_version: oura_store::storage::DECODER_VERSION,
            collector_id: self.config.collector_id.clone(),
            collector_epoch: self.config.epoch.clone(), generation, created_unix: now,
            size_bytes: 0, sha256: String::new(),
            build_revision: self.config.build_revision.clone(), identity: self.config.identity,
        };
        tx.execute("INSERT INTO generations(epoch,last_generation) VALUES(?1,?2) ON CONFLICT(epoch) DO UPDATE SET last_generation=excluded.last_generation", params![self.config.epoch, generation])?;
        tx.execute("INSERT INTO snapshots(epoch,generation,metadata,filename,next_attempt,state) VALUES(?1,?2,?3,?4,?5,'building')",
            params![self.config.epoch, generation, serde_json::to_string(&metadata)?, filename, now])?;
        tx.commit()?;
        let partial = partial_path(&path);
        if let Err(error) = store.backup(&partial) {
            self.connection.execute("UPDATE snapshots SET state='interrupted',last_error='snapshot build interrupted; retained for inspection' WHERE epoch=?1 AND generation=?2", params![self.config.epoch, generation])?;
            self.recovery_issues.push(format!("generation {generation}: incomplete snapshot retained"));
            return Err(error);
        }
        metadata.size_bytes = fs::metadata(&partial)?.len();
        metadata.sha256 = checksum(&partial)?;
        if metadata.size_bytes > estimate {
            // Check extra growth without counting this new partial twice.
            self.check_existing_capacity()?;
        }
        self.connection.execute("UPDATE snapshots SET metadata=?3,state='prepared' WHERE epoch=?1 AND generation=?2",
            params![self.config.epoch, generation, serde_json::to_string(&metadata)?])?;
        fs::rename(&partial, &path)?;
        sync_directory(&self.config.outbox_dir())?;
        self.connection.execute("UPDATE snapshots SET state='queued' WHERE epoch=?1 AND generation=?2", params![self.config.epoch, generation])?;
        self.refresh()?;
        Ok(metadata)
    }

    /// Attempts and their next retry are durable before entering the network.
    /// Unknown crash outcomes may be retried; duplicate server acceptance is safe.
    pub async fn deliver_due(&mut self, sink: &mut impl UploadSink, now: i64) -> Result<usize, CollectorError> {
        self.deliver_limit(sink, now, usize::MAX).await
    }

    /// Deliver at most one file so the actor's network delay stays bounded.
    pub async fn deliver_one_due(&mut self, sink: &mut impl UploadSink, now: i64) -> Result<usize, CollectorError> {
        self.deliver_limit(sink, now, 1).await
    }

    async fn deliver_limit(&mut self, sink: &mut impl UploadSink, now: i64, limit: usize) -> Result<usize, CollectorError> {
        let mut delivered = 0;
        let mut failed = false;
        let due: Vec<(String, u64)> = self.pending.iter()
            .filter(|item| matches!(item.state, DeliveryState::Queued | DeliveryState::RetryWait)
                && item.next_attempt_unix <= now)
            .take(limit)
            .map(|item| (item.metadata.collector_epoch.clone(), item.metadata.generation)).collect();
        for (epoch, generation) in due {
            let Some(item) = self.pending.iter().find(|item| item.metadata.collector_epoch == epoch && item.metadata.generation == generation).cloned() else { continue };
            if !snapshot_matches(&item.path, &item.metadata)? {
                self.set_state(&epoch, generation, "corrupt", Some("snapshot bytes do not match metadata"))?;
                failed = true;
                continue;
            }
            let attempts = item.attempts.saturating_add(1);
            let next = now.saturating_add(backoff(self.config.retry_min_secs, self.config.retry_max_secs, attempts) as i64);
            self.connection.execute("UPDATE snapshots SET attempts=?3,next_attempt=?4,state='delivering',last_error=NULL WHERE epoch=?1 AND generation=?2", params![epoch, generation, attempts, next])?;
            self.refresh()?;
            match sink.upload(&item.metadata, &item.path).await {
                Ok(ack) if ack.protocol_version == PROTOCOL_VERSION && ack.accepted
                    && ack.generation == generation
                    && (ack.active_generation >= generation || ack.message == "pending_review") => {
                    // Never remove a file before durable acknowledgement is recorded.
                    self.set_state(&epoch, generation, "accepted", None)?;
                    self.refresh()?;
                    fs::remove_file(&item.path)?;
                    sync_directory(&self.config.outbox_dir())?;
                    delivered += 1;
                }
                Ok(_) => {
                    self.set_state(&epoch, generation, "rejected", Some("hub acknowledgement rejected or mismatched; retained for inspection"))?;
                    failed = true;
                }
                Err(CollectorError::Protocol(_)) => {
                    self.set_state(&epoch, generation, "rejected", Some("hub rejected snapshot or acknowledgement; retained for inspection"))?;
                    failed = true;
                }
                Err(_) => {
                    // Do not persist arbitrary peer/HTTP errors that could contain credentials.
                    self.set_state(&epoch, generation, "retry_wait", Some("upload failed; snapshot retained"))?;
                    failed = true;
                }
            }
            self.refresh()?;
        }
        self.refresh()?;
        if failed { Err(CollectorError::Delivery("one or more snapshots remain undelivered; inspect queue state".into())) }
        else { Ok(delivered) }
    }

    fn set_state(&self, epoch: &str, generation: u64, state: &str, error: Option<&str>) -> Result<(), CollectorError> {
        self.connection.execute("UPDATE snapshots SET state=?3,last_error=?4 WHERE epoch=?1 AND generation=?2", params![epoch, generation, state, error])?;
        Ok(())
    }

    fn check_capacity(&self, additional: u64) -> Result<(), CollectorError> {
        let (files, bytes) = self.outbox_usage()?;
        if files >= self.config.outbox_max_files || bytes.checked_add(additional).map(|n| n > self.config.outbox_max_bytes).unwrap_or(true) {
            return Err(CollectorError::OutboxFull);
        }
        if fs2::available_space(&self.config.state_dir)? < self.config.min_free_bytes.saturating_add(additional) {
            return Err(CollectorError::LowDisk);
        }
        Ok(())
    }

    fn check_existing_capacity(&self) -> Result<(), CollectorError> {
        let (files, bytes) = self.outbox_usage()?;
        if files > self.config.outbox_max_files || bytes > self.config.outbox_max_bytes { return Err(CollectorError::OutboxFull); }
        if fs2::available_space(&self.config.state_dir)? < self.config.min_free_bytes { return Err(CollectorError::LowDisk); }
        Ok(())
    }

    fn outbox_usage(&self) -> Result<(usize, u64), CollectorError> {
        let mut files = 0usize;
        let mut bytes = 0u64;
        for entry in fs::read_dir(self.config.outbox_dir())? {
            let entry = entry?;
            if !entry.file_type()?.is_file() { return Err(CollectorError::Protocol("outbox contains a non-regular artifact".into())); }
            files = files.saturating_add(1);
            bytes = bytes.saturating_add(entry.metadata()?.len());
        }
        Ok((files, bytes))
    }

    fn refresh(&mut self) -> Result<(), CollectorError> {
        let mut statement = self.connection.prepare("SELECT metadata,filename,attempts,next_attempt,state,last_error FROM snapshots WHERE state NOT IN ('accepted','building','prepared','interrupted') ORDER BY epoch,generation")?;
        let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, u32>(2)?, row.get::<_, i64>(3)?, row.get::<_, String>(4)?, row.get::<_, Option<String>>(5)?)))?;
        let mut pending = Vec::new();
        for row in rows {
            let (metadata, filename, attempts, next_attempt_unix, state, last_error) = row?;
            let metadata: UploadMetadata = serde_json::from_str(&metadata)?;
            validate_stored_identity(&metadata, &self.config)?;
            let state: DeliveryState = serde_json::from_value(serde_json::Value::String(state))?;
            pending.push(QueuedSnapshot { metadata, path: checked_artifact_path(&self.config, &filename)?, attempts, next_attempt_unix, state, last_error });
        }
        self.pending = pending;
        Ok(())
    }

    fn recover(&mut self) -> Result<(), CollectorError> {
        let records: Vec<(String, u64, String, String, String)> = {
            let mut statement = self.connection.prepare("SELECT epoch,generation,metadata,filename,state FROM snapshots ORDER BY epoch,generation")?;
            let records = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            records
        };
        for (epoch, generation, encoded, filename, state) in &records {
            let mut metadata: UploadMetadata = serde_json::from_str(encoded)?;
            validate_stored_identity(&metadata, &self.config)?;
            if &metadata.collector_epoch != epoch || metadata.generation != *generation {
                return Err(CollectorError::Protocol("queue record identity mismatch".into()));
            }
            let path = checked_artifact_path(&self.config, filename)?;
            let partial = partial_path(&path);
            match state.as_str() {
                "accepted" => {
                    if path.exists() { fs::remove_file(&path)?; sync_directory(&self.config.outbox_dir())?; }
                }
                "prepared" => {
                    if !path.exists() && partial.exists() && snapshot_matches(&partial, &metadata)? {
                        fs::rename(&partial, &path)?;
                        sync_directory(&self.config.outbox_dir())?;
                    }
                    if snapshot_matches(&path, &metadata)? { self.set_state(epoch, *generation, "queued", None)?; }
                    else { self.set_state(epoch, *generation, "corrupt", Some("interrupted snapshot finalization; artifact retained"))?; }
                }
                "building" if path.exists() => {
                    // A fully renamed snapshot is recoverable even if state update failed.
                    metadata.size_bytes = fs::metadata(&path)?.len();
                    metadata.sha256 = checksum(&path)?;
                    let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                    check_ring_tables(&connection)?;
                    let check: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
                    if check != "ok" { return Err(CollectorError::Protocol("interrupted snapshot is corrupt".into())); }
                    self.connection.execute("UPDATE snapshots SET metadata=?3,state='queued' WHERE epoch=?1 AND generation=?2", params![epoch, generation, serde_json::to_string(&metadata)?])?;
                }
                "building" | "interrupted" => {
                    self.set_state(epoch, *generation, "interrupted", Some("incomplete snapshot retained for inspection"))?;
                    self.recovery_issues.push(format!("epoch {} generation {generation}: interrupted snapshot retained", hex::encode(Sha256::digest(epoch.as_bytes()))));
                }
                _ => {
                    if !path.exists() { self.set_state(epoch, *generation, "missing", Some("queued snapshot file is missing"))?; }
                    else if !snapshot_matches(&path, &metadata)? { self.set_state(epoch, *generation, "corrupt", Some("queued snapshot checksum mismatch"))?; }
                    else if state == "delivering" || state == "exhausted" {
                        self.set_state(epoch, *generation, "retry_wait", Some("previous upload outcome uncertain after restart; retry remains eligible"))?;
                    }
                }
            }
            // Reconstruct counters from all durable reservations if the counter row
            // was removed, rather than reusing an accepted or interrupted number.
            self.connection.execute("INSERT INTO generations(epoch,last_generation) VALUES(?1,?2) ON CONFLICT(epoch) DO UPDATE SET last_generation=MAX(last_generation,excluded.last_generation)", params![epoch, generation])?;
        }
        for entry in fs::read_dir(self.config.outbox_dir())? {
            let entry = entry?;
            if !entry.file_type()?.is_file() { return Err(CollectorError::Protocol("outbox contains a non-regular artifact".into())); }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !records.iter().any(|row| row.3 == name || format!("{}.partial", row.3) == name) {
                return Err(CollectorError::Protocol("untracked outbox artifact retained; operator recovery required".into()));
            }
        }
        Ok(())
    }
}

fn validate_stored_identity(metadata: &UploadMetadata, config: &Config) -> Result<(), CollectorError> {
    if metadata.collector_id != config.collector_id || metadata.identity != config.identity || metadata.protocol_version != PROTOCOL_VERSION {
        return Err(CollectorError::Protocol("persisted queue belongs to another collector or protocol".into()));
    }
    Ok(())
}

fn checked_artifact_path(config: &Config, name: &str) -> Result<PathBuf, CollectorError> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_hexdigit() || b"-.".contains(&b)) || !name.ends_with(".db") || name.contains("..") {
        return Err(CollectorError::Protocol("invalid persisted snapshot filename".into()));
    }
    let path = config.outbox_dir().join(name);
    reject_symlinks(&path)?;
    reject_symlinks(&partial_path(&path))?;
    Ok(path)
}

fn partial_path(path: &Path) -> PathBuf { path.with_extension("db.partial") }

pub(crate) fn backoff(minimum: u64, maximum: u64, attempt: u32) -> u64 {
    minimum.saturating_mul(1u64.checked_shl(attempt.saturating_sub(1).min(63)).unwrap_or(u64::MAX)).min(maximum)
}

fn snapshot_matches(path: &Path, metadata: &UploadMetadata) -> Result<bool, CollectorError> {
    let info = match fs::metadata(path) {
        Ok(info) => info,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    reject_symlinks(path)?;
    Ok(info.is_file() && info.len() == metadata.size_bytes && checksum(path)? == metadata.sha256)
}

fn checksum(path: &Path) -> Result<String, CollectorError> {
    let mut source = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let bytes = source.read(&mut buffer)?;
        if bytes == 0 { break; }
        hash.update(&buffer[..bytes]);
    }
    Ok(hex::encode(hash.finalize()))
}

pub(crate) fn open_private_database(path: &Path) -> Result<Connection, CollectorError> {
    reject_symlinks(path)?;
    if !path.exists() { drop(private_new_file(path)?); }
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    Ok(connection)
}
