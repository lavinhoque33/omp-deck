//! `omp-deck-host serve`: owns one session's PTY, mirrors its output into a headless alacritty
//! `Term` (answering terminal queries while no client is attached), and serves clients.
//!
//! Threads: accept loop (main), one per connection, PTY reader, PTY writer, child waiter,
//! sync-update timer, one writer per attached client.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::term::Config;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::vte::ansi::{Processor, Rgb};

use crate::error::{Error, Result};
use crate::namespace::Namespace;
use crate::protocol::{
    self, AttachParams, ExitedPayload, Hello, HostInfo, Kind, PROTOCOL, Palette, PeekParams,
    ResizeParams, SNAPSHOT_CHUNK, SpawnRequest,
};
use crate::pty::{self, PtyChild, PtySize};
use crate::snapshot::{self, SideScanner};
use crate::transport::{Listener, Stream};

/// Scrollback kept by the daemon's terminal.
const SCROLLBACK: usize = 50_000;
/// Queued frames per attached client before it is considered stuck and dropped (it can
/// re-attach and gets a fresh snapshot).
const CLIENT_QUEUE: usize = 4096;
/// After `SIGHUP`, the process group gets this long before `SIGKILL`.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// After the child exited, remaining PTY output is drained for at most this long.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
/// Shutdown waits this long for clients to receive `Exited`.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// Options of `omp-deck-host serve`.
pub struct ServeOptions<'a> {
    pub ns: &'a str,
    pub name: &'a str,
    pub spec_file: &'a Path,
    /// Unix: stay in the foreground instead of forking into a new session.
    pub foreground: bool,
}

/// Runs the daemon. Returns only on startup failure (logged); the process exits when the
/// session ends.
pub fn serve(opts: &ServeOptions<'_>) -> Result<()> {
    let ns = Namespace::new(opts.ns)?;
    let endpoint = ns.endpoint(opts.name)?;
    let spec_bytes = std::fs::read(opts.spec_file);
    let _ = std::fs::remove_file(opts.spec_file);
    let request: SpawnRequest = serde_json::from_slice(&spec_bytes?)?;

    #[cfg(unix)]
    let ready = if opts.foreground {
        None
    } else {
        Some(daemonize()?)
    };
    #[cfg(windows)]
    let _ = opts.foreground;

    crate::logfile::init(ns.log_path(opts.name).as_deref());
    let result = start(&endpoint, opts.name, request);
    let (shared, listener) = match result {
        Ok(started) => started,
        Err(e) => {
            log::error!("startup failed: {e}");
            log::logger().flush();
            return Err(e);
        }
    };
    log::info!("serving {} (child pid {})", opts.name, shared.child.pid());
    #[cfg(unix)]
    if let Some(ready) = ready {
        signal_ready(ready);
    }
    accept_loop(&shared, &listener);
}

