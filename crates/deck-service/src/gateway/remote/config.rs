//! The relay settings (`relay.json`: `{url, room_key}`, owner-only): device-local, never in the
//! synced app settings. The URL is a base (`wss://host[:port]`); plain `ws://` is accepted only
//! for a loopback host, because a cleartext hello exposes the room secret and the tokens.

use std::io;
use std::net::{IpAddr, Ipv6Addr};
use std::path::Path;

use deck_remote::noise::MAX_INVITE_URL;
use deck_remote::relay::RoomKey;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::RemoteError;

pub const FILE: &str = "relay.json";

/// A validated, normalized relay base URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayUrl {
    /// `wss://host[:port]` or `ws://host[:port]`, lowercase, default ports omitted.
    url: String,
    tls: bool,
    /// As written in the URL (IPv6 in brackets).
    host: String,
    port: u16,
}

impl RelayUrl {
    pub fn parse(text: &str) -> Result<RelayUrl, RemoteError> {
        let invalid = |why: &str| RemoteError::Invalid(format!("relay URL: {why}"));
        let text = text.trim();
        let lower = text.to_ascii_lowercase();
        let (tls, rest) = if let Some(r) = lower.strip_prefix("wss://") {
            (true, r)
        } else if let Some(r) = lower.strip_prefix("ws://") {
            (false, r)
        } else {
            return Err(invalid(
                "it must start with wss:// (or ws:// for this computer)",
            ));
        };
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        if !(tail.is_empty() || tail == "/") {
            return Err(invalid(
                "give only the scheme, host and port (no path or query)",
            ));
        }
        if authority.contains('@') {
            return Err(invalid("user names or passwords are not supported"));
        }
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (inner, after) = v6.split_once(']').ok_or_else(|| invalid("unclosed ["))?;
            inner
                .parse::<Ipv6Addr>()
                .map_err(|_| invalid("not an IPv6 address"))?;
            let port = match after {
                "" => None,
                p => Some(p.strip_prefix(':').ok_or_else(|| invalid("bad port"))?),
            };
            (&authority[..inner.len() + 2], port)
        } else {
            match authority.split_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            }
        };
        if host.is_empty() {
            return Err(invalid("there is no host"));
        }
        if !host.starts_with('[')
            && !host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        {
            return Err(invalid("the host name has invalid characters"));
        }
        let default_port = if tls { 443 } else { 80 };
        let port = match port {
            None => default_port,
            Some(p) => p
                .parse::<u16>()
                .ok()
                .filter(|&p| p != 0)
                .ok_or_else(|| invalid("bad port"))?,
        };
        if !tls && !is_loopback(host) {
            return Err(invalid(
                "plain ws:// is only allowed for this computer (localhost); use wss://",
            ));
        }
        let scheme = if tls { "wss" } else { "ws" };
        let url = if port == default_port {
            format!("{scheme}://{host}")
        } else {
            format!("{scheme}://{host}:{port}")
        };
        if url.len() > MAX_INVITE_URL {
            return Err(invalid("it is too long"));
        }
        Ok(RelayUrl {
            url,
            tls,
            host: host.to_owned(),
            port,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.url
    }

    pub fn tls(&self) -> bool {
        self.tls
    }

    /// The host to resolve (IPv6 without brackets).
    pub fn host(&self) -> &str {
        self.host.trim_start_matches('[').trim_end_matches(']')
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `url` + `path` (`/v1/gateway`).
    pub fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.url)
    }

    /// The PWA's origin: the relay's own (`wss://` → `https://`, `ws://` → `http://`).
    pub fn pwa_origin(&self) -> String {
        match self.url.strip_prefix("wss://") {
            Some(rest) => format!("https://{rest}"),
            None => format!("http://{}", self.url.trim_start_matches("ws://")),
        }
    }
}

/// `localhost`, `127.0.0.0/8` or `::1`.
fn is_loopback(host: &str) -> bool {
    if host == "localhost" {
        return true;
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => v4.is_loopback(),
        Ok(IpAddr::V6(v6)) => v6 == Ipv6Addr::LOCALHOST,
        Err(_) => false,
    }
}

/// The configured relay.
#[derive(Clone, PartialEq, Eq)]
pub struct RelayConfig {
    pub url: RelayUrl,
    pub room_key: RoomKey,
}

impl std::fmt::Debug for RelayConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayConfig")
            .field("url", &self.url.as_str())
            .field("room_key", &self.room_key)
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
struct Stored {
    url: String,
    room_key: String,
}

impl Drop for Stored {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.room_key);
    }
}

impl RelayConfig {
    /// Validates what the user entered.
    pub fn new(url: &str, room_key: &str) -> Result<RelayConfig, RemoteError> {
        let url = RelayUrl::parse(url)?;
        let room_key = RoomKey::parse(room_key).map_err(|_| {
            RemoteError::Invalid(
                "room key: paste the whole key printed by `omp-deck-relay enroll` \
                 (ompdeck-relay-v1:…)"
                    .to_owned(),
            )
        })?;
        Ok(RelayConfig { url, room_key })
    }

