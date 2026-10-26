//! The singleton owner (`omp-deck-gateway serve`): binds the private endpoint, runs the
//! handshake of every connection and the per-connection reader/writer threads, and runs the
//! [`super::engine`] on the calling thread until the lifetime ends.
//!
//! A control link has a bounded event queue: a client that lags beyond it loses the queued
//! events and gets one [`ServerEvent::Resync`] instead, so a slow client never grows the
//! owner's memory or delays others. Replies have their own queue ahead of events (a client
//! bounds them by its outstanding requests). A frontend view has its own connection and
//! writer; its terminal output is bounded by the terminal's own output subscription.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use deck_host::transport::{Endpoint, Listener, Stream};
use deck_term::{OutputEvent, OutputSubscription};

use super::engine::{self, Msg, ViewOp};
use super::protocol::{
    self, Envelope, Hello, InputRefused, PROTOCOL, Refusal, Reply, ServerEvent, ViewEnd, Welcome,
    kind,
};
use super::wire::{self, Assembly};
use super::{OWNER_ENDPOINT, Profile};

/// How long a connection may take to say hello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the handshake waits for the engine's verdict.
const VERDICT_TIMEOUT: Duration = Duration::from_secs(10);
/// Queued events per control link before it is declared lagging.
pub(crate) const EVENT_QUEUE: usize = 1024;
/// Queued event bytes per control link before it is declared lagging.
const EVENT_BYTES: usize = 8 << 20;
/// Out-of-band items of one view; a view that does not read them is ended.
const SIDE_ITEMS: usize = 256;

/// Exit codes of `serve`.
pub mod exit {
    /// Clean exit (also: another owner already serves this identity).
    pub const OK: i32 = 0;
    pub const FAILED: i32 = 1;
    /// The maintenance barrier is up.
    pub const BARRIER: i32 = 5;
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---- control link outbox ----------------------------------------------------------------------

#[derive(Default)]
struct OutQ {
    replies: VecDeque<Vec<u8>>,
    events: VecDeque<Vec<u8>>,
    event_bytes: usize,
    lagged: bool,
    closed: bool,
}

/// What the engine sends to one control link; drained by that link's writer thread.
#[derive(Default)]
pub(crate) struct Outbox {
    q: Mutex<OutQ>,
    cv: Condvar,
}

impl Outbox {
    /// Queues a reply as chunked `REPLY` frames (chunk id = request id). A reply too large to
    /// reassemble is replaced by [`protocol::GatewayError::TooLarge`].
    pub(crate) fn reply(&self, id: u64, reply: &Reply) {
        let mut bytes = serde_json::to_vec(reply).unwrap_or_default();
        if bytes.len() > wire::MAX_ASSEMBLED {
            let r: Reply = Err(protocol::GatewayError::TooLarge);
            bytes = serde_json::to_vec(&r).unwrap_or_default();
        }
        let mut q = lock(&self.q);
        if q.closed {
            return;
        }
        for chunk in wire::chunks(id, &bytes) {
            q.replies.push_back(frame(kind::REPLY, &chunk));
        }
        drop(q);
        self.cv.notify_one();
    }

    /// Queues an event; a full queue is dropped for one `Resync`.
    pub(crate) fn event(&self, ev: &ServerEvent) {
        let Ok(bytes) = serde_json::to_vec(ev) else {
            return;
        };
        let mut q = lock(&self.q);
        if q.closed || q.lagged {
            return;
        }
        if q.events.len() >= EVENT_QUEUE || q.event_bytes + bytes.len() > EVENT_BYTES {
            q.events.clear();
            q.event_bytes = 0;
            q.lagged = true;
        } else {
            q.event_bytes += bytes.len();
            q.events.push_back(frame(kind::EVENT, &bytes));
        }
        drop(q);
        self.cv.notify_one();
    }

    /// No more frames; the writer ends once the queued replies are written.
    pub(crate) fn close(&self) {
        lock(&self.q).closed = true;
        self.cv.notify_all();
    }