fn start(
    endpoint: &crate::transport::Endpoint,
    name: &str,
    request: SpawnRequest,
) -> Result<(Arc<Shared>, Arc<Listener>)> {
    if request.spec.name != name {
        return Err(Error::Protocol(format!(
            "spec is for session {:?}, not {name:?}",
            request.spec.name
        )));
    }
    let listener = Arc::new(Listener::bind(endpoint)?);
    let size = PtySize {
        cols: request.cols.max(2),
        rows: request.rows.max(1),
        cell_width: request.cell_width,
        cell_height: request.cell_height,
    };
    let spawned = match pty::spawn(&request.spec, size) {
        Ok(s) => s,
        Err(e) => {
            listener.remove();
            return Err(e);
        }
    };
    let started_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);

    let (event_tx, events) = mpsc::channel();
    let config = Config {
        scrolling_history: SCROLLBACK,
        // ConPTY does not implement kitty keyboard input; omp then uses win32-input-mode.
        kitty_keyboard: cfg!(not(windows)),
        ..Config::default()
    };
    let term = Term::new(config, &term_size(size), EventProxy(event_tx));
    let (input, input_rx) = mpsc::channel::<Vec<u8>>();
    let spec = &request.spec;
    let mut argv = vec![spec.program.to_string_lossy().into_owned()];
    argv.extend(spec.args.iter().cloned());
    let shared = Arc::new(Shared {
        core: Mutex::new(Core {
            term,
            processor: Processor::new(),
            scanner: SideScanner::new(size.rows),
            events,
            palette: request.palette,
            size,
            client: None,
            next_client: 1,
        }),
        sync_cv: Condvar::new(),
        status: Mutex::new(Status {
            exited: None,
            reader_done: false,
            busy: 0,
            shutting_down: false,
        }),
        status_cv: Condvar::new(),
        input,
        child: spawned.child,
        listener: Arc::clone(&listener),
        name: name.to_owned(),
        argv,
        cwd: spec.cwd.clone(),
        term_session_id: spec
            .env_set
            .iter()
            .find(|(k, _)| k == "TERM_SESSION_ID")
            .map(|(_, v)| v.clone()),
        started_ms,
    });

    let mut writer = spawned.writer;
    spawn_thread("pty-writer", move || {
        for bytes in input_rx {
            if let Err(e) = writer.write_all(&bytes) {
                log::warn!("pty write: {e}");
                break;
            }
        }
    })?;
    let s = Arc::clone(&shared);
    let reader = spawned.reader;
    spawn_thread("pty-reader", move || read_loop(&s, reader))?;
    let s = Arc::clone(&shared);
    spawn_thread("sync-timer", move || sync_timer(&s))?;
    let s = Arc::clone(&shared);
    spawn_thread("child-waiter", move || {
        let code = s.child.wait();
        log::info!("child exited: {code:?}");
        // Windows: closing the pseudoconsole ends its output pipe; the reader keeps draining.
        s.child.close_console();
        s.wait_status(DRAIN_TIMEOUT, |st| st.reader_done);
        finish(&s, code);
    })?;
    Ok((shared, listener))
}

fn spawn_thread(name: &str, f: impl FnOnce() + Send + 'static) -> Result<()> {
    std::thread::Builder::new().name(name.to_owned()).spawn(f)?;
    Ok(())
}

fn term_size(size: PtySize) -> TermSize {
    TermSize::new(usize::from(size.cols), usize::from(size.rows))
}

/// Terminal events are queued and handled right after each `advance`, outside the handler.
struct EventProxy(Sender<Event>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let _ = self.0.send(event);
    }
}

enum Out {
    Frame(Kind, Vec<u8>),
    Snapshot(Vec<u8>),
}

struct ClientHandle {
    id: u64,
    queue: SyncSender<Out>,
}

struct Core {
    term: Term<EventProxy>,
    processor: Processor,
    scanner: SideScanner,
    events: Receiver<Event>,
    palette: Palette,
    size: PtySize,
    client: Option<ClientHandle>,
    next_client: u64,
}

struct Status {
    /// `Some(code)` once the session ended.
    exited: Option<Option<i32>>,
    reader_done: bool,
    /// Threads that must deliver something before the process exits.
    busy: usize,
    shutting_down: bool,
}

struct Shared {
    core: Mutex<Core>,
    /// Wakes the sync-update timer (paired with `core`).
    sync_cv: Condvar,
    status: Mutex<Status>,
    status_cv: Condvar,
    input: Sender<Vec<u8>>,
    child: PtyChild,
    listener: Arc<Listener>,
    name: String,
    argv: Vec<String>,
    cwd: std::path::PathBuf,
    term_session_id: Option<String>,
    started_ms: u64,
}

