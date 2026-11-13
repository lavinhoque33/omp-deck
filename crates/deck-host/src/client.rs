//! Blocking client API: start sessions, attach (controller), observe (read-only), stream
//! events.

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::namespace::{Conn, Namespace, remote_error, valid_session_name};
use crate::protocol::{
    self, AttachParams, BLOB_ID_LEN, BlobHead, BlobRequest, ExitedPayload, Grid, HostInfo, Kind,
    MAX_FRAME, ObserveParams, Palette, PeekParams, ResizeParams, SessionSpec, SpawnRequest,
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
    let blob_fetch = daemon_blob_fetch(&mut conn, name)?;
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
            blobs: HashMap::new(),
            closed: false,
        },
        tx: AttachedTx(
            Arc::new(TxInner {
                conn: Mutex::new(TxConn {
                    stream: tx_stream,
                    buf: out,
                }),
                done: AtomicBool::new(false),
            }),
            blob_fetch,
        ),
    })
}

/// Whether the daemon behind `conn` answers `BlobGet` (asked before attaching, so the answer
/// is known before the first snapshot; a daemon that predates it closes the connection on an
/// unknown frame kind).
fn daemon_blob_fetch(conn: &mut Conn, name: &str) -> Result<bool> {
    conn.send(Kind::Info, &[])?;
    loop {
        match conn.recv()? {
            Some(Kind::Info) => {
                let info: HostInfo = protocol::parse(&conn.payload)?;
                return Ok(info.blob_fetch);
            }
            Some(Kind::Error) => return Err(remote_error(&conn.payload)),
            None => return Err(Error::NotRunning(name.to_owned())),
            Some(_) => {}
        }
    }
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

/// The TSP document of a `SessionSpec::tsp` session as JSON (`deck_tsp::document::Document`'s
/// serialization), without attaching. Fails with a remote error for a non-TSP session.
pub fn tsp_peek(ns: &Namespace, name: &str) -> Result<serde_json::Value> {
    let mut conn = ns.connect(name, ATTACH_TIMEOUT)?;
    conn.check_protocol()?;
    conn.send(Kind::TspPeek, &[])?;
    let mut document = Vec::new();
    loop {
        match conn.recv()? {
            Some(Kind::Snapshot) => document.extend_from_slice(&conn.payload),
            Some(Kind::SnapshotEnd) => break,
            Some(Kind::Error) => return Err(remote_error(&conn.payload)),
            Some(Kind::Exited) | None => return Err(Error::NotRunning(name.to_owned())),
            Some(_) => {}
        }
    }
    serde_json::from_slice(&document)
        .map_err(|e| Error::Protocol(format!("TSP document is not JSON: {e}")))
}

/// What an attached client receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    /// Whole VT replay of the session state; reset the terminal and feed it.
    Snapshot(Vec<u8>),
    /// Live PTY output.
    Output(Vec<u8>),
    /// The answer to [`AttachedTx::blob`]: the blob (`None`: the host holds no such blob).
    Blob {
        id: String,
        mime: Option<String>,
        bytes: Option<Vec<u8>>,
    },
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
    /// Blobs whose `BlobData` frames are still arriving, by id.
    blobs: HashMap<String, PartialBlob>,
    closed: bool,
}

impl AttachedRx {
    /// Next event. After `Exited`, `Displaced` or `Closed`, the following call returns `Closed`.
    pub fn recv(&mut self) -> Result<HostEvent> {
        self.recv_inner(None)
    }

    /// Next event within one total budget, including partial frames and snapshot chunks.
    ///
    /// On timeout returns [`Error::Timeout`] and aborts the connection (both halves).
    /// Any other read/protocol error also aborts it: a partially consumed frame cannot
    /// safely be resumed. Subsequent reads return [`HostEvent::Closed`]. Success restores
    /// ordinary blocking reads. OS timer granularity and I/O cancellation can delay return
    /// slightly beyond the budget; this is not a hard real-time deadline.
    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<HostEvent> {
        if self.closed {
            return Ok(HostEvent::Closed);
        }
        let budget = ReadBudget {
            started: Instant::now(),
            timeout,
        };
        let result = self.recv_inner(Some(&budget));
        let result = match result {
            Err(Error::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                Err(Error::Timeout("receiving host event"))
            }
            other => other,
        };
        let result = result.and_then(|event| {
            self.stream.set_read_timeout(None)?;
            Ok(event)
        });
        if result.is_err() {
            self.closed = true;
            self.stream.abort();
        }
        result
    }

