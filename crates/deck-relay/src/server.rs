//! The relay: admission, routing between one gateway and its devices per room, limits, health and
//! graceful shutdown (`docs/notes/mobile-relay.md`, "Relay protocol").

use std::collections::{HashMap, HashSet};
use std::future::{Future, IntoFuture as _};
use std::net::{IpAddr, SocketAddr};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use deck_remote::relay::{
    self, DEVICE_PATH, FromRelay, GATEWAY_PATH, HEALTH_PATH, HELLO_OK, Hash, Hello, PeerId, Room,
    Secret, ToRelay, Token, close,
};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt as _, StreamExt as _};
use subtle::ConstantTimeEq as _;
use tokio::net::TcpListener;
use tokio::sync::{Notify, oneshot};
use tokio::time::{Instant, timeout, timeout_at};

use crate::conn::{Conn, Next, RateLimiter};
use crate::tls::TlsListener;
use crate::transport::{Limits, PeerAddr, Plain, Slot};
use crate::{Config, RelayError, rooms};

/// Runs the relay on `listener` until `shutdown` resolves, then closes every WebSocket with
/// `GOING_AWAY` and returns within [`Config::shutdown_timeout`].
pub async fn serve<F>(config: Config, listener: TcpListener, shutdown: F) -> Result<(), RelayError>
where
    F: Future<Output = ()> + Send,
{
    let local = listener.local_addr().map_err(RelayError::Listen)?;
    let tls = config.tls.clone();
    let handshake_timeout = config.tls_handshake_timeout;
    let request_timeout = config.request_timeout;
    let relay = Arc::new(Relay::new(config));
    let limits = relay.limits.clone();
    let make = Router::new()
        .route(GATEWAY_PATH, get(gateway_ws))
        .route(DEVICE_PATH, get(device_ws))
        .route(HEALTH_PATH, get(|| async { "ok" }))
        .with_state(relay.clone())
        .into_make_service_with_connect_info::<PeerAddr>();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let stop = async move {
        let _ = stop_rx.await;
    };
    let mut server = match tls {
        Some(tls) => {
            let listener =
                TlsListener::new(listener, tls, limits, handshake_timeout, request_timeout)
                    .map_err(RelayError::Listen)?;
            log::info!("listening on {local} (TLS)");
            tokio::spawn(
                axum::serve(listener, make)
                    .with_graceful_shutdown(stop)
                    .into_future(),
            )
        }
        None => {
            log::info!("listening on {local}");
            tokio::spawn(
                axum::serve(Plain::new(listener, limits, request_timeout), make)
                    .with_graceful_shutdown(stop)
                    .into_future(),
            )
        }
    };
    let reload = tokio::spawn({
        let relay = relay.clone();
        async move {
            let every = relay.config.rooms_reload_interval;
            let mut tick = tokio::time::interval_at(Instant::now() + every, every);
            loop {
                tick.tick().await;
                relay.reload_rooms();
            }
        }
    });

    let early = tokio::select! {
        () = shutdown => None,
        r = &mut server => Some(r),
    };
    reload.abort();
    relay.begin_shutdown();
    if let Some(r) = early {
        let e = match r {
            Ok(Ok(())) => std::io::Error::other("server stopped unexpectedly"),
            Ok(Err(e)) => e,
            Err(e) => std::io::Error::other(e),
        };
        return Err(RelayError::Listen(e));
    }
    log::info!("shutting down");
    let _ = stop_tx.send(());
    let deadline = relay.config.shutdown_timeout;
    let drained = timeout(deadline, async {
        let _ = (&mut server).await;
        relay.wait_idle().await;
    })
    .await;
    if drained.is_err() {
        log::warn!(
            "shutdown: {} connection(s) still open after {deadline:?}",
            relay.conns().len()
        );
        server.abort();
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    Gateway,
    Device,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::Gateway => "gateway",
            Role::Device => "device",
        }
    }
}

/// What a connection's hello admitted it as.
enum Registration {
    Gateway(Room),
    Device(Room, PeerId),
}

