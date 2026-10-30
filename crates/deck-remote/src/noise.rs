//! The OmpDeck device protocol (`docs/notes/mobile-relay.md`, "R1 contract").
//!
//! - **Pairing**: `Noise_IKpsk1_25519_ChaChaPoly_BLAKE2s`. The device knows the gateway's static
//!   key and a one-use 32-byte PSK from the QR [`PairingInvite`]. The PSK is mixed at the end of
//!   message 1, so [`respond`] rejects a device without it before anything is shown or answered.
//! - **Reconnect**: `Noise_IK_25519_ChaChaPoly_BLAKE2s`. [`Pending::device_public`] is known
//!   before any answer exists, so an unknown or revoked device never gets message 2.
//! - **Session**: every transport message's plaintext starts with a flags byte ([`FLAG_MORE`],
//!   [`FLAG_REKEY`]); messages larger than [`MAX_FRAGMENT_DATA`] are fragmented, and every
//!   non-final fragment carries exactly [`MAX_FRAGMENT_DATA`] bytes; both sides run Noise's
//!   REKEY exactly at the flagged message. Any error poisons the [`Session`].
//!
//! Message 1 carries a [`DeviceHello`] (with a strictly increasing timestamp, see
//! [`timestamp_fresh`]); message 2 carries a [`Welcome`]. Both are small flat JSON objects,
//! read by a strict scanner (`noise/payload.rs`) that the PWA mirrors exactly. A payload whose
//! `v` is an integer other than 1 is [`NoiseError::UnsupportedVersion`], distinct from a
//! malformed one, so the gateway can answer `relay::close::UPGRADE_REQUIRED` (the device keeps
//! its pairing). Device-supplied names are [`is_display_safe`] text.
//!
//! **Secrets in memory.** [`Keypair`] and [`PairingInvite::psk`] zeroize their own buffers on
//! drop, and nothing here prints them. That is the whole guarantee: `snow` 0.10 zeroizes
//! nothing, so its copies of the static key, the PSK, the ephemeral keys, the chaining key
//! and the transport keys stay in freed memory until reused, and so do stack copies made
//! while deriving a public key. The PWA (`pwa/src/remote/protocol.ts`) zeroes its own handshake
//! copies after `split` or a failure, but not engine or library copies (see its module comment).
//! Treat process memory as holding every session secret.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use blake2::{Blake2s256, Digest};
use curve25519_dalek::montgomery::MontgomeryPoint;
use serde::Serialize;
use snow::params::NoiseParams;
use zeroize::Zeroizing;

use crate::relay::{ROOM_LEN, Room, TOKEN_LEN, Token};

#[cfg(test)]
mod forge;
mod payload;
#[cfg(test)]
mod vectors;

/// Mixed into both handshakes: binds them to this protocol and version.
pub const PROLOGUE: &[u8] = b"ompdeck-remote-v1";
pub const PAIRING_PATTERN: &str = "Noise_IKpsk1_25519_ChaChaPoly_BLAKE2s";
pub const RECONNECT_PATTERN: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";

pub const KEY_LEN: usize = 32;
pub const PSK_LEN: usize = 32;
pub const HASH_LEN: usize = 32;
/// AEAD tag of every encrypted Noise payload.
pub const TAG_LEN: usize = 16;
/// The Noise spec's message cap; also the relay's device payload cap.
pub const MAX_MESSAGE: usize = 65_535;
/// Application bytes per transport frame: the message cap minus the tag and the flags byte.
pub const MAX_FRAGMENT_DATA: usize = MAX_MESSAGE - TAG_LEN - 1;
/// Largest [`DeviceHello`] or [`Welcome`] payload.
pub const MAX_HANDSHAKE_PAYLOAD: usize = 1024;
/// Largest message 1: ephemeral, encrypted static, encrypted payload.
pub const MAX_MSG1: usize = KEY_LEN + KEY_LEN + TAG_LEN + MAX_HANDSHAKE_PAYLOAD + TAG_LEN;
/// Largest message 2: ephemeral, encrypted payload.
pub const MAX_MSG2: usize = KEY_LEN + MAX_HANDSHAKE_PAYLOAD + TAG_LEN;

pub const MAX_NAME_CHARS: usize = 64;
pub const MAX_PLATFORM_CHARS: usize = 32;
pub const MAX_DEVICE_ID_CHARS: usize = 64;
pub const MAX_GATEWAY_LABEL_CHARS: usize = 64;
/// Timestamps stay exactly representable as JavaScript numbers (2^53 − 1).
pub const MAX_TIMESTAMP: u64 = MAX_PAYLOAD_INT;
/// Largest integer of the payload grammar (2^53 − 1).
const MAX_PAYLOAD_INT: u64 = (1 << 53) - 1;
/// [`DeviceHello::features`] names at most.
pub const MAX_FEATURES: usize = 32;

