//! deck-term's [`Remote`] over an OmpDeck session host connection.
//!
//! The connection is made lazily on the terminal's reader thread, so opening a tab never blocks
//! the UI: the terminal exists at once, shows "Starting session…", and the first real event is
//! the host's snapshot. A failure is printed into the terminal (with the daemon's log path)
//! followed by an exit. What the terminal reports meanwhile (grid size, palette, a detach or
//! kill request) is remembered and applied once the connection is up.

use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use deck_host::{
    AttachParams, AttachedRx, AttachedTx, HostEvent, HostInfo, Namespace, Observed, ObserverEvent,
    ObserverRx, Palette, SessionSpec, create_or_attach,
};
use deck_term::{Observer, ObserverTx, Remote, RemoteEvent, RemoteRx, RemoteTx};

/// Scrollback rows a snapshot carries (the terminal's own scrollback is 10 000 lines).
const HISTORY_LINES: u32 = 10_000;
/// How long a fresh connection waits for the terminal's first grid size before attaching
/// with the last one known (a tab that is not on screen never reports one).
const GRID_WAIT: Duration = Duration::from_millis(400);
/// First-event confirmation is bounded independently of the host client's connection timeout.
const FIRST_EVENT_TIMEOUT: Duration = Duration::from_secs(10);
/// New readers fail rather than bypass an attachment whose confirmation is still pending.
const ATTACH_QUEUE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Default)]
struct AttachGate {
    busy: Mutex<bool>,
    available: Condvar,
}

struct AttachPermit(Arc<AttachGate>);

impl AttachGate {
    fn enter(self: &Arc<Self>, timeout: Duration) -> deck_host::Result<AttachPermit> {
        let (mut busy, _) = self
            .available
            .wait_timeout_while(
                self.busy.lock().unwrap_or_else(PoisonError::into_inner),
                timeout,
                |busy| *busy,
            )
            .unwrap_or_else(PoisonError::into_inner);
        if *busy {
            return Err(deck_host::Error::Timeout(
                "waiting for an earlier host attachment",
            ));
        }
        *busy = true;
        Ok(AttachPermit(Arc::clone(self)))
    }
}

impl Drop for AttachPermit {
    fn drop(&mut self) {
        *self.0.busy.lock().unwrap_or_else(PoisonError::into_inner) = false;
        self.0.available.notify_one();
    }
}

struct AttachEntry {
    ns: Namespace,
    name: String,
    gate: Weak<AttachGate>,
}

static ATTACH_GATES: Mutex<Vec<AttachEntry>> = Mutex::new(Vec::new());

fn attach_gate(ns: &Namespace, name: &str) -> Arc<AttachGate> {
    let mut entries = ATTACH_GATES.lock().unwrap_or_else(PoisonError::into_inner);
    entries.retain(|entry| entry.gate.strong_count() != 0);
    if let Some(gate) = entries.iter().find_map(|entry| {
        (entry.ns == *ns && entry.name == name)
            .then(|| entry.gate.upgrade())
            .flatten()
    }) {
        return gate;
    }
    let gate = Arc::new(AttachGate::default());
    entries.push(AttachEntry {
        ns: ns.clone(),
        name: name.to_owned(),
        gate: Arc::downgrade(&gate),
    });
    gate
}

const STARTING: &[u8] = b"\x1b[2mStarting session\xe2\x80\xa6\x1b[0m";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Grid {
    cols: u16,
    rows: u16,
    cell_width: u16,
    cell_height: u16,
}

/// The size of the most recently resized terminal: a new tab attaches with it (0x0 = keep the
/// session's size / the host's default) instead of flashing at the wrong size.
static LAST_GRID: Mutex<Option<Grid>> = Mutex::new(None);

fn last_grid() -> Option<Grid> {
    *LAST_GRID.lock().unwrap_or_else(PoisonError::into_inner)
}

fn remember_grid(grid: Grid) {
    *LAST_GRID.lock().unwrap_or_else(PoisonError::into_inner) = Some(grid);
}

/// What the terminal asked for before the connection was up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Request {
    None,
    Detach,
    Kill,
}