    fn recv_inner(&mut self, budget: Option<&ReadBudget>) -> Result<HostEvent> {
        loop {
            if self.closed {
                return Ok(HostEvent::Closed);
            }
            let kind = match budget {
                Some(budget) => read_event_frame(
                    &mut DeadlineReader {
                        stream: &mut self.stream,
                        budget,
                    },
                    &mut self.payload,
                )?,
                None => read_event_frame(&mut self.stream, &mut self.payload)?,
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
                Some(Kind::BlobHead) => {
                    if let Some(event) = self.blob_head()? {
                        return Ok(event);
                    }
                }
                Some(Kind::BlobData) => {
                    if let Some(event) = self.blob_data()? {
                        return Ok(event);
                    }
                }
                Some(Kind::Error) => return Err(remote_error(&self.payload)),
                Some(_) => {}
            }
        }
    }
}

/// A blob whose `BlobData` frames are still arriving.
struct PartialBlob {
    mime: Option<String>,
    len: usize,
    bytes: Vec<u8>,
}

/// Blobs that may be arriving at once on one connection.
const MAX_PARTIAL_BLOBS: usize = 8;

impl AttachedRx {
    /// `BlobHead`: an absent or empty blob completes now, any other starts collecting.
    fn blob_head(&mut self) -> Result<Option<HostEvent>> {
        let head: BlobHead = protocol::parse(&self.payload)?;
        let BlobHead { id, mime, len } = head;
        let Some(len) = len else {
            return Ok(Some(HostEvent::Blob {
                id,
                mime: None,
                bytes: None,
            }));
        };
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        if len > deck_tsp::document::MAX_BLOB
            || (self.blobs.len() >= MAX_PARTIAL_BLOBS && !self.blobs.contains_key(&id))
        {
            return Err(Error::Protocol(format!(
                "blob answer of {len} bytes refused"
            )));
        }
        if len == 0 {
            return Ok(Some(HostEvent::Blob {
                id,
                mime,
                bytes: Some(Vec::new()),
            }));
        }
        let bytes = Vec::with_capacity(len.min(MAX_FRAME));
        self.blobs.insert(id, PartialBlob { mime, len, bytes });
        Ok(None)
    }

    /// `BlobData`: appends to its blob; the last chunk completes it.
    fn blob_data(&mut self) -> Result<Option<HostEvent>> {
        let (id, chunk) = self
            .payload
            .split_at_checked(BLOB_ID_LEN)
            .ok_or_else(|| Error::Protocol("short BlobData frame".into()))?;
        let id = std::str::from_utf8(id)
            .map_err(|_| Error::Protocol("BlobData id is not text".into()))?;
        let Some(part) = self.blobs.get_mut(id) else {
            return Ok(None);
        };
        if part.bytes.len() + chunk.len() > part.len {
            return Err(Error::Protocol("BlobData past the announced length".into()));
        }
        part.bytes.extend_from_slice(chunk);
        if part.bytes.len() < part.len {
            return Ok(None);
        }
        let id = id.to_owned();
        Ok(self.blobs.remove(&id).map(|p| HostEvent::Blob {
            id,
            mime: p.mime,
            bytes: Some(p.bytes),
        }))
    }
}

/// Shared by every underlying read, not renewed at frame or snapshot chunk boundaries.
struct ReadBudget {
    started: Instant,
    timeout: Duration,
}

struct DeadlineReader<'a> {
    stream: &'a mut Stream,
    budget: &'a ReadBudget,
}

impl Read for DeadlineReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self
            .budget
            .timeout
            .checked_sub(self.budget.started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
        // Keep Windows' millisecond timeout below its INFINITE sentinel even for
        // caller-supplied budgets of many days.
        let remaining = remaining.min(Duration::from_millis(u64::from(u32::MAX - 1)));
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(buf)
    }
}

/// Reads one frame of an event stream; a connection reset or broken pipe counts as the end.
fn read_event_frame(stream: &mut impl Read, payload: &mut Vec<u8>) -> Result<Option<Kind>> {
    match protocol::read_frame(stream, payload) {
        Err(Error::Io(e))
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionAborted
            ) =>
        {
            Ok(None)
        }
        other => other,
    }
}

