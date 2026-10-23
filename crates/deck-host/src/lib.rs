//! OmpDeck session host: keeps a session's program running in a PTY (Unix) or ConPTY (Windows)
//! owned by a per-session daemon (`omp-deck-host serve`), so the GUI can detach and re-attach
//! (tmux replacement). Contract: `docs/notes/contracts.md` "Wave 12".
//!
//! Client side (blocking; run it on worker threads): [`Namespace`] lists, inspects and kills
//! sessions, [`create_or_attach`] starts a daemon when needed and returns an [`Attached`]
//! connection that splits into [`AttachedRx`] / [`AttachedTx`].

mod client;
pub mod daemon;
mod error;
mod logfile;
mod namespace;
pub mod protocol;
pub mod pty;
pub mod snapshot;
mod spawn;
mod transport;
#[cfg(windows)]
mod winproc;

pub use client::{
    Attached, AttachedRx, AttachedTx, HostEvent, attach, create_or_attach, daemon_exe, peek, start,
};
pub use error::{Error, Result};
pub use namespace::{Namespace, valid_namespace, valid_session_name};
pub use protocol::{AttachParams, HostInfo, PROTOCOL, Palette, SessionSpec};

/// Version of this crate, reported in `Hello` and [`HostInfo::host_version`].
pub const HOST_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Environment override for [`daemon_exe`].
pub const HOST_EXE_ENV: &str = "OMP_DECK_HOST_EXE";
