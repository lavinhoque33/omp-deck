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
use std::io;
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use deck_remote::noise::{
    self, DeviceHello, Keypair, Mode, Pending, Psk, PublicKey, Session, Welcome,
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
    /// Answer the held message 1 of `peer` with message 2.
    Accept {
        peer: PeerId,
        welcome: Welcome,
    },
    /// Close the device connection (the relay reports `Closed`).
    Close {
        peer: PeerId,
        code: u16,
    },
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
}

struct Shared {
    stop: AtomicBool,
    /// The relay accepted the hello: a stop closes politely instead of cutting the socket.
    up: AtomicBool,
    /// A clone of the connected socket: a stop shuts it down to unblock the thread.
    socket: Mutex<Option<TcpStream>>,
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
    Live(Box<Session>),
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
                Ok(Cmd::Accept { .. } | Cmd::Close { .. }) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return true,
            }
        }
    }

    fn connect(&self, tls: Option<&Arc<rustls::ClientConfig>>) -> Result<Ws, String> {
        let url = &self.relay.url;
        let addrs = (url.host(), url.port())
            .to_socket_addrs()
            .map_err(|e| format!("cannot resolve {}: {e}", url.host()))?;
        let mut last = None;
        let mut stream = None;
        for addr in addrs {
            if self.shared.stopped() {
                return Err("stopped".into());
            }
            match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let stream = stream.ok_or_else(|| match last {
            Some(e) => format!("cannot connect to {}: {e}", url.as_str()),
            None => format!("{} has no address", url.host()),
        })?;
        let io_err = |e: io::Error| format!("connection to {}: {e}", url.as_str());
        stream.set_nodelay(true).map_err(io_err)?;
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
        if let Some(s) = lock(&self.shared.socket).as_ref()
            && let Err(e) = s.set_read_timeout(Some(POLL))
        {
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
            if self.shared.stopped() {
                conn.goodbye();
                return End::Stopped;
            }
            loop {
                match self.cmds.try_recv() {
                    Ok(cmd) => {
                        if let Err(e) = self.command(&mut conn, cmd) {
                            return lost(e, true);
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        conn.goodbye();
                        return End::Stopped;
                    }
                }
            }
            match conn.ws.read() {
                Ok(Message::Binary(b)) => {
                    let msg = match FromRelay::decode(&b) {
                        Ok(m) => m,
                        Err(e) => return lost(format!("undecodable relay message: {e}"), true),
                    };
                    if let Err(e) = self.relayed(&mut conn, msg) {
                        return lost(e, true);
                    }
                }
                Ok(Message::Close(frame)) => return closed_by_relay(frame, true),
                Ok(Message::Text(_)) => return lost("the relay sent text".into(), true),
                Ok(_) => {}
                Err(e) if is_timeout(&e) => {
                    // Queued control replies (pongs) go out with the next write or flush.
                    match conn.ws.flush() {
                        Ok(()) => {}
                        Err(e) if is_timeout(&e) => {}
                        Err(e) => return lost(format!("relay connection: {e}"), true),
                    }
                }
                Err(e) => return lost(format!("relay connection: {e}"), true),
            }
        }
    }

    fn command(&mut self, conn: &mut Conn, cmd: Cmd) -> Result<(), String> {
        match cmd {
            Cmd::Tokens(t) => {
                self.tokens = t;
                conn.send(&ToRelay::Tokens(self.tokens.clone()))
            }
            Cmd::Invite(i) => {
                self.invite = i;
                Ok(())
            }
            Cmd::Accept { peer, welcome } => match conn.peers.remove(&peer) {
                Some(Slot::Held(pending)) => match pending.accept(&welcome) {
                    Ok((session, msg2)) => {
                        conn.send(&ToRelay::Data {
                            peer,
                            payload: &msg2,
                        })?;
                        conn.peers.insert(peer, Slot::Live(Box::new(session)));
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
        }
    }

    fn relayed(&mut self, conn: &mut Conn, msg: FromRelay<'_>) -> Result<(), String> {
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
                Some(Slot::Live(session)) => {
                    // No application messages exist yet (R4): an authentic one is a protocol
                    // error, anything else a forgery.
                    let code = match session.open(payload) {
                        Ok(_) => close::PROTOCOL,
                        Err(_) => close::UNAUTHORIZED,
                    };
                    log::info!("relay peer {peer}: unexpected message ({code})");
                    conn.close(peer, code)
                }
            },
        }
    }
}

struct Conn {
    ws: Ws,
    peers: HashMap<PeerId, Slot>,
}

impl Conn {
    fn send(&mut self, msg: &ToRelay<'_>) -> Result<(), String> {
        let bytes = msg
            .encode()
            .map_err(|e| format!("encoding a relay message: {e}"))?;
        self.ws
            .send(Message::binary(bytes))
            .map_err(|e| format!("relay connection: {e}"))
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

fn is_timeout(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
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
        assert!(started.elapsed() < STOP_WAIT, "{:?}", started.elapsed());
        assert!(ended(&rx, Duration::from_secs(1)));
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