/// Flags bit: more fragments of this application message follow.
pub const FLAG_MORE: u8 = 0x01;
/// Flags bit: the sender rekeyed its sending cipher after this frame; the receiver rekeys its
/// receiving cipher after opening it.
pub const FLAG_REKEY: u8 = 0x02;
const KNOWN_FLAGS: u8 = FLAG_MORE | FLAG_REKEY;

pub const DEFAULT_REKEY_MESSAGES: u64 = 1 << 20;
pub const DEFAULT_REKEY_INTERVAL: Duration = Duration::from_secs(30 * 60);
pub const DEFAULT_MAX_REASSEMBLY: usize = 16 * 1024 * 1024;

/// Prefix of an encoded [`PairingInvite`].
pub const INVITE_PREFIX: &str = "v1.";
pub const MAX_INVITE_URL: usize = 255;

pub type PublicKey = [u8; KEY_LEN];
pub type Psk = [u8; PSK_LEN];

#[derive(Debug, thiserror::Error)]
pub enum NoiseError {
    #[error("the operating system's random number generator failed")]
    Random,
    #[error("Noise setup failed: {0}")]
    Setup(#[source] snow::Error),
    #[error("handshake message of {0} bytes is malformed or too large")]
    MessageSize(usize),
    /// Wrong PSK, wrong gateway key, tampered or forged handshake message, or a low-order
    /// (small-subgroup) X25519 key from the peer ([`is_low_order`]).
    #[error("handshake message failed authentication")]
    Handshake,
    #[error("invalid handshake payload: {0}")]
    Payload(&'static str),
    /// The payload's `v` is an integer other than 1: the peer speaks another payload version.
    #[error("unsupported handshake payload version")]
    UnsupportedVersion,
    #[error("transport frame of {0} bytes is malformed")]
    FrameSize(usize),
    /// Tampered, replayed, reordered or foreign frame.
    #[error("transport frame failed authentication")]
    Decrypt,
    #[error("transport frame has unknown flag bits {0:#04x}")]
    BadFlags(u8),
    #[error("message of {size} bytes exceeds the {cap}-byte limit")]
    TooLarge { size: usize, cap: usize },
    #[error("Noise transport failed: {0}")]
    Transport(#[source] snow::Error),
    #[error("the session failed earlier and is unusable")]
    Poisoned,
    #[error("invalid pairing invite: {0}")]
    Invite(&'static str),
}

/// An X25519 static key pair. The secret is zeroized on drop and never printed.
#[derive(Clone)]
pub struct Keypair {
    secret: Zeroizing<[u8; KEY_LEN]>,
    public: PublicKey,
}

impl Keypair {
    pub fn generate() -> Result<Keypair, NoiseError> {
        let mut secret = Zeroizing::new([0u8; KEY_LEN]);
        getrandom::fill(secret.as_mut()).map_err(|_| NoiseError::Random)?;
        Ok(Keypair::from_zeroizing(secret))
    }

    /// Restores a stored key pair from its secret bytes.
    pub fn from_secret(secret: [u8; KEY_LEN]) -> Keypair {
        Keypair::from_zeroizing(Zeroizing::new(secret))
    }

    fn from_zeroizing(secret: Zeroizing<[u8; KEY_LEN]>) -> Keypair {
        let public = MontgomeryPoint::mul_base_clamped(*secret).to_bytes();
        Keypair { secret, public }
    }

    pub fn public(&self) -> &PublicKey {
        &self.public
    }

    /// The secret bytes, for storage only.
    pub fn secret_bytes(&self) -> &[u8; KEY_LEN] {
        &self.secret
    }
}

impl std::fmt::Debug for Keypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keypair")
            .field("fingerprint", &fingerprint(&self.public))
            .finish_non_exhaustive()
    }
}

/// What the desktop shows for a key: the first 10 bytes of BLAKE2s-256(public) as hex in five
/// dash-separated groups of 4 (`1a2b-3c4d-5e6f-7081-92a3`).
pub fn fingerprint(public: &PublicKey) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Blake2s256::digest(public);
    let mut out = String::with_capacity(24);
    for (i, byte) in digest[..10].iter().enumerate() {
        if i > 0 && i % 2 == 0 {
            out.push('-');
        }
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

/// Clamped scalar used to probe a key's order; any clamped scalar works (it is a multiple of the
/// cofactor 8 and not a multiple of the prime subgroup order).
const LOW_ORDER_PROBE: [u8; KEY_LEN] = [0x5a; KEY_LEN];

/// Whether an X25519 public key has small order (its order divides the cofactor 8), including
/// non-canonical encodings of such points. A clamped multiplication by it yields all zeros, so
/// every DH with it is a constant an attacker knows: such keys are refused as static and
/// ephemeral keys of the peer, on both sides.
pub fn is_low_order(public: &PublicKey) -> bool {
    MontgomeryPoint(*public)
        .mul_clamped(LOW_ORDER_PROBE)
        .to_bytes()
        == [0u8; KEY_LEN]
}

/// The peer's ephemeral key: the first 32 bytes of messages 1 and 2.
fn check_ephemeral(message: &[u8]) -> Result<(), NoiseError> {
    let ephemeral: &PublicKey = message[..KEY_LEN]
        .try_into()
        .map_err(|_| NoiseError::MessageSize(message.len()))?;
    if is_low_order(ephemeral) {
        return Err(NoiseError::Handshake);
    }
    Ok(())
}

/// Whether a device timestamp may be accepted after the last one seen from that device.
pub fn timestamp_fresh(last: u64, ts: u64) -> bool {
    ts > last
}

/// The device's next timestamp: the current time, but always after the last one sent.
pub fn next_timestamp(last: u64, now_ms: u64) -> u64 {
    now_ms.max(last.saturating_add(1))
}

/// Whether device-supplied text (a [`DeviceHello`] name or platform, a device rename) is safe
/// to show: no C0/C1 control character (`char::is_control`), and none of the invisible,
/// separator and bidirectional formatting characters U+00AD, U+061C, U+180E, U+200B–U+200F,
/// U+2028–U+202E, U+2060–U+2064, U+2066–U+206F, U+FEFF and U+FFF9–U+FFFB. An explicit list
/// (no Unicode tables), identical in the PWA; it also refuses ZWJ emoji sequences.
pub fn is_display_safe(text: &str) -> bool {
    !text.chars().any(|c| {
        c.is_control()
            || matches!(
                c,
                '\u{ad}'
                    | '\u{61c}'
                    | '\u{180e}'
                    | '\u{200b}'..='\u{200f}'
                    | '\u{2028}'..='\u{202e}'
                    | '\u{2060}'..='\u{2064}'
                    | '\u{2066}'..='\u{206f}'
                    | '\u{feff}'
                    | '\u{fff9}'..='\u{fffb}'
            )
    })
}

/// Message 1 payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceHello {
    /// At most [`MAX_NAME_CHARS`], [`is_display_safe`].
    pub name: String,
    /// At most [`MAX_PLATFORM_CHARS`], [`is_display_safe`].
    pub platform: String,
    /// Unix milliseconds, strictly increasing per device ([`next_timestamp`]).
    pub ts: u64,
    /// The highest application protocol (`api::PROTOCOL`) the device speaks, at least 1.
    /// Key `api`; absent means 1 (and 1 is not written).
    pub api: u64,
    /// Optional application features the device understands: at most [`MAX_FEATURES`]
    /// distinct non-empty names of `[a-z0-9_.]`. Key `features`, comma-separated; absent
    /// means none (and none is not written).
    pub features: Vec<String>,
}

#[derive(Serialize)]
struct HelloOut<'a> {
    v: u8,
    name: &'a str,
    platform: &'a str,
    ts: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    api: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    features: Option<String>,
}

impl DeviceHello {
    pub fn encode(&self) -> Result<Vec<u8>, NoiseError> {
        self.validate()?;
        let out = serde_json::to_vec(&HelloOut {
            v: 1,
            name: &self.name,
            platform: &self.platform,
            ts: self.ts,
            api: (self.api != 1).then_some(self.api),
            features: (!self.features.is_empty()).then(|| self.features.join(",")),
        })
        .map_err(|_| NoiseError::Payload("hello is not serializable"))?;
        check_payload_len(&out)?;
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<DeviceHello, NoiseError> {
        const INVALID: &str = "hello is not valid";
        let mut fields = read_payload(bytes, INVALID)?;
        let hello = DeviceHello {
            name: take_str(&mut fields, "name", INVALID)?,
            platform: take_str(&mut fields, "platform", INVALID)?,
            ts: match fields.take("ts") {
                Some(payload::Value::Int(ts)) => ts,
                _ => return Err(NoiseError::Payload(INVALID)),
            },
            api: match fields.take("api") {
                None => 1,
                Some(payload::Value::Int(api)) => api,
                Some(payload::Value::Str(_)) => return Err(NoiseError::Payload(INVALID)),
            },
            features: match fields.take("features") {
                None => Vec::new(),
                Some(payload::Value::Str(list)) => list.split(',').map(str::to_owned).collect(),
                Some(payload::Value::Int(_)) => return Err(NoiseError::Payload(INVALID)),
            },
        };
        hello.validate()?;
        Ok(hello)
    }

    fn validate(&self) -> Result<(), NoiseError> {
        check_chars(&self.name, MAX_NAME_CHARS, "device name too long")?;
        check_chars(&self.platform, MAX_PLATFORM_CHARS, "platform too long")?;
        if !is_display_safe(&self.name) {
            return Err(NoiseError::Payload("device name is not display-safe"));
        }
        if !is_display_safe(&self.platform) {
            return Err(NoiseError::Payload("platform is not display-safe"));
        }
        if self.ts > MAX_TIMESTAMP {
            return Err(NoiseError::Payload("timestamp out of range"));
        }
        if self.api == 0 || self.api > MAX_PAYLOAD_INT {
            return Err(NoiseError::Payload("api version out of range"));
        }
        if self.features.len() > MAX_FEATURES {
            return Err(NoiseError::Payload("too many features"));
        }
        for (i, name) in self.features.iter().enumerate() {
            let valid = !name.is_empty()
                && name
                    .bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.'));
            if !valid || self.features[..i].contains(name) {
                return Err(NoiseError::Payload("features are not valid"));
            }
        }
        Ok(())
    }
}

/// Message 2 payload. `token` (the device's relay admission token) is present exactly in pairing.
#[derive(Clone, PartialEq, Eq)]
pub struct Welcome {
    pub device: String,
    pub gateway: String,
    pub token: Option<Token>,
}

impl std::fmt::Debug for Welcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Welcome")
            .field("device", &self.device)
            .field("gateway", &self.gateway)
            .field("token", &self.token.map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Serialize)]
struct WelcomeOut<'a> {
    v: u8,
    device: &'a str,
    gateway: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
}

