use oura_collector::{CollectorError, Config};

const USAGE: &str = "Usage: oura-collector --help | --version | validate CONFIG.toml | run CONFIG.toml\n\nvalidate reads configuration only. run explicitly starts history-only collection.\nNo provisioning, pairing, feature mutation, reset or live-HR commands exist.";

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match execute().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => { eprintln!("{}", error.code()); std::process::ExitCode::FAILURE }
    }
}
async fn execute() -> Result<(), CollectorError> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [arg] if arg == "--help" || arg == "-h" => { println!("{USAGE}"); Ok(()) }
        [arg] if arg == "--version" => { println!("oura-collector {}", oura_collector::COLLECTOR_BUILD); Ok(()) }
        [command, path] if command == "validate" => {
            Config::load(std::path::Path::new(path))?;
            println!("config_valid");
            Ok(())
        }
        [command, path] if command == "run" => run(Config::load(std::path::Path::new(path))?).await,
        _ => { eprintln!("{USAGE}"); Err(CollectorError::Config) }
    }
}
#[cfg(not(feature = "ble"))]
async fn run(_: Config) -> Result<(), CollectorError> { Err(CollectorError::Config) }

#[cfg(feature = "ble")]
async fn run(config: Config) -> Result<(), CollectorError> {
    use std::{sync::Arc, time::Duration};
    use oura_ble::{BleOptions, BleRing};
    use oura_collector::{Collector, SystemClock, delivery::HttpUploadSink};
    let mut options = BleOptions::default();
    options.ring_name = config.ring_name.clone();
    options.history_max_events = config.history_max_events;
    options.history_max_bytes = config.history_max_bytes;
    options.history_max_pages = config.history_max_pages;
    options.timeouts.connect = Duration::from_secs(config.operation_timeout_secs);
    options.timeouts.operation = Duration::from_secs(config.operation_timeout_secs);
    options.timeouts.history = Duration::from_secs(config.operation_timeout_secs);
    let ring = BleRing::from_key_file(options, &config.ring_key_file).map_err(|_| CollectorError::Key)?;
    let clock = Arc::new(SystemClock);
    let sink = HttpUploadSink::new(&config, clock.clone())?;
    let mut collector = Collector::new(config, ring, sink, clock)?;
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        #[cfg(unix)]
        let stopping = async { tokio::select! { _ = tokio::signal::ctrl_c() => (), _ = terminate.recv() => () } };
        #[cfg(not(unix))]
        let stopping = async { let _ = tokio::signal::ctrl_c().await; };
        tokio::select! {
            _ = stopping => break,
            result = async { interval.tick().await; collector.tick().await } => result?,
        }
    }
    collector.disconnect().await;
    Ok(())
}