    /// The next frame to write (replies first); `None` once closed and drained.
    fn next(&self) -> Option<Vec<u8>> {
        let mut q = lock(&self.q);
        loop {
            if let Some(f) = q.replies.pop_front() {
                return Some(f);
            }
            if q.closed {
                return None;
            }
            if q.lagged {
                q.lagged = false;
                let bytes = serde_json::to_vec(&ServerEvent::Resync).unwrap_or_default();
                return Some(frame(kind::EVENT, &bytes));
            }
            if let Some(f) = q.events.pop_front() {
                q.event_bytes = q.event_bytes.saturating_sub(f.len());
                return Some(f);
            }
            q = self.cv.wait(q).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// One encoded frame (`u32 LE len | kind | payload`).
fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    // Writing into a Vec cannot fail.
    let _ = wire::write_frame(&mut out, kind, payload);
    out
}

// ---- frontend view side queue -----------------------------------------------------------------

/// Out-of-band frames of a view, besides its terminal output.
pub(crate) enum SideItem {
    Refused(InputRefused),
    Ended(ViewEnd),
}

#[derive(Default)]
struct SideQ {
    sub: Option<Arc<OutputSubscription>>,
    items: VecDeque<SideItem>,
    signaled: bool,
}

/// Shared between the engine (which attaches the output subscription and queues refusals) and
/// the view's writer thread.
#[derive(Default)]
pub(crate) struct ViewSide {
    q: Mutex<SideQ>,
    cv: Condvar,
}

impl ViewSide {
    pub(crate) fn set_subscription(&self, sub: Arc<OutputSubscription>) {
        lock(&self.q).sub = Some(sub);
        self.signal();
    }

    pub(crate) fn push(&self, item: SideItem) {
        {
            let mut q = lock(&self.q);
            if q.items.len() >= SIDE_ITEMS {
                // The view does not read: keep the newest items and the final one.
                q.items.pop_front();
            }
            q.items.push_back(item);
        }
        self.signal();
    }

    /// Output is ready (the subscription's notify).
    pub(crate) fn signal(&self) {
        lock(&self.q).signaled = true;
        self.cv.notify_one();
    }

    /// Blocks until something is ready.
    fn wait(&self) -> (Vec<SideItem>, Option<Arc<OutputSubscription>>) {
        let mut q = lock(&self.q);
        while !q.signaled {
            q = self.cv.wait(q).unwrap_or_else(PoisonError::into_inner);
        }
        q.signaled = false;
        (q.items.drain(..).collect(), q.sub.clone())
    }
}

// ---- serve ------------------------------------------------------------------------------------

pub struct ServeOptions {
    pub profile: Profile,
    /// The startup context this owner was started with (supervisor/installer), if any.
    pub startup_file: Option<PathBuf>,
    /// Called once the endpoint accepts connections (daemon readiness).
    pub ready: Option<Box<dyn FnOnce()>>,
}

/// Runs the owner until its lifetime ends; returns the process exit code.
pub fn serve(opts: ServeOptions) -> i32 {
    let ServeOptions {
        profile,
        startup_file,
        ready,
    } = opts;
    let app_root = profile.app_root();
    if let Some(m) = super::maintenance_active(&app_root) {
        log::info!("not starting: maintenance ({:?}) in progress", m.purpose);
        return exit::BARRIER;
    }
    let id = profile.id();
    let endpoint = match id
        .namespace()
        .and_then(|ns| Endpoint::new(&ns, OWNER_ENDPOINT))
    {
        Ok(e) => e,
        Err(e) => {
            log::error!("gateway endpoint: {e}");
            return exit::FAILED;
        }
    };
    let listener = match Listener::bind(&endpoint) {
        Ok(l) => l,
        Err(deck_host::Error::AlreadyRunning(_)) => {
            log::info!("another gateway owner already serves {}", id.as_str());
            // The launcher asked for "an owner serves this identity", which is now true.
            if let Some(ready) = ready {
                ready();
            }
            return exit::OK;
        }
        Err(e) => {
            log::error!("binding the gateway endpoint: {e}");
            return exit::FAILED;
        }
    };
    // A barrier raised between the check and the bind: the installer saw no owner, so none may
    // start now.
    if super::maintenance_active(&app_root).is_some() {
        listener.remove();
        return exit::BARRIER;
    }
    let (tx, rx) = mpsc::channel();
    let engine = match engine::Engine::new(profile, startup_file, tx.clone()) {
        Ok(e) => e,
        Err(e) => {
            log::error!("starting the gateway: {e}");
            listener.remove();
            return exit::FAILED;
        }
    };
    let listener = Arc::new(listener);
    {
        let (for_thread, tx) = (listener.clone(), tx.clone());
        let spawned = std::thread::Builder::new()
            .name("gateway-accept".into())
            .spawn(move || accept_loop(&for_thread, &tx));
        if let Err(e) = spawned {
            log::error!("starting the accept thread: {e}");
            listener.remove();
            return exit::FAILED;
        }
    }
    if let Some(ready) = ready {
        ready();
    }
    let code = engine.run(rx);
    listener.remove();
    code
}

fn accept_loop(listener: &Listener, tx: &Sender<Msg>) {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    loop {
        let stream = match listener.accept() {
            Ok(s) => s,
            Err(e) => {
                log::debug!("gateway accept ended: {e}");
                return;
            }
        };
        let conn = NEXT.fetch_add(1, Ordering::Relaxed);
        let tx = tx.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("gateway-conn-{conn}"))
            .spawn(move || connection(conn, stream, &tx));
        if let Err(e) = spawned {
            log::warn!("cannot serve a connection: {e}");
        }
    }
}

fn refuse(stream: &mut Stream, refusal: &Refusal) {
    if let Ok(bytes) = serde_json::to_vec(refusal) {
        let _ = wire::write_frame(stream, kind::REFUSE, &bytes);
    }
    stream.shutdown();
}

fn read_hello(stream: &mut Stream) -> io::Result<Hello> {
    stream.set_read_timeout(Some(HELLO_TIMEOUT))?;
    match wire::read_frame(stream)? {
        Some((kind::HELLO, payload)) => serde_json::from_slice(&payload)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Some((k, _)) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected HELLO, got frame kind {k}"),
        )),
        None => Err(io::ErrorKind::UnexpectedEof.into()),
    }
}