enum Phase {
    Connecting {
        grid: Option<Grid>,
        palette: Option<Palette>,
        request: Request,
    },
    Ready(AttachedTx),
    /// Failed, detached or over: every call fails.
    Dead,
}

struct Link {
    phase: Mutex<Phase>,
    changed: Condvar,
}

impl Link {
    fn lock(&self) -> MutexGuard<'_, Phase> {
        self.phase.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A session to create or attach to, as a deck-term [`Remote`].
pub struct HostRemote {
    ns: Namespace,
    spec: SessionSpec,
}

impl HostRemote {
    pub fn new(ns: Namespace, spec: SessionSpec) -> HostRemote {
        HostRemote { ns, spec }
    }
}

impl Remote for HostRemote {
    fn split(self: Box<Self>) -> (Box<dyn RemoteRx>, Box<dyn RemoteTx>) {
        let link = Arc::new(Link {
            phase: Mutex::new(Phase::Connecting {
                grid: None,
                palette: None,
                request: Request::None,
            }),
            changed: Condvar::new(),
        });
        let HostRemote { ns, spec } = *self;
        let rx = HostRx {
            link: Arc::clone(&link),
            state: RxState::Banner,
            ns,
            spec,
        };
        (Box::new(rx), Box::new(HostTx { link }))
    }
}

enum RxState {
    /// Nothing delivered yet.
    Banner,
    /// The banner is out; the next call connects.
    Connect,
    Live(AttachedRx),
    /// An error text was delivered; the exit follows.
    Failed,
    Over,
}

struct HostRx {
    link: Arc<Link>,
    state: RxState,
    ns: Namespace,
    spec: SessionSpec,
}

impl HostRx {
    /// Waits briefly for the terminal's first grid size, then establishes the accepted backend.
    /// A tab closed before connecting creates only a detached backend, never a controller.
    fn connect(
        &mut self,
        first_timeout: Duration,
    ) -> Option<Result<(AttachedRx, HostEvent), deck_host::Error>> {
        {
            let mut phase = self.link.lock();
            if last_grid().is_none() {
                phase = self
                    .link
                    .changed
                    .wait_timeout_while(phase, GRID_WAIT, |p| {
                        matches!(
                            p,
                            Phase::Connecting {
                                grid: None,
                                request: Request::None,
                                ..
                            }
                        )
                    })
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
            if matches!(&*phase, Phase::Dead | Phase::Ready(_)) {
                return None;
            }
        }
        let gate = attach_gate(&self.ns, &self.spec.name);
        let _permit = match gate.enter(ATTACH_QUEUE_TIMEOUT) {
            Ok(permit) => permit,
            Err(error) => {
                *self.link.lock() = Phase::Dead;
                self.link.changed.notify_all();
                return Some(Err(error));
            }
        };
        // Close/resize may have arrived while an earlier attachment held the gate.
        let (grid, palette, request) = match &*self.link.lock() {
            Phase::Connecting {
                grid,
                palette,
                request,
            } => (*grid, *palette, *request),
            Phase::Ready(_) | Phase::Dead => return None,
        };
        let size = grid.or_else(last_grid);
        let params = AttachParams {
            cols: size.map_or(0, |g| g.cols),
            rows: size.map_or(0, |g| g.rows),
            cell_width: size.map_or(0, |g| g.cell_width),
            cell_height: size.map_or(0, |g| g.cell_height),
            history_lines: HISTORY_LINES,
            palette: palette.unwrap_or_default(),
        };
        if request == Request::Detach {
            // A reopened tab may already control this session. Even a momentary attach would
            // displace it, so preserve an accepted launch using the creation-only API.
            let result = deck_host::start(
                &self.ns,
                &self.spec,
                if params.cols == 0 { 80 } else { params.cols },
                if params.rows == 0 { 24 } else { params.rows },
            );
            let mut phase = self.link.lock();
            if matches!(
                &*phase,
                Phase::Connecting {
                    request: Request::Kill,
                    ..
                }
            ) {
                let _ = self.ns.kill(&self.spec.name);
            }
            *phase = Phase::Dead;
            self.link.changed.notify_all();
            return match result {
                Ok(()) | Err(deck_host::Error::AlreadyRunning(_)) => None,
                Err(e) => Some(Err(e)),
            };
        }
        let attached = match create_or_attach(&self.ns, &self.spec, params.clone()) {
            Ok(attached) => attached,
            Err(e) => {
                *self.link.lock() = Phase::Dead;
                self.link.changed.notify_all();
                return Some(Err(e));
            }
        };
        let (mut rx, tx) = attached.split();
        // Writing Attach is not an acknowledgment: another connection can overtake it.
        // Keep the gate until the host confirms this attachment has actually been applied.
        let first = match rx.recv_timeout(first_timeout) {
            Ok(first @ (HostEvent::Snapshot(_) | HostEvent::Exited(_))) => first,
            result => {
                let _ = tx.detach();
                *self.link.lock() = Phase::Dead;
                self.link.changed.notify_all();
                return Some(Err(match result {
                    Err(error) => error,
                    Ok(other) => deck_host::Error::Protocol(format!(
                        "host attachment did not begin with a snapshot: {other:?}"
                    )),
                }));
            }
        };
        let mut phase = self.link.lock();
        let (grid, palette, request) = match &*phase {
            Phase::Connecting {
                grid,
                palette,
                request,
            } => (*grid, *palette, *request),
            Phase::Ready(_) | Phase::Dead => (None, None, Request::Detach),
        };
        match request {
            Request::Detach => {
                // Confirmation arrived: detach before a reopened tab can take its turn.
                *phase = Phase::Dead;
                self.link.changed.notify_all();
                let _ = tx.detach();
                return None;
            }
            Request::Kill => {
                let _ = tx.kill();
            }
            Request::None => {}
        }
        // Whatever changed while the attach was in flight.
        if let Some(g) = grid.filter(|g| Some(*g) != size) {
            let _ = tx.resize(g.cols, g.rows, g.cell_width, g.cell_height);
        }
        if let Some(p) = palette.filter(|p| *p != params.palette) {
            let _ = tx.palette(&p);
        }
        *phase = Phase::Ready(tx);
        self.link.changed.notify_all();
        Some(Ok((rx, first)))
    }
}

impl RemoteRx for HostRx {
    fn recv(&mut self) -> io::Result<RemoteEvent> {
        loop {
            match &mut self.state {
                RxState::Banner => {
                    self.state = RxState::Connect;
                    return Ok(RemoteEvent::Output(STARTING.to_vec()));
                }
                RxState::Connect => match self.connect(FIRST_EVENT_TIMEOUT) {
                    None => self.state = RxState::Over,
                    Some(Ok((rx, first))) => {
                        self.state = RxState::Live(rx);
                        return Ok(event(first));
                    }
                    Some(Err(e)) => {
                        let text = failure_text(&e, self.ns.log_path(&self.spec.name).as_deref());
                        self.state = RxState::Failed;
                        return Ok(RemoteEvent::Output(text));
                    }
                },
                RxState::Live(rx) => {
                    return rx.recv().map(event).map_err(io::Error::other);
                }
                RxState::Failed => {
                    self.state = RxState::Over;
                    return Ok(RemoteEvent::Exited(None));
                }
                RxState::Over => return Ok(RemoteEvent::Closed),
            }
        }
    }
}

fn event(e: HostEvent) -> RemoteEvent {
    match e {
        HostEvent::Snapshot(b) => RemoteEvent::Snapshot(b),
        HostEvent::Output(b) => RemoteEvent::Output(b),
        HostEvent::Blob { id, mime, bytes } => RemoteEvent::Blob { id, mime, bytes },
        HostEvent::Exited(code) => RemoteEvent::Exited(code),
        HostEvent::Displaced => RemoteEvent::Displaced,
        HostEvent::Closed => RemoteEvent::Closed,
    }
}

/// The terminal text for a session that could not be started: the reason (the daemon's own
/// log lines when it died at startup) and where its log is.
fn failure_text(error: &deck_host::Error, log: Option<&std::path::Path>) -> Vec<u8> {
    let mut text = String::from("\r\n\x1b[1;31mCould not start the session\x1b[0m\r\n");
    for line in error.to_string().lines() {
        text.push_str(line.trim_end());
        text.push_str("\r\n");
    }
    if let Some(log) = log {
        text.push_str(&format!("Host log: {}\r\n", log.display()));
    }
    text.into_bytes()
}

struct HostTx {
    link: Arc<Link>,
}

impl HostTx {
    /// The live connection, waiting for it while it is being made.
    fn ready(&self) -> io::Result<AttachedTx> {
        let mut phase = self.link.lock();
        loop {
            match &*phase {
                Phase::Ready(tx) => return Ok(tx.clone()),
                Phase::Dead => return Err(io::ErrorKind::NotConnected.into()),
                Phase::Connecting { request, .. } if *request != Request::None => {
                    return Err(io::ErrorKind::NotConnected.into());
                }
                Phase::Connecting { .. } => {
                    phase = self
                        .link
                        .changed
                        .wait(phase)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
        }
    }
}

fn io_err(e: deck_host::Error) -> io::Error {
    io::Error::other(e)
}

impl RemoteTx for HostTx {
    fn input(&self, bytes: &[u8]) -> io::Result<()> {
        self.ready()?.input(bytes).map_err(io_err)
    }

    fn resize(&self, cols: u16, rows: u16, cell_width: u16, cell_height: u16) -> io::Result<()> {
        let grid = Grid {
            cols,
            rows,
            cell_width,
            cell_height,
        };
        remember_grid(grid);
        let tx = {
            let mut phase = self.link.lock();
            match &mut *phase {
                Phase::Connecting { grid: pending, .. } => {
                    *pending = Some(grid);
                    self.link.changed.notify_all();
                    return Ok(());
                }
                Phase::Ready(tx) => tx.clone(),
                Phase::Dead => return Err(io::ErrorKind::NotConnected.into()),
            }
        };
        tx.resize(cols, rows, cell_width, cell_height)
            .map_err(io_err)
    }

    fn palette(&self, palette: [[u8; 3]; 19]) -> io::Result<()> {
        let palette = Palette(palette);
        let tx = {
            let mut phase = self.link.lock();
            match &mut *phase {
                Phase::Connecting { palette: p, .. } => {
                    *p = Some(palette);
                    return Ok(());
                }
                Phase::Ready(tx) => tx.clone(),
                Phase::Dead => return Err(io::ErrorKind::NotConnected.into()),
            }
        };
        tx.palette(&palette).map_err(io_err)
    }
    fn blob(&self, id: &str) -> io::Result<()> {
        let tx = {
            let phase = self.link.lock();
            match &*phase {
                Phase::Ready(tx) => tx.clone(),
                // Asked after the snapshot arrived, so the connection is up; anything else
                // is a detached terminal.
                Phase::Connecting { .. } | Phase::Dead => {
                    return Err(io::ErrorKind::NotConnected.into());
                }
            }
        };
        if !tx.blob_fetch() {
            return Err(io::ErrorKind::Unsupported.into());
        }
        tx.blob(id).map_err(io_err)
    }

    fn kill(&self) -> io::Result<()> {
        self.finish(Request::Kill)
    }

    fn detach(&self) -> io::Result<()> {
        self.finish(Request::Detach)
    }
}

impl HostTx {
    /// A kill or detach: delivered now when connected, else remembered for the connection.
    fn finish(&self, what: Request) -> io::Result<()> {
        let tx = {
            let mut phase = self.link.lock();
            match &mut *phase {
                Phase::Connecting { request, .. } => {
                    // A kill is not downgraded by a later detach (the terminal detaches on drop).
                    if *request != Request::Kill {
                        *request = what;
                    }
                    self.link.changed.notify_all();
                    return Ok(());
                }
                Phase::Ready(tx) => {
                    let tx = tx.clone();
                    if what == Request::Detach {
                        *phase = Phase::Dead;
                    }
                    tx
                }
                Phase::Dead => return Ok(()),
            }
        };
        match what {
            Request::Kill => tx.kill(),
            _ => tx.detach(),
        }
        .map_err(io_err)
    }
}

impl Drop for HostTx {
    /// Dropping the writer half leaves the session running.
    fn drop(&mut self) {
        let _ = self.finish(Request::Detach);
    }
}

/// A read-only follower of a host session, as a deck-term [`Observer`] (for
/// `deck_term::Terminal::observe` / `observe_headless`). It never attaches, so the session's
/// controller keeps its connection; the terminal renders at the host's grid and cannot write,
/// resize or kill.
pub struct HostObserver {
    observed: Observed,
}

impl HostObserver {
    /// Subscribes to session `name` (blocks up to the host's handshake timeout: call it on a
    /// worker). Old daemons answer `deck_host::Error::Unsupported`; there is no attach fallback.
    pub fn connect(
        ns: &Namespace,
        name: &str,
    ) -> Result<(HostInfo, HostObserver), deck_host::Error> {
        let (info, observed) = deck_host::observe(ns, name, HISTORY_LINES)?;
        Ok((info, HostObserver { observed }))
    }
}

impl Observer for HostObserver {
    fn split(self: Box<Self>) -> (Box<dyn RemoteRx>, Box<dyn ObserverTx>) {
        let (rx, tx) = self.observed.split();
        (
            Box::new(HostObserverRx { rx, snapshot: None }),
            Box::new(HostObserverTx(tx)),
        )
    }
}

struct HostObserverRx {
    rx: ObserverRx,
    /// The snapshot bytes due right after the grid they belong to.
    snapshot: Option<Vec<u8>>,
}

impl RemoteRx for HostObserverRx {
    fn recv(&mut self) -> io::Result<RemoteEvent> {
        if let Some(bytes) = self.snapshot.take() {
            return Ok(RemoteEvent::Snapshot(bytes));
        }
        let e = self.rx.recv().map_err(io::Error::other)?;
        Ok(match e {
            ObserverEvent::Snapshot { bytes, grid } => {
                // Size first, so the replay lands on the host's grid.
                self.snapshot = Some(bytes);
                resize_event(grid)
            }
            ObserverEvent::Output(b) => RemoteEvent::Output(b),
            ObserverEvent::Resized(grid) => resize_event(grid),
            ObserverEvent::Exited(code) => RemoteEvent::Exited(code),
            ObserverEvent::Closed => RemoteEvent::Closed,
        })
    }
}

fn resize_event(g: deck_host::Grid) -> RemoteEvent {
    RemoteEvent::Resize {
        cols: g.cols,
        rows: g.rows,
        cell_width: g.cell_width,
        cell_height: g.cell_height,
    }
}

struct HostObserverTx(deck_host::ObserverTx);

impl ObserverTx for HostObserverTx {
    fn detach(&self) -> io::Result<()> {
        self.0.detach().map_err(io_err)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    #[cfg(unix)]
    use std::path::PathBuf;
    #[cfg(unix)]
    use std::process::Command;
    #[cfg(unix)]
    use std::sync::mpsc;
    #[cfg(unix)]
    use std::time::Instant;

    use super::*;

    #[cfg(unix)]
    fn spec(name: &str) -> SessionSpec {
        SessionSpec {
            name: name.to_owned(),
            program: PathBuf::from("/bin/cat"),
            args: Vec::new(),
            cwd: PathBuf::from("/"),
            env_set: Vec::new(),
            env_remove: Vec::new(),
            tsp: false,
        }
    }

    #[cfg(unix)]
    fn test_ns(tag: &str) -> Namespace {
        Namespace::new(&format!("omp-deck-test-ui{}-{tag}", std::process::id())).expect("ns")
    }

    #[test]
    fn attach_gate_times_out_without_bypassing_its_owner_and_releases_on_drop() {
        let gate = Arc::new(AttachGate::default());
        let owner = gate.enter(Duration::ZERO).expect("first owner");
        assert!(matches!(
            gate.enter(Duration::from_millis(20)),
            Err(deck_host::Error::Timeout(_))
        ));
        drop(owner);
        assert!(gate.enter(Duration::ZERO).is_ok());
    }

    #[cfg(unix)]
    fn fake_attaching(mut stream: deck_host::transport::Stream) -> deck_host::transport::Stream {
        use deck_host::protocol::{self, Hello, Kind};
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut payload = Vec::new();
        let mut out = Vec::new();
        assert_eq!(
            protocol::read_frame(&mut stream, &mut payload).unwrap(),
            Some(Kind::Hello)
        );
        protocol::write_json(&mut stream, &mut out, Kind::Hello, &Hello::ours()).unwrap();
        assert_eq!(
            protocol::read_frame(&mut stream, &mut payload).unwrap(),
            Some(Kind::Info)
        );
        protocol::write_json(&mut stream, &mut out, Kind::Info, &serde_json::json!({
            "name":"fake", "protocol":deck_host::PROTOCOL, "host_version":deck_host::HOST_VERSION,
            "daemon_pid":1, "child_pid":2, "child_started_unix_ms":0,
            "cwd":"/", "argv":[], "tty":null, "term_session_id":null,
            "attached":false, "cols":80, "rows":24
        })).unwrap();
        assert_eq!(
            protocol::read_frame(&mut stream, &mut payload).unwrap(),
            Some(Kind::Attach)
        );
        stream
    }

    #[cfg(unix)]
    #[test]
    fn close_during_in_flight_attach_cannot_overtake_the_reopened_controller() {
        use deck_host::{
            protocol::{self, Kind},
            transport::{Endpoint, Listener, Stream},
        };
        let ns = test_ns("flight");
        let name = "omp-flight";
        let listener = Arc::new(Listener::bind(&Endpoint::new(&ns, name).unwrap()).unwrap());
        struct Cleanup(Arc<Listener>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.0.remove();
            }
        }
        let _cleanup = Cleanup(Arc::clone(&listener));
        let gate = attach_gate(&ns, name);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            type Current = Arc<Mutex<Option<(usize, Stream)>>>;
            let current: Current = Arc::new(Mutex::new(None));
            let mut clients = Vec::new();
            let mut first = Some((entered_tx, release_rx));
            for id in 0..2 {
                let stream = listener.accept().unwrap();
                let current = Arc::clone(&current);
                let first = first.take();
                clients.push(std::thread::spawn(move || {
                    let mut stream = fake_attaching(stream);
                    if let Some((entered, release)) = first {
                        entered.send(()).unwrap();
                        release.recv_timeout(Duration::from_secs(3)).unwrap();
                    }
                    let mut out = Vec::new();
                    {
                        let mut active = current.lock().unwrap();
                        if let Some((_, previous)) = &mut *active {
                            let _ = protocol::write_frame(previous, &mut out, Kind::Displaced, &[]);
                        }
                        *active = Some((id, stream.try_clone().unwrap()));
                        protocol::write_frame(&mut stream, &mut out, Kind::Snapshot, b"attached")
                            .unwrap();
                        protocol::write_frame(&mut stream, &mut out, Kind::SnapshotEnd, &[])
                            .unwrap();
                    }
                    let mut payload = Vec::new();
                    while let Some(kind) = protocol::read_frame(&mut stream, &mut payload).unwrap()
                    {
                        match kind {
                            Kind::Input
                                if current
                                    .lock()
                                    .unwrap()
                                    .as_ref()
                                    .is_some_and(|(active, _)| *active == id) =>
                            {
                                protocol::write_frame(
                                    &mut stream,
                                    &mut out,
                                    Kind::Output,
                                    &payload,
                                )
                                .unwrap();
                            }
                            Kind::Detach => break,
                            _ => {}
                        }
                    }
                }));
            }
            for client in clients {
                client.join().unwrap();
            }
        });
        let (mut old_rx, old_tx) = Box::new(HostRemote::new(ns.clone(), spec(name))).split();
        old_tx.resize(80, 24, 8, 16).unwrap();
        let old = std::thread::spawn(move || {
            assert!(matches!(old_rx.recv().unwrap(), RemoteEvent::Output(_)));
            old_rx.recv().unwrap()
        });
        entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("old Attach is in flight");
        old_tx.detach().unwrap();
        assert!(
            gate.enter(Duration::from_millis(50)).is_err(),
            "sending Attach alone must not release its gate"
        );
        let (mut new_rx, new_tx) = Box::new(HostRemote::new(ns, spec(name))).split();
        new_tx.resize(80, 24, 8, 16).unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let reopened = std::thread::spawn(move || {
            assert!(matches!(new_rx.recv().unwrap(), RemoteEvent::Output(_)));
            let first = new_rx.recv().unwrap();
            done_tx.send(first).unwrap();
            let echo = new_rx.recv().unwrap();
            done_tx.send(echo).unwrap();
        });
        release_tx.send(()).unwrap();
        assert_eq!(old.join().unwrap(), RemoteEvent::Closed);
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
            RemoteEvent::Snapshot(_)
        ));
        new_tx.input(b"reopened-still-controls").unwrap();
        let echo = done_rx.recv_timeout(Duration::from_secs(3));
        new_tx.detach().unwrap();
        assert_eq!(
            echo.unwrap(),
            RemoteEvent::Output(b"reopened-still-controls".to_vec())
        );
        reopened.join().unwrap();
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unconfirmed_attach_times_out_and_releases_the_session_gate() {
        use deck_host::{
            protocol,
            transport::{Endpoint, Listener},
        };
        let ns = test_ns("ack-timeout");
        let name = "omp-timeout";
        let listener = Listener::bind(&Endpoint::new(&ns, name).unwrap()).unwrap();
        let server = std::thread::spawn(move || {
            let mut stream = fake_attaching(listener.accept().unwrap());
            let mut payload = Vec::new();
            let _ = protocol::read_frame(&mut stream, &mut payload);
        });
        let gate = attach_gate(&ns, name);
        let link = Arc::new(Link {
            phase: Mutex::new(Phase::Connecting {
                grid: Some(Grid {
                    cols: 80,
                    rows: 24,
                    cell_width: 8,
                    cell_height: 16,
                }),
                palette: None,
                request: Request::None,
            }),
            changed: Condvar::new(),
        });
        let mut reader = HostRx {
            link: Arc::clone(&link),
            state: RxState::Connect,
            ns,
            spec: spec(name),
        };
        assert!(matches!(
            reader.connect(Duration::from_millis(30)),
            Some(Err(deck_host::Error::Timeout(_)))
        ));
        assert!(matches!(*link.lock(), Phase::Dead));
        assert!(
            gate.enter(Duration::ZERO).is_ok(),
            "a hung host cannot retain the gate forever"
        );
        server.join().unwrap();
    }