impl Welcome {
    pub fn encode(&self) -> Result<Vec<u8>, NoiseError> {
        self.validate()?;
        let out = serde_json::to_vec(&WelcomeOut {
            v: 1,
            device: &self.device,
            gateway: &self.gateway,
            token: self.token.map(|t| URL_SAFE_NO_PAD.encode(t)),
        })
        .map_err(|_| NoiseError::Payload("welcome is not serializable"))?;
        check_payload_len(&out)?;
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Welcome, NoiseError> {
        const INVALID: &str = "welcome is not valid";
        let mut fields = read_payload(bytes, INVALID)?;
        let device = take_str(&mut fields, "device", INVALID)?;
        let gateway = take_str(&mut fields, "gateway", INVALID)?;
        let token = match fields.take("token") {
            None => None,
            Some(payload::Value::Str(text)) => {
                let bytes = URL_SAFE_NO_PAD
                    .decode(text)
                    .map_err(|_| NoiseError::Payload("welcome token is not base64url"))?;
                Some(
                    Token::try_from(bytes.as_slice())
                        .map_err(|_| NoiseError::Payload("welcome token has the wrong length"))?,
                )
            }
            Some(payload::Value::Int(_)) => {
                return Err(NoiseError::Payload("welcome token is not base64url"));
            }
        };
        let welcome = Welcome {
            device,
            gateway,
            token,
        };
        welcome.validate()?;
        Ok(welcome)
    }

    fn validate(&self) -> Result<(), NoiseError> {
        check_chars(&self.device, MAX_DEVICE_ID_CHARS, "device id too long")?;
        check_chars(
            &self.gateway,
            MAX_GATEWAY_LABEL_CHARS,
            "gateway label too long",
        )
    }

    fn check_mode(&self, pairing: bool) -> Result<(), NoiseError> {
        match (pairing, self.token.is_some()) {
            (true, false) => Err(NoiseError::Payload("pairing welcome lacks a token")),
            (false, true) => Err(NoiseError::Payload("reconnect welcome carries a token")),
            _ => Ok(()),
        }
    }
}

fn check_chars(text: &str, max: usize, what: &'static str) -> Result<(), NoiseError> {
    if text.chars().count() > max {
        return Err(NoiseError::Payload(what));
    }
    Ok(())
}

fn check_payload_len(bytes: &[u8]) -> Result<(), NoiseError> {
    if bytes.len() > MAX_HANDSHAKE_PAYLOAD {
        return Err(NoiseError::Payload("payload exceeds 1024 bytes"));
    }
    Ok(())
}

/// Checks the size and grammar of a handshake payload and its version (`v` exactly 1).
fn read_payload(bytes: &[u8], invalid: &'static str) -> Result<payload::Fields, NoiseError> {
    check_payload_len(bytes)?;
    let mut fields = payload::parse(bytes).ok_or(NoiseError::Payload(invalid))?;
    match fields.take("v") {
        Some(payload::Value::Int(1)) => Ok(fields),
        Some(payload::Value::Int(_)) => Err(NoiseError::UnsupportedVersion),
        Some(payload::Value::Str(_)) | None => Err(NoiseError::Payload(invalid)),
    }
}

fn take_str(
    fields: &mut payload::Fields,
    key: &str,
    invalid: &'static str,
) -> Result<String, NoiseError> {
    match fields.take(key) {
        Some(payload::Value::Str(text)) => Ok(text),
        _ => Err(NoiseError::Payload(invalid)),
    }
}

/// Which handshake to run.
#[derive(Clone, Copy)]
pub enum Mode<'a> {
    /// First contact through a [`PairingInvite`]: IKpsk1 with the invite's PSK.
    Pairing { psk: &'a Psk },
    /// A paired device: IK.
    Reconnect,
}

impl Mode<'_> {
    fn is_pairing(&self) -> bool {
        matches!(self, Mode::Pairing { .. })
    }
}