fn connection(conn: u64, mut stream: Stream, tx: &Sender<Msg>) {
    let hello = match read_hello(&mut stream) {
        Ok(h) => h,
        Err(e) => {
            log::debug!("connection {conn}: no hello: {e}");
            stream.shutdown();
            return;
        }
    };
    if hello.protocol != PROTOCOL {
        refuse(
            &mut stream,
            &Refusal::Unsupported {
                ours: PROTOCOL,
                theirs: hello.protocol,
            },
        );
        return;
    }
    let (reply_tx, reply_rx) = mpsc::channel();
    let attach = hello.attach.is_some();
    let out = Arc::new(Outbox::default());
    let side = Arc::new(ViewSide::default());
    let msg = if attach {
        Msg::Attach {
            view: conn,
            hello,
            side: side.clone(),
            reply: reply_tx,
        }
    } else {
        Msg::Hello {
            conn,
            hello,
            out: out.clone(),
            reply: reply_tx,
        }
    };
    if tx.send(msg).is_err() {
        refuse(&mut stream, &Refusal::ShuttingDown);
        return;
    }
    let welcome = match reply_rx.recv_timeout(VERDICT_TIMEOUT) {
        Ok(Ok(w)) => w,
        Ok(Err(refusal)) => {
            refuse(&mut stream, &refusal);
            return;
        }
        Err(_) => {
            refuse(&mut stream, &Refusal::ShuttingDown);
            // The engine may still register the link later: tell it it is gone.
            let _ = tx.send(if attach {
                Msg::ViewGone { view: conn }
            } else {
                Msg::Bye { conn, clean: false }
            });
            return;
        }
    };
    if write_welcome(&mut stream, &welcome).is_err() || stream.set_read_timeout(None).is_err() {
        stream.shutdown();
        let _ = tx.send(if attach {
            Msg::ViewGone { view: conn }
        } else {
            Msg::Bye { conn, clean: false }
        });
        return;
    }
    if attach {
        serve_view(conn, stream, &side, tx);
    } else {
        serve_control(conn, stream, &out, tx);
    }
}

