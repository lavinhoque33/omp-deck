//! Remote terminals: the child runs under a session host (`omp-deck-host`) that owns the PTY
//! and its own headless `Term`; this side renders and sends input. The host answers every
//! terminal query, so the local `Term` here only displays (see [`crate::Terminal::connect`]).
//!
//! Threads per remote terminal: a reader (blocking [`RemoteRx::recv`]), a feeder (parses
//! output into the shared `Term` and runs the output scanners; owns the synchronized-update
//! timeout) and a writer (drains [`RemoteCmd`]s into [`RemoteTx`], so the UI thread never
//! blocks on the connection).

use std::io;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

use crate::core::{Listener, Notice, SharedTerm};
use crate::out::{PtyOut, WriteProgress};
use crate::scan::OutputScan;

/// What a session host sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteEvent {
    /// Whole-screen replay (VT bytes) that a fresh terminal turns into the host's state; the
    /// local terminal is reset before it is applied.
    Snapshot(Vec<u8>),
    /// Child output.
    Output(Vec<u8>),
    /// The child exited (`None`: no exit code, e.g. killed by a signal).
    Exited(Option<i32>),
    /// Another client attached to the session; this connection is over.
    Displaced,
    /// The connection ended.
    Closed,
}

/// A connection to a session host, split into its receiving and sending halves by
/// [`crate::Terminal::connect`].
pub trait Remote: Send {
    fn split(self: Box<Self>) -> (Box<dyn RemoteRx>, Box<dyn RemoteTx>);
}

/// Receiving half (runs on its own thread).
pub trait RemoteRx: Send {
    /// Blocks for the next event. After [`RemoteTx::detach`] or [`RemoteTx::kill`] it must
    /// return [`RemoteEvent::Closed`] (or an error) promptly; an error counts as `Closed`.
    fn recv(&mut self) -> io::Result<RemoteEvent>;
}

/// Sending half (runs on its own thread; calls may block). Dropping it detaches.
pub trait RemoteTx: Send + Sync {
    /// Input bytes for the child (keys, paste, focus reports, OSC 52 replies).
    fn input(&self, bytes: &[u8]) -> io::Result<()>;
    /// New grid size and cell pixel size.
    fn resize(&self, cols: u16, rows: u16, cell_width: u16, cell_height: u16) -> io::Result<()>;
    /// Colors the host answers OSC 4/10/11/12 queries with: ANSI 0..=15, foreground,
    /// background, cursor.
    fn palette(&self, palette: [[u8; 3]; 19]) -> io::Result<()>;
    /// Ends the child (and the session).
    fn kill(&self) -> io::Result<()>;
    /// Leaves the session running.
    fn detach(&self) -> io::Result<()>;
}

/// Work for the writer thread, in order.
pub(crate) enum RemoteCmd {
    Input(Vec<u8>),
    Resize(WindowSize),
    Palette([[u8; 3]; 19]),
    Detach,
    Kill,
}

/// Longest run of bytes parsed under one `Term` lock (alacritty's event loop uses the same
/// bound), so a large snapshot never stalls the UI thread for long.
const MAX_LOCKED: usize = 0x1_0000;

/// Writer thread: forwards commands until `Detach`/`Kill` (or until every sender is gone;
/// dropping `tx` then detaches). Every input chunk counts as handled in `progress` whether
/// or not the write succeeded, so a dropped terminal's detach never waits on a dead link.
pub(crate) fn run_writer(
    tx: Box<dyn RemoteTx>,
    cmds: Receiver<RemoteCmd>,
    progress: Arc<WriteProgress>,
) {
    let mut failed = false;
    let mut report = |what: &str, result: io::Result<()>| {
        if let Err(e) = result
            && !failed
        {
            failed = true;
            log::debug!("remote terminal {what} failed: {e}");
        }
    };
    while let Ok(cmd) = cmds.recv() {
        match cmd {
            RemoteCmd::Input(bytes) => {
                let result = tx.input(&bytes);
                progress.add(bytes.len());
                report("input", result);
            }
            RemoteCmd::Resize(ws) => report(
                "resize",
                tx.resize(ws.num_cols, ws.num_lines, ws.cell_width, ws.cell_height),
            ),
            RemoteCmd::Palette(p) => report("palette", tx.palette(p)),
            RemoteCmd::Detach => return report("detach", tx.detach()),
            RemoteCmd::Kill => return report("kill", tx.kill()),
        }
    }
}

