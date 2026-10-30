//! The relay link: one thread holding the gateway's WebSocket to the relay (`/v1/gateway`),
//! blocking `tungstenite` over rustls/ring with the webpki roots. It owns the socket, the
//! reconnect backoff and the per-device Noise state ([`deck_remote::noise::Pending`] while the
//! engine decides, then the [`Session`]); the engine owns the registry and every decision. It
//! reports [`Event`]s and obeys [`Cmd`]s; DNS, TCP, TLS, WebSocket and Noise work never run on
//! the engine thread.
//!
//! One thread serves reads and writes: the socket's short read timeout ([`POLL`]; `WouldBlock`
//! and `TimedOut` both mean "no data") lets it drain the command queue between reads.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use deck_remote::api;
use deck_remote::noise::{
    self, DeviceHello, Keypair, Mode, Pending, Psk, PublicKey, Session, SessionConfig, Welcome,
};
use deck_remote::relay::{
    self, FromRelay, GATEWAY_PATH, HELLO_OK, Hash, Hello, MAX_GATEWAY_MESSAGE, PeerId, ToRelay,
    close,
};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::protocol::frame::CloseFrame;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Connector, Message, WebSocket};
use zeroize::Zeroizing;

use super::config::RelayConfig;
use super::device::{DeviceOut, Inbound, Pace, Runtime};
use super::epoch::Epochs;
use crate::gateway::protocol::RelayLink;

/// Socket read timeout while connected: the command queue's latency.
pub const POLL: Duration = Duration::from_millis(50);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// TLS, WebSocket upgrade and the relay's hello answer.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// How long a stopping link that is up gets to close politely before its socket is cut.
const GRACE: Duration = Duration::from_millis(300);
/// How long the teardown then waits before leaving the thread to finish on its own.
const STOP_WAIT: Duration = Duration::from_secs(2);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The open pairing window's admission token (hash) and PSK: a device admitted with that token
/// runs the pairing handshake.
pub struct Invite {
    pub token_hash: Hash,
    pub psk: Zeroizing<Psk>,
}

/// Engine → link.
pub enum Cmd {
    /// The full admission token set (sent now and after every hello).
    Tokens(Vec<Hash>),
    Invite(Option<Invite>),
    /// Answer the held message 1 of `peer` with message 2. A reconnect carries the device's
    /// application session (`device`); a pairing connection carries none (it ends after
    /// message 2, and any record on it closes it).
    Accept {
        peer: PeerId,
        welcome: Welcome,
        device: Option<Arc<DeviceOut>>,
    },
    /// Close the device connection (the relay reports `Closed`).
    Close {
        peer: PeerId,
        code: u16,
    },
    /// A device's queue has something ([`Link::waker`]).
    Wake,
}

/// Link → engine.
#[derive(Debug)]
pub enum Event {
    Link(RelayLink),
    /// A device was admitted with the token whose hash is `token`.
    Opened {
        peer: PeerId,
        token: Hash,
    },
    /// A device's message 1 verified (pairing: with the invite's PSK); its handshake is held
    /// until [`Cmd::Accept`] or [`Cmd::Close`].
    Hello {
        peer: PeerId,
        pairing: bool,
        public: PublicKey,
        hello: DeviceHello,
    },
    Closed {
        peer: PeerId,
    },
    /// A device session asked for something.
    Device {
        peer: PeerId,
        inbound: Inbound,
    },
}

struct Shared {
    stop: AtomicBool,
    /// The relay accepted the hello: a stop closes politely instead of cutting the socket.
    up: AtomicBool,
    /// A clone of the connected socket: a stop shuts it down to unblock the thread.
    socket: Mutex<Option<TcpStream>>,
    /// A [`Cmd::Wake`] is queued (wakeups coalesce).
    woken: AtomicBool,
}

impl Shared {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// Tells the thread to stop, without waiting. A link not yet up has its socket cut right
    /// away, so a late hello can never take the room over from the link that replaces it (the
    /// thread stores its socket before checking the flag, so one of the two sees the other).
    fn cancel(&self) {
        self.stop.store(true, Ordering::Release);
        if !self.up.load(Ordering::Acquire) {
            self.cut();
        }
    }

