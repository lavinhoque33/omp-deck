//! Accept-level connection limits and the per-request HTTP deadlines, shared by the plain and TLS
//! listeners: every TCP connection reserves a [`Slot`] before anything else happens (TLS handshake
//! included) and holds it until the socket is dropped, so sockets that never finish TLS or HTTP
//! count against the same caps as WebSockets.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use axum::body::{Body, Bytes, HttpBody};
use axum::extract::connect_info::Connected;
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use axum::serve::{IncomingStream, Listener};
use http_body::{Frame, SizeHint};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Instant, Sleep};

use crate::Config;
use crate::config::{ProxyRange, canonical, is_proxy};
use crate::tls::TlsListener;

/// The connection counters behind `--max-connections` and `--max-connections-per-ip`.
pub(crate) struct Limits {
    max_total: usize,
    max_per_ip: usize,
    /// With `--trust-forwarded-for`: the proxies (besides loopback) whose connections take their
    /// per-IP slot at the upgrade, for the forwarded address ([`Limits::upgrade`]). Every other
    /// connection is keyed on its socket peer at accept.
    proxies: Option<Vec<ProxyRange>>,
    counts: Mutex<Counts>,
}

#[derive(Default)]
struct Counts {
    per_ip: HashMap<IpAddr, usize>,
    total: usize,
}

/// The per-IP key of a client address: IPv4 as is (IPv4-mapped IPv6 included), IPv6 by its /64
/// (one subscriber's allocation; per-address keys would give one attacker 2^64 "clients").
pub(crate) fn limit_key(ip: IpAddr) -> IpAddr {
    match canonical(ip) {
        IpAddr::V6(v6) => IpAddr::V6((u128::from(v6) & !(u128::MAX >> 64)).into()),
        v4 => v4,
    }
}

impl Limits {
    pub fn new(config: &Config) -> Arc<Limits> {
        Arc::new(Limits {
            max_total: config.max_connections,
            max_per_ip: config.max_connections_per_ip,
            proxies: config
                .trust_forwarded_for
                .then(|| config.trusted_proxies.clone()),
            counts: Mutex::new(Counts::default()),
        })
    }

    fn counts(&self) -> MutexGuard<'_, Counts> {
        self.counts.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn proxied(&self, peer: IpAddr) -> bool {
        self.proxies
            .as_deref()
            .is_some_and(|ranges| is_proxy(ranges, peer))
    }

    /// The slot of a freshly accepted TCP connection from `peer`; `None`: over a cap.
    pub fn accept(self: &Arc<Self>, peer: IpAddr) -> Option<Slot> {
        let ip = (!self.proxied(peer)).then(|| limit_key(peer));
        self.reserve(true, ip)
    }

    /// At the WebSocket upgrade of a connection from `peer` for `client` (the forwarded address,
    /// or `peer`): the per-IP slot of a trusted proxy's client (`None`: over the cap). Other
    /// connections are covered by their accept slot.
    pub fn upgrade(self: &Arc<Self>, peer: IpAddr, client: IpAddr) -> Option<Option<Slot>> {
        if !self.proxied(peer) {
            return Some(None);
        }
        self.reserve(false, Some(limit_key(client))).map(Some)
    }

    fn reserve(self: &Arc<Self>, total: bool, ip: Option<IpAddr>) -> Option<Slot> {
        let mut c = self.counts();
        if total && c.total >= self.max_total {
            return None;
        }
        if let Some(ip) = ip {
            let n = c.per_ip.get(&ip).copied().unwrap_or(0);
            if n >= self.max_per_ip {
                return None;
            }
            c.per_ip.insert(ip, n + 1);
        }
        if total {
            c.total += 1;
        }
        Some(Slot {
            limits: self.clone(),
            total,
            ip,
        })
    }
}

/// A reserved connection slot (total and/or per-IP), released on drop.
pub(crate) struct Slot {
    limits: Arc<Limits>,
    total: bool,
    ip: Option<IpAddr>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut c = self.limits.counts();
        if self.total {
            c.total -= 1;
        }
        if let Some(ip) = self.ip
            && let Some(n) = c.per_ip.get_mut(&ip)
        {
            *n -= 1;
            if *n == 0 {
                c.per_ip.remove(&ip);
            }
        }
    }
}