impl Shared {
    fn core(&self) -> MutexGuard<'_, Core> {
        self.core.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn status(&self) -> MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn update_status(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status());
        self.status_cv.notify_all();
    }

    /// Waits until `done(status)` or `timeout`; returns whether it became true.
    fn wait_status(&self, timeout: Duration, done: impl Fn(&Status) -> bool) -> bool {
        let guard = self.status();
        let (guard, _) = self
            .status_cv
            .wait_timeout_while(guard, timeout, |st| !done(st))
            .unwrap_or_else(PoisonError::into_inner);
        done(&guard)
    }

    fn info(&self, core: &Core) -> HostInfo {
        HostInfo {
            name: self.name.clone(),
            protocol: PROTOCOL,
            host_version: crate::HOST_VERSION.to_owned(),
            daemon_pid: std::process::id(),
            child_pid: self.child.pid(),
            child_started_unix_ms: self.started_ms,
            cwd: self.cwd.clone(),
            argv: self.argv.clone(),
            tty: self.child.tty().map(str::to_owned),
            term_session_id: self.term_session_id.clone(),
            attached: core.client.is_some(),
            cols: core.size.cols,
            rows: core.size.rows,
        }
    }
}

impl Core {
    /// Feeds PTY output to the terminal and side scanner and answers queued terminal queries.
    fn feed(&mut self, bytes: &[u8], input: &Sender<Vec<u8>>) {
        self.processor.advance(&mut self.term, bytes);
        self.scanner.advance(bytes);
        self.answer_queries(input);
    }

    fn answer_queries(&mut self, input: &Sender<Vec<u8>>) {
        while let Ok(event) = self.events.try_recv() {
            let reply = match event {
                Event::PtyWrite(text) => text,
                Event::ColorRequest(index, format) => {
                    let rgb = self.term.colors()[index]
                        .unwrap_or_else(|| palette_color(&self.palette, index));
                    format(rgb)
                }
                Event::TextAreaSizeRequest(format) => format(WindowSize {
                    num_lines: self.size.rows,
                    num_cols: self.size.cols,
                    cell_width: self.size.cell_width,
                    cell_height: self.size.cell_height,
                }),
                _ => continue,
            };
            let _ = input.send(reply.into_bytes());
        }
    }

    /// Ends a pending synchronized update so the terminal reflects everything received.
    fn flush_sync(&mut self, input: &Sender<Vec<u8>>) {
        if self.processor.sync_timeout().sync_timeout().is_some() {
            self.processor.stop_sync(&mut self.term);
            self.answer_queries(input);
        }
    }

    fn resize(
        &mut self,
        child: &PtyChild,
        cols: u16,
        rows: u16,
        cell_width: u16,
        cell_height: u16,
    ) {
        if cell_width != 0 && cell_height != 0 {
            self.size.cell_width = cell_width;
            self.size.cell_height = cell_height;
        }
        if cols == 0 || rows == 0 || (cols == self.size.cols && rows == self.size.rows) {
            return;
        }
        self.size.cols = cols.max(2);
        self.size.rows = rows;
        if let Err(e) = child.resize(self.size) {
            log::warn!("pty resize: {e}");
        }
        self.term.resize(term_size(self.size));
        self.scanner.resize(rows);
    }

    fn snapshot(&mut self, history_lines: u32, input: &Sender<Vec<u8>>) -> Vec<u8> {
        self.flush_sync(input);
        let side = self.scanner.state().clone();
        snapshot::encode(&mut self.term, &side, history_lines as usize)
    }
}

/// Color for a query the child's own overrides do not answer: the client's palette, else the
/// xterm 256-color cube and gray ramp.
fn palette_color(palette: &Palette, index: usize) -> Rgb {
    let [r, g, b] = match index {
        0..=15 => palette.0[index],
        16..=231 => {
            let i = index - 16;
            let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
            [level(i / 36), level((i / 6) % 6), level(i % 6)]
        }
        232..=255 => {
            let v = (8 + (index - 232) * 10) as u8;
            [v, v, v]
        }
        257 => palette.0[Palette::BACKGROUND],
        258 => palette.0[Palette::CURSOR],
        _ => palette.0[Palette::FOREGROUND],
    };
    Rgb { r, g, b }
}

fn read_loop(shared: &Shared, mut reader: pty::PtyReader) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                log::warn!("pty read: {e}");
                break;
            }
        };
        let chunk = &buf[..n];
        let mut core = shared.core();
        core.feed(chunk, &shared.input);
        if core.processor.sync_timeout().sync_timeout().is_some() {
            shared.sync_cv.notify_one();
        }
        if let Some(client) = &core.client {
            match client
                .queue
                .try_send(Out::Frame(Kind::Output, chunk.to_vec()))
            {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    log::warn!("client {} is not keeping up; dropping it", client.id);
                    core.client = None;
                }
                Err(TrySendError::Disconnected(_)) => core.client = None,
            }
        }
    }
    shared.update_status(|st| st.reader_done = true);
}