    fn cut(&self) {
        if let Some(s) = lock(&self.socket).as_ref() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

/// The running link thread. Dropping it stops the thread without blocking: the bounded wait
/// for it runs on a teardown thread.
pub struct Link {
    cmds: Sender<Cmd>,
    shared: Arc<Shared>,
    done: Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl Link {
    /// Starts connecting to `relay` as the gateway `identity`, registering `tokens` (and the
    /// `invite`'s PSK) after every hello. Each attempt's hello epoch comes from `epochs`, shared
    /// by every link of this owner. `events` receives every [`Event`].
    pub fn start(
        relay: RelayConfig,
        identity: Keypair,
        epochs: Arc<Mutex<Epochs>>,
        tokens: Vec<Hash>,
        invite: Option<Invite>,
        events: Box<dyn Fn(Event) + Send>,
    ) -> io::Result<Link> {
        let (cmds, rx) = std::sync::mpsc::channel();
        let (done_tx, done) = std::sync::mpsc::channel();
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            up: AtomicBool::new(false),
            socket: Mutex::new(None),
            woken: AtomicBool::new(false),
        });
        let worker = Worker {
            relay,
            identity,
            epochs,
            cmds: rx,
            shared: shared.clone(),
            events,
            tokens,
            invite,
            pace: Pace::new(Instant::now()),
            next_peer: 0,
        };
        let thread = std::thread::Builder::new()
            .name("gateway-relay".into())
            .spawn(move || {
                worker.run();
                let _ = done_tx.send(());
            })?;
        Ok(Link {
            cmds,
            shared,
            done,
            thread: Some(thread),
        })
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmds.send(cmd);
    }

    /// Makes the thread pull from the device queues soon (coalesced).
    pub fn waker(&self) -> super::device::Wake {
        let (cmds, shared) = (self.cmds.clone(), self.shared.clone());
        Arc::new(move || {
            if !shared.woken.swap(true, Ordering::AcqRel) {
                let _ = cmds.send(Cmd::Wake);
            }
        })
    }

    /// Stops the thread without blocking the caller: a link that is up closes with
    /// `GOING_AWAY`, anything else has its socket cut; a teardown thread waits (bounded) for it.
    pub fn stop(self) {
        drop(self);
    }

    /// [`Link::stop`], waiting (bounded) for the thread: for process exit.
    pub fn stop_and_wait(mut self) {
        self.shared.cancel();
        if let Some(thread) = self.thread.take() {
            finish(&self.shared, &self.done, thread);
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.shared.cancel();
        let Some(thread) = self.thread.take() else {
            return;
        };
        let shared = self.shared.clone();
        let (_, idle) = std::sync::mpsc::channel();
        let done = std::mem::replace(&mut self.done, idle);
        let teardown = std::thread::Builder::new()
            .name("gateway-relay-stop".into())
            .spawn(move || finish(&shared, &done, thread));
        if let Err(e) = teardown {
            // The flag is set and a pending socket cut: the thread ends on its own.
            log::warn!("cannot wait for the relay link to stop ({e}); leaving it to finish");
        }
    }
}

/// Waits for a cancelled link thread: a connected link notices within `POLL` and closes
/// politely; anything still blocked after [`GRACE`] has its socket cut.
fn finish(shared: &Shared, done: &Receiver<()>, thread: JoinHandle<()>) {
    let mut finished = done.recv_timeout(GRACE);
    if matches!(finished, Err(RecvTimeoutError::Timeout)) {
        shared.cut();
        finished = done.recv_timeout(STOP_WAIT);
    }
    match finished {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => {
            let _ = thread.join();
        }
        // Stuck in DNS or a connect: it ends on its own once that returns.
        Err(RecvTimeoutError::Timeout) => {
            log::warn!("the relay link did not stop in time; leaving it to finish");
        }
    }
}

/// Why a connection ended.
enum End {
    Stopped,
    /// Retry with backoff; `up`: it had been up (the backoff restarts).
    Lost {
        reason: String,
        up: bool,
    },
    /// The relay refused this gateway: retried at the slowest pace.
    Refused(String),
    /// Another gateway took the room over: the link stays down until restarted (retrying would
    /// take the room back with a newer epoch, and two gateways would flap forever).
    Replaced(String),
}

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

/// A device connection's handshake state.
enum Slot {
    /// Admitted; its message 1 is next.
    Open { token: Hash },
    /// Message 1 verified; the engine decides.
    Held(Box<Pending>),
    /// Message 2 sent.
    Live(Box<Live>),
}

/// An established session: a reconnect runs the device's application session on it.
struct Live {
    session: Session,
    device: Option<Runtime>,
}

struct Worker {
    relay: RelayConfig,
    identity: Keypair,
    epochs: Arc<Mutex<Epochs>>,
    cmds: Receiver<Cmd>,
    shared: Arc<Shared>,
    events: Box<dyn Fn(Event) + Send>,
    tokens: Vec<Hash>,
    invite: Option<Invite>,
    pace: Pace,
    /// The device that goes first in the next flush (where the pace ran out).
    next_peer: PeerId,
}

/// What one device's turn in a flush did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flush {
    Sent,
    /// Nothing to send (or it was closed).
    Idle,
    /// The egress pace ran out before it could send.
    Paced,
}

/// Gives `peers` turns of one message each (starting at the first one at or after `next`,
/// wrapping around) until every one is idle or the pace runs out. Returns the device that
/// should go first next time when the pace ran out.
fn round_robin<E>(
    mut peers: Vec<PeerId>,
    next: PeerId,
    mut turn: impl FnMut(PeerId) -> Result<Flush, E>,
) -> Result<Option<PeerId>, E> {
    peers.sort_unstable();
    let start = peers.partition_point(|p| *p < next);
    peers.rotate_left(start);
    while !peers.is_empty() {
        let mut more = Vec::with_capacity(peers.len());
        for &peer in &peers {
            match turn(peer)? {
                Flush::Sent => more.push(peer),
                Flush::Idle => {}
                Flush::Paced => return Ok(Some(peer)),
            }
        }
        peers = more;
    }
    Ok(None)
}

impl Worker {
    fn run(mut self) {
        let tls = if self.relay.url.tls() {
            match tls_config() {
                Ok(c) => Some(c),
                Err(e) => {
                    (self.events)(Event::Link(RelayLink::Refused {
                        reason: format!("TLS setup failed: {e}"),
                    }));
                    return;
                }
            }
        } else {
            None
        };
        let mut attempt: u32 = 0;
        let mut error: Option<String> = None;
        let mut refused = false;
        while !self.shared.stopped() {
            attempt = attempt.saturating_add(1);
            if !refused {
                (self.events)(Event::Link(RelayLink::Connecting {
                    attempt,
                    error: error.clone(),
                }));
            }
            let end = match self.connect(tls.as_ref()) {
                Ok(ws) => self.serve(ws),
                Err(e) => End::Lost {
                    reason: e,
                    up: false,
                },
            };
            self.shared.up.store(false, Ordering::Release);
            *lock(&self.shared.socket) = None;
            let delay = match end {
                End::Stopped => return,
                End::Lost { reason, up } => {
                    if up {
                        attempt = 0;
                    }
                    log::info!("relay link: {reason}");
                    error = Some(reason);
                    refused = false;
                    backoff(attempt.max(1))
                }
                End::Refused(reason) => {
                    log::warn!("relay link refused: {reason}");
                    (self.events)(Event::Link(RelayLink::Refused {
                        reason: reason.clone(),
                    }));
                    error = Some(reason);
                    refused = true;
                    BACKOFF_MAX
                }
                End::Replaced(reason) => {
                    log::warn!("relay link ended: {reason}; not retrying");
                    (self.events)(Event::Link(RelayLink::Refused { reason }));
                    return;
                }
            };
            if self.pause(delay) {
                return;
            }
        }
    }

