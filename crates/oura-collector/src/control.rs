//! Outbound control uses the frozen v1 messages, never a second wire schema.
//!
//! The journal retains at most 256 results for 24 hours. Duplicate suppression is
//! bounded, not exactly-once physical execution. A crash after durable admission
//! is reported as uncertain and never automatically replayed against the ring.
use std::{future::Future, path::Path, pin::Pin, sync::Arc, time::Duration};

use futures_util::{stream::FuturesUnordered, SinkExt, StreamExt};
use oura_ble::RingError;
use oura_protocol_v1::{CollectorState, CommandName, ControlMessage, Identity, ReceiptState, ResultState, PROTOCOL_VERSION};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use tokio_tungstenite::{tungstenite::{client::IntoClientRequest, http::{header::AUTHORIZATION, HeaderValue}, protocol::WebSocketConfig, Message}, MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

use crate::{clock::Clock, config::Config, coordinator::{CollectorStatus, CommandOutcome, CoordinatorHandle, LocalCommand}, error::CollectorError};

const RESULT_LIMIT: i64 = 256;
const RESULT_RETENTION_SECS: i64 = 24 * 60 * 60;
const NETWORK_TIMEOUT: Duration = Duration::from_secs(10);
type Completion = Pin<Box<dyn Future<Output = (String, Result<CommandOutcome, CollectorError>)> + Send>>;
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Journal {
    connection: Connection,
    scope: String,
}

impl Journal {
    fn open(config: &Config, now: i64) -> Result<Self, CollectorError> {
        config.prepare_state()?;
        let path = config.state_dir.join("control-results.sqlite");
        crate::config::reject_symlinks(&path)?;
        let connection = Connection::open(&path)?;
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS command_results (
                scope TEXT NOT NULL, id TEXT NOT NULL, running INTEGER NOT NULL,
                receipt TEXT NOT NULL, result TEXT, updated_unix INTEGER NOT NULL,
                PRIMARY KEY(scope,id));")?;
        let mut journal = Self { connection, scope: format!("{}\n{}", config.collector_id, config.epoch) };
        let interrupted = {
            let mut statement = journal.connection.prepare("SELECT id FROM command_results WHERE scope=?1 AND running=1")?;
            let rows = statement.query_map([&journal.scope], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for id in interrupted {
            journal.finish(&id, result(&id, ResultState::Failed, json!({"code":"interrupted", "uncertain":true, "automatic_retry":false})), now)?;
        }
        journal.prune(now)?;
        Ok(journal)
    }

    fn prune(&self, now: i64) -> Result<(), CollectorError> {
        self.connection.execute("DELETE FROM command_results WHERE running=0 AND updated_unix<?1", [now.saturating_sub(RESULT_RETENTION_SECS)])?;
        Ok(())
    }

    fn get(&self, id: &str) -> Result<Option<(ControlMessage, Option<ControlMessage>)>, CollectorError> {
        let row = self.connection.query_row("SELECT receipt,result FROM command_results WHERE scope=?1 AND id=?2", params![self.scope, id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))).optional()?;
        row.map(|(receipt, result)| Ok((serde_json::from_str(&receipt)?, result.map(|value| serde_json::from_str(&value)).transpose()?))).transpose()
    }

    fn begin(&mut self, id: &str, now: i64) -> Result<(), CollectorError> {
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM command_results WHERE running=0 AND updated_unix<?1", [now.saturating_sub(RESULT_RETENTION_SECS)])?;
        let count: i64 = transaction.query_row("SELECT COUNT(*) FROM command_results", [], |row| row.get(0))?;
        if count >= RESULT_LIMIT {
            transaction.execute("DELETE FROM command_results WHERE rowid IN (SELECT rowid FROM command_results WHERE running=0 ORDER BY updated_unix,rowid LIMIT ?1)", [count - RESULT_LIMIT + 1])?;
        }
        let count: i64 = transaction.query_row("SELECT COUNT(*) FROM command_results", [], |row| row.get(0))?;
        if count >= RESULT_LIMIT { return Err(CollectorError::Busy); }
        let receipt = serde_json::to_string(&receipt(id, ReceiptState::Accepted, None))?;
        transaction.execute("INSERT INTO command_results(scope,id,running,receipt,result,updated_unix) VALUES(?1,?2,1,?3,NULL,?4)", params![self.scope, id, receipt, now])?;
        transaction.commit()?;
        Ok(())
    }

    fn reject(&mut self, id: &str, reason: &str, terminal: ControlMessage, now: i64) -> Result<Vec<ControlMessage>, CollectorError> {
        let receipt = receipt(id, ReceiptState::Rejected, Some(reason.to_owned()));
        self.connection.execute("UPDATE command_results SET receipt=?3 WHERE scope=?1 AND id=?2", params![self.scope, id, serde_json::to_string(&receipt)?])?;
        self.finish(id, terminal.clone(), now)?;
        Ok(vec![receipt, terminal])
    }

    fn finish(&mut self, id: &str, result: ControlMessage, now: i64) -> Result<ControlMessage, CollectorError> {
        self.connection.execute("UPDATE command_results SET running=0,result=?3,updated_unix=?4 WHERE scope=?1 AND id=?2", params![self.scope, id, serde_json::to_string(&result)?, now])?;
        Ok(result)
    }

    fn results(&self, now: i64) -> Result<Vec<ControlMessage>, CollectorError> {
        let mut statement = self.connection.prepare("SELECT result FROM command_results WHERE scope=?1 AND running=0 AND updated_unix>=?2 ORDER BY updated_unix,rowid")?;
        let rows = statement.query_map(params![self.scope, now.saturating_sub(RESULT_RETENTION_SECS)], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }
}

pub struct ControlSession {
    config: Config,
    clock: Arc<dyn Clock>,
    handle: CoordinatorHandle,
    journal: Journal,
    pending: FuturesUnordered<Completion>,
}

impl ControlSession {
    pub fn open(config: &Config, clock: Arc<dyn Clock>, handle: CoordinatorHandle) -> Result<Self, CollectorError> {
        let journal = Journal::open(config, clock.now_unix())?;
        Ok(Self { config: config.clone(), clock, handle, journal, pending: FuturesUnordered::new() })
    }

    pub fn hello(&self) -> ControlMessage {
        ControlMessage::Hello { protocol_version: PROTOCOL_VERSION, collector_id: self.config.collector_id.clone(), build_revision: self.config.build_revision.clone() }
    }

    pub fn status(&self) -> ControlMessage {
        ControlMessage::Status { protocol_version: PROTOCOL_VERSION, collector_state: wire_status(self.handle.status()) }
    }

    pub fn connected(&self) { self.handle.set_control_reachable(true); }
    pub fn disconnect(&self) { self.handle.set_control_reachable(false); }
    pub fn replay_results(&self) -> Result<Vec<ControlMessage>, CollectorError> { self.journal.results(self.clock.now_unix()) }

    /// Returns admission frames only. Ring completion is returned by `next_result`.
    pub async fn receive(&mut self, message: ControlMessage) -> Result<Vec<ControlMessage>, CollectorError> {
        let ControlMessage::Command { protocol_version, id, name, args, created_unix, expires_unix } = message else {
            return Err(CollectorError::Protocol("only command frames are accepted from hub".into()));
        };
        if protocol_version != PROTOCOL_VERSION {
            return Err(CollectorError::Protocol("unsupported control protocol version".into()));
        }
        if id.is_empty() || id.len() > 128 {
            return Err(CollectorError::Protocol("command id must contain 1..128 bytes".into()));
        }
        let now = self.clock.now_unix();
        self.journal.prune(now)?;
        if let Some((receipt, result)) = self.journal.get(&id)? {
            let mut frames = vec![receipt];
            if let Some(result) = result { frames.push(result); }
            return Ok(frames);
        }
        self.journal.begin(&id, now)?;
        if now >= expires_unix {
            return self.journal.reject(&id, "expired", result(&id, ResultState::Expired, json!({"code":"expired"})), now);
        }
        if created_unix > now || expires_unix <= created_unix {
            return self.journal.reject(&id, "invalid command time bounds", result(&id, ResultState::Failed, json!({"code":"invalid_time_bounds"})), now);
        }
        let command = match local_command(name, args) {
            Ok(command) => command,
            Err(reason) => return self.journal.reject(&id, reason, result(&id, ResultState::Failed, json!({"code":"invalid_command"})), now),
        };
        match self.handle.submit_until(command, expires_unix, self.clock.clone()) {
            Ok(ticket) => {
                let completion_id = id.clone();
                self.pending.push(Box::pin(async move { (completion_id, ticket.wait().await) }));
                Ok(vec![receipt(&id, ReceiptState::Accepted, None)])
            }
            Err(error) => {
                let (state, detail) = error_result(&error);
                let reason = if matches!(error, CollectorError::Busy) { "busy" } else { "command not admitted" };
                self.journal.reject(&id, reason, result(&id, state, detail), now)
            }
        }
    }

    /// Cancellation-safe: a dropped wait keeps the command ticket in this session.
    pub async fn next_result(&mut self) -> Result<ControlMessage, CollectorError> {
        let (id, outcome) = match self.pending.next().await {
            Some(completion) => completion,
            None => std::future::pending().await,
        };
        let terminal = match outcome {
            Ok(outcome) => result(&id, ResultState::Completed, outcome.detail),
            Err(error) => { let (state, detail) = error_result(&error); result(&id, state, detail) }
        };
        self.journal.finish(&id, terminal, self.clock.now_unix())
    }
}

fn local_command(name: CommandName, args: Value) -> Result<LocalCommand, &'static str> {
    match name {
        CommandName::SyncNow => Ok(LocalCommand::SyncNow),
        CommandName::RefreshDeviceInfo => Ok(LocalCommand::RefreshDeviceInfo),
        CommandName::RefreshBattery => Ok(LocalCommand::RefreshBattery),
        CommandName::CollectorStatus => Ok(LocalCommand::CollectorStatus),
        CommandName::StartLiveHr => args.get("max_secs").and_then(Value::as_u64).filter(|seconds| *seconds > 0).map(|max_secs| LocalCommand::StartLiveHr { max_secs }).ok_or("max_secs must be a positive integer"),
        CommandName::StopLiveHr => Ok(LocalCommand::StopLiveHr),
        CommandName::InspectSupportedFeatures | CommandName::SetFeatureMode => Err("command is not enabled on this collector"),
    }
}

fn receipt(id: &str, state: ReceiptState, reason: Option<String>) -> ControlMessage {
    ControlMessage::Receipt { protocol_version: PROTOCOL_VERSION, id: id.to_owned(), state, reason }
}

fn result(id: &str, state: ResultState, detail: Value) -> ControlMessage {
    ControlMessage::CommandResult { protocol_version: PROTOCOL_VERSION, id: id.to_owned(), state, detail }
}

fn error_result(error: &CollectorError) -> (ResultState, Value) {
    match error {
        CollectorError::Cancelled | CollectorError::Ring(RingError::Cancelled) => (ResultState::Cancelled, json!({"code":"cancelled"})),
        CollectorError::Ring(RingError::Expired) => (ResultState::Expired, json!({"code":"expired"})),
        CollectorError::Ring(RingError::PartialHistory) => (ResultState::Failed, json!({"code":"partial_history","partial":true,"durable_progress":true})),
        CollectorError::Ring(RingError::Timeout { operation }) => (ResultState::Failed, json!({"code":"timeout","operation":operation})),
        CollectorError::Busy => (ResultState::Failed, json!({"code":"busy"})),
        CollectorError::OutboxFull => (ResultState::Failed, json!({"code":"outbox_full","pending_snapshot":true})),
        CollectorError::LowDisk => (ResultState::Failed, json!({"code":"low_disk","pending_snapshot":true})),
        _ => (ResultState::Failed, json!({"code":"operation_failed"})),
    }
}

fn wire_status(status: CollectorStatus) -> CollectorState {
    let unknown = [status.pi_reachable, status.adapter_present, status.ring_recently_seen, status.ring_connected, status.operation_running, status.ring_unreachable].iter().any(Option::is_none);
    CollectorState {
        pi_reachable: status.pi_reachable.unwrap_or(false),
        adapter_present: status.adapter_present.unwrap_or(false),
        ring_recently_seen: status.ring_recently_seen.unwrap_or(false),
        ring_connected: status.ring_connected.unwrap_or(false),
        operation_running: status.operation_running.unwrap_or(false),
        ring_unreachable: status.ring_unreachable.unwrap_or(false),
        unknown,
    }
}

/// Run on a network task separate from `Coordinator::spawn` and its scheduler.
/// Stopping this task cancels live HR, but does not stop normal collection.
pub async fn run_control(config: Config, clock: Arc<dyn Clock>, handle: CoordinatorHandle, shutdown: CancellationToken) -> Result<(), CollectorError> {
    let mut session = ControlSession::open(&config, clock, handle)?;
    let mut delay = config.retry_min_secs.max(1);
    loop {
        let connection = tokio::select! {
            biased;
            _ = shutdown.cancelled() => { session.disconnect(); return Ok(()); },
            connected = connect(&config) => connected,
            completed = session.next_result() => { completed?; continue; },
        };
        match connection {
            Ok(mut socket) => {
                session.connected();
                let serving = serve(&mut session, &mut socket, &shutdown).await;
                session.disconnect();
                let _ = tokio::time::timeout(NETWORK_TIMEOUT, socket.close(None)).await;
                if shutdown.is_cancelled() { return Ok(()); }
                // Transport failures reconnect. A journal failure must not be hidden.
                if let Err(error @ (CollectorError::Sqlite(_) | CollectorError::Io(_) | CollectorError::Json(_))) = serving { return Err(error); }
                delay = config.retry_min_secs.max(1);
            }
            Err(CollectorError::Config(message)) => return Err(CollectorError::Config(message)),
            Err(_) => session.disconnect(),
        }
        let sleep = tokio::time::sleep(Duration::from_secs(delay));
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => { session.disconnect(); return Ok(()); },
                _ = &mut sleep => break,
                completed = session.next_result() => { completed?; },
            }
        }
        delay = delay.saturating_mul(2).min(config.retry_max_secs.max(1));
    }
}

