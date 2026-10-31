// Noise Protocol Framework (revision 34) state machines for exactly the two OmpDeck patterns:
// Noise_IK_25519_ChaChaPoly_BLAKE2s and Noise_IKpsk1_25519_ChaChaPoly_BLAKE2s.
// Primitives: X25519 (@noble/curves), ChaCha20-Poly1305 (@noble/ciphers), BLAKE2s + HMAC
// (@noble/hashes).

import { chacha20poly1305 } from '@noble/ciphers/chacha.js';
import { x25519 } from '@noble/curves/ed25519.js';
import { blake2s } from '@noble/hashes/blake2.js';
import { hmac } from '@noble/hashes/hmac.js';
import { randomBytes } from '@noble/hashes/utils.js';

import { type Bytes, concat, utf8 } from './bytes.ts';

export const DH_LEN = 32;
export const HASH_LEN = 32;
export const KEY_LEN = 32;
export const TAG_LEN = 16;
/** The Noise spec's message cap. */
export const MAX_MESSAGE = 65_535;

export type Pattern = 'IK' | 'IKpsk1';

export const PROTOCOL_NAMES: Readonly<Record<Pattern, string>> = {
  IK: 'Noise_IK_25519_ChaChaPoly_BLAKE2s',
  IKpsk1: 'Noise_IKpsk1_25519_ChaChaPoly_BLAKE2s',
};

const EMPTY: Bytes = new Uint8Array(0);
/** The nonce 2^64 − 1 that REKEY encrypts with. */
const MAX_NONCE: Bytes = (() => {
  const nonce = new Uint8Array(12);
  nonce.fill(0xff, 4);
  return nonce;
})();

function hash(data: Uint8Array): Bytes {
  return Uint8Array.from(blake2s(data));
}

/** HMAC-BLAKE2s into a buffer this module owns; the library's output is zeroed. */
function hmacHash(key: Uint8Array, data: Uint8Array): Bytes {
  const mac = hmac(blake2s, key, data);
  const out = Uint8Array.from(mac);
  mac.fill(0);
  return out;
}

/** HKDF of the Noise spec with BLAKE2s; returns 2 or 3 outputs. Intermediates are zeroed. */
function hkdf(chainingKey: Uint8Array, ikm: Uint8Array, outputs: 2 | 3): Bytes[] {
  const temp = hmacHash(chainingKey, ikm);
  const out1 = hmacHash(temp, Uint8Array.of(1));
  const input2 = concat(out1, Uint8Array.of(2));
  const out2 = hmacHash(temp, input2);
  input2.fill(0);
  const out = [out1, out2];
  if (outputs === 3) {
    const input3 = concat(out2, Uint8Array.of(3));
    out.push(hmacHash(temp, input3));
    input3.fill(0);
  }
  temp.fill(0);
  return out;
}

/** X25519 into a buffer this module owns; the library's output is zeroed. */
export function dh(secret: Uint8Array, publicKey: Uint8Array): Bytes {
  const shared = x25519.getSharedSecret(secret, publicKey);
  const out = Uint8Array.from(shared);
  shared.fill(0);
  return out;
}

export function publicKeyOf(secret: Uint8Array): Bytes {
  return Uint8Array.from(x25519.getPublicKey(secret));
}

export function randomKey(): Bytes {
  return Uint8Array.from(randomBytes(KEY_LEN));
}

/** Clamped scalar used to probe a key's order (a multiple of the cofactor 8). */
const LOW_ORDER_PROBE: Bytes = new Uint8Array(KEY_LEN).fill(0x5a);

/**
 * Whether an X25519 public key has small order (its order divides the cofactor 8), including
 * non-canonical encodings: every DH with it is a constant an attacker knows. Same rule as Rust
 * `deck_remote::noise::is_low_order`. noble refuses such keys itself (throws); this names it.
 */
export function isLowOrder(publicKey: Uint8Array): boolean {
  if (publicKey.length !== DH_LEN) throw new Error('X25519 public key must be 32 bytes');
  try {
    return x25519.scalarMult(LOW_ORDER_PROBE, publicKey).every((byte) => byte === 0);
  } catch {
    return true;
  }
}

/** Thrown when the peer's static or ephemeral key is a low-order point. */
export class LowOrderKeyError extends Error {
  constructor() {
    super('X25519 key of small order refused');
    this.name = 'LowOrderKeyError';
  }
}

/** Thrown when an AEAD tag does not verify. */
export class DecryptError extends Error {
  constructor() {
    super('Noise decryption failed');
    this.name = 'DecryptError';
  }
}