    /// Waits `delay` while keeping the token set and invite current; `true` = stopped.
    fn pause(&mut self, delay: Duration) -> bool {
        let until = Instant::now() + delay;
        loop {
            if self.shared.stopped() {
                return true;
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            match self.cmds.recv_timeout(left.min(Duration::from_millis(100))) {
                // Peers are gone while disconnected: answers and closes have nothing to act on.
                Ok(Cmd::Tokens(t)) => self.tokens = t,
                Ok(Cmd::Invite(i)) => self.invite = i,
                Ok(Cmd::Accept { .. } | Cmd::Close { .. } | Cmd::Wake)
                | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return true,
            }
        }
    }

    /// Resolves and connects on a helper thread, polling for a stop meanwhile: neither DNS nor
    /// a TCP connect can be interrupted, and on Windows even a refused loopback connect takes
    /// about 2 s (SYN retries after the RST). A stop abandons the helper, which ends on its own
    /// within [`CONNECT_TIMEOUT`] and drops whatever it connected (no hello is ever sent on it).
    fn dial(&self) -> Result<TcpStream, String> {
        let url = &self.relay.url;
        let (host, port, shown) = (url.host().to_owned(), url.port(), url.as_str().to_owned());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("gateway-relay-dial".into())
            .spawn(move || {
                let _ = tx.send(dial(&host, port, &shown));
            })
            .map_err(|e| format!("cannot start connecting: {e}"))?;
        loop {
            match rx.recv_timeout(POLL) {
                Ok(result) => return result,
                Err(RecvTimeoutError::Timeout) if self.shared.stopped() => {
                    return Err("stopped".into());
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Err("connecting failed".into()),
            }
        }
    }

    fn connect(&self, tls: Option<&Arc<rustls::ClientConfig>>) -> Result<Ws, String> {
        let url = &self.relay.url;
        let stream = self.dial()?;
        let io_err = |e: io::Error| format!("connection to {}: {e}", url.as_str());
        stream.set_nodelay(true).map_err(io_err)?;
        // Blocking with timeouts for the one-shot TLS/upgrade/hello exchange only: a timeout
        // there abandons the connection (on Windows a timed-out receive leaves the socket in an
        // indeterminate state, so it must not be reused). Routing then runs non-blocking.
        stream
            .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
            .map_err(io_err)?;
        stream
            .set_write_timeout(Some(WRITE_TIMEOUT))
            .map_err(io_err)?;
        *lock(&self.shared.socket) = Some(stream.try_clone().map_err(io_err)?);
        if self.shared.stopped() {
            return Err("stopped".into());
        }
        let connector = match tls {
            Some(c) => Connector::Rustls(c.clone()),
            None => Connector::Plain,
        };
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_GATEWAY_MESSAGE))
            .max_frame_size(Some(MAX_GATEWAY_MESSAGE));
        let (ws, _) = tungstenite::client_tls_with_config(
            url.endpoint(GATEWAY_PATH),
            stream,
            Some(config),
            Some(connector),
        )
        .map_err(|e| format!("connecting to {}: {e}", url.as_str()))?;
        Ok(ws)
    }

    /// Runs one connection: hello, token set, then routing until it ends.
    fn serve(&mut self, mut ws: Ws) -> End {
        // Allocated under the lock every link of this owner shares, and on disk before the
        // hello leaves. The stop check sits inside the lock: a link cancelled before its
        // replacement started can never allocate an epoch above the replacement's.
        let epoch = {
            let mut epochs = lock(&self.epochs);
            if self.shared.stopped() {
                return End::Stopped;
            }
            match epochs.next(crate::gateway::unix_ms()) {
                Ok(e) => e,
                Err(e) => return lost(format!("recording the gateway epoch: {e}"), false),
            }
        };
        let hello = Hello::Gateway {
            room: self.relay.room_key.room,
            secret: self.relay.room_key.secret,
            epoch,
        };
        if let Err(e) = ws.send(Message::binary(hello.encode())) {
            return lost(format!("sending the hello: {e}"), false);
        }
        loop {
            match ws.read() {
                Ok(Message::Binary(b)) if b[..] == HELLO_OK[..] => break,
                Ok(Message::Close(frame)) => return closed_by_relay(frame, false),
                Ok(Message::Ping(_) | Message::Pong(_)) => {}
                Ok(_) => return lost("the relay answered the hello wrongly".into(), false),
                Err(e) if is_timeout(&e) => {
                    return lost("the relay did not answer the hello".into(), false);
                }
                Err(e) => return lost(format!("waiting for the relay: {e}"), false),
            }
        }
        self.shared.up.store(true, Ordering::Release);
        // Routing polls a non-blocking socket instead of using a short read timeout: on Windows
        // a timed-out blocking receive leaves the connection indeterminate (SO_RCVTIMEO), while
        // `WouldBlock` is well defined everywhere. Set on the stream itself, not the clone.
        let nonblocking = match ws.get_ref() {
            MaybeTlsStream::Plain(s) => s.set_nonblocking(true),
            MaybeTlsStream::Rustls(s) => s.sock.set_nonblocking(true),
            _ => Err(io::Error::other("unsupported stream")),
        };
        if let Err(e) = nonblocking {
            return lost(format!("socket: {e}"), false);
        }
        let mut conn = Conn {
            ws,
            peers: HashMap::new(),
        };
        if let Err(e) = conn.send(&ToRelay::Tokens(self.tokens.clone())) {
            return lost(e, false);
        }
        (self.events)(Event::Link(RelayLink::Up));
        log::info!(
            "relay link up: {} room {}",
            self.relay.url.as_str(),
            relay::room_id(&self.relay.room_key.room)
        );
        loop {
            if let Some(end) = self.pump(&mut conn) {
                return end;
            }
        }
    }