async fn connect(config: &Config) -> Result<Socket, CollectorError> {
    let mut url = url::Url::parse(&config.control_url()?).map_err(|_| CollectorError::Config("invalid hub URL".into()))?;
    url.set_query(None);
    url.set_fragment(None);
    if !url.username().is_empty() || url.password().is_some() {
        return Err(CollectorError::Config("hub URL must not contain credentials".into()));
    }
    let loopback = match url.host() {
        Some(url::Host::Domain(host)) => host == "localhost",
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if url.scheme() != "wss" && !(config.identity == Identity::Demo && loopback) {
        return Err(CollectorError::Config("collector control requires WSS outside loopback demo".into()));
    }
    check_token_file(&config.token_file)?;
    let token = tokio::fs::read_to_string(&config.token_file).await?;
    let token = token.trim();
    if token.is_empty() { return Err(CollectorError::Config("collector credential is empty".into())); }
    let mut request = url.as_str().into_client_request().map_err(|_| CollectorError::Protocol("invalid control request".into()))?;
    let mut authorization = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| CollectorError::Config("invalid collector credential".into()))?;
    authorization.set_sensitive(true);
    request.headers_mut().insert(AUTHORIZATION, authorization);
    let mut limits = WebSocketConfig::default();
    limits.max_message_size = Some(64 * 1024);
    limits.max_frame_size = Some(64 * 1024);
    let connected = tokio::time::timeout(NETWORK_TIMEOUT, tokio_tungstenite::connect_async_with_config(request, Some(limits), false)).await;
    // Never format a request/handshake error: it may contain Authorization.
    connected.map_err(|_| CollectorError::Protocol("control connection timed out".into()))?.map(|(socket, _)| socket).map_err(|_| CollectorError::Protocol("control connection failed".into()))
}

