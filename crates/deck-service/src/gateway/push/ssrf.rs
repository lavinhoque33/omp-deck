//! The push endpoint is chosen by a paired device, so the gateway's HTTP client must never be
//! aimed at this computer or its networks through it: only public unicast addresses are
//! connected to. The check runs at subscribe time (the host must resolve to at least one) and
//! again inside the HTTP client's resolver at every send, so a host re-pointed later (DNS
//! rebinding) is skipped too.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs as _};
use std::sync::mpsc;
use std::time::Duration;

use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::NextTimeout;

use super::{Policy, PushError};

/// How long a subscribe-time lookup may take.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether `ip` may be connected to: public unicast only, or loopback too under the test
/// override.
pub fn allowed(ip: IpAddr, policy: Policy) -> bool {
    if policy.allow_loopback && is_loopback(ip) {
        return true;
    }
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => public_v6(v6),
    }
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0 // "this network", unspecified
        || a == 10
        || a == 127
        || (a == 100 && (64..128).contains(&b)) // CGNAT 100.64/10
        || (a == 169 && b == 254) // link-local
        || (a == 172 && (16..32).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 192 && b == 0 && c == 2) // TEST-NET-1
        || (a == 192 && b == 88 && c == 99) // 6to4 relay anycast
        || (a == 198 && (b == 18 || b == 19)) // benchmarking
        || (a == 198 && b == 51 && c == 100) // TEST-NET-2
        || (a == 203 && b == 0 && c == 113) // TEST-NET-3
        || a >= 224) // multicast, reserved, broadcast
}

fn public_v6(ip: Ipv6Addr) -> bool {
    // IPv4 carried in IPv6: judged as that IPv4 address (mapped, compatible, NAT64, 6to4).
    if let Some(v4) = ip.to_ipv4_mapped() {
        return public_v4(v4);
    }
    let s = ip.segments();
    let embedded = |hi: u16, lo: u16| Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo));
    if s[..6] == [0; 6] {
        // `::`, `::1` and the deprecated IPv4-compatible `::a.b.c.d`.
        return !ip.is_unspecified() && !ip.is_loopback() && public_v4(embedded(s[6], s[7]));
    }
    if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] || s[..3] == [0x64, 0xff9b, 1] {
        return false; // NAT64 64:ff9b::/96 and 64:ff9b:1::/48 reach internal hosts via a gateway
    }
    if s[..4] == [0; 4] && s[4] == 0xffff && s[5] == 0 {
        return false; // IPv4-translated ::ffff:0:0:0/96
    }
    if s[0] == 0x2002 {
        return public_v4(embedded(s[1], s[2]));
    }
    !((s[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
        || (s[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        || (s[0] & 0xffc0) == 0xfec0 // site-local (deprecated) fec0::/10
        || (s[0] & 0xff00) == 0xff00 // multicast
        || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation 2001:db8::/32
        || (s[0] == 0x2001 && s[1] == 0) // Teredo 2001::/32
        || (s[0] == 0x0100 && s[1..4] == [0, 0, 0])) // discard-only 100::/64
}

/// The endpoint's host and port as `to_socket_addrs` takes them (IPv6 without brackets).
fn host_port(uri: &Uri) -> Option<(String, u16)> {
    let host = uri.host()?.trim_start_matches('[').trim_end_matches(']');
    let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
        Some("http") => 80,
        _ => 443,
    });
    Some((host.to_owned(), port))
}

/// Subscribe time: the endpoint's host must resolve to at least one allowed address. Blocks for
/// up to [`LOOKUP_TIMEOUT`]; never call it on the engine thread.
pub fn check_host(uri: &Uri, policy: Policy) -> Result<(), PushError> {
    let (host, port) =
        host_port(uri).ok_or_else(|| PushError::Invalid("the push endpoint has no host".into()))?;
    let addrs = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => lookup(host, port)?,
    };
    if addrs.iter().any(|a| allowed(a.ip(), policy)) {
        Ok(())
    } else {
        Err(PushError::Blocked)
    }
}

fn lookup(host: String, port: u16) -> Result<Vec<SocketAddr>, PushError> {
    let unresolved = || PushError::Invalid("the push endpoint's host cannot be resolved".into());
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("gateway-push-lookup".to_owned())
        .spawn(move || {
            let _ = tx.send(
                (host.as_str(), port)
                    .to_socket_addrs()
                    .map(Iterator::collect),
            );
        })
        .map_err(|_| unresolved())?;
    match rx.recv_timeout(LOOKUP_TIMEOUT) {
        Ok(Ok(addrs)) => Ok(addrs),
        _ => Err(unresolved()),
    }
}

