// Reproduces crates/deck-remote/tests/vectors/noise-v1.json byte for byte, as initiator (device)
// and as responder (gateway).

import { describe, expect, test } from 'bun:test';
import { sha256 } from '@noble/hashes/sha2.js';

import { fromBase64Url, fromHex, toHex } from '../src/remote/bytes.ts';
import { MAX_MESSAGE } from '../src/remote/noise.ts';
import {
  encodeDeviceHello,
  encodePairingInvite,
  encodeWelcome,
  fingerprint,
  Keypair,
  MAX_FRAGMENT_DATA,
  type Mode,
  NoiseError,
  PAIRING_PATTERN,
  PROLOGUE,
  parsePairingInvite,
  RECONNECT_PATTERN,
  respondWith,
  type Session,
  startInitiator,
  type Welcome,
} from '../src/remote/protocol.ts';
import { encodeRelayHello } from '../src/remote/relay.ts';
import { generated, sharedVectors } from './vectors.ts';

const v = sharedVectors();
const device = Keypair.fromSecret(fromHex(v.device_static));
const gateway = Keypair.fromSecret(fromHex(v.gateway_static));

test('constants and keys', () => {
  expect(toHex(PROLOGUE)).toBe(v.prologue);
  expect(MAX_FRAGMENT_DATA).toBe(v.max_fragment_data);
  expect(MAX_FRAGMENT_DATA).toBe(MAX_MESSAGE - 17);
  expect(toHex(device.public)).toBe(v.device_public);
  expect(toHex(gateway.public)).toBe(v.gateway_public);
  expect(fingerprint(device.public)).toBe(v.device_fingerprint);
  expect(fingerprint(gateway.public)).toBe(v.gateway_fingerprint);
  expect(v.handshakes.map((h) => h.pattern)).toEqual([PAIRING_PATTERN, RECONNECT_PATTERN]);
});

test('payloads, invite and relay hello', () => {
  expect(toHex(encodeDeviceHello(v.hello))).toBe(v.hello_payload);
  const invite = {
    url: v.invite.url,
    room: fromHex(v.invite.room),
    token: fromHex(v.invite.token),
    gatewayPublic: fromHex(v.invite.gateway_public),
    psk: fromHex(v.invite.psk),
  };
  expect(encodePairingInvite(invite)).toBe(v.invite.encoded);
  expect(parsePairingInvite(v.invite.encoded)).toEqual(invite);
  expect(toHex(encodeRelayHello(invite.room, invite.token))).toBe(v.relay_hello);
});

for (const h of v.handshakes) {
  describe(h.mode, () => {
    const mode: Mode = h.mode === 'pairing' ? { kind: 'pairing', psk: fromHex(v.psk) } : { kind: 'reconnect' };
    const welcome: Welcome = { device: h.welcome.device, gateway: h.welcome.gateway };
    if (h.welcome.token !== null) welcome.token = fromBase64Url(h.welcome.token)!;

    test('welcome payload', () => {
      expect(toHex(encodeWelcome(welcome))).toBe(h.welcome_payload);
    });

    test('handshake and transport reproduce exact bytes on both sides', () => {
      const { initiator, msg1 } = startInitiator(
        mode,
        device,
        gateway.public,
        v.hello,
        fromHex(v.device_ephemeral),
      );
      expect(toHex(msg1)).toBe(h.msg1);

      const pending = respondWith(mode, gateway, fromHex(h.msg1), fromHex(v.gateway_ephemeral));
      expect(toHex(pending.devicePublic)).toBe(v.device_public);
      expect(pending.hello).toEqual(v.hello);
      const { session: gatewaySession, msg2 } = pending.accept(welcome);
      expect(toHex(msg2)).toBe(h.msg2);

      const { session: deviceSession, welcome: received } = initiator.finish(fromHex(h.msg2));
      expect(received).toEqual(welcome);
      expect(toHex(deviceSession.handshakeHash)).toBe(h.handshake_hash);
      expect(toHex(gatewaySession.handshakeHash)).toBe(h.handshake_hash);

      for (const session of [deviceSession, gatewaySession]) {
        session.config = { ...session.config, rekeyMessages: v.rekey_messages };
      }
      const sides: Record<'device' | 'gateway', [Session, Session]> = {
        device: [deviceSession, gatewaySession],
        gateway: [gatewaySession, deviceSession],
      };
      for (const step of h.transport) {
        const [sender, receiver] = sides[step.from];
        const plain =
          step.plaintext_hex !== undefined ? fromHex(step.plaintext_hex) : generated(step.plaintext_generated!);
        const frames = sender.seal(plain);
        expect(frames.length).toBe(step.frames.length);
        step.frames.forEach((expected, i) => {
          const frame = frames[i]!;
          expect(frame.length).toBe(expected.len);
          expect(toHex(sha256(frame))).toBe(expected.sha256);
          if (expected.hex !== undefined) expect(toHex(frame)).toBe(expected.hex);
          // Open the Rust bytes where inlined; otherwise the (hash-identical) local bytes.
          const opened = receiver.open(expected.hex !== undefined ? fromHex(expected.hex) : frame);
          if (i + 1 < frames.length) expect(opened).toBeNull();
          else expect(toHex(opened!)).toBe(toHex(plain));
        });
      }
    });
  });
}