fn check_token_file(path: &Path) -> Result<(), CollectorError> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > 4096 { return Err(CollectorError::Config("collector credential must be a small regular file".into())); }
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(CollectorError::Config("collector credential must have mode 0600".into()));
        }
    }
    Ok(())
}

async fn serve(session: &mut ControlSession, socket: &mut Socket, shutdown: &CancellationToken) -> Result<(), CollectorError> {
    send(socket, session.hello()).await?;
    send(socket, session.status()).await?;
    for result in session.replay_results()? { send(socket, result).await?; }
    let mut status_interval = tokio::time::interval(Duration::from_secs(15));
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Ok(()),
            frame = socket.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    let command = serde_json::from_str::<ControlMessage>(&text).map_err(|_| CollectorError::Protocol("invalid control message".into()))?;
                    for frame in session.receive(command).await? { send(socket, frame).await?; }
                }
                Some(Ok(Message::Ping(bytes))) => {
                    tokio::time::timeout(NETWORK_TIMEOUT, socket.send(Message::Pong(bytes))).await.map_err(|_| CollectorError::Protocol("control pong timed out".into()))?.map_err(|_| CollectorError::Protocol("control pong failed".into()))?;
                }
                Some(Ok(Message::Pong(_))) => {},
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(Ok(_)) | Some(Err(_)) => return Err(CollectorError::Protocol("control connection closed".into())),
            },
            terminal = session.next_result() => send(socket, terminal?).await?,
            _ = status_interval.tick() => send(socket, session.status()).await?,
        }
    }
}

async fn send(socket: &mut Socket, frame: ControlMessage) -> Result<(), CollectorError> {
    let text = serde_json::to_string(&frame)?;
    tokio::time::timeout(NETWORK_TIMEOUT, socket.send(Message::Text(text.into()))).await.map_err(|_| CollectorError::Protocol("control write timed out".into()))?.map_err(|_| CollectorError::Protocol("control write failed".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_refusal_result_codes_remain_distinct() {
        let (full_state, full) = error_result(&CollectorError::OutboxFull);
        let (disk_state, disk) = error_result(&CollectorError::LowDisk);
        assert_eq!(full_state, ResultState::Failed);
        assert_eq!(disk_state, ResultState::Failed);
        assert_eq!(full["code"], "outbox_full");
        assert_eq!(disk["code"], "low_disk");
        assert_eq!(full["pending_snapshot"], true);
        assert_eq!(disk["pending_snapshot"], true);
    }
}
