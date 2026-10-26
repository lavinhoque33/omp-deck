//! Unix local PTY driver: alacritty's [`tty::Pty`] (spawn with env/cwd, `setsid`, SIGCHLD
//! pipe, `TIOCSWINSZ`, SIGHUP + reap on drop) run by our own I/O loop instead of alacritty's
//! `EventLoop`, so every chunk read goes through the [`Engine`]'s single critical section
//! (parse, scanners, publication) — the event loop's read tap and wake-up are not an atomic
//! publication boundary. Loop structure and order follow alacritty 0.26's `EventLoop`: level
//! polling of the PTY and the child-signal pipe, queued writes with write interest only while
//! bytes wait, a bounded read batch per wake-up, the synchronized-update timeout, and a final
//! drain of the output when the child exits.

use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read, Write};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener, OnResize, WindowSize};
use alacritty_terminal::tty::{self, ChildEvent, EventedPty, EventedReadWrite};
use polling::{Event as PollEvent, Events, PollMode, Poller};

use crate::core::Listener;
use crate::engine::{Engine, OutputEvent};
use crate::out::WriteProgress;

/// Keys `tty::Pty::register` assigns: the PTY master and the SIGCHLD pipe.
const PTY_IO: usize = 0;
const CHILD_SIGNAL: usize = 1;
/// One read.
const READ_CHUNK: usize = 0x1_0000;
/// Bytes read per wake-up before queued messages (input, resize) are served again.
const READ_BATCH: usize = 0x10_0000;

/// Work for the PTY loop, in order.
pub(crate) enum Msg {
    Input(Vec<u8>),
    Resize(WindowSize),
    /// Ends the loop; dropping the Pty hangs up the child.
    Shutdown,
}

/// Sending side of a PTY loop (wakes its poller).
pub(crate) struct LoopSender {
    tx: Sender<Msg>,
    poller: Arc<Poller>,
}

impl LoopSender {
    /// False once the loop is gone.
    pub(crate) fn send(&self, msg: Msg) -> bool {
        let sent = self.tx.send(msg).is_ok();
        if sent && let Err(e) = self.poller.notify() {
            log::warn!("pty loop wake-up: {e}");
        }
        sent
    }
}

/// A PTY loop whose sender exists before it runs (so no reply can be lost).
pub(crate) struct PtyLoop {
    pty: tty::Pty,
    poller: Arc<Poller>,
    rx: Receiver<Msg>,
    engine: Arc<Engine>,
    listener: Listener,
    progress: Arc<WriteProgress>,
}

impl PtyLoop {
    pub(crate) fn new(
        pty: tty::Pty,
        engine: Arc<Engine>,
        listener: Listener,
        progress: Arc<WriteProgress>,
    ) -> io::Result<(PtyLoop, LoopSender)> {
        let poller = Arc::new(Poller::new()?);
        let (tx, rx) = mpsc::channel();
        let sender = LoopSender {
            tx,
            poller: poller.clone(),
        };
        Ok((
            PtyLoop {
                pty,
                poller,
                rx,
                engine,
                listener,
                progress,
            },
            sender,
        ))
    }

    /// Starts the loop thread (detached: it owns the Pty and drops it at the end).
    pub(crate) fn spawn(self) -> io::Result<()> {
        std::thread::Builder::new()
            .name("deck-term-pty".into())
            .spawn(move || self.run())
            .map(drop)
    }

    fn run(mut self) {
        let end = self.serve();
        let _ = self.pty.deregister(&self.poller);
        self.engine.finish(end);
        // `self.pty` drops here: SIGHUP to the child, then reaped.
    }