    #[test]
    fn a_failure_is_shown_with_the_log_path_then_the_terminal_exits() {
        let e = deck_host::Error::DaemonStartup("boom\nsecond line".into());
        let text = String::from_utf8(failure_text(&e, Some(Path::new("/l/x.log")))).unwrap();
        assert!(text.contains("Could not start the session"));
        assert!(text.contains("boom\r\nsecond line\r\n"), "{text:?}");
        assert!(text.ends_with("Host log: /l/x.log\r\n"));
    }

    #[cfg(unix)]
    #[test]
    fn detaching_before_connection_creates_the_accepted_session_then_ends_the_stream() {
        let Some(exe) = host_exe() else {
            return;
        };
        // Isolate executable discovery from the parallel unit suite's process environment.
        if std::env::var_os("OMPDECK_EARLY_DETACH_TEST_CHILD").is_none() {
            let output = Command::new(std::env::current_exe().expect("test executable"))
                .args(["--exact", "host_remote::tests::detaching_before_connection_creates_the_accepted_session_then_ends_the_stream", "--nocapture"])
                .env("OMPDECK_EARLY_DETACH_TEST_CHILD", "1")
                .env(deck_host::HOST_EXE_ENV, exe)
                .output().expect("isolated early-detach test");
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let ns = test_ns("early");
        struct Cleanup(Namespace);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.0.kill("omp-early");
            }
        }
        let _cleanup = Cleanup(ns.clone());
        let remote = HostRemote::new(ns.clone(), spec("omp-early"));
        let (mut rx, tx) = Box::new(remote).split();
        tx.resize(100, 30, 8, 16).unwrap();
        tx.detach().unwrap();
        assert!(
            matches!(rx.recv().unwrap(), RemoteEvent::Output(_)),
            "banner"
        );
        assert_eq!(rx.recv().unwrap(), RemoteEvent::Closed);
        assert!(tx.input(b"x").is_err(), "input after detach fails");
        assert!(
            ns.has("omp-early"),
            "an accepted backend survives an immediate tab close"
        );

