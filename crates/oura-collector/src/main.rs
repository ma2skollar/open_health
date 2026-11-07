#[cfg(feature = "ble")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use oura_ble::{BleOptions, BleRing, OperationTimeouts};
    use oura_collector::{control::run_control, delivery::HttpUploadSink, CollectorError, Config, Coordinator, SystemClock};
    use oura_protocol_v1::Identity;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.len() != 1 {
        return Err("usage: oura-collector CONFIG.toml".into());
    }
    let config = Config::load(std::path::Path::new(&arguments[0]))?;
    if config.identity != Identity::Production {
        return Err("real collector requires production identity; use oura-collector-sim for demo".into());
    }
    let options = BleOptions {
        ring_name: config.ring_name.clone(),
        timeouts: OperationTimeouts::default(),
        ..BleOptions::default()
    };
    let ring = BleRing::from_key_file(options, &config.key_file)
        .map_err(|error| CollectorError::Config(error.to_string()))?;
    let sink = HttpUploadSink::new(&config)?;
    let clock = Arc::new(SystemClock);
    let coordinator = Coordinator::new(config.clone(), ring, clock.clone())?;
    let (handle, mut actor) = coordinator.spawn_with_delivery(sink);
    let shutdown = CancellationToken::new();
    let control = tokio::spawn(run_control(config, clock, handle.clone(), shutdown.clone()));
    println!("identity: production");
    let actor_result = tokio::select! {
        signal = shutdown_signal() => {
            shutdown.cancel();
            let stopped = handle.shutdown().await;
            let joined = actor.await?;
            signal?;
            stopped?;
            joined
        }
        result = &mut actor => {
            shutdown.cancel();
            result?
        }
    };
    shutdown.cancel();
    control.await??;
    actor_result?;
    Ok(())
}

#[cfg(feature = "ble")]
async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}

#[cfg(not(feature = "ble"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    Err("real collector requires --features ble; use oura-collector-sim for Bluetooth-free scenarios".into())
}