impl std::fmt::Debug for Mode<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.is_pairing() {
            "Pairing"
        } else {
            "Reconnect"
        })
    }
}

fn handshake_state(
    mode: Mode<'_>,
    local: &Keypair,
    remote: Option<&PublicKey>,
    ephemeral: Option<&[u8; KEY_LEN]>,
) -> Result<snow::HandshakeState, NoiseError> {
    let pattern = match mode {
        Mode::Pairing { .. } => PAIRING_PATTERN,
        Mode::Reconnect => RECONNECT_PATTERN,
    };
    let params: NoiseParams = pattern.parse().map_err(NoiseError::Setup)?;
    let mut builder = snow::Builder::new(params)
        .prologue(PROLOGUE)
        .and_then(|b| b.local_private_key(local.secret_bytes()))
        .map_err(NoiseError::Setup)?;
    if let Mode::Pairing { psk } = mode {
        builder = builder.psk(1, psk).map_err(NoiseError::Setup)?;
    }
    if let Some(ephemeral) = ephemeral {
        builder = builder.fixed_ephemeral_key_for_testing_only(ephemeral);
    }
    match remote {
        Some(remote) => builder
            .remote_public_key(remote)
            .and_then(|b| b.build_initiator()),
        None => builder.build_responder(),
    }
    .map_err(NoiseError::Setup)
}

