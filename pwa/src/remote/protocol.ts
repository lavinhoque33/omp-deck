// The OmpDeck device protocol, mirroring Rust `deck_remote::noise` (docs/notes/mobile-relay.md,
// "R1 contract"). The device is the Noise initiator; `respond`/`Pending` mirror the gateway side
// for cross-tests.
//
// Secrets in memory. What is zeroed:
// - `Keypair.wipe()`: the key pair's secret (`secret`/`secretBytes()` hand out copies the caller
//   must wipe itself).
// - The handshake (`noise.ts` `HandshakeState`): its own copies of the static secret, the PSK and
//   the ephemeral secret, the chaining key and the handshake cipher key, once `split()` has made
//   the transport ciphers, when a step fails, and on `Initiator.abandon()`/`Pending.abandon()`.
//   Every DH output and HKDF intermediate is zeroed as soon as it is mixed in; superseded
//   chaining and cipher keys are zeroed when replaced, old transport keys on every rekey.
// What cannot be: copies the engine or libraries make (`@noble` internal hash/cipher/curve
// state, JIT and garbage-collector copies), secrets held in strings (the invite text), the
// caller's own buffers (the `Mode` PSK, invite fields), and the live transport keys of a
// `Session` until it is collected. Wiping is therefore best effort; the device key's real
// protection is the storage it is kept in.

import { blake2s } from '@noble/hashes/blake2.js';

import {
  type Bytes,
  concat,
  fromBase64Url,
  fromUtf8,
  toBase64Url,
  toHex,
  utf8,
} from './bytes.ts';
import {
  CipherState,
  DecryptError,
  HandshakeState,
  isLowOrder,
  KEY_LEN,
  MAX_MESSAGE,
  type Pattern,
  publicKeyOf,
  randomKey,
  TAG_LEN,
} from './noise.ts';
import { parsePayload, type PayloadValue } from './payload.ts';
import { ROOM_LEN, TOKEN_LEN } from './relay.ts';

export const PROLOGUE: Bytes = utf8('ompdeck-remote-v1');
export const PAIRING_PATTERN = 'Noise_IKpsk1_25519_ChaChaPoly_BLAKE2s';
export const RECONNECT_PATTERN = 'Noise_IK_25519_ChaChaPoly_BLAKE2s';

export const PSK_LEN = 32;
export const HASH_LEN = 32;
/** Application bytes per transport frame: the message cap minus the tag and the flags byte. */
export const MAX_FRAGMENT_DATA = MAX_MESSAGE - TAG_LEN - 1;
export const MAX_HANDSHAKE_PAYLOAD = 1024;
export const MAX_MSG1 = KEY_LEN + KEY_LEN + TAG_LEN + MAX_HANDSHAKE_PAYLOAD + TAG_LEN;
export const MAX_MSG2 = KEY_LEN + MAX_HANDSHAKE_PAYLOAD + TAG_LEN;

export const MAX_NAME_CHARS = 64;
export const MAX_PLATFORM_CHARS = 32;
export const MAX_DEVICE_ID_CHARS = 64;
export const MAX_GATEWAY_LABEL_CHARS = 64;
export const MAX_TIMESTAMP = Number.MAX_SAFE_INTEGER;
/** {@link DeviceHello.features} names at most. */
export const MAX_FEATURES = 32;

export const FLAG_MORE = 0x01;
export const FLAG_REKEY = 0x02;
const KNOWN_FLAGS = FLAG_MORE | FLAG_REKEY;

export const DEFAULT_REKEY_MESSAGES = 2 ** 20;
export const DEFAULT_REKEY_INTERVAL_MS = 30 * 60 * 1000;
export const DEFAULT_MAX_REASSEMBLY = 16 * 1024 * 1024;

export const INVITE_PREFIX = 'v1.';
export const MAX_INVITE_URL = 255;

export type NoiseErrorCode =
  | 'message-size'
  | 'handshake'
  | 'payload'
  | 'unsupported-version'
  | 'frame-size'
  | 'decrypt'
  | 'bad-flags'
  | 'too-large'
  | 'poisoned'
  | 'invite';

/** Every failure of this module; never carries secrets. */
export class NoiseError extends Error {
  readonly code: NoiseErrorCode;

