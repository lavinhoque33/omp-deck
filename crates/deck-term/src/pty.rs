//! Unix PTY wrapper (local terminals): runs the child's output through the
//! [`OutputScan`] scanners and counts written input. Everything else is delegated to
//! alacritty's [`tty::Pty`].

use std::fs::File;
use std::io::{self, Read, Write};
use std::sync::Arc;

use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::tty::{self, ChildEvent, EventedPty, EventedReadWrite};
use polling::{Event, PollMode, Poller};

use crate::out::WriteProgress;
use crate::scan::OutputScan;

/// Reader half: a dup of the PTY master fd whose reads are scanned (bytes pass unchanged).
pub(crate) struct ScanReader {
    file: File,
    scan: OutputScan,
}

impl Read for ScanReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.file.read(buf)?;
        self.scan.feed(&buf[..n]);
        Ok(n)
    }
}

/// Writer half: a dup of the PTY master fd whose writes are counted.
pub(crate) struct CountingWriter {
    file: File,
    progress: Arc<WriteProgress>,
}

impl Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let result = self.file.write(buf);
        match &result {
            Ok(n) => self.progress.record(*n, true),
            Err(e)
                if !matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                self.progress.record(0, false);
            }
            Err(_) => {}
        }
        result
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// [`tty::Pty`] plus the output scanners and the write counter. Field order matters:
/// the dup'ed fds close before `Pty`'s drop hangs up and reaps the child.
pub(crate) struct TrackedPty {
    reader: ScanReader,
    writer: CountingWriter,
    pty: tty::Pty,
}

impl TrackedPty {
    /// `scan` watches the child's output; `progress` counts the input bytes written.
    pub(crate) fn new(
        pty: tty::Pty,
        scan: OutputScan,
        progress: Arc<WriteProgress>,
    ) -> io::Result<TrackedPty> {
        let file = pty.file().try_clone()?;
        let writer = CountingWriter {
            file: pty.file().try_clone()?,
            progress,
        };
        Ok(TrackedPty {
            reader: ScanReader { file, scan },
            writer,
            pty,
        })
    }
}

impl EventedReadWrite for TrackedPty {
    type Reader = ScanReader;
    type Writer = CountingWriter;

    unsafe fn register(
        &mut self,
        poll: &Arc<Poller>,
        interest: Event,
        mode: PollMode,
    ) -> io::Result<()> {
        // SAFETY: forwarded contract — the Pty (and its fds) live as long as this wrapper,
        // which the event loop deregisters before dropping.
        unsafe { self.pty.register(poll, interest, mode) }
    }

    fn reregister(
        &mut self,
        poll: &Arc<Poller>,
        interest: Event,
        mode: PollMode,
    ) -> io::Result<()> {
        self.pty.reregister(poll, interest, mode)
    }

    fn deregister(&mut self, poll: &Arc<Poller>) -> io::Result<()> {
        self.pty.deregister(poll)
    }

    fn reader(&mut self) -> &mut ScanReader {
        &mut self.reader
    }

    fn writer(&mut self) -> &mut CountingWriter {
        &mut self.writer
    }
}

impl EventedPty for TrackedPty {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        self.pty.next_child_event()
    }
}

impl OnResize for TrackedPty {
    fn on_resize(&mut self, window_size: WindowSize) {
        self.pty.on_resize(window_size);
    }
}
