//! Per-view frontend links: one gateway connection per native terminal view, carrying the binary
//! `F_*` / `U_*` frames of [`crate::gateway::protocol::kind`].
//!
//! The receiving half reconnects on its own after a lost link (reporting
//! [`RemoteEvent::Disconnected`] once, then a fresh [`RemoteEvent::Snapshot`] from an
//! identity-checked re-attach). The sending half writes only while a link is up: input, focus,
//! geometry and palette sent while it is down are refused and dropped, never queued; the terminal
//! sends its desired view state again after the next snapshot.

use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use deck_host::transport::{self, Stream};
use deck_term::{Frontend, FrontendState, FrontendTx, InputMeta, RemoteEvent, RemoteRx};

use super::{Handshake, Shared, handshake};
use crate::controller::SessionTarget;
use crate::gateway::protocol::{
    self, Attach, Hello, InputRefused, PROTOCOL, Refusal, ViewEnd, ViewRole, kind,
};
use crate::gateway::wire::{self, Assembly};
use crate::gateway::{GATEWAY_VERSION, OWNER_ENDPOINT};

/// First retry delay after a lost or refused view link.
const RETRY_MIN: Duration = Duration::from_millis(200);
/// Longest retry delay.
const RETRY_MAX: Duration = Duration::from_secs(5);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A view of one session, before [`Frontend::split`].
pub(crate) struct ViewFrontend {
    pub(crate) shared: Arc<Shared>,
    pub(crate) target: SessionTarget,
    pub(crate) view: ViewRole,
}

impl Frontend for ViewFrontend {
    fn split(self: Box<Self>) -> (Box<dyn RemoteRx>, Box<dyn FrontendTx>) {
        let link = Arc::new(ViewLink {
            shared: self.shared,
            target: self.target,
            view: self.view,
            state: Mutex::new(LinkState::default()),
            cv: Condvar::new(),
        });
        (
            Box::new(ViewRx {
                link: link.clone(),
                reader: None,
                assembly: Assembly::default(),
                reported_down: false,
                finished: false,
                delay: Duration::ZERO,
            }),
            Box::new(ViewTx { link }),
        )
    }
}

/// The writing side of the current connection.
struct Writer {
    stream: Mutex<Stream>,
}

#[derive(Default)]
struct LinkState {
    /// `None` while no link is up: sends are refused.
    writer: Option<Arc<Writer>>,
    /// A clone used only to shut the current connection down (never blocks on a writer).
    stopper: Option<Stream>,
    /// Detached, killed-and-ended or ended by the gateway: no reconnect.
    closed: bool,
}

struct ViewLink {
    shared: Arc<Shared>,
    target: SessionTarget,
    view: ViewRole,
    state: Mutex<LinkState>,
    cv: Condvar,
}

impl ViewLink {
    fn closed(&self) -> bool {
        lock(&self.state).closed
    }

    /// Ends the current connection (if any); the receiver notices and may reconnect.
    fn drop_conn(&self) {
        let mut st = lock(&self.state);
        st.writer = None;
        if let Some(s) = st.stopper.take() {
            s.shutdown();
        }
    }

    /// Ends the link for good.
    fn close(&self) {
        let mut st = lock(&self.state);
        st.closed = true;
        st.writer = None;
        if let Some(s) = st.stopper.take() {
            s.shutdown();
        }
        drop(st);
        self.cv.notify_all();
    }

    /// Sleeps up to `d` unless the link is closed meanwhile; `true` when closed.
    fn sleep(&self, d: Duration) -> bool {
        let st = lock(&self.state);
        let (st, _) = self
            .cv
            .wait_timeout_while(st, d, |s| !s.closed)
            .unwrap_or_else(PoisonError::into_inner);
        st.closed
    }

