//! Host namespaces: where a namespace's session endpoints live and the control operations
//! (list/info/has/kill) on them.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::protocol::{self, HostInfo, Kind};
use crate::transport::{self, Endpoint, Stream};

/// Liveness probe budget: endpoint accepts and answers `Hello` within this time.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
/// Control requests (`Info`) wait at most this long for the reply.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
/// `kill` returns once the daemon confirmed or its endpoint vanished, at most after this.
const KILL_TIMEOUT: Duration = Duration::from_secs(5);

/// `[A-Za-z0-9_-]{1,40}`.
pub fn valid_namespace(ns: &str) -> bool {
    valid_ident(ns, 40)
}

/// `[A-Za-z0-9_-]{1,64}`.
pub fn valid_session_name(name: &str) -> bool {
    valid_ident(name, 64)
}

fn valid_ident(s: &str, max: usize) -> bool {
    (1..=max).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A host namespace: `omp-deck` for the app, `omp-deck-test-<unique>` for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Namespace {
    ns: String,
    #[cfg(unix)]
    base: PathBuf,
    /// 16 hex chars of sha256(current user SID string).
    #[cfg(windows)]
    user: String,
}

impl Namespace {
    /// Validates `ns` and resolves the endpoint location for the current user.
    /// Unix: `$XDG_RUNTIME_DIR/omp-deck-host` else `<temp_dir>/omp-deck-host-<uid>`.
    pub fn new(ns: &str) -> Result<Namespace> {
        if !valid_namespace(ns) {
            return Err(Error::InvalidNamespace(ns.to_owned()));
        }
        #[cfg(unix)]
        {
            let base = match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
                Some(dir) => PathBuf::from(dir).join("omp-deck-host"),
                // SAFETY: geteuid has no preconditions and cannot fail.
                None => std::env::temp_dir()
                    .join(format!("omp-deck-host-{}", unsafe { libc::geteuid() })),
            };
            Ok(Namespace {
                ns: ns.to_owned(),
                base,
            })
        }
        #[cfg(windows)]
        {
            Ok(Namespace {
                ns: ns.to_owned(),
                user: transport::user_key()?,
            })
        }
    }

    /// The namespace string.
    pub fn as_str(&self) -> &str {
        &self.ns
    }

    /// Daemon log file of `name`: `<cache_dir>/omp-deck/host/<ns>/<name>.log`.
    pub fn log_path(&self, name: &str) -> Option<PathBuf> {
        Some(
            dirs::cache_dir()?
                .join("omp-deck")
                .join("host")
                .join(&self.ns)
                .join(format!("{name}.log")),
        )
    }

    /// Unix: directory holding this namespace's sockets.
    #[cfg(unix)]
    pub(crate) fn dir(&self) -> PathBuf {
        self.base.join(&self.ns)
    }

    #[cfg(unix)]
    pub(crate) fn base(&self) -> &std::path::Path {
        &self.base
    }

    /// Windows: pipe name prefix `\\.\pipe\omp-deck-host.<user>.<ns>.`.
    #[cfg(windows)]
    pub(crate) fn pipe_prefix(&self) -> String {
        format!(r"\\.\pipe\omp-deck-host.{}.{}.", self.user, self.ns)
    }

    pub(crate) fn endpoint(&self, name: &str) -> Result<Endpoint> {
        Endpoint::new(self, name)
    }

    /// Names of live sessions (endpoint accepts and answers `Hello` within 500 ms), sorted.
    pub fn list(&self) -> Vec<String> {
        let mut names = match transport::list_names(self) {
            Ok(names) => names,
            Err(e) => {
                log::warn!("listing host namespace {}: {e}", self.ns);
                return Vec::new();
            }
        };
        names.retain(|n| valid_session_name(n) && self.has(n));
        names.sort();
        names
    }

    /// Whether `name` is alive.
    pub fn has(&self, name: &str) -> bool {
        self.connect(name, PROBE_TIMEOUT).is_ok()
    }

    /// Session details.
    pub fn info(&self, name: &str) -> Result<HostInfo> {
        let mut conn = self.connect(name, CONTROL_TIMEOUT)?;
        conn.check_protocol()?;
        conn.send(Kind::Info, &[])?;
        loop {
            match conn.recv()? {
                Some(Kind::Info) => return protocol::parse(&conn.payload),
                Some(Kind::Error) => return Err(remote_error(&conn.payload)),
                Some(_) => {}
                None => return Err(Error::NotRunning(name.to_owned())),
            }
        }
    }

    /// Kills the session's process tree. Returns once the daemon confirmed (`Exited`) or its
    /// endpoint vanished, at most after 5 s. A session that is not running is not an error.
    pub fn kill(&self, name: &str) -> Result<()> {
        let deadline = Instant::now() + KILL_TIMEOUT;
        let mut conn = match self.connect(name, KILL_TIMEOUT) {
            Ok(conn) => conn,
            Err(Error::NotRunning(_)) => return Ok(()),
            Err(e) => return Err(e),
        };
        // Kill works across protocol versions: Hello and Kill are frozen.
        conn.send(Kind::Kill, &[])?;
        loop {
            match conn.recv() {
                Ok(Some(Kind::Exited)) | Ok(None) | Err(_) => break,
                Ok(Some(_)) => {}
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        drop(conn);
        while Instant::now() < deadline {
            if transport::connect(&self.endpoint(name)?).is_err() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(Error::Timeout(
            "waiting for the killed session's endpoint to vanish",
        ))
    }

    /// Connects and exchanges `Hello` (reply read within `timeout`). The read timeout stays
    /// set; callers that stream clear it.
    pub(crate) fn connect(&self, name: &str, timeout: Duration) -> Result<Conn> {
        let endpoint = self.endpoint(name)?;
        let stream = transport::connect(&endpoint)?;
        stream.set_read_timeout(Some(timeout))?;
        let mut conn = Conn {
            stream,
            payload: Vec::new(),
            out: Vec::new(),
            peer: None,
        };
        conn.send_json(Kind::Hello, &protocol::Hello::ours())?;
        match conn.recv() {
            Ok(Some(Kind::Hello)) => {
                conn.peer = Some(protocol::parse(&conn.payload)?);
                Ok(conn)
            }
            Ok(Some(kind)) => Err(Error::Protocol(format!("expected Hello, got {kind:?}"))),
            Ok(None) => Err(Error::NotRunning(name.to_owned())),
            Err(Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Err(Error::Timeout("waiting for the session daemon's Hello"))
            }
            Err(e) => Err(e),
        }
    }
}

pub(crate) fn remote_error(payload: &[u8]) -> Error {
    Error::Remote(
        protocol::parse::<String>(payload)
            .unwrap_or_else(|_| String::from_utf8_lossy(payload).into_owned()),
    )
}

/// A client connection after the `Hello` exchange.
pub(crate) struct Conn {
    pub stream: Stream,
    pub payload: Vec<u8>,
    pub out: Vec<u8>,
    pub peer: Option<protocol::Hello>,
}

impl Conn {
    pub fn check_protocol(&self) -> Result<()> {
        match &self.peer {
            Some(p) if p.protocol != protocol::PROTOCOL => Err(Error::ProtocolMismatch {
                ours: protocol::PROTOCOL,
                theirs: p.protocol,
                host_version: p.version.clone(),
            }),
            _ => Ok(()),
        }
    }

    pub fn send(&mut self, kind: Kind, payload: &[u8]) -> Result<()> {
        protocol::write_frame(&mut self.stream, &mut self.out, kind, payload)
    }

    pub fn send_json<T: serde::Serialize>(&mut self, kind: Kind, value: &T) -> Result<()> {
        protocol::write_json(&mut self.stream, &mut self.out, kind, value)
    }

    pub fn recv(&mut self) -> Result<Option<Kind>> {
        protocol::read_frame(&mut self.stream, &mut self.payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_rules() {
        assert!(valid_namespace("omp-deck"));
        assert!(valid_namespace(&"a".repeat(40)));
        assert!(!valid_namespace(&"a".repeat(41)));
        assert!(!valid_namespace(""));
        assert!(!valid_namespace("a.b"));
        assert!(!valid_namespace("a/b"));
        assert!(valid_session_name(&"Z_9-".repeat(16)));
        assert!(!valid_session_name(&"a".repeat(65)));
        assert!(!valid_session_name("x y"));
        assert!(matches!(
            Namespace::new("bad ns"),
            Err(Error::InvalidNamespace(_))
        ));
    }
}