    /// One round of routing on a non-blocking connection: queued commands, every message
    /// already readable, the pending writes, then a wait of at most [`POLL`] for the next
    /// command. `Some` = the connection is over.
    fn pump<S: Read + Write>(&mut self, conn: &mut Conn<S>) -> Option<End> {
        if self.shared.stopped() {
            conn.goodbye();
            return Some(End::Stopped);
        }
        // Wakeups from now on queue a new `Wake`.
        self.shared.woken.store(false, Ordering::Release);
        loop {
            match self.cmds.try_recv() {
                Ok(cmd) => {
                    if let Err(e) = self.command(conn, cmd) {
                        return Some(lost(e, true));
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    conn.goodbye();
                    return Some(End::Stopped);
                }
            }
        }
        loop {
            match conn.ws.read() {
                Ok(Message::Binary(b)) => {
                    let msg = match FromRelay::decode(&b) {
                        Ok(m) => m,
                        Err(e) => {
                            return Some(lost(format!("undecodable relay message: {e}"), true));
                        }
                    };
                    if let Err(e) = self.relayed(conn, msg) {
                        return Some(lost(e, true));
                    }
                }
                Ok(Message::Close(frame)) => return Some(closed_by_relay(frame, true)),
                Ok(Message::Text(_)) => return Some(lost("the relay sent text".into(), true)),
                Ok(_) => {}
                Err(e) if would_block(&e) => break,
                Err(e) => return Some(lost(format!("relay connection: {e}"), true)),
            }
        }
        if let Err(e) = self.flush_devices(conn) {
            return Some(lost(e, true));
        }
        // Frames a full socket buffer held back (and automatic pongs) go out now; tungstenite
        // keeps them queued in order until a flush completes.
        match conn.ws.flush() {
            Ok(()) => {}
            Err(e) if would_block(&e) => {}
            Err(e) => return Some(lost(format!("relay connection: {e}"), true)),
        }
        match self.cmds.recv_timeout(POLL) {
            Ok(cmd) => self.command(conn, cmd).err().map(|e| lost(e, true)),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                conn.goodbye();
                Some(End::Stopped)
            }
        }
    }

    /// Sends what every device session has ready, within its window and the egress pace, and
    /// closes devices that stopped acknowledging. Devices take turns one message at a time,
    /// starting where the pace ran out last time, so a backlogged device cannot keep the
    /// shared egress credit to itself.
    fn flush_devices<S: Read + Write>(&mut self, conn: &mut Conn<S>) -> Result<(), String> {
        let now = Instant::now();
        let peers: Vec<PeerId> = conn
            .peers
            .iter()
            .filter(|(_, s)| matches!(s, Slot::Live(l) if l.device.is_some()))
            .map(|(p, _)| *p)
            .collect();
        let mut gone: Vec<(PeerId, u64)> = Vec::new();
        let next = round_robin(peers, self.next_peer, |peer| {
            self.flush_one(conn, peer, now, &mut gone)
        })?;
        if let Some(peer) = next {
            self.next_peer = peer;
        }
        for (peer, view) in gone {
            (self.events)(Event::Device {
                peer,
                inbound: Inbound::ViewGone(view),
            });
        }
        Ok(())
    }

