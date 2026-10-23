//! Blocking client API: start sessions, attach, stream events.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::namespace::{Conn, Namespace, remote_error, valid_session_name};
use crate::protocol::{
    self, AttachParams, ExitedPayload, HostInfo, Kind, MAX_FRAME, Palette, PeekParams,
    ResizeParams, SessionSpec, SpawnRequest,
};
use crate::spawn;
use crate::transport::Stream;

/// Daemon readiness budget after spawning it.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const ATTACH_TIMEOUT: Duration = Duration::from_secs(5);

/// The `omp-deck-host[.exe]` to spawn: `$OMP_DECK_HOST_EXE` if set, else next to the current
/// executable. `None` if that file does not exist.
pub fn daemon_exe() -> Option<PathBuf> {
    let path = match std::env::var_os(crate::HOST_EXE_ENV).filter(|v| !v.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => std::env::current_exe()
            .ok()?
            .with_file_name(format!("omp-deck-host{}", std::env::consts::EXE_SUFFIX)),
    };
    path.is_file().then_some(path)
}

/// Starts `spec` as a detached session (like `tmux new -d`) and waits until it serves.
/// [`Error::AlreadyRunning`] if a session of that name is alive. The PTY starts at
/// `cols`×`rows`.
pub fn start(ns: &Namespace, spec: &SessionSpec, cols: u16, rows: u16) -> Result<()> {
    if ns.has(&spec.name) {
        return Err(Error::AlreadyRunning(spec.name.clone()));
    }
    let request = SpawnRequest {
        spec: spec.clone(),
        cols,
        rows,
        cell_width: 0,
        cell_height: 0,
        palette: Palette::default(),
    };
    spawn_and_wait(ns, &request)
}

/// Attaches to `spec.name` if it is alive (never respawning it), else spawns its daemon
/// detached, waits ≤ 5 s for it to serve, then attaches.
pub fn create_or_attach(
    ns: &Namespace,
    spec: &SessionSpec,
    params: AttachParams,
) -> Result<Attached> {
    match attach(ns, &spec.name, params.clone()) {
        Err(Error::NotRunning(_)) => {}
        other => return other,
    }
    let request = SpawnRequest {
        spec: spec.clone(),
        cols: if params.cols == 0 { 80 } else { params.cols },
        rows: if params.rows == 0 { 24 } else { params.rows },
        cell_width: params.cell_width,
        cell_height: params.cell_height,
        palette: params.palette,
    };
    spawn_and_wait(ns, &request)?;
    attach(ns, &spec.name, params)
}

fn spawn_and_wait(ns: &Namespace, request: &SpawnRequest) -> Result<()> {
    let name = &request.spec.name;
    if !valid_session_name(name) {
        return Err(Error::InvalidSessionName(name.clone()));
    }
    // Writing the spec first also creates/verifies the private endpoint directories (Unix).
    let spec_file = spawn::write_spec(ns, name, &serde_json::to_vec(request)?)?;
    let Some(exe) = daemon_exe() else {
        let _ = std::fs::remove_file(&spec_file);
        return Err(Error::DaemonExeMissing(
            std::env::var_os(crate::HOST_EXE_ENV)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("omp-deck-host")),
        ));
    };
    let log_path = ns.log_path(name);
    let log_start = log_path
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map_or(0, |m| m.len());
    let mut child = match spawn::spawn_daemon(&exe, ns.as_str(), name, &spec_file) {
        Ok(child) => child,
        Err(e) => {
            let _ = std::fs::remove_file(&spec_file);
            return Err(Error::Spawn {
                program: exe.display().to_string(),
                source: e,
            });
        }
    };
    let result = wait_ready(ns, name, &mut child, log_path.as_deref(), log_start);
    if result.is_err() {
        // The daemon deletes the spec once read; clean up if it never got that far.
        let _ = std::fs::remove_file(&spec_file);
    }
    result
}

