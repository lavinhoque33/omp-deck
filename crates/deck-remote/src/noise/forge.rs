//! A minimal hand-written Noise IK/IKpsk1 peer for tests: it forges handshake messages with
//! keys `snow` would never emit (low-order points) and payloads this crate would never encode.
//! Shared by the vector generator (`noise::vectors`) and `tests/low_order.rs` (`#[path]`).

use blake2::{Blake2s256, Digest};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use curve25519_dalek::montgomery::MontgomeryPoint;

use super::{Keypair, PAIRING_PATTERN, PROLOGUE, Psk, RECONNECT_PATTERN};

const ZERO: [u8; 32] = [0; 32];

fn from_hex(text: &str) -> [u8; 32] {
    std::array::from_fn(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).unwrap())
}

/// Every small-order point encoding class: 0, 1, p − 1, both order-8 points, the non-canonical
/// p and p + 1, and a canonical one with the (ignored) top bit set.
pub fn low_order_points() -> Vec<[u8; 32]> {
    let mut with_top_bit = ZERO;
    with_top_bit[31] = 0x80;
    let mut one = ZERO;
    one[0] = 1;
    vec![
        ZERO,
        one,
        from_hex("ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f"),
        from_hex("e0eb7a7c3b41b8ae1656e3faf19fc46ada098deb9c32b1fd866205165f49b800"),
        from_hex("5f9c95bca3508c24b1d0b1559c83ef5b04445cc4581c8e86d8224eddd09f1157"),
        from_hex("edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f"),
        from_hex("eeffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f"),
        with_top_bit,
    ]
}

/// X25519 with a clamped scalar (all zeros for a low-order public key).
pub fn dh(secret: &[u8; 32], public: &[u8; 32]) -> [u8; 32] {
    MontgomeryPoint(*public).mul_clamped(*secret).to_bytes()
}

pub fn pattern(psk: Option<&Psk>) -> &'static str {
    if psk.is_some() {
        PAIRING_PATTERN
    } else {
        RECONNECT_PATTERN
    }
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

/// Message 1 from a forging device carrying `payload`: `e`/`s` public keys of its choice, with
/// the DH results the responder will compute (all zeros for a low-order key).
pub fn forge_msg1(
    psk: Option<&Psk>,
    gateway: &[u8; 32],
    e: &[u8; 32],
    es: &[u8; 32],
    s: &[u8; 32],
    ss: &[u8; 32],
    payload: &[u8],
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
    out.extend(sym.encrypt_and_hash(payload));
    out
}

/// A forging gateway: reads the device's message 1 and answers `payload` with `e` of its
/// choice. `e_secret` is `e`'s secret, or `None` for a low-order `e` (the initiator's `ee` and
/// `se` are then both zero).
pub fn forge_msg2(
    psk: Option<&Psk>,
    gateway: &Keypair,
    msg1: &[u8],
    e: &[u8; 32],
    e_secret: Option<&[u8; 32]>,
    payload: &[u8],
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
    match e_secret {
        Some(e_secret) => {
            sym.mix_key(&dh(e_secret, &re));
            sym.mix_key(&dh(e_secret, &rs));
        }
        None => {
            sym.mix_key(&ZERO);
            sym.mix_key(&ZERO);
        }
    }
    out.extend(sym.encrypt_and_hash(payload));
    out
}
