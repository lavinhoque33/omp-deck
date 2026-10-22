//! PTY wrapper that watches the child's output for xterm modifyOtherKeys requests
//! (`CSI > 4 ; n m`), which alacritty_terminal parses but ignores. Everything else is delegated
//! to alacritty's [`tty::Pty`].

use std::fs::File;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::tty::{self, ChildEvent, EventedPty, EventedReadWrite};
use polling::{Event, PollMode, Poller};

/// Byte-level recogniser for `CSI > Pp ; Pv m` and RIS (`ESC c`). Keeps its state between
/// reads, so sequences split across reads are recognised.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MokScanner {
    state: State,
    /// Parameters of the current `CSI >` sequence (only the first two matter).
    params: [u16; 2],
    /// Index of the parameter being parsed.
    index: u8,
    /// Any parameter byte seen (distinguishes `CSI > m` from `CSI > 0 m`).
    has_params: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Esc,
    /// After `ESC [`.
    Csi,
    /// After `ESC [ >`: collecting parameters.
    CsiGt,
}

impl MokScanner {
    /// Feeds child output; returns the new modifyOtherKeys level if a request was completed
    /// (the last one wins).
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Option<u8> {
        let mut level = None;
        for &b in bytes {
            if let Some(l) = self.step(b) {
                level = Some(l);
            }
        }
        level
    }

    fn step(&mut self, b: u8) -> Option<u8> {
        const ESC: u8 = 0x1b;
        match self.state {
            State::Ground => {
                if b == ESC {
                    self.state = State::Esc;
                }
            }
            State::Esc => {
                self.state = match b {
                    b'[' => State::Csi,
                    ESC => State::Esc,
                    b'c' => {
                        self.state = State::Ground;
                        return Some(0);
                    }
                    _ => State::Ground,
                }
            }
            State::Csi => {
                self.state = match b {
                    b'>' => {
                        self.params = [0; 2];
                        self.index = 0;
                        self.has_params = false;
                        State::CsiGt
                    }
                    ESC => State::Esc,
                    _ => State::Ground,
                }
            }
            State::CsiGt => match b {
                b'0'..=b'9' => {
                    self.has_params = true;
                    if let Some(p) = self.params.get_mut(usize::from(self.index)) {
                        *p = p.saturating_mul(10).saturating_add(u16::from(b - b'0'));
                    }
                }
                b';' => {
                    self.has_params = true;
                    self.index = self.index.saturating_add(1);
                }
                b'm' => {
                    self.state = State::Ground;
                    // `CSI > m` resets every resource; `CSI > 4 m` / `CSI > 4;0 m` reset
                    // modifyOtherKeys; `CSI > 4;n m` sets it.
                    return match self.params {
                        _ if !self.has_params => Some(0),
                        [4, n] => Some(u8::try_from(n).unwrap_or(u8::MAX)),
                        _ => None,
                    };
                }
                ESC => self.state = State::Esc,
                _ => self.state = State::Ground,
            },
        }
        None
    }
}

/// Reader half: a dup of the PTY master fd whose reads are scanned.
pub(crate) struct ScanReader {
    file: File,
    scanner: MokScanner,
    level: Arc<AtomicU8>,
}

impl Read for ScanReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.file.read(buf)?;
        if let Some(level) = self.scanner.feed(&buf[..n]) {
            self.level.store(level, Ordering::Relaxed);
        }
        Ok(n)
    }
}

/// Input bytes the event loop wrote to the PTY and when the last writer call succeeded (see
/// [`crate::Terminal`]'s detach).
#[derive(Default)]
pub(crate) struct WriteProgress {
    state: Mutex<Written>,
    changed: Condvar,
}

#[derive(Default)]
struct Written {
    bytes: u64,
    last_at: Option<Instant>,
}

impl WriteProgress {
    fn add(&self, n: usize) {
        let mut w = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        w.bytes += n as u64;
        w.last_at = Some(Instant::now());
        self.changed.notify_all();
    }