/// The HTTP timeouts and request cap of one connection, until it upgrades to a WebSocket.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HttpLimits {
    pub header: Duration,
    pub keep_alive: Duration,
    pub write_stall: Duration,
    pub max_requests: u32,
}

impl HttpLimits {
    pub fn of(config: &Config) -> HttpLimits {
        HttpLimits {
            header: config.header_timeout,
            keep_alive: config.keep_alive_timeout,
            write_stall: config.write_stall_timeout,
            max_requests: config.max_requests_per_connection,
        }
    }
}

/// Where a connection's HTTP exchange is, shared between its [`Guarded`] I/O (which enforces the
/// read deadline) and the [`track`] middleware (which sees requests start and responses end).
pub(crate) struct Exchange {
    limits: HttpLimits,
    upgraded: AtomicBool,
    state: Mutex<ExchangeState>,
}

struct ExchangeState {
    phase: Phase,
    requests: u32,
    /// The task last parked on a pending read, woken when the phase changes its deadline.
    reader: Option<Waker>,
}

#[derive(Clone, Copy)]
enum Phase {
    /// Request headers must be complete by then.
    Headers(Instant),
    /// A handler or its response body is running: no read deadline (the write stall applies).
    Busy,
    /// Between requests: closed by then unless traffic moves in either direction.
    Idle(Instant),
}

impl Exchange {
    fn new(limits: HttpLimits) -> Arc<Exchange> {
        Arc::new(Exchange {
            limits,
            upgraded: AtomicBool::new(false),
            state: Mutex::new(ExchangeState {
                phase: Phase::Headers(Instant::now() + limits.header),
                requests: 0,
                reader: None,
            }),
        })
    }

    fn state(&self) -> MutexGuard<'_, ExchangeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn upgraded(&self) -> bool {
        self.upgraded.load(Ordering::Acquire)
    }

    /// The current read deadline; `waker` is woken when it changes.
    fn read_deadline(&self, waker: &Waker) -> Option<Instant> {
        let mut s = self.state();
        match &mut s.reader {
            Some(w) if w.will_wake(waker) => {}
            slot => *slot = Some(waker.clone()),
        }
        match s.phase {
            Phase::Headers(at) | Phase::Idle(at) => Some(at),
            Phase::Busy => None,
        }
    }

    /// Bytes arrived: on an idle connection they start the next request's header deadline.
    fn read_progress(&self) {
        let mut s = self.state();
        if let Phase::Idle(_) = s.phase {
            s.phase = Phase::Headers(Instant::now() + self.limits.header);
        }
    }

    /// Response bytes left: a connection still sending is not idle.
    fn write_progress(&self) {
        let mut s = self.state();
        if let Phase::Idle(_) = s.phase {
            s.phase = Phase::Idle(Instant::now() + self.limits.keep_alive);
        }
    }

    /// A request reached the router; `true`: it is the connection's last.
    fn begin_request(&self) -> bool {
        let mut s = self.state();
        s.phase = Phase::Busy;
        s.requests = s.requests.saturating_add(1);
        s.requests >= self.limits.max_requests
    }

    /// The response body is done (or dropped): the connection goes idle.
    fn end_request(&self) {
        let reader = {
            let mut s = self.state();
            s.phase = Phase::Idle(Instant::now() + self.limits.keep_alive);
            s.reader.take()
        };
        if let Some(w) = reader {
            w.wake();
        }
    }
}

/// A connection holding its [`Slot`] for as long as the socket lives (through the HTTP exchange
/// and the WebSocket after it). Until a handler marks it upgraded, I/O fails with `TimedOut` when
/// the [`Exchange`]'s read deadline passes (slow or never-sent request headers, idle keep-alive)
/// or a write makes no progress for [`HttpLimits::write_stall`]: `axum::serve` configures no hyper
/// timer, so hyper on its own would wait these out forever.
pub(crate) struct Guarded<S> {
    /// Declared first so it is dropped first: once the peer sees the socket close, the slot is
    /// already free.
    _slot: Slot,
    io: S,
    exchange: Arc<Exchange>,
    read_timer: Option<Pin<Box<Sleep>>>,
    write_timer: Option<Pin<Box<Sleep>>>,
    /// `write_timer` runs for the current stall (reset on progress, not reallocated).
    write_armed: bool,
}