    /// One message of device `peer`, if it has one and the pace allows.
    fn flush_one<S: Read + Write>(
        &mut self,
        conn: &mut Conn<S>,
        peer: PeerId,
        now: Instant,
        gone: &mut Vec<(PeerId, u64)>,
    ) -> Result<Flush, String> {
        let Some(Slot::Live(live)) = conn.peers.get_mut(&peer) else {
            return Ok(Flush::Idle);
        };
        let Some(rt) = live.device.as_mut() else {
            return Ok(Flush::Idle);
        };
        if rt.stalled(now) {
            log::info!("relay peer {peer}: the device stopped acknowledging");
            conn.close(peer, close::STALLED)?;
            return Ok(Flush::Idle);
        }
        if !self.pace.open(now) {
            return Ok(Flush::Paced);
        }
        let mut ended = Vec::new();
        let msg = rt.next_message(now, &mut ended);
        gone.extend(ended.into_iter().map(|v| (peer, v)));
        let Some(msg) = msg else {
            return Ok(Flush::Idle);
        };
        let frames = match live.session.seal(&msg) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("relay peer {peer}: sealing failed: {e}");
                conn.close(peer, close::PROTOCOL)?;
                return Ok(Flush::Idle);
            }
        };
        let bytes: usize = frames.iter().map(Vec::len).sum();
        self.pace.spend(bytes, frames.len());
        for f in &frames {
            conn.send(&ToRelay::Data { peer, payload: f })?;
        }
        Ok(Flush::Sent)
    }

    fn command<S: Read + Write>(&mut self, conn: &mut Conn<S>, cmd: Cmd) -> Result<(), String> {
        match cmd {
            Cmd::Tokens(t) => {
                self.tokens = t;
                conn.send(&ToRelay::Tokens(self.tokens.clone()))
            }
            Cmd::Invite(i) => {
                self.invite = i;
                Ok(())
            }
            Cmd::Accept {
                peer,
                welcome,
                device,
            } => match conn.peers.remove(&peer) {
                Some(Slot::Held(pending)) => match pending.accept(&welcome) {
                    Ok((mut session, msg2)) => {
                        conn.send(&ToRelay::Data {
                            peer,
                            payload: &msg2,
                        })?;
                        if device.is_some() {
                            session.set_config(SessionConfig {
                                max_message: api::MAX_APP_MESSAGE,
                                ..*session.config()
                            });
                        }
                        conn.peers.insert(
                            peer,
                            Slot::Live(Box::new(Live {
                                session,
                                device: device.map(Runtime::new),
                            })),
                        );
                        Ok(())
                    }
                    Err(e) => {
                        log::warn!("relay peer {peer}: answering failed: {e}");
                        conn.close(peer, close::UNAUTHORIZED)
                    }
                },
                Some(other) => {
                    // Not waiting for an answer: leave it as it was.
                    conn.peers.insert(peer, other);
                    Ok(())
                }
                None => Ok(()),
            },
            Cmd::Close { peer, code } => conn.close(peer, code),
            Cmd::Wake => Ok(()),
        }
    }

    fn relayed<S: Read + Write>(
        &mut self,
        conn: &mut Conn<S>,
        msg: FromRelay<'_>,
    ) -> Result<(), String> {
        match msg {
            FromRelay::Open { peer, token } => {
                conn.peers.insert(peer, Slot::Open { token });
                (self.events)(Event::Opened { peer, token });
                Ok(())
            }
            FromRelay::Closed { peer } => {
                conn.peers.remove(&peer);
                (self.events)(Event::Closed { peer });
                Ok(())
            }
            FromRelay::Data { peer, payload } => match conn.peers.get_mut(&peer) {
                None => Ok(()),
                Some(Slot::Open { token }) => {
                    let token = *token;
                    let psk = self
                        .invite
                        .as_ref()
                        .filter(|i| i.token_hash == token)
                        .map(|i| i.psk.clone());
                    let mode = match &psk {
                        Some(psk) => Mode::Pairing { psk },
                        None => Mode::Reconnect,
                    };
                    match noise::respond(mode, &self.identity, payload) {
                        Ok(pending) => {
                            (self.events)(Event::Hello {
                                peer,
                                pairing: psk.is_some(),
                                public: *pending.device_public(),
                                hello: pending.hello().clone(),
                            });
                            conn.peers.insert(peer, Slot::Held(Box::new(pending)));
                            Ok(())
                        }
                        Err(e) => {
                            log::info!("relay peer {peer}: handshake refused ({e})");
                            conn.close(peer, close::UNAUTHORIZED)
                        }
                    }
                }
                Some(Slot::Held(_)) => {
                    log::info!("relay peer {peer}: spoke before its answer");
                    conn.close(peer, close::PROTOCOL)
                }
                Some(Slot::Live(live)) => {
                    let message = match live.session.open(payload) {
                        Ok(Some(m)) => m,
                        // More frames of the message follow.
                        Ok(None) => return Ok(()),
                        Err(e) => {
                            log::info!("relay peer {peer}: undecryptable message ({e})");
                            return conn.close(peer, close::UNAUTHORIZED);
                        }
                    };
                    // A pairing session carries no records.
                    let result = match live.device.as_mut() {
                        Some(rt) => rt.receive(&message, Instant::now()),
                        None => Err(close::APP_PROTOCOL),
                    };
                    match result {
                        Ok(inbound) => {
                            for inbound in inbound {
                                (self.events)(Event::Device { peer, inbound });
                            }
                            Ok(())
                        }
                        Err(code) => {
                            log::info!("relay peer {peer}: unreadable application message");
                            conn.close(peer, code)
                        }
                    }
                }
            },
        }
    }
}

/// One routed relay connection. Generic over the stream so the routing round can be driven
/// by a scripted stream in tests.
struct Conn<S: Read + Write = MaybeTlsStream<TcpStream>> {
    ws: WebSocket<S>,
    peers: HashMap<PeerId, Slot>,
}

impl<S: Read + Write> Conn<S> {
    /// Queues a message and pushes it out as far as the socket takes it. `WouldBlock` is not a
    /// loss: tungstenite keeps the unwritten bytes (`write_out_buffer` drains only what was
    /// written) and the next round's flush continues them in order.
    fn send(&mut self, msg: &ToRelay<'_>) -> Result<(), String> {
        let bytes = msg
            .encode()
            .map_err(|e| format!("encoding a relay message: {e}"))?;
        match self.ws.send(Message::binary(bytes)) {
            Ok(()) => Ok(()),
            Err(e) if would_block(&e) => Ok(()),
            Err(e) => Err(format!("relay connection: {e}")),
        }
    }