/// Subscribes read-only to a live session's terminal stream without affecting it: no
/// displacement of the attached controller, no resize, no palette change, and
/// [`HostInfo::attached`] keeps reporting the controller. Returns the session's info (its
/// `cols`/`rows` equal the first snapshot's grid) and the subscription; its first event is
/// [`ObserverEvent::Snapshot`] with up to `history_lines` scrollback rows.
///
/// Read-only is enforced by the daemon, not just by this API (which offers no way to write):
/// an observer connection that sends input, resize, palette, attach or kill is disconnected
/// immediately and nothing reaches the session. Killing a session stays [`Namespace::kill`].
///
/// Errors: [`Error::Unsupported`] when the session's daemon predates observing (it closed
/// the connection on the unknown request; nothing changed — do not fall back to
/// [`attach`], which would displace the controller), [`Error::NotRunning`] when the session
/// is gone or ending, [`Error::ProtocolMismatch`] as for [`attach`].
pub fn observe(ns: &Namespace, name: &str, history_lines: u32) -> Result<(HostInfo, Observed)> {
    let mut conn = ns.connect(name, ATTACH_TIMEOUT)?;
    conn.check_protocol()?;
    conn.send_json(Kind::Observe, &ObserveParams { history_lines })?;
    let info: HostInfo = match read_event_frame(&mut conn.stream, &mut conn.payload) {
        Ok(Some(Kind::Info)) => protocol::parse(&conn.payload)?,
        Ok(Some(Kind::Error)) => return Err(remote_error(&conn.payload)),
        Ok(Some(Kind::Exited)) => return Err(Error::NotRunning(name.to_owned())),
        Ok(Some(kind)) => {
            return Err(Error::Protocol(format!(
                "expected Info after Observe, got {kind:?}"
            )));
        }
        // Our Hello was answered, so the daemon dropped the request itself: an older daemon
        // rejects the unknown kind that way. Unless the session just ended.
        Ok(None) if ns.has(name) => return Err(Error::Unsupported(name.to_owned())),
        Ok(None) => return Err(Error::NotRunning(name.to_owned())),
        Err(Error::Io(e))
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            return Err(Error::Timeout(
                "waiting for the session daemon's Observe reply",
            ));
        }
        Err(e) => return Err(e),
    };
    conn.stream.set_read_timeout(None)?;
    let Conn {
        stream,
        payload,
        out,
        ..
    } = conn;
    let tx_stream = stream.try_clone()?;
    let observed = Observed {
        rx: ObserverRx {
            stream,
            payload,
            snapshot: Vec::new(),
            grid: None,
            started: false,
            closed: false,
        },
        tx: ObserverTx(Arc::new(TxInner {
            conn: Mutex::new(TxConn {
                stream: tx_stream,
                buf: out,
            }),
            done: AtomicBool::new(false),
        })),
    };
    Ok((info, observed))
}

/// What an observer receives, in session order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObserverEvent {
    /// Always first: whole VT replay of the session state at `grid`; size the terminal to
    /// `grid`, reset it and feed `bytes`.
    Snapshot { bytes: Vec<u8>, grid: Grid },
    /// Live PTY output.
    Output(Vec<u8>),
    /// The session's grid changed (controller resize or a new controller's attach); applies
    /// to all output after it. Observers render at the host grid, they cannot change it.
    Resized(Grid),
    /// The session's program exited (code `None` when killed by a signal).
    Exited(Option<i32>),
    /// The subscription ended: detached, daemon gone, or disconnected by the daemon for
    /// falling behind (observe again for a fresh snapshot).
    Closed,
}

/// A read-only subscription from [`observe`]; [`Observed::split`] it for a reader thread
/// and a detach handle. Dropping both halves detaches.
pub struct Observed {
    rx: ObserverRx,
    tx: ObserverTx,
}

impl Observed {
    pub fn split(self) -> (ObserverRx, ObserverTx) {
        (self.rx, self.tx)
    }
}

/// Receiving half of an observation (blocking).
pub struct ObserverRx {
    stream: Stream,
    payload: Vec<u8>,
    snapshot: Vec<u8>,
    /// Grid announced before the snapshot.
    grid: Option<Grid>,
    /// The snapshot was delivered; later `Resized` frames are events.
    started: bool,
    closed: bool,
}

