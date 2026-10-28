//! Mobile access, gateway side (`docs/notes/mobile-relay.md`, "R3 contract"): the gateway's
//! static key ([`identity`]), the relay settings ([`config`]), the paired devices
//! ([`registry`]), the pairing window state machine ([`pairing`]) and the relay link thread
//! ([`link`]). The engine owns every decision (`engine/remote.rs`); the link thread owns the
//! socket and the per-peer Noise state.
//!
//! Every file lives in the gateway's private state dir, written owner-only. No secret (room
//! secret, admission token, PSK, invite, key) reaches a topic, a log or a `Debug` output.

pub mod config;
pub mod epoch;
pub mod identity;
pub mod link;
pub mod pairing;
pub mod registry;

use std::io;
use std::path::PathBuf;

/// Everything that can go wrong with the mobile-access state.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{}: {detail}", path.display())]
    Corrupt { path: PathBuf, detail: String },
    #[error("the operating system's random number generator failed")]
    Random,
    #[error("{0}")]
    Invalid(String),
}

impl RemoteError {
    fn io(path: &std::path::Path, source: io::Error) -> RemoteError {
        RemoteError::Io {
            path: path.to_path_buf(),
            source,
        }
    }

    fn corrupt(path: &std::path::Path, detail: impl Into<String>) -> RemoteError {
        RemoteError::Corrupt {
            path: path.to_path_buf(),
            detail: detail.into(),
        }
    }
}

/// `N` random bytes from the OS.
pub(crate) fn random<const N: usize>() -> Result<[u8; N], RemoteError> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|_| RemoteError::Random)?;
    Ok(bytes)
}

/// Lowercase hex of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}
