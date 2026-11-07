//! `oura-backup` — create, verify, list and restore database backups.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use oura_backup::{format_utc, read_manifest, Source};

#[derive(Parser)]
#[command(
    name = "oura-backup",
    about = "Consistent, verified backups of the local health databases"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Copy the databases into a new timestamped backup directory.
    Create {
        /// Directory that holds the backups.
        #[arg(long)]
        dest: PathBuf,
        /// Ring database to include, when it exists.
        #[arg(long)]
        db: Option<PathBuf>,
        /// Manual database to include, when it exists.
        #[arg(long)]
        manual_db: Option<PathBuf>,
        /// Free-text label appended to the directory name.
        #[arg(long, default_value = "scheduled")]
        label: String,
    },
    /// List the backups under a destination, newest first.
    List {
        #[arg(long)]
        dest: PathBuf,
    },
    /// Check a backup: presence, size, checksum and integrity of every file.
    Verify {
        #[arg(long)]
        dir: PathBuf,
    },
    /// Verify a backup, then restore it over the destination databases.
    Restore {
        #[arg(long)]
        dir: PathBuf,
        /// Directory that holds the databases to replace.
        #[arg(long)]
        dest: PathBuf,
        /// Replace existing files. Without this an existing file is refused.
        #[arg(long)]
        force: bool,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> oura_backup::Result<Vec<String>> {
    match cli.command {
        Command::Create {
            dest,
            db,
            manual_db,
            label,
        } => {
            let mut sources = Vec::new();
            // The manual database holds data ring ingestion can never recreate,
            // so it is listed first and restored first.
            if let Some(path) = manual_db {
                sources.push(Source::new("manual.db", path));
            }
            if let Some(path) = db {
                sources.push(Source::new("oura.db", path));
            }

            let dir = oura_backup::create(&dest, &sources, &label)?;
            let manifest = read_manifest(&dir)?;
            let mut out = vec![format!("wrote {}", dir.display())];
            for file in &manifest.files {
                out.push(format!(
                    "  {}: {} bytes{}",
                    file.name,
                    file.bytes,
                    file.schema_version
                        .as_ref()
                        .map(|v| format!(", schema {v}"))
                        .unwrap_or_default()
                ));
            }
            Ok(out)
        }
        Command::List { dest } => {
            let entries = oura_backup::list(&dest)?;
            if entries.is_empty() {
                return Ok(vec![format!("no backups under {}", dest.display())]);
            }
            Ok(entries
                .into_iter()
                .map(|entry| {
                    format!(
                        "{}  {} file(s)  {}",
                        format_utc(entry.manifest.created_unix),
                        entry.manifest.files.len(),
                        entry.dir.display()
                    )
                })
                .collect())
        }
        Command::Verify { dir } => {
            let report = oura_backup::verify(&dir)?;
            let mut out = vec![format!("{}: verified", dir.display())];
            out.extend(report.files);
            Ok(out)
        }
        Command::Restore { dir, dest, force } => {
            let report = oura_backup::restore(&dir, &dest, force)?;
            let mut out = vec![format!(
                "restored the backup taken at {}",
                format_utc(report.created_unix)
            )];
            out.extend(report.restored);
            out.push("restart the service after restoring — see docs/OPERATIONS.md".to_string());
            Ok(out)
        }
    }
}
