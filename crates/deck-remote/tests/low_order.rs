#![cfg(feature = "noise")]
//! Low-order (small-subgroup) X25519 keys are refused on both sides. snow cannot emit such keys,
//! so a minimal hand-written Noise peer forges handshakes whose DH results are the all-zero
//! constants a low-order key produces — exactly what an attacker would send.

use blake2::{Blake2s256, Digest};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use curve25519_dalek::montgomery::MontgomeryPoint;
use deck_remote::noise::*;

const PSK: Psk = [7; 32];
const ZERO: [u8; 32] = [0; 32];

fn unhex(text: &str) -> [u8; 32] {
    std::array::from_fn(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).unwrap())
}

/// Every small-order point encoding class: 0, 1, p − 1, both order-8 points, the non-canonical
/// p and p + 1, and a canonical one with the (ignored) top bit set.
fn low_order_points() -> Vec<[u8; 32]> {
    let mut with_top_bit = ZERO;
    with_top_bit[31] = 0x80;
    let mut one = ZERO;
    one[0] = 1;
    vec![
        ZERO,
        one,
        unhex("ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f"),
        unhex("e0eb7a7c3b41b8ae1656e3faf19fc46ada098deb9c32b1fd866205165f49b800"),
        unhex("5f9c95bca3508c24b1d0b1559c83ef5b04445cc4581c8e86d8224eddd09f1157"),
        unhex("edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f"),
        unhex("eeffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f"),
        with_top_bit,
    ]
}

fn dh(secret: &[u8; 32], public: &[u8; 32]) -> [u8; 32] {
    MontgomeryPoint(*public).mul_clamped(*secret).to_bytes()
}

fn hmac(key: &[u8; 32], parts: &[&[u8]]) -> [u8; 32] {
    let mut block = [0u8; 64];
    block[..32].copy_from_slice(key);
    let mut inner = Blake2s256::new();
    inner.update(block.map(|b| b ^ 0x36));
    for part in parts {
        inner.update(part);
    }
    let mut outer = Blake2s256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// Noise SymmetricState over BLAKE2s / ChaChaPoly.
struct Symmetric {
    ck: [u8; 32],
    h: [u8; 32],
    k: Option<[u8; 32]>,
    n: u64,
}

impl Symmetric {
    fn new(pattern: &str, responder_static: &[u8; 32]) -> Symmetric {
        let h: [u8; 32] = Blake2s256::digest(pattern.as_bytes()).into();
        let mut sym = Symmetric {
            ck: h,
            h,
            k: None,
            n: 0,
        };
        sym.mix_hash(PROLOGUE);
        sym.mix_hash(responder_static);
        sym
    }

    fn mix_hash(&mut self, data: &[u8]) {
        let mut hasher = Blake2s256::new();
        hasher.update(self.h);
        hasher.update(data);
        self.h = hasher.finalize().into();
    }

    fn hkdf(&self, ikm: &[u8]) -> [[u8; 32]; 3] {
        let temp = hmac(&self.ck, &[ikm]);
        let o1 = hmac(&temp, &[&[1]]);
        let o2 = hmac(&temp, &[&o1, &[2]]);
        let o3 = hmac(&temp, &[&o2, &[3]]);
        [o1, o2, o3]
    }

    fn mix_key(&mut self, ikm: &[u8]) {
        let [ck, k, _] = self.hkdf(ikm);
        self.ck = ck;
        self.k = Some(k);
        self.n = 0;
    }

    fn mix_key_and_hash(&mut self, ikm: &[u8]) {
        let [ck, h, k] = self.hkdf(ikm);
        self.ck = ck;
        self.mix_hash(&h);
        self.k = Some(k);
        self.n = 0;
    }

    fn nonce(&self) -> Nonce {
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&self.n.to_le_bytes());
        nonce.into()
    }

    fn encrypt_and_hash(&mut self, plain: &[u8]) -> Vec<u8> {
        let k = self.k.expect("key");
        let out = ChaCha20Poly1305::new(&k.into())
            .encrypt(
                &self.nonce(),
                Payload {
                    msg: plain,
                    aad: &self.h,
                },
            )
            .unwrap();
        self.n += 1;
        self.mix_hash(&out);
        out
    }

    fn decrypt_and_hash(&mut self, cipher: &[u8]) -> Vec<u8> {
        let k = self.k.expect("key");
        let out = ChaCha20Poly1305::new(&k.into())
            .decrypt(
                &self.nonce(),
                Payload {
                    msg: cipher,
                    aad: &self.h,
                },
            )
            .unwrap();
        self.n += 1;
        self.mix_hash(cipher);
        out
    }
}