    /// The stored relay, if one is configured.
    pub fn load(path: &Path) -> Result<Option<RelayConfig>, RemoteError> {
        let bytes = match std::fs::read(path) {
            Ok(b) => Zeroizing::new(b),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(RemoteError::io(path, e)),
        };
        let stored: Stored = serde_json::from_slice(&bytes)
            .map_err(|_| RemoteError::corrupt(path, "not a relay configuration"))?;
        RelayConfig::new(&stored.url, &stored.room_key)
            .map(Some)
            .map_err(|e| RemoteError::corrupt(path, e.to_string()))
    }

    pub fn save(&self, path: &Path) -> Result<(), RemoteError> {
        let stored = Stored {
            url: self.url.as_str().to_owned(),
            room_key: self.room_key.encode(),
        };
        let bytes = Zeroizing::new(
            serde_json::to_vec_pretty(&stored)
                .map_err(|e| RemoteError::Invalid(format!("relay configuration: {e}")))?,
        );
        crate::gateway::write_private(path, &bytes).map_err(|e| RemoteError::io(path, e))
    }

    pub fn clear(path: &Path) -> Result<(), RemoteError> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(RemoteError::io(path, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> String {
        RelayUrl::parse(text).unwrap().as_str().to_owned()
    }

    fn err(text: &str) -> String {
        RelayUrl::parse(text).unwrap_err().to_string()
    }

    #[test]
    fn urls_are_normalized() {
        assert_eq!(ok("wss://Relay.Example.com"), "wss://relay.example.com");
        assert_eq!(
            ok(" wss://relay.example.com:443/ "),
            "wss://relay.example.com"
        );
        assert_eq!(ok("WSS://h.example:8443"), "wss://h.example:8443");
        assert_eq!(ok("ws://127.0.0.1:9000"), "ws://127.0.0.1:9000");
        assert_eq!(ok("ws://127.5.6.7:80"), "ws://127.5.6.7");
        assert_eq!(ok("ws://localhost:1"), "ws://localhost:1");
        assert_eq!(ok("ws://[::1]:9000"), "ws://[::1]:9000");
        assert_eq!(ok("wss://[2001:db8::1]"), "wss://[2001:db8::1]");
        let u = RelayUrl::parse("ws://[::1]:9000").unwrap();
        assert_eq!((u.host(), u.port(), u.tls()), ("::1", 9000, false));
        assert_eq!(u.endpoint("/v1/gateway"), "ws://[::1]:9000/v1/gateway");
    }

    #[test]
    fn plain_ws_only_for_loopback() {
        assert!(err("ws://relay.example.com").contains("only allowed for this computer"));
        assert!(err("ws://10.0.0.1:9000").contains("only allowed"));
        assert!(err("ws://[2001:db8::1]").contains("only allowed"));
        assert!(err("ws://0.0.0.0:9000").contains("only allowed"));
        assert!(err("ws://localhost.evil.com").contains("only allowed"));
    }

    #[test]
    fn bad_urls_are_refused() {
        assert!(err("https://relay.example.com").contains("must start with"));
        assert!(err("relay.example.com").contains("must start with"));
        assert!(err("wss://").contains("no host"));
        assert!(err("wss://relay.example.com/sub").contains("no path"));
        assert!(err("wss://relay.example.com?x=1").contains("no path"));
        assert!(err("wss://user:pw@relay.example.com").contains("user names"));
        assert!(err("wss://relay.example.com:0").contains("bad port"));
        assert!(err("wss://relay.example.com:99999").contains("bad port"));
        assert!(err("wss://rel ay.example.com").contains("invalid characters"));
        assert!(err("wss://[::1").contains("unclosed"));
        assert!(err(&format!("wss://{}.com", "a".repeat(260))).contains("too long"));
    }

    #[test]
    fn pwa_origin_is_the_relay_origin() {
        let u = RelayUrl::parse("wss://relay.example.com:8443").unwrap();
        assert_eq!(u.pwa_origin(), "https://relay.example.com:8443");
        let u = RelayUrl::parse("ws://127.0.0.1:9000").unwrap();
        assert_eq!(u.pwa_origin(), "http://127.0.0.1:9000");
    }

    #[test]
    fn config_round_trips_owner_only_and_debug_hides_the_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        assert_eq!(RelayConfig::load(&path).unwrap(), None);
        let key = RoomKey {
            room: [7; 16],
            secret: [9; 32],
        };
        let cfg = RelayConfig::new("wss://relay.example.com", &key.encode()).unwrap();
        cfg.save(&path).unwrap();
        assert_eq!(RelayConfig::load(&path).unwrap(), Some(cfg.clone()));
        let debug = format!("{cfg:?}");
        let secret_b64 = key.encode();
        assert!(!debug.contains(&secret_b64[secret_b64.len() - 20..]));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        RelayConfig::clear(&path).unwrap();
        RelayConfig::clear(&path).unwrap();
        assert_eq!(RelayConfig::load(&path).unwrap(), None);
    }

    #[test]
    fn bad_room_keys_and_corrupt_files_are_reported() {
        assert!(
            RelayConfig::new("wss://r.example", "nope")
                .unwrap_err()
                .to_string()
                .contains("room key")
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, b"{").unwrap();
        assert!(matches!(
            RelayConfig::load(&path),
            Err(RemoteError::Corrupt { .. })
        ));
    }
}
