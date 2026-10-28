//! Relay limits and timeouts. Every value is a field so tests (and `serve` flags) can shrink them.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use deck_remote::relay::{HASH_LEN, MAX_GATEWAY_MESSAGE, MAX_PAYLOAD, MAX_TOKENS};

const MIB: u64 = 1024 * 1024;

/// A per-connection inbound token bucket pair: bytes and messages per second, each with a burst.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateLimit {
    pub bytes_per_sec: u64,
    pub bytes_burst: u64,
    pub messages_per_sec: u64,
    pub messages_burst: u64,
}

impl RateLimit {
    /// Device connections: 1 MiB/s (burst 4 MiB), 500 messages/s (burst 1000).
    pub const DEVICE: RateLimit = RateLimit {
        bytes_per_sec: MIB,
        bytes_burst: 4 * MIB,
        messages_per_sec: 500,
        messages_burst: 1000,
    };
    /// Gateway connections: 8 MiB/s (burst 16 MiB), 4000 messages/s (burst 8000).
    pub const GATEWAY: RateLimit = RateLimit {
        bytes_per_sec: 8 * MIB,
        bytes_burst: 16 * MIB,
        messages_per_sec: 4000,
        messages_burst: 8000,
    };
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Holds `rooms.json`.
    pub state_dir: PathBuf,
    /// In-process TLS ([`crate::load_tls`]); `None` serves plain HTTP (behind a TLS proxy).
    pub tls: Option<Arc<rustls::ServerConfig>>,
    /// Key the per-IP limits on the last `X-Forwarded-For` hop (only behind a trusted proxy).
    pub trust_forwarded_for: bool,
    /// The first message must arrive within this time.
    pub hello_timeout: Duration,
    pub ping_interval: Duration,
    /// A connection with no inbound traffic (pongs included) for this long is closed.
    pub idle_timeout: Duration,
    /// `rooms.json` is re-read this often (and on every gateway hello).
    pub rooms_reload_interval: Duration,
    /// Time a closing connection gets to flush its queue and close frame and to answer it.
    pub close_timeout: Duration,
    /// `serve` returns at most this long after the shutdown future resolves.
    pub shutdown_timeout: Duration,
    /// A TLS handshake must finish within this time.
    pub tls_handshake_timeout: Duration,
    /// A connection must have upgraded to a WebSocket this long after accept (after the TLS
    /// handshake with in-process TLS); otherwise it is closed. Bounds slow or never-sent request
    /// headers and idle HTTP keep-alive (health checks).
    pub request_timeout: Duration,
    pub max_devices_per_room: usize,
    /// Per client address: the socket peer, checked at accept; with `trust_forwarded_for` the
    /// forwarded address, checked at the WebSocket upgrade.
    pub max_connections_per_ip: usize,
    /// Open TCP connections in total, checked at accept: WebSockets and sockets still in the TLS
    /// handshake or HTTP request alike.
    pub max_connections: usize,
    pub max_device_message: usize,
    pub max_gateway_message: usize,
    pub device_rate: RateLimit,
    pub gateway_rate: RateLimit,
    /// Outbound queue bound per connection: whichever of the two fills first closes the
    /// connection with `SLOW_CONSUMER`.
    pub queue_messages: usize,
    pub queue_bytes: usize,
}

impl Config {
    /// The production defaults (`docs/relay.md`).
    pub fn new(state_dir: impl Into<PathBuf>) -> Config {
        Config {
            state_dir: state_dir.into(),
            tls: None,
            trust_forwarded_for: false,
            hello_timeout: Duration::from_secs(10),
            ping_interval: Duration::from_secs(25),
            idle_timeout: Duration::from_secs(75),
            rooms_reload_interval: Duration::from_secs(30),
            close_timeout: Duration::from_secs(2),
            shutdown_timeout: Duration::from_secs(5),
            tls_handshake_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(10),
            max_devices_per_room: 16,
            max_connections_per_ip: 32,
            max_connections: 1024,
            max_device_message: MAX_PAYLOAD,
            max_gateway_message: MAX_GATEWAY_MESSAGE.max(3 + MAX_TOKENS * HASH_LEN),
            device_rate: RateLimit::DEVICE,
            gateway_rate: RateLimit::GATEWAY,
            queue_messages: 256,
            queue_bytes: 4 * MIB as usize,
        }
    }
}