export class CipherState {
  #key: Bytes | null;
  #nonce = 0;
  #wiped = false;

  constructor(key: Uint8Array | null = null) {
    this.#key = key === null ? null : Uint8Array.from(key);
  }

  /** Zeroes the key; every later use throws. */
  wipe(): void {
    this.#key?.fill(0);
    this.#wiped = true;
  }

  /** @internal Test seam: the key buffer itself (not a copy), to observe {@link wipe}. */
  get keyBuffer(): Bytes | null {
    return this.#key;
  }

  hasKey(): boolean {
    return this.#key !== null;
  }

  get nonce(): number {
    return this.#nonce;
  }

  #nonceBytes(): Bytes {
    if (this.#wiped) throw new Error('cipher state was wiped');
    if (this.#nonce >= Number.MAX_SAFE_INTEGER) throw new Error('Noise nonce exhausted');
    const nonce = new Uint8Array(12);
    const view = new DataView(nonce.buffer);
    view.setUint32(4, this.#nonce >>> 0, true);
    view.setUint32(8, Math.floor(this.#nonce / 0x1_0000_0000), true);
    return nonce;
  }

  encryptWithAd(ad: Uint8Array, plaintext: Uint8Array): Bytes {
    if (this.#wiped) throw new Error('cipher state was wiped');
    if (this.#key === null) return Uint8Array.from(plaintext);
    const out = Uint8Array.from(chacha20poly1305(this.#key, this.#nonceBytes(), ad).encrypt(plaintext));
    this.#nonce++;
    return out;
  }

  /** Throws {@link DecryptError}; the nonce advances only on success. */
  decryptWithAd(ad: Uint8Array, ciphertext: Uint8Array): Bytes {
    if (this.#wiped) throw new Error('cipher state was wiped');
    if (this.#key === null) return Uint8Array.from(ciphertext);
    let out: Bytes;
    try {
      out = Uint8Array.from(chacha20poly1305(this.#key, this.#nonceBytes(), ad).decrypt(ciphertext));
    } catch {
      throw new DecryptError();
    }
    this.#nonce++;
    return out;
  }

  /** REKEY(): k = first 32 bytes of ENCRYPT(k, 2^64 − 1, ε, zeros). The nonce is kept. */
  rekey(): void {
    if (this.#key === null || this.#wiped) throw new Error('rekey without a key');
    const sealed = chacha20poly1305(this.#key, MAX_NONCE, EMPTY).encrypt(new Uint8Array(KEY_LEN));
    this.#key.fill(0);
    this.#key = Uint8Array.from(sealed.subarray(0, KEY_LEN));
  }
}

export class SymmetricState {
  #cipher = new CipherState();
  #chainingKey: Bytes;
  #hash: Bytes;

  constructor(protocolName: string) {
    const name = utf8(protocolName);
    if (name.length <= HASH_LEN) {
      this.#hash = new Uint8Array(HASH_LEN);
      this.#hash.set(name);
    } else {
      this.#hash = hash(name);
    }
    this.#chainingKey = Uint8Array.from(this.#hash);
  }

  get handshakeHash(): Bytes {
    return Uint8Array.from(this.#hash);
  }

  mixKey(ikm: Uint8Array): void {
    const [chainingKey, key] = hkdf(this.#chainingKey, ikm, 2);
    this.#replaceKeys(chainingKey!, key!);
  }

  #replaceKeys(chainingKey: Bytes, key: Bytes): void {
    this.#chainingKey.fill(0);
    this.#chainingKey = chainingKey;
    this.#cipher.wipe();
    this.#cipher = new CipherState(key);
    key.fill(0);
  }

  mixHash(data: Uint8Array): void {
    this.#hash = hash(concat(this.#hash, data));
  }

  mixKeyAndHash(ikm: Uint8Array): void {
    const [chainingKey, tempHash, key] = hkdf(this.#chainingKey, ikm, 3);
    this.mixHash(tempHash!);
    this.#replaceKeys(chainingKey!, key!);
  }

  encryptAndHash(plaintext: Uint8Array): Bytes {
    const ciphertext = this.#cipher.encryptWithAd(this.#hash, plaintext);
    this.mixHash(ciphertext);
    return ciphertext;
  }

  decryptAndHash(ciphertext: Uint8Array): Bytes {
    const plaintext = this.#cipher.decryptWithAd(this.#hash, ciphertext);
    this.mixHash(ciphertext);
    return plaintext;
  }

  hasKey(): boolean {
    return this.#cipher.hasKey();
  }

  split(): [CipherState, CipherState] {
    const [k1, k2] = hkdf(this.#chainingKey, EMPTY, 2);
    const out: [CipherState, CipherState] = [new CipherState(k1!), new CipherState(k2!)];
    k1!.fill(0);
    k2!.fill(0);
    return out;
  }

  /** Zeroes the chaining key and the handshake cipher key; the hash (public) is kept. */
  wipe(): void {
    this.#chainingKey.fill(0);
    this.#cipher.wipe();
  }

  /** @internal Test seam: the chaining key and cipher key buffers themselves. */
  get secretBuffers(): Bytes[] {
    const key = this.#cipher.keyBuffer;
    return key === null ? [this.#chainingKey] : [this.#chainingKey, key];
  }
}

export interface StaticKey {
  readonly secret: Uint8Array;
  readonly public: Uint8Array;
}

export interface HandshakeOptions {
  pattern: Pattern;
  initiator: boolean;
  prologue: Uint8Array;
  /** Local static key pair. */
  s: StaticKey;
  /** The responder's static public key (initiator only: IK pre-message). */
  rs?: Uint8Array;
  /** Required for IKpsk1. */
  psk?: Uint8Array;
  /** Fixed ephemeral secret, for test vectors only. */
  ephemeral?: Uint8Array;
}

/**
 * IK / IKpsk1: `<- s`, `-> e, es, s, ss [, psk]`, `<- e, ee, se`.
 *
 * It owns copies of its secrets (static secret, ephemeral secret, PSK, chaining key, handshake
 * cipher key) and zeroes them in {@link wipe}: automatically after {@link split} and when a
 * message fails, otherwise by the caller abandoning the handshake. DH and HKDF intermediates are
 * zeroed as soon as they are mixed in.
 */
export class HandshakeState {
  readonly #initiator: boolean;
  readonly #psk: Bytes | null;
  readonly #s: { readonly secret: Bytes; readonly public: Bytes };
  readonly #fixedEphemeral: Bytes | null;
  readonly #symmetric: SymmetricState;
  #rs: Bytes | null;
  #re: Bytes | null = null;
  #e: { secret: Bytes; public: Bytes } | null = null;
  #step = 0;
  #wiped = false;

  constructor(options: HandshakeOptions) {
    this.#initiator = options.initiator;
    if (options.pattern === 'IKpsk1' && options.psk?.length !== KEY_LEN) {
      throw new Error('IKpsk1 needs a 32-byte PSK');
    }
    if (options.pattern === 'IK' && options.psk !== undefined) throw new Error('IK takes no PSK');
    if (options.initiator && options.rs?.length !== DH_LEN) {
      throw new Error("the initiator needs the responder's static key");
    }
    if (!options.initiator && options.rs !== undefined) {
      throw new Error("the responder learns the initiator's static key from message 1");
    }
    if (options.rs !== undefined && isLowOrder(options.rs)) throw new LowOrderKeyError();
    // Secrets are copied only once nothing can throw: a later `wipe()` of the caller's buffers
    // cannot change this handshake, and this handshake's `wipe()` reaches every copy.
    this.#psk = options.pattern === 'IKpsk1' ? Uint8Array.from(options.psk!) : null;
    this.#s = { secret: Uint8Array.from(options.s.secret), public: Uint8Array.from(options.s.public) };
    this.#rs = options.rs === undefined ? null : Uint8Array.from(options.rs);
    this.#fixedEphemeral = options.ephemeral === undefined ? null : Uint8Array.from(options.ephemeral);
    this.#symmetric = new SymmetricState(PROTOCOL_NAMES[options.pattern]);
    this.#symmetric.mixHash(options.prologue);
    this.#symmetric.mixHash(options.initiator ? this.#rs! : this.#s.public);
  }

  /**
   * Zeroes every secret this handshake owns; every later use throws. Idempotent. Called by
   * {@link split} and on a failed message; call it when abandoning the handshake.
   */
  wipe(): void {
    this.#wiped = true;
    this.#s.secret.fill(0);
    this.#e?.secret.fill(0);
    this.#fixedEphemeral?.fill(0);
    this.#psk?.fill(0);
    this.#symmetric.wipe();
  }

  /** @internal Test seam: the secret buffers themselves (not copies), to observe {@link wipe}. */
  get secretBuffers(): Bytes[] {
    const out = [this.#s.secret, ...this.#symmetric.secretBuffers];
    if (this.#e !== null) out.push(this.#e.secret);
    if (this.#fixedEphemeral !== null) out.push(this.#fixedEphemeral);
    if (this.#psk !== null) out.push(this.#psk);
    return out;
  }

  /** Runs one step; any failure wipes the handshake. */
  #guarded<T>(run: () => T): T {
    if (this.#wiped) throw new Error('handshake was wiped');
    try {
      return run();
    } catch (error) {
      this.wipe();
      throw error;
    }
  }

  /** Mixes DH(`secret`, `publicKey`) into the chaining key, then zeroes the shared secret. */
  #mixDh(secret: Uint8Array, publicKey: Uint8Array): void {
    const shared = dh(secret, publicKey);
    this.#symmetric.mixKey(shared);
    shared.fill(0);
  }

  get finished(): boolean {
    return this.#step === 2;
  }

  /** Whose turn it is to write. */
  get myTurn(): boolean {
    return !this.finished && (this.#step === 0) === this.#initiator;
  }

  get handshakeHash(): Bytes {
    return this.#symmetric.handshakeHash;
  }

  get remoteStatic(): Bytes | null {
    return this.#rs === null ? null : Uint8Array.from(this.#rs);
  }

  #writeE(): Bytes {
    const secret = this.#fixedEphemeral ?? randomKey();
    this.#e = { secret, public: publicKeyOf(secret) };
    this.#symmetric.mixHash(this.#e.public);
    if (this.#psk !== null) this.#symmetric.mixKey(this.#e.public);
    return this.#e.public;
  }

  #readE(message: Uint8Array, offset: number): void {
    this.#re = message.slice(offset, offset + DH_LEN);
    if (isLowOrder(this.#re)) throw new LowOrderKeyError();
    this.#symmetric.mixHash(this.#re);
    if (this.#psk !== null) this.#symmetric.mixKey(this.#re);
  }

  writeMessage(payload: Uint8Array): Bytes {
    return this.#guarded(() => {
      if (!this.myTurn) throw new Error('not our turn to write');
      const sym = this.#symmetric;
      const parts: Uint8Array[] = [];
      if (this.#step === 0) {
        parts.push(this.#writeE());
        this.#mixDh(this.#e!.secret, this.#rs!); // es
        parts.push(sym.encryptAndHash(this.#s.public)); // s
        this.#mixDh(this.#s.secret, this.#rs!); // ss
        if (this.#psk !== null) sym.mixKeyAndHash(this.#psk);
      } else {
        parts.push(this.#writeE());
        this.#mixDh(this.#e!.secret, this.#re!); // ee
        this.#mixDh(this.#e!.secret, this.#rs!); // se (responder side)
      }
      parts.push(sym.encryptAndHash(payload));
      this.#step++;
      const message = concat(...parts);
      if (message.length > MAX_MESSAGE) throw new Error('Noise message too large');
      return message;
    });
  }

  /** Throws {@link DecryptError} on authentication failure (and wipes the handshake). */
  readMessage(message: Uint8Array): Bytes {
    return this.#guarded(() => {
      if (this.myTurn || this.finished) throw new Error('not our turn to read');
      if (message.length > MAX_MESSAGE) throw new Error('Noise message too large');
      const sym = this.#symmetric;
      let offset = 0;
      if (this.#step === 0) {
        if (message.length < DH_LEN + DH_LEN + TAG_LEN + TAG_LEN) throw new DecryptError();
        this.#readE(message, 0);
        this.#mixDh(this.#s.secret, this.#re!); // es
        this.#rs = sym.decryptAndHash(message.subarray(DH_LEN, DH_LEN + DH_LEN + TAG_LEN)); // s
        if (isLowOrder(this.#rs)) throw new LowOrderKeyError();
        this.#mixDh(this.#s.secret, this.#rs); // ss
        if (this.#psk !== null) sym.mixKeyAndHash(this.#psk);
        offset = DH_LEN + DH_LEN + TAG_LEN;
      } else {
        if (message.length < DH_LEN + TAG_LEN) throw new DecryptError();
        this.#readE(message, 0);
        this.#mixDh(this.#e!.secret, this.#re!); // ee
        this.#mixDh(this.#s.secret, this.#re!); // se (initiator side)
        offset = DH_LEN;
      }
      const payload = sym.decryptAndHash(message.subarray(offset));
      this.#step++;
      return payload;
    });
  }

  /**
   * After the handshake: `[send, receive]` cipher states for this side. Wipes the handshake;
   * {@link handshakeHash} stays readable.
   */
  split(): [CipherState, CipherState] {
    return this.#guarded(() => {
      if (!this.finished) throw new Error('handshake not finished');
      const [c1, c2] = this.#symmetric.split();
      this.wipe();
      return this.#initiator ? [c1, c2] : [c2, c1];
    });
  }
}