fn wait_ready(
    ns: &Namespace,
    name: &str,
    child: &mut spawn::Launcher,
    log: Option<&std::path::Path>,
    log_start: u64,
) -> Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut exited = false;
    loop {
        if ns.has(name) {
            // Unix: the launcher exits right after the daemon reports readiness; reap it so
            // no zombie stays behind in the calling (GUI) process. Windows: `child` is the
            // daemon itself and keeps running.
            #[cfg(unix)]
            while !exited && Instant::now() < deadline {
                exited = child.try_wait()?.is_some();
                if !exited {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            return Ok(());
        }
        if !exited && let Some(status) = child.try_wait()? {
            // Unix: the launcher exits 0 once the forked daemon serves. Windows: the daemon
            // itself exited, which is always a failure before it served.
            exited = true;
            // A concurrent client may have started the same session first.
            if (!status.success() || cfg!(windows)) && !ns.has(name) {
                return Err(Error::DaemonStartup(startup_failure(
                    log,
                    log_start,
                    Some(status),
                )));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.try_wait();
            return Err(Error::DaemonStartup(format!(
                "not ready within {} s; {}",
                READY_TIMEOUT.as_secs(),
                startup_failure(log, log_start, None)
            )));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Text the daemon logged since `start`, else a generic status message.
fn startup_failure(
    log: Option<&std::path::Path>,
    start: u64,
    status: Option<std::process::ExitStatus>,
) -> String {
    let logged = log
        .and_then(|p| std::fs::read(p).ok())
        .map(|bytes| {
            // The daemon truncates a large log at startup: then everything is new.
            let from = usize::try_from(start)
                .ok()
                .filter(|&s| s <= bytes.len())
                .unwrap_or(0);
            String::from_utf8_lossy(&bytes[from..]).trim().to_owned()
        })
        .filter(|s| !s.is_empty());
    match (logged, status, log) {
        (Some(text), _, _) => text,
        (None, Some(status), Some(log)) => {
            format!("daemon exited ({status}); log: {}", log.display())
        }
        (None, Some(status), None) => format!("daemon exited ({status})"),
        (None, None, Some(log)) => format!("no log output ({})", log.display()),
        (None, None, None) => "no log output".to_owned(),
    }
}

/// Attaches to a live session; the previously attached client is displaced. The first event is
/// the [`HostEvent::Snapshot`].
pub fn attach(ns: &Namespace, name: &str, params: AttachParams) -> Result<Attached> {
    let mut conn = ns.connect(name, ATTACH_TIMEOUT)?;
    conn.check_protocol()?;
    conn.send_json(Kind::Attach, &params)?;
    conn.stream.set_read_timeout(None)?;
    let Conn {
        stream,
        payload,
        out,
        ..
    } = conn;
    let tx_stream = stream.try_clone()?;
    Ok(Attached {
        rx: AttachedRx {
            stream,
            payload,
            snapshot: Vec::new(),
            closed: false,
        },
        tx: AttachedTx(Arc::new(TxInner {
            conn: Mutex::new(TxConn {
                stream: tx_stream,
                buf: out,
            }),
            done: AtomicBool::new(false),
        })),
    })
}

/// Non-displacing snapshot of a live session (`dump`): its info and a VT replay with up to
/// `history_lines` scrollback rows. Does not resize the session.
pub fn peek(ns: &Namespace, name: &str, history_lines: u32) -> Result<(HostInfo, Vec<u8>)> {
    let mut conn = ns.connect(name, ATTACH_TIMEOUT)?;
    conn.check_protocol()?;
    conn.send(Kind::Info, &[])?;
    conn.send_json(Kind::Peek, &PeekParams { history_lines })?;
    let mut info = None;
    let mut snapshot = Vec::new();
    loop {
        match conn.recv()? {
            Some(Kind::Info) => info = Some(protocol::parse(&conn.payload)?),
            Some(Kind::Snapshot) => snapshot.extend_from_slice(&conn.payload),
            Some(Kind::SnapshotEnd) => break,
            Some(Kind::Error) => return Err(remote_error(&conn.payload)),
            Some(Kind::Exited) | None => return Err(Error::NotRunning(name.to_owned())),
            Some(_) => {}
        }
    }
    let info = info.ok_or_else(|| Error::Protocol("no Info before SnapshotEnd".into()))?;
    Ok((info, snapshot))
}

/// What an attached client receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    /// Whole VT replay of the session state; reset the terminal and feed it.
    Snapshot(Vec<u8>),
    /// Live PTY output.
    Output(Vec<u8>),
    /// The session's program exited (code `None` when killed by a signal).
    Exited(Option<i32>),
    /// Another client attached; this connection is closed.
    Displaced,
    /// The connection ended (detached, daemon gone).
    Closed,
}

/// An attached connection; [`Attached::split`] it for a reader thread and writers.
/// Dropping both halves detaches.
pub struct Attached {
    rx: AttachedRx,
    tx: AttachedTx,
}

impl Attached {
    pub fn split(self) -> (AttachedRx, AttachedTx) {
        (self.rx, self.tx)
    }
}

/// Receiving half (blocking).
pub struct AttachedRx {
    stream: Stream,
    payload: Vec<u8>,
    snapshot: Vec<u8>,
    closed: bool,
}

impl AttachedRx {
    /// Next event. After `Exited`, `Displaced` or `Closed`, the following call returns `Closed`.
    pub fn recv(&mut self) -> Result<HostEvent> {
        loop {
            if self.closed {
                return Ok(HostEvent::Closed);
            }
            let kind = match protocol::read_frame(&mut self.stream, &mut self.payload) {
                Ok(kind) => kind,
                Err(Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    None
                }
                Err(e) => return Err(e),
            };
            match kind {
                None => {
                    self.closed = true;
                    return Ok(HostEvent::Closed);
                }
                Some(Kind::Output) => {
                    return Ok(HostEvent::Output(std::mem::take(&mut self.payload)));
                }
                Some(Kind::Snapshot) => self.snapshot.extend_from_slice(&self.payload),
                Some(Kind::SnapshotEnd) => {
                    return Ok(HostEvent::Snapshot(std::mem::take(&mut self.snapshot)));
                }
                Some(Kind::Exited) => {
                    let exited: ExitedPayload = protocol::parse(&self.payload)?;
                    self.closed = true;
                    return Ok(HostEvent::Exited(exited.code));
                }
                Some(Kind::Displaced) => {
                    self.closed = true;
                    return Ok(HostEvent::Displaced);
                }
                Some(Kind::Error) => return Err(remote_error(&self.payload)),
                Some(_) => {}
            }
        }
    }
}

struct TxConn {
    stream: Stream,
    buf: Vec<u8>,
}

struct TxInner {
    conn: Mutex<TxConn>,
    done: AtomicBool,
}

impl TxInner {
    fn send(&self, kind: Kind, payload: &[u8]) -> Result<()> {
        if self.done.load(Ordering::Acquire) {
            return Err(Error::Io(std::io::ErrorKind::NotConnected.into()));
        }
        let mut conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let TxConn { stream, buf } = &mut *conn;
        protocol::write_frame(stream, buf, kind, payload)
    }

    fn send_json<T: serde::Serialize>(&self, kind: Kind, value: &T) -> Result<()> {
        self.send(kind, &serde_json::to_vec(value)?)
    }

    fn finish(&self, kind: Kind) -> Result<()> {
        let result = self.send(kind, &[]);
        if !self.done.swap(true, Ordering::AcqRel) {
            self.conn
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .stream
                .shutdown();
        }
        result
    }
}

impl Drop for TxInner {
    fn drop(&mut self) {
        if !self.done.load(Ordering::Acquire) {
            let _ = self.finish(Kind::Detach);
        }
    }
}

/// Sending half; clones share the connection. Dropping the last clone detaches.
#[derive(Clone)]
pub struct AttachedTx(Arc<TxInner>);

impl AttachedTx {
    /// Bytes for the session's PTY (split into ≤ 1 MiB frames).
    pub fn input(&self, bytes: &[u8]) -> Result<()> {
        for chunk in bytes.chunks(MAX_FRAME) {
            self.0.send(Kind::Input, chunk)?;
        }
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16, cell_width: u16, cell_height: u16) -> Result<()> {
        self.0.send_json(
            Kind::Resize,
            &ResizeParams {
                cols,
                rows,
                cell_width,
                cell_height,
            },
        )
    }

    /// Colors used to answer OSC 4/10/11/12 queries.
    pub fn palette(&self, palette: &Palette) -> Result<()> {
        self.0.send_json(Kind::Palette, palette)
    }

    /// Kills the session; the receiver gets `Exited` once the daemon confirmed.
    pub fn kill(&self) -> Result<()> {
        self.0.send(Kind::Kill, &[])
    }

    /// Detaches (the session keeps running); the receiver gets `Closed`.
    pub fn detach(&self) -> Result<()> {
        self.0.finish(Kind::Detach)
    }
}
