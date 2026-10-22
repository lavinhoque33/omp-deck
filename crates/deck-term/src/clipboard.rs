//! The app's single system clipboard (arboard; CLIPBOARD + PRIMARY), owned by a dedicated
//! worker thread so a slow or hung clipboard owner never stalls the UI thread.

use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use arboard::{GetExtLinux, LinuxClipboardKind, SetExtLinux};

/// How long a user-initiated paste waits for the clipboard owner before giving up.
const READ_TIMEOUT: Duration = Duration::from_millis(500);

/// Receives the clipboard text (None = empty/unavailable) on the worker thread.
type ReadCallback = Box<dyn FnOnce(Option<String>) + Send>;

enum Request {
    Set(LinuxClipboardKind, String),
    Get(LinuxClipboardKind, Sender<Option<String>>),
    Load(LinuxClipboardKind, ReadCallback),
}

/// Shared system clipboard. Clone = same worker. The worker connects to the display on first
/// use (if that fails every call is a logged no-op / `None`) and exits when the last clone is
/// dropped.
#[derive(Clone)]
pub struct Clipboard {
    tx: Sender<Request>,
}

impl Default for Clipboard {
    fn default() -> Self {
        Clipboard::new()
    }
}

impl std::fmt::Debug for Clipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clipboard").finish_non_exhaustive()
    }
}

impl Clipboard {
    pub fn new() -> Clipboard {
        let (tx, rx) = mpsc::channel();
        if let Err(e) = std::thread::Builder::new()
            .name("deck-clipboard".into())
            .spawn(move || worker(rx))
        {
            // The receiver is gone with the closure: sends fail and reads return None.
            log::warn!("clipboard worker could not start: {e}");
        }
        Clipboard { tx }
    }

    fn request(&self, req: Request) {
        if self.tx.send(req).is_err() {
            log::debug!("clipboard worker is not running");
        }
    }

    /// Blocks up to [`READ_TIMEOUT`].
    fn get(&self, kind: LinuxClipboardKind) -> Option<String> {
        let (tx, rx) = mpsc::channel();
        self.request(Request::Get(kind, tx));
        match rx.recv_timeout(READ_TIMEOUT) {
            Ok(text) => text,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                log::warn!("clipboard read ({kind:?}) timed out after {READ_TIMEOUT:?}");
                None
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => None,
        }
    }

    /// CLIPBOARD contents (user-initiated paste; waits at most 500 ms).
    pub fn get_text(&self) -> Option<String> {
        self.get(LinuxClipboardKind::Clipboard)
    }

    /// Replaces CLIPBOARD (fire-and-forget).
    pub fn set_text(&self, text: &str) {
        self.request(Request::Set(LinuxClipboardKind::Clipboard, text.to_owned()));
    }

    /// PRIMARY selection contents (user-initiated paste; waits at most 500 ms).
    pub fn get_primary(&self) -> Option<String> {
        self.get(LinuxClipboardKind::Primary)
    }

    /// Replaces PRIMARY (fire-and-forget).
    pub fn set_primary(&self, text: &str) {
        self.request(Request::Set(LinuxClipboardKind::Primary, text.to_owned()));
    }

    /// Reads the clipboard without blocking the caller: `done` runs on the worker thread once
    /// the read completes (OSC 52 queries answer the PTY from there).
    pub(crate) fn load(&self, primary: bool, done: impl FnOnce(Option<String>) + Send + 'static) {
        let kind = if primary {
            LinuxClipboardKind::Primary
        } else {
            LinuxClipboardKind::Clipboard
        };
        self.request(Request::Load(kind, Box::new(done)));
    }
}

enum State {
    Uninit,
    Ready(arboard::Clipboard),
    Failed,
}

fn worker(rx: Receiver<Request>) {
    let mut state = State::Uninit;
    while let Ok(req) = rx.recv() {
        if matches!(state, State::Uninit) {
            state = match arboard::Clipboard::new() {
                Ok(cb) => State::Ready(cb),
                Err(e) => {
                    log::warn!("system clipboard unavailable: {e}");
                    State::Failed
                }
            };
        }
        let cb = match &mut state {
            State::Ready(cb) => Some(cb),
            _ => None,
        };
        match req {
            Request::Set(kind, text) => {
                if let Some(cb) = cb
                    && let Err(e) = cb.set().clipboard(kind).text(text)
                {
                    log::debug!("clipboard write ({kind:?}) failed: {e}");
                }
            }
            Request::Get(kind, reply) => {
                // The requester may have timed out already.
                let _ = reply.send(cb.and_then(|cb| read(cb, kind)));
            }
            Request::Load(kind, done) => done(cb.and_then(|cb| read(cb, kind))),
        }
    }
}

fn read(cb: &mut arboard::Clipboard, kind: LinuxClipboardKind) -> Option<String> {
    match cb.get().clipboard(kind).text() {
        Ok(t) => Some(t),
        Err(arboard::Error::ContentNotAvailable) => None,
        Err(e) => {
            log::debug!("clipboard read ({kind:?}) failed: {e}");
            None
        }
    }
}
