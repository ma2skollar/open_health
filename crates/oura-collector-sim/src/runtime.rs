use crate::{Fixture, START};
use oura_ble::ScriptOutcome;
use oura_collector::{control::ControlSession, CollectorError, LocalCommand};
use oura_protocol_v1::{CommandName, ControlMessage, ResultState, PROTOCOL_VERSION};
use std::time::Duration;

pub(super) async fn run(name: &str) -> Result<String, CollectorError> {
    let f = Fixture::new()?;
    let ring = f.ring.clone();
    let clock = f.clock.clone();
    let config = f.config.clone();
    let (handle, actor) = f.collector.spawn();
    let log = match name {
        "live-stop" | "live-timeout" | "live-disconnect" | "live-shutdown" => {
            ring.push("live_hr", ScriptOutcome::Slow).unwrap();
            let ticket = handle.submit(LocalCommand::StartLiveHr { max_secs: 2 })?;
            wait_for_call(&ring, "live_hr").await;
            match name {
                "live-stop" => {
                    handle.execute(LocalCommand::StopLiveHr).await?;
                }
                "live-timeout" => {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
                "live-disconnect" => {
                    ring.disconnect();
                }
                "live-shutdown" => {
                    handle.shutdown().await?;
                }
                _ => unreachable!(),
            }
            let outcome = ticket.wait().await;
            assert!(outcome.is_err());
            assert!(!ring.live_active());
            assert!(ring.calls().iter().any(|call| call == "stop_live_hr"));
            if name != "live-shutdown" {
                handle.execute(LocalCommand::SyncNow).await?;
                handle.shutdown().await?;
            }
            format!("live HR: bounded session started\nteardown: {}\nring live session: inactive\ncollection: {}\n", &name[5..], if name == "live-shutdown" { "shutdown complete" } else { "recovered after live teardown" })
        }
        "busy-cancelled" => {
            ring.push("drain_history", ScriptOutcome::Slow).unwrap();
            let ticket = handle.submit(LocalCommand::SyncNow)?;
            wait_for_call(&ring, "drain_history").await;
            assert!(matches!(handle.submit(LocalCommand::RefreshBattery), Err(CollectorError::Busy)));
            assert!(handle.cancel());
            assert!(ticket.wait().await.is_err());
            handle.execute(LocalCommand::SyncNow).await?;
            handle.shutdown().await?;
            "command: first running\nsecond command: busy; not queued\nfirst command: cancelled; link torn down\nnext sync: recovered\n".into()
        }
        "expired-reconnect" => {
            let mut control = ControlSession::open(&config, clock.clone(), handle.clone())?;
            control.disconnect();
            clock.advance(Duration::from_secs(10));
            let before = ring.calls().len();
            control.connected();
            let messages = control.receive(ControlMessage::Command {
                protocol_version: PROTOCOL_VERSION,
                id: "expired-offline-demo-command".into(),
                name: CommandName::StartLiveHr,
                args: serde_json::json!({"max_secs": 2}),
                created_unix: START,
                expires_unix: START + 5,
            }).await?;
            assert!(messages.iter().any(|message| matches!(message, ControlMessage::CommandResult { state: ResultState::Expired, .. })));
            assert_eq!(ring.calls().len(), before);
            handle.shutdown().await?;
            "control: disconnected\nclock: advanced beyond interactive expiry\ncontrol: reconnected; expired command received\nreceipt: rejected\nresult: expired\nphysical execution count: 0\n".into()
        }
        _ => return Err(CollectorError::Config(format!("unknown runtime scenario: {name}"))),
    };
    actor.await.map_err(|error| CollectorError::Protocol(error.to_string()))??;
    Ok(log)
}

async fn wait_for_call(ring: &oura_ble::FakeRingHandle, operation: &str) {
    for _ in 0..100 {
        if ring.calls().iter().any(|call| call == operation) {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("coordinator never started {operation}");
}