        // Reopen the surviving backend, then let a cancelled lazy reader connect.
        // The cancelled reader must not steal this new controlling attachment.
        let (mut active_rx, active_tx) =
            Box::new(HostRemote::new(ns.clone(), spec("omp-early"))).split();
        active_tx.resize(100, 30, 8, 16).unwrap();
        assert!(
            recv_until(&mut *active_rx, |e| matches!(e, RemoteEvent::Snapshot(_))).is_some(),
            "the reopened tab controls the surviving backend"
        );
        let (mut cancelled_rx, cancelled_tx) =
            Box::new(HostRemote::new(ns.clone(), spec("omp-early"))).split();
        drop(cancelled_tx);
        assert!(matches!(
            cancelled_rx.recv().unwrap(),
            RemoteEvent::Output(_)
        ));
        assert_eq!(cancelled_rx.recv().unwrap(), RemoteEvent::Closed);

        active_tx.input(b"still-controlled-after-reopen\n").unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let echoed = recv_until(&mut *active_rx, |e| {
                matches!(e, RemoteEvent::Output(bytes)
                    if String::from_utf8_lossy(bytes).contains("still-controlled-after-reopen"))
            });
            let _ = done_tx.send(echoed);
        });
        assert!(
            done_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("the active controller still receives output")
                .is_some(),
            "a cancelled lazy reader must not displace the reopened controller"
        );
        reader.join().unwrap();
        active_tx.detach().unwrap();
        assert!(ns.has("omp-early"), "the backend still survives detach");
    }

    /// `omp-deck-host` next to the test binaries, when the workspace build produced it.
    #[cfg(unix)]
    fn host_exe() -> Option<PathBuf> {
        let deps = std::env::current_exe().ok()?;
        let exe = deps
            .parent()?
            .parent()?
            .join(format!("omp-deck-host{}", std::env::consts::EXE_SUFFIX));
        exe.is_file().then_some(exe)
    }

    #[cfg(unix)]
    fn recv_until(
        rx: &mut dyn RemoteRx,
        pred: impl Fn(&RemoteEvent) -> bool,
    ) -> Option<RemoteEvent> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let ev = rx.recv().ok()?;
            if pred(&ev) {
                return Some(ev);
            }
            if ev == RemoteEvent::Closed {
                return None;
            }
        }
        None
    }

    #[cfg(unix)]
    #[test]
    fn a_live_host_session_streams_input_back_and_closes_promptly_after_detach() {
        let Some(exe) = host_exe() else {
            eprintln!("skipped: omp-deck-host is not built (cargo build -p deck-host)");
            return;
        };
        let ns = test_ns("live");
        let name = "omp-live";
        /// Kills the test session whatever the outcome.
        struct Cleanup<'a>(&'a Namespace, &'a str);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                let _ = self.0.kill(self.1);
            }
        }
        let _cleanup = Cleanup(&ns, name);
        let started = Command::new(&exe)
            .args(["new", name, "--ns", ns.as_str(), "--", "/bin/cat"])
            .status()
            .expect("run omp-deck-host");
        assert!(started.success());
        let remote = HostRemote::new(ns.clone(), spec(name));
        let (mut rx, tx) = Box::new(remote).split();
        tx.resize(90, 20, 8, 16).unwrap();
        assert!(
            recv_until(&mut *rx, |e| matches!(e, RemoteEvent::Snapshot(_))).is_some(),
            "attaching a live session starts with its snapshot"
        );
        tx.input(b"hello-host\n").unwrap();
        let echoed = recv_until(
            &mut *rx,
            |e| matches!(e, RemoteEvent::Output(b) if String::from_utf8_lossy(b).contains("hello-host")),
        );
        assert!(echoed.is_some(), "typed text comes back from the session");
        let (done_tx, done_rx) = mpsc::channel();
        tx.detach().unwrap();
        std::thread::spawn(move || {
            // Output already in flight (the echo's tail) may still arrive before the end.
            let closed = recv_until(&mut *rx, |e| *e == RemoteEvent::Closed);
            let _ = done_tx.send(closed);
        });
        let after = done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("the stream ends after detach");
        assert_eq!(after, Some(RemoteEvent::Closed));
        assert!(ns.has(name), "detaching leaves the session running");
    }
}
