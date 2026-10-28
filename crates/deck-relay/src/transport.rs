//! Accept-level connection limits and the pre-WebSocket deadline, shared by the plain and TLS
//! listeners: every TCP connection reserves a [`Slot`] before anything else happens (TLS handshake
//! included) and holds it until the socket is dropped, so sockets that never finish TLS or HTTP
//! count against the same caps as WebSockets.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::extract::connect_info::Connected;
use axum::serve::{IncomingStream, Listener};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Sleep;

use crate::Config;
use crate::tls::TlsListener;

/// The connection counters behind `--max-connections` and `--max-connections-per-ip`.
pub(crate) struct Limits {
    max_total: usize,
    max_per_ip: usize,
    /// Without `--trust-forwarded-for` the per-IP limit keys on the socket peer and is enforced at
    /// accept; with it the socket peer is the proxy, so the per-IP slot is taken at the upgrade
    /// for the forwarded address instead ([`Limits::forwarded`]).
    per_ip_at_accept: bool,
    counts: Mutex<Counts>,
}

#[derive(Default)]
struct Counts {
    per_ip: HashMap<IpAddr, usize>,
    total: usize,
}

impl Limits {
    pub fn new(config: &Config) -> Arc<Limits> {
        Arc::new(Limits {
            max_total: config.max_connections,
            max_per_ip: config.max_connections_per_ip,
            per_ip_at_accept: !config.trust_forwarded_for,
            counts: Mutex::new(Counts::default()),
        })
    }

    fn counts(&self) -> MutexGuard<'_, Counts> {
        self.counts.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The slot of a freshly accepted TCP connection from `peer`; `None`: over a cap.
    pub fn accept(self: &Arc<Self>, peer: IpAddr) -> Option<Slot> {
        let ip = self.per_ip_at_accept.then_some(peer);
        self.reserve(true, ip)
    }

    /// With `--trust-forwarded-for`: the per-IP slot of the forwarded client address (`None`:
    /// over the cap). Without it the accept slot already covers the address.
    pub fn forwarded(self: &Arc<Self>, ip: IpAddr) -> Option<Option<Slot>> {
        if self.per_ip_at_accept {
            return Some(None);
        }
        self.reserve(false, Some(ip)).map(Some)
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

/// A connection holding its [`Slot`] for as long as the socket lives (through the HTTP exchange
/// and the WebSocket after it). Until a handler marks it upgraded, I/O fails with `TimedOut` once
/// the request deadline passes: slow or never-sent request headers and idle keep-alive both end
/// there, which hyper on its own (no timer configured by `axum::serve`) would wait out forever.
pub(crate) struct Guarded<S> {
    /// Declared first so it is dropped first: once the peer sees the socket close, the slot is
    /// already free.
    _slot: Slot,
    io: S,
    deadline: Option<Pin<Box<Sleep>>>,
    upgraded: Arc<AtomicBool>,
}

impl<S> Guarded<S> {
    pub fn new(io: S, slot: Slot, request_timeout: Duration) -> Guarded<S> {
        Guarded {
            io,
            _slot: slot,
            deadline: Some(Box::pin(tokio::time::sleep(request_timeout))),
            upgraded: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Called while the inner I/O is pending: arms the deadline's wakeup, `Err` once it passed.
    fn check_deadline(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        let Some(deadline) = &mut self.deadline else {
            return Ok(());
        };
        if self.upgraded.load(Ordering::Acquire) {
            self.deadline = None;
            return Ok(());
        }
        match deadline.as_mut().poll(cx) {
            Poll::Ready(()) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no WebSocket upgrade before the request deadline",
            )),
            Poll::Pending => Ok(()),
        }
    }

    fn guard<T>(&mut self, cx: &mut Context<'_>, r: Poll<io::Result<T>>) -> Poll<io::Result<T>> {
        if r.is_pending()
            && let Err(e) = self.check_deadline(cx)
        {
            return Poll::Ready(Err(e));
        }
        r
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Guarded<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let r = Pin::new(&mut this.io).poll_read(cx, buf);
        this.guard(cx, r)
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
        this.guard(cx, r)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let r = Pin::new(&mut this.io).poll_write_vectored(cx, bufs);
        this.guard(cx, r)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let r = Pin::new(&mut this.io).poll_flush(cx);
        this.guard(cx, r)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

/// What handlers learn about their connection, for both plain and TLS listeners.
#[derive(Clone, Debug)]
pub(crate) struct PeerAddr {
    pub addr: SocketAddr,
    upgraded: Arc<AtomicBool>,
}

impl PeerAddr {
    /// The connection is becoming a WebSocket: lift the request deadline (the hello timeout and
    /// idle timeout take over).
    pub fn mark_upgraded(&self) {
        self.upgraded.store(true, Ordering::Release);
    }

    fn of<S>(io: &Guarded<S>, addr: SocketAddr) -> PeerAddr {
        PeerAddr {
            addr,
            upgraded: io.upgraded.clone(),
        }
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
    request_timeout: Duration,
}

impl Plain {
    pub fn new(listener: TcpListener, limits: Arc<Limits>, request_timeout: Duration) -> Plain {
        Plain {
            listener,
            limits,
            request_timeout,
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
            return (Guarded::new(stream, slot, self.request_timeout), addr);
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
        addr.ip()
    );
}