/// Every resolved address was refused.
#[derive(Debug, thiserror::Error)]
#[error("the push endpoint resolves to no public internet address")]
pub struct Refused;

/// The send-time half: ureq's own resolver with every disallowed address dropped.
#[derive(Debug)]
pub struct PublicResolver {
    pub policy: Policy,
}

impl Resolver for PublicResolver {
    fn resolve(
        &self,
        uri: &Uri,
        config: &Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let found = DefaultResolver::default().resolve(uri, config, timeout)?;
        let mut kept = self.empty();
        for addr in found.iter().filter(|a| allowed(a.ip(), self.policy)) {
            kept.push(*addr);
        }
        if kept.is_empty() {
            return Err(ureq::Error::Other(Box::new(Refused)));
        }
        Ok(kept)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STRICT: Policy = Policy {
        allow_loopback: false,
    };
    const LOOPBACK: Policy = Policy {
        allow_loopback: true,
    };

    fn ok(ip: &str) -> bool {
        allowed(ip.parse().unwrap(), STRICT)
    }

    #[test]
    fn only_public_unicast_addresses_are_allowed() {
        for blocked in [
            "0.0.0.0",
            "127.0.0.1",
            "127.8.9.10",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "192.0.0.8",
            "192.0.2.1",
            "198.18.0.1",
            "198.51.100.7",
            "203.0.113.9",
            "224.0.0.1",
            "239.255.255.250",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "64:ff9b::a00:1",
            "64:ff9b::808:808",
            "64:ff9b:1::1",
            "::ffff:0:0:1",
            "2001::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
            "192.88.99.1",
            "2002:7f00:1::",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
            "100::1",
        ] {
            assert!(!ok(blocked), "{blocked} must be refused");
        }
        for public in [
            "1.1.1.1",
            "8.8.8.8",
            "100.63.255.255",
            "100.128.0.1",
            "172.15.0.1",
            "172.32.0.1",
            "142.250.74.110",
            "2a00:1450:4001:80b::200a",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
            "2002:808:808::",
        ] {
            assert!(ok(public), "{public} must be allowed");
        }
    }

    #[test]
    fn the_test_override_admits_loopback_only() {
        for ip in ["127.0.0.1", "::1", "::ffff:127.0.0.1"] {
            assert!(allowed(ip.parse().unwrap(), LOOPBACK), "{ip}");
        }
        for ip in ["10.0.0.1", "192.168.0.1", "fe80::1", "100.64.0.1"] {
            assert!(!allowed(ip.parse().unwrap(), LOOPBACK), "{ip}");
        }
    }

    #[test]
    fn subscribe_time_checks_refuse_literal_private_hosts() {
        for endpoint in [
            "https://127.0.0.1/push",
            "https://10.0.0.5:8443/push",
            "https://[::1]/push",
            "https://[::ffff:127.0.0.1]/push",
            "https://100.64.1.2/push",
            "https://localhost/push",
        ] {
            let uri: Uri = endpoint.parse().unwrap();
            assert!(
                matches!(check_host(&uri, STRICT), Err(PushError::Blocked)),
                "{endpoint}"
            );
        }
        let uri: Uri = "https://[2606:4700:4700::1111]/push".parse().unwrap();
        assert!(check_host(&uri, STRICT).is_ok());
    }

    #[test]
    fn the_send_time_resolver_refuses_private_hosts() {
        let resolver = PublicResolver { policy: STRICT };
        let config = Config::default();
        for endpoint in [
            "https://127.0.0.1/p",
            "https://[::1]/p",
            "https://10.9.8.7/p",
        ] {
            let uri: Uri = endpoint.parse().unwrap();
            let err = resolver
                .resolve(&uri, &config, NextTimeout::default())
                .unwrap_err();
            assert!(
                matches!(&err, ureq::Error::Other(e) if e.is::<Refused>()),
                "{endpoint}: {err}"
            );
        }
        let uri: Uri = "https://1.1.1.1/p".parse().unwrap();
        assert_eq!(
            resolver
                .resolve(&uri, &config, NextTimeout::default())
                .unwrap()
                .len(),
            1
        );
    }
}