    pub(crate) fn written(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .bytes
    }

    /// When the last successful PTY writer call returned.
    pub(crate) fn last_write(&self) -> Option<Instant> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last_at
    }

    /// Blocks until at least `target` bytes were written or `timeout` passed; true on success.
    pub(crate) fn wait_for(&self, target: u64, timeout: Duration) -> bool {
        let w = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (w, _) = self
            .changed
            .wait_timeout_while(w, timeout, |w| w.bytes < target)
            .unwrap_or_else(PoisonError::into_inner);
        w.bytes >= target
    }
}

/// Writer half: a dup of the PTY master fd whose writes are counted.
pub(crate) struct CountingWriter {
    file: File,
    progress: Arc<WriteProgress>,
}

impl Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.file.write(buf)?;
        self.progress.add(n);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// [`tty::Pty`] plus the modifyOtherKeys tracker and the write counter. Field order matters:
/// the dup'ed fds close before `Pty`'s drop hangs up and reaps the child.
pub(crate) struct TrackedPty {
    reader: ScanReader,
    writer: CountingWriter,
    pty: tty::Pty,
}

impl TrackedPty {
    /// `level` receives the child's current modifyOtherKeys level (0 = off); `progress` counts
    /// the input bytes written.
    pub(crate) fn new(
        pty: tty::Pty,
        level: Arc<AtomicU8>,
        progress: Arc<WriteProgress>,
    ) -> io::Result<TrackedPty> {
        let file = pty.file().try_clone()?;
        let writer = CountingWriter {
            file: pty.file().try_clone()?,
            progress,
        };
        Ok(TrackedPty {
            reader: ScanReader {
                file,
                scanner: MokScanner::default(),
                level,
            },
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

#[cfg(test)]
mod tests {
    use super::*;

    fn levels(chunks: &[&[u8]]) -> Vec<Option<u8>> {
        let mut s = MokScanner::default();
        chunks.iter().map(|c| s.feed(c)).collect()
    }

    #[test]
    fn set_and_reset() {
        assert_eq!(levels(&[b"\x1b[>4;2m"]), [Some(2)]);
        assert_eq!(levels(&[b"\x1b[>4;1m"]), [Some(1)]);
        assert_eq!(levels(&[b"\x1b[>4m"]), [Some(0)]);
        assert_eq!(levels(&[b"\x1b[>4;0m"]), [Some(0)]);
        assert_eq!(levels(&[b"\x1b[>m"]), [Some(0)]);
        assert_eq!(levels(&[b"\x1bc"]), [Some(0)]);
        // Last request in a chunk wins.
        assert_eq!(levels(&[b"\x1b[>4;2mtext\x1b[>4m"]), [Some(0)]);
    }

    #[test]
    fn split_across_reads() {
        assert_eq!(
            levels(&[b"ab\x1b", b"[", b">4", b";", b"2", b"m"]),
            [None, None, None, None, None, Some(2)]
        );
        assert_eq!(levels(&[b"x\x1b", b"c"]), [None, Some(0)]);
    }

    #[test]
    fn ignores_other_sequences() {
        // SGR, kitty push, DA2 query, other `CSI >` resources, private modes.
        assert_eq!(
            levels(&[b"\x1b[4;2m\x1b[>1u\x1b[>c\x1b[>1;2m\x1b[?4;2m\x1b[>4;2x"]),
            [None]
        );
        // An ESC aborts a sequence and starts a new one.
        assert_eq!(levels(&[b"\x1b[>4\x1b[>4;2m"]), [Some(2)]);
        // Huge parameters saturate instead of overflowing.
        assert_eq!(levels(&[b"\x1b[>4;99999999m"]), [Some(255)]);
        // `CSI > 0 m` resets modifyKeyboard, not modifyOtherKeys.
        assert_eq!(levels(&[b"\x1b[>4;2m\x1b[>0m"]), [Some(2)]);
    }
}
