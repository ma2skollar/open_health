//! HTTP framing for collector ingestion. Metadata is one `X-Oura-Metadata`
//! JSON header; the body contains only SQLite bytes. Authentication precedes
//! metadata decoding and all filesystem work.

use crate::control::ControlEvent;
use crate::{Hub, HubError};
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Request, State};
use axum::http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use oura_protocol_v1::{CommandName, ControlMessage, UploadAck, UploadMetadata, PROTOCOL_VERSION};
use oura_protocol_v1::ErrorEnvelope;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinError;
use tokio::time::{timeout_at, Instant as AsyncInstant};

const METADATA_HEADER: &str = "x-oura-metadata";
const MAX_METADATA_BYTES: usize = 16 * 1024;
const STREAM_CHUNK_BYTES: usize = 64 * 1024;
const BUFFERED_CHUNKS: usize = 2;

#[derive(Clone)]
struct HttpState {
    hub: Hub,
    request_timeout: Duration,
}

/// Build the loopback service's routes. Pass the same timeout used by `Config`.
///
/// One absolute deadline covers metadata, streaming, and structural validation.
/// The core checks that deadline before commit starts. Once commit starts, this
/// handler waits for durable completion instead of returning a misleading
/// timeout while an active-generation pointer is still being written.
pub fn router(hub: Hub, request_timeout: Duration) -> Router {
    Router::new()
        .route("/api/v1/collector/snapshot", post(snapshot))
        .route("/api/v1/collector/control", get(control_ws))
        .route("/api/v1/control/{name}", post(control_command))
        .route("/api/v1/control/events", get(control_events))
        .route("/api/v1/hub/status", get(status))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(HttpState {
            hub,
            request_timeout,
        })
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let (scheme, token) = value.to_str().ok()?.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer")
        || token.is_empty()
        || token.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return None;
    }
    Some(token)
}

fn metadata(headers: &HeaderMap) -> Result<UploadMetadata, HubError> {
    let mut values = headers.get_all(METADATA_HEADER).iter();
    let value = values
        .next()
        .ok_or_else(|| HubError::new("invalid_metadata", "X-Oura-Metadata header is required"))?;
    if values.next().is_some() || value.as_bytes().len() > MAX_METADATA_BYTES {
        return Err(HubError::new(
            "invalid_metadata",
            "exactly one X-Oura-Metadata header of at most 16384 bytes is required",
        ));
    }
    serde_json::from_slice(value.as_bytes()).map_err(|_| {
        HubError::new(
            "invalid_metadata",
            "X-Oura-Metadata must contain valid upload metadata JSON",
        )
    })
}

fn request_deadline(timeout: Duration) -> Result<Instant, HubError> {
    Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| HubError::new("storage_error", "request timeout configuration is invalid"))
}

fn check_deadline(deadline: Instant) -> Result<(), HubError> {
    if Instant::now() >= deadline {
        Err(request_timeout())
    } else {
        Ok(())
    }
}

fn request_timeout() -> HubError {
    HubError::new("request_timeout", "request deadline exceeded")
}

fn interrupted() -> HubError {
    HubError::new("upload_interrupted", "snapshot body did not complete")
}