  constructor(code: NoiseErrorCode, message: string) {
    super(message);
    this.name = 'NoiseError';
    this.code = code;
  }
}

/**
 * An X25519 static key pair. `wipe()` overwrites the secret once it is no longer needed (best
 * effort, see the module comment); `secret` and `secretBytes()` hand out copies, so a caller can
 * neither wipe nor alter the key pair's own secret through them.
 */
export class Keypair {
  readonly public: Bytes;
  readonly #secret: Bytes;

  private constructor(secret: Bytes) {
    this.#secret = secret;
    this.public = publicKeyOf(secret);
  }

  static generate(): Keypair {
    return new Keypair(randomKey());
  }

  /** Restores a stored key pair from its secret bytes. */
  static fromSecret(secret: Uint8Array): Keypair {
    if (secret.length !== KEY_LEN) throw new NoiseError('payload', 'secret key must be 32 bytes');
    return new Keypair(Uint8Array.from(secret));
  }

  /** The secret bytes, for storage only. */
  secretBytes(): Bytes {
    return Uint8Array.from(this.#secret);
  }

  wipe(): void {
    this.#secret.fill(0);
  }

  get secret(): Bytes {
    return Uint8Array.from(this.#secret);
  }
}

/**
 * The first 10 bytes of BLAKE2s-256(public) as hex in five dash-separated groups of 4.
 */
export function fingerprint(publicKey: Uint8Array): string {
  const hex = toHex(blake2s(publicKey).subarray(0, 10));
  return [0, 4, 8, 12, 16].map((i) => hex.slice(i, i + 4)).join('-');
}

/** Whether a device timestamp may be accepted after the last one seen from that device. */
export function timestampFresh(last: number, ts: number): boolean {
  return ts > last;
}

/** The device's next timestamp: the current time, but always after the last one sent. */
export function nextTimestamp(last: number, nowMs: number): number {
  return Math.max(nowMs, last + 1);
}

/**
 * Whether device-supplied text (a hello name or platform, a device rename) is safe to show: no
 * C0/C1 control character, and none of the invisible, separator and bidirectional formatting
 * characters U+00AD, U+061C, U+180E, U+200B–U+200F, U+2028–U+202E, U+2060–U+2064,
 * U+2066–U+206F, U+FEFF and U+FFF9–U+FFFB. The same explicit list as Rust's `is_display_safe`.
 */
export function isDisplaySafe(text: string): boolean {
  return !UNSAFE_TEXT.test(text);
}

const UNSAFE_TEXT =
  /[\u0000-\u001f\u007f-\u009f\u00ad\u061c\u180e\u200b-\u200f\u2028-\u202e\u2060-\u2064\u2066-\u206f\ufeff\ufff9-\ufffb]/u;

export interface DeviceHello {
  /** At most {@link MAX_NAME_CHARS}, {@link isDisplaySafe}. */
  name: string;
  /** At most {@link MAX_PLATFORM_CHARS}, {@link isDisplaySafe}. */
  platform: string;
  /** Unix milliseconds, strictly increasing per device. */
  ts: number;
  /** The highest application protocol the device speaks, at least 1; absent means 1. */
  api?: number;
  /**
   * Optional application features the device understands: at most {@link MAX_FEATURES}
   * distinct non-empty names of `[a-z0-9_.]`; absent means none.
   */
  features?: readonly string[];
}

/** A decoded {@link DeviceHello}: the optional keys with their defaults filled in. */
export type ReceivedHello = Required<DeviceHello>;

export interface Welcome {
  device: string;
  gateway: string;
  /** The device's relay admission token: present exactly in pairing. */
  token?: Bytes;
}

function checkChars(text: unknown, max: number, what: string): string {
  // Code points, like Rust's `chars()`; lone surrogates cannot cross to Rust.
  if (typeof text !== 'string' || !text.isWellFormed() || [...text].length > max) {
    throw new NoiseError('payload', what);
  }
  return text;
}

function checkPayloadLen(bytes: Uint8Array): void {
  if (bytes.length > MAX_HANDSHAKE_PAYLOAD) {
    throw new NoiseError('payload', 'payload exceeds 1024 bytes');
  }
}

/**
 * Checks the size and grammar of a handshake payload and its version: an integer `v` other than
 * 1 is `unsupported-version`, a missing or string `v` a payload error.
 */
function parseObject(bytes: Uint8Array, what: string): Map<string, PayloadValue> {
  checkPayloadLen(bytes);
  const fields = parsePayload(bytes);
  const v = fields?.get('v');
  if (fields === null || typeof v !== 'number') throw new NoiseError('payload', `${what} is not valid`);
  if (v !== 1) throw new NoiseError('unsupported-version', `unsupported ${what} version ${v}`);
  return fields;
}

function takeStr(fields: Map<string, PayloadValue>, key: string, what: string): string {
  const value = fields.get(key);
  if (typeof value !== 'string') throw new NoiseError('payload', `${what} is not valid`);
  return value;
}

function validTimestamp(ts: unknown): number {
  if (typeof ts !== 'number' || !Number.isSafeInteger(ts) || ts < 0) {
    throw new NoiseError('payload', 'timestamp out of range');
  }
  return ts;
}

const FEATURE_NAME = /^[a-z0-9_.]+$/;

/** Validates a hello, as Rust `DeviceHello::validate`; returns it with its defaults. */
function checkHello(hello: DeviceHello): ReceivedHello {
  const name = checkChars(hello.name, MAX_NAME_CHARS, 'device name too long');
  const platform = checkChars(hello.platform, MAX_PLATFORM_CHARS, 'platform too long');
  if (!isDisplaySafe(name)) throw new NoiseError('payload', 'device name is not display-safe');
  if (!isDisplaySafe(platform)) throw new NoiseError('payload', 'platform is not display-safe');
  const ts = validTimestamp(hello.ts);
  const api = hello.api ?? 1;
  if (!Number.isSafeInteger(api) || api < 1) throw new NoiseError('payload', 'api version out of range');
  const features = hello.features ?? [];
  if (features.length > MAX_FEATURES) throw new NoiseError('payload', 'too many features');
  if (features.some((f, i) => !FEATURE_NAME.test(f) || features.indexOf(f) !== i)) {
    throw new NoiseError('payload', 'features are not valid');
  }
  return { name, platform, ts, api, features: [...features] };
}

/** Writes `api` only when not 1 and `features` (comma-joined) only when not empty, like Rust. */
export function encodeDeviceHello(hello: DeviceHello): Bytes {
  const { name, platform, ts, api, features } = checkHello(hello);
  const out = utf8(
    JSON.stringify({
      v: 1,
      name,
      platform,
      ts,
      ...(api === 1 ? {} : { api }),
      ...(features.length === 0 ? {} : { features: features.join(',') }),
    }),
  );
  checkPayloadLen(out);
  return out;
}

export function decodeDeviceHello(bytes: Uint8Array): ReceivedHello {
  const fields = parseObject(bytes, 'hello');
  const ts = fields.get('ts');
  const api = fields.get('api') ?? 1;
  const features = fields.get('features');
  if (typeof ts !== 'number' || typeof api !== 'number' || typeof features === 'number') {
    throw new NoiseError('payload', 'hello is not valid');
  }
  return checkHello({
    name: takeStr(fields, 'name', 'hello'),
    platform: takeStr(fields, 'platform', 'hello'),
    ts,
    api,
    features: features === undefined ? [] : features.split(','),
  });
}

export function encodeWelcome(welcome: Welcome): Bytes {
  const device = checkChars(welcome.device, MAX_DEVICE_ID_CHARS, 'device id too long');
  const gateway = checkChars(welcome.gateway, MAX_GATEWAY_LABEL_CHARS, 'gateway label too long');
  if (welcome.token !== undefined && welcome.token.length !== TOKEN_LEN) {
    throw new NoiseError('payload', 'welcome token has the wrong length');
  }
  const body =
    welcome.token === undefined
      ? { v: 1, device, gateway }
      : { v: 1, device, gateway, token: toBase64Url(welcome.token) };
  const out = utf8(JSON.stringify(body));
  checkPayloadLen(out);
  return out;
}

export function decodeWelcome(bytes: Uint8Array): Welcome {
  const fields = parseObject(bytes, 'welcome');
  const device = takeStr(fields, 'device', 'welcome');
  const gateway = takeStr(fields, 'gateway', 'welcome');
  const welcome: Welcome = {
    device: checkChars(device, MAX_DEVICE_ID_CHARS, 'device id too long'),
    gateway: checkChars(gateway, MAX_GATEWAY_LABEL_CHARS, 'gateway label too long'),
  };
  const raw = fields.get('token');
  if (raw !== undefined) {
    const token = typeof raw === 'string' ? fromBase64Url(raw) : null;
    if (token === null) throw new NoiseError('payload', 'welcome token is not base64url');
    if (token.length !== TOKEN_LEN) throw new NoiseError('payload', 'welcome token has the wrong length');
    welcome.token = token;
  }
  return welcome;
}

function checkWelcomeMode(welcome: Welcome, pairing: boolean): void {
  if (pairing && welcome.token === undefined) {
    throw new NoiseError('payload', 'pairing welcome lacks a token');
  }
  if (!pairing && welcome.token !== undefined) {
    throw new NoiseError('payload', 'reconnect welcome carries a token');
  }
}

/** Which handshake to run: pairing (IKpsk1 with the invite's PSK) or reconnect (IK). */
export type Mode = { kind: 'pairing'; psk: Uint8Array } | { kind: 'reconnect' };

function handshake(
  mode: Mode,
  local: Keypair,
  remote: Uint8Array | undefined,
  ephemeral: Uint8Array | undefined,
): HandshakeState {
  const pattern: Pattern = mode.kind === 'pairing' ? 'IKpsk1' : 'IK';
  if (mode.kind === 'pairing' && mode.psk.length !== PSK_LEN) {
    throw new NoiseError('payload', 'PSK must be 32 bytes');
  }
  if (remote !== undefined && remote.length !== KEY_LEN) {
    throw new NoiseError('payload', 'gateway public key must be 32 bytes');
  }
  if (remote !== undefined && isLowOrder(remote)) {
    throw new NoiseError('handshake', 'gateway public key is a low-order point');
  }
  return new HandshakeState({
    pattern,
    initiator: remote !== undefined,
    prologue: PROLOGUE,
    s: local,
    ...(remote === undefined ? {} : { rs: remote }),
    ...(mode.kind === 'pairing' ? { psk: mode.psk } : {}),
    ...(ephemeral === undefined ? {} : { ephemeral }),
  });
}

function intoSession(state: HandshakeState): Session {
  const [send, receive] = state.split();
  return new Session(send, receive, state.handshakeHash);
}

/** Runs a handshake step; any failure wipes the handshake's secrets before it propagates. */
function wipingOnError<T>(state: HandshakeState, run: () => T): T {
  try {
    return run();
  } catch (error) {
    state.wipe();
    throw error;
  }
}

/** The device side between sending message 1 and receiving message 2. */
export class Initiator {
  readonly #state: HandshakeState;
  readonly #pairing: boolean;

  /** @internal Use {@link Initiator.start}. */
  constructor(state: HandshakeState, pairing: boolean) {
    this.#state = state;
    this.#pairing = pairing;
  }

  /** Starts a handshake with the gateway whose static key is `gatewayPublic`. */
  static start(
    mode: Mode,
    device: Keypair,
    gatewayPublic: Uint8Array,
    hello: DeviceHello,
  ): { initiator: Initiator; msg1: Bytes } {
    return startInitiator(mode, device, gatewayPublic, hello, undefined);
  }

  /** Completes the handshake with the gateway's message 2. One use only; a failure wipes it. */
  finish(msg2: Uint8Array): { session: Session; welcome: Welcome } {
    return wipingOnError(this.#state, () => {
      if (msg2.length > MAX_MSG2 || msg2.length < KEY_LEN + TAG_LEN) {
        throw new NoiseError('message-size', `handshake message of ${msg2.length} bytes is malformed`);
      }
      let payload: Bytes;
      try {
        payload = this.#state.readMessage(msg2);
      } catch {
        throw new NoiseError('handshake', 'handshake message failed authentication');
      }
      const welcome = decodeWelcome(payload);
      checkWelcomeMode(welcome, this.#pairing);
      return { session: intoSession(this.#state), welcome };
    });
  }

  /** Gives up before message 2 (connection lost, timeout): wipes the handshake's secrets. */
  abandon(): void {
    this.#state.wipe();
  }
}

/**
 * @internal Test seam behind {@link Initiator.start}: `ephemeral` fixes the ephemeral key
 * (vectors only). Not exported from the package index.
 */
export function startInitiator(
  mode: Mode,
  device: Keypair,
  gatewayPublic: Uint8Array,
  hello: DeviceHello,
  ephemeral: Uint8Array | undefined,
): { initiator: Initiator; msg1: Bytes } {
  const payload = encodeDeviceHello(hello);
  const state = handshake(mode, device, gatewayPublic, ephemeral);
  const msg1 = state.writeMessage(payload);
  return { initiator: new Initiator(state, mode.kind === 'pairing'), msg1 };
}

/** A verified message 1 awaiting the gateway's decision. */
export class Pending {
  readonly devicePublic: Bytes;
  readonly hello: ReceivedHello;
  readonly #state: HandshakeState;
  readonly #pairing: boolean;

  constructor(state: HandshakeState, devicePublic: Bytes, hello: ReceivedHello, pairing: boolean) {
    this.#state = state;
    this.devicePublic = devicePublic;
    this.hello = hello;
    this.#pairing = pairing;
  }

  /** Answers with message 2. `welcome.token` must be present exactly in pairing. One use only. */
  accept(welcome: Welcome): { session: Session; msg2: Bytes } {
    return wipingOnError(this.#state, () => {
      checkWelcomeMode(welcome, this.#pairing);
      const msg2 = this.#state.writeMessage(encodeWelcome(welcome));
      return { session: intoSession(this.#state), msg2 };
    });
  }

  /** Refuses the device without answering: wipes the handshake's secrets. */
  abandon(): void {
    this.#state.wipe();
  }
}

/** Reads a device's message 1 (gateway side). */
export function respond(mode: Mode, gateway: Keypair, msg1: Uint8Array): Pending {
  return respondWith(mode, gateway, msg1, undefined);
}

/** @internal Test seam behind {@link respond}: `ephemeral` fixes the ephemeral key (vectors only). */
export function respondWith(
  mode: Mode,
  gateway: Keypair,
  msg1: Uint8Array,
  ephemeral: Uint8Array | undefined,
): Pending {
  if (msg1.length > MAX_MSG1 || msg1.length < KEY_LEN + KEY_LEN + TAG_LEN + TAG_LEN) {
    throw new NoiseError('message-size', `handshake message of ${msg1.length} bytes is malformed`);
  }
  const state = handshake(mode, gateway, undefined, ephemeral);
  let payload: Bytes;
  try {
    payload = state.readMessage(msg1);
  } catch {
    throw new NoiseError('handshake', 'handshake message failed authentication');
  }
  const hello = wipingOnError(state, () => decodeDeviceHello(payload));
  return new Pending(state, state.remoteStatic!, hello, mode.kind === 'pairing');
}

export interface SessionConfig {
  /** Rekey the sending cipher after this many frames (the last one carries FLAG_REKEY). */
  rekeyMessages: number;
  /** Rekey on the first frame sealed this long after the last rekey. */
  rekeyIntervalMs: number;
  /** Largest application message sealed or reassembled. */
  maxMessage: number;
}

export const DEFAULT_SESSION_CONFIG: Readonly<SessionConfig> = {
  rekeyMessages: DEFAULT_REKEY_MESSAGES,
  rekeyIntervalMs: DEFAULT_REKEY_INTERVAL_MS,
  maxMessage: DEFAULT_MAX_REASSEMBLY,
};

/**
 * An established session. After any error it is poisoned: every later call throws
 * `NoiseError('poisoned')` and the connection must be closed. Clock: `performance.now()` ms.
 */
export class Session {
  config: SessionConfig = { ...DEFAULT_SESSION_CONFIG };
  readonly #send: CipherState;
  readonly #receive: CipherState;
  readonly #handshakeHash: Bytes;
  #sentSinceRekey = 0;
  #lastRekey: number;
  /** Non-final fragments of the message being reassembled: each exactly MAX_FRAGMENT_DATA bytes. */
  #parts: Bytes[] = [];
  #poisoned = false;

  constructor(send: CipherState, receive: CipherState, handshakeHash: Bytes, now = performance.now()) {
    this.#send = send;
    this.#receive = receive;
    this.#handshakeHash = handshakeHash;
    this.#lastRekey = now;
  }

  /** The session id, equal on both sides. */
  get handshakeHash(): Bytes {
    return Uint8Array.from(this.#handshakeHash);
  }

  get poisoned(): boolean {
    return this.#poisoned;
  }

  #guard<T>(run: () => T): T {
    if (this.#poisoned) throw new NoiseError('poisoned', 'the session failed earlier and is unusable');
    try {
      return run();
    } catch (error) {
      this.#poisoned = true;
      this.#parts = [];
      if (error instanceof NoiseError) throw error;
      if (error instanceof DecryptError) {
        throw new NoiseError('decrypt', 'transport frame failed authentication');
      }
      throw new NoiseError('decrypt', `Noise transport failed: ${String(error)}`);
    }
  }

  /** Encrypts one application message into one or more frames (one relay payload each). */
  seal(message: Uint8Array, now = performance.now()): Bytes[] {
    return this.#guard(() => {
      if (message.length > this.config.maxMessage) {
        throw new NoiseError(
          'too-large',
          `message of ${message.length} bytes exceeds the ${this.config.maxMessage}-byte limit`,
        );
      }
      const count = Math.max(1, Math.ceil(message.length / MAX_FRAGMENT_DATA));
      const frames: Bytes[] = [];
      for (let index = 0; index < count; index++) {
        const chunk = message.subarray(index * MAX_FRAGMENT_DATA, (index + 1) * MAX_FRAGMENT_DATA);
        const rekey =
          this.#sentSinceRekey + 1 >= this.config.rekeyMessages ||
          now - this.#lastRekey >= this.config.rekeyIntervalMs;
        const flags = (index + 1 < count ? FLAG_MORE : 0) | (rekey ? FLAG_REKEY : 0);
        frames.push(this.#send.encryptWithAd(new Uint8Array(0), concat(Uint8Array.of(flags), chunk)));
        if (rekey) {
          this.#send.rekey();
          this.#sentSinceRekey = 0;
          this.#lastRekey = now;
        } else {
          this.#sentSinceRekey++;
        }
      }
      return frames;
    });
  }

  /** Decrypts one frame; returns the application message once its last fragment arrived. */
  open(frame: Uint8Array): Bytes | null {
    return this.#guard(() => {
      if (frame.length > MAX_MESSAGE || frame.length <= TAG_LEN) {
        throw new NoiseError('frame-size', `transport frame of ${frame.length} bytes is malformed`);
      }
      const plain = this.#receive.decryptWithAd(new Uint8Array(0), frame);
      const flags = plain[0]!;
      if ((flags & ~KNOWN_FLAGS) !== 0) {
        throw new NoiseError('bad-flags', `transport frame has unknown flag bits 0x${flags.toString(16)}`);
      }
      if ((flags & FLAG_REKEY) !== 0) this.#receive.rekey();
      const data = plain.subarray(1);
      const more = (flags & FLAG_MORE) !== 0;
      // Every non-final fragment is full, as `seal` makes them: this bounds the fragments of
      // one message by the reassembly cap.
      if (more && data.length !== MAX_FRAGMENT_DATA) {
        throw new NoiseError('frame-size', `transport frame of ${frame.length} bytes is malformed`);
      }
      const size = this.#parts.length * MAX_FRAGMENT_DATA + data.length;
      if (size > this.config.maxMessage) {
        throw new NoiseError(
          'too-large',
          `message of ${size} bytes exceeds the ${this.config.maxMessage}-byte limit`,
        );
      }
      if (more) {
        this.#parts.push(data);
        return null;
      }
      if (this.#parts.length === 0) return data;
      const whole = new Uint8Array(size);
      let offset = 0;
      for (const part of this.#parts) {
        whole.set(part, offset);
        offset += MAX_FRAGMENT_DATA;
      }
      whole.set(data, offset);
      this.#parts = [];
      return whole;
    });
  }
}

/**
 * The QR code's secret: `v1.` + base64url(`url_len u8 ‖ url ‖ room 16 ‖ pairing token 32 ‖
 * gateway public 32 ‖ psk 32`).
 */
export interface PairingInvite {
  /** The canonical relay base URL, see {@link checkInviteUrl}. */
  url: string;
  room: Bytes;
  token: Bytes;
  gatewayPublic: Bytes;
  psk: Bytes;
}

const INVITE_FIXED = ROOM_LEN + TOKEN_LEN + KEY_LEN + PSK_LEN;

/**
 * Reads IP addresses exactly as Rust's `std::net::Ipv4Addr`/`Ipv6Addr` parsers do (so both sides
 * accept the same invite URLs): decimal octets of 1–3 digits ≤ 255 without a leading zero; hex
 * groups of 1–4 digits; one `::` standing for at least one zero group; an embedded IPv4 address
 * only as the last two groups.
 */
class AddressReader {
  #pos = 0;
  readonly #text: string;

  constructor(text: string) {
    this.#text = text;
  }

  /** Runs `read`; on `null` puts the position back. */
  #attempt<T>(read: () => T | null): T | null {
    const start = this.#pos;
    const out = read();
    if (out === null) this.#pos = start;
    return out;
  }

  #char(c: string): boolean {
    if (this.#text[this.#pos] !== c) return false;
    this.#pos++;
    return true;
  }

  #number(radix: 10 | 16, maxDigits: number, allowZeroPrefix: boolean, max: number): number | null {
    return this.#attempt(() => {
      const leadingZero = this.#text[this.#pos] === '0';
      let value = 0;
      let digits = 0;
      while (digits < maxDigits) {
        const c = this.#text[this.#pos];
        const digit = c === undefined || !/^[0-9a-fA-F]$/.test(c) ? NaN : parseInt(c, radix);
        if (Number.isNaN(digit)) break;
        value = value * radix + digit;
        if (value > max) return null;
        this.#pos++;
        digits++;
      }
      if (digits === 0 || (!allowZeroPrefix && leadingZero && digits > 1)) return null;
      return value;
    });
  }

  #ipv4(): number[] | null {
    return this.#attempt(() => {
      const octets: number[] = [];
      for (let i = 0; i < 4; i++) {
        if (i > 0 && !this.#char('.')) return null;
        const octet = this.#number(10, 3, false, 255);
        if (octet === null) return null;
        octets.push(octet);
      }
      return octets;
    });
  }

  /** Reads up to `limit` groups into `groups`; returns how many and whether an IPv4 ended them. */
  #groups(groups: number[], limit: number): [number, boolean] {
    for (let i = 0; i < limit; i++) {
      const separated = (): boolean => i === 0 || this.#char(':');
      if (i < limit - 1) {
        const v4 = this.#attempt(() => (separated() ? this.#ipv4() : null));
        if (v4 !== null) {
          groups[i] = (v4[0]! << 8) | v4[1]!;
          groups[i + 1] = (v4[2]! << 8) | v4[3]!;
          return [i + 2, true];
        }
      }
      const group = this.#attempt(() => (separated() ? this.#number(16, 4, true, 0xffff) : null));
      if (group === null) return [i, false];
      groups[i] = group;
    }
    return [limit, false];
  }

  #ipv6(): number[] | null {
    const head = new Array<number>(8).fill(0);
    const [headSize, headIpv4] = this.#groups(head, 8);
    if (headSize === 8) return head;
    if (headIpv4 || !this.#char(':') || !this.#char(':')) return null;
    const tail = new Array<number>(7).fill(0);
    const [tailSize] = this.#groups(tail, 8 - (headSize + 1));
    head.splice(8 - tailSize, tailSize, ...tail.slice(0, tailSize));
    return head;
  }

  static ipv4(text: string): number[] | null {
    const reader = new AddressReader(text);
    const out = reader.#ipv4();
    return reader.#pos === text.length ? out : null;
  }

  static ipv6(text: string): number[] | null {
    const reader = new AddressReader(text);
    const out = reader.#ipv6();
    return reader.#pos === text.length ? out : null;
  }
}

