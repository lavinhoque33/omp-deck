use std::io;
use std::path::PathBuf;

/// Errors of the host client, daemon and PTY layers.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid host namespace {0:?} (expected [A-Za-z0-9_-]{{1,40}})")]
    InvalidNamespace(String),
    #[error("invalid session name {0:?} (expected [A-Za-z0-9_-]{{1,64}})")]
    InvalidSessionName(String),
    #[error("refusing insecure host directory {path}: {reason}")]
    InsecureDir { path: PathBuf, reason: String },
    #[error("socket path too long ({len} bytes, limit {limit}): {path}")]
    PathTooLong {
        path: PathBuf,
        len: usize,
        limit: usize,
    },
    #[error("session {0:?} is not running")]
    NotRunning(String),
    #[error("session {0:?} is already running")]
    AlreadyRunning(String),
    #[error(
        "host protocol mismatch: this build speaks {ours}, session daemon speaks {theirs} \
         (omp-deck-host {host_version}); the session can only be killed"
    )]
    ProtocolMismatch {
        ours: u32,
        theirs: u32,
        host_version: String,
    },
    #[error("host protocol error: {0}")]
    Protocol(String),
    /// The session's daemon predates the requested operation (e.g. `Observe`); nothing was
    /// changed and the caller must not fall back to a displacing attach.
    #[error("session {0:?} runs an older omp-deck-host that does not support observing")]
    Unsupported(String),
    #[error("frame of {0} bytes exceeds the 1 MiB limit")]
    FrameTooLarge(usize),
    #[error("host daemon reported: {0}")]
    Remote(String),
    #[error("host daemon executable not found: {0}")]
    DaemonExeMissing(PathBuf),
    #[error("host daemon failed to start: {0}")]
    DaemonStartup(String),
    #[error("timed out {0}")]
    Timeout(&'static str),
    #[error("cannot start {program}: {source}")]
    Spawn { program: String, source: io::Error },
    #[error("argument {0:?} cannot be passed safely to a .cmd/.bat program")]
    UnsafeBatchArgument(String),
    #[error("{call} failed: {source}")]
    Os {
        call: &'static str,
        source: io::Error,
    },
    #[error("invalid session spec: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// `Os` error from the calling thread's last OS error.
    pub(crate) fn last_os(call: &'static str) -> Self {
        Error::Os {
            call,
            source: io::Error::last_os_error(),
        }
    }
}
