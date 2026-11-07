//! Collector configuration and identity-scoped filesystem ownership.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use chrono::NaiveTime;
use chrono_tz::Tz;
use oura_protocol_v1::Identity;
use serde::{Deserialize, Serialize};

use crate::CollectorError;

const DEMO_DIRECTORY: &str = "oura-collector-demo";
const OWNER_FILE: &str = ".collector-owner.json";

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub collector_id: String,
    pub epoch: String,
    pub ring_name: String,
    pub key_file: PathBuf,
    pub database_path: PathBuf,
    pub hub_url: String,
    pub token_file: PathBuf,
    pub wake_start: String,
    pub wake_end: String,
    pub timezone: String,
    pub retry_min_secs: u64,
    pub retry_max_secs: u64,
    pub max_attempts: u32,
    pub outbox_max_files: usize,
    pub outbox_max_bytes: u64,
    pub min_free_bytes: u64,
    pub identity: Identity,
    pub state_dir: PathBuf,
    pub build_revision: String,
}

// Do not accidentally print credentials, URL userinfo, secret contents or paths.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("collector_id", &self.collector_id)
            .field("epoch", &self.epoch)
            .field("identity", &self.identity)
            .field("timezone", &self.timezone)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize, Serialize)]
struct Owner {
    format: u32,
    collector_id: String,
    identity: Identity,
}