    fn close(&mut self, peer: PeerId, code: u16) -> Result<(), String> {
        self.peers.remove(&peer);
        self.send(&ToRelay::Close { peer, code })
    }

    /// Ends the connection politely (best effort: the socket goes away right after).
    fn goodbye(&mut self) {
        let _ = self.ws.close(Some(CloseFrame {
            code: CloseCode::from(close::GOING_AWAY),
            reason: "".into(),
        }));
        let _ = self.ws.flush();
    }
}

fn lost(reason: String, up: bool) -> End {
    End::Lost { reason, up }
}

fn closed_by_relay(frame: Option<CloseFrame>, up: bool) -> End {
    let code = frame.map(|f| u16::from(f.code));
    match code {
        Some(close::UNAUTHORIZED) => {
            End::Refused("the relay does not accept this room key (removed or mistyped?)".into())
        }
        Some(close::BAD_HELLO) => {
            End::Refused("the relay did not accept the hello (incompatible relay version?)".into())
        }
        Some(close::REPLACED) => {
            End::Replaced("another OmpDeck gateway took over this relay room".into())
        }
        Some(close::GOING_AWAY) => lost("the relay is restarting".into(), up),
        Some(close::RATE_LIMITED) => lost("the relay is limiting connections".into(), up),
        Some(c) => lost(format!("the relay closed the link ({c})"), up),
        None => lost("the relay closed the link".into(), up),
    }
}

/// A blocking exchange's timeout (the handshake): `WouldBlock` on Unix, `TimedOut` (WSAETIMEDOUT)
/// on Windows.
fn is_timeout(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
}

/// Nothing to read, or the socket buffer is full, on a non-blocking stream.
fn would_block(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if io.kind() == io::ErrorKind::WouldBlock)
}

/// Resolves `host` and connects to its addresses in turn (runs on the dial thread).
fn dial(host: &str, port: u16, shown: &str) -> Result<TcpStream, String> {
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {host}: {e}"))?;
    let mut last = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    Err(match last {
        Some(e) => format!("cannot connect to {shown}: {e}"),
        None => format!("{host} has no address"),
    })
}

/// 1 s, 2 s, 4 s … 60 s, plus up to 25 % jitter.
fn backoff(attempt: u32) -> Duration {
    let base = BACKOFF_MIN
        .saturating_mul(1u32 << attempt.saturating_sub(1).min(6))
        .min(BACKOFF_MAX);
    let jitter = super::random::<2>()
        .map(|b| u64::from(u16::from_le_bytes(b)))
        .unwrap_or(0);
    let extra = base.as_millis() as u64 * jitter / (4 * u64::from(u16::MAX));
    base + Duration::from_millis(extra)
}