const HOST_NAME = /^[a-z0-9.-]+$/;
const PORT = /^:[1-9][0-9]*$/;

/**
 * The canonical relay base URL (what the gateway's relay setting produces), at most
 * {@link MAX_INVITE_URL} bytes, as Rust's `check_invite_url`: `wss://host[:port]`, or `ws://`
 * only for a loopback host (`localhost`, an IPv4 `127.x.y.z`, an IPv6 equal to `::1`).
 *
 * - `host`: lowercase `[a-z0-9.-]+`, or `[` lowercase IPv6 `]`;
 * - `port`: decimal 1–65535 without leading zeros, never the scheme's default (443/80);
 * - nothing else: no userinfo, path, query, fragment, whitespace or control characters.
 */
function checkInviteUrl(url: string): Bytes {
  const bytes = utf8(url);
  if (bytes.length > MAX_INVITE_URL) throw new NoiseError('invite', 'url exceeds 255 bytes');
  const tls = url.startsWith('wss://');
  if (!tls && !url.startsWith('ws://')) throw new NoiseError('invite', 'url must start with ws:// or wss://');
  const authority = url.slice(tls ? 6 : 5);
  const bracketed = authority.startsWith('[');
  const split = bracketed ? authority.indexOf(']') : authority.indexOf(':');
  const end = split === -1 ? authority.length : bracketed ? split + 1 : split;
  const host = authority.slice(0, end);
  const port = authority.slice(end);
  if (host.length === 0) throw new NoiseError('invite', 'url has no host');
  let loopback: boolean;
  if (host.startsWith('[')) {
    const inner = host.endsWith(']') ? host.slice(1, -1) : null;
    const groups = inner === null || /[A-Z]/.test(inner) ? null : AddressReader.ipv6(inner);
    if (groups === null) throw new NoiseError('invite', 'url host is not valid');
    loopback = groups.every((group, i) => group === (i === 7 ? 1 : 0));
  } else {
    if (!HOST_NAME.test(host)) throw new NoiseError('invite', 'url host is not valid');
    loopback = host === 'localhost' || AddressReader.ipv4(host)?.[0] === 127;
  }
  if (port.length > 0) {
    const number = PORT.test(port) ? Number(port.slice(1)) : NaN;
    if (!(number <= 0xffff)) throw new NoiseError('invite', 'url port is not valid');
    if (number === (tls ? 443 : 80)) throw new NoiseError('invite', 'url names the default port');
  }
  if (!tls && !loopback) throw new NoiseError('invite', 'ws:// is only for a loopback host');
  return bytes;
}