impl ObserverRx {
    /// Next event. After `Exited` or `Closed`, the following call returns `Closed`.
    pub fn recv(&mut self) -> Result<ObserverEvent> {
        loop {
            if self.closed {
                return Ok(ObserverEvent::Closed);
            }
            let kind = match read_event_frame(&mut self.stream, &mut self.payload) {
                Ok(kind) => kind,
                // The daemon cut this observer off mid-frame (fell behind): a normal end.
                Err(e) if protocol::is_truncated(&e) => None,
                Err(e) => return Err(e),
            };
            match kind {
                // `Displaced` is never sent to observers; treat it as the end regardless.
                None | Some(Kind::Displaced) => {
                    self.snapshot = Vec::new();
                    self.closed = true;
                    return Ok(ObserverEvent::Closed);
                }
                Some(Kind::Output) => {
                    return Ok(ObserverEvent::Output(std::mem::take(&mut self.payload)));
                }
                Some(Kind::Resized) => {
                    let grid: Grid = protocol::parse(&self.payload)?;
                    if self.started {
                        return Ok(ObserverEvent::Resized(grid));
                    }
                    self.grid = Some(grid);
                }
                Some(Kind::Snapshot) => self.snapshot.extend_from_slice(&self.payload),
                Some(Kind::SnapshotEnd) => {
                    let grid = self
                        .grid
                        .ok_or_else(|| Error::Protocol("no grid before the snapshot".into()))?;
                    self.started = true;
                    return Ok(ObserverEvent::Snapshot {
                        bytes: std::mem::take(&mut self.snapshot),
                        grid,
                    });
                }
                Some(Kind::Exited) => {
                    let exited: ExitedPayload = protocol::parse(&self.payload)?;
                    self.closed = true;
                    return Ok(ObserverEvent::Exited(exited.code));
                }
                Some(Kind::Error) => {
                    self.closed = true;
                    return Err(remote_error(&self.payload));
                }
                Some(_) => {}
            }
        }
    }
}

/// Detach handle of an observation; clones share it. It deliberately has no input, resize,
/// palette or kill operation. Dropping the last clone detaches.
#[derive(Clone)]
pub struct ObserverTx(Arc<TxInner>);

