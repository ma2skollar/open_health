//! Raw input is authoritative. `events` is a rebuildable physiological projection.
//! Acceptance never runs physiological decoding or model code.
use std::{fs::{self, OpenOptions}, path::{Path, PathBuf}, sync::Arc, time::{Duration, SystemTime, UNIX_EPOCH}};
use anyhow::{anyhow, Result};
use axum::{body::to_bytes, extract::{Request, State}, http::{HeaderMap, StatusCode}, response::{IntoResponse, Response}, Json, Router};
use fs2::FileExt;
use oura_protocol_v1::ingest::{self as wire, Ack, Batch, SigningFields};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Semaphore;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS ingest_batches (
 revision INTEGER PRIMARY KEY AUTOINCREMENT, device_id TEXT NOT NULL, batch_id TEXT NOT NULL,
 body_sha256 TEXT NOT NULL, body BLOB NOT NULL, receipt TEXT NOT NULL, accepted_unix_ms INTEGER NOT NULL,
 UNIQUE(device_id,batch_id));
CREATE TABLE IF NOT EXISTS ingest_nonces (key_id TEXT NOT NULL, nonce TEXT NOT NULL, expires_unix INTEGER NOT NULL, PRIMARY KEY(key_id,nonce));
CREATE TABLE IF NOT EXISTS ingest_raw (
 id INTEGER PRIMARY KEY AUTOINCREMENT, revision INTEGER NOT NULL, device_id TEXT NOT NULL,
 collector_epoch TEXT NOT NULL, source_row_id TEXT NOT NULL, serial TEXT NOT NULL,
 tag INTEGER NOT NULL, ring_counter INTEGER NOT NULL, body BLOB NOT NULL,
 captured_unix_ms INTEGER NOT NULL, event_json TEXT NOT NULL, transport BLOB,
 UNIQUE(device_id,collector_epoch,source_row_id));
CREATE TABLE IF NOT EXISTS ingest_clock_anchors (
 raw_id INTEGER PRIMARY KEY, kind TEXT NOT NULL, ring_counter INTEGER NOT NULL, captured_unix_ms INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS ingest_collectors (device_id TEXT PRIMARY KEY, received_unix_ms INTEGER NOT NULL, heartbeat TEXT NOT NULL, collector_build TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS ingest_jobs (
 revision INTEGER PRIMARY KEY, state TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
 next_attempt_unix INTEGER NOT NULL DEFAULT 0, last_error TEXT, stage TEXT NOT NULL DEFAULT 'decode',
 started_unix_ms INTEGER, completed_unix_ms INTEGER);
CREATE TABLE IF NOT EXISTS ingest_failures (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL, failed_unix_ms INTEGER NOT NULL, stage TEXT NOT NULL, code TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS ingest_processing (
 id INTEGER PRIMARY KEY CHECK(id=1), completed_revision INTEGER NOT NULL DEFAULT 0,
 decoded_revision INTEGER NOT NULL DEFAULT 0, summary TEXT, summary_unix_ms INTEGER);
INSERT OR IGNORE INTO ingest_processing(id) VALUES(1);
CREATE TABLE IF NOT EXISTS ingest_recovery (
 id INTEGER PRIMARY KEY CHECK(id=1), holdoff_until_unix INTEGER NOT NULL);
"#;

fn now_ms() -> i64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().min(i64::MAX as u128) as i64 }
fn open(db: &Path) -> Result<Connection> {
    let c = Connection::open(db)?;
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(db, fs::Permissions::from_mode(0o600))?;
    }
    c.busy_timeout(Duration::from_secs(5))?;
    c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
    c.execute_batch(SCHEMA)?;
    Ok(c)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry { keys: Vec<DeviceKey> }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceKey { key_id: String, device_id: String, key_hex: String, #[serde(default)] revoked: bool }

// Deliberately no Debug implementation: recoverable HMAC material must not leak.
fn key(path: &Path, fields: &SigningFields) -> Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path).map_err(|_| anyhow!("key_registry_unavailable"))?;
    let metadata = file.metadata()?;
    #[cfg(unix)] {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o777 != 0o600 || metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1 {
            return Err(anyhow!("unsafe_key_registry"));
        }
    }
    if !metadata.is_file() || metadata.len() > 65536 { return Err(anyhow!("unsafe_key_registry")); }
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    if bytes.len() > 65536 { return Err(anyhow!("unsafe_key_registry")); }
    let registry: Registry = serde_json::from_slice(&bytes).map_err(|_| anyhow!("invalid_key_registry"))?;
    let mut ids = std::collections::HashSet::new();
    if registry.keys.iter().any(|k| !ids.insert(k.key_id.as_str())) { return Err(anyhow!("invalid_key_registry")); }
    let entry = registry.keys.iter().find(|k| k.key_id == fields.key_id && k.device_id == fields.device_id && !k.revoked).ok_or_else(|| anyhow!("unauthorized"))?;
    let bytes = hex::decode(&entry.key_hex).map_err(|_| anyhow!("invalid_key_registry"))?;
    if bytes.len() != wire::KEY_BYTES { return Err(anyhow!("invalid_key_registry")); }
    Ok(bytes)
}

