// Behaviour and rejection tests of the TypeScript device protocol.

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

import { type Bytes, concat, fromBase64Url, toBase64Url, utf8 } from '../src/remote/bytes.ts';
import { type CipherState, HandshakeState, MAX_MESSAGE, TAG_LEN } from '../src/remote/noise.ts';
import {
  DEFAULT_SESSION_CONFIG,
  type DeviceHello,
  decodeDeviceHello,
  decodeWelcome,
  encodeDeviceHello,
  encodePairingInvite,
  encodeWelcome,
  FLAG_MORE,
  FLAG_REKEY,
  fingerprint,
  INVITE_PREFIX,
  Initiator,
  Keypair,
  MAX_FRAGMENT_DATA,
  MAX_MSG1,
  MAX_NAME_CHARS,
  MAX_PLATFORM_CHARS,
  type Mode,
  NoiseError,
  type NoiseErrorCode,
  nextTimestamp,
  type PairingInvite,
  PROLOGUE,
  parsePairingInvite,
  respond,
  type Session,
  timestampFresh,
  type Welcome,
} from '../src/remote/protocol.ts';
import { CLOSE, encodeRelayHello, HELLO_OK, MAX_PAYLOAD } from '../src/remote/relay.ts';

const PSK = new Uint8Array(32).fill(7);
const TOKEN = new Uint8Array(32).fill(9);
const PAIRING: Mode = { kind: 'pairing', psk: PSK };
const RECONNECT: Mode = { kind: 'reconnect' };
const hello: DeviceHello = { name: 'Pixel', platform: 'android', ts: 1000 };

function welcome(pairing: boolean): Welcome {
  return pairing ? { device: 'dev-1', gateway: 'Desk', token: TOKEN } : { device: 'dev-1', gateway: 'Desk' };
}

function expectCode(code: NoiseErrorCode, run: () => unknown): void {
  let caught: unknown;
  try {
    run();
  } catch (error) {
    caught = error;
  }
  expect(caught).toBeInstanceOf(NoiseError);
  expect(caught instanceof NoiseError ? caught.code : undefined).toBe(code);
}

function connect(mode: Mode): [Session, Session] {
  const device = Keypair.generate();
  const gateway = Keypair.generate();
  const pairing = mode.kind === 'pairing';
  const { initiator, msg1 } = Initiator.start(mode, device, gateway.public, hello);
  const pending = respond(mode, gateway, msg1);
  expect(pending.devicePublic).toEqual(device.public);
  expect(pending.hello).toEqual(hello);
  const { session: gatewaySession, msg2 } = pending.accept(welcome(pairing));
  const { session: deviceSession, welcome: got } = initiator.finish(msg2);
  expect(got).toEqual(welcome(pairing));
  expect(deviceSession.handshakeHash).toEqual(gatewaySession.handshakeHash);
  return [deviceSession, gatewaySession];
}

function deliver(from: Session, to: Session, message: Bytes): void {
  const frames = from.seal(message);
  frames.forEach((frame, i) => {
    const opened = to.open(frame);
    if (i + 1 < frames.length) expect(opened).toBeNull();
    else expect(opened).toEqual(message);
  });
}

/** A device driven by the raw Noise state machine, for frames the API never produces. */
function rawDevice(mode: Mode): { send: CipherState; receive: CipherState; gateway: Session } {
  const device = Keypair.generate();
  const gateway = Keypair.generate();
  const state = new HandshakeState({
    pattern: mode.kind === 'pairing' ? 'IKpsk1' : 'IK',
    initiator: true,
    prologue: PROLOGUE,
    s: device,
    rs: gateway.public,
    ...(mode.kind === 'pairing' ? { psk: mode.psk } : {}),
  });
  const msg1 = state.writeMessage(encodeDeviceHello(hello));
  const { session, msg2 } = respond(mode, gateway, msg1).accept(welcome(mode.kind === 'pairing'));
  state.readMessage(msg2);
  const [send, receive] = state.split();
  return { send, receive, gateway: session };
}