    fn send(&self, kind: u8, payload: &[u8]) -> io::Result<()> {
        let writer = {
            let st = lock(&self.state);
            if st.closed {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "the view was closed",
                ));
            }
            st.writer.clone().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    "not connected to the OmpDeck gateway; dropped",
                )
            })?
        };
        let result = {
            let mut s = lock(&writer.stream);
            wire::write_frame(&mut *s, kind, payload)
        };
        if result.is_err() {
            // This connection is broken: the receiver sees it end and reconnects.
            let mut st = lock(&self.state);
            if st.writer.as_ref().is_some_and(|w| Arc::ptr_eq(w, &writer)) {
                st.writer = None;
                if let Some(s) = st.stopper.take() {
                    s.shutdown();
                }
            }
        }
        result
    }

    /// Connects and attaches this view.
    fn connect(&self) -> Result<Stream, ViewConnect> {
        let shared = &self.shared;
        let endpoint = shared
            .profile
            .id()
            .namespace()
            .and_then(|ns| transport::Endpoint::new(&ns, OWNER_ENDPOINT))
            .map_err(|e| ViewConnect::Retry(e.to_string()))?;
        let mut stream =
            transport::connect(&endpoint).map_err(|e| ViewConnect::Retry(e.to_string()))?;
        let hello = Hello {
            protocol: PROTOCOL,
            version: GATEWAY_VERSION.to_owned(),
            role: shared.role,
            client_id: shared.client_id.clone(),
            pid: std::process::id(),
            profile: shared.profile.clone(),
            attach: Some(Attach {
                target: self.target.clone(),
                view: self.view,
            }),
        };
        match handshake(&mut stream, &hello, &shared.profile) {
            Handshake::Welcome(w) if w.epoch != self.target.epoch => {
                stream.shutdown();
                Err(ViewConnect::Ended("the gateway restarted".into()))
            }
            Handshake::Welcome(_) => Ok(stream),
            Handshake::Refused(r @ (Refusal::Stale { .. } | Refusal::NotAllowed(_))) => {
                Err(ViewConnect::Ended(r.to_string()))
            }
            Handshake::Refused(r) => Err(ViewConnect::Retry(r.to_string())),
            Handshake::Failed(e) => Err(ViewConnect::Retry(e)),
        }
    }
}

enum ViewConnect {
    /// The view cannot attach any more (session changed, not allowed): it ends.
    Ended(String),
    /// Try again later.
    Retry(String),
}

struct ViewRx {
    link: Arc<ViewLink>,
    reader: Option<Stream>,
    assembly: Assembly,
    /// `Disconnected` was returned and no snapshot arrived since.
    reported_down: bool,
    /// The process exited or the gateway ended the view: only `Closed` follows.
    finished: bool,
    /// Wait before the next connect attempt.
    delay: Duration,
}

impl ViewRx {
    fn lose(&mut self, delay: Duration) {
        self.reader = None;
        self.assembly = Assembly::default();
        self.link.drop_conn();
        self.delay = delay;
    }

    fn backoff(&self) -> Duration {
        (self.delay * 2).clamp(RETRY_MIN, RETRY_MAX)
    }

    /// The link is lost: report it once, then reconnect.
    fn disconnected(&mut self, delay: Duration) -> Option<RemoteEvent> {
        self.lose(delay);
        if self.link.closed() {
            self.finished = true;
            return Some(RemoteEvent::Closed);
        }
        (!std::mem::replace(&mut self.reported_down, true)).then_some(RemoteEvent::Disconnected)
    }

    fn end(&mut self) -> RemoteEvent {
        self.finished = true;
        self.reader = None;
        self.link.close();
        RemoteEvent::Closed
    }

    /// Waits out the retry delay and attaches; an event to report, or `None` to go on.
    fn attach(&mut self) -> Option<RemoteEvent> {
        if !self.delay.is_zero() && self.link.sleep(self.delay) {
            return Some(self.end());
        }
        match self.link.connect() {
            Ok(stream) => {
                let (Ok(reader), Ok(writer), Ok(stopper)) =
                    (stream.try_clone(), stream.try_clone(), stream.try_clone())
                else {
                    stream.shutdown();
                    let d = self.backoff();
                    return self.disconnected(d);
                };
                let mut st = lock(&self.link.state);
                if st.closed {
                    stream.shutdown();
                    drop(st);
                    return Some(self.end());
                }
                st.writer = Some(Arc::new(Writer {
                    stream: Mutex::new(writer),
                }));
                st.stopper = Some(stopper);
                drop(st);
                self.reader = Some(reader);
                self.assembly = Assembly::default();
                None
            }
            Err(ViewConnect::Ended(why)) => {
                log::info!("frontend view ended: {why}");
                Some(self.end())
            }
            Err(ViewConnect::Retry(why)) => {
                log::debug!("frontend view not connected: {why}");
                let d = self.backoff();
                self.disconnected(d)
            }
        }
    }
}

impl RemoteRx for ViewRx {
    fn recv(&mut self) -> io::Result<RemoteEvent> {
        loop {
            if self.finished || self.link.closed() {
                self.finished = true;
                return Ok(RemoteEvent::Closed);
            }
            if self.reader.is_none() {
                if let Some(ev) = self.attach() {
                    return Ok(ev);
                }
                continue;
            }
            let frame = match self.reader.as_mut().map(wire::read_frame) {
                Some(Ok(Some(f))) => f,
                _ => {
                    if let Some(ev) = self.disconnected(RETRY_MIN) {
                        return Ok(ev);
                    }
                    continue;
                }
            };
            match decode(frame, &mut self.assembly) {
                Decoded::Event(ev) => {
                    if matches!(ev, RemoteEvent::Snapshot(_)) {
                        self.reported_down = false;
                        self.delay = Duration::ZERO;
                    }
                    if matches!(ev, RemoteEvent::Exited(_)) {
                        // A real exit: the view is over after it.
                        self.finished = true;
                        self.reader = None;
                        self.link.close();
                    }
                    return Ok(ev);
                }
                Decoded::Nothing => {}
                Decoded::Ended(ViewEnd::Closed | ViewEnd::Stale) => return Ok(self.end()),
                Decoded::Ended(ViewEnd::Overflow) => {
                    // Lagged: re-attach at once for a fresh snapshot.
                    if let Some(ev) = self.disconnected(Duration::ZERO) {
                        return Ok(ev);
                    }
                }
                Decoded::Ended(ViewEnd::Disconnected | ViewEnd::Shutdown) => {
                    if let Some(ev) = self.disconnected(RETRY_MIN) {
                        return Ok(ev);
                    }
                }
                Decoded::Invalid(why) => {
                    log::warn!("frontend view: {why}");
                    if let Some(ev) = self.disconnected(RETRY_MIN) {
                        return Ok(ev);
                    }
                }
            }
        }
    }
}