fn write_welcome(stream: &mut Stream, welcome: &Welcome) -> io::Result<()> {
    let bytes = serde_json::to_vec(welcome).map_err(io::Error::other)?;
    wire::write_frame(stream, kind::WELCOME, &bytes)?;
    stream.flush()
}

fn serve_control(conn: u64, stream: Stream, out: &Arc<Outbox>, tx: &Sender<Msg>) {
    let writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            log::warn!("connection {conn}: {e}");
            stream.shutdown();
            let _ = tx.send(Msg::Bye { conn, clean: false });
            return;
        }
    };
    {
        let out = out.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("gateway-out-{conn}"))
            .spawn(move || control_writer(writer, &out));
        if let Err(e) = spawned {
            log::warn!("connection {conn}: {e}");
            stream.shutdown();
            let _ = tx.send(Msg::Bye { conn, clean: false });
            return;
        }
    }
    let clean = control_reader(conn, &stream, tx);
    stream.shutdown();
    out.close();
    let _ = tx.send(Msg::Bye { conn, clean });
}

fn control_writer(mut stream: Stream, out: &Outbox) {
    while let Some(f) = out.next() {
        if stream.write_all(&f).and_then(|()| stream.flush()).is_err() {
            out.close();
            stream.shutdown();
            return;
        }
    }
    let _ = stream.flush();
    stream.shutdown();
}

/// Reads requests until the client leaves; `true` for a clean `BYE`.
fn control_reader(conn: u64, stream: &Stream, tx: &Sender<Msg>) -> bool {
    let Ok(mut r) = stream.try_clone() else {
        return false;
    };
    let mut assembly = Assembly::default();
    loop {
        match wire::read_frame(&mut r) {
            Ok(Some((kind::REQUEST, chunk))) => match assembly.push(&chunk) {
                Ok(Some((_, bytes))) => match serde_json::from_slice::<Envelope>(&bytes) {
                    Ok(env) => {
                        if tx.send(Msg::Request { conn, env }).is_err() {
                            return false;
                        }
                    }
                    Err(e) => {
                        log::warn!("connection {conn}: bad request: {e}");
                        return false;
                    }
                },
                Ok(None) => {}
                Err(e) => {
                    log::warn!("connection {conn}: {e}");
                    return false;
                }
            },
            Ok(Some((kind::BYE, _))) => return true,
            Ok(Some((k, _))) => {
                log::warn!("connection {conn}: unexpected frame kind {k}");
                return false;
            }
            Ok(None) => return false,
            Err(e) => {
                log::debug!("connection {conn}: {e}");
                return false;
            }
        }
    }
}

// ---- frontend views ---------------------------------------------------------------------------

fn serve_view(view: u64, stream: Stream, side: &Arc<ViewSide>, tx: &Sender<Msg>) {
    let writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            log::warn!("view {view}: {e}");
            stream.shutdown();
            let _ = tx.send(Msg::ViewGone { view });
            return;
        }
    };
    {
        let side = side.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("gateway-view-{view}"))
            .spawn(move || view_writer(writer, &side));
        if let Err(e) = spawned {
            log::warn!("view {view}: {e}");
            stream.shutdown();
            let _ = tx.send(Msg::ViewGone { view });
            return;
        }
    }
    view_reader(view, &stream, tx);
    stream.shutdown();
    // Wakes the writer so it notices the closed stream.
    side.push(SideItem::Ended(ViewEnd::Closed));
    let _ = tx.send(Msg::ViewGone { view });
}

fn view_reader(view: u64, stream: &Stream, tx: &Sender<Msg>) {
    let Ok(mut r) = stream.try_clone() else {
        return;
    };
    loop {
        let (k, payload) = match wire::read_frame(&mut r) {
            Ok(Some(f)) => f,
            Ok(None) => return,
            Err(e) => {
                log::debug!("view {view}: {e}");
                return;
            }
        };
        let op = match k {
            kind::U_INPUT => match protocol::decode_input(&payload) {
                Some((meta, bytes)) => ViewOp::Input(bytes.to_vec(), meta),
                None => return,
            },
            kind::U_CLIP => ViewOp::Clipboard(payload),
            kind::U_FOCUS => ViewOp::Focus(payload.first().is_some_and(|b| *b != 0)),
            kind::U_RESIZE => match protocol::decode_geometry(&payload) {
                Some((c, r, w, h)) => ViewOp::Resize(c, r, w, h),
                None => return,
            },
            kind::U_PALETTE => match protocol::decode_palette(&payload) {
                Some(p) => ViewOp::Palette(p),
                None => return,
            },
            kind::U_KILL => ViewOp::Kill,
            kind::U_DETACH => return,
            other => {
                log::warn!("view {view}: unexpected frame kind {other}");
                return;
            }
        };
        if tx.send(Msg::View { view, op }).is_err() {
            return;
        }
    }
}