function rawMsg1(mode: Mode, gatewayPublic: Uint8Array, payload: Uint8Array): Uint8Array {
  const state = new HandshakeState({
    pattern: mode.kind === 'pairing' ? 'IKpsk1' : 'IK',
    initiator: true,
    prologue: PROLOGUE,
    s: Keypair.generate(),
    rs: gatewayPublic,
    ...(mode.kind === 'pairing' ? { psk: mode.psk } : {}),
  });
  return state.writeMessage(payload);
}

function expectPoisoned(session: Session): void {
  expect(session.poisoned).toBe(true);
  expectCode('poisoned', () => session.open(new Uint8Array(32)));
  expectCode('poisoned', () => session.seal(utf8('x')));
}

const EMPTY = new Uint8Array(0);

describe('handshake', () => {
  test('both modes complete and carry traffic', () => {
    for (const [device, gateway] of [connect(PAIRING), connect(RECONNECT)]) {
      deliver(device, gateway, utf8('hello gateway'));
      deliver(gateway, device, utf8('hello device'));
      deliver(device, gateway, EMPTY);
    }
  });

  test('wrong PSK is rejected by the responder', () => {
    const gateway = Keypair.generate();
    const { msg1 } = Initiator.start(
      { kind: 'pairing', psk: new Uint8Array(32).fill(8) },
      Keypair.generate(),
      gateway.public,
      hello,
    );
    expectCode('handshake', () => respond(PAIRING, gateway, msg1));
  });

  test('pairing and reconnect messages are not interchangeable', () => {
    const gateway = Keypair.generate();
    const device = Keypair.generate();
    expectCode('handshake', () =>
      respond(RECONNECT, gateway, Initiator.start(PAIRING, device, gateway.public, hello).msg1),
    );
    expectCode('handshake', () =>
      respond(PAIRING, gateway, Initiator.start(RECONNECT, device, gateway.public, hello).msg1),
    );
  });

  test('wrong gateway key is rejected', () => {
    const gateway = Keypair.generate();
    const impostor = Keypair.generate();
    for (const mode of [PAIRING, RECONNECT]) {
      const { msg1 } = Initiator.start(mode, Keypair.generate(), impostor.public, hello);
      expectCode('handshake', () => respond(mode, gateway, msg1));
    }
  });

  test('tampered, truncated and oversized messages are rejected', () => {
    const gateway = Keypair.generate();
    const { initiator, msg1 } = Initiator.start(PAIRING, Keypair.generate(), gateway.public, hello);
    for (const i of [0, 40, msg1.length - 1]) {
      const bad = msg1.slice();
      bad[i]! ^= 1;
      expectCode('handshake', () => respond(PAIRING, gateway, bad));
    }
    expectCode('message-size', () => respond(PAIRING, gateway, msg1.subarray(0, 90)));
    expectCode('message-size', () => respond(PAIRING, gateway, new Uint8Array(MAX_MSG1 + 1)));
    const { msg2 } = respond(PAIRING, gateway, msg1).accept(welcome(true));
    const bad = msg2.slice();
    bad[bad.length - 1]! ^= 0x80;
    expectCode('handshake', () => initiator.finish(bad));
  });

  test('malformed and oversized hello payloads are rejected', () => {
    const gateway = Keypair.generate();
    const cases: [string, NoiseErrorCode][] = [
      ['not json', 'payload'],
      ['{"v":2,"name":"a","platform":"b","ts":1}', 'payload'],
      ['{"v":1,"name":"a","ts":1}', 'payload'],
      ['{"v":1,"name":"a","platform":"b","ts":9007199254740992}', 'payload'],
      [`{"v":1,"name":"a","platform":"b","ts":1,"pad":"${'x'.repeat(1100)}"}`, 'message-size'],
    ];
    for (const [payload, code] of cases) {
      expectCode(code, () => respond(PAIRING, gateway, rawMsg1(PAIRING, gateway.public, utf8(payload))));
    }
    // Unknown string and integer fields are ignored; any other unknown value is rejected.
    const ok = rawMsg1(RECONNECT, gateway.public, utf8('{"v":1,"name":"a","platform":"b","ts":5,"extra":"x","n":2}'));
    expect(respond(RECONNECT, gateway, ok).hello.ts).toBe(5);
    const nested = rawMsg1(RECONNECT, gateway.public, utf8('{"v":1,"name":"a","platform":"b","ts":5,"extra":[1]}'));
    expectCode('payload', () => respond(RECONNECT, gateway, nested));
  });

  test('welcome token must match the mode', () => {
    const gateway = Keypair.generate();
    const device = Keypair.generate();
    const pairing = respond(PAIRING, gateway, Initiator.start(PAIRING, device, gateway.public, hello).msg1);
    expectCode('payload', () => pairing.accept(welcome(false)));
    const reconnect = respond(RECONNECT, gateway, Initiator.start(RECONNECT, device, gateway.public, hello).msg1);
    expectCode('payload', () => reconnect.accept(welcome(true)));
  });
});

