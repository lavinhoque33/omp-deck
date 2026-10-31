//! Gateway epochs (`docs/notes/mobile-relay.md`, "Gateway epoch"): every relay connection
//! attempt says `max(unix ms, last epoch + 1)` in its hello, and the relay lets a hello displace
//! the room's gateway only when its epoch is not older. An abandoned connection's late hello
//! therefore cannot take the room back from the link that replaced it.
//!
//! One [`Epochs`] per owner, shared by every link thread behind one lock: allocation is strictly
//! increasing across old and new links, and persisted before the hello leaves, so it keeps
//! rising across owner restarts even when the clock steps back. Losing the file is harmless
//! (wall time takes over).

use std::io;
use std::path::{Path, PathBuf};

use super::RemoteError;

/// In the gateway's state dir: the last epoch, in decimal.
pub const FILE: &str = "relay-epoch";

#[derive(Debug)]
pub struct Epochs {
    path: PathBuf,
    last: u64,
}

impl Epochs {
    /// Reads the last epoch; a missing or unreadable file starts from 0 (logged if unreadable).
    pub fn load(path: &Path) -> Epochs {
        let last = match std::fs::read_to_string(path) {
            Ok(text) => text.trim().parse().unwrap_or_else(|_| {
                log::warn!("{}: not an epoch; starting from the clock", path.display());
                0
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
            Err(e) => {
                log::warn!("{}: {e}; starting from the clock", path.display());
                0
            }
        };
        Epochs {
            path: path.to_path_buf(),
            last,
        }
    }

    /// The next epoch, `max(now_unix_ms, last + 1)`, on disk before it is returned. An
    /// exhausted counter is an error (a reused epoch would not be newer), not a wrap.
    pub fn next(&mut self, now_unix_ms: u64) -> Result<u64, RemoteError> {
        let after = self.last.checked_add(1).ok_or_else(|| {
            RemoteError::corrupt(
                &self.path,
                "the gateway epoch is exhausted; delete this file to restart from the clock",
            )
        })?;
        let epoch = now_unix_ms.max(after);
        crate::gateway::write_private(&self.path, epoch.to_string().as_bytes())
            .map_err(|e| RemoteError::io(&self.path, e))?;
        self.last = epoch;
        Ok(epoch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn epochs_rise_strictly_even_when_the_clock_steps_back_and_survive_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let mut e = Epochs::load(&path);
        assert_eq!(e.next(1_000).unwrap(), 1_000);
        // Same millisecond, then a clock stepping back: still strictly rising.
        assert_eq!(e.next(1_000).unwrap(), 1_001);
        assert_eq!(e.next(500).unwrap(), 1_002);
        assert_eq!(e.next(2_000).unwrap(), 2_000);
        // An owner restart with a clock further back continues from the stored epoch.
        let mut again = Epochs::load(&path);
        assert_eq!(again.next(10).unwrap(), 2_001);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "owner-only");
        }
        // A garbled file is harmless: the clock takes over.
        std::fs::write(&path, "garbage").unwrap();
        assert_eq!(Epochs::load(&path).next(5_000).unwrap(), 5_000);
    }

    #[test]
    fn an_exhausted_epoch_is_an_error_never_a_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let mut e = Epochs::load(&path);
        assert_eq!(e.next(u64::MAX).unwrap(), u64::MAX);
        assert!(e.next(u64::MAX).is_err());
        assert!(e.next(0).is_err());
        // Nothing was rewritten, and a reload stays exhausted.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            u64::MAX.to_string()
        );
        assert!(Epochs::load(&path).next(1).is_err());
    }

    #[test]
    fn concurrent_links_never_share_or_reorder_an_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(Mutex::new(Epochs::load(&dir.path().join(FILE))));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let shared = shared.clone();
                std::thread::spawn(move || {
                    // A frozen clock: only the lock keeps them apart.
                    (0..50)
                        .map(|_| shared.lock().unwrap().next(7).unwrap())
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut all = Vec::new();
        for t in threads {
            let mine = t.join().unwrap();
            assert!(mine.windows(2).all(|w| w[0] < w[1]), "{mine:?}");
            all.extend(mine);
        }
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 100);
    }
}
