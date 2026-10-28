//! Wire format between `omp-deck-relay` and its clients (relay protocol v1).
//!
//! Every WebSocket message is binary. A client's first message is a [`Hello`]; the relay answers
//! [`HELLO_OK`] or closes with one of the [`close`] codes. Afterwards:
//!
//! - a **device** sends and receives raw payloads (one opaque Noise message each, at most
//!   [`MAX_PAYLOAD`] bytes);
//! - the **gateway** sends [`ToRelay`] and receives [`FromRelay`] messages, which carry the
//!   relay-assigned [`PeerId`] of the device connection next to the payload.
//!
//! The relay never sees anything but routing data: the room, peer ids, SHA-256 hashes of the
//! admission tokens, sizes and timing.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// Relay protocol version carried in every [`Hello`].
pub const VERSION: u8 = 1;
pub const ROOM_LEN: usize = 16;
pub const SECRET_LEN: usize = 32;
pub const TOKEN_LEN: usize = 32;
pub const HASH_LEN: usize = 32;
/// Largest device payload: one Noise message (the Noise spec caps messages at 65 535 bytes).
pub const MAX_PAYLOAD: usize = 65_535;
/// Largest gateway message: type byte + peer id + payload, or a full token set.
pub const MAX_GATEWAY_MESSAGE: usize = 1 + 4 + MAX_PAYLOAD;
/// Most admission tokens one gateway may register (paired devices + open pairing windows).
pub const MAX_TOKENS: usize = 1024;

pub const GATEWAY_PATH: &str = "/v1/gateway";
pub const DEVICE_PATH: &str = "/v1/device";
pub const HEALTH_PATH: &str = "/healthz";

/// The relay's answer to an accepted [`Hello`].
pub const HELLO_OK: [u8; 1] = [0];

/// Prefix of a [`RoomKey`] string.
pub const ROOM_KEY_PREFIX: &str = "ompdeck-relay-v1:";

/// WebSocket close codes the relay (and the gateway, for [`ToRelay::Close`]) uses.
pub mod close {
    /// Normal close.
    pub const NORMAL: u16 = 1000;
    /// The relay is shutting down.
    pub const GOING_AWAY: u16 = 1001;
    /// A text message or an undecodable message.
    pub const PROTOCOL: u16 = 1002;
    /// The first message is not a valid [`super::Hello`] or did not arrive in time.
    pub const BAD_HELLO: u16 = 4001;
    /// Unknown room, wrong room secret, unknown or revoked device token.
    pub const UNAUTHORIZED: u16 = 4003;
    /// A device connected while its room's gateway is not connected.
    pub const GATEWAY_OFFLINE: u16 = 4004;
    /// The peer did not read fast enough; its queue filled up.
    pub const SLOW_CONSUMER: u16 = 4008;
    /// A newer gateway connection for the same room took over.
    pub const REPLACED: u16 = 4009;
    /// A message exceeded its size limit.
    pub const TOO_LARGE: u16 = 4013;
    /// Too many connections, devices or bytes/messages per second.
    pub const RATE_LIMITED: u16 = 4029;
}

pub type Room = [u8; ROOM_LEN];
pub type Secret = [u8; SECRET_LEN];
pub type Token = [u8; TOKEN_LEN];
pub type Hash = [u8; HASH_LEN];
/// A device connection, assigned by the relay; unique per room while the relay runs.
pub type PeerId = u32;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WireError {
    #[error("empty relay message")]
    Empty,
    #[error("unknown relay message type {0:#04x}")]
    UnknownType(u8),
    #[error("relay protocol version {0} is not supported (this build speaks {VERSION})")]
    Version(u8),
    #[error("relay message of {0} bytes is malformed")]
    Malformed(usize),
    #[error("relay payload of {0} bytes exceeds {MAX_PAYLOAD}")]
    TooLarge(usize),
    #[error("{0} admission tokens exceed {MAX_TOKENS}")]
    TooManyTokens(usize),
    #[error("not a relay room key")]
    RoomKey,
}

/// SHA-256 of a device admission token or room secret: what the relay stores and compares.
pub fn hash(secret: &[u8; 32]) -> Hash {
    Sha256::digest(secret).into()
}

/// A client's first message.
#[derive(Clone, PartialEq, Eq)]
pub enum Hello {
    Gateway { room: Room, secret: Secret },
    Device { room: Room, token: Token },
}

const HELLO_GATEWAY: u8 = b'G';
const HELLO_DEVICE: u8 = b'D';
const HELLO_LEN: usize = 2 + ROOM_LEN + 32;

