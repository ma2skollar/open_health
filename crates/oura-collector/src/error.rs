use thiserror::Error;

#[derive(Debug, Error)]
pub enum CollectorError {
    #[error("collector I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("collector SQLite operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("collector state JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid collector configuration: {0}")]
    Config(String),
    #[error("ring operation failed: {0}")]
    Ring(#[from] oura_ble::RingError),
    #[error("outbox is full; pending snapshots retained")]
    OutboxFull,
    #[error("disk reserve reached; pending snapshots retained")]
    LowDisk,
    #[error("snapshot delivery failed: {0}")]
    Delivery(String),
    #[error("control protocol failed: {0}")]
    Protocol(String),
    #[error("collector is busy")]
    Busy,
    #[error("collector command was cancelled")]
    Cancelled,
}
