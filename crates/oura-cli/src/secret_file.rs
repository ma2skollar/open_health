//! Writing files that must not be readable by other local users.
//!
//! Used for the ring authentication key. The file is created with a restrictive
//! mode so there is never a window where it exists world-readable, and a failed
//! permission change is reported as an error instead of being silently ignored.

use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("writing {}", path.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("writing {}", path.display()))?;

        // An existing file keeps the mode it already had: set it explicitly, and
        // fail loudly rather than leave a secret readable by other users.
        let mut perms = file
            .metadata()
            .with_context(|| format!("stat {}", path.display()))?
            .permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("restricting permissions on {}", path.display()))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("oura-secret-{}-{nanos}-{tag}", std::process::id()))
    }

    #[test]
    fn writes_the_exact_bytes() {
        let path = temp_path("bytes");
        write_private(&path, b"deadbeef\n").expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"deadbeef\n");
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn new_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_path("new");
        write_private(&path, b"x").expect("write");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "a new secret file must be owner-only");
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_wide_file_is_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_path("wide");
        write_private(&path, b"a").expect("write");
        // Simulate a file that already existed with a wide mode: the rewrite must
        // not leave it readable by other users.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        write_private(&path, b"b").expect("rewrite");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "rewriting must restore owner-only");
        let _ = std::fs::remove_file(&path);
    }
}