fn error_response(error: HubError) -> Response {
    let status =
        StatusCode::from_u16(error.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    envelope_response(status, error.code, error.message)
}

fn envelope_response(status: StatusCode, code: String, message: String) -> Response {
    (
        status,
        Json(ErrorEnvelope {
            protocol_version: PROTOCOL_VERSION,
            code,
            message,
        }),
    )
        .into_response()
}

enum UploadChunk {
    Data(Bytes),
    Finish,
}

async fn stream_body(
    mut body: Body,
    sender: mpsc::Sender<UploadChunk>,
    deadline: AsyncInstant,
) -> Result<(), HubError> {
    loop {
        let frame = timeout_at(deadline, body.frame())
            .await
            .map_err(|_| request_timeout())?;
        match frame {
            Some(Ok(frame)) => {
                if let Ok(mut data) = frame.into_data() {
                    while !data.is_empty() {
                        let chunk = data.split_to(data.len().min(STREAM_CHUNK_BYTES));
                        timeout_at(deadline, sender.send(UploadChunk::Data(chunk)))
                            .await
                            .map_err(|_| request_timeout())?
                            .map_err(|_| interrupted())?;
                    }
                }
            }
            Some(Err(_)) => {
                return Err(HubError::new(
                    "invalid_body",
                    "snapshot body could not be read",
                ));
            }
            None => {
                return timeout_at(deadline, sender.send(UploadChunk::Finish))
                    .await
                    .map_err(|_| request_timeout())?
                    .map_err(|_| interrupted());
            }
        }
    }
}

fn receive_upload(
    hub: &Hub,
    metadata: &UploadMetadata,
    mut receiver: mpsc::Receiver<UploadChunk>,
    deadline: Instant,
) -> Result<UploadAck, HubError> {
    check_deadline(deadline)?;
    hub.check_request(metadata)?;
    let mut staged = hub.stage_before(metadata, deadline)?;
    loop {
        let chunk = receiver.blocking_recv();
        check_deadline(deadline)?;
        match chunk {
            Some(UploadChunk::Data(bytes)) => staged.write_chunk(&bytes)?,
            Some(UploadChunk::Finish) => {
                let validated = staged.finish_before(deadline)?;
                return hub.commit_before(validated, deadline);
            }
            None => return Err(interrupted()),
        }
    }
}

fn worker_result(
    result: Result<Result<UploadAck, HubError>, JoinError>,
) -> Result<UploadAck, HubError> {
    result.unwrap_or_else(|_| Err(HubError::new("storage_error", "snapshot processing failed")))
}

async fn snapshot(State(state): State<HttpState>, request: Request) -> Response {
    let deadline = match request_deadline(state.request_timeout) {
        Ok(deadline) => deadline,
        Err(error) => return error_response(error),
    };
    if let Err(error) = state.hub.authenticate(bearer_token(request.headers())) {
        return error_response(error);
    }
    let metadata = match metadata(request.headers()) {
        Ok(metadata) => metadata,
        Err(error) => return error_response(error),
    };
    let (sender, receiver) = mpsc::channel(BUFFERED_CHUNKS);
    let mut worker = tokio::task::spawn_blocking(move || {
        let result = receive_upload(&state.hub, &metadata, receiver, deadline);
        if let Err(error) = &result {
            state.hub.record_error(&metadata.collector_id, &error.code);
        }
        result
    });
    let producer = stream_body(
        request.into_body(),
        sender,
        AsyncInstant::from_std(deadline),
    );
    tokio::pin!(producer);
    let result = tokio::select! {
        result = &mut worker => worker_result(result),
        produced = &mut producer => {
            // Dropping the producer's sender wakes the blocking receiver on
            // failure. Always join: validation and commit must not be detached
            // behind a timeout response.
            let result = worker_result(worker.await);
            match (produced, result) {
                (Err(body_error), Err(error)) if error.code == "upload_interrupted" => Err(body_error),
                (_, result) => result,
            }
        }
    };
    match result {
        Ok(ack) => Json(ack).into_response(),
        Err(error) => error_response(error),
    }
}

async fn status(State(state): State<HttpState>, request: Request) -> Response {
    let deadline = match request_deadline(state.request_timeout) {
        Ok(deadline) => deadline,
        Err(error) => return error_response(error),
    };
    if let Err(error) = state.hub.authenticate(bearer_token(request.headers())) {
        return error_response(error);
    }
    let worker = tokio::task::spawn_blocking(move || {
        check_deadline(deadline)?;
        let status = state.hub.status()?;
        check_deadline(deadline)?;
        Ok::<_, HubError>(status)
    });
    match timeout_at(AsyncInstant::from_std(deadline), worker).await {
        Ok(Ok(Ok(status))) => Json(status).into_response(),
        Ok(Ok(Err(error))) => error_response(error),
        Ok(Err(_)) => error_response(HubError::new(
            "storage_error",
            "hub status could not be read",
        )),
        Err(_) => error_response(request_timeout()),
    }
}

/// Pi-initiated control channel. The collector dials out to the hub; the hub
/// never dials the Pi. The hub sends queued commands and records receipts and
/// results. The collector sends `Hello` then `Status` first.
async fn control_ws(
    ws: WebSocketUpgrade,
    State(state): State<HttpState>,
    request: Request,
) -> Response {
    if let Err(error) = state.hub.authenticate(bearer_token(request.headers())) {
        return error_response(error);
    }
    ws.on_upgrade(move |socket| handle_control(socket, state))
}

async fn handle_control(socket: WebSocket, state: HttpState) {
    let (mut sender, mut receiver) = socket.split();
    let hello = match receiver.next().await {
        Some(Ok(Message::Text(text))) => serde_json::from_str::<ControlMessage>(&text).ok(),
        _ => return,
    };
    match hello {
        Some(ControlMessage::Hello {
            protocol_version,
            collector_id,
            ..
        }) => {
            if protocol_version != PROTOCOL_VERSION || !crate::valid_collector(&collector_id) {
                let _ = sender.send(Message::Close(None)).await;
                return;
            }
        }
        _ => {
            let _ = sender.send(Message::Close(None)).await;
            return;
        }
    }
    let status = match receiver.next().await {
        Some(Ok(Message::Text(text))) => serde_json::from_str::<ControlMessage>(&text).ok(),
        _ => return,
    };
    if !matches!(status, Some(ControlMessage::Status { .. })) {
        let _ = sender.send(Message::Close(None)).await;
        return;
    }
    loop {
        let notified = state.hub.control_notify().notified();
        tokio::pin!(notified);
        let now = Hub::now();
        let command = match state.hub.control().lock() {
            Ok(mut queue) => queue.next_valid(now),
            Err(_) => return,
        };
        if let Some(command) = command {
            let text = match serde_json::to_string(&command.command_frame()) {
                Ok(text) => text,
                Err(_) => return,
            };
            if sender.send(Message::Text(text.into())).await.is_err() {
                return;
            }
            continue;
        }
        tokio::select! {
            _ = &mut notified => {}
            frame = receiver.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ControlMessage>(&text) {
                            Ok(ControlMessage::Hello { .. }) => {}
                            Ok(ControlMessage::Status { .. }) => {}
                            Ok(ControlMessage::Receipt {
                                id,
                                state: receipt_state,
                                reason,
                                ..
                            }) => {
                                state.hub.record_control_receipt(&id, receipt_state, reason);
                            }
                            Ok(ControlMessage::CommandResult {
                                id,
                                state: result_state,
                                detail,
                                ..
                            }) => {
                                state.hub.record_control_result(&id, result_state, detail);
                            }
                            Ok(message @ ControlMessage::Stream { .. }) => {
                                // Bounded live-HR sample from the collector.
                                // Forward to dashboard subscribers; the hub does
                                // not persist it. The collector owns the cap.
                                state.hub.broadcast_stream(message);
                            }
                            Ok(ControlMessage::Command { .. }) => {}
                            Err(_) => return,
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = sender.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(_)) | Some(Err(_)) => return,
                }
            }
        }
    }
}