/// Applies the synchronized-update timeout (150 ms in vte) even when no further output
/// arrives, so queries inside an unterminated update still get answered.
fn sync_timer(shared: &Shared) {
    let mut core = shared.core();
    loop {
        let deadline = core.processor.sync_timeout().sync_timeout();
        core = match deadline {
            None => shared
                .sync_cv
                .wait(core)
                .unwrap_or_else(PoisonError::into_inner),
            Some(at) => {
                let now = Instant::now();
                if at <= now {
                    core.flush_sync(&shared.input);
                    core
                } else {
                    shared
                        .sync_cv
                        .wait_timeout(core, at - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
            }
        };
    }
}

/// Ends the daemon: endpoint removed, `Exited` to the attached client and pending killers,
/// then process exit.
fn finish(shared: &Shared, code: Option<i32>) -> ! {
    shared.listener.remove();
    shared.update_status(|st| {
        st.exited = Some(code);
        st.shutting_down = true;
    });
    {
        let mut core = shared.core();
        if let Some(client) = core.client.take() {
            let payload = serde_json::to_vec(&ExitedPayload { code }).unwrap_or_default();
            let _ = client.queue.try_send(Out::Frame(Kind::Exited, payload));
        }
    }
    shared.wait_status(SHUTDOWN_GRACE, |st| st.busy == 0);
    log::info!("session ended");
    log::logger().flush();
    std::process::exit(0);
}

fn accept_loop(shared: &Arc<Shared>, listener: &Listener) -> ! {
    loop {
        match listener.accept() {
            Ok(stream) => {
                let s = Arc::clone(shared);
                if let Err(e) = spawn_thread("conn", move || {
                    if let Err(e) = serve_conn(&s, stream) {
                        log::debug!("connection: {e}");
                    }
                }) {
                    log::warn!("spawning connection thread: {e}");
                }
            }
            Err(e) => {
                log::warn!("accept: {e}");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Per-connection state.
struct Conn<'a> {
    shared: &'a Arc<Shared>,
    stream: Stream,
    payload: Vec<u8>,
    out: Vec<u8>,
    /// Client id while this connection is (or was) the attached one; its writes then go
    /// through the client queue.
    attached: Option<u64>,
    peer_protocol: u32,
}

impl Conn<'_> {
    fn reply(&mut self, kind: Kind, payload: &[u8]) -> Result<()> {
        if let Some(id) = self.attached {
            let core = self.shared.core();
            if let Some(client) = core.client.as_ref().filter(|c| c.id == id) {
                let _ = client.queue.try_send(Out::Frame(kind, payload.to_vec()));
            }
            return Ok(());
        }
        protocol::write_frame(&mut self.stream, &mut self.out, kind, payload)
    }

    fn reply_json<T: serde::Serialize>(&mut self, kind: Kind, value: &T) -> Result<()> {
        let payload = serde_json::to_vec(value)?;
        self.reply(kind, &payload)
    }

    fn is_current(&self, core: &Core) -> bool {
        self.attached.is_some() && core.client.as_ref().map(|c| c.id) == self.attached
    }
}

fn serve_conn(shared: &Arc<Shared>, stream: Stream) -> Result<()> {
    stream.set_read_timeout(Some(HELLO_TIMEOUT))?;
    let mut conn = Conn {
        shared,
        stream,
        payload: Vec::new(),
        out: Vec::new(),
        attached: None,
        peer_protocol: 0,
    };
    match protocol::read_frame(&mut conn.stream, &mut conn.payload)? {
        Some(Kind::Hello) => {}
        _ => return Ok(()),
    }
    let hello: Hello = protocol::parse(&conn.payload)?;
    conn.peer_protocol = hello.protocol;
    conn.reply_json(Kind::Hello, &Hello::ours())?;
    conn.stream.set_read_timeout(None)?;

    let result = conn_loop(&mut conn);
    if let Some(id) = conn.attached {
        let mut core = shared.core();
        if core.client.as_ref().is_some_and(|c| c.id == id) {
            core.client = None;
        }
    }
    result
}

fn conn_loop(conn: &mut Conn<'_>) -> Result<()> {
    let shared = conn.shared;
    loop {
        let Some(kind) = protocol::read_frame(&mut conn.stream, &mut conn.payload)? else {
            return Ok(());
        };
        if kind == Kind::Kill {
            return kill(conn);
        }
        if kind == Kind::Ping {
            conn.reply(Kind::Pong, &[])?;
            continue;
        }
        if conn.peer_protocol != PROTOCOL {
            let msg = format!(
                "protocol {} not supported (daemon speaks {PROTOCOL})",
                conn.peer_protocol
            );
            conn.reply_json(Kind::Error, &msg)?;
            continue;
        }
        match kind {
            Kind::Info => {
                let info = shared.info(&shared.core());
                conn.reply_json(Kind::Info, &info)?;
            }
            Kind::Peek => {
                let params: PeekParams = protocol::parse(&conn.payload)?;
                let snapshot = shared.core().snapshot(params.history_lines, &shared.input);
                for chunk in snapshot.chunks(SNAPSHOT_CHUNK) {
                    conn.reply(Kind::Snapshot, chunk)?;
                }
                conn.reply(Kind::SnapshotEnd, &[])?;
            }
            Kind::Attach => {
                let params: AttachParams = protocol::parse(&conn.payload)?;
                attach(conn, &params)?;
            }
            Kind::Input => {
                let core = shared.core();
                if conn.is_current(&core) {
                    let _ = shared.input.send(conn.payload.clone());
                }
            }
            Kind::Resize => {
                let p: ResizeParams = protocol::parse(&conn.payload)?;
                let mut core = shared.core();
                if conn.is_current(&core) {
                    core.resize(&shared.child, p.cols, p.rows, p.cell_width, p.cell_height);
                }
            }
            Kind::Palette => {
                let palette: Palette = protocol::parse(&conn.payload)?;
                let mut core = shared.core();
                if conn.is_current(&core) {
                    core.palette = palette;
                }
            }
            Kind::Detach => {
                let mut core = shared.core();
                if conn.is_current(&core) {
                    // Dropping the queue ends the client writer, which closes the stream.
                    core.client = None;
                }
                return Ok(());
            }
            _ => {}
        }
    }
}

fn attach(conn: &mut Conn<'_>, params: &AttachParams) -> Result<()> {
    let shared = conn.shared;
    if shared.status().shutting_down {
        let payload = serde_json::to_vec(&ExitedPayload {
            code: shared.status().exited.flatten(),
        })?;
        return conn.reply(Kind::Exited, &payload);
    }
    let writer_stream = conn.stream.try_clone()?;
    let (queue, rx) = mpsc::sync_channel(CLIENT_QUEUE);
    let mut core = shared.core();
    core.resize(
        &shared.child,
        params.cols,
        params.rows,
        params.cell_width,
        params.cell_height,
    );
    core.palette = params.palette;
    let snapshot = core.snapshot(params.history_lines, &shared.input);
    if let Some(old) = core.client.take() {
        // The old writer sends this, then sees the queue closed and ends its stream.
        let _ = old.queue.try_send(Out::Frame(Kind::Displaced, Vec::new()));
    }
    let id = core.next_client;
    core.next_client += 1;
    // Fresh queue with capacity ≥ 1: cannot fail.
    let _ = queue.try_send(Out::Snapshot(snapshot));
    core.client = Some(ClientHandle { id, queue });
    conn.attached = Some(id);
    drop(core);

    shared.update_status(|st| st.busy += 1);
    let s = Arc::clone(shared);
    let spawned = spawn_thread("client-writer", move || {
        client_writer(writer_stream, &rx);
        s.update_status(|st| st.busy -= 1);
    });
    if spawned.is_err() {
        shared.update_status(|st| st.busy -= 1);
    }
    spawned
}

fn client_writer(mut stream: Stream, rx: &Receiver<Out>) {
    let mut buf = Vec::new();
    for out in rx {
        let result = match out {
            Out::Frame(kind, payload) => {
                protocol::write_frame(&mut stream, &mut buf, kind, &payload)
            }
            Out::Snapshot(bytes) => bytes
                .chunks(SNAPSHOT_CHUNK)
                .try_for_each(|c| protocol::write_frame(&mut stream, &mut buf, Kind::Snapshot, c))
                .and_then(|()| {
                    protocol::write_frame(&mut stream, &mut buf, Kind::SnapshotEnd, &[])
                }),
        };
        if result.is_err() {
            break;
        }
    }
    stream.shutdown();
}

/// `Kill`: terminate the process tree, wait for the session to end, confirm with `Exited`.
fn kill(conn: &mut Conn<'_>) -> Result<()> {
    let shared = conn.shared;
    shared.update_status(|st| st.busy += 1);
    if shared.status().exited.is_none() {
        log::info!("kill requested");
        shared.child.kill_tree(KILL_GRACE);
    }
    shared.wait_status(DRAIN_TIMEOUT + KILL_GRACE, |st| st.exited.is_some());
    let code = shared.status().exited.flatten();
    // The attached client gets `Exited` through its queue from `finish`.
    let result = if conn.attached.is_none() {
        conn.reply_json(Kind::Exited, &ExitedPayload { code })
    } else {
        Ok(())
    };
    shared.update_status(|st| st.busy -= 1);
    result
}

/// Unix: fork; the parent waits until the daemon (child, new session, stdio on /dev/null)
/// reports readiness and exits 0, or exits 1 if the daemon died first.
#[cfg(unix)]
fn daemonize() -> Result<std::os::fd::OwnedFd> {
    use std::os::fd::{FromRawFd, OwnedFd};
    let mut fds = [0; 2];
    // SAFETY: valid out array of two fds.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(Error::last_os("pipe"));
    }
    for fd in fds {
        // Still single-threaded: no fork can race this. The session child must not inherit
        // the write end, or the launcher would wait for it too.
        // SAFETY: valid fds from pipe().
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    // SAFETY: freshly created fds we own.
    let (read_end, write_end) =
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // SAFETY: called while the process is single-threaded (before any thread is spawned).
    match unsafe { libc::fork() } {
        -1 => Err(Error::last_os("fork")),
        0 => {
            drop(read_end);
            // SAFETY: plain syscalls in the child.
            unsafe {
                libc::setsid();
                let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
                if null >= 0 {
                    libc::dup2(null, 0);
                    libc::dup2(null, 1);
                    libc::dup2(null, 2);
                    if null > 2 {
                        libc::close(null);
                    }
                }
            }
            Ok(write_end)
        }
        _ => {
            drop(write_end);
            let mut file = std::fs::File::from(read_end);
            let mut byte = [0u8; 1];
            let ok = matches!(file.read(&mut byte), Ok(1));
            std::process::exit(if ok { 0 } else { 1 });
        }
    }
}

#[cfg(unix)]
fn signal_ready(fd: std::os::fd::OwnedFd) {
    let _ = std::fs::File::from(fd).write_all(b"1");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_colors_fall_back_to_palette_and_xterm_ramps() {
        let mut p = Palette::default();
        p.0[3] = [1, 2, 3];
        p.0[Palette::BACKGROUND] = [9, 8, 7];
        assert_eq!(palette_color(&p, 3), Rgb { r: 1, g: 2, b: 3 });
        assert_eq!(palette_color(&p, 257), Rgb { r: 9, g: 8, b: 7 });
        assert_eq!(palette_color(&p, 16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(palette_color(&p, 196), Rgb { r: 255, g: 0, b: 0 });
        assert_eq!(
            palette_color(&p, 244),
            Rgb {
                r: 128,
                g: 128,
                b: 128
            }
        );
    }
}