impl Config {
    /// All demo writes live in a dedicated child, never directly in `root`.
    pub fn demo(root: &Path) -> Self {
        let root = if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(root)
        };
        let state_dir = root.join(DEMO_DIRECTORY);
        Self {
            collector_id: "demo-collector".into(),
            epoch: "demo-epoch-1".into(),
            ring_name: "simulated-ring".into(),
            key_file: state_dir.join("secrets/ring.key"),
            database_path: state_dir.join("ring.db"),
            hub_url: "http://localhost:3001".into(),
            token_file: state_dir.join("secrets/collector.token"),
            wake_start: "00:00".into(),
            wake_end: "23:59".into(),
            timezone: "UTC".into(),
            retry_min_secs: 5,
            retry_max_secs: 300,
            max_attempts: 5,
            outbox_max_files: 32,
            outbox_max_bytes: 256 * 1024 * 1024,
            min_free_bytes: 16 * 1024 * 1024,
            identity: Identity::Demo,
            state_dir,
            build_revision: "demo".into(),
        }
    }

    pub fn load(path: &Path) -> Result<Self, CollectorError> {
        Self::load_with_overrides(path, std::env::vars())
    }

    /// Explicit overrides keep tests deterministic without changing process env.
    pub fn load_with_overrides(
        path: &Path,
        overrides: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, CollectorError> {
        let text = fs::read_to_string(path)?;
        let mut value: serde_json::Value = if path.extension().and_then(|v| v.to_str()) == Some("json") {
            serde_json::from_str(&text).map_err(|_| config_error("invalid JSON configuration"))?
        } else {
            let value: toml::Value = toml::from_str(&text)
                .map_err(|_| config_error("invalid TOML configuration"))?;
            serde_json::to_value(value)?
        };
        let object = value.as_object_mut().ok_or_else(|| config_error("configuration must be an object"))?;
        let names: BTreeMap<&str, &str> = [
            ("COLLECTOR_ID", "collector_id"), ("EPOCH", "epoch"),
            ("RING_NAME", "ring_name"), ("KEY_FILE", "key_file"),
            ("DATABASE_PATH", "database_path"), ("HUB_URL", "hub_url"),
            ("TOKEN_FILE", "token_file"), ("WAKE_START", "wake_start"),
            ("WAKE_END", "wake_end"), ("TIMEZONE", "timezone"),
            ("RETRY_MIN_SECS", "retry_min_secs"), ("RETRY_MAX_SECS", "retry_max_secs"),
            ("MAX_ATTEMPTS", "max_attempts"), ("OUTBOX_MAX_FILES", "outbox_max_files"),
            ("OUTBOX_MAX_BYTES", "outbox_max_bytes"), ("MIN_FREE_BYTES", "min_free_bytes"),
            ("IDENTITY", "identity"), ("STATE_DIR", "state_dir"),
            ("BUILD_REVISION", "build_revision"),
        ].into_iter().collect();
        for (name, value) in overrides {
            let Some(name) = name.strip_prefix("OURA_") else { continue };
            let Some(field) = names.get(name) else { continue };
            let numeric = matches!(*field, "retry_min_secs" | "retry_max_secs" | "max_attempts"
                | "outbox_max_files" | "outbox_max_bytes" | "min_free_bytes");
            let value = if numeric {
                serde_json::Value::from(value.parse::<u64>()
                    .map_err(|_| config_error("invalid numeric environment override"))?)
            } else {
                serde_json::Value::String(value)
            };
            object.insert((*field).into(), value);
        }
        let mut config: Self = serde_json::from_value(value)
            .map_err(|_| config_error("missing or invalid collector configuration field"))?;
        let base = absolute_clean(path)?.parent().unwrap_or(Path::new("/")).to_path_buf();
        for path in [&mut config.state_dir, &mut config.database_path, &mut config.key_file, &mut config.token_file] {
            if !path.is_absolute() { *path = base.join(&*path); }
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), CollectorError> {
        if self.collector_id.is_empty() || self.collector_id.len() > 128
            || !self.collector_id.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            || self.epoch.is_empty() || self.epoch.len() > 512 || self.epoch.chars().any(char::is_control)
            || self.ring_name.is_empty() || self.build_revision.is_empty() || self.build_revision.len() > 256 {
            return Err(config_error("invalid collector identity, epoch, ring name or revision"));
        }
        parse_wake_time(&self.wake_start)?;
        parse_wake_time(&self.wake_end)?;
        if self.wake_start == self.wake_end {
            return Err(config_error("wake window must have distinct start and end"));
        }
        self.timezone.parse::<Tz>().map_err(|_| config_error("timezone must be an IANA timezone"))?;
        if self.retry_min_secs == 0 || self.retry_max_secs < self.retry_min_secs
            || self.retry_max_secs > 86_400 || self.max_attempts == 0 || self.max_attempts > 1_000
            || self.outbox_max_files == 0 || self.outbox_max_files > 100_000
            || self.outbox_max_bytes < 4096 {
            return Err(config_error("invalid retry or outbox bounds"));
        }
        let url = reqwest::Url::parse(&self.hub_url).map_err(|_| config_error("invalid hub URL"))?;
        if !url.username().is_empty() || url.password().is_some() || url.query().is_some()
            || url.fragment().is_some() || url.host_str().is_none() || !matches!(url.path(), "" | "/") {
            return Err(config_error("hub URL must be an origin without credentials, query or fragment"));
        }
        let loopback = url.host_str().map(|host| host == "localhost" || host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().map(|ip| ip.is_loopback()).unwrap_or(false)).unwrap_or(false);
        if url.scheme() != "https" && !(self.identity == Identity::Demo && url.scheme() == "http" && loopback) {
            return Err(config_error("production requires HTTPS; demo HTTP requires loopback"));
        }
        let paths = [&self.state_dir, &self.database_path, &self.key_file, &self.token_file];
        for path in paths {
            if !path.is_absolute() { return Err(config_error("collector storage paths must be absolute")); }
            absolute_clean(path)?;
            reject_symlinks(path)?;
        }
        if self.database_path.file_name().and_then(|s| s.to_str()).map(|s| s.eq_ignore_ascii_case("manual.db")).unwrap_or(false)
            || self.database_path == self.key_file || self.database_path == self.token_file
            || self.key_file == self.token_file || self.key_file.starts_with(self.outbox_dir())
            || self.token_file.starts_with(self.outbox_dir()) || self.database_path.starts_with(self.outbox_dir())
            || self.key_file == self.state_dir || self.token_file == self.state_dir {
            return Err(config_error("ring, manual, secret and outbox paths must be separate"));
        }
        if !self.database_path.starts_with(&self.state_dir) || self.database_path == self.state_dir {
            return Err(config_error("ring database must live inside collector state directory"));
        }
        for name in [OWNER_FILE, ".collector.lock", "delivery.sqlite", "scheduler.sqlite", "control-results.sqlite", "pending-history-snapshot"] {
            let reserved = self.state_dir.join(name);
            if [&self.database_path, &self.key_file, &self.token_file].into_iter().any(|path| path == &reserved) {
                return Err(config_error("ring and credential paths cannot replace collector state files"));
            }
        }
        if self.identity == Identity::Demo {
            if self.state_dir.file_name().and_then(|s| s.to_str()) != Some(DEMO_DIRECTORY)
                || self.database_path != self.state_dir.join("ring.db")
                || self.key_file != self.state_dir.join("secrets/ring.key")
                || self.token_file != self.state_dir.join("secrets/collector.token") {
                return Err(config_error("demo paths must remain inside the dedicated demo root"));
            }
            if self.state_dir.components().any(|c| match c {
                Component::Normal(s) => s.to_str().map(|s| matches!(s.to_ascii_lowercase().as_str(), "production" | "manual")).unwrap_or(false),
                _ => false,
            }) {
                return Err(config_error("demo root cannot be inside production or manual storage"));
            }
        } else {
            #[cfg(unix)] {
                check_secret_file(&self.key_file)?;
                check_secret_file(&self.token_file)?;
            }
            #[cfg(not(unix))]
            return Err(config_error("production requires Unix 0600 credential permission checks"));
        }
        Ok(())
    }

    pub fn outbox_dir(&self) -> PathBuf { self.state_dir.join("outbox") }

    pub fn control_url(&self) -> Result<String, CollectorError> {
        self.validate()?;
        let mut url = reqwest::Url::parse(&self.hub_url).map_err(|_| config_error("invalid hub URL"))?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme).map_err(|_| config_error("invalid control URL"))?;
        url.set_path("/api/v1/collector/control");
        Ok(url.into())
    }

    /// Refuse adopting an existing unowned directory or a different identity.
    pub fn prepare_state(&self) -> Result<(), CollectorError> {
        self.validate()?;
        if self.state_dir.exists() {
            if !self.state_dir.join(OWNER_FILE).exists() && !self.provisioned_secrets_only(&self.state_dir)? {
                return Err(config_error("collector state directory is not owned by this collector"));
            }
        }
        private_directory(&self.state_dir)?;
        let owner_path = self.state_dir.join(OWNER_FILE);
        reject_symlinks(&owner_path)?;
        if owner_path.exists() {
            self.check_owner()?;
        } else {
            let owner = Owner { format: 1, collector_id: self.collector_id.clone(), identity: self.identity };
            let mut file = private_new_file(&owner_path)?;
            file.write_all(&serde_json::to_vec(&owner)?)?;
            file.sync_all()?;
            sync_directory(&self.state_dir)?;
        }
        private_directory(&self.outbox_dir())?;
        Ok(())
    }

    fn provisioned_secrets_only(&self, directory: &Path) -> Result<bool, CollectorError> {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if self.identity != Identity::Production || kind.is_symlink() { return Ok(false); }
            if kind.is_dir() {
                if ![&self.key_file, &self.token_file].into_iter().any(|secret| secret.starts_with(&path))
                    || !self.provisioned_secrets_only(&path)? { return Ok(false); }
            } else if !kind.is_file() || (path != self.key_file && path != self.token_file) { return Ok(false); }
        }
        Ok(true)
    }

    fn check_owner(&self) -> Result<(), CollectorError> {
        let owner: Owner = serde_json::from_slice(&fs::read(self.state_dir.join(OWNER_FILE))?)
            .map_err(|_| config_error("invalid collector ownership marker"))?;
        if owner.format != 1 || owner.collector_id != self.collector_id || owner.identity != self.identity {
            return Err(config_error("collector state belongs to another collector or identity"));
        }
        Ok(())
    }

    /// Delete only known, owned demo artifacts. Reject links and unknown entries first.
    pub fn reset_demo(&self) -> Result<(), CollectorError> {
        self.validate()?;
        if self.identity != Identity::Demo { return Err(config_error("reset is demo-only")); }
        if !self.state_dir.exists() { return Ok(()); }
        self.check_owner()?;
        let lease_path = self.state_dir.join(".collector.lock");
        reject_symlinks(&lease_path)?;
        let lease = if lease_path.exists() {
            OpenOptions::new().read(true).write(true).open(&lease_path)?
        } else { private_new_file(&lease_path)? };
        fs2::FileExt::try_lock_exclusive(&lease).map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock
                || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() { CollectorError::Busy }
            else { CollectorError::Io(error) }
        })?;
        let mut files = Vec::new();
        let mut directories = Vec::new();
        collect_demo_files(&self.state_dir, &self.state_dir, &mut files, &mut directories)?;
        // Rename the owned tree before deletion. A newly started demo can only
        // create a new tree, never race reset into deleting its live lock/data.
        let parent = self.state_dir.parent().ok_or_else(|| config_error("demo root has no parent"))?;
        static RESET_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let quarantine = loop {
            let sequence = RESET_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let candidate = parent.join(format!(".oura-collector-demo-reset-{}-{sequence}", std::process::id()));
            if !candidate.try_exists()? { break candidate; }
        };
        fs::rename(&self.state_dir, &quarantine)?;
        sync_directory(parent)?;
        let relocated = |path: &Path| -> Result<PathBuf, CollectorError> {
            Ok(quarantine.join(path.strip_prefix(&self.state_dir).map_err(|_| config_error("invalid reset artifact path"))?))
        };
        for path in files {
            if path != lease_path { fs::remove_file(relocated(&path)?)?; }
        }
        for path in directories.into_iter().rev() { fs::remove_dir(relocated(&path)?)?; }
        // The quarantined lock is unlinked only after releasing its lease.
        drop(lease);
        fs::remove_file(quarantine.join(".collector.lock"))?;
        fs::remove_dir(quarantine)?;
        sync_directory(parent)?;
        Ok(())
    }
}

