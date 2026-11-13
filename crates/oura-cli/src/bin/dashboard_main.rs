//! `oura-dashboard` — the web dashboard server only, with no BLE backend.
//!
//! Packaged builds compile this binary with `--no-default-features` so no
//! btleplug/BlueZ/D-Bus code is linked at all: an LXC with no Bluetooth
//! adapter serves the same dashboard as `oura dashboard`. Configuration is
//! environment-only (the `oura` CLI keeps its flags):
//!
//!   OURA_RING_DB     ring/summary SQLite (required)
//!   OURA_MANUAL_DB   manual metrics SQLite (default: manual.db beside the ring DB)
//!   OURA_BIND        listen address (default 127.0.0.1)
//!   OURA_PORT        listen port (default 8090)
//!   OURA_TZ_OFFSET   display timezone, hours from UTC (default: the host's)
//!   OURA_WEIGHT_UNIT kg | lb (default kg)
//!   OURA_PUBLIC_ORIGIN  https://host[:port] when a reverse proxy fronts the dashboard
//!   OURA_HUB_DB      the hub's ingest SQLite (default: the ring DB path)

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};

#[path = "../dashboard.rs"]
mod dashboard;
#[path = "../icons.rs"]
mod icons;
#[path = "../pyrunner.rs"]
mod pyrunner;

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| default.to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,oura=info".into()),
        )
        .with_target(false)
        .init();

    let db = std::env::var_os("OURA_RING_DB")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .context("OURA_RING_DB (the ring/summary SQLite path) is required")?;
    let manual_db = std::env::var_os("OURA_MANUAL_DB")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| db.with_file_name("manual.db"));
    let tz = match env_or("OURA_TZ_OFFSET", "").parse::<f64>() {
        Ok(h) if h.is_finite() && (-24.0..=24.0).contains(&h) => h,
        // No explicit offset: fall back to the host's, like the CLI default.
        _ => {
            #[cfg(unix)]
            unsafe {
                let mut now: libc::time_t = 0;
                libc::time(&mut now);
                let mut tm: libc::tm = std::mem::zeroed();
                if libc::localtime_r(&now, &mut tm).is_null() {
                    0.0
                } else {
                    tm.tm_gmtoff as f64 / 3600.0
                }
            }
            #[cfg(not(unix))]
            {
                rusqlite::Connection::open_in_memory()
                    .ok()
                    .and_then(|conn| {
                        conn.query_row(
                            "SELECT round((julianday('now', 'localtime') - julianday('now')) * 96.0) / 4.0",
                            [],
                            |row| row.get::<_, f64>(0),
                        )
                        .ok()
                    })
                    .filter(|h| h.is_finite() && (-24.0..=24.0).contains(h))
                    .unwrap_or(0.0)
            }
        }
    };
    let weight_unit = env_or("OURA_WEIGHT_UNIT", "kg");
    let unit = crate::dashboard::WeightUnit::parse(&weight_unit)
        .ok_or_else(|| anyhow!("OURA_WEIGHT_UNIT must be kg or lb (got {weight_unit:?})"))?;
    let seed = crate::dashboard::Demographics {
        sex: env_or("OURA_SEX", "M").chars().next().unwrap_or('M'),
        age: env_or("OURA_AGE", "30").parse().unwrap_or(30.0),
        height_m: env_or("OURA_HEIGHT", "1.78").parse().unwrap_or(1.78),
        weight_kg: env_or("OURA_PROFILE_WEIGHT", "75").parse().unwrap_or(75.0),
        ring_size: env_or("OURA_RING_SIZE", "12").parse().unwrap_or(12.0),
    };
    crate::dashboard::serve(
        env_or("OURA_PORT", "8090").parse().context("OURA_PORT must be a number")?,
        env_or("OURA_BIND", "127.0.0.1"),
        db,
        tz,
        seed,
        manual_db,
        unit,
        env_or("OURA_PUBLIC_ORIGIN", ""),
    )
    .await
}