fn pattern(psk: Option<&Psk>) -> &'static str {
    if psk.is_some() {
        PAIRING_PATTERN
    } else {
        RECONNECT_PATTERN
    }
}

fn mode(psk: Option<&Psk>) -> Mode<'_> {
    match psk {
        Some(psk) => Mode::Pairing { psk },
        None => Mode::Reconnect,
    }
}

/// Message 1 from a forging device: `e`/`s` public keys of its choice, with the DH results the
/// responder will compute (all zeros for a low-order key).
fn forge_msg1(
    psk: Option<&Psk>,
    gateway: &[u8; 32],
    e: &[u8; 32],
    es: &[u8; 32],
    s: &[u8; 32],
    ss: &[u8; 32],
) -> Vec<u8> {
    let mut sym = Symmetric::new(pattern(psk), gateway);
    let mut out = e.to_vec();
    sym.mix_hash(e);
    if psk.is_some() {
        sym.mix_key(e);
    }
    sym.mix_key(es);
    out.extend(sym.encrypt_and_hash(s));
    sym.mix_key(ss);
    if let Some(psk) = psk {
        sym.mix_key_and_hash(psk);
    }
    let hello = DeviceHello {
        name: "forger".into(),
        platform: "x".into(),
        ts: 1,
    };
    out.extend(sym.encrypt_and_hash(&hello.encode().unwrap()));
    out
}

fn honest_keys() -> ([u8; 32], [u8; 32]) {
    let key = Keypair::generate().unwrap();
    (*key.secret_bytes(), *key.public())
}

/// A raw snow responder: the same handshake without this crate's key checks.
fn snow_accepts_msg1(psk: Option<&Psk>, gateway: &Keypair, msg1: &[u8]) -> bool {
    let mut builder = snow::Builder::new(pattern(psk).parse().unwrap())
        .prologue(PROLOGUE)
        .unwrap()
        .local_private_key(gateway.secret_bytes())
        .unwrap();
    if let Some(psk) = psk {
        builder = builder.psk(1, psk).unwrap();
    }
    let mut state = builder.build_responder().unwrap();
    let mut payload = vec![0; MAX_MESSAGE];
    state.read_message(msg1, &mut payload).is_ok()
}

#[test]
fn low_order_points_are_detected() {
    for point in low_order_points() {
        assert!(is_low_order(&point), "{point:02x?}");
    }
    for _ in 0..1000 {
        assert!(!is_low_order(Keypair::generate().unwrap().public()));
    }
}

#[test]
fn initiator_refuses_a_low_order_gateway_key() {
    let device = Keypair::generate().unwrap();
    let hello = DeviceHello {
        name: "a".into(),
        platform: "b".into(),
        ts: 1,
    };
    for point in low_order_points() {
        for psk in [Some(&PSK), None] {
            let err = Initiator::start(mode(psk), &device, &point, &hello).unwrap_err();
            assert!(matches!(err, NoiseError::Handshake), "{err:?}");
        }
    }
}

#[test]
fn forger_is_a_correct_noise_peer() {
    let gateway = Keypair::generate().unwrap();
    for psk in [Some(&PSK), None] {
        let (e_secret, e) = honest_keys();
        let (s_secret, s) = honest_keys();
        let msg1 = forge_msg1(
            psk,
            gateway.public(),
            &e,
            &dh(&e_secret, gateway.public()),
            &s,
            &dh(&s_secret, gateway.public()),
        );
        let pending = respond(mode(psk), &gateway, &msg1).unwrap();
        assert_eq!(pending.device_public(), &s);
    }
}

#[test]
fn responder_refuses_a_low_order_device_ephemeral() {
    let gateway = Keypair::generate().unwrap();
    for point in low_order_points() {
        for psk in [Some(&PSK), None] {
            let (s_secret, s) = honest_keys();
            let msg1 = forge_msg1(
                psk,
                gateway.public(),
                &point,
                &ZERO,
                &s,
                &dh(&s_secret, gateway.public()),
            );
            // Without the check the forged message is a valid handshake message.
            assert!(snow_accepts_msg1(psk, &gateway, &msg1));
            let err = respond(mode(psk), &gateway, &msg1).unwrap_err();
            assert!(matches!(err, NoiseError::Handshake), "{err:?}");
        }
    }
}