/// rustls on ring with the webpki roots (the set ureq uses): no process-wide provider needed.
fn tls_config() -> Result<Arc<rustls::ClientConfig>, rustls::Error> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_share_the_egress_pace_one_message_per_turn() {
        let mut queued: HashMap<PeerId, usize> = HashMap::from([(1, 3), (2, 1), (3, 2)]);
        let mut budget = 4;
        let mut order = Vec::new();
        let next = round_robin(vec![3, 1, 2], 2, |peer| -> Result<Flush, ()> {
            let left = queued.get_mut(&peer).ok_or(())?;
            if *left == 0 {
                return Ok(Flush::Idle);
            }
            if budget == 0 {
                return Ok(Flush::Paced);
            }
            budget -= 1;
            *left -= 1;
            order.push(peer);
            Ok(Flush::Sent)
        });
        assert_eq!(
            order,
            [2, 3, 1, 3],
            "a backlogged device does not go twice in a row"
        );
        assert_eq!(next, Ok(Some(1)), "where the pace ran out starts next time");
        let next = round_robin(vec![1, 2, 3], 1, |_| -> Result<Flush, ()> {
            Ok(Flush::Idle)
        });
        assert_eq!(next, Ok(None));
    }

    #[test]
    fn backoff_grows_to_a_minute_with_bounded_jitter() {
        for (attempt, base) in [(1, 1), (2, 2), (3, 4), (6, 32), (7, 60), (50, 60)] {
            let d = backoff(attempt);
            let base = Duration::from_secs(base);
            assert!(d >= base && d <= base + base / 4, "{attempt}: {d:?}");
        }
    }

    #[test]
    fn tls_config_builds() {
        assert!(tls_config().is_ok());
    }

    #[test]
    fn relay_refusals_are_not_retried_fast() {
        let frame = |code: u16| {
            Some(CloseFrame {
                code: CloseCode::from(code),
                reason: "".into(),
            })
        };
        assert!(matches!(
            closed_by_relay(frame(close::UNAUTHORIZED), false),
            End::Refused(_)
        ));
        assert!(matches!(
            closed_by_relay(frame(close::REPLACED), true),
            End::Replaced(_)
        ));
        assert!(matches!(
            closed_by_relay(frame(close::GOING_AWAY), true),
            End::Lost { up: true, .. }
        ));
        assert!(matches!(
            closed_by_relay(None, false),
            End::Lost { up: false, .. }
        ));
    }

    fn start(port: u16, events: std::sync::mpsc::Sender<String>) -> Link {
        let key = relay::RoomKey {
            room: [1; 16],
            secret: [2; 32],
        };
        let cfg = RelayConfig::new(&format!("ws://127.0.0.1:{port}"), &key.encode()).unwrap();
        Link::start(
            cfg,
            Keypair::generate().unwrap(),
            // Never reached: these links do not get as far as a hello.
            Arc::new(Mutex::new(Epochs::load(&std::env::temp_dir().join(
                format!("omp-deck-test-epoch-{}-{port}", std::process::id()),
            )))),
            Vec::new(),
            None,
            Box::new(move |e| {
                let _ = events.send(format!("{e:?}"));
            }),
        )
        .unwrap()
    }

    /// The thread ended: its event callback (and so the sender) was dropped.
    fn ended(rx: &std::sync::mpsc::Receiver<String>, within: Duration) -> bool {
        let end = Instant::now() + within;
        loop {
            match rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
                Ok(_) => {}
                Err(RecvTimeoutError::Disconnected) => return true,
                Err(RecvTimeoutError::Timeout) => return false,
            }
        }
    }

    #[test]
    fn a_stopped_link_to_nowhere_ends_promptly() {
        // Nothing listens on this port: the thread backs off; stop must not wait for it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let (tx, rx) = std::sync::mpsc::channel();
        let link = start(port, tx);
        let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(first.contains("Connecting"), "{first}");
        let started = Instant::now();
        link.stop_and_wait();
        // Well inside GRACE: the stop is noticed between dial polls, never waited out. (On
        // Windows a refused loopback connect alone takes ~2 s, which a blocking connect on the
        // link thread made the stop wait for.)
        assert!(started.elapsed() < GRACE, "{:?}", started.elapsed());
        assert!(ended(&rx, Duration::from_secs(1)));
    }

    /// A loopback listener with a zero backlog whose accept queue is already full: further
    /// SYNs are dropped, so a connect to it hangs in the kernel until its timeout (the way a
    /// refused connect takes ~2 s on Windows).
    #[cfg(target_os = "linux")]
    fn saturated_listener() -> (std::net::TcpListener, u16, Vec<TcpStream>) {
        use std::os::fd::FromRawFd;
        // SAFETY: plain socket calls on a fresh descriptor, owned by the listener right away.
        let listener = unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
            assert!(fd >= 0);
            let listener = std::net::TcpListener::from_raw_fd(fd);
            let mut addr: libc::sockaddr_in = std::mem::zeroed();
            addr.sin_family = libc::AF_INET as libc::sa_family_t;
            addr.sin_addr.s_addr = u32::from(std::net::Ipv4Addr::LOCALHOST).to_be();
            let len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
            assert_eq!(libc::bind(fd, (&raw const addr).cast(), len), 0);
            assert_eq!(libc::listen(fd, 0), 0);
            listener
        };
        let addr = listener.local_addr().unwrap();
        let mut held = Vec::new();
        while let Ok(s) = TcpStream::connect_timeout(&addr, Duration::from_millis(300)) {
            held.push(s);
            assert!(held.len() < 64, "the backlog never filled");
        }
        (listener, addr.port(), held)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_stop_during_a_hanging_connect_is_not_waited_out() {
        let (_listener, port, _held) = saturated_listener();
        let (tx, rx) = std::sync::mpsc::channel();
        let link = start(port, tx);
        let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(first.contains("Connecting"), "{first}");
        // The link thread is now inside the connect (it would hang for CONNECT_TIMEOUT).
        std::thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        link.stop_and_wait();
        assert!(started.elapsed() < GRACE, "{:?}", started.elapsed());
        assert!(ended(&rx, Duration::from_secs(1)));
    }

    /// A non-blocking socket double: reads hand out scripted bytes, then `WouldBlock`; writes
    /// take at most `chunk` bytes and refuse every other call with `WouldBlock`.
    #[derive(Default)]
    struct Wire {
        inbound: std::collections::VecDeque<u8>,
        outbound: Vec<u8>,
        writes: usize,
        chunk: usize,
    }

    struct Scripted(Arc<Mutex<Wire>>);

    impl Read for Scripted {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut w = self.0.lock().unwrap();
            if w.inbound.is_empty() {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let n = buf.len().min(w.inbound.len());
            for (slot, b) in buf.iter_mut().zip(w.inbound.drain(..n)) {
                *slot = b;
            }
            Ok(n)
        }
    }

    impl Write for Scripted {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut w = self.0.lock().unwrap();
            w.writes += 1;
            if w.writes % 2 == 1 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let n = buf.len().min(w.chunk);
            w.outbound.extend_from_slice(&buf[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Frames as the relay (server side) would send them.
    fn relay_frames(msgs: &[FromRelay<'_>]) -> Vec<u8> {
        let mut ws = WebSocket::from_raw_socket(
            io::Cursor::new(Vec::new()),
            tungstenite::protocol::Role::Server,
            None,
        );
        for m in msgs {
            ws.send(Message::binary(m.encode().unwrap())).unwrap();
        }
        ws.get_ref().get_ref().clone()
    }

    /// What the relay received so far, decoded (a partial last frame is left out).
    fn received(wire: &Arc<Mutex<Wire>>) -> Vec<String> {
        let bytes = wire.lock().unwrap().outbound.clone();
        let mut ws = WebSocket::from_raw_socket(
            io::Cursor::new(bytes),
            tungstenite::protocol::Role::Server,
            None,
        );
        let mut out = Vec::new();
        while let Ok(Message::Binary(b)) = ws.read() {
            out.push(format!("{:?}", ToRelay::decode(&b).unwrap()));
        }
        out
    }

    #[test]
    fn queued_sends_survive_would_block_reads_and_writes_and_routing_continues() {
        let wire = Arc::new(Mutex::new(Wire {
            chunk: 16,
            ..Wire::default()
        }));
        let mut conn = Conn {
            ws: WebSocket::from_raw_socket(
                Scripted(wire.clone()),
                tungstenite::protocol::Role::Client,
                None,
            ),
            peers: HashMap::new(),
        };
        let (cmds_tx, cmds) = std::sync::mpsc::channel();
        let (events_tx, events) = std::sync::mpsc::channel();
        let dir = tempfile::tempdir().unwrap();
        let key = relay::RoomKey {
            room: [1; 16],
            secret: [2; 32],
        };
        let mut worker = Worker {
            relay: RelayConfig::new("ws://127.0.0.1:9", &key.encode()).unwrap(),
            identity: Keypair::generate().unwrap(),
            epochs: Arc::new(Mutex::new(Epochs::load(&dir.path().join("epoch")))),
            cmds,
            shared: Arc::new(Shared {
                stop: AtomicBool::new(false),
                up: AtomicBool::new(true),
                socket: Mutex::new(None),
                woken: AtomicBool::new(false),
            }),
            events: Box::new(move |e| {
                let _ = events_tx.send(format!("{e:?}"));
            }),
            tokens: Vec::new(),
            invite: None,
            pace: Pace::new(Instant::now()),
            next_peer: 0,
        };
        let rounds = |worker: &mut Worker, conn: &mut Conn<Scripted>, n: usize| {
            for _ in 0..n {
                assert!(worker.pump(conn).is_none(), "the connection ended");
            }
        };
        // Rounds until the relay has `want` messages (each round moves one chunk at most).
        let deliver = |worker: &mut Worker, conn: &mut Conn<Scripted>, want: usize| {
            for _ in 0..400 {
                if received(&wire).len() >= want {
                    return;
                }
                assert!(worker.pump(conn).is_none(), "the connection ended");
            }
        };

        // The first token set's write is refused (WouldBlock) mid-send: not a loss.
        cmds_tx.send(Cmd::Tokens(vec![[7; 32]])).unwrap();
        rounds(&mut worker, &mut conn, 1);
        // Nothing to read in between (WouldBlock reads) and a second set queued behind it.
        cmds_tx.send(Cmd::Tokens(vec![[7; 32], [8; 32]])).unwrap();
        deliver(&mut worker, &mut conn, 2);
        let sent = received(&wire);
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert!(sent[0].starts_with("Tokens") && sent[1].starts_with("Tokens"));
        assert!(
            sent[1].contains("8, 8"),
            "the newer set went last: {sent:?}"
        );

        // Inbound routing still works after all of that.
        wire.lock()
            .unwrap()
            .inbound
            .extend(relay_frames(&[FromRelay::Open {
                peer: 3,
                token: [7; 32],
            }]));
        rounds(&mut worker, &mut conn, 1);
        let opened = events.try_iter().collect::<Vec<_>>();
        assert!(
            opened
                .iter()
                .any(|e| e.contains("Opened") && e.contains("peer: 3")),
            "{opened:?}"
        );
        // And so do sends after it.
        cmds_tx
            .send(Cmd::Close {
                peer: 3,
                code: close::UNAUTHORIZED,
            })
            .unwrap();
        deliver(&mut worker, &mut conn, 3);
        let sent = received(&wire);
        assert_eq!(sent.len(), 3, "{sent:?}");
        assert!(sent[2].starts_with("Close"), "{sent:?}");
    }

    #[test]
    fn stopping_a_link_stuck_in_its_handshake_never_blocks_the_caller() {
        // The "relay" accepts TCP and never answers the upgrade: the thread blocks in the
        // handshake (up to its 10 s timeout) until the stop cuts the socket.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        let link = start(port, tx);
        let (_held, _) = listener.accept().unwrap();
        let started = Instant::now();
        link.stop();
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "stop blocked for {:?}",
            started.elapsed()
        );
        // The cut ends the thread well before the handshake timeout.
        assert!(ended(&rx, Duration::from_secs(2)));
    }

    #[test]
    fn a_replaced_link_stays_down_without_retrying() {
        // A "relay" that answers the gateway hello with REPLACED, then counts reconnects.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let relay = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            let hello = ws.read().unwrap();
            assert!(matches!(hello, Message::Binary(_)), "{hello:?}");
            ws.close(Some(CloseFrame {
                code: CloseCode::from(close::REPLACED),
                reason: "".into(),
            }))
            .unwrap();
            while ws.read().is_ok() {}
            // Anything past the slowest retry pace would be a reconnect.
            listener.set_nonblocking(true).unwrap();
            std::thread::sleep(Duration::from_millis(1_500));
            listener.accept().is_ok()
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let link = start(port, tx);
        let mut events = Vec::new();
        let end = Instant::now() + Duration::from_secs(10);
        let ended = loop {
            match rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
                Ok(e) => events.push(e),
                Err(RecvTimeoutError::Disconnected) => break true,
                Err(RecvTimeoutError::Timeout) => break false,
            }
        };
        assert!(ended, "the link thread kept running: {events:?}");
        let last = events.last().cloned().unwrap_or_default();
        assert!(
            last.contains("Refused") && last.contains("took over"),
            "{events:?}"
        );
        assert!(!relay.join().unwrap(), "the replaced link reconnected");
        drop(link);
        let _ = std::fs::remove_file(
            std::env::temp_dir().join(format!("omp-deck-test-epoch-{}-{port}", std::process::id())),
        );
    }
}
