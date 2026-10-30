//! `omp-deck-relay`: the self-hosted WebSocket relay between an OmpDeck gateway and its paired
//! devices (`docs/notes/mobile-relay.md`, `docs/relay.md`). It routes opaque Noise messages by
//! room and relay-assigned peer id; it never holds a key that decrypts them.

mod config;
mod conn;
mod logging;
pub mod rooms;
mod server;
mod tls;
mod transport;

use std::path::PathBuf;

pub use config::{AdmissionRate, Config, ProxyRange, RateLimit};
pub use logging::init_logging;
pub use server::serve;
pub use tls::load_tls;

#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("{path}: {detail}", path = .0.display(), detail = .1)]
    Io(PathBuf, #[source] std::io::Error),
    #[error("{path}: {detail}", path = .0.display(), detail = .1)]
    Rooms(PathBuf, String),
    #[error("TLS: {0}")]
    Tls(String),
    #[error("random number generator: {0}")]
    Random(String),
    #[error("listener: {0}")]
    Listen(#[source] std::io::Error),
}
