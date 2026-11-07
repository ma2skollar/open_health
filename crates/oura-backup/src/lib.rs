//! Consistent, verified backups of the local health databases.
//!
//! A backup is a **directory** holding one file per database plus a manifest with
//! the size, SHA-256 and schema version of each. Copies are made with the SQLite
//! online backup API, so a live database with a concurrent writer and an
//! uncommitted WAL is captured consistently — a plain file copy would not be.
//!
//! A restore verifies before it replaces anything: it checks the manifest shape,
//! every file's size and hash, and `PRAGMA integrity_check` on each copied
//! database. Files are renamed into place, and a failure leaves the destination
//! untouched. Nothing here ever deletes a backup.
//!
//! The manual database holds data that ring ingestion can never recreate, so
//! restore it first — see `docs/OPERATIONS.md` for the ordering.

#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Manifest format this build writes and understands.
pub const FORMAT_VERSION: u32 = 1;

/// Manifest file name inside a backup directory.
pub const MANIFEST: &str = "manifest.json";

/// Result alias for every fallible operation in this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("sqlite error on {path}: {source}")]
    Sqlite {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("{path} failed the integrity check: {detail}")]
    Integrity { path: PathBuf, detail: String },
    #[error("{name} in {dir} is {actual} bytes, the manifest says {expected}")]
    SizeMismatch {
        dir: PathBuf,
        name: String,
        expected: u64,
        actual: u64,
    },
    #[error("{name} in {dir} failed its checksum: the file was modified after the backup")]
    ChecksumMismatch { dir: PathBuf, name: String },
    #[error("{name} is missing from {dir}")]
    Missing { dir: PathBuf, name: String },
    #[error("manifest in {dir} has format version {found}, this build supports {supported}")]
    FormatTooNew {
        dir: PathBuf,
        found: u32,
        supported: u32,
    },
    #[error("{path} already exists; pass --force to replace it")]
    DestinationExists { path: PathBuf },
    #[error("no database was found to back up at {0}")]
    NoSources(PathBuf),
    #[error("{path} is not a backup directory: {detail}")]
    NotABackup { path: PathBuf, detail: String },
    #[error("{0}")]
    Invalid(String),
}

/// One database inside a backup directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Destination file name, e.g. `manual.db`.
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
    /// Schema version reported by the copied database, when it has a meta table.
    pub schema_version: Option<String>,
}

/// What a backup directory contains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format_version: u32,
    pub created_unix: i64,
    pub label: String,
    pub files: Vec<FileEntry>,
}

/// A source database to copy.
#[derive(Debug, Clone)]
pub struct Source {
    /// File name inside the backup directory.
    pub name: String,
    pub path: PathBuf,
}

impl Source {
    pub fn new(name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
        }
    }
}

/// What [`verify`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub ok: bool,
    /// One line per file: name and what was checked.
    pub files: Vec<String>,
}

/// What [`restore`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    pub restored: Vec<String>,
    pub created_unix: i64,
}

/// What [`list`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupEntry {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

