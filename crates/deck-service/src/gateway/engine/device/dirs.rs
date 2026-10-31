//! A device's dir tokens (`docs/notes/mobile-pwa.md`, "No path ever crosses to a device"): the
//! folder browser hands out `"f-"` + 16 hex digits for each folder it shows, and folder ops
//! accept only those. The map lives as long as the device session and holds at most
//! [`api::MAX_DIR_TOKENS`] folders (the oldest are forgotten first).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use deck_remote::api;
use sha2::{Digest, Sha256};

pub(super) struct DirTokens {
    /// Makes this session's tokens unrelated to another session's.
    salt: [u8; 16],
    paths: HashMap<String, PathBuf>,
    /// Tokens oldest first.
    order: VecDeque<String>,
}

impl Default for DirTokens {
    fn default() -> DirTokens {
        let mut salt = [0u8; 16];
        if getrandom::fill(&mut salt).is_err() {
            let n = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            salt = n.to_le_bytes();
        }
        DirTokens {
            salt,
            paths: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

impl DirTokens {
    /// The token of `dir` (the same one while it is remembered).
    pub(super) fn token(&mut self, dir: &Path) -> String {
        let mut h = Sha256::new();
        h.update(self.salt);
        h.update(dir.as_os_str().as_encoded_bytes());
        let digest = h.finalize();
        let mut token = String::with_capacity(18);
        token.push_str("f-");
        for b in &digest[..8] {
            token.push_str(&format!("{b:02x}"));
        }
        if !self.paths.contains_key(&token) {
            while self.paths.len() >= api::MAX_DIR_TOKENS {
                match self.order.pop_front() {
                    Some(old) => {
                        self.paths.remove(&old);
                    }
                    None => break,
                }
            }
            self.paths.insert(token.clone(), dir.to_path_buf());
            self.order.push_back(token.clone());
        }
        token
    }

    /// The folder of `token` (`None`: never handed out, or forgotten).
    pub(super) fn resolve(&self, token: &str) -> Option<&Path> {
        self.paths.get(token).map(PathBuf::as_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_opaque_stable_and_bounded() {
        let mut t = DirTokens::default();
        let a = t.token(Path::new("/home/u/project"));
        assert!(a.starts_with("f-") && a.len() == 18);
        assert!(!a.contains("home"));
        assert_eq!(t.token(Path::new("/home/u/project")), a);
        assert_eq!(t.resolve(&a), Some(Path::new("/home/u/project")));
        assert_eq!(t.resolve("/home/u/project"), None);
        assert_eq!(t.resolve("f-0000000000000000"), None);
        // Another session's tokens differ.
        assert_ne!(DirTokens::default().token(Path::new("/home/u/project")), a);
        for i in 0..api::MAX_DIR_TOKENS {
            t.token(&PathBuf::from(format!("/d/{i}")));
        }
        assert_eq!(t.paths.len(), api::MAX_DIR_TOKENS);
        // The oldest went first.
        assert_eq!(t.resolve(&a), None);
    }
}