fn into_session(state: snow::HandshakeState) -> Result<Session, NoiseError> {
    let handshake_hash: [u8; HASH_LEN] = state
        .get_handshake_hash()
        .try_into()
        .map_err(|_| NoiseError::Handshake)?;
    let transport = state.into_transport_mode().map_err(NoiseError::Setup)?;
    Ok(Session::new(transport, handshake_hash))
}

/// The device side between sending message 1 and receiving message 2.
pub struct Initiator {
    state: snow::HandshakeState,
    pairing: bool,
}

impl std::fmt::Debug for Initiator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Initiator")
            .field("pairing", &self.pairing)
            .finish_non_exhaustive()
    }
}

impl Initiator {
    /// Starts a handshake with the gateway whose static key is `gateway_public`.
    pub fn start(
        mode: Mode<'_>,
        device: &Keypair,
        gateway_public: &PublicKey,
        hello: &DeviceHello,
    ) -> Result<(Initiator, Vec<u8>), NoiseError> {
        Initiator::start_with(mode, device, gateway_public, hello, None)
    }

    fn start_with(
        mode: Mode<'_>,
        device: &Keypair,
        gateway_public: &PublicKey,
        hello: &DeviceHello,
        ephemeral: Option<&[u8; KEY_LEN]>,
    ) -> Result<(Initiator, Vec<u8>), NoiseError> {
        if is_low_order(gateway_public) {
            return Err(NoiseError::Handshake);
        }
        let payload = hello.encode()?;
        let mut state = handshake_state(mode, device, Some(gateway_public), ephemeral)?;
        let mut msg1 = vec![0u8; MAX_MSG1];
        let len = state
            .write_message(&payload, &mut msg1)
            .map_err(NoiseError::Setup)?;
        msg1.truncate(len);
        Ok((
            Initiator {
                state,
                pairing: mode.is_pairing(),
            },
            msg1,
        ))
    }

    /// Completes the handshake with the gateway's message 2.
    pub fn finish(mut self, msg2: &[u8]) -> Result<(Session, Welcome), NoiseError> {
        if msg2.len() > MAX_MSG2 || msg2.len() < KEY_LEN + TAG_LEN {
            return Err(NoiseError::MessageSize(msg2.len()));
        }
        check_ephemeral(msg2)?;
        let mut payload = [0u8; MAX_MSG2];
        let len = self
            .state
            .read_message(msg2, &mut payload)
            .map_err(|_| NoiseError::Handshake)?;
        let welcome = Welcome::decode(&payload[..len])?;
        welcome.check_mode(self.pairing)?;
        Ok((into_session(self.state)?, welcome))
    }
}

/// Reads a device's message 1. On success the device has proven its static key (and in
/// pairing, the PSK); the caller checks [`Pending::device_public`] and the [`DeviceHello`]
/// timestamp, possibly waits for desktop approval, then [`Pending::accept`]s or drops it.
pub fn respond(mode: Mode<'_>, gateway: &Keypair, msg1: &[u8]) -> Result<Pending, NoiseError> {
    respond_with(mode, gateway, msg1, None)
}