impl Hello {
    pub fn encode(&self) -> Vec<u8> {
        let (tag, room, key) = match self {
            Hello::Gateway { room, secret } => (HELLO_GATEWAY, room, secret),
            Hello::Device { room, token } => (HELLO_DEVICE, room, token),
        };
        let mut out = Vec::with_capacity(HELLO_LEN);
        out.push(tag);
        out.push(VERSION);
        out.extend_from_slice(room);
        out.extend_from_slice(key);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Hello, WireError> {
        let (&tag, rest) = bytes.split_first().ok_or(WireError::Empty)?;
        if tag != HELLO_GATEWAY && tag != HELLO_DEVICE {
            return Err(WireError::UnknownType(tag));
        }
        let (&version, rest) = rest
            .split_first()
            .ok_or(WireError::Malformed(bytes.len()))?;
        if version != VERSION {
            return Err(WireError::Version(version));
        }
        if rest.len() != ROOM_LEN + 32 {
            return Err(WireError::Malformed(bytes.len()));
        }
        let (room, key) = rest.split_at(ROOM_LEN);
        let room: Room = room
            .try_into()
            .map_err(|_| WireError::Malformed(bytes.len()))?;
        let key: [u8; 32] = key
            .try_into()
            .map_err(|_| WireError::Malformed(bytes.len()))?;
        Ok(if tag == HELLO_GATEWAY {
            Hello::Gateway { room, secret: key }
        } else {
            Hello::Device { room, token: key }
        })
    }

    pub fn room(&self) -> &Room {
        match self {
            Hello::Gateway { room, .. } | Hello::Device { room, .. } => room,
        }
    }
}

// Secrets never reach logs, even through `{:?}`.
impl std::fmt::Debug for Hello {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            Hello::Gateway { .. } => "Gateway",
            Hello::Device { .. } => "Device",
        };
        write!(f, "Hello::{kind} {{ room: {}, .. }}", room_id(self.room()))
    }
}

/// Printable room id (not a secret): what relay logs and the CLI show.
pub fn room_id(room: &Room) -> String {
    URL_SAFE_NO_PAD.encode(room)
}

const TO_DATA: u8 = 0x01;
const TO_TOKENS: u8 = 0x02;
const TO_CLOSE: u8 = 0x03;
const FROM_DATA: u8 = 0x81;
const FROM_OPEN: u8 = 0x82;
const FROM_CLOSED: u8 = 0x83;

/// Gateway → relay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToRelay<'a> {
    /// Forward `payload` to the device connection `peer`.
    Data { peer: PeerId, payload: &'a [u8] },
    /// Replace the set of admitted device tokens (hashes). Connected devices whose token hash is no
    /// longer in the set are closed with [`close::UNAUTHORIZED`].
    Tokens(Vec<Hash>),
    /// Close the device connection `peer` with `code`.
    Close { peer: PeerId, code: u16 },
}

impl ToRelay<'_> {
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        Ok(match self {
            ToRelay::Data { peer, payload } => data(TO_DATA, *peer, payload)?,
            ToRelay::Tokens(hashes) => {
                if hashes.len() > MAX_TOKENS {
                    return Err(WireError::TooManyTokens(hashes.len()));
                }
                let mut out = Vec::with_capacity(3 + hashes.len() * HASH_LEN);
                out.push(TO_TOKENS);
                // Bounded by MAX_TOKENS (1024) above, so it fits in u16.
                out.extend_from_slice(&(hashes.len() as u16).to_le_bytes());
                for h in hashes {
                    out.extend_from_slice(h);
                }
                out
            }
            ToRelay::Close { peer, code } => {
                let mut out = Vec::with_capacity(7);
                out.push(TO_CLOSE);
                out.extend_from_slice(&peer.to_le_bytes());
                out.extend_from_slice(&code.to_le_bytes());
                out
            }
        })
    }

    pub fn decode(bytes: &[u8]) -> Result<ToRelay<'_>, WireError> {
        let (&kind, rest) = bytes.split_first().ok_or(WireError::Empty)?;
        match kind {
            TO_DATA => {
                let (peer, payload) = peer_and_rest(bytes, rest)?;
                Ok(ToRelay::Data { peer, payload })
            }
            TO_TOKENS => {
                let (count, hashes) = rest
                    .split_first_chunk::<2>()
                    .ok_or(WireError::Malformed(bytes.len()))?;
                let count = u16::from_le_bytes(*count) as usize;
                if count > MAX_TOKENS {
                    return Err(WireError::TooManyTokens(count));
                }
                if hashes.len() != count * HASH_LEN {
                    return Err(WireError::Malformed(bytes.len()));
                }
                // Length checked above, so there is no remainder.
                let set = hashes.as_chunks::<HASH_LEN>().0.to_vec();
                Ok(ToRelay::Tokens(set))
            }
            TO_CLOSE => {
                let (peer, code) = peer_and_rest(bytes, rest)?;
                let code: [u8; 2] = code
                    .try_into()
                    .map_err(|_| WireError::Malformed(bytes.len()))?;
                Ok(ToRelay::Close {
                    peer,
                    code: u16::from_le_bytes(code),
                })
            }
            other => Err(WireError::UnknownType(other)),
        }
    }
}