describe('payloads', () => {
  test('limits', () => {
    const name = 'é'.repeat(MAX_NAME_CHARS);
    expect(decodeDeviceHello(encodeDeviceHello({ ...hello, name }))).toEqual({ ...hello, name });
    expectCode('payload', () => encodeDeviceHello({ ...hello, name: `${name}é` }));
    expectCode('payload', () => encodeDeviceHello({ ...hello, platform: 'p'.repeat(MAX_PLATFORM_CHARS + 1) }));
    expectCode('payload', () => encodeDeviceHello({ ...hello, ts: 2 ** 53 }));
    expectCode('payload', () => encodeDeviceHello({ ...hello, name: '\ud800' }));
    expectCode('payload', () =>
      decodeDeviceHello(utf8(`{"v":1,"name":"a","platform":"b","ts":1,"pad":"${'x'.repeat(1000)}"}`)),
    );
    expectCode('payload', () => encodeWelcome({ device: 'd'.repeat(65), gateway: 'g' }));
    expectCode('payload', () => encodeWelcome({ device: 'd', gateway: 'g'.repeat(65) }));
    expect(decodeWelcome(encodeWelcome(welcome(true)))).toEqual(welcome(true));
    expectCode('payload', () => decodeWelcome(utf8('{"v":1,"device":"d","gateway":"g","token":"AAAA"}')));
  });

  test('fingerprint, keys and timestamps', () => {
    const key = Keypair.generate();
    expect(Keypair.fromSecret(key.secretBytes()).public).toEqual(key.public);
    expect(fingerprint(key.public)).toMatch(/^[0-9a-f]{4}(-[0-9a-f]{4}){4}$/);
    expect(timestampFresh(5, 6)).toBe(true);
    expect(timestampFresh(5, 5)).toBe(false);
    expect(timestampFresh(5, 4)).toBe(false);
    expect(nextTimestamp(10, 5)).toBe(11);
    expect(nextTimestamp(10, 50)).toBe(50);
  });

  test('base64url is strict', () => {
    const bytes = Uint8Array.from({ length: 33 }, (_, i) => i * 5);
    expect(fromBase64Url(toBase64Url(bytes))).toEqual(bytes);
    expect(fromBase64Url('AB==')).toBeNull();
    expect(fromBase64Url('AB')).toBeNull(); // non-canonical trailing bits
    expect(fromBase64Url('A')).toBeNull();
    expect(fromBase64Url('a+b/')).toBeNull();
  });
});

