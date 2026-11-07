use oura_collector_sim::{reset_demo, run_scenario};

macro_rules! scenario {
    ($test:ident, $name:literal) => {
        #[tokio::test(flavor = "current_thread", start_paused = true)]
        async fn $test() {
            let log = run_scenario($name).await.unwrap();
            assert!(log.starts_with("identity: demo\n"));
            assert!(log.ends_with("result: PASS\n"));
            println!("{log}");
        }
    };
}

scenario!(normal_sync, "normal-sync");
scenario!(ring_absent_with_backoff, "absent-backoff");
scenario!(offline_server_then_recovery_with_buffered_snapshots, "offline-recovery");
scenario!(duplicate_delivery_is_harmless_after_lost_ack, "duplicate-delivery");
scenario!(stale_generation_cannot_roll_back_data, "stale-generation");
scenario!(collector_reprovisioning_needs_explicit_new_epoch_review, "new-epoch");
scenario!(incomplete_history_keeps_partial_progress_and_resumes, "incomplete-history");
scenario!(live_hr_stop_tears_down, "live-stop");
scenario!(live_hr_local_timeout_tears_down, "live-timeout");
scenario!(live_hr_disconnect_tears_down, "live-disconnect");
scenario!(live_hr_shutdown_tears_down, "live-shutdown");
scenario!(busy_and_cancelled_commands_do_not_overlap, "busy-cancelled");
scenario!(expired_command_never_executes_after_reconnect, "expired-reconnect");

#[test]
fn safe_demo_reset_never_accepts_production_paths() {
    let production = tempfile::tempdir().unwrap();
    let manual = production.path().join("manual.db");
    let ring = production.path().join("production-ring.db");
    std::fs::write(&manual, b"real manual sentinel").unwrap();
    std::fs::write(&ring, b"real ring sentinel").unwrap();
    let log = reset_demo().unwrap();
    assert!(log.contains("identity: demo"));
    assert_eq!(std::fs::read(manual).unwrap(), b"real manual sentinel");
    assert_eq!(std::fs::read(ring).unwrap(), b"real ring sentinel");
}