fn collect_demo_files(root: &Path, dir: &Path, files: &mut Vec<PathBuf>, dirs: &mut Vec<PathBuf>) -> Result<(), CollectorError> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_symlink() { return Err(config_error("demo reset refuses symlinks")); }
        let relative = path.strip_prefix(root).map_err(|_| config_error("invalid demo path"))?;
        let name = relative.to_str().ok_or_else(|| config_error("invalid demo filename"))?.replace('\\', "/");
        if kind.is_dir() {
            if !matches!(name.as_str(), "outbox" | "secrets") { return Err(config_error("demo reset found an unknown directory")); }
            dirs.push(path.clone());
            collect_demo_files(root, &path, files, dirs)?;
        } else if kind.is_file() {
            let known = matches!(name.as_str(), OWNER_FILE | ".collector.lock" | "ring.db" | "ring.db-wal" | "ring.db-shm"
                | "delivery.sqlite" | "delivery.sqlite-wal" | "delivery.sqlite-shm"
                | "scheduler.sqlite" | "scheduler.sqlite-wal" | "scheduler.sqlite-shm"
                | "control-results.sqlite" | "control-results.sqlite-wal" | "control-results.sqlite-shm"
                | "pending-history-snapshot"
                | "secrets/ring.key" | "secrets/collector.token")
                || (name.starts_with("outbox/") && (name.ends_with(".db") || name.ends_with(".db.partial"))
                    && name[7..].bytes().all(|b| b.is_ascii_hexdigit() || b"-.partialdb".contains(&b)));
            if !known { return Err(config_error("demo reset found an unknown file")); }
            files.push(path);
        } else { return Err(config_error("demo reset refuses special files")); }
    }
    Ok(())
}