#[derive(Clone)]
pub struct Service { db: PathBuf, registry: PathBuf, slots: Arc<Semaphore>, pub body_timeout: Duration }
impl Service {
    /// Registry is an external owner-only JSON file, reloaded for every request.
    pub fn new(db: PathBuf, registry: PathBuf) -> Self {
        Self { db, registry, slots: Arc::new(Semaphore::new(8)), body_timeout: Duration::from_secs(15) }
    }
    /// Verify exact bytes and atomically commit nonce, receipt, raw rows and job.
    pub fn accept(&self, fields: &SigningFields, signature: &str, bytes: &[u8], now_seconds: i64) -> std::result::Result<Ack, IngestError> {
        if bytes.len() > wire::MAX_BODY_BYTES { return Err(IngestError::new(413, "body_too_large")); }
        let secret = key(&self.registry, fields).map_err(|_| IngestError::new(401, "unauthorized"))?;
        fields.verify(&secret, signature, now_seconds).map_err(|_| IngestError::new(401, "unauthorized"))?;
        if fields.body_sha256 != wire::body_hash(bytes) { return Err(IngestError::new(401, "unauthorized")); }
        let batch: Batch = serde_json::from_slice(bytes).map_err(|_| IngestError::new(400, "invalid_batch"))?;
        batch.validate().map_err(|_| IngestError::new(400, "invalid_batch"))?;
        if batch.device_id != fields.device_id { return Err(IngestError::new(401, "unauthorized")); }
        self.commit(fields, bytes, &batch, now_seconds).map_err(|e| e.downcast::<IngestError>().unwrap_or_else(|_| IngestError::new(503, "storage_unavailable")))
    }
    fn commit(&self, fields: &SigningFields, bytes: &[u8], batch: &Batch, now: i64) -> Result<Ack> {
        let mut c = open(&self.db)?;
        // Fail closed after a backup restore: refuse new ingestion while the
        // replay-protection window from `oura-backup` (table ingest_recovery) is open.
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let holdoff: Option<i64> = tx.query_row("SELECT holdoff_until_unix FROM ingest_recovery WHERE id=1", [], |r| r.get(0)).optional()?;
        if holdoff.is_some_and(|until| until > now) { return Err(IngestError::new(503, "restore_holdoff").into()); }
        tx.execute("DELETE FROM ingest_nonces WHERE expires_unix < ?1", [now])?;
        if tx.execute("INSERT OR IGNORE INTO ingest_nonces VALUES(?1,?2,?3)", params![fields.key_id, fields.nonce, now.saturating_add(900)])? == 0 {
            return Err(IngestError::new(409, "nonce_replay").into());
        }
        let previous: Option<(String,String)> = tx.query_row("SELECT body_sha256,receipt FROM ingest_batches WHERE device_id=?1 AND batch_id=?2", params![batch.device_id,batch.batch_id], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((hash,receipt)) = previous {
            if hash != fields.body_sha256 { return Err(IngestError::new(409,"batch_conflict").into()); }
            let ack = serde_json::from_str(&receipt)?;
            tx.commit()?;
            return Ok(ack);
        }
        let accepted = now.saturating_mul(1000);
        let ack = Ack { version: wire::VERSION, device_id: batch.device_id.clone(), batch_id: batch.batch_id.clone(), body_sha256: fields.body_sha256.clone(), durable: true, accepted_unix_ms: accepted };
        tx.execute("INSERT INTO ingest_batches(device_id,batch_id,body_sha256,body,receipt,accepted_unix_ms) VALUES(?1,?2,?3,?4,?5,?6)", params![batch.device_id,batch.batch_id,fields.body_sha256,bytes,serde_json::to_string(&ack)?,accepted])?;
        let revision = tx.last_insert_rowid();
        for event in &batch.events {
            let source = event.source_row_id.to_string();
            let encoded = serde_json::to_string(event)?;
            let prior: Option<String> = tx.query_row("SELECT event_json FROM ingest_raw WHERE device_id=?1 AND collector_epoch=?2 AND source_row_id=?3", params![batch.device_id,batch.collector_epoch,source], |r| r.get(0)).optional()?;
            if let Some(prior) = prior {
                if prior != encoded { return Err(IngestError::new(409,"source_conflict").into()); }
                continue;
            }
            let body = hex::decode(&event.body_hex)?;
            let transport = event.transport.as_ref().map(|t| hex::decode(&t.raw_hex)).transpose()?;
            tx.execute("INSERT INTO ingest_raw(revision,device_id,collector_epoch,source_row_id,serial,tag,ring_counter,body,captured_unix_ms,event_json,transport) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)", params![revision,batch.device_id,batch.collector_epoch,source,batch.device.serial.as_deref().unwrap_or(&batch.device_id),event.tag,event.ring_counter,body,event.captured_unix_ms,encoded,transport])?;
            let raw_id = tx.last_insert_rowid();
            if let Some(anchor) = &event.clock_anchor {
                tx.execute("INSERT INTO ingest_clock_anchors VALUES(?1,?2,?3,?4)",params![raw_id,anchor.kind,anchor.ring_counter,anchor.captured_unix_ms])?;
            }
        }
        tx.execute("INSERT INTO ingest_collectors VALUES(?1,?2,?3,?4) ON CONFLICT(device_id) DO UPDATE SET received_unix_ms=excluded.received_unix_ms,heartbeat=excluded.heartbeat,collector_build=excluded.collector_build",params![batch.device_id,accepted,serde_json::to_string(&batch.heartbeat)?,batch.collector_build])?;
        // Heartbeats do not need physiology or models to be accepted or marked complete.
        let state = if batch.events.is_empty() { "done" } else { "pending" };
        tx.execute("INSERT INTO ingest_jobs(revision,state) VALUES(?1,?2)",params![revision,state])?;
        tx.commit()?;
        Ok(ack)
    }
}

#[derive(Debug)]
pub struct IngestError { pub status: u16, pub code: &'static str }
impl IngestError { fn new(status: u16, code: &'static str) -> Self { Self {status,code} } }
impl std::fmt::Display for IngestError { fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result { f.write_str(self.code) } }
impl std::error::Error for IngestError {}
impl IntoResponse for IngestError { fn into_response(self)->Response { (StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),Json(json!({"error":self.code}))).into_response() } }
fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter(); let value = values.next()?;
    if values.next().is_some() { return None; } value.to_str().ok()
}
/// No control, status, redirect or bearer-snapshot routes exist on this listener.
pub fn router(service: Service) -> Router { Router::new().fallback(receive).with_state(service) }
async fn receive(State(service): State<Service>, request: Request) -> Response {
    if request.method().as_str() != wire::METHOD || request.uri().to_string() != wire::PATH { return IngestError::new(404,"not_found").into_response(); }
    let headers = request.headers();
    if header(headers,"content-type") != Some(wire::CONTENT_TYPE) || headers.contains_key("content-encoding") { return IngestError::new(415,"unsupported_media_type").into_response(); }
    let fields = (|| Some(SigningFields { key_id:header(headers,wire::HEADER_KEY_ID)?.into(),device_id:header(headers,wire::HEADER_DEVICE_ID)?.into(),timestamp:header(headers,wire::HEADER_TIMESTAMP)?.parse().ok()?,nonce:header(headers,wire::HEADER_NONCE)?.into(),body_sha256:header(headers,wire::HEADER_BODY_SHA256)?.into() }))();
    let Some(fields) = fields else { return IngestError::new(401,"unauthorized").into_response(); };
    let Some(signature) = header(headers,wire::HEADER_SIGNATURE).map(str::to_owned) else { return IngestError::new(401,"unauthorized").into_response(); };
    let length = if headers.contains_key("content-length") { match header(headers,"content-length").and_then(|v|v.parse::<usize>().ok()) {Some(n) if n <= wire::MAX_BODY_BYTES=>Some(n),_=>return IngestError::new(413,"body_too_large").into_response()} } else {None};
    let Ok(permit) = service.slots.clone().try_acquire_owned() else { return IngestError::new(429,"busy").into_response(); };
    let body = match tokio::time::timeout(service.body_timeout,to_bytes(request.into_body(),wire::MAX_BODY_BYTES)).await {
        Err(_)=>return IngestError::new(408,"body_timeout").into_response(),
        Ok(Err(error))=> { use std::error::Error; let too_large=error.source().is_some_and(|e|e.is::<http_body_util::LengthLimitError>()); return IngestError::new(if too_large {413}else{400},if too_large {"body_too_large"}else{"incomplete_body"}).into_response(); },
        Ok(Ok(body))=>body,
    };
    if length.is_some_and(|n|n!=body.len()) { return IngestError::new(400,"incomplete_body").into_response(); }
    match tokio::task::spawn_blocking(move || { let _permit=permit; service.accept(&fields,&signature,&body,now_ms()/1000) }).await {
        Ok(Ok(ack))=>(StatusCode::OK,Json(ack)).into_response(),Ok(Err(e))=>e.into_response(),Err(_)=>IngestError::new(503,"storage_unavailable").into_response()
    }
}

/// Read-only status: absent DB is never created, and differs from inaccessible DB.
pub fn status(db: &Path) -> Value {
    let result = (|| -> Result<Value> {
        let c=Connection::open_with_flags(db,OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        c.busy_timeout(Duration::from_secs(2))?;
        let installed: bool=c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='ingest_batches')",[],|r|r.get(0))?;
        if !installed { return Ok(json!({"state":"no_upload","revision":0,"raw_events":0,"decoded_events":0,"processing":{"state":"idle","last_error":null,"completed_revision":0},"collectors":[]})); }
        let revision:i64=c.query_row("SELECT COALESCE(MAX(revision),0) FROM ingest_batches",[],|r|r.get(0))?;
        let raw:i64=c.query_row("SELECT COUNT(*) FROM ingest_raw",[],|r|r.get(0))?;
        let projected:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='events')",[],|r|r.get(0))?;
        let decoded:i64=if projected {c.query_row("SELECT COUNT(*) FROM events WHERE decoded_json IS NOT NULL",[],|r|r.get(0))?}else{0};
        let (completed,decoded_revision,summary,summary_ms):(i64,i64,Option<String>,Option<i64>)=c.query_row("SELECT completed_revision,decoded_revision,summary,summary_unix_ms FROM ingest_processing WHERE id=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        let job:Option<(String,Option<String>,String,i64,i64)>=c.query_row("SELECT state,last_error,stage,attempts,next_attempt_unix FROM ingest_jobs WHERE state!='done' ORDER BY revision LIMIT 1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
        let mut statement=c.prepare("SELECT device_id,received_unix_ms,heartbeat,collector_build FROM ingest_collectors ORDER BY device_id")?;
        let collectors=statement.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))?.map(|row|{let (device,received,heartbeat,build)=row?;Ok(json!({"device_id":device,"received_unix_ms":received,"heartbeat":serde_json::from_str::<Value>(&heartbeat).unwrap_or(Value::Null),"collector_build":build}))}).collect::<rusqlite::Result<Vec<_>>>()?;
        let (state,error,stage,attempts,retry)=job.unwrap_or(("idle".into(),None,"complete".into(),0,0));
        let overall=if revision==0 {"no_upload"}else if state=="failed" {"analysis_failed"}else if state!="idle" {"raw_pending"}else if raw==0 {"collector_ready"}else if decoded==0 {"undecoded"}else{"ready"};
        Ok(json!({"state":overall,"revision":revision,"raw_events":raw,"decoded_events":decoded,"decoded_revision":decoded_revision,"analyzed_revision":completed,"processing":{"state":state,"last_error":error,"stage":stage,"attempts":attempts,"next_attempt_unix":retry,"completed_revision":completed},"collectors":collectors,"last_good_summary":summary.and_then(|s|serde_json::from_str::<Value>(&s).ok()),"summary_unix_ms":summary_ms}))
    })();
    match result {Ok(v)=>v,Err(_)=>json!({"state":if !db.exists(){"db_missing"}else{"db_inaccessible"},"revision":null,"raw_events":null,"decoded_events":null,"processing":{"state":"unavailable","last_error":"database_unavailable","completed_revision":null},"collectors":[]})}
}

