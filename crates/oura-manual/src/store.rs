//! The SQLite-backed manual store.

use std::path::Path;
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};

use crate::batch::{ImportBatch, RollbackReport};
use crate::error::{Error, Result};
use crate::model::{ManualEntry, MetricKind, UpsertOutcome};
use crate::validate::validate_entry;

/// Schema version this crate writes and understands.
pub const SCHEMA_VERSION: i64 = 1;

const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);

/// Metadata table, created before anything else so the version can be checked
/// before this crate touches a schema it does not own.
const META_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS store_manual_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

const ENTRIES_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS manual_entries (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    local_date TEXT    NOT NULL,
    kind       TEXT    NOT NULL,
    value      REAL    NOT NULL,
    note       TEXT,
    source     TEXT    NOT NULL,
    batch_id   INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(local_date, kind)
);
CREATE INDEX IF NOT EXISTS idx_manual_entries_date ON manual_entries(local_date);
"#;

/// Import provenance. `import_provenance` records the `updated_at` an import left
/// behind for each entry it wrote, which is what lets a rollback tell "still the
/// imported value" from "edited afterwards".
const BATCH_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS import_batches (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    source          TEXT    NOT NULL,
    adapter_version TEXT    NOT NULL,
    imported_at     INTEGER NOT NULL,
    row_count       INTEGER NOT NULL,
    accepted        INTEGER NOT NULL,
    rejected        INTEGER NOT NULL,
    note            TEXT
);
CREATE TABLE IF NOT EXISTS import_provenance (
    batch_id   INTEGER NOT NULL REFERENCES import_batches(id),
    local_date TEXT    NOT NULL,
    kind       TEXT    NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (batch_id, local_date, kind)
);
"#;

const SELECT_ALL: &str = "SELECT id, local_date, kind, value, note, source, batch_id, created_at, updated_at \
     FROM manual_entries";

const SELECT_BATCH: &str = "SELECT id, source, adapter_version, imported_at, row_count, accepted, rejected, note \
     FROM import_batches";

/// Application-owned store for manual metric records.
pub struct ManualStore {
    conn: Connection,
}

