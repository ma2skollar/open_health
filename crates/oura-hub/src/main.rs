//! Production ingest service. Serves only `POST /ingest/v1/batches` per the
//! frozen contract in `crates/oura-protocol-v1/src/ingest.rs`; every other
//! method and path is rejected. The raw batch and its nonce are durably
//! committed before the acknowledgement leaves the process.
//!
//! Configuration is environment-only: `OURA_HUB_DB` (SQLite path),
//! `OURA_HUB_KEYS` (owner-only JSON key registry) and `OURA_HUB_BIND`
//! (defaults to `127.0.0.1:8787`).

use std::{net::SocketAddr, path::PathBuf};

use oura_hub::ingest;
use tokio::{net::TcpListener, signal};

fn env_path(name: &str) -> Result<PathBuf, String> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| format!("{name} is required"))
}

fn bind_from_env() -> Result<SocketAddr, String> {
    match std::env::var("OURA_HUB_BIND") {
        Ok(value) => value
            .parse()
            .map_err(|_| "OURA_HUB_BIND must be a socket address".to_string()),
        Err(std::env::VarError::NotPresent) => Ok(SocketAddr::from(([127, 0, 0, 1], 8787))),
        Err(_) => Err("OURA_HUB_BIND is not valid unicode".to_string()),
    }
}

async fn run() -> Result<(), String> {
    let db = env_path("OURA_HUB_DB")?;
    let keys = env_path("OURA_HUB_KEYS")?;
    let bind = bind_from_env()?;
    let listener = TcpListener::bind(bind)
        .await
        .map_err(|error| format!("could not bind {bind}: {error}"))?;
    let service = ingest::Service::new(db, keys);
    axum::serve(listener, ingest::router(service))
        .with_graceful_shutdown(shutdown())
        .await
        .map_err(|error| format!("ingest service failed: {error}"))?;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut terminate = signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler could not be installed");
        tokio::select! {
            _ = signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = signal::ctrl_c().await;
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("oura-hub: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}