    /// Runs until shutdown, child exit or an I/O error; returns the subscribers' final event.
    fn serve(&mut self) -> OutputEvent {
        let mut interest = PollEvent::readable(PTY_IO);
        let mode = PollMode::Level;
        // SAFETY: the Pty (and its fds) outlive the registration: it is deregistered in
        // `run` before the Pty is dropped.
        if let Err(e) = unsafe { self.pty.register(&self.poller, interest, mode) } {
            log::error!("pty loop registration: {e}");
            return OutputEvent::Closed;
        }
        let mut events = Events::with_capacity(NonZeroUsize::new(64).unwrap_or(NonZeroUsize::MIN));
        let mut buf = vec![0u8; READ_CHUNK];
        let mut writes = Writes::default();
        loop {
            let timeout = self
                .engine
                .sync_deadline()
                .map(|at| at.saturating_duration_since(Instant::now()));
            events.clear();
            if let Err(e) = self.poller.wait(&mut events, timeout) {
                if e.kind() == ErrorKind::Interrupted {
                    continue;
                }
                log::error!("pty loop polling: {e}");
                return OutputEvent::Closed;
            }
            // The app never ended its synchronized update: show what it drew.
            self.engine.flush_sync_due();

            loop {
                match self.rx.try_recv() {
                    Ok(Msg::Input(bytes)) => writes.queue.push_back(bytes),
                    Ok(Msg::Resize(size)) => self.pty.on_resize(size),
                    Ok(Msg::Shutdown) | Err(TryRecvError::Disconnected) => {
                        return OutputEvent::Closed;
                    }
                    Err(TryRecvError::Empty) => break,
                }
            }

            for event in events.iter() {
                match event.key {
                    CHILD_SIGNAL => {
                        if let Some(ChildEvent::Exited(status)) = self.pty.next_child_event() {
                            if let Some(status) = status {
                                self.listener.send_event(Event::ChildExit(status));
                            }
                            // Drain what the child wrote before it exited.
                            let _ = self.read(&mut buf);
                            self.engine
                                .finish(OutputEvent::Exited(status.and_then(|s| s.code())));
                            self.listener.send_event(Event::Exit);
                            self.listener.send_event(Event::Wakeup);
                            return OutputEvent::Closed;
                        }
                    }
                    PTY_IO => {
                        if event.is_interrupt() {
                            // No I/O on a dead PTY; the exit event follows.
                            continue;
                        }
                        if event.readable
                            && let Err(e) = self.read(&mut buf)
                        {
                            // Linux: EIO once the child side hung up; the exit event follows.
                            if hung_up(&e) {
                                continue;
                            }
                            log::error!("pty read: {e}");
                            return OutputEvent::Closed;
                        }
                        if event.writable
                            && let Err(e) = writes.flush(self.pty.file(), &self.progress)
                        {
                            log::error!("pty write: {e}");
                            return OutputEvent::Closed;
                        }
                    }
                    _ => {}
                }
            }

            let needs_write = !writes.queue.is_empty();
            if needs_write != interest.writable {
                interest.writable = needs_write;
                if let Err(e) = self.pty.reregister(&self.poller, interest, mode) {
                    log::error!("pty loop re-registration: {e}");
                    return OutputEvent::Closed;
                }
            }
        }
    }

    /// Reads until the PTY would block or one batch was read, feeding every chunk to the
    /// engine as it arrives.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<()> {
        let mut total = 0;
        while total < READ_BATCH {
            match self.pty.file().read(buf) {
                Ok(0) => break,
                Ok(n) => {
                    self.engine.feed(&buf[..n]);
                    total += n;
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// Linux: a PTY master read fails with `EIO` once the child side hung up.
fn hung_up(e: &io::Error) -> bool {
    /// `EIO` on every Linux architecture.
    const EIO: i32 = 5;
    cfg!(target_os = "linux") && e.raw_os_error() == Some(EIO)
}

/// Input waiting for the PTY to accept it.
#[derive(Default)]
struct Writes {
    queue: VecDeque<Vec<u8>>,
    /// Bytes of the front entry already written.
    offset: usize,
}

impl Writes {
    /// Writes until the PTY would block; every successful write call is counted.
    fn flush(&mut self, mut file: &std::fs::File, progress: &WriteProgress) -> io::Result<()> {
        while let Some(front) = self.queue.front() {
            match file.write(&front[self.offset..]) {
                Ok(0) => break,
                Ok(n) => {
                    progress.record(n, true);
                    self.offset += n;
                    if self.offset == front.len() {
                        self.queue.pop_front();
                        self.offset = 0;
                    }
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {
                    break;
                }
                Err(e) => {
                    progress.record(0, false);
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}
