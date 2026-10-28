//! The gateway's static X25519 key (`remote.key`: the 32 raw secret bytes, owner-only), created
//! once and shown as its fingerprint in Settings.

use std::io;
use std::path::Path;

use deck_remote::noise::{KEY_LEN, Keypair};
use zeroize::Zeroizing;

use super::RemoteError;

pub const FILE: &str = "remote.key";

/// Loads the key at `path`, creating it on first use. A file of any other size is reported,
/// never replaced: paired devices trust the existing key.
pub fn load_or_create(path: &Path) -> Result<Keypair, RemoteError> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let bytes = Zeroizing::new(bytes);
            let secret: [u8; KEY_LEN] = bytes.as_slice().try_into().map_err(|_| {
                RemoteError::corrupt(
                    path,
                    format!("expected {KEY_LEN} bytes, found {}", bytes.len()),
                )
            })?;
            Ok(Keypair::from_secret(secret))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let pair = Keypair::generate().map_err(|_| RemoteError::Random)?;
            crate::gateway::write_private(path, pair.secret_bytes())
                .map_err(|e| RemoteError::io(path, e))?;
            // Re-read: what is on disk is the key from now on.
            let stored = Zeroizing::new(std::fs::read(path).map_err(|e| RemoteError::io(path, e))?);
            if stored.as_slice() != pair.secret_bytes() {
                return Err(RemoteError::corrupt(path, "the new key did not persist"));
            }
            Ok(pair)
        }
        Err(e) => Err(RemoteError::io(path, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_once_then_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join(FILE);
        let first = load_or_create(&path).unwrap();
        let again = load_or_create(&path).unwrap();
        assert_eq!(first.public(), again.public());
        assert_eq!(std::fs::read(&path).unwrap().len(), KEY_LEN);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_wrong_size_file_is_kept_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, b"short").unwrap();
        assert!(matches!(
            load_or_create(&path),
            Err(RemoteError::Corrupt { .. })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"short");
    }
}
