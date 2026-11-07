use std::path::Path;

use thiserror::Error;
use zeroize::Zeroizing;

/// Ring authentication key. Intentionally has no `Debug`, `Display`, serde,
/// cloning, or public byte accessor. Memory is zeroed when the key is dropped.
///
/// `load` accepts exactly 16 raw bytes or 32 hexadecimal digits with optional
/// surrounding ASCII whitespace (at most 64 bytes total). Unix requires a regular, non-symlink file
/// owned by the effective user with permissions exactly `0600`. Other platforms
/// reject loading because they cannot enforce this Unix security contract.
/// The loader never reports file contents, key bytes, or the supplied path.
pub struct AuthKey {
    #[cfg_attr(not(feature = "ble"), allow(dead_code))]
    bytes: Zeroizing<[u8; 16]>,
}

#[derive(Debug, Error)]
pub enum AuthKeyError {
    #[error("cannot read ring authentication key")]
    Io(#[source] std::io::Error),
    #[error("ring authentication key must be a regular file owned by the effective user with mode 0600")]
    Permissions,
    #[error("ring authentication key must contain 16 raw bytes or 32 hexadecimal digits")]
    InvalidFormat,
    #[error("ring authentication key loading requires Unix permission enforcement")]
    UnsupportedPlatform,
}

impl AuthKey {
    #[cfg(unix)]
    pub fn load(path: &Path) -> Result<Self, AuthKeyError> {
        use std::fs::OpenOptions;
        use std::io::Read;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

        // Open first, then inspect the actual descriptor: a path substitution
        // cannot swap the checked file for a different key. O_NONBLOCK prevents
        // a FIFO from blocking before the regular-file check.
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(AuthKeyError::Io)?;
        let metadata = file.metadata().map_err(AuthKeyError::Io)?;
        // SAFETY: geteuid has no preconditions and does not access Rust memory.
        let effective_user = unsafe { libc::geteuid() };
        if !metadata.is_file()
            || metadata.permissions().mode() & 0o7777 != 0o600
            || metadata.uid() != effective_user
        {
            return Err(AuthKeyError::Permissions);
        }
        let mut contents = Zeroizing::new(Vec::with_capacity(65));
        file.by_ref().take(65).read_to_end(&mut contents).map_err(AuthKeyError::Io)?;
        if contents.len() > 64 { return Err(AuthKeyError::InvalidFormat); }
        Self::parse(&contents)
    }

    #[cfg(not(unix))]
    pub fn load(_path: &Path) -> Result<Self, AuthKeyError> {
        Err(AuthKeyError::UnsupportedPlatform)
    }

    fn parse(contents: &[u8]) -> Result<Self, AuthKeyError> {
        let mut bytes = Zeroizing::new([0u8; 16]);
        if contents.len() == 16 {
            bytes.copy_from_slice(contents);
        } else {
            let start = contents.iter().position(|byte| !byte.is_ascii_whitespace()).unwrap_or(contents.len());
            let end = contents.iter().rposition(|byte| !byte.is_ascii_whitespace()).map(|index| index + 1).unwrap_or(start);
            let encoded = &contents[start..end];
            if encoded.len() != 32 { return Err(AuthKeyError::InvalidFormat); }
            for (slot, pair) in bytes.iter_mut().zip(encoded.chunks_exact(2)) {
                let digit = |byte: u8| match byte {
                    b'0'..=b'9' => Some(byte - b'0'),
                    b'a'..=b'f' => Some(byte - b'a' + 10),
                    b'A'..=b'F' => Some(byte - b'A' + 10),
                    _ => None,
                };
                let high = digit(pair[0]).ok_or(AuthKeyError::InvalidFormat)?;
                let low = digit(pair[1]).ok_or(AuthKeyError::InvalidFormat)?;
                *slot = high * 16 + low;
            }
        }
        Ok(Self { bytes })
    }

    #[cfg(feature = "ble")]
    pub(crate) fn bytes(&self) -> &[u8; 16] {
        &self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_binary_and_hex_without_echoing_invalid_contents() {
        let raw = [0x5a; 16];
        let parsed = AuthKey::parse(&raw).unwrap();
        assert_eq!(parsed.bytes.as_ref(), &raw);
        let encoded = [b'5', b'a'].repeat(16);
        let parsed = AuthKey::parse(&encoded).unwrap();
        assert_eq!(parsed.bytes.as_ref(), &raw);
        assert!(matches!(AuthKey::parse(b"not a key"), Err(AuthKeyError::InvalidFormat)));
        assert_eq!(AuthKeyError::InvalidFormat.to_string(), "ring authentication key must contain 16 raw bytes or 32 hexadecimal digits");
    }

    #[cfg(unix)]
    #[test]
    fn requires_mode_0600_and_rejects_symlinks_and_nonfiles() {
        use std::fs::{self, Permissions};
        use std::os::unix::fs::{symlink, PermissionsExt};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("key");
        fs::write(&path, [0x5a; 16]).unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        assert!(AuthKey::load(&path).is_ok());
        for mode in [0o400, 0o640, 0o644, 0o660, 0o700] {
            fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
            assert!(matches!(AuthKey::load(&path), Err(AuthKeyError::Permissions)));
        }
        fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        let link = directory.path().join("key-link");
        symlink(&path, &link).unwrap();
        assert!(AuthKey::load(&link).is_err());
        assert!(matches!(AuthKey::load(directory.path()), Err(AuthKeyError::Permissions)));
        fs::write(&path, [0x5a; 66]).unwrap();
        assert!(matches!(AuthKey::load(&path), Err(AuthKeyError::InvalidFormat)));
        let mut oversized = vec![b'5'; 32];
        oversized.extend_from_slice(&[b' '; 40]);
        fs::write(&path, oversized).unwrap();
        assert!(matches!(AuthKey::load(&path), Err(AuthKeyError::InvalidFormat)));
    }

    #[cfg(not(unix))]
    #[test]
    fn refuses_platforms_without_unix_permissions() {
        assert!(matches!(AuthKey::load(Path::new("unused")), Err(AuthKeyError::UnsupportedPlatform)));
    }
}