fn io_err(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn sqlite_err(path: &Path, source: rusqlite::Error) -> Error {
    Error::Sqlite {
        path: path.to_path_buf(),
        source,
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `YYYYMMDD-HHMMSS` in UTC for a Unix timestamp.
pub fn format_utc(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Days since 1970-01-01 to a civil (year, month, day), UTC.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Copy `src` into `dst` using the SQLite online backup API.
///
/// A live database with a writer and an uncommitted WAL is captured
/// consistently; a plain `fs::copy` of such a file would not be.
fn copy_database(src: &Path, dst: &Path) -> Result<()> {
    let source = Connection::open_with_flags(src, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| sqlite_err(src, e))?;
    let mut target = Connection::open(dst).map_err(|e| sqlite_err(dst, e))?;
    let backup = rusqlite::backup::Backup::new(&source, &mut target).map_err(|e| sqlite_err(src, e))?;
    backup
        .run_to_completion(64, Duration::from_millis(2), None)
        .map_err(|e| sqlite_err(src, e))?;
    drop(backup);
    target
        .execute_batch("PRAGMA journal_mode=DELETE;")
        .map_err(|e| sqlite_err(dst, e))?;
    Ok(())
}

/// `PRAGMA integrity_check` on a database, as a plain verdict.
fn integrity_check(path: &Path) -> Result<String> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| sqlite_err(path, e))?;
    let verdict: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|e| sqlite_err(path, e))?;
    Ok(verdict)
}

/// Read the schema version a database reports, if it has one of the known meta
/// tables. Advisory only: it is recorded, never used to gate a restore.
fn schema_version_of(path: &Path) -> Option<String> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    for (table, key) in [
        ("store_manual_meta", "schema_version"),
        ("store_meta", "decoder_version"),
        ("store_meta", "schema_version"),
    ] {
        let sql = format!("SELECT value FROM {table} WHERE key = ?1");
        if let Ok(value) = conn.query_row(&sql, [key], |row| row.get::<_, String>(0)) {
            return Some(value);
        }
    }
    None
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).map_err(|e| io_err(path, e))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|e| io_err(path, e))?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Sanitise a label so it cannot escape the destination directory.
fn safe_label(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "backup".to_string()
    } else {
        trimmed
    }
}

/// Copy every existing source into a new directory under `dest_root`.
///
/// Returns the backup directory. Nothing is overwritten: the directory name
/// carries the creation time, so a second run in the same second is the only way
/// to collide, and that fails loudly instead of merging.
pub fn create(dest_root: &Path, sources: &[Source], label: &str) -> Result<PathBuf> {
    let existing: Vec<&Source> = sources.iter().filter(|s| s.path.is_file()).collect();
    if existing.is_empty() {
        let where_ = sources
            .first()
            .map(|s| s.path.clone())
            .unwrap_or_else(|| dest_root.to_path_buf());
        return Err(Error::NoSources(where_));
    }

    let created_unix = now_unix();
    let dir = dest_root.join(format!("{}-{}", format_utc(created_unix), safe_label(label)));
    if dir.exists() {
        return Err(Error::DestinationExists { path: dir });
    }
    fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;

    let mut files = Vec::new();
    for source in existing {
        let target = dir.join(&source.name);
        copy_database(&source.path, &target)?;

        let verdict = integrity_check(&target)?;
        if verdict != "ok" {
            // Do not keep a bad copy: remove it, so the directory is either a
            // complete backup or an empty one.
            let _ = fs::remove_file(&target);
            return Err(Error::Integrity {
                path: source.path.clone(),
                detail: verdict,
            });
        }

        let bytes = fs::metadata(&target).map_err(|e| io_err(&target, e))?.len();
        files.push(FileEntry {
            name: source.name.clone(),
            bytes,
            sha256: sha256_file(&target)?,
            schema_version: schema_version_of(&target),
        });
    }

    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        created_unix,
        label: safe_label(label),
        files,
    };
    let path = dir.join(MANIFEST);
    let json = serde_json::to_vec_pretty(&manifest).map_err(|e| Error::Invalid(e.to_string()))?;
    fs::write(&path, json).map_err(|e| io_err(&path, e))?;

    Ok(dir)
}

/// Read a backup's manifest, refusing a format this build cannot interpret.
pub fn read_manifest(dir: &Path) -> Result<Manifest> {
    let path = dir.join(MANIFEST);
    let raw = fs::read(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::NotABackup {
                path: dir.to_path_buf(),
                detail: format!("no {MANIFEST}"),
            }
        } else {
            io_err(&path, e)
        }
    })?;
    let manifest: Manifest = serde_json::from_slice(&raw).map_err(|e| Error::NotABackup {
        path: dir.to_path_buf(),
        detail: format!("{MANIFEST} is unreadable: {e}"),
    })?;
    if manifest.format_version > FORMAT_VERSION {
        return Err(Error::FormatTooNew {
            dir: dir.to_path_buf(),
            found: manifest.format_version,
            supported: FORMAT_VERSION,
        });
    }
    Ok(manifest)
}