struct Relay {
    config: Config,
    /// Enrolled rooms (`rooms.json`): room → `sha256(secret)`. Lock order: `rooms`, then `live`.
    rooms: Mutex<HashMap<Room, Hash>>,
    live: Mutex<HashMap<Room, LiveRoom>>,
    limits: Arc<Limits>,
    /// Every upgraded WebSocket, for shutdown.
    conns: Mutex<HashMap<u64, Arc<Conn>>>,
    next_conn: AtomicU64,
    shutting_down: AtomicBool,
    idle: Notify,
}

struct Device {
    conn: Arc<Conn>,
    token: Hash,
}

/// Runtime state of one room. Kept after its gateway leaves so peer ids stay unique.
struct LiveRoom {
    room: Room,
    gateway: Option<Arc<Conn>>,
    /// The hello epoch of `gateway` (meaningless without one; never kept after it leaves).
    gateway_epoch: u64,
    /// The admission token hashes of `gateway`; `None` until its first `Tokens` (devices are then
    /// refused `GATEWAY_OFFLINE`, retryable, not `UNAUTHORIZED`).
    tokens: Option<HashSet<Hash>>,
    devices: HashMap<PeerId, Device>,
    last_peer: PeerId,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl LiveRoom {
    fn new(room: Room) -> LiveRoom {
        LiveRoom {
            room,
            gateway: None,
            gateway_epoch: 0,
            tokens: None,
            devices: HashMap::new(),
            last_peer: 0,
        }
    }

    fn id(&self) -> String {
        relay::room_id(&self.room)
    }

    fn alloc_peer(&mut self) -> PeerId {
        loop {
            self.last_peer = self.last_peer.wrapping_add(1);
            if self.last_peer != 0 && !self.devices.contains_key(&self.last_peer) {
                return self.last_peer;
            }
        }
    }

    /// Queues a message for the gateway; a full gateway queue drops the gateway.
    fn send_gateway(&mut self, msg: &FromRelay<'_>) {
        let Some(gateway) = &self.gateway else {
            return;
        };
        let bytes = Bytes::from(msg.encode().expect("relay messages are within limits"));
        if gateway.push(bytes).is_err() {
            log::warn!(
                "room {}: gateway #{} too slow, closing ({})",
                self.id(),
                gateway.id,
                close::SLOW_CONSUMER
            );
            self.drop_gateway(None);
        }
    }

    /// Removes the gateway (closing it with `code` unless already closing) and closes its devices
    /// `devices_code`.
    fn drop_gateway_with(&mut self, code: Option<u16>, devices_code: u16) {
        let Some(gateway) = self.gateway.take() else {
            return;
        };
        if let Some(code) = code {
            gateway.close(code);
        }
        for (peer, d) in self.devices.drain() {
            log::info!(
                "room {}: closing device {peer} ({devices_code})",
                relay::room_id(&self.room)
            );
            d.conn.close(devices_code);
        }
        self.tokens = None;
    }

    fn drop_gateway(&mut self, code: Option<u16>) {
        self.drop_gateway_with(code, close::GATEWAY_OFFLINE);
    }

    /// Removes a device (closing it with `code`, if given) and reports `Closed` to the gateway.
    fn remove_device(&mut self, peer: PeerId, code: Option<u16>) {
        let Some(d) = self.devices.remove(&peer) else {
            return;
        };
        match code {
            Some(code) => {
                log::info!("room {}: closing device {peer} ({code})", self.id());
                d.conn.close(code);
            }
            None => log::info!("room {}: device {peer} left", self.id()),
        }
        self.send_gateway(&FromRelay::Closed { peer });
    }
}

impl Relay {
    fn new(config: Config) -> Relay {
        let rooms = match rooms::read_table(&config.state_dir) {
            Ok(t) => t,
            Err(e) => {
                log::error!("{e}; starting with no rooms");
                HashMap::new()
            }
        };
        log::info!("{} room(s) enrolled", rooms.len());
        Relay {
            limits: Limits::new(&config),
            config,
            rooms: Mutex::new(rooms),
            live: Mutex::new(HashMap::new()),
            conns: Mutex::new(HashMap::new()),
            next_conn: AtomicU64::new(1),
            shutting_down: AtomicBool::new(false),
            idle: Notify::new(),
        }
    }