/// Relay → gateway.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FromRelay<'a> {
    /// `payload` arrived from the device connection `peer`.
    Data { peer: PeerId, payload: &'a [u8] },
    /// A device was admitted with the token whose hash is `token`.
    Open { peer: PeerId, token: Hash },
    /// The device connection `peer` ended (any reason).
    Closed { peer: PeerId },
}

impl FromRelay<'_> {
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        Ok(match self {
            FromRelay::Data { peer, payload } => data(FROM_DATA, *peer, payload)?,
            FromRelay::Open { peer, token } => {
                let mut out = Vec::with_capacity(5 + HASH_LEN);
                out.push(FROM_OPEN);
                out.extend_from_slice(&peer.to_le_bytes());
                out.extend_from_slice(token);
                out
            }
            FromRelay::Closed { peer } => {
                let mut out = Vec::with_capacity(5);
                out.push(FROM_CLOSED);
                out.extend_from_slice(&peer.to_le_bytes());
                out
            }
        })
    }

    pub fn decode(bytes: &[u8]) -> Result<FromRelay<'_>, WireError> {
        let (&kind, rest) = bytes.split_first().ok_or(WireError::Empty)?;
        match kind {
            FROM_DATA => {
                let (peer, payload) = peer_and_rest(bytes, rest)?;
                Ok(FromRelay::Data { peer, payload })
            }
            FROM_OPEN => {
                let (peer, token) = peer_and_rest(bytes, rest)?;
                let token: Hash = token
                    .try_into()
                    .map_err(|_| WireError::Malformed(bytes.len()))?;
                Ok(FromRelay::Open { peer, token })
            }
            FROM_CLOSED => {
                let (peer, tail) = peer_and_rest(bytes, rest)?;
                if !tail.is_empty() {
                    return Err(WireError::Malformed(bytes.len()));
                }
                Ok(FromRelay::Closed { peer })
            }
            other => Err(WireError::UnknownType(other)),
        }
    }
}

/// Checks a device payload's size (device messages carry no header).
pub fn check_payload(payload: &[u8]) -> Result<(), WireError> {
    if payload.len() > MAX_PAYLOAD {
        return Err(WireError::TooLarge(payload.len()));
    }
    Ok(())
}

fn data(kind: u8, peer: PeerId, payload: &[u8]) -> Result<Vec<u8>, WireError> {
    check_payload(payload)?;
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(kind);
    out.extend_from_slice(&peer.to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

fn peer_and_rest<'a>(whole: &[u8], rest: &'a [u8]) -> Result<(PeerId, &'a [u8]), WireError> {
    let (peer, tail) = rest
        .split_first_chunk::<4>()
        .ok_or(WireError::Malformed(whole.len()))?;
    check_payload(tail)?;
    Ok((u32::from_le_bytes(*peer), tail))
}

/// A gateway's credential for one relay room, created by `omp-deck-relay enroll` and pasted into
/// OmpDeck's Mobile access settings: `ompdeck-relay-v1:<base64url(room ‖ secret)>`.
#[derive(Clone, PartialEq, Eq)]
pub struct RoomKey {
    pub room: Room,
    pub secret: Secret,
}

impl RoomKey {
    pub fn encode(&self) -> String {
        let mut raw = [0u8; ROOM_LEN + SECRET_LEN];
        raw[..ROOM_LEN].copy_from_slice(&self.room);
        raw[ROOM_LEN..].copy_from_slice(&self.secret);
        format!("{ROOM_KEY_PREFIX}{}", URL_SAFE_NO_PAD.encode(raw))
    }

    pub fn parse(text: &str) -> Result<RoomKey, WireError> {
        let body = text
            .trim()
            .strip_prefix(ROOM_KEY_PREFIX)
            .ok_or(WireError::RoomKey)?;
        let raw = URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| WireError::RoomKey)?;
        if raw.len() != ROOM_LEN + SECRET_LEN {
            return Err(WireError::RoomKey);
        }
        let (room, secret) = raw.split_at(ROOM_LEN);
        Ok(RoomKey {
            room: room.try_into().map_err(|_| WireError::RoomKey)?,
            secret: secret.try_into().map_err(|_| WireError::RoomKey)?,
        })
    }
}