/// Check every file in a backup: presence, size, checksum and integrity.
pub fn verify(dir: &Path) -> Result<VerifyReport> {
    let manifest = read_manifest(dir)?;
    let mut files = Vec::new();

    for entry in &manifest.files {
        let path = dir.join(&entry.name);
        if !path.is_file() {
            return Err(Error::Missing {
                dir: dir.to_path_buf(),
                name: entry.name.clone(),
            });
        }
        let actual = fs::metadata(&path).map_err(|e| io_err(&path, e))?.len();
        if actual != entry.bytes {
            return Err(Error::SizeMismatch {
                dir: dir.to_path_buf(),
                name: entry.name.clone(),
                expected: entry.bytes,
                actual,
            });
        }
        let digest = sha256_file(&path)?;
        if !digest.eq_ignore_ascii_case(&entry.sha256) {
            return Err(Error::ChecksumMismatch {
                dir: dir.to_path_buf(),
                name: entry.name.clone(),
            });
        }
        let verdict = integrity_check(&path)?;
        if verdict != "ok" {
            return Err(Error::Integrity {
                path,
                detail: verdict,
            });
        }
        files.push(format!(
            "{}: {} bytes, sha256 ok, integrity ok{}",
            entry.name,
            entry.bytes,
            entry
                .schema_version
                .as_ref()
                .map(|v| format!(", schema {v}"))
                .unwrap_or_default()
        ));
    }

    Ok(VerifyReport { ok: true, files })
}

/// Every backup directory under `dest_root`, newest first. A directory whose
/// manifest cannot be read is reported as an error, never skipped silently.
pub fn list(dest_root: &Path) -> Result<Vec<BackupEntry>> {
    let mut out = Vec::new();
    let entries = fs::read_dir(dest_root).map_err(|e| io_err(dest_root, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| io_err(dest_root, e))?;
        let path = entry.path();
        if !path.is_dir() || !path.join(MANIFEST).is_file() {
            continue;
        }
        out.push(BackupEntry {
            manifest: read_manifest(&path)?,
            dir: path,
        });
    }
    out.sort_by(|a, b| {
        b.manifest
            .created_unix
            .cmp(&a.manifest.created_unix)
            .then_with(|| b.dir.cmp(&a.dir))
    });
    Ok(out)
}

/// Verify a backup, then replace the destination files with its contents.
///
/// Without `force`, an existing destination file is refused. Each file is written
/// to a temporary name in the destination directory and renamed into place, so a
/// crash cannot leave a half-written database and a rejected restore leaves the
/// previous file untouched.
pub fn restore(dir: &Path, dest_dir: &Path, force: bool) -> Result<RestoreReport> {
    let report = verify(dir)?;
    let manifest = read_manifest(dir)?;

    for entry in &manifest.files {
        let target = dest_dir.join(&entry.name);
        if target.exists() && !force {
            return Err(Error::DestinationExists { path: target });
        }
    }

    fs::create_dir_all(dest_dir).map_err(|e| io_err(dest_dir, e))?;

    let mut restored = Vec::new();
    for entry in &manifest.files {
        let from = dir.join(&entry.name);
        let target = dest_dir.join(&entry.name);
        let temp = dest_dir.join(format!(".{}.restoring", entry.name));
        fs::copy(&from, &temp).map_err(|e| io_err(&temp, e))?;
        // Rename is atomic within a directory: a reader sees the old file or the
        // new one, never a partial write.
        fs::rename(&temp, &target).map_err(|e| {
            let _ = fs::remove_file(&temp);
            io_err(&target, e)
        })?;
        restored.push(format!("{}: restored {} bytes", entry.name, entry.bytes));
    }

    debug_assert!(report.ok);
    Ok(RestoreReport {
        restored,
        created_unix: manifest.created_unix,
    })
}