export function encodePairingInvite(invite: PairingInvite): string {
  const url = checkInviteUrl(invite.url);
  const fields: [Uint8Array, number][] = [
    [invite.room, ROOM_LEN],
    [invite.token, TOKEN_LEN],
    [invite.gatewayPublic, KEY_LEN],
    [invite.psk, PSK_LEN],
  ];
  for (const [field, len] of fields) {
    if (field.length !== len) throw new NoiseError('invite', 'wrong length');
  }
  return INVITE_PREFIX + toBase64Url(concat(Uint8Array.of(url.length), url, ...fields.map(([f]) => f)));
}

export function parsePairingInvite(text: string): PairingInvite {
  if (!text.startsWith(INVITE_PREFIX)) throw new NoiseError('invite', 'missing v1. prefix');
  const raw = fromBase64Url(text.slice(INVITE_PREFIX.length));
  if (raw === null) throw new NoiseError('invite', 'not base64url');
  if (raw.length === 0) throw new NoiseError('invite', 'wrong length');
  const urlLen = raw[0]!;
  if (raw.length !== 1 + urlLen + INVITE_FIXED) throw new NoiseError('invite', 'wrong length');
  let url: string;
  try {
    url = fromUtf8(raw.subarray(1, 1 + urlLen));
  } catch {
    throw new NoiseError('invite', 'url is not UTF-8');
  }
  checkInviteUrl(url);
  let offset = 1 + urlLen;
  const take = (len: number): Bytes => {
    const out = raw.slice(offset, offset + len);
    offset += len;
    return out;
  };
  return {
    url,
    room: take(ROOM_LEN),
    token: take(TOKEN_LEN),
    gatewayPublic: take(KEY_LEN),
    psk: take(PSK_LEN),
  };
}
