//! Relay limits and timeouts. Every value is a field so tests (and `serve` flags) can shrink them.

use std::net::IpAddr;
use std::path::PathBuf;
use std::str::FromStr;
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
    /// Key the per-IP limits on the last `X-Forwarded-For` hop, for connections whose socket peer
    /// is loopback or in `trusted_proxies` (others are keyed on the socket peer).
    pub trust_forwarded_for: bool,
    /// Proxies besides loopback whose `X-Forwarded-For` is honoured (`--trusted-proxy`).
    pub trusted_proxies: Vec<ProxyRange>,
    /// The first message must arrive within this time.
    pub hello_timeout: Duration,
    pub ping_interval: Duration,
    /// A connection with no inbound traffic (pongs included) for this long is closed.
    pub idle_timeout: Duration,
    /// `rooms.json` is re-read this often, and on a gateway hello for a room it does not list (at
    /// most once per second).
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
    /// Device admissions per room (each queues an `Open`, and later a `Closed`, on the gateway):
    /// over this rate, device hellos get `RATE_LIMITED` before anything is queued.
    pub device_admissions: AdmissionRate,
    /// Device admissions per admission token, checked before the room's: one token holder
    /// churning connections cannot spend the room's admissions other devices need.
    pub token_admissions: AdmissionRate,
    /// Concurrent device connections per admission token (overlap while a device reconnects).
    pub max_connections_per_token: usize,
    /// Per client address (IPv6: per /64): the socket peer, checked at accept; for a trusted
    /// proxy's connections the forwarded address, checked at the WebSocket upgrade.
    pub max_connections_per_ip: usize,
    /// Open TCP connections in total, checked at accept: WebSockets and sockets still in the TLS
    /// handshake or HTTP request alike.
    pub max_connections: usize,
    pub max_device_message: usize,
    pub max_gateway_message: usize,
    pub device_rate: RateLimit,
    pub gateway_rate: RateLimit,
    /// Outbound queue bound per connection: whichever of the two fills first closes the
    /// connection with `SLOW_CONSUMER`. Device data toward a gateway is refused (closing the
    /// sending device with `SLOW_CONSUMER`) above [`GATEWAY_DATA_SHARE`] of either bound in total,
    /// or above [`TOKEN_DATA_SHARE`] for one admission token's queued data, so the gateway's queue
    /// keeps room for `Open`/`Closed` and for the other devices.
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
            trusted_proxies: Vec::new(),
            hello_timeout: Duration::from_secs(10),
            ping_interval: Duration::from_secs(25),
            idle_timeout: Duration::from_secs(75),
            rooms_reload_interval: Duration::from_secs(30),
            close_timeout: Duration::from_secs(2),
            shutdown_timeout: Duration::from_secs(5),
            tls_handshake_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(10),
            max_devices_per_room: 16,
            device_admissions: AdmissionRate::DEVICE,
            token_admissions: AdmissionRate::TOKEN,
            max_connections_per_token: 2,
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

/// The share (numerator, denominator) of a gateway's queue bounds that device data may fill.
pub(crate) const GATEWAY_DATA_SHARE: (usize, usize) = (3, 4);
/// The share of a gateway's queue bounds that one admission token's queued data may fill (its
/// at most two connections together), so a token holder cannot crowd out the other devices.
pub(crate) const TOKEN_DATA_SHARE: (usize, usize) = (1, 4);

/// A token bucket: `per_sec` events per second, up to `burst` at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmissionRate {
    pub per_sec: u64,
    pub burst: u64,
}

impl AdmissionRate {
    /// Default device admissions per room.
    pub const DEVICE: AdmissionRate = AdmissionRate {
        per_sec: 10,
        burst: 20,
    };
    /// Default admissions per device token (below the room's, so one token cannot drain it).
    pub const TOKEN: AdmissionRate = AdmissionRate {
        per_sec: 2,
        burst: 4,
    };