impl std::fmt::Debug for RoomKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RoomKey {{ room: {}, .. }}", room_id(&self.room))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_round_trips_and_rejects_other_versions_and_lengths() {
        let g = Hello::Gateway {
            room: [1; ROOM_LEN],
            secret: [2; 32],
        };
        let d = Hello::Device {
            room: [3; ROOM_LEN],
            token: [4; 32],
        };
        assert_eq!(Hello::decode(&g.encode()), Ok(g.clone()));
        assert_eq!(Hello::decode(&d.encode()), Ok(d));

        let mut v2 = g.encode();
        v2[1] = 2;
        assert_eq!(Hello::decode(&v2), Err(WireError::Version(2)));
        let bytes = g.encode();
        assert!(Hello::decode(&bytes[..bytes.len() - 1]).is_err());
        let mut long = g.encode();
        long.push(0);
        assert!(Hello::decode(&long).is_err());
        assert_eq!(Hello::decode(&[]), Err(WireError::Empty));
        assert_eq!(Hello::decode(&[b'X', 1]), Err(WireError::UnknownType(b'X')));
        // Debug never prints the secret.
        assert!(!format!("{g:?}").contains("2, 2"));
    }

    #[test]
    fn gateway_messages_round_trip_and_enforce_limits() {
        let payload = vec![7u8; MAX_PAYLOAD];
        for m in [
            ToRelay::Data {
                peer: 9,
                payload: &payload,
            },
            ToRelay::Data {
                peer: u32::MAX,
                payload: &[],
            },
            ToRelay::Tokens(vec![[5; HASH_LEN], [6; HASH_LEN]]),
            ToRelay::Tokens(Vec::new()),
            ToRelay::Close {
                peer: 3,
                code: close::UNAUTHORIZED,
            },
        ] {
            let bytes = m.encode().unwrap();
            assert!(bytes.len() <= MAX_GATEWAY_MESSAGE.max(3 + MAX_TOKENS * HASH_LEN));
            assert_eq!(ToRelay::decode(&bytes), Ok(m));
        }

        let over = vec![0u8; MAX_PAYLOAD + 1];
        assert_eq!(
            ToRelay::Data {
                peer: 1,
                payload: &over
            }
            .encode(),
            Err(WireError::TooLarge(MAX_PAYLOAD + 1))
        );
        assert_eq!(
            ToRelay::Tokens(vec![[0; HASH_LEN]; MAX_TOKENS + 1]).encode(),
            Err(WireError::TooManyTokens(MAX_TOKENS + 1))
        );
        assert!(
            ToRelay::Tokens(vec![[0; HASH_LEN]; MAX_TOKENS])
                .encode()
                .is_ok()
        );

        // A token count that disagrees with the bytes present is malformed.
        let mut forged = ToRelay::Tokens(vec![[1; HASH_LEN]]).encode().unwrap();
        forged[1] = 2;
        assert!(ToRelay::decode(&forged).is_err());
        assert!(ToRelay::decode(&[TO_CLOSE, 1, 0, 0, 0, 1]).is_err());
        assert!(ToRelay::decode(&[TO_DATA, 1, 0]).is_err());
        assert_eq!(ToRelay::decode(&[0x7f]), Err(WireError::UnknownType(0x7f)));
    }

    #[test]
    fn relay_messages_round_trip_and_reject_trailing_bytes() {
        for m in [
            FromRelay::Data {
                peer: 4,
                payload: b"noise",
            },
            FromRelay::Open {
                peer: 5,
                token: [8; HASH_LEN],
            },
            FromRelay::Closed { peer: 6 },
        ] {
            assert_eq!(FromRelay::decode(&m.encode().unwrap()), Ok(m));
        }
        let mut closed = FromRelay::Closed { peer: 6 }.encode().unwrap();
        closed.push(0);
        assert!(FromRelay::decode(&closed).is_err());
        let open = FromRelay::Open {
            peer: 5,
            token: [8; HASH_LEN],
        }
        .encode()
        .unwrap();
        assert!(FromRelay::decode(&open[..open.len() - 1]).is_err());
    }

    #[test]
    fn room_keys_round_trip_and_reject_foreign_text() {
        let key = RoomKey {
            room: [0xab; ROOM_LEN],
            secret: [0xcd; SECRET_LEN],
        };
        let text = key.encode();
        assert!(text.starts_with(ROOM_KEY_PREFIX));
        assert_eq!(RoomKey::parse(&format!("  {text}\n")), Ok(key.clone()));
        assert_eq!(
            RoomKey::parse("ompdeck-relay-v1:AAAA"),
            Err(WireError::RoomKey)
        );
        assert_eq!(RoomKey::parse(&text[1..]), Err(WireError::RoomKey));
        assert!(!format!("{key:?}").contains("zc3N"));
    }

    #[test]
    fn hashes_are_sha256() {
        // SHA-256 of 32 zero bytes.
        assert_eq!(hash(&[0; 32])[..4], [0x66, 0x68, 0x7a, 0xad]);
    }
}
