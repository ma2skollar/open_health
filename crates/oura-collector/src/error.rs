use thiserror::Error;

/// Display and Debug contain only safe codes, never paths, payloads or credentials.
#[derive(Error)]
pub enum CollectorError {
    #[error("io_failed")]
    Io(#[from] std::io::Error),
    #[error("sqlite_failed")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json_failed")]
    Json(#[from] serde_json::Error),
    #[error("config_invalid")]
    Config,
    #[error("key_permissions")]
    Permissions,
    #[error("key_invalid")]
    Key,
    #[error("state_busy")]
    Busy,
    #[error("state_invalid")]
    State,
    #[error("outbox_pressure")]
    OutboxFull,
    #[error("disk_reserve")]
    LowDisk,
    #[error("clock_unsynchronized")]
    Clock,
    #[error("upload_failed")]
    Delivery,
    #[error("ack_invalid")]
    Ack,
    #[error("contract_invalid")]
    Contract,
    #[error("cursor_regression")]
    CursorRegression,
    #[error("history_limit")]
    HistoryLimit,
    #[error("ring_failed")]
    Ring(#[from] oura_ble::RingError),
    #[error("operation_timeout")]
    Timeout,
}

impl CollectorError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "io_failed", Self::Sqlite(_) => "sqlite_failed", Self::Json(_) => "json_failed",
            Self::Config => "config_invalid", Self::Permissions => "key_permissions", Self::Key => "key_invalid",
            Self::Busy => "state_busy", Self::State => "state_invalid", Self::OutboxFull => "outbox_pressure",
            Self::LowDisk => "disk_reserve", Self::Clock => "clock_unsynchronized", Self::Delivery => "upload_failed",
            Self::Ack => "ack_invalid", Self::Contract => "contract_invalid", Self::CursorRegression => "cursor_regression",
            Self::Ring(oura_ble::RingError::HistoryLimit) => "history_limit",
            Self::Ring(oura_ble::RingError::CursorRegression) => "cursor_regression",
            Self::HistoryLimit => "history_limit", Self::Ring(_) => "ring_failed", Self::Timeout => "operation_timeout",
        }
    }
}

impl std::fmt::Debug for CollectorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code())
    }
}