    /// As a [`RateLimit`] that counts events and ignores sizes.
    pub(crate) fn as_rate_limit(self) -> RateLimit {
        RateLimit {
            bytes_per_sec: 0,
            bytes_burst: 0,
            messages_per_sec: self.per_sec,
            messages_burst: self.burst,
        }
    }
}

impl Config {
    /// Whether `X-Forwarded-For` is honoured on a connection from `peer`.
    pub(crate) fn trusts_proxy(&self, peer: IpAddr) -> bool {
        self.trust_forwarded_for && is_proxy(&self.trusted_proxies, peer)
    }
}

/// `peer` is loopback or in `ranges`.
pub(crate) fn is_proxy(ranges: &[ProxyRange], peer: IpAddr) -> bool {
    let peer = canonical(peer);
    peer.is_loopback() || ranges.iter().any(|r| r.contains(peer))
}

/// `ip`, with an IPv4-mapped IPv6 address (`::ffff:a.b.c.d`, as a dual-stack listener reports IPv4
/// clients) turned into its IPv4 address.
pub(crate) fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

/// An address or CIDR range (`10.88.0.1`, `10.88.0.0/16`, `fd00::/8`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProxyRange {
    net: IpAddr,
    prefix: u8,
}

impl ProxyRange {
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.net, canonical(ip)) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

impl FromStr for ProxyRange {
    type Err = String;

    fn from_str(s: &str) -> Result<ProxyRange, String> {
        let bad = || format!("{s:?}: expected an IP address or CIDR range");
        let (addr, prefix) = match s.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (s, None),
        };
        let net = canonical(addr.parse::<IpAddr>().map_err(|_| bad())?);
        let max = if net.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => max,
            Some(p) => p.parse::<u8>().ok().filter(|p| *p <= max).ok_or_else(bad)?,
        };
        Ok(ProxyRange { net, prefix })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn proxy_ranges_parse_and_match() {
        let r: ProxyRange = "10.88.0.0/16".parse().unwrap();
        assert!(r.contains(ip("10.88.3.4")));
        assert!(r.contains(ip("::ffff:10.88.0.1")));
        assert!(!r.contains(ip("10.89.0.1")));
        assert!(!r.contains(ip("::1")));
        let one: ProxyRange = "10.88.0.1".parse().unwrap();
        assert!(one.contains(ip("10.88.0.1")) && !one.contains(ip("10.88.0.2")));
        let v6: ProxyRange = "fd00::/8".parse().unwrap();
        assert!(v6.contains(ip("fd12::1")) && !v6.contains(ip("fe80::1")));
        let all: ProxyRange = "0.0.0.0/0".parse().unwrap();
        assert!(all.contains(ip("203.0.113.9")));
        for bad in [
            "",
            "10.88.0.0/33",
            "fd00::/129",
            "host",
            "10.0.0.0/x",
            "10.0.0.0/",
        ] {
            assert!(bad.parse::<ProxyRange>().is_err(), "{bad}");
        }
    }

    #[test]
    fn forwarded_for_is_trusted_only_from_loopback_or_listed_proxies() {
        let mut c = Config::new("x");
        assert!(!c.trusts_proxy(ip("127.0.0.1")), "flag off");
        c.trust_forwarded_for = true;
        assert!(c.trusts_proxy(ip("127.0.0.1")));
        assert!(c.trusts_proxy(ip("::1")));
        assert!(c.trusts_proxy(ip("::ffff:127.0.0.1")));
        assert!(!c.trusts_proxy(ip("10.88.0.1")));
        c.trusted_proxies = vec!["10.88.0.1".parse().unwrap()];
        assert!(c.trusts_proxy(ip("10.88.0.1")));
        assert!(c.trusts_proxy(ip("::ffff:10.88.0.1")));
        assert!(!c.trusts_proxy(ip("203.0.113.9")));
    }
}
