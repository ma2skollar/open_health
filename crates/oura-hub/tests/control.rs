//! End-to-end control channel: a real WebSocket client drives the hub's Pi-
//! initiated control server. Proves Hello/Status admission, command delivery,
//! receipt and result recording, and that a rejected command becomes terminal.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use oura_hub::control::CommandState;
use oura_hub::{Config, Hub};
use oura_protocol_v1::{
    CollectorState, CommandName, ControlMessage, ReceiptState, ResultState, PROTOCOL_VERSION,
};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const TOKEN: &str = "synthetic-control-token-never-in-output";

type Socket = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

fn frame(message: &ControlMessage) -> Message {
    Message::Text(serde_json::to_string(message).unwrap().into())
}

async fn connect(addr: std::net::SocketAddr) -> Socket {
    let mut request = format!("ws://{addr}/api/v1/collector/control")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

async fn hello_and_status(socket: &mut Socket) {
    let hello = ControlMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        collector_id: "pi-01".into(),
        build_revision: "test-rev".into(),
    };
    socket.send(frame(&hello)).await.unwrap();
    let status = ControlMessage::Status {
        protocol_version: PROTOCOL_VERSION,
        collector_state: CollectorState::default(),
    };
    socket.send(frame(&status)).await.unwrap();
}

async fn start_hub() -> (TempDir, Hub, std::net::SocketAddr) {
    let temp = TempDir::new().unwrap();
    let config = Config::new(temp.path().to_path_buf());
    let hub = Hub::new(config, TOKEN.as_bytes().to_vec()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = oura_hub::http::router(hub.clone(), Duration::from_secs(30));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (temp, hub, addr)
}

#[tokio::test]
async fn control_channel_delivers_command_and_records_result() {
    let (_temp, hub, addr) = start_hub().await;

    let mut socket = connect(addr).await;
    hello_and_status(&mut socket).await;

    // The hub's handler needs a moment to admit Hello/Status before it starts
    // polling the queue. Yield so the accept and handshake complete.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let command = hub
        .enqueue_command(
            CommandName::SyncNow,
            serde_json::json!({}),
            Duration::from_secs(60),
        )
        .unwrap();
    assert_eq!(command.state, CommandState::Queued);

    let delivered = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("command must be delivered")
        .expect("socket stays open")
        .expect("valid frame");
    let message = match delivered {
        Message::Text(text) => serde_json::from_str::<ControlMessage>(&text).unwrap(),
        other => panic!("expected text command frame, got {other:?}"),
    };
    match message {
        ControlMessage::Command {
            protocol_version,
            id,
            name,
            ..
        } => {
            assert_eq!(protocol_version, PROTOCOL_VERSION);
            assert_eq!(id, command.id);
            assert_eq!(name, CommandName::SyncNow);
        }
        other => panic!("expected command frame, got {other:?}"),
    }

    let receipt = ControlMessage::Receipt {
        protocol_version: PROTOCOL_VERSION,
        id: command.id.clone(),
        state: ReceiptState::Accepted,
        reason: None,
    };
    socket.send(frame(&receipt)).await.unwrap();
    let result = ControlMessage::CommandResult {
        protocol_version: PROTOCOL_VERSION,
        id: command.id.clone(),
        state: ResultState::Completed,
        detail: serde_json::json!({"events": 2}),
    };
    socket.send(frame(&result)).await.unwrap();

    // Give the handler time to record receipt and result.
    for _ in 0..50 {
        if hub
            .control_snapshot()
            .iter()
            .any(|queued| queued.id == command.id && queued.state == CommandState::Completed)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let snapshot = hub.control_snapshot();
    let queued = snapshot
        .iter()
        .find(|queued| queued.id == command.id)
        .expect("command is retained after completion");
    assert_eq!(queued.state, CommandState::Completed);
    assert_eq!(queued.result_detail.as_ref().unwrap()["events"], 2);
}

#[tokio::test]
async fn rejected_receipt_is_terminal_on_the_hub() {
    let (_temp, hub, addr) = start_hub().await;

    let mut socket = connect(addr).await;
    hello_and_status(&mut socket).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let command = hub
        .enqueue_command(
            CommandName::RefreshBattery,
            serde_json::json!({}),
            Duration::from_secs(60),
        )
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("command must be delivered");

    let receipt = ControlMessage::Receipt {
        protocol_version: PROTOCOL_VERSION,
        id: command.id.clone(),
        state: ReceiptState::Rejected,
        reason: Some("busy".into()),
    };
    socket.send(frame(&receipt)).await.unwrap();

    for _ in 0..50 {
        if hub
            .control_snapshot()
            .iter()
            .any(|queued| queued.id == command.id && queued.state == CommandState::Failed)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let queued = hub
        .control_snapshot()
        .into_iter()
        .find(|queued| queued.id == command.id)
        .expect("rejected command stays for status");
    assert_eq!(queued.state, CommandState::Failed);
    assert_eq!(queued.receipt_reason.as_deref(), Some("busy"));
}