#[test]
fn responder_refuses_a_low_order_device_static() {
    let gateway = Keypair::generate().unwrap();
    for point in low_order_points() {
        for psk in [Some(&PSK), None] {
            let (e_secret, e) = honest_keys();
            let msg1 = forge_msg1(
                psk,
                gateway.public(),
                &e,
                &dh(&e_secret, gateway.public()),
                &point,
                &ZERO,
            );
            assert!(snow_accepts_msg1(psk, &gateway, &msg1));
            let err = respond(mode(psk), &gateway, &msg1).unwrap_err();
            assert!(matches!(err, NoiseError::Handshake), "{err:?}");
        }
    }
}

/// A forging gateway: reads the device's message 1 and answers with `e` of its choice; the
/// initiator's `ee` and `se` with a low-order `e` are both zero.
fn forge_msg2(
    psk: Option<&Psk>,
    gateway: &Keypair,
    msg1: &[u8],
    e: &[u8; 32],
    ee_se: Option<&[u8; 32]>,
) -> Vec<u8> {
    let mut sym = Symmetric::new(pattern(psk), gateway.public());
    let re: [u8; 32] = msg1[..32].try_into().unwrap();
    sym.mix_hash(&re);
    if psk.is_some() {
        sym.mix_key(&re);
    }
    sym.mix_key(&dh(gateway.secret_bytes(), &re));
    let rs: [u8; 32] = sym.decrypt_and_hash(&msg1[32..80]).try_into().unwrap();
    sym.mix_key(&dh(gateway.secret_bytes(), &rs));
    if let Some(psk) = psk {
        sym.mix_key_and_hash(psk);
    }
    sym.decrypt_and_hash(&msg1[80..]);

    let mut out = e.to_vec();
    sym.mix_hash(e);
    if psk.is_some() {
        sym.mix_key(e);
    }
    match ee_se {
        Some(e_secret) => {
            sym.mix_key(&dh(e_secret, &re));
            sym.mix_key(&dh(e_secret, &rs));
        }
        None => {
            sym.mix_key(&ZERO);
            sym.mix_key(&ZERO);
        }
    }
    let welcome = Welcome {
        device: "dev".into(),
        gateway: "gw".into(),
        token: psk.map(|_| [1; 32]),
    };
    out.extend(sym.encrypt_and_hash(&welcome.encode().unwrap()));
    out
}

fn snow_initiator(psk: Option<&Psk>, device: &Keypair, gateway: &Keypair) -> snow::HandshakeState {
    let mut builder = snow::Builder::new(pattern(psk).parse().unwrap())
        .prologue(PROLOGUE)
        .unwrap()
        .local_private_key(device.secret_bytes())
        .unwrap()
        .remote_public_key(gateway.public())
        .unwrap();
    if let Some(psk) = psk {
        builder = builder.psk(1, psk).unwrap();
    }
    builder.build_initiator().unwrap()
}

#[test]
fn initiator_refuses_a_low_order_gateway_ephemeral() {
    let gateway = Keypair::generate().unwrap();
    let device = Keypair::generate().unwrap();
    let hello = DeviceHello {
        name: "a".into(),
        platform: "b".into(),
        ts: 1,
    };
    for psk in [Some(&PSK), None] {
        // Sanity: an honest forged answer completes the handshake.
        let (initiator, msg1) =
            Initiator::start(mode(psk), &device, gateway.public(), &hello).unwrap();
        let (e_secret, e) = honest_keys();
        let msg2 = forge_msg2(psk, &gateway, &msg1, &e, Some(&e_secret));
        initiator.finish(&msg2).unwrap();

        for point in low_order_points() {
            let (initiator, msg1) =
                Initiator::start(mode(psk), &device, gateway.public(), &hello).unwrap();
            let msg2 = forge_msg2(psk, &gateway, &msg1, &point, None);
            let err = initiator.finish(&msg2).unwrap_err();
            assert!(matches!(err, NoiseError::Handshake), "{err:?}");

            // Without the check, snow completes this handshake with known keys.
            let mut raw = snow_initiator(psk, &device, &gateway);
            let mut buf = vec![0; MAX_MESSAGE];
            let len = raw
                .write_message(&hello.encode().unwrap(), &mut buf)
                .unwrap();
            let msg2 = forge_msg2(psk, &gateway, &buf[..len], &point, None);
            assert!(raw.read_message(&msg2, &mut buf).is_ok());
        }
    }
}
