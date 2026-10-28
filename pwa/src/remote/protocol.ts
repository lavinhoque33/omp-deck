// The OmpDeck device protocol, mirroring Rust `deck_remote::noise` (docs/notes/mobile-relay.md,
// "R1 contract"). The device is the Noise initiator; `respond`/`Pending` mirror the gateway side
// for cross-tests.

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

/** An X25519 static key pair. `wipe()` overwrites the secret once it is no longer needed. */
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
    return this.#secret;
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

export interface DeviceHello {
  name: string;
  platform: string;
  /** Unix milliseconds, strictly increasing per device. */
  ts: number;
}

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

/** Checks the size and grammar of a handshake payload and its version (`v` exactly 1). */
function parseObject(bytes: Uint8Array, what: string): Map<string, PayloadValue> {
  checkPayloadLen(bytes);
  const fields = parsePayload(bytes);
  if (fields === null || !fields.has('v')) throw new NoiseError('payload', `${what} is not valid`);
  if (fields.get('v') !== 1) throw new NoiseError('payload', `unsupported ${what} version`);
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

export function encodeDeviceHello(hello: DeviceHello): Bytes {
  const name = checkChars(hello.name, MAX_NAME_CHARS, 'device name too long');
  const platform = checkChars(hello.platform, MAX_PLATFORM_CHARS, 'platform too long');
  const ts = validTimestamp(hello.ts);
  const out = utf8(JSON.stringify({ v: 1, name, platform, ts }));
  checkPayloadLen(out);
  return out;
}

export function decodeDeviceHello(bytes: Uint8Array): DeviceHello {
  const fields = parseObject(bytes, 'hello');
  const name = takeStr(fields, 'name', 'hello');
  const platform = takeStr(fields, 'platform', 'hello');
  const ts = fields.get('ts');
  if (typeof ts !== 'number') throw new NoiseError('payload', 'hello is not valid');
  return {
    name: checkChars(name, MAX_NAME_CHARS, 'device name too long'),
    platform: checkChars(platform, MAX_PLATFORM_CHARS, 'platform too long'),
    ts,
  };
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

  /** Completes the handshake with the gateway's message 2. One use only. */
  finish(msg2: Uint8Array): { session: Session; welcome: Welcome } {
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
  readonly hello: DeviceHello;
  readonly #state: HandshakeState;
  readonly #pairing: boolean;

  constructor(state: HandshakeState, devicePublic: Bytes, hello: DeviceHello, pairing: boolean) {
    this.#state = state;
    this.devicePublic = devicePublic;
    this.hello = hello;
    this.#pairing = pairing;
  }

  /** Answers with message 2. `welcome.token` must be present exactly in pairing. */
  accept(welcome: Welcome): { session: Session; msg2: Bytes } {
    checkWelcomeMode(welcome, this.#pairing);
    const msg2 = this.#state.writeMessage(encodeWelcome(welcome));
    return { session: intoSession(this.#state), msg2 };
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
  const hello = decodeDeviceHello(payload);
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
  /** Relay base URL, `ws://` or `wss://`, at most 255 bytes. */
  url: string;
  room: Bytes;
  token: Bytes;
  gatewayPublic: Bytes;
  psk: Bytes;
}

const INVITE_FIXED = ROOM_LEN + TOKEN_LEN + KEY_LEN + PSK_LEN;

function checkInviteUrl(url: string): Bytes {
  const rest = url.startsWith('wss://') ? url.slice(6) : url.startsWith('ws://') ? url.slice(5) : null;
  if (rest === null) throw new NoiseError('invite', 'url must start with ws:// or wss://');
  if (rest.length === 0) throw new NoiseError('invite', 'url has no host');
  if (!url.isWellFormed()) throw new NoiseError('invite', 'url is not UTF-8');
  const bytes = utf8(url);
  if (bytes.length > MAX_INVITE_URL) throw new NoiseError('invite', 'url exceeds 255 bytes');
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
