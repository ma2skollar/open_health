use clap::{Parser, Subcommand};
use oura_hub::{http, Config, Hub, HubError};
use std::env;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(
    name = "oura-hub",
    about = "Authenticated collector snapshot ingestion"
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Explicitly promote the newest reviewed snapshot from a collector epoch.
    AcceptEpoch {
        #[arg(long)]
        collector: String,
        #[arg(long)]
        epoch: String,
    },
}

fn configuration(message: &str) -> HubError {
    HubError::new("configuration", message)
}

fn positive_number(name: &str) -> Result<Option<u64>, HubError> {
    let value = match env::var(name) {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => return Ok(None),
        Err(_) => return Err(configuration(&format!("{name} must be a positive integer"))),
    };
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .map(Some)
        .ok_or_else(|| configuration(&format!("{name} must be a positive integer")))
}

fn config_from_env() -> Result<Config, HubError> {
    let data_root = env::var_os("OURA_HUB_DATA_DIR")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| configuration("OURA_HUB_DATA_DIR is required"))?;
    let mut config = Config::new(PathBuf::from(data_root));
    if let Some(max_bytes) = positive_number("OURA_HUB_MAX_BYTES")? {
        config.max_bytes = max_bytes;
    }
    if let Some(seconds) = positive_number("OURA_HUB_TIMEOUT_SECONDS")? {
        config.request_timeout = Duration::from_secs(seconds);
    }
    if Instant::now().checked_add(config.request_timeout).is_none() {
        return Err(configuration("OURA_HUB_TIMEOUT_SECONDS is too large"));
    }
    if let Some(limit) = positive_number("OURA_HUB_RATE_LIMIT")? {
        config.rate_limit = u32::try_from(limit)
            .map_err(|_| configuration("OURA_HUB_RATE_LIMIT exceeds the supported range"))?;
    }
    Ok(config)
}

fn bind_from_env() -> Result<SocketAddr, HubError> {
    let value = match env::var("OURA_HUB_BIND") {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => return Ok(SocketAddr::from(([127, 0, 0, 1], 8040))),
        Err(_) => {
            return Err(configuration(
                "OURA_HUB_BIND must be a loopback socket address",
            ))
        }
    };
    let address: SocketAddr = value
        .parse()
        .map_err(|_| configuration("OURA_HUB_BIND must be a loopback socket address"))?;
    if !address.ip().is_loopback() {
        return Err(configuration(
            "OURA_HUB_BIND must use a loopback IP address",
        ));
    }
    Ok(address)
}

async fn run(args: Args) -> Result<(), HubError> {
    let config = config_from_env()?;
    let request_timeout = config.request_timeout;
    let bind = if args.command.is_none() {
        Some(bind_from_env()?)
    } else {
        None
    };
    let hub = tokio::task::spawn_blocking(move || Hub::from_env(config))
        .await
        .map_err(|_| HubError::new("storage_error", "hub initialization failed"))??;
    match args.command {
        Some(Command::AcceptEpoch { collector, epoch }) => {
            let ack = tokio::task::spawn_blocking(move || hub.accept_epoch(&collector, &epoch))
                .await
                .map_err(|_| HubError::new("storage_error", "epoch promotion failed"))??;
            let mut stdout = std::io::stdout().lock();
            serde_json::to_writer(&mut stdout, &ack).map_err(|_| {
                HubError::new("output_error", "acknowledgement could not be written")
            })?;
            stdout.write_all(b"\n").map_err(|_| {
                HubError::new("output_error", "acknowledgement could not be written")
            })?;
            Ok(())
        }
        None => {
            let address = bind.ok_or_else(|| configuration("ingestion bind address is missing"))?;
            let listener = tokio::net::TcpListener::bind(address).await.map_err(|_| {
                HubError::new(
                    "storage_error",
                    "loopback ingestion listener could not be bound",
                )
            })?;
            axum::serve(listener, http::router(hub, request_timeout))
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await
                .map_err(|_| HubError::new("storage_error", "ingestion HTTP service failed"))
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Args::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let mut stderr = std::io::stderr().lock();
            let _ = serde_json::to_writer(&mut stderr, &error.envelope());
            let _ = stderr.write_all(b"\n");
            ExitCode::FAILURE
        }
    }
}