fn json_frame(stream: &mut Stream, kind: u8, value: &impl serde::Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    wire::write_frame(stream, kind, &bytes)
}

/// Writes one output event; `Ok(true)` when the view ended with it.
fn write_output(stream: &mut Stream, ev: OutputEvent, snapshot_seq: &mut u64) -> io::Result<bool> {
    match ev {
        OutputEvent::Resize {
            cols,
            rows,
            cell_width,
            cell_height,
        } => {
            let g = protocol::encode_geometry(cols, rows, cell_width, cell_height);
            wire::write_frame(stream, kind::F_RESIZE, &g)?;
        }
        OutputEvent::Snapshot(bytes) => {
            *snapshot_seq += 1;
            for chunk in wire::chunks(*snapshot_seq, &bytes[..]) {
                wire::write_frame(stream, kind::F_SNAPSHOT, &chunk)?;
            }
        }
        OutputEvent::Output(bytes) => {
            for piece in bytes.chunks(wire::CHUNK) {
                wire::write_frame(stream, kind::F_OUTPUT, piece)?;
            }
        }
        OutputEvent::State(state) => json_frame(stream, kind::F_STATE, &state)?,
        OutputEvent::Exited(code) => {
            json_frame(stream, kind::F_EXITED, &code)?;
            return Ok(true);
        }
        OutputEvent::Overflow => {
            json_frame(stream, kind::F_ENDED, &ViewEnd::Overflow)?;
            return Ok(true);
        }
        OutputEvent::Closed => {
            json_frame(stream, kind::F_ENDED, &ViewEnd::Closed)?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn view_writer(mut stream: Stream, side: &ViewSide) {
    let mut snapshot_seq = 0u64;
    let result = (|| -> io::Result<()> {
        loop {
            let (items, sub) = side.wait();
            for item in items {
                match item {
                    SideItem::Refused(r) => json_frame(&mut stream, kind::F_REFUSED, &r)?,
                    SideItem::Ended(end) => {
                        json_frame(&mut stream, kind::F_ENDED, &end)?;
                        return Ok(());
                    }
                }
            }
            if let Some(sub) = sub {
                while let Some(ev) = sub.try_recv() {
                    if write_output(&mut stream, ev, &mut snapshot_seq)? {
                        return Ok(());
                    }
                }
            }
            stream.flush()?;
        }
    })();
    if let Err(e) = result {
        log::debug!("view writer ended: {e}");
    }
    let _ = stream.flush();
    stream.shutdown();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(f: &[u8]) -> (u8, Vec<u8>) {
        let mut r = f;
        wire::read_frame(&mut r).expect("frame").expect("some")
    }

    #[test]
    fn a_lagging_link_drops_its_events_for_one_resync_and_keeps_replies() {
        let out = Outbox::default();
        for _ in 0..EVENT_QUEUE + 5 {
            out.event(&ServerEvent::Bell(crate::controller::SessionTarget {
                epoch: crate::controller::ServiceEpoch::current(),
                key: crate::controller::SessionKey::direct(),
                generation: 1,
            }));
        }
        out.reply(7, &Ok(protocol::Answer::Done));
        let (k, _) = decode(&out.next().expect("reply first"));
        assert_eq!(k, kind::REPLY);
        let (k, payload) = decode(&out.next().expect("resync"));
        assert_eq!(k, kind::EVENT);
        let ev: ServerEvent = serde_json::from_slice(&payload).expect("event");
        assert!(matches!(ev, ServerEvent::Resync));
        out.close();
        assert!(out.next().is_none(), "the dropped events are gone");
    }
}