impl<S> Guarded<S> {
    pub fn new(io: S, slot: Slot, limits: HttpLimits) -> Guarded<S> {
        Guarded {
            _slot: slot,
            io,
            exchange: Exchange::new(limits),
            read_timer: None,
            write_timer: None,
            write_armed: false,
        }
    }

    /// Whether the connection became a WebSocket (its timers are then dropped for good).
    fn upgraded(&mut self) -> bool {
        if !self.exchange.upgraded() {
            return false;
        }
        self.read_timer = None;
        self.write_timer = None;
        true
    }

    /// Called while a read is pending: arms the read deadline's wakeup, `Err` once it passed.
    fn check_read(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        let Some(deadline) = self.exchange.read_deadline(cx.waker()) else {
            return Ok(());
        };
        let timer = self
            .read_timer
            .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(deadline)));
        if timer.deadline() != deadline {
            timer.as_mut().reset(deadline);
        }
        match timer.as_mut().poll(cx) {
            Poll::Ready(()) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP request deadline passed",
            )),
            Poll::Pending => Ok(()),
        }
    }

    fn guard_write<T>(
        &mut self,
        cx: &mut Context<'_>,
        r: Poll<io::Result<T>>,
    ) -> Poll<io::Result<T>> {
        if self.upgraded() {
            return r;
        }
        if r.is_ready() {
            self.write_armed = false;
            self.exchange.write_progress();
            return r;
        }
        let at = Instant::now() + self.exchange.limits.write_stall;
        let timer = self
            .write_timer
            .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(at)));
        if !self.write_armed {
            timer.as_mut().reset(at);
            self.write_armed = true;
        }
        match timer.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP response write stalled",
            ))),
            Poll::Pending => r,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Guarded<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let filled = buf.filled().len();
        let r = Pin::new(&mut this.io).poll_read(cx, buf);
        if this.upgraded() {
            return r;
        }
        match r {
            Poll::Pending => {
                if let Err(e) = this.check_read(cx) {
                    return Poll::Ready(Err(e));
                }
            }
            Poll::Ready(Ok(())) if buf.filled().len() > filled => this.exchange.read_progress(),
            Poll::Ready(_) => {}
        }
        r
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Guarded<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let r = Pin::new(&mut this.io).poll_write(cx, buf);
        this.guard_write(cx, r)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let r = Pin::new(&mut this.io).poll_write_vectored(cx, bufs);
        this.guard_write(cx, r)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let r = Pin::new(&mut this.io).poll_flush(cx);
        this.guard_write(cx, r)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

/// What handlers learn about their connection, for both plain and TLS listeners.
#[derive(Clone)]
pub(crate) struct PeerAddr {
    pub addr: SocketAddr,
    exchange: Arc<Exchange>,
}

impl std::fmt::Debug for PeerAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerAddr")
            .field("addr", &self.addr)
            .finish()
    }
}

impl PeerAddr {
    /// The connection is becoming a WebSocket: lift the HTTP deadlines (the hello timeout and
    /// idle timeout take over).
    pub fn mark_upgraded(&self) {
        self.exchange.upgraded.store(true, Ordering::Release);
    }

    fn of<S>(io: &Guarded<S>, addr: SocketAddr) -> PeerAddr {
        PeerAddr {
            addr,
            exchange: io.exchange.clone(),
        }
    }
}

/// Router middleware: tells the connection's [`Exchange`] when a request starts and when its
/// response body is done, and closes the connection after its last allowed request.
pub(crate) async fn track(
    ConnectInfo(peer): ConnectInfo<PeerAddr>,
    req: Request,
    next: Next,
) -> Response {
    let last = peer.exchange.begin_request();
    let done = Done(peer.exchange);
    let mut resp = next.run(req).await;
    if last && resp.status() != StatusCode::SWITCHING_PROTOCOLS {
        resp.headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("close"));
    }
    resp.map(|inner| Body::new(Tracked { inner, _done: done }))
}

/// Ends the request on drop: when hyper finished (or abandoned) the response body.
struct Done(Arc<Exchange>);