    fn conns(&self) -> MutexGuard<'_, HashMap<u64, Arc<Conn>>> {
        lock(&self.conns)
    }

    /// Re-reads `rooms.json`. Rooms that disappeared (or whose secret changed) lose their gateway
    /// and devices (`UNAUTHORIZED`). An unreadable file keeps the previous table.
    fn reload_rooms(&self) {
        let table = match rooms::read_table(&self.config.state_dir) {
            Ok(t) => t,
            Err(e) => {
                log::error!("{e}; keeping the previous room table");
                return;
            }
        };
        let mut rooms = lock(&self.rooms);
        let revoked: Vec<Room> = rooms
            .iter()
            .filter(|(room, hash)| table.get(*room) != Some(*hash))
            .map(|(room, _)| *room)
            .collect();
        *rooms = table;
        if revoked.is_empty() {
            return;
        }
        let mut live = lock(&self.live);
        for room in revoked {
            log::info!("room {} was removed", relay::room_id(&room));
            if let Some(lr) = live.get_mut(&room) {
                lr.drop_gateway_with(Some(close::UNAUTHORIZED), close::UNAUTHORIZED);
            }
        }
    }

    /// Admits a gateway, displacing the current one unless `epoch` is older than its epoch (a
    /// stale hello is refused with `REPLACED` and the current gateway is left alone).
    fn admit_gateway(
        &self,
        conn: &Arc<Conn>,
        room: Room,
        secret: &Secret,
        epoch: u64,
    ) -> Result<(), u16> {
        self.reload_rooms();
        let rooms = lock(&self.rooms);
        let stored = rooms.get(&room).ok_or(close::UNAUTHORIZED)?;
        if !bool::from(relay::hash(secret).ct_eq(stored)) {
            return Err(close::UNAUTHORIZED);
        }
        let mut live = lock(&self.live);
        let lr = live.entry(room).or_insert_with(|| LiveRoom::new(room));
        if let Some(old) = &lr.gateway {
            if epoch < lr.gateway_epoch {
                log::info!(
                    "room {}: stale gateway hello #{} (epoch {epoch}) refused; gateway #{} \
                     (epoch {}) stays ({})",
                    lr.id(),
                    conn.id,
                    old.id,
                    lr.gateway_epoch,
                    close::REPLACED
                );
                return Err(close::REPLACED);
            }
            log::info!(
                "room {}: gateway #{} (epoch {}) replaced by #{} (epoch {epoch}) ({})",
                lr.id(),
                old.id,
                lr.gateway_epoch,
                conn.id,
                close::REPLACED
            );
            lr.drop_gateway(Some(close::REPLACED));
        }
        // A fresh queue: the hello answer is always its first message.
        let _ = conn.push(Bytes::from_static(&HELLO_OK));
        lr.gateway = Some(conn.clone());
        lr.gateway_epoch = epoch;
        // Devices wait (GATEWAY_OFFLINE) until this gateway registers its token set.
        lr.tokens = None;
        Ok(())
    }

    fn admit_device(&self, conn: &Arc<Conn>, room: Room, token: &Token) -> Result<PeerId, u16> {
        let token = relay::hash(token);
        let mut live = lock(&self.live);
        let lr = live
            .get_mut(&room)
            .filter(|lr| lr.gateway.is_some())
            .ok_or(close::GATEWAY_OFFLINE)?;
        let tokens = lr.tokens.as_ref().ok_or(close::GATEWAY_OFFLINE)?;
        if !tokens.contains(&token) {
            return Err(close::UNAUTHORIZED);
        }
        if lr.devices.len() >= self.config.max_devices_per_room {
            return Err(close::RATE_LIMITED);
        }
        let peer = lr.alloc_peer();
        let _ = conn.push(Bytes::from_static(&HELLO_OK));
        lr.devices.insert(
            peer,
            Device {
                conn: conn.clone(),
                token,
            },
        );
        lr.send_gateway(&FromRelay::Open { peer, token });
        Ok(peer)
    }

