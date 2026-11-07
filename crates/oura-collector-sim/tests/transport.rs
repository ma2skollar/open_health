use oura_ble::{FakeRing, RingError, RingTransport, ScriptOutcome};

#[tokio::test(start_paused = true)]
async fn every_scripted_fault_is_typed_and_recoverable_without_bluetooth() {
    for outcome in [
        ScriptOutcome::Absent,
        ScriptOutcome::OutOfRange,
        ScriptOutcome::Busy,
        ScriptOutcome::Failed,
        ScriptOutcome::Cancelled,
        ScriptOutcome::Expired,
    ] {
        let (mut ring, script) = FakeRing::new();
        script.push("connect", outcome).unwrap();
        assert!(ring.connect().await.is_err());
        script.push("connect", ScriptOutcome::Present).unwrap();
        ring.connect().await.unwrap();
        assert!(!ring.hardware_id().await.unwrap().is_empty());
        assert!(!ring.device_info().await.unwrap().serial.is_empty());
        assert!(ring.battery().await.unwrap() <= 100);
        ring.set_feature_mode(1).await.unwrap();
        assert_eq!(ring.feature_mode().await.unwrap(), 1);
        ring.disconnect().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn slow_connect_and_history_return_typed_timeouts_then_recover() {
    let (mut ring, script) = FakeRing::new();
    script.push("connect", ScriptOutcome::Slow).unwrap();
    assert!(matches!(ring.connect().await, Err(RingError::Timeout { operation: "connect" })));
    ring.connect().await.unwrap();
    script.push("drain_history", ScriptOutcome::Slow).unwrap();
    assert!(matches!(ring.drain_history(0).await, Err(RingError::Timeout { operation: "drain_history" })));
    let batch = ring.drain_history(0).await.unwrap();
    assert!(batch.complete);
    assert_eq!(batch.events.len(), 2);
    ring.disconnect().await.unwrap();
}