fn respond_with(
    mode: Mode<'_>,
    gateway: &Keypair,
    msg1: &[u8],
    ephemeral: Option<&[u8; KEY_LEN]>,
) -> Result<Pending, NoiseError> {
    if msg1.len() > MAX_MSG1 || msg1.len() < KEY_LEN + KEY_LEN + TAG_LEN + TAG_LEN {
        return Err(NoiseError::MessageSize(msg1.len()));
    }
    check_ephemeral(msg1)?;
    let mut state = handshake_state(mode, gateway, None, ephemeral)?;
    let mut payload = [0u8; MAX_MSG1];
    let len = state
        .read_message(msg1, &mut payload)
        .map_err(|_| NoiseError::Handshake)?;
    let device_public: PublicKey = state
        .get_remote_static()
        .and_then(|key| key.try_into().ok())
        .ok_or(NoiseError::Handshake)?;
    if is_low_order(&device_public) {
        return Err(NoiseError::Handshake);
    }
    let hello = DeviceHello::decode(&payload[..len])?;
    Ok(Pending {
        state,
        device_public,
        hello,
        pairing: mode.is_pairing(),
    })
}

/// A verified message 1 awaiting the gateway's decision. Dropping it closes the handshake.
pub struct Pending {
    state: snow::HandshakeState,
    device_public: PublicKey,
    hello: DeviceHello,
    pairing: bool,
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending")
            .field("device", &fingerprint(&self.device_public))
            .field("hello", &self.hello)
            .field("pairing", &self.pairing)
            .finish_non_exhaustive()
    }
}

impl Pending {
    pub fn device_public(&self) -> &PublicKey {
        &self.device_public
    }

    pub fn hello(&self) -> &DeviceHello {
        &self.hello
    }

    /// Answers with message 2. `welcome.token` must be present exactly in pairing.
    pub fn accept(mut self, welcome: &Welcome) -> Result<(Session, Vec<u8>), NoiseError> {
        welcome.check_mode(self.pairing)?;
        let payload = welcome.encode()?;
        let mut msg2 = vec![0u8; MAX_MSG2];
        let len = self
            .state
            .write_message(&payload, &mut msg2)
            .map_err(NoiseError::Setup)?;
        msg2.truncate(len);
        Ok((into_session(self.state)?, msg2))
    }
}

/// Transport limits and the rekey policy of one side of a [`Session`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionConfig {
    /// Rekey the sending cipher after this many frames (the last one carries [`FLAG_REKEY`]).
    pub rekey_messages: u64,
    /// Rekey the sending cipher on the first frame sealed this long after the last rekey.
    pub rekey_interval: Duration,
    /// Largest application message sealed or reassembled.
    pub max_message: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            rekey_messages: DEFAULT_REKEY_MESSAGES,
            rekey_interval: DEFAULT_REKEY_INTERVAL,
            max_message: DEFAULT_MAX_REASSEMBLY,
        }
    }
}

/// An established, encrypted session. After any error it is poisoned: every later call fails
/// with [`NoiseError::Poisoned`] and the connection must be closed.
pub struct Session {
    transport: snow::TransportState,
    handshake_hash: [u8; HASH_LEN],
    config: SessionConfig,
    sent_since_rekey: u64,
    last_rekey: Instant,
    reassembly: Vec<u8>,
    poisoned: bool,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("config", &self.config)
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}

impl Session {
    fn new(transport: snow::TransportState, handshake_hash: [u8; HASH_LEN]) -> Session {
        Session {
            transport,
            handshake_hash,
            config: SessionConfig::default(),
            sent_since_rekey: 0,
            last_rekey: Instant::now(),
            reassembly: Vec::new(),
            poisoned: false,
        }
    }