    /// A device payload, forwarded to its gateway.
    fn device_message(&self, room: &Room, peer: PeerId, conn: &Conn, payload: &[u8]) {
        let mut live = lock(&self.live);
        let Some(lr) = live.get_mut(room) else {
            return;
        };
        if lr.devices.get(&peer).is_some_and(|d| d.conn.id == conn.id) {
            lr.send_gateway(&FromRelay::Data { peer, payload });
        }
    }

    /// A gateway message. `Err` = undecodable.
    fn gateway_message(&self, room: &Room, conn: &Conn, msg: &Bytes) -> Result<(), ()> {
        let decoded = ToRelay::decode(msg).map_err(|_| ())?;
        let mut live = lock(&self.live);
        let Some(lr) = live.get_mut(room) else {
            return Ok(());
        };
        if lr.gateway.as_ref().is_none_or(|g| g.id != conn.id) {
            return Ok(());
        }
        match decoded {
            ToRelay::Data { peer, payload } => {
                let Some(d) = lr.devices.get(&peer) else {
                    return Ok(());
                };
                if d.conn.push(msg.slice_ref(payload)).is_err() {
                    log::warn!(
                        "room {}: device {peer} too slow ({})",
                        lr.id(),
                        close::SLOW_CONSUMER
                    );
                    lr.remove_device(peer, None);
                }
            }
            ToRelay::Tokens(set) => {
                let tokens = lr.tokens.insert(set.into_iter().collect());
                let revoked: Vec<PeerId> = lr
                    .devices
                    .iter()
                    .filter(|(_, d)| !tokens.contains(&d.token))
                    .map(|(peer, _)| *peer)
                    .collect();
                let count = tokens.len();
                log::debug!(
                    "room {}: {count} admission token(s), {} device(s) revoked",
                    lr.id(),
                    revoked.len()
                );
                for peer in revoked {
                    lr.remove_device(peer, Some(close::UNAUTHORIZED));
                }
            }
            ToRelay::Close { peer, code } => {
                let code = if code == close::NORMAL || (4000..=4999).contains(&code) {
                    code
                } else {
                    close::NORMAL
                };
                lr.remove_device(peer, Some(code));
            }
        }
        Ok(())
    }

    /// Bookkeeping once a connection ends (no-op when the relay already removed it).
    fn gone(&self, conn: &Conn, reg: Option<&Registration>) {
        let mut live = lock(&self.live);
        match reg {
            Some(Registration::Gateway(room)) => {
                if let Some(lr) = live.get_mut(room)
                    && lr.gateway.as_ref().is_some_and(|g| g.id == conn.id)
                {
                    log::info!("room {}: gateway #{} left", lr.id(), conn.id);
                    lr.drop_gateway(None);
                }
            }
            Some(Registration::Device(room, peer)) => {
                if let Some(lr) = live.get_mut(room)
                    && lr.devices.get(peer).is_some_and(|d| d.conn.id == conn.id)
                {
                    lr.remove_device(*peer, None);
                }
            }
            None => {}
        }
    }

    fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        for conn in self.conns().values() {
            conn.close(close::GOING_AWAY);
        }
    }

    async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            let mut notified = pin!(notified);
            notified.as_mut().enable();
            if self.conns().is_empty() {
                return;
            }
            notified.await;
        }
    }
}

/// The address the per-IP limits key on: the socket peer, or with `--trust-forwarded-for` the
/// last `X-Forwarded-For` hop (the one the trusted proxy appended).
fn client_ip(config: &Config, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    if config.trust_forwarded_for
        && let Some(ip) = headers
            .get_all("x-forwarded-for")
            .iter()
            .next_back()
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .and_then(|hop| hop.trim().parse::<IpAddr>().ok())
    {
        return ip;
    }
    peer.ip()
}

async fn gateway_ws(
    State(relay): State<Arc<Relay>>,
    ConnectInfo(peer): ConnectInfo<PeerAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    upgrade(relay, Role::Gateway, &peer, &headers, ws)
}

async fn device_ws(
    State(relay): State<Arc<Relay>>,
    ConnectInfo(peer): ConnectInfo<PeerAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    upgrade(relay, Role::Device, &peer, &headers, ws)
}