describe('transport', () => {
  test('tampered frame poisons the session', () => {
    const [device, gateway] = connect(PAIRING);
    const frame = device.seal(utf8('secret'))[0]!;
    const bad = frame.slice();
    bad[3]! ^= 0x10;
    expectCode('decrypt', () => gateway.open(bad));
    expectPoisoned(gateway);
    expectCode('poisoned', () => gateway.open(frame));
  });

  test('replayed frame poisons the session', () => {
    const [device, gateway] = connect(PAIRING);
    const frame = device.seal(utf8('once'))[0]!;
    expect(gateway.open(frame)).toEqual(utf8('once'));
    expectCode('decrypt', () => gateway.open(frame));
    expectPoisoned(gateway);
  });

  test('reordered frames poison the session', () => {
    const [device, gateway] = connect(RECONNECT);
    const first = device.seal(utf8('one'))[0]!;
    const second = device.seal(utf8('two'))[0]!;
    expectCode('decrypt', () => gateway.open(second));
    expectPoisoned(gateway);
    expectCode('poisoned', () => gateway.open(first));
  });

  test('reordered fragments poison the session', () => {
    const [device, gateway] = connect(PAIRING);
    const frames = device.seal(new Uint8Array(MAX_FRAGMENT_DATA + 1));
    expect(frames.length).toBe(2);
    expectCode('decrypt', () => gateway.open(frames[1]!));
    expectPoisoned(gateway);
  });

  test('unknown flag bits and missing flags poison the session', () => {
    for (const bits of [0x04, 0x80, 0xff]) {
      const raw = rawDevice(PAIRING);
      const frame = raw.send.encryptWithAd(EMPTY, Uint8Array.of(bits, 0x78));
      expectCode('bad-flags', () => raw.gateway.open(frame));
      expectPoisoned(raw.gateway);
    }
    const raw = rawDevice(RECONNECT);
    const frame = raw.send.encryptWithAd(EMPTY, EMPTY);
    expect(frame.length).toBe(TAG_LEN);
    expectCode('frame-size', () => raw.gateway.open(frame));
    expectPoisoned(raw.gateway);
  });

  test('oversized frame is rejected', () => {
    const [, gateway] = connect(PAIRING);
    expectCode('frame-size', () => gateway.open(new Uint8Array(MAX_MESSAGE + 1)));
    expectPoisoned(gateway);
  });

  test('fragmentation boundary', () => {
    const [device, gateway] = connect(PAIRING);
    const exact = new Uint8Array(MAX_FRAGMENT_DATA).fill(0xab);
    const one = device.seal(exact);
    expect(one.length).toBe(1);
    expect(one[0]!.length).toBe(MAX_MESSAGE);
    expect(gateway.open(one[0]!)).toEqual(exact);

    const over = new Uint8Array(MAX_FRAGMENT_DATA + 1).fill(0xcd);
    const two = device.seal(over);
    expect(two.map((f) => f.length)).toEqual([MAX_MESSAGE, 1 + 1 + TAG_LEN]);
    expect(gateway.open(two[0]!)).toBeNull();
    expect(gateway.open(two[1]!)).toEqual(over);

    const big = Uint8Array.from({ length: 3 * MAX_FRAGMENT_DATA + 7 }, (_, i) => i & 0xff);
    deliver(device, gateway, big);
    deliver(gateway, device, big);
  });

  test('raw peer sees the wire flags', () => {
    const raw = rawDevice(PAIRING);
    const frames = raw.gateway.seal(new Uint8Array(MAX_FRAGMENT_DATA + 1).fill(5));
    const first = raw.receive.decryptWithAd(EMPTY, frames[0]!);
    expect(first[0]).toBe(FLAG_MORE);
    expect(first.length).toBe(1 + MAX_FRAGMENT_DATA);
    expect(raw.receive.decryptWithAd(EMPTY, frames[1]!)).toEqual(Uint8Array.of(0, 5));
  });

  test('reassembly cap', () => {
    const [device, gateway] = connect(PAIRING);
    const cap = 100_000;
    gateway.config = { ...gateway.config, maxMessage: cap };
    deliver(device, gateway, new Uint8Array(cap));
    const frames = device.seal(new Uint8Array(cap + 1));
    expect(gateway.open(frames[0]!)).toBeNull();
    expectCode('too-large', () => gateway.open(frames[1]!));
    expectPoisoned(gateway);

    device.config = { ...device.config, maxMessage: 10 };
    expectCode('too-large', () => device.seal(new Uint8Array(11)));
    expect(device.poisoned).toBe(true);
  });

  test('short non-final fragment poisons the session', () => {
    const full = new Uint8Array(1 + MAX_FRAGMENT_DATA).fill(3);
    full[0] = FLAG_MORE;
    for (const len of [1, MAX_FRAGMENT_DATA - 1]) {
      const raw = rawDevice(PAIRING);
      expectCode('frame-size', () => raw.gateway.open(raw.send.encryptWithAd(EMPTY, full.subarray(0, 1 + len))));
      expectPoisoned(raw.gateway);
    }
    // After a valid full fragment too.
    const raw = rawDevice(PAIRING);
    expect(raw.gateway.open(raw.send.encryptWithAd(EMPTY, full))).toBeNull();
    expectCode('frame-size', () => raw.gateway.open(raw.send.encryptWithAd(EMPTY, full.subarray(0, 100))));
    expectPoisoned(raw.gateway);
  });

  test('empty non-final fragment poisons the session', () => {
    const raw = rawDevice(RECONNECT);
    const frame = raw.send.encryptWithAd(EMPTY, Uint8Array.of(FLAG_MORE));
    expect(frame.length).toBe(1 + TAG_LEN);
    expectCode('frame-size', () => raw.gateway.open(frame));
    expectPoisoned(raw.gateway);
  });

  test('many-fragment message at the default cap', () => {
    const [device, gateway] = connect(PAIRING);
    const cap = DEFAULT_SESSION_CONFIG.maxMessage;
    const message = Uint8Array.from({ length: cap }, (_, i) => i % 251);
    const frames = device.seal(message);
    expect(frames.length).toBe(Math.ceil(cap / MAX_FRAGMENT_DATA));
    for (const frame of frames.slice(0, -1)) {
      expect(frame.length).toBe(MAX_MESSAGE);
      expect(gateway.open(frame)).toBeNull();
    }
    expect(gateway.open(frames.at(-1)!)).toEqual(message);

    // One byte more: the sender may allow it, the receiver's cap rejects it.
    device.config = { ...device.config, maxMessage: cap + 1 };
    const over = device.seal(new Uint8Array(cap + 1));
    for (const frame of over.slice(0, -1)) expect(gateway.open(frame)).toBeNull();
    expectCode('too-large', () => gateway.open(over.at(-1)!));
    expectPoisoned(gateway);
  });

  test('default config', () => {
    expect(DEFAULT_SESSION_CONFIG).toEqual({
      rekeyMessages: 2 ** 20,
      rekeyIntervalMs: 30 * 60 * 1000,
      maxMessage: 16 * 1024 * 1024,
    });
  });

  test('rekey by count keeps both sides in sync', () => {
    const [device, gateway] = connect(PAIRING);
    for (const session of [device, gateway]) session.config = { ...session.config, rekeyMessages: 2 };
    for (let i = 0; i < 20; i++) {
      deliver(device, gateway, Uint8Array.of(i));
      deliver(gateway, device, Uint8Array.of(i, i));
      deliver(device, gateway, new Uint8Array(2 * MAX_FRAGMENT_DATA + 3).fill(i));
    }
  });

  test('the rekey flag really rekeys', () => {
    const raw = rawDevice(PAIRING);
    raw.gateway.config = { ...raw.gateway.config, rekeyMessages: 2 };
    const [a, b, c] = [utf8('a'), utf8('b'), utf8('c')].map((m) => raw.gateway.seal(m)[0]!);
    expect(raw.receive.decryptWithAd(EMPTY, a!)).toEqual(Uint8Array.of(0, 0x61));
    expect(raw.receive.decryptWithAd(EMPTY, b!)).toEqual(Uint8Array.of(FLAG_REKEY, 0x62));
    // A peer that skips REKEY cannot read on.
    expect(() => raw.receive.decryptWithAd(EMPTY, c!)).toThrow();

    const other = rawDevice(RECONNECT);
    expect(other.gateway.open(other.send.encryptWithAd(EMPTY, Uint8Array.of(FLAG_REKEY, 0x78)))).toEqual(utf8('x'));
    other.send.rekey();
    expect(other.gateway.open(other.send.encryptWithAd(EMPTY, Uint8Array.of(0, 0x79)))).toEqual(utf8('y'));
  });

  test('rekey by time keeps both sides in sync', () => {
    const raw = rawDevice(PAIRING);
    const start = performance.now();
    const early = raw.gateway.seal(utf8('early'), start)[0]!;
    expect(raw.receive.decryptWithAd(EMPTY, early)[0]).toBe(0);
    const later = start + DEFAULT_SESSION_CONFIG.rekeyIntervalMs + 1000;
    const late = raw.gateway.seal(utf8('late'), later)[0]!;
    expect(raw.receive.decryptWithAd(EMPTY, late)[0]).toBe(FLAG_REKEY);
    raw.receive.rekey();
    const next = raw.gateway.seal(utf8('next'), later + 60_000)[0]!;
    expect(raw.receive.decryptWithAd(EMPTY, next)).toEqual(concat(Uint8Array.of(0), utf8('next')));

    const [device, gateway] = connect(PAIRING);
    let now = performance.now();
    for (let i = 0; i < 10; i++) {
      now += 20 * 60 * 1000;
      expect(gateway.open(device.seal(Uint8Array.of(i), now)[0]!)).toEqual(Uint8Array.of(i));
      expect(device.open(gateway.seal(Uint8Array.of(i, i), now)[0]!)).toEqual(Uint8Array.of(i, i));
    }
  });

  test('sessions have distinct ids', () => {
    expect(connect(PAIRING)[0].handshakeHash).not.toEqual(connect(PAIRING)[0].handshakeHash);
  });
});