    /// The session id, equal on both sides.
    pub fn handshake_hash(&self) -> &[u8; HASH_LEN] {
        &self.handshake_hash
    }

    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    pub fn set_config(&mut self, config: SessionConfig) {
        self.config = config;
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Encrypts one application message into one or more frames (one relay payload each).
    pub fn seal(&mut self, msg: &[u8]) -> Result<Vec<Vec<u8>>, NoiseError> {
        self.seal_at(msg, Instant::now())
    }

    /// [`Session::seal`] with an explicit clock for the time-based rekey.
    pub fn seal_at(&mut self, msg: &[u8], now: Instant) -> Result<Vec<Vec<u8>>, NoiseError> {
        if self.poisoned {
            return Err(NoiseError::Poisoned);
        }
        let result = self.seal_inner(msg, now);
        self.poisoned = result.is_err();
        result
    }

    fn seal_inner(&mut self, msg: &[u8], now: Instant) -> Result<Vec<Vec<u8>>, NoiseError> {
        if msg.len() > self.config.max_message {
            return Err(NoiseError::TooLarge {
                size: msg.len(),
                cap: self.config.max_message,
            });
        }
        let count = msg.len().div_ceil(MAX_FRAGMENT_DATA).max(1);
        let mut frames = Vec::with_capacity(count);
        let mut plain = Vec::with_capacity(1 + msg.len().min(MAX_FRAGMENT_DATA));
        for index in 0..count {
            let start = index * MAX_FRAGMENT_DATA;
            let chunk = &msg[start..msg.len().min(start + MAX_FRAGMENT_DATA)];
            let rekey = self.sent_since_rekey.saturating_add(1) >= self.config.rekey_messages
                || now.saturating_duration_since(self.last_rekey) >= self.config.rekey_interval;
            let mut flags = if index + 1 < count { FLAG_MORE } else { 0 };
            if rekey {
                flags |= FLAG_REKEY;
            }
            plain.clear();
            plain.push(flags);
            plain.extend_from_slice(chunk);
            let mut frame = vec![0u8; plain.len() + TAG_LEN];
            let len = self
                .transport
                .write_message(&plain, &mut frame)
                .map_err(NoiseError::Transport)?;
            frame.truncate(len);
            frames.push(frame);
            if rekey {
                self.transport.rekey_outgoing();
                self.sent_since_rekey = 0;
                self.last_rekey = now;
            } else {
                self.sent_since_rekey += 1;
            }
        }
        Ok(frames)
    }

    /// Decrypts one frame. Returns the application message once its last fragment arrived.
    pub fn open(&mut self, frame: &[u8]) -> Result<Option<Vec<u8>>, NoiseError> {
        if self.poisoned {
            return Err(NoiseError::Poisoned);
        }
        let result = self.open_inner(frame);
        if result.is_err() {
            self.poisoned = true;
            self.reassembly = Vec::new();
        }
        result
    }

    fn open_inner(&mut self, frame: &[u8]) -> Result<Option<Vec<u8>>, NoiseError> {
        if frame.len() > MAX_MESSAGE || frame.len() <= TAG_LEN {
            return Err(NoiseError::FrameSize(frame.len()));
        }
        let mut plain = vec![0u8; frame.len()];
        let len = self
            .transport
            .read_message(frame, &mut plain)
            .map_err(|_| NoiseError::Decrypt)?;
        let flags = plain[0];
        if flags & !KNOWN_FLAGS != 0 {
            return Err(NoiseError::BadFlags(flags));
        }
        if flags & FLAG_REKEY != 0 {
            self.transport.rekey_incoming();
        }
        let data = &plain[1..len];
        // Every non-final fragment is full, as `seal` makes them: this bounds the fragments of
        // one message by the reassembly cap.
        if flags & FLAG_MORE != 0 && data.len() != MAX_FRAGMENT_DATA {
            return Err(NoiseError::FrameSize(frame.len()));
        }
        let size = self.reassembly.len() + data.len();
        if size > self.config.max_message {
            return Err(NoiseError::TooLarge {
                size,
                cap: self.config.max_message,
            });
        }
        if flags & FLAG_MORE != 0 {
            self.reassembly.extend_from_slice(data);
            return Ok(None);
        }
        if self.reassembly.is_empty() {
            plain.copy_within(1..len, 0);
            plain.truncate(len - 1);
            return Ok(Some(plain));
        }
        self.reassembly.extend_from_slice(data);
        Ok(Some(std::mem::take(&mut self.reassembly)))
    }
}

/// The QR code's secret: `v1.` + base64url(`url_len u8 ‖ url ‖ room 16 ‖ pairing token 32 ‖
/// gateway public 32 ‖ psk 32`). Carried in the URL fragment (`#pair=…`), never logged.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingInvite {
    /// Relay base URL, canonical: `wss://host[:port]` (or `ws://` for a loopback host),
    /// lowercase, no default port, no userinfo, path or query; at most 255 bytes.
    pub url: String,
    pub room: Room,
    /// One-use relay admission token for this pairing window.
    pub token: Token,
    pub gateway_public: PublicKey,
    pub psk: Zeroizing<Psk>,
}

const INVITE_FIXED: usize = ROOM_LEN + TOKEN_LEN + KEY_LEN + PSK_LEN;

impl PairingInvite {
    pub fn encode(&self) -> Result<String, NoiseError> {
        check_invite_url(&self.url)?;
        let mut raw = Zeroizing::new(Vec::with_capacity(1 + self.url.len() + INVITE_FIXED));
        raw.push(self.url.len() as u8);
        raw.extend_from_slice(self.url.as_bytes());
        raw.extend_from_slice(&self.room);
        raw.extend_from_slice(&self.token);
        raw.extend_from_slice(&self.gateway_public);
        raw.extend_from_slice(self.psk.as_ref());
        let mut out = String::with_capacity(INVITE_PREFIX.len() + raw.len().div_ceil(3) * 4);
        out.push_str(INVITE_PREFIX);
        URL_SAFE_NO_PAD.encode_string(raw.as_slice(), &mut out);
        Ok(out)
    }