// Replace vendor content uniqueness with source identity. Historical local rows retain
// their original dedup rule; reboot-identical uploaded records remain distinct.
fn projection_schema(c:&mut Connection)->Result<()> {
    let tx=c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='events')",[],|r|r.get(0))?;
    let migrated:bool=if exists {let mut s=tx.prepare("PRAGMA table_info(events)")?;let names=s.query_map([],|r|r.get::<_,String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;names.iter().any(|n|n=="ingest_raw_id")}else{false};
    if !migrated {
        if exists {tx.execute_batch("ALTER TABLE events RENAME TO ingest_old_events;")?;}
        tx.execute_batch("CREATE TABLE events(id INTEGER PRIMARY KEY AUTOINCREMENT,serial TEXT NOT NULL,tag INTEGER NOT NULL,name TEXT NOT NULL,ring_timestamp INTEGER NOT NULL,body BLOB NOT NULL,decoded_json TEXT,captured_unix INTEGER NOT NULL,ingest_raw_id INTEGER UNIQUE,source_device TEXT,collector_epoch TEXT,source_row_id TEXT,clock_anchor_json TEXT,transport_json TEXT);")?;
        if exists {tx.execute_batch("INSERT INTO events(id,serial,tag,name,ring_timestamp,body,decoded_json,captured_unix) SELECT id,serial,tag,name,ring_timestamp,body,decoded_json,captured_unix FROM ingest_old_events; DROP TABLE ingest_old_events;")?;}
        tx.execute_batch("CREATE UNIQUE INDEX idx_events_legacy_identity ON events(serial,tag,ring_timestamp,body) WHERE ingest_raw_id IS NULL;")?;
    }
    tx.execute_batch("CREATE INDEX IF NOT EXISTS idx_events_capture ON events(captured_unix,id); CREATE INDEX IF NOT EXISTS idx_events_serial_tag ON events(serial,tag); CREATE INDEX IF NOT EXISTS idx_events_tag_time ON events(tag,ring_timestamp); CREATE INDEX IF NOT EXISTS idx_events_decoded_tag ON events(tag) WHERE decoded_json IS NOT NULL;
CREATE TABLE IF NOT EXISTS store_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS device(serial TEXT PRIMARY KEY,hardware_id TEXT,firmware TEXT,api_version TEXT,mac TEXT,updated_unix INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS sync_state(serial TEXT PRIMARY KEY,next_cursor INTEGER NOT NULL,last_sync_unix INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS readings(id INTEGER PRIMARY KEY AUTOINCREMENT,serial TEXT NOT NULL,kind TEXT NOT NULL,value REAL NOT NULL,unit TEXT,captured_unix INTEGER NOT NULL);")?;
    tx.commit()?;Ok(())
}
fn project(c:&mut Connection, revision:i64)->Result<()> {
    projection_schema(c)?;
    let tx=c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Each new row runs the full decoder, irrespective of store decoder_version.
    let mut statement=tx.prepare("SELECT r.id,r.serial,r.device_id,r.collector_epoch,r.source_row_id,r.event_json FROM ingest_raw r LEFT JOIN events e ON e.ingest_raw_id=r.id WHERE r.revision<=?1 AND e.id IS NULL ORDER BY r.id")?;
    let rows=statement.query_map([revision],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?)))?;
    for row in rows {
        let (id,serial,device,epoch,source,encoded)=row?;
        let event:wire::RawEvent=serde_json::from_str(&encoded)?;
        let body=hex::decode(&event.body_hex)?;
        validate_transport(&event,&body)?;
        let decoded=oura_store::storage::decode_stored_event(event.tag as u8,&body,None);
        let anchor=event.clock_anchor.as_ref().map(serde_json::to_string).transpose()?;
        let transport=event.transport.as_ref().map(serde_json::to_string).transpose()?;
        tx.execute("INSERT INTO events(serial,tag,name,ring_timestamp,body,decoded_json,captured_unix,ingest_raw_id,source_device,collector_epoch,source_row_id,clock_anchor_json,transport_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",params![serial,event.tag,oura_protocol::events::event_name(event.tag as u8),event.ring_counter,body,decoded,event.captured_unix_ms/1000,id,device,epoch,source,anchor,transport])?;
    }
    drop(statement);
    // Device metadata remains independent of whether any physiological tag decoded.
    let mut metadata=tx.prepare("SELECT body FROM ingest_batches WHERE revision<=?1 ORDER BY revision")?;
    let bodies=metadata.query_map([revision],|r|r.get::<_,Vec<u8>>(0))?;
    for bytes in bodies { let batch:Batch=serde_json::from_slice(&bytes?)?;tx.execute("INSERT INTO device(serial,hardware_id,firmware,updated_unix) VALUES(?1,?2,?3,?4) ON CONFLICT(serial) DO UPDATE SET hardware_id=excluded.hardware_id,firmware=excluded.firmware,updated_unix=excluded.updated_unix",params![batch.device.serial.as_deref().unwrap_or(&batch.device_id),batch.device.hardware,batch.device.firmware,now_ms()/1000])?; }
    drop(metadata);
    tx.execute("UPDATE ingest_processing SET decoded_revision=MAX(decoded_revision,?1) WHERE id=1",[revision])?;
    tx.execute("UPDATE ingest_jobs SET stage='analysis' WHERE state='processing' AND revision<=?1",[revision])?;
    tx.commit()?;Ok(())
}

fn validate_transport(event:&wire::RawEvent,body:&[u8])->Result<()> {
    let Some(t)=&event.transport else{return Ok(())};
    let raw=hex::decode(&t.raw_hex)?;
    if t.encoding==wire::TRANSPORT_LEGACY_EVENT_PACKET {
        if raw.len()<6 || raw[0] as u32!=event.tag || raw[1] as usize!=raw.len()-2 || u32::from_le_bytes(raw[2..6].try_into()?) as u64!=t.counter_value || raw[6..]!=*body {return Err(anyhow!("transport_mismatch"));}
    } else {
        if raw.len()<4 || raw[1] as u32!=event.tag {return Err(anyhow!("transport_mismatch"));}
        let mut value=0u64;let mut end=None;
        for (i,b) in raw[3..].iter().copied().enumerate().take(10) {
            if i==9 && b>1 {return Err(anyhow!("transport_counter_overflow"));}
            value|=u64::from(b&127)<<(7*i);
            if b&128==0 {end=Some(i+4);break;}
        }
        let end=end.ok_or_else(||anyhow!("transport_counter_invalid"))?;
        if value!=t.counter_value || raw[end..]!=*body {return Err(anyhow!("transport_mismatch"));}
        // Delta counters cannot be validated as absolute without predecessor context.
        // Preserve them raw, but never project a collector-synthesized timestamp.
        if t.counter_unit==wire::UNIT_MILLISECOND_DELTA {return Err(anyhow!("delta_counter_requires_context"));}
    }
    Ok(())
}

/// Claim/restart pending work under a crash-released process lock. No SQLite lock
/// survives into `build`; errors retain last good summary and retry with backoff.
/// Returns true only when a durable job was attempted.
pub fn process_pending<F>(db:&Path,build:F)->Result<bool> where F:FnOnce(&Path)->Result<Value> {
    if !db.exists(){return Ok(false);}
    let lock_path=db.with_extension("ingest-worker.lock");
    let mut options=OpenOptions::new();options.write(true).create(true).truncate(false);
    #[cfg(unix)] {use std::os::unix::fs::OpenOptionsExt;options.mode(0o600);}
    let lock=options.open(lock_path)?;
    match lock.try_lock_exclusive() {Ok(())=>{},Err(e) if e.kind()==std::io::ErrorKind::WouldBlock=>return Ok(false),Err(e)=>return Err(e.into())}
    let mut c=open(db)?;
    let revision={
        let tx=c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Holding the OS lock proves an old processing owner no longer exists.
        tx.execute("UPDATE ingest_jobs SET state='pending' WHERE state='processing'",[])?;
        let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM ingest_jobs WHERE state='failed' AND next_attempt_unix>?1)",[now_ms()/1000],|r|r.get(0))?;
        if blocked {return Ok(false);}
        let target:Option<i64>=tx.query_row("SELECT MAX(revision) FROM ingest_jobs WHERE state IN ('pending','failed')",[],|r|r.get(0))?;
        let Some(target)=target else {return Ok(false)};
        tx.execute("UPDATE ingest_jobs SET state='processing',attempts=attempts+1,started_unix_ms=?1 WHERE state IN ('pending','failed') AND revision<=?2",params![now_ms(),target])?;
        tx.commit()?;target
    };
    let projected=project(&mut c,revision);
    // All DB statements/connections drop before arbitrary analysis/model work.
    drop(c);
    let (result,stage)=match projected {Ok(())=>(build(db),"analysis"),Err(error)=>(Err(error),"decode")};
    let mut c=open(db)?;
    let tx=c.transaction_with_behavior(TransactionBehavior::Immediate)?;
    match result {
        Ok(summary)=>{
            tx.execute("UPDATE ingest_processing SET completed_revision=?1,summary=?2,summary_unix_ms=?3 WHERE id=1",params![revision,serde_json::to_string(&summary)?,now_ms()])?;
            tx.execute("UPDATE ingest_jobs SET state='done',stage='complete',completed_unix_ms=?1 WHERE state='processing' AND revision<=?2",params![now_ms(),revision])?;
            tx.commit()?;Ok(true)
        },
        Err(error)=>{
            let code=if stage=="decode" {"decode_failed"}else{"analysis_failed"};
            tx.execute("INSERT INTO ingest_failures(revision,failed_unix_ms,stage,code) VALUES(?1,?2,?3,?4)",params![revision,now_ms(),stage,code])?;
            tx.execute("UPDATE ingest_jobs SET state='failed',stage=?1,last_error=?2,next_attempt_unix=?3+MIN(300,5*(1 << MIN(attempts,6))) WHERE state='processing' AND revision<=?4",params![stage,code,now_ms()/1000,revision])?;
            tx.commit()?;Err(error.context(code))
        }
    }
}