describe('pairing invite', () => {
  const invite: PairingInvite = {
    url: 'wss://relay.example.org:8443/base',
    room: new Uint8Array(16).fill(1),
    token: new Uint8Array(32).fill(2),
    gatewayPublic: new Uint8Array(32).fill(3),
    psk: new Uint8Array(32).fill(4),
  };

  test('round trip', () => {
    const text = encodePairingInvite(invite);
    expect(text.startsWith(INVITE_PREFIX)).toBe(true);
    expect(parsePairingInvite(text)).toEqual(invite);
    const ws = { ...invite, url: `ws://${'h'.repeat(250)}` };
    expect(parsePairingInvite(encodePairingInvite(ws))).toEqual(ws);
  });

  test('rejects', () => {
    const text = encodePairingInvite(invite);
    const body = text.slice(INVITE_PREFIX.length);
    expectCode('invite', () => parsePairingInvite(body));
    expectCode('invite', () => parsePairingInvite(`v2.${body}`));
    expectCode('invite', () => parsePairingInvite('v1.!!!'));
    expectCode('invite', () => parsePairingInvite('v1.'));
    const raw = fromBase64Url(body)!;
    expectCode('invite', () => parsePairingInvite(INVITE_PREFIX + toBase64Url(raw.subarray(0, raw.length - 1))));
    expectCode('invite', () => parsePairingInvite(INVITE_PREFIX + toBase64Url(concat(raw, Uint8Array.of(0)))));
    for (const url of ['https://relay.example.org', 'wss://', 'relay.example.org']) {
      expectCode('invite', () => encodePairingInvite({ ...invite, url }));
      const forged = concat(Uint8Array.of(utf8(url).length), utf8(url), new Uint8Array(112));
      expectCode('invite', () => parsePairingInvite(INVITE_PREFIX + toBase64Url(forged)));
    }
    expectCode('invite', () => encodePairingInvite({ ...invite, url: `wss://${'h'.repeat(255)}` }));
  });
});

describe('relay', () => {
  test('device hello and constants mirror relay.rs', () => {
    const hello = encodeRelayHello(new Uint8Array(16).fill(1), new Uint8Array(32).fill(2));
    expect(hello.length).toBe(50);
    expect([hello[0], hello[1]]).toEqual([0x44, 1]);
    expect(HELLO_OK).toEqual(Uint8Array.of(0));
    expect(MAX_PAYLOAD).toBe(MAX_MESSAGE);
    // Every close code of relay.rs, by name and value.
    const source = readFileSync(new URL('../../crates/deck-remote/src/relay.rs', import.meta.url), 'utf8');
    const codes = Object.fromEntries(
      [...source.matchAll(/pub const ([A-Z_]+): u16 = (\d+);/g)].map((m) => [m[1]!, Number(m[2])]),
    );
    expect(Object.keys(codes).length).toBeGreaterThan(5);
    expect(codes).toEqual({ ...CLOSE });
    expect(() => encodeRelayHello(new Uint8Array(15), new Uint8Array(32))).toThrow();
  });
});