impl ObserverTx {
    /// Ends the subscription (the session is unaffected) and closes the connection, so a
    /// [`ObserverRx::recv`] blocked on another thread returns `Closed` promptly.
    pub fn detach(&self) -> Result<()> {
        self.0.finish(Kind::Detach)
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
pub struct AttachedTx(Arc<TxInner>, bool);

impl AttachedTx {
    /// The daemon answers [`AttachedTx::blob`] (older daemons do not, and drop a connection
    /// that asks).
    pub fn blob_fetch(&self) -> bool {
        self.1
    }

    /// Asks the daemon for the blob `id` (lowercase hex SHA-256) of the session's TSP
    /// document. The answer arrives as [`HostEvent::Blob`]. Fails with `Unsupported` I/O when
    /// the daemon cannot answer.
    pub fn blob(&self, id: &str) -> Result<()> {
        if !self.1 {
            return Err(Error::Io(std::io::ErrorKind::Unsupported.into()));
        }
        self.0
            .send_json(Kind::BlobGet, &BlobRequest { id: id.to_owned() })
    }

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

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::mpsc;
    use std::thread;

    use super::*;
    use crate::transport::{self, Listener};

    fn connection() -> (AttachedRx, Stream) {
        let mut nonce = [0; 4];
        getrandom::fill(&mut nonce).unwrap();
        let ns = Namespace::new(&format!(
            "omp-deck-test-{}-{:08x}",
            std::process::id(),
            u32::from_ne_bytes(nonce)
        ))
        .unwrap();
        let endpoint = ns.endpoint("recv").unwrap();
        let listener = Listener::bind(&endpoint).unwrap();
        let accept = thread::spawn(move || {
            let stream = listener.accept().unwrap();
            listener.remove();
            stream
        });
        let stream = transport::connect(&endpoint).unwrap();
        let peer = accept.join().unwrap();
        #[cfg(unix)]
        std::fs::remove_dir_all(ns.dir()).unwrap();
        (
            AttachedRx {
                stream,
                payload: Vec::new(),
                snapshot: Vec::new(),
                blobs: HashMap::new(),
                closed: false,
            },
            peer,
        )
    }

    fn frame(kind: Kind, bytes: &[u8]) -> Vec<u8> {
        let mut encoded = Vec::new();
        protocol::write_frame(&mut encoded, &mut Vec::new(), kind, bytes).unwrap();
        encoded
    }

    #[test]
    fn timeout_aborts_silent_and_partially_consumed_connections() {
        let encoded = frame(Kind::Snapshot, b"snapshot");
        // No header, partial header, and partial payload.
        for prefix in [0, 2, 7] {
            let (mut rx, mut peer) = connection();
            let mut writer = rx.stream.try_clone().unwrap();
            peer.write_all(&encoded[..prefix]).unwrap();
            assert!(matches!(
                rx.recv_timeout(Duration::from_millis(30)),
                Err(Error::Timeout("receiving host event"))
            ));
            assert_eq!(rx.recv().unwrap(), HostEvent::Closed);
            assert_eq!(rx.recv_timeout(Duration::ZERO).unwrap(), HostEvent::Closed);
            assert!(writer.write_all(b"input").is_err());
            peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        }
    }

    #[test]
    fn zero_budget_closes_without_reading() {
        let (mut rx, mut peer) = connection();
        peer.write_all(&frame(Kind::SnapshotEnd, b"")).unwrap();
        assert!(matches!(
            rx.recv_timeout(Duration::ZERO),
            Err(Error::Timeout(_))
        ));
        assert_eq!(rx.recv().unwrap(), HostEvent::Closed);
    }

    #[test]
    fn deadline_is_not_renewed_by_trickling_bytes_or_snapshot_chunks() {
        for chunks in [false, true] {
            let (mut rx, mut peer) = connection();
            let (started_tx, started_rx) = mpsc::channel();
            let sender = thread::spawn(move || {
                // Each read/frame fits inside the budget, but the complete event does not.
                let bytes = frame(Kind::Snapshot, &[b'x'; 100]);
                started_tx.send(()).unwrap();
                for i in 0..100 {
                    let bytes = if chunks {
                        frame(Kind::Snapshot, b"x")
                    } else {
                        vec![bytes[i]]
                    };
                    if peer.write_all(&bytes).is_err() {
                        return;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                if !chunks && peer.write_all(&bytes[100..]).is_err() {
                    return;
                }
                let _ = peer.write_all(&frame(Kind::SnapshotEnd, b""));
            });
            started_rx.recv().unwrap();
            assert!(matches!(
                rx.recv_timeout(Duration::from_millis(100)),
                Err(Error::Timeout(_))
            ));
            assert_eq!(rx.recv().unwrap(), HostEvent::Closed);
            sender.join().unwrap();
        }
    }

    #[test]
    fn complete_snapshot_restores_blocking_recv() {
        let (mut rx, mut peer) = connection();
        peer.write_all(&frame(Kind::Snapshot, b"first ")).unwrap();
        peer.write_all(&frame(Kind::Snapshot, b"snapshot")).unwrap();
        peer.write_all(&frame(Kind::SnapshotEnd, b"")).unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(100)).unwrap(),
            HostEvent::Snapshot(b"first snapshot".to_vec())
        );
        let sender = thread::spawn(move || {
            // Exceed the old budget: a leaked transport timeout would fail this read.
            thread::sleep(Duration::from_millis(200));
            peer.write_all(&frame(Kind::Output, b"live")).unwrap();
            peer.write_all(&frame(Kind::Displaced, b"")).unwrap();
        });
        assert_eq!(rx.recv().unwrap(), HostEvent::Output(b"live".to_vec()));
        assert_eq!(rx.recv().unwrap(), HostEvent::Displaced);
        assert_eq!(rx.recv().unwrap(), HostEvent::Closed);
        sender.join().unwrap();
    }

    #[test]
    fn frame_reads_share_the_original_budget() {
        let (mut rx, mut peer) = connection();
        peer.write_all(&frame(Kind::Snapshot, b"chunk")).unwrap();
        peer.write_all(&frame(Kind::SnapshotEnd, b"")).unwrap();
        let mut budget = ReadBudget {
            started: Instant::now(),
            timeout: Duration::from_secs(60),
        };
        assert_eq!(
            read_event_frame(
                &mut DeadlineReader {
                    stream: &mut rx.stream,
                    budget: &budget,
                },
                &mut rx.payload,
            )
            .unwrap(),
            Some(Kind::Snapshot)
        );
        // Expire the same budget without sleeping or depending on thread scheduling.
        budget.started -= Duration::from_secs(120);
        let result = read_event_frame(
            &mut DeadlineReader {
                stream: &mut rx.stream,
                budget: &budget,
            },
            &mut rx.payload,
        );
        assert!(matches!(result, Err(Error::Io(e)) if e.kind() == io::ErrorKind::TimedOut));
    }
}