impl Drop for Done {
    fn drop(&mut self) {
        self.0.end_request();
    }
}

/// A response body that ends its request when dropped.
struct Tracked {
    inner: Body,
    _done: Done,
}

impl HttpBody for Tracked {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        Pin::new(&mut self.get_mut().inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Connected<IncomingStream<'_, Plain>> for PeerAddr {
    fn connect_info(stream: IncomingStream<'_, Plain>) -> Self {
        PeerAddr::of(stream.io(), *stream.remote_addr())
    }
}

impl Connected<IncomingStream<'_, TlsListener>> for PeerAddr {
    fn connect_info(stream: IncomingStream<'_, TlsListener>) -> Self {
        PeerAddr::of(stream.io(), *stream.remote_addr())
    }
}

/// The plain-TCP listener (`TCP_NODELAY` on: relay traffic is small interactive messages).
/// Connections over a cap are closed right after accept.
pub(crate) struct Plain {
    listener: TcpListener,
    limits: Arc<Limits>,
    http: HttpLimits,
}

impl Plain {
    pub fn new(listener: TcpListener, limits: Arc<Limits>, http: HttpLimits) -> Plain {
        Plain {
            listener,
            limits,
            http,
        }
    }
}

impl Listener for Plain {
    type Io = Guarded<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, SocketAddr) {
        loop {
            // axum's `TcpListener` impl already backs off on accept errors (EMFILE).
            let (stream, addr) = Listener::accept(&mut self.listener).await;
            let Some(slot) = self.limits.accept(addr.ip()) else {
                refuse(addr);
                continue;
            };
            let _ = stream.set_nodelay(true);
            return (Guarded::new(stream, slot, self.http), addr);
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

/// Logs a connection closed at accept for being over a cap.
pub(crate) fn refuse(addr: SocketAddr) {
    log::warn!(
        "refusing connection from {}: too many connections",
        canonical(addr.ip())
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn per_ip_keys_fold_mapped_ipv4_and_ipv6_slash_64() {
        assert_eq!(limit_key(ip("::ffff:198.51.100.7")), ip("198.51.100.7"));
        assert_eq!(limit_key(ip("198.51.100.7")), ip("198.51.100.7"));
        assert_eq!(limit_key(ip("2001:db8:1:2:aaaa::1")), ip("2001:db8:1:2::"));
        assert_eq!(
            limit_key(ip("2001:db8:1:2:ffff:ffff:ffff:ffff")),
            ip("2001:db8:1:2::")
        );
        assert_ne!(limit_key(ip("2001:db8:1:3::1")), ip("2001:db8:1:2::"));
    }

    #[test]
    fn only_trusted_proxies_skip_the_per_ip_check_at_accept() {
        let mut config = Config::new("x");
        config.max_connections_per_ip = 1;
        config.trust_forwarded_for = true;
        config.trusted_proxies = vec!["10.88.0.1".parse().unwrap()];
        let limits = Limits::new(&config);
        // Proxies: the per-IP slot waits for the forwarded address.
        let _a = limits.accept(ip("127.0.0.1")).unwrap();
        let _b = limits.accept(ip("127.0.0.1")).unwrap();
        let _c = limits.accept(ip("10.88.0.1")).unwrap();
        let _d = limits.accept(ip("::ffff:10.88.0.1")).unwrap();
        let _f = limits.upgrade(ip("10.88.0.1"), ip("203.0.113.1")).unwrap();
        assert!(limits.upgrade(ip("127.0.0.1"), ip("203.0.113.1")).is_none());
        // Anyone else is keyed on the socket address at accept, whatever header it sends.
        let direct = limits.accept(ip("198.51.100.7")).unwrap();
        assert!(limits.accept(ip("::ffff:198.51.100.7")).is_none());
        assert!(matches!(
            limits.upgrade(ip("198.51.100.7"), ip("203.0.113.9")),
            Some(None)
        ));
        drop(direct);
        assert!(limits.accept(ip("198.51.100.7")).is_some());
        // IPv6: one slot per /64.
        let _v6 = limits.accept(ip("2001:db8::1")).unwrap();
        assert!(limits.accept(ip("2001:db8::2")).is_none());
        assert!(limits.accept(ip("2001:db8:0:1::1")).is_some());
    }
}