/** The vector name (`snake_case`) of the error `run` fails with. */
function failure(run: () => unknown): string {
  try {
    run();
    return 'accepted';
  } catch (error) {
    if (error instanceof NoiseError) return error.code.replaceAll('-', '_');
    throw error;
  }
}

function vectorMode(name: 'pairing' | 'reconnect'): Mode {
  return name === 'pairing' ? { kind: 'pairing', psk: fromHex(v.psk) } : { kind: 'reconnect' };
}

/** Fresh sessions of the vector reconnect handshake, default config. */
function reconnectSessions(): Record<'device' | 'gateway', Session> {
  const mode = vectorMode('reconnect');
  const { initiator, msg1 } = startInitiator(mode, device, gateway.public, v.hello, fromHex(v.device_ephemeral));
  const welcome = { device: 'device-1', gateway: 'Desk' };
  const { session: gatewaySession, msg2 } = respondWith(mode, gateway, msg1, fromHex(v.gateway_ephemeral)).accept(
    welcome,
  );
  return { device: initiator.finish(msg2).session, gateway: gatewaySession };
}

describe('shared rejections', () => {
  test('handshake messages', () => {
    expect(v.rejected_handshakes.length).toBeGreaterThan(70);
    for (const vector of v.rejected_handshakes) {
      const mode = vectorMode(vector.mode);
      const start = (gatewayPublic: Uint8Array) =>
        startInitiator(mode, device, gatewayPublic, v.hello, fromHex(v.device_ephemeral));
      let error: string;
      if ('msg1' in vector) {
        error = failure(() => respondWith(mode, gateway, fromHex(vector.msg1), fromHex(v.gateway_ephemeral)));
      } else if ('msg2' in vector) {
        const { initiator } = start(gateway.public);
        error = failure(() => initiator.finish(fromHex(vector.msg2)));
      } else {
        error = failure(() => start(fromHex(vector.gateway_public)));
      }
      expect([vector.case, error]).toEqual([vector.case, vector.error]);
    }
  });

  test('transport frames', () => {
    expect(v.rejected_frames.length).toBeGreaterThan(15);
    for (const vector of v.rejected_frames) {
      const session = reconnectSessions()[vector.receiver];
      if (vector.max_message !== null) session.config = { ...session.config, maxMessage: vector.max_message };
      const frames = vector.frames.map(fromHex);
      for (const frame of frames.slice(0, -1)) session.open(frame);
      expect([vector.case, failure(() => session.open(frames.at(-1)!))]).toEqual([vector.case, vector.error]);
      expect([vector.case, failure(() => session.open(frames[0]!))]).toEqual([vector.case, 'poisoned']);
    }
  });
});