pub(crate) fn config_error(message: &str) -> CollectorError { CollectorError::Config(message.into()) }

pub(crate) fn parse_wake_time(value: &str) -> Result<NaiveTime, CollectorError> {
    if value.len() != 5 || value.as_bytes()[2] != b':'
        || !value.bytes().enumerate().all(|(i, b)| i == 2 || b.is_ascii_digit()) {
        return Err(config_error("wake time must use HH:MM"));
    }
    NaiveTime::parse_from_str(value, "%H:%M").map_err(|_| config_error("wake time is outside 00:00..23:59"))
}

fn absolute_clean(path: &Path) -> Result<PathBuf, CollectorError> {
    if path.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) || path.as_os_str().is_empty() {
        return Err(config_error("collector paths cannot contain traversal"));
    }
    Ok(if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) })
}

pub(crate) fn reject_symlinks(path: &Path) -> Result<(), CollectorError> {
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => return Err(config_error("collector paths cannot traverse symlinks")),
            Ok(_) => {},
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub(crate) fn check_secret_file(path: &Path) -> Result<(), CollectorError> {
    reject_symlinks(path)?;
    let meta = fs::metadata(path)?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > 16 * 1024 {
        return Err(config_error("credential must be a nonempty regular file of at most 16384 bytes"));
    }
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o777 != 0o600 {
            return Err(config_error("credential files must have mode 0600"));
        }
    }
    Ok(())
}

pub(crate) fn private_directory(path: &Path) -> Result<(), CollectorError> {
    reject_symlinks(path)?;
    fs::create_dir_all(path)?;
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(crate) fn private_new_file(path: &Path) -> Result<fs::File, CollectorError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

pub(crate) fn sync_directory(path: &Path) -> Result<(), CollectorError> {
    #[cfg(unix)] { fs::File::open(path)?.sync_all()?; }
    #[cfg(not(unix))] { let _ = path; }
    Ok(())
}