impl ManualStore {
    /// Open (creating and migrating if needed) the manual database at `path`.
    ///
    /// On Unix the file is created with mode `0600`, because the contents are
    /// personal health data and the file lives beside the ring database.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = open_file(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // Ring-store wording on purpose: an unavailable WAL means the store is not
        // usable for concurrent readers, so refuse rather than degrade silently.
        let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        if mode != "wal" {
            return Err(Error::Storage(format!(
                "WAL unavailable for {}: journal_mode is {mode}",
                path.display()
            )));
        }
        conn.execute_batch("PRAGMA synchronous=FULL;")?;
        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    /// Open a throwaway store backed by memory. No file, no permissions, and the
    /// WAL requirement is skipped (in-memory databases cannot use WAL).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.execute_batch("PRAGMA synchronous=FULL;")?;
        let store = Self { conn };
        store.init_schema()?;
        Ok(store)
    }

    /// Create the metadata table, refuse a newer schema, then create the rest.
    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(META_SCHEMA)?;
        match read_schema_version(&self.conn)? {
            Some(found) if found > SCHEMA_VERSION => {
                return Err(Error::SchemaTooNew {
                    found,
                    supported: SCHEMA_VERSION,
                });
            }
            // Equal, or an older/absent version: this crate has no historical
            // migrations, so the `IF NOT EXISTS` schema below brings the file to
            // the current shape. The stored version is never rewritten, so a
            // hand-authored older label is not silently upgraded.
            Some(_) => {}
            None => {
                self.conn.execute(
                    "INSERT INTO store_manual_meta (key, value) VALUES ('schema_version', ?1)",
                    params![SCHEMA_VERSION.to_string()],
                )?;
            }
        }
        self.conn.execute_batch(ENTRIES_SCHEMA)?;
        self.conn.execute_batch(BATCH_SCHEMA)?;
        Ok(())
    }

    /// Insert, or replace the existing row for `(local_date, kind)`.
    ///
    /// A replacement overwrites `value`, `note` and `updated_at`; `created_at`,
    /// `source` and `batch_id` of the stored row are preserved, so provenance of
    /// the first write is never lost. `entry.id` is ignored.
    pub fn upsert_entry(&self, entry: &ManualEntry) -> Result<UpsertOutcome> {
        validate_entry(entry)?;
        let tx = self.conn.unchecked_transaction()?;
        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM manual_entries WHERE local_date = ?1 AND kind = ?2",
                params![entry.local_date, entry.kind.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        let outcome = match existing {
            Some(id) => {
                tx.execute(
                    "UPDATE manual_entries SET value = ?1, note = ?2, updated_at = ?3 WHERE id = ?4",
                    params![entry.value, entry.note, entry.updated_at, id],
                )?;
                UpsertOutcome::Updated
            }
            None => {
                tx.execute(
                    "INSERT INTO manual_entries \
                     (local_date, kind, value, note, source, batch_id, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        entry.local_date,
                        entry.kind.as_str(),
                        entry.value,
                        entry.note,
                        entry.source,
                        entry.batch_id,
                        entry.created_at,
                        entry.updated_at
                    ],
                )?;
                UpsertOutcome::Inserted
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// Read the single row for a local date and kind. `Ok(None)` when the day has
    /// no such record — a missing value is not zero.
    pub fn get_entry(&self, local_date: &str, kind: MetricKind) -> Result<Option<ManualEntry>> {
        let mut stmt = self.conn.prepare(&format!(
            "{SELECT_ALL} WHERE local_date = ?1 AND kind = ?2 LIMIT 1"
        ))?;
        let mut rows = stmt.query(params![local_date, kind.as_str()])?;
        match rows.next()? {
            Some(row) => Ok(Some(map_row(row)?)),
            None => Ok(None),
        }
    }

    /// List entries ordered by `local_date` (then `kind`). `from`/`to` are
    /// inclusive `YYYY-MM-DD` bounds; `None` leaves that end open.
    pub fn list_entries(
        &self,
        from: Option<&str>,
        to: Option<&str>,
        kind: Option<MetricKind>,
    ) -> Result<Vec<ManualEntry>> {
        let mut stmt = self.conn.prepare(&format!(
            "{SELECT_ALL} WHERE (?1 IS NULL OR local_date >= ?1) \
             AND (?2 IS NULL OR local_date <= ?2) \
             AND (?3 IS NULL OR kind = ?3) \
             ORDER BY local_date ASC, kind ASC"
        ))?;
        let rows = stmt.query(params![from, to, kind.map(MetricKind::as_str)])?;
        collect_rows(rows)
    }

    /// Every row in the database, ordered like [`Self::list_entries`].
    pub fn export_all(&self) -> Result<Vec<ManualEntry>> {
        let mut stmt = self.conn.prepare(&format!("{SELECT_ALL} ORDER BY local_date ASC, kind ASC"))?;
        let rows = stmt.query([])?;
        collect_rows(rows)
    }

    /// Delete the row for `(local_date, kind)`. `Ok(false)` when there was none.
    pub fn delete_entry(&self, local_date: &str, kind: MetricKind) -> Result<bool> {
        let affected = self.conn.execute(
            "DELETE FROM manual_entries WHERE local_date = ?1 AND kind = ?2",
            params![local_date, kind.as_str()],
        )?;
        Ok(affected > 0)
    }

    /// Record an import run and return its assigned id.
    pub fn create_batch(&self, batch: &ImportBatch) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO import_batches \
             (source, adapter_version, imported_at, row_count, accepted, rejected, note) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                batch.source,
                batch.adapter_version,
                batch.imported_at,
                batch.row_count,
                batch.accepted,
                batch.rejected,
                batch.note
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Read one batch. `Ok(None)` when the id is unknown.
    pub fn get_batch(&self, id: i64) -> Result<Option<ImportBatch>> {
        let mut stmt = self.conn.prepare(&format!("{SELECT_BATCH} WHERE id = ?1"))?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => Ok(Some(map_batch(row)?)),
            None => Ok(None),
        }
    }

    /// Every batch, newest first.
    pub fn list_batches(&self) -> Result<Vec<ImportBatch>> {
        let mut stmt = self
            .conn
            .prepare(&format!("{SELECT_BATCH} ORDER BY imported_at DESC, id DESC"))?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(map_batch(row)?);
        }
        Ok(out)
    }

    /// Record that `batch_id` wrote these entries at their current `updated_at`.
    ///
    /// Call this **after** the entries are written, passing them back as read, so
    /// the recorded timestamp is the one actually stored.
    pub fn record_provenance(&self, batch_id: i64, entries: &[ManualEntry]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO import_provenance \
                 (batch_id, local_date, kind, updated_at) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for entry in entries {
                stmt.execute(params![
                    batch_id,
                    entry.local_date,
                    entry.kind.as_str(),
                    entry.updated_at
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The entries a batch wrote and that still exist, ordered like
    /// [`Self::list_entries`]. Entries edited after the import are included: this
    /// answers "what did this batch touch", not "what would a rollback delete".
    pub fn batch_entries(&self, batch_id: i64) -> Result<Vec<ManualEntry>> {
        let mut stmt = self.conn.prepare(&format!(
            "{SELECT_ALL} WHERE EXISTS (SELECT 1 FROM import_provenance p \
             WHERE p.batch_id = ?1 \
               AND p.local_date = manual_entries.local_date \
               AND p.kind = manual_entries.kind) \
             ORDER BY local_date ASC, kind ASC"
        ))?;
        let rows = stmt.query(params![batch_id])?;
        collect_rows(rows)
    }

    /// Undo a batch, preserving later edits and pre-existing rows.
    ///
    /// An entry is deleted only when **both** hold:
    ///
    /// 1. it was created by this batch — its `batch_id` is this batch — so a row
    ///    the import merely overwrote is never destroyed; and
    /// 2. its `updated_at` still equals the value the import recorded, so a row
    ///    the user corrected afterwards is kept.
    ///
    /// Everything else is kept and counted. The batch and its provenance are
    /// removed either way, in one transaction.
    pub fn rollback_batch(&self, batch_id: i64) -> Result<RollbackReport> {
        let tx = self.conn.unchecked_transaction()?;
        let mut report = RollbackReport::default();

        let recorded: Vec<(String, String, i64)> = {
            let mut stmt = tx.prepare(
                "SELECT local_date, kind, updated_at FROM import_provenance WHERE batch_id = ?1",
            )?;
            let rows = stmt.query_map(params![batch_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        for (local_date, kind, recorded_updated_at) in recorded {
            let current: Option<(i64, Option<i64>)> = tx
                .query_row(
                    "SELECT updated_at, batch_id FROM manual_entries \
                     WHERE local_date = ?1 AND kind = ?2",
                    params![local_date, kind],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            match current {
                None => report.missing += 1,
                Some((_, current_batch)) if current_batch != Some(batch_id) => {
                    report.kept_not_created_by_this_batch += 1;
                }
                Some((updated_at, _)) if updated_at == recorded_updated_at => {
                    tx.execute(
                        "DELETE FROM manual_entries WHERE local_date = ?1 AND kind = ?2",
                        params![local_date, kind],
                    )?;
                    report.removed += 1;
                }
                Some(_) => report.kept_edited_after_import += 1,
            }
        }

        tx.execute(
            "DELETE FROM import_provenance WHERE batch_id = ?1",
            params![batch_id],
        )?;
        tx.execute("DELETE FROM import_batches WHERE id = ?1", params![batch_id])?;
        tx.commit()?;
        Ok(report)
    }
}

fn map_batch(row: &rusqlite::Row<'_>) -> Result<ImportBatch> {
    Ok(ImportBatch {
        id: row.get(0)?,
        source: row.get(1)?,
        adapter_version: row.get(2)?,
        imported_at: row.get(3)?,
        row_count: row.get(4)?,
        accepted: row.get(5)?,
        rejected: row.get(6)?,
        note: row.get(7)?,
    })
}

fn collect_rows(mut rows: rusqlite::Rows<'_>) -> Result<Vec<ManualEntry>> {
    let mut entries = Vec::new();
    while let Some(row) = rows.next()? {
        entries.push(map_row(row)?);
    }
    Ok(entries)
}

fn map_row(row: &rusqlite::Row<'_>) -> Result<ManualEntry> {
    let id: i64 = row.get(0)?;
    let local_date: String = row.get(1)?;
    let kind_raw: String = row.get(2)?;
    let value: f64 = row.get(3)?;
    let note: Option<String> = row.get(4)?;
    let source: String = row.get(5)?;
    let batch_id: Option<i64> = row.get(6)?;
    let created_at: i64 = row.get(7)?;
    let updated_at: i64 = row.get(8)?;
    let kind = MetricKind::from_db(&kind_raw)
        .ok_or_else(|| Error::CorruptData(format!("unknown manual_entries.kind {kind_raw:?}")))?;
    Ok(ManualEntry {
        id,
        local_date,
        kind,
        value,
        note,
        source,
        batch_id,
        created_at,
        updated_at,
    })
}

fn read_schema_version(conn: &Connection) -> Result<Option<i64>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM store_manual_meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    match raw {
        None => Ok(None),
        Some(raw) => raw.trim().parse::<i64>().map(Some).map_err(|_| {
            Error::CorruptData(format!("store_manual_meta.schema_version is not an integer: {raw:?}"))
        }),
    }
}

/// Open the database file, owner-only.
///
/// Preferred path: create the file with mode `0600` in the same syscall that
/// creates it, so there is never a readable window. If that is impossible (an
/// exotic filesystem, say), fall back to a plain create followed immediately by
/// `chmod`, and refuse to open at all if the `chmod` fails.
fn open_file(path: &Path) -> Result<Connection> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let created = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .open(path);
        match created {
            Ok(file) => drop(file),
            Err(restrictive) => {
                let fallback = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .open(path)
                    .map_err(|e| {
                        Error::Storage(format!(
                            "cannot create {}: restrictive create failed ({restrictive}), plain create failed ({e})",
                            path.display()
                        ))
                    })?;
                drop(fallback);
            }
        }
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms).map_err(|e| {
            Error::Storage(format!(
                "cannot restrict {} to mode 0600: {e}",
                path.display()
            ))
        })?;
    }
    Ok(Connection::open(path)?)
}