/// Reader thread: forwards events to the feeder until the connection ends.
pub(crate) fn run_reader(mut rx: Box<dyn RemoteRx>, events: Sender<RemoteEvent>) {
    loop {
        let event = rx.recv().unwrap_or_else(|e| {
            log::debug!("remote terminal connection lost: {e}");
            RemoteEvent::Closed
        });
        let closed = event == RemoteEvent::Closed;
        if events.send(event).is_err() || closed {
            return;
        }
    }
}

/// Parses remote output into the shared `Term` and reports session state changes.
pub(crate) struct Feeder {
    pub(crate) term: SharedTerm,
    /// Config of the fresh `Term` a snapshot starts from.
    pub(crate) config: Config,
    pub(crate) listener: Listener,
    pub(crate) scan: OutputScan,
    pub(crate) out: Arc<PtyOut>,
    pub(crate) notices: Sender<Notice>,
    pub(crate) ctx: egui::Context,
}

impl Feeder {
    /// Runs until the reader is gone or the connection closed.
    pub(crate) fn run(mut self, events: Receiver<RemoteEvent>) {
        let mut parser: Processor = Processor::new();
        loop {
            let event = match parser.sync_timeout().sync_timeout() {
                Some(deadline) => {
                    match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Ok(event) => event,
                        Err(RecvTimeoutError::Timeout) => {
                            // The app never ended its synchronized update: show what it drew.
                            parser.stop_sync(&mut *self.term.lock());
                            self.listener.send_event(Event::Wakeup);
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                None => match events.recv() {
                    Ok(event) => event,
                    Err(_) => return,
                },
            };
            match event {
                RemoteEvent::Snapshot(bytes) => {
                    parser = Processor::new();
                    self.reset();
                    self.feed(&mut parser, &bytes);
                }
                RemoteEvent::Output(bytes) => self.feed(&mut parser, &bytes),
                RemoteEvent::Exited(code) => self.notify(Notice::Exited(code)),
                RemoteEvent::Displaced => {
                    self.out.displaced();
                    self.notify(Notice::Displaced);
                }
                RemoteEvent::Closed => return self.notify(Notice::Closed),
            }
        }
    }

    fn notify(&self, notice: Notice) {
        let _ = self.notices.send(notice);
        self.ctx.request_repaint();
    }

    /// Fresh `Term` at the current size (keeping focus state), fresh scanners.
    fn reset(&mut self) {
        {
            let mut term = self.term.lock();
            let size = TermSize::new(term.columns(), term.screen_lines());
            let focused = term.is_focused;
            *term = Term::new(self.config.clone(), &size, self.listener.clone());
            term.is_focused = focused;
        }
        self.scan.reset();
        self.notify(Notice::Reset);
    }

    fn feed(&mut self, parser: &mut Processor, bytes: &[u8]) {
        for chunk in bytes.chunks(MAX_LOCKED) {
            parser.advance(&mut *self.term.lock(), chunk);
            self.scan.feed(chunk);
            // Redraw unless all of it went into a pending synchronized update.
            if parser.sync_bytes_count() < chunk.len() {
                self.listener.send_event(Event::Wakeup);
            }
        }
    }
}

/// Starts the reader, feeder and writer threads of a remote terminal; returns the writer's
/// command queue and handle. A thread that cannot start ends the connection (`Closed`).
pub(crate) fn start(
    remote: Box<dyn Remote>,
    feeder: Feeder,
    progress: Arc<WriteProgress>,
) -> (Sender<RemoteCmd>, Option<std::thread::JoinHandle<()>>) {
    let (rx, tx) = remote.split();
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let writer = std::thread::Builder::new()
        .name("deck-term-remote-tx".into())
        .spawn(move || run_writer(tx, cmd_rx, progress));
    let writer = match writer {
        Ok(handle) => Some(handle),
        Err(e) => {
            log::warn!("remote terminal writer thread: {e}");
            None
        }
    };
    let (ev_tx, ev_rx) = mpsc::channel();
    let notices = feeder.notices.clone();
    let ctx = feeder.ctx.clone();
    let started = std::thread::Builder::new()
        .name("deck-term-remote-feed".into())
        .spawn(move || feeder.run(ev_rx))
        .and_then(|_| {
            std::thread::Builder::new()
                .name("deck-term-remote-rx".into())
                .spawn(move || run_reader(rx, ev_tx))
        });
    if let Err(e) = started {
        log::warn!("remote terminal reader threads: {e}");
        let _ = notices.send(Notice::Closed);
        ctx.request_repaint();
    }
    (cmd_tx, writer)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use alacritty_terminal::term::TermMode;

    use super::*;
    use crate::input::KeyRules;
    use crate::theme::{Palette, TermTheme};
    use crate::{TermOptions, Terminal, wait_detached};

    const TIMEOUT: Duration = Duration::from_secs(10);

    /// What the terminal sent to the fake host.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) enum Sent {
        Input(Vec<u8>),
        Resize(u16, u16, u16, u16),
        Palette([[u8; 3]; 19]),
        Kill,
        Detach,
    }

    pub(crate) type Log = Arc<Mutex<Vec<Sent>>>;

    /// In-memory host: events come from the test, everything sent is logged.
    struct Fake {
        events: Receiver<RemoteEvent>,
        log: Log,
    }

    struct FakeRx(Receiver<RemoteEvent>);

    struct FakeTx(Log);

    impl Remote for Fake {
        fn split(self: Box<Self>) -> (Box<dyn RemoteRx>, Box<dyn RemoteTx>) {
            (Box::new(FakeRx(self.events)), Box::new(FakeTx(self.log)))
        }
    }

    impl RemoteRx for FakeRx {
        fn recv(&mut self) -> io::Result<RemoteEvent> {
            Ok(self.0.recv().unwrap_or(RemoteEvent::Closed))
        }
    }

    impl FakeTx {
        fn push(&self, sent: Sent) -> io::Result<()> {
            self.0.lock().expect("log").push(sent);
            Ok(())
        }
    }

    impl RemoteTx for FakeTx {
        fn input(&self, bytes: &[u8]) -> io::Result<()> {
            self.push(Sent::Input(bytes.to_vec()))
        }

        fn resize(&self, cols: u16, rows: u16, cell_w: u16, cell_h: u16) -> io::Result<()> {
            self.push(Sent::Resize(cols, rows, cell_w, cell_h))
        }

        fn palette(&self, palette: [[u8; 3]; 19]) -> io::Result<()> {
            self.push(Sent::Palette(palette))
        }

        fn kill(&self) -> io::Result<()> {
            self.push(Sent::Kill)
        }

        fn detach(&self) -> io::Result<()> {
            self.push(Sent::Detach)
        }
    }

    pub(crate) fn connect() -> (Terminal, Sender<RemoteEvent>, Log) {
        let (host, events) = mpsc::channel();
        let log = Log::default();
        let fake = Fake {
            events,
            log: log.clone(),
        };
        let t = Terminal::connect(
            Box::new(fake),
            TermOptions::default(),
            egui::Context::default(),
        );
        (t, host, log)
    }

    fn output(host: &Sender<RemoteEvent>, bytes: &[u8]) {
        host.send(RemoteEvent::Output(bytes.to_vec()))
            .expect("feeder alive");
    }

    /// Polls `t` until `f` holds, failing after [`TIMEOUT`].
    fn until(t: &mut Terminal, what: &str, mut f: impl FnMut(&mut Terminal) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !f(t) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            t.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn logged(log: &Log) -> Vec<Sent> {
        log.lock().expect("log").clone()
    }

    fn inputs(log: &Log) -> Vec<u8> {
        logged(log)
            .into_iter()
            .filter_map(|s| match s {
                Sent::Input(b) => Some(b),
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// Output the feeder has certainly processed: a marker drawn after it is on screen.
    fn settle(t: &mut Terminal, host: &Sender<RemoteEvent>, marker: &str) {
        output(host, format!("\r\n{marker}").as_bytes());
        until(t, marker, |t| {
            t.screen_text().lines.iter().any(|l| l.contains(marker))
        });
    }

    #[test]
    fn snapshot_resets_and_replays_then_output_follows() {
        let (mut t, host, _log) = connect();
        // State before the snapshot: text, scrollback, modes, title, key modes.
        let mut before = Vec::new();
        for i in 0..40 {
            before.extend_from_slice(format!("old{i}\r\n").as_bytes());
        }
        before.extend_from_slice(b"\x1b]0;old title\x07\x1b[?1004h\x1b[>4;2m\x1b[?9001h");
        output(&host, &before);
        until(&mut t, "pre-snapshot title", |t| {
            t.title() == Some("old title")
        });
        until(&mut t, "win32 mode", |t| t.key_rules() == KeyRules::Win32);
        assert!(t.mode().contains(TermMode::FOCUS_IN_OUT));

        host.send(RemoteEvent::Snapshot(
            b"\x1b]2;snap\x07hello\x1b[?2004h\x1b[2;5H".to_vec(),
        ))
        .expect("send");
        until(&mut t, "snapshot replay", |t| {
            t.screen_text().lines[0] == "hello"
        });
        until(&mut t, "snapshot title", |t| t.title() == Some("snap"));
        let text = t.screen_text();
        assert!(text.lines[1..].iter().all(String::is_empty), "{text:?}");
        assert_eq!((text.cursor_row, text.cursor_col), (1, 4));
        {
            let term = t.term.lock();
            assert_eq!(term.grid().history_size(), 0, "scrollback reset");
        }
        let mode = t.mode();
        assert!(mode.contains(TermMode::BRACKETED_PASTE));
        assert!(!mode.contains(TermMode::FOCUS_IN_OUT), "modes reset");
        // Key modes reset too (the snapshot re-sends the ones still active).
        assert_eq!(t.key_rules(), KeyRules::Legacy);

        // Live output continues on the replayed state.
        output(&host, b"world");
        until(&mut t, "output after snapshot", |t| {
            t.screen_text().lines[1] == "    world"
        });
        // The modifyOtherKeys and win32 scanners run on remote output, across chunks.
        output(&host, b"\x1b[?90");
        output(&host, b"01h");
        until(&mut t, "win32 mode again", |t| {
            t.key_rules() == KeyRules::Win32
        });
        output(&host, b"\x1b[?9001l\x1b[>4;");
        output(&host, b"2m");
        until(&mut t, "modifyOtherKeys", |t| {
            t.key_rules() == KeyRules::ModifyOtherKeys
        });
    }

    #[test]
    fn replies_suppressed_but_clipboard_load_answered() {
        let (mut t, host, log) = connect();
        // DA1, DA2, DSR, CPR, kitty query, OSC 10/11/4 color queries, CSI 14/18 t.
        output(
            &host,
            b"\x1b[c\x1b[>c\x1b[5n\x1b[6n\x1b[?u\x1b]10;?\x07\x1b]11;?\x07\x1b]4;1;?\x07\x1b[14t\x1b[18t",
        );
        settle(&mut t, &host, "queried");
        for _ in 0..5 {
            t.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(inputs(&log), b"", "the host answers queries, not this side");

        // OSC 52 read: answered here (the clipboard lives here).
        output(&host, b"\x1b]52;c;?\x07");
        until(&mut t, "clipboard reply", |_| !inputs(&log).is_empty());
        let sent = inputs(&log);
        assert!(sent.starts_with(b"\x1b]52;c;"), "{sent:?}");
        assert!(sent.ends_with(b"\x07"), "{sent:?}");
        assert_eq!(sent.iter().filter(|&&b| b == 0x1b).count(), 1, "{sent:?}");
    }

    #[test]
    fn resize_and_palette_forwarded() {
        let (mut t, _host, log) = connect();
        // The theme's palette goes out at once.
        let dark = Palette::for_theme(TermTheme::Dark).host_colors();
        until(&mut t, "initial palette", |_| !logged(&log).is_empty());
        assert_eq!(logged(&log), [Sent::Palette(dark)]);
        // The first size is always sent (the host's size is unknown here), even if it
        // equals the initial one; unchanged sizes are not repeated.
        t.resize(80, 24, 8, 16);
        t.resize(80, 24, 8, 16);
        t.resize(100, 30, 9, 18);
        t.resize(100, 30, 10, 20);
        {
            let term = t.term.lock();
            assert_eq!((term.columns(), term.screen_lines()), (100, 30));
        }
        t.set_theme(TermTheme::Light);
        t.set_theme(TermTheme::Light);
        let light = Palette::for_theme(TermTheme::Light).host_colors();
        assert_ne!(light, dark);
        let want = [
            Sent::Palette(dark),
            Sent::Resize(80, 24, 8, 16),
            Sent::Resize(100, 30, 9, 18),
            Sent::Resize(100, 30, 10, 20),
            Sent::Palette(light),
        ];
        until(&mut t, "forwarded", |_| logged(&log).len() >= want.len());
        assert_eq!(logged(&log), want);
        // Light palette layout: ANSI 0..=15, foreground, background, cursor.
        let p = Palette::for_theme(TermTheme::Light);
        let rgb = |c: alacritty_terminal::vte::ansi::Rgb| [c.r, c.g, c.b];
        assert_eq!(light[1], rgb(p.ansi[1]));
        assert_eq!(
            light[16..],
            [rgb(p.foreground), rgb(p.background), rgb(p.cursor)]
        );
    }

    #[test]
    fn displaced_stops_input_without_exit() {
        let (mut t, host, log) = connect();
        t.write(b"before");
        until(&mut t, "input", |_| inputs(&log) == b"before");
        host.send(RemoteEvent::Displaced).expect("send");
        until(&mut t, "displaced", |t| t.is_displaced());
        assert_eq!(t.banner.as_deref(), Some("[attached elsewhere]"));
        // The connection is let go; a following Closed is no exit.
        until(&mut t, "detach", |_| {
            logged(&log).last() == Some(&Sent::Detach)
        });
        host.send(RemoteEvent::Closed).expect("send");
        std::thread::sleep(Duration::from_millis(50));
        let ev = t.poll();
        assert_eq!(ev.exited, None);
        assert!(!t.is_exited());
        t.write(b"after");
        t.resize(90, 20, 8, 16);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(inputs(&log), b"before");
        assert!(!logged(&log).iter().any(|s| matches!(s, Sent::Resize(..))));
    }

    #[test]
    fn exit_is_reported_once_and_closed_after_it_ignored() {
        let (mut t, host, _log) = connect();
        host.send(RemoteEvent::Exited(Some(3))).expect("send");
        host.send(RemoteEvent::Closed).expect("send");
        let mut exited = None;
        until(&mut t, "exit", |t| {
            exited = exited.or(t.poll().exited);
            exited.is_some()
        });
        assert_eq!(exited, Some(3));
        assert_eq!(t.exit_code(), Some(3));
        assert_eq!(t.banner.as_deref(), Some("[process exited: 3]"));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(t.poll().exited, None);

        let (mut t, host, _log) = connect();
        host.send(RemoteEvent::Exited(None)).expect("send");
        until(&mut t, "signal exit", |t| t.exit_code() == Some(-1));

        // Connection lost without an exit status.
        let (mut t, host, _log) = connect();
        drop(host);
        until(&mut t, "lost", |t| t.exit_code() == Some(-1));
        assert_eq!(
            t.banner.as_deref(),
            Some("[connection to the session host lost]")
        );
    }

    #[test]
    fn kill_and_drop_reach_the_host() {
        let (mut t, _host, log) = connect();
        t.kill();
        assert_eq!(t.exit_code(), Some(-1));
        until(&mut t, "kill", |_| logged(&log).last() == Some(&Sent::Kill));

        // Drop: input written before is delivered, then the session is detached (not killed).
        let (t, _host, log) = connect();
        t.write(b"typed");
        drop(t);
        wait_detached(TIMEOUT);
        let sent = logged(&log);
        assert_eq!(sent.last(), Some(&Sent::Detach));
        assert!(!sent.contains(&Sent::Kill));
        assert_eq!(inputs(&log), b"typed");
    }

    #[test]
    fn synchronized_update_flushes_after_timeout() {
        let (mut t, host, _log) = connect();
        output(&host, b"\x1b[?2026hpending");
        // Held back while the update is open; shown once the timeout (150 ms) passed even
        // though no further output arrives.
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(t.screen_text().lines[0], "");
        until(&mut t, "sync timeout", |t| {
            t.screen_text().lines[0] == "pending"
        });
    }

    #[test]
    fn paste_bytes_follow_bracketed_paste_and_key_rules() {
        let (mut t, host, _log) = connect();
        assert_eq!(t.paste_bytes("hi"), None, "?2004 off");

        output(&host, b"\x1b[?2004h");
        until(&mut t, "?2004", |t| {
            t.mode().contains(TermMode::BRACKETED_PASTE)
        });
        assert_eq!(
            t.paste_bytes("one\ntwo\r\nthree").as_deref(),
            Some(&b"\x1b[200~one\ntwo\r\nthree\x1b[201~"[..])
        );
        // An embedded end marker cannot close the bracket early.
        assert_eq!(
            t.paste_bytes("a\x1b[201~b").as_deref(),
            Some(&b"\x1b[200~ab\x1b[201~"[..])
        );
        assert_eq!(t.paste_bytes("").as_deref(), Some(&b""[..]));

        // win32-input-mode: ConPTY's paste filter (newlines to CR, ESC dropped), bracketed.
        output(&host, b"\x1b[?9001h");
        until(&mut t, "win32 mode", |t| t.key_rules() == KeyRules::Win32);
        assert_eq!(
            t.paste_bytes("a\nb\x1b[201~c").as_deref(),
            Some(&b"\x1b[200~a\rb[201~c\x1b[201~"[..])
        );

        output(&host, b"\x1b[?2004l");
        until(&mut t, "?2004 off", |t| {
            !t.mode().contains(TermMode::BRACKETED_PASTE)
        });
        assert_eq!(t.paste_bytes("a\nb"), None);
    }
}