fn upgrade(
    relay: Arc<Relay>,
    role: Role,
    peer: &PeerAddr,
    headers: &HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if relay.shutting_down.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let ip = client_ip(&relay.config, peer.addr, headers);
    let Some(slot) = relay.limits.forwarded(ip) else {
        log::warn!(
            "refusing {} connection from {ip}: too many connections",
            role.name()
        );
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    peer.mark_upgraded();
    // tungstenite's own cap is twice the relay limit: a message up to that size is read whole and
    // answered with TOO_LARGE (no unread bytes that would turn the close into a TCP reset).
    let tungstenite_max = max_message(&relay.config, role).saturating_mul(2);
    ws.max_message_size(tungstenite_max)
        .max_frame_size(tungstenite_max)
        .on_failed_upgrade(move |e| log::debug!("WebSocket upgrade from {ip} failed: {e}"))
        .on_upgrade(move |socket| run(relay, role, ip, socket, slot))
}

fn max_message(config: &Config, role: Role) -> usize {
    match role {
        Role::Gateway => config.max_gateway_message,
        Role::Device => config.max_device_message,
    }
}

async fn run(relay: Arc<Relay>, role: Role, ip: IpAddr, socket: WebSocket, _slot: Option<Slot>) {
    let id = relay.next_conn.fetch_add(1, Ordering::Relaxed);
    let conn = Arc::new(Conn::new(
        id,
        relay.config.queue_messages,
        relay.config.queue_bytes,
    ));
    relay.conns().insert(id, conn.clone());
    if relay.shutting_down.load(Ordering::SeqCst) {
        conn.close(close::GOING_AWAY);
    }
    log::debug!("{} #{id} connected from {ip}", role.name());
    let close_timeout = relay.config.close_timeout;
    let reg = OnceLock::new();
    let (mut sink, mut stream) = socket.split();
    {
        let mut writer = pin!(write_loop(&relay.config, &conn, &mut sink));
        let reader_first = {
            let mut reader = pin!(read_loop(&relay, &conn, role, ip, &mut stream, &reg));
            tokio::select! {
                () = &mut reader => true,
                () = &mut writer => false,
            }
        };
        // The peer left (close frame, EOF or reset) without a relay-side close.
        let peer_left = reader_first && conn.close_code().is_none();
        if peer_left {
            // Nothing more to send.
            conn.close(close::NORMAL);
        }
        relay.gone(&conn, reg.get());
        // A relay-side close: read until the peer answers it, so the socket is not dropped with
        // unread input.
        if reader_first && !peer_left {
            // A violation: the writer sends the close frame while the input is drained.
            let _ = timeout(close_timeout * 2, async {
                tokio::join!(&mut writer, drain(&mut stream));
            })
            .await;
        } else if !reader_first {
            // The close frame is out (or the socket failed).
            let _ = timeout(close_timeout, drain(&mut stream)).await;
        }
    }
    log::debug!(
        "{} #{id} from {ip} closed ({})",
        role.name(),
        conn.close_code().unwrap_or(close::NORMAL)
    );
    let mut conns = relay.conns();
    conns.remove(&id);
    if conns.is_empty() {
        relay.idle.notify_waiters();
    }
}

/// Bytes [`drain`] discards before giving up on a closing peer.
const MAX_DRAIN_BYTES: usize = 16 * 1024 * 1024;

/// After a relay-side close: reads and discards (neither charged nor forwarded) until the peer's
/// close reply or the end of the socket. Dropping a socket with unread input makes the OS send a
/// reset instead of a FIN, which can destroy the close frame before the peer reads it (Windows
/// discards the peer's receive buffer on a reset) and breaks the peer's close reply.
async fn drain(stream: &mut SplitStream<WebSocket>) {
    let mut left = MAX_DRAIN_BYTES;
    while let Some(Ok(msg)) = stream.next().await {
        let len = match msg {
            Message::Close(_) => return,
            Message::Binary(b) | Message::Ping(b) | Message::Pong(b) => b.len(),
            Message::Text(t) => t.len(),
        };
        match left.checked_sub(len) {
            Some(l) => left = l,
            None => return,
        }
    }
}

/// Why reading a WebSocket failed.
#[derive(Debug, PartialEq, Eq)]
enum ReadError {
    /// The peer broke a rule: answer with this close code.
    Violation(u16),
    /// The connection is gone (reset, or EOF without a close frame): nothing to answer.
    Lost,
}

fn read_error(e: axum::Error) -> ReadError {
    use tungstenite::error::ProtocolError;
    match e.into_inner().downcast::<tungstenite::Error>().map(|e| *e) {
        Ok(tungstenite::Error::Capacity(_)) => ReadError::Violation(close::TOO_LARGE),
        Ok(tungstenite::Error::Protocol(ProtocolError::ResetWithoutClosingHandshake)) => {
            ReadError::Lost
        }
        Ok(tungstenite::Error::Protocol(_) | tungstenite::Error::Utf8(_)) => {
            ReadError::Violation(close::PROTOCOL)
        }
        _ => ReadError::Lost,
    }
}

/// Reads the hello, then the peer's messages, until the peer leaves or breaks a rule (then the
/// close is requested on `conn` and this returns). Messages that arrive while the connection is
/// closing are discarded.
async fn read_loop(
    relay: &Relay,
    conn: &Arc<Conn>,
    role: Role,
    ip: IpAddr,
    stream: &mut SplitStream<WebSocket>,
    reg: &OnceLock<Registration>,
) {
    let config = &relay.config;
    let max = max_message(config, role);
    let mut limiter = RateLimiter::new(match role {
        Role::Gateway => config.gateway_rate,
        Role::Device => config.device_rate,
    });
    let reject = |code: u16, why: &str| {
        log::info!("{} #{} from {ip}: {why} ({code})", role.name(), conn.id);
        conn.close(code);
    };
    // A lost connection needs no close: `run` then finishes it like a peer that left.
    let failed = |e: axum::Error, what: &str| match read_error(e) {
        ReadError::Violation(code) => reject(code, what),
        ReadError::Lost => log::info!("{} #{} from {ip}: connection lost", role.name(), conn.id),
    };

    let deadline = Instant::now() + config.hello_timeout;
    let first = loop {
        let msg = match timeout_at(deadline, stream.next()).await {
            Err(_) => return reject(close::BAD_HELLO, "no hello in time"),
            Ok(None) => return,
            Ok(Some(Err(e))) => return failed(e, "unreadable hello"),
            Ok(Some(Ok(msg))) => msg,
        };
        match msg {
            Message::Close(_) => return,
            // Closing: drain until the peer answers the close frame.
            _ if conn.close_code().is_some() => continue,
            Message::Text(_) => return reject(close::PROTOCOL, "text message"),
            // Every message before the hello is charged too, so it cannot be flooded with pings.
            Message::Ping(b) | Message::Pong(b) => {
                if !limiter.take(b.len()) {
                    return reject(close::RATE_LIMITED, "rate limit");
                }
            }
            Message::Binary(b) => break b,
        }
    };
    if first.len() > max {
        return reject(close::TOO_LARGE, "oversized hello");
    }
    if !limiter.take(first.len()) {
        return reject(close::RATE_LIMITED, "rate limit");
    }
    let registration = match (role, Hello::decode(&first)) {
        (
            Role::Gateway,
            Ok(Hello::Gateway {
                room,
                secret,
                epoch,
            }),
        ) => match relay.admit_gateway(conn, room, &secret, epoch) {
            Ok(()) => {
                log::info!(
                    "room {}: gateway #{} from {ip} connected",
                    relay::room_id(&room),
                    conn.id
                );
                Registration::Gateway(room)
            }
            Err(code) => return reject(code, &format!("room {}", relay::room_id(&room))),
        },
        (Role::Device, Ok(Hello::Device { room, token })) => {
            match relay.admit_device(conn, room, &token) {
                Ok(peer) => {
                    log::info!(
                        "room {}: device {peer} (#{}) from {ip} connected",
                        relay::room_id(&room),
                        conn.id
                    );
                    Registration::Device(room, peer)
                }
                Err(code) => return reject(code, &format!("room {}", relay::room_id(&room))),
            }
        }
        _ => return reject(close::BAD_HELLO, "bad hello"),
    };
    let registration = reg.get_or_init(|| registration);

    loop {
        let msg = match timeout(config.idle_timeout, stream.next()).await {
            Err(_) => return reject(close::NORMAL, "idle"),
            Ok(None) => return,
            Ok(Some(Err(e))) => return failed(e, "unreadable message"),
            Ok(Some(Ok(msg))) => msg,
        };
        let payload = match msg {
            Message::Close(_) => return,
            // Closing: drain until the peer answers the close frame.
            _ if conn.close_code().is_some() => continue,
            Message::Text(_) => return reject(close::PROTOCOL, "text message"),
            Message::Ping(b) | Message::Pong(b) => {
                if !limiter.take(b.len()) {
                    return reject(close::RATE_LIMITED, "rate limit");
                }
                continue;
            }
            Message::Binary(b) => b,
        };
        if payload.len() > max {
            return reject(close::TOO_LARGE, "oversized message");
        }
        if !limiter.take(payload.len()) {
            return reject(close::RATE_LIMITED, "rate limit");
        }
        match registration {
            Registration::Gateway(room) => {
                if relay.gateway_message(room, conn, &payload).is_err() {
                    return reject(close::PROTOCOL, "undecodable message");
                }
            }
            Registration::Device(room, peer) => relay.device_message(room, *peer, conn, &payload),
        }
    }
}

enum Sent {
    Ok,
    /// A close was requested while the write was pending.
    Closing(u16),
    /// The socket failed.
    Gone,
}

async fn send(sink: &mut SplitSink<WebSocket, Message>, conn: &Conn, msg: Message) -> Sent {
    tokio::select! {
        r = sink.send(msg) => if r.is_ok() { Sent::Ok } else { Sent::Gone },
        code = conn.closing() => Sent::Closing(code),
    }
}

/// Writes the queue and pings until a close is requested, then flushes what is left and the close
/// frame within the close timeout.
async fn write_loop(config: &Config, conn: &Conn, sink: &mut SplitSink<WebSocket, Message>) {
    let mut ping =
        tokio::time::interval_at(Instant::now() + config.ping_interval, config.ping_interval);
    let code = loop {
        let msg = tokio::select! {
            next = conn.next() => match next {
                Next::Data(b) => Message::Binary(b),
                Next::Close(code) => break code,
            },
            _ = ping.tick() => Message::Ping(Bytes::new()),
        };
        match send(sink, conn, msg).await {
            Sent::Ok => {}
            Sent::Closing(code) => break code,
            Sent::Gone => return,
        }
    };
    let _ = timeout(config.close_timeout, async {
        while let Next::Data(b) = conn.next().await {
            if sink.feed(Message::Binary(b)).await.is_err() {
                return;
            }
        }
        let frame = CloseFrame {
            code,
            reason: "".into(),
        };
        let _ = sink.send(Message::Close(Some(frame))).await;
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tungstenite::error::{CapacityError, ProtocolError};

    #[test]
    fn abrupt_disconnects_are_lost_connections_not_violations() {
        let classify = |e: tungstenite::Error| read_error(axum::Error::new(e));
        // EOF without a close frame (a client process that exited) and a TCP reset.
        assert_eq!(
            classify(tungstenite::Error::Protocol(
                ProtocolError::ResetWithoutClosingHandshake
            )),
            ReadError::Lost
        );
        assert_eq!(
            classify(tungstenite::Error::Io(
                std::io::ErrorKind::ConnectionReset.into()
            )),
            ReadError::Lost
        );
        assert_eq!(
            classify(tungstenite::Error::ConnectionClosed),
            ReadError::Lost
        );
        // Genuine violations keep their close codes.
        assert_eq!(
            classify(tungstenite::Error::Protocol(
                ProtocolError::NonZeroReservedBits
            )),
            ReadError::Violation(close::PROTOCOL)
        );
        assert_eq!(
            classify(tungstenite::Error::Capacity(
                CapacityError::MessageTooLong {
                    size: 2,
                    max_size: 1
                }
            )),
            ReadError::Violation(close::TOO_LARGE)
        );
    }
}