enum Decoded {
    Event(RemoteEvent),
    Ended(ViewEnd),
    /// A frame that produces no event (a snapshot chunk, a refusal note).
    Nothing,
    Invalid(String),
}

fn decode((k, payload): (u8, Vec<u8>), assembly: &mut Assembly) -> Decoded {
    fn json<T: serde::de::DeserializeOwned>(b: &[u8]) -> Result<T, String> {
        serde_json::from_slice(b).map_err(|e| e.to_string())
    }
    match k {
        kind::F_SNAPSHOT => match assembly.push(&payload) {
            Ok(Some((_, bytes))) => Decoded::Event(RemoteEvent::Snapshot(bytes)),
            Ok(None) => Decoded::Nothing,
            Err(e) => Decoded::Invalid(format!("snapshot: {e}")),
        },
        kind::F_OUTPUT => Decoded::Event(RemoteEvent::Output(payload)),
        kind::F_RESIZE => match protocol::decode_geometry(&payload) {
            Some((cols, rows, cell_width, cell_height)) => Decoded::Event(RemoteEvent::Resize {
                cols,
                rows,
                cell_width,
                cell_height,
            }),
            None => Decoded::Invalid("bad geometry frame".into()),
        },
        kind::F_STATE => match json::<FrontendState>(&payload) {
            Ok(s) => Decoded::Event(RemoteEvent::State(s)),
            Err(e) => Decoded::Invalid(format!("state: {e}")),
        },
        kind::F_EXITED => match json::<Option<i32>>(&payload) {
            Ok(code) => Decoded::Event(RemoteEvent::Exited(code)),
            Err(e) => Decoded::Invalid(format!("exit: {e}")),
        },
        kind::F_REFUSED => {
            match json::<InputRefused>(&payload) {
                Ok(r) => log::debug!("frontend write refused: {r:?}"),
                Err(e) => log::debug!("frontend write refused (undecodable: {e})"),
            }
            Decoded::Nothing
        }
        kind::F_ENDED => match json::<ViewEnd>(&payload) {
            Ok(end) => Decoded::Ended(end),
            Err(e) => Decoded::Invalid(format!("view end: {e}")),
        },
        other => Decoded::Invalid(format!("unexpected frame kind {other}")),
    }
}

struct ViewTx {
    link: Arc<ViewLink>,
}

impl FrontendTx for ViewTx {
    fn input(&self, bytes: &[u8], meta: InputMeta) -> io::Result<()> {
        self.link
            .send(kind::U_INPUT, &protocol::encode_input(bytes, meta))
    }

    fn clipboard_reply(&self, bytes: &[u8]) -> io::Result<()> {
        self.link.send(kind::U_CLIP, bytes)
    }

    fn focus(&self, focused: bool) -> io::Result<()> {
        self.link.send(kind::U_FOCUS, &[u8::from(focused)])
    }

    fn resize(&self, cols: u16, rows: u16, cell_width: u16, cell_height: u16) -> io::Result<()> {
        self.link.send(
            kind::U_RESIZE,
            &protocol::encode_geometry(cols, rows, cell_width, cell_height),
        )
    }

    fn palette(&self, palette: [[u8; 3]; 19]) -> io::Result<()> {
        self.link
            .send(kind::U_PALETTE, &protocol::encode_palette(&palette))
    }

    fn kill(&self) -> io::Result<()> {
        self.link.send(kind::U_KILL, &[])
    }

    fn detach(&self) -> io::Result<()> {
        if self.link.closed() {
            return Ok(());
        }
        // Best effort: the gateway also ends the view when the connection closes.
        let said = self.link.send(kind::U_DETACH, &[]);
        self.link.close();
        if let Err(e) = said {
            log::debug!("frontend detach without a link: {e}");
        }
        Ok(())
    }
}

impl Drop for ViewTx {
    fn drop(&mut self) {
        let _ = self.detach();
    }
}