/// Queue an allowlisted command for the connected collector. The name is a path
/// segment and must match the frozen allowlist; arbitrary names are rejected.
async fn control_command(
    State(state): State<HttpState>,
    Path(name): Path<String>,
    request: Request,
) -> Response {
    if let Err(error) = state.hub.authenticate(bearer_token(request.headers())) {
        return error_response(error);
    }
    let command_name = match serde_json::from_str::<CommandName>(&format!("\"{name}\"")) {
        Ok(name) => name,
        Err(_) => {
            return error_response(HubError::new("invalid_command", "unknown command name"))
        }
    };
    let body = match request.into_body().collect().await {
        Ok(body) => body.to_bytes(),
        Err(_) => {
            return error_response(HubError::new(
                "invalid_body",
                "request body could not be read",
            ))
        }
    };
    let args = if body.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_slice::<serde_json::Value>(&body).unwrap_or(serde_json::json!({}))
    };
    match state
        .hub
        .enqueue_command(command_name, args, Duration::from_secs(300))
    {
        Ok(command) => Json(command).into_response(),
        Err(error) => error_response(HubError::new("control_error", &error)),
    }
}

/// Server-sent events for command lifecycle. Each event is the full queue
/// snapshot after a change. The stream ends when the broadcast channel closes.
async fn control_events(State(state): State<HttpState>, request: Request) -> Response {
    if let Err(error) = state.hub.authenticate(bearer_token(request.headers())) {
        return error_response(error);
    }
    let receiver = state.hub.control_broadcast().subscribe();
    // Emit the current queue first, then every subsequent event. A live sample
    // arrives as `ControlEvent::Stream`; a lifecycle change arrives as
    // `ControlEvent::Queue`.
    let initial = ControlEvent::Queue(state.hub.control_snapshot());
    let stream = futures_util::stream::unfold(
        (state, receiver, Some(initial)),
        |(state, mut receiver, pending)| async move {
            let event = match pending {
                Some(event) => event,
                None => match receiver.recv().await {
                    Ok(event) => event,
                    Err(_) => return None,
                },
            };
            let data = Event::default().json_data(event).ok()?;
            Some((
                Ok::<Event, std::convert::Infallible>(data),
                (state, receiver, None),
            ))
        },
    );
    Sse::new(stream).into_response()
}

async fn not_found() -> Response {
    envelope_response(
        StatusCode::NOT_FOUND,
        "not_found".into(),
        "route not found".into(),
    )
}

async fn method_not_allowed() -> Response {
    envelope_response(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed".into(),
        "method not allowed".into(),
    )
}