    pub fn parse(text: &str) -> Result<PairingInvite, NoiseError> {
        let body = text
            .strip_prefix(INVITE_PREFIX)
            .ok_or(NoiseError::Invite("missing v1. prefix"))?;
        let raw = Zeroizing::new(
            URL_SAFE_NO_PAD
                .decode(body)
                .map_err(|_| NoiseError::Invite("not base64url"))?,
        );
        let (&url_len, rest) = raw
            .split_first()
            .ok_or(NoiseError::Invite("wrong length"))?;
        let url_len = usize::from(url_len);
        if rest.len() != url_len + INVITE_FIXED {
            return Err(NoiseError::Invite("wrong length"));
        }
        let (url, rest) = rest.split_at(url_len);
        let url = std::str::from_utf8(url)
            .map_err(|_| NoiseError::Invite("url is not UTF-8"))?
            .to_owned();
        check_invite_url(&url)?;
        let (room, rest) = rest.split_at(ROOM_LEN);
        let (token, rest) = rest.split_at(TOKEN_LEN);
        let (gateway_public, psk) = rest.split_at(KEY_LEN);
        let invalid = |_| NoiseError::Invite("wrong length");
        Ok(PairingInvite {
            url,
            room: room.try_into().map_err(invalid)?,
            token: token.try_into().map_err(invalid)?,
            gateway_public: gateway_public.try_into().map_err(invalid)?,
            psk: Zeroizing::new(psk.try_into().map_err(invalid)?),
        })
    }
}

/// The canonical relay base URL (what the gateway's relay setting produces), at most
/// [`MAX_INVITE_URL`] bytes: `wss://host[:port]`, or `ws://` only for a loopback host
/// (`localhost`, an IPv4 `127.x.y.z`, an IPv6 equal to `::1`).
///
/// - `host`: lowercase `[a-z0-9.-]+`, or `[` lowercase IPv6 `]` as `std::net::Ipv6Addr`
///   parses it;
/// - `port`: decimal 1–65535 without leading zeros, never the scheme's default (443/80);
/// - nothing else: no userinfo, path, query, fragment, whitespace or control characters.
fn check_invite_url(url: &str) -> Result<(), NoiseError> {
    if url.len() > MAX_INVITE_URL {
        return Err(NoiseError::Invite("url exceeds 255 bytes"));
    }
    let (tls, authority) = match (url.strip_prefix("wss://"), url.strip_prefix("ws://")) {
        (Some(rest), _) => (true, rest),
        (None, Some(rest)) => (false, rest),
        (None, None) => return Err(NoiseError::Invite("url must start with ws:// or wss://")),
    };
    let split = if authority.starts_with('[') {
        authority.find(']').map(|i| i + 1)
    } else {
        authority.find(':')
    };
    let (host, port) = authority.split_at(split.unwrap_or(authority.len()));
    if host.is_empty() {
        return Err(NoiseError::Invite("url has no host"));
    }
    let lowercase = |text: &str| !text.bytes().any(|b| b.is_ascii_uppercase());
    let loopback = match host.strip_prefix('[') {
        Some(bracketed) => {
            let address = bracketed
                .strip_suffix(']')
                .filter(|inner| lowercase(inner))
                .and_then(|inner| inner.parse::<Ipv6Addr>().ok())
                .ok_or(NoiseError::Invite("url host is not valid"))?;
            address == Ipv6Addr::LOCALHOST
        }
        None => {
            if !host
                .bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.'))
            {
                return Err(NoiseError::Invite("url host is not valid"));
            }
            host == "localhost" || host.parse::<Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
        }
    };
    if !port.is_empty() {
        let digits = port.strip_prefix(':').unwrap_or_default();
        let number = (!digits.starts_with('0') && digits.bytes().all(|b| b.is_ascii_digit()))
            .then(|| digits.parse::<u16>().ok())
            .flatten()
            .ok_or(NoiseError::Invite("url port is not valid"))?;
        if number == if tls { 443 } else { 80 } {
            return Err(NoiseError::Invite("url names the default port"));
        }
    }
    if !tls && !loopback {
        return Err(NoiseError::Invite("ws:// is only for a loopback host"));
    }
    Ok(())
}

impl std::fmt::Debug for PairingInvite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingInvite")
            .field("url", &self.url)
            .field("room", &crate::relay::room_id(&self.room))
            .field("gateway", &fingerprint(&self.gateway_public))
            .finish_non_exhaustive()
    }
}
