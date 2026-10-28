// Low-order (small-subgroup) X25519 keys are refused on both sides, matching Rust
// (crates/deck-remote/tests/low_order.rs). A hand-driven SymmetricState forges handshakes whose
// DH results are the all-zero constants a low-order key produces.

import { describe, expect, test } from 'bun:test';

import { type Bytes, fromHex } from '../src/remote/bytes.ts';
import { dh, isLowOrder, SymmetricState } from '../src/remote/noise.ts';
import {
  type DeviceHello,
  encodeDeviceHello,
  encodeWelcome,
  Initiator,
  Keypair,
  type Mode,
  NoiseError,
  PAIRING_PATTERN,
  PROLOGUE,
  RECONNECT_PATTERN,
  respond,
} from '../src/remote/protocol.ts';

const PSK = new Uint8Array(32).fill(7);
const ZERO = new Uint8Array(32);
const MODES: Mode[] = [{ kind: 'pairing', psk: PSK }, { kind: 'reconnect' }];
const hello: DeviceHello = { name: 'forger', platform: 'x', ts: 1 };

const topBit = new Uint8Array(32);
topBit[31] = 0x80;
const LOW_ORDER: Bytes[] = [
  ZERO,
  fromHex('0100000000000000000000000000000000000000000000000000000000000000'),
  fromHex('ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f'),
  fromHex('e0eb7a7c3b41b8ae1656e3faf19fc46ada098deb9c32b1fd866205165f49b800'),
  fromHex('5f9c95bca3508c24b1d0b1559c83ef5b04445cc4581c8e86d8224eddd09f1157'),
  fromHex('edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f'),
  fromHex('eeffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f'),
  topBit,
];

function expectHandshakeError(run: () => unknown): void {
  let caught: unknown;
  try {
    run();
  } catch (error) {
    caught = error;
  }
  expect(caught).toBeInstanceOf(NoiseError);
  expect(caught instanceof NoiseError ? caught.code : undefined).toBe('handshake');
}

function symmetric(mode: Mode, responderStatic: Uint8Array): SymmetricState {
  const sym = new SymmetricState(mode.kind === 'pairing' ? PAIRING_PATTERN : RECONNECT_PATTERN);
  sym.mixHash(PROLOGUE);
  sym.mixHash(responderStatic);
  return sym;
}

/** Message 1 from a forging device with chosen `e`/`s` and the DH results the gateway computes. */
function forgeMsg1(mode: Mode, gateway: Uint8Array, e: Uint8Array, es: Uint8Array, s: Uint8Array, ss: Uint8Array): Bytes {
  const sym = symmetric(mode, gateway);
  sym.mixHash(e);
  if (mode.kind === 'pairing') sym.mixKey(e);
  sym.mixKey(es);
  const encryptedS = sym.encryptAndHash(s);
  sym.mixKey(ss);
  if (mode.kind === 'pairing') sym.mixKeyAndHash(mode.psk);
  const payload = sym.encryptAndHash(encodeDeviceHello(hello));
  const out = new Uint8Array(32 + encryptedS.length + payload.length);
  out.set(e);
  out.set(encryptedS, 32);
  out.set(payload, 32 + encryptedS.length);
  return out;
}

/** A forging gateway answering `msg1` with a chosen `e`; `eSecret` null means low-order zeros. */
function forgeMsg2(mode: Mode, gateway: Keypair, msg1: Uint8Array, e: Uint8Array, eSecret: Uint8Array | null): Bytes {
  const sym = symmetric(mode, gateway.public);
  const re = msg1.slice(0, 32);
  sym.mixHash(re);
  if (mode.kind === 'pairing') sym.mixKey(re);
  sym.mixKey(dh(gateway.secret, re));
  const rs = sym.decryptAndHash(msg1.subarray(32, 80));
  sym.mixKey(dh(gateway.secret, rs));
  if (mode.kind === 'pairing') sym.mixKeyAndHash(mode.psk);
  sym.decryptAndHash(msg1.subarray(80));
  sym.mixHash(e);
  if (mode.kind === 'pairing') sym.mixKey(e);
  sym.mixKey(eSecret === null ? ZERO : dh(eSecret, re));
  sym.mixKey(eSecret === null ? ZERO : dh(eSecret, rs));
  const welcome = encodeWelcome(
    mode.kind === 'pairing'
      ? { device: 'dev', gateway: 'gw', token: new Uint8Array(32).fill(1) }
      : { device: 'dev', gateway: 'gw' },
  );
  const payload = sym.encryptAndHash(welcome);
  const out = new Uint8Array(32 + payload.length);
  out.set(e);
  out.set(payload, 32);
  return out;
}

test('low-order points are detected', () => {
  for (const point of LOW_ORDER) expect(isLowOrder(point)).toBe(true);
  for (let i = 0; i < 200; i++) expect(isLowOrder(Keypair.generate().public)).toBe(false);
});

for (const mode of MODES) {
  describe(mode.kind, () => {
    test('initiator refuses a low-order gateway key', () => {
      for (const point of LOW_ORDER) {
        expectHandshakeError(() => Initiator.start(mode, Keypair.generate(), point, hello));
      }
    });

    test('the forger is a correct Noise peer', () => {
      const gateway = Keypair.generate();
      const e = Keypair.generate();
      const s = Keypair.generate();
      const msg1 = forgeMsg1(mode, gateway.public, e.public, dh(e.secret, gateway.public), s.public, dh(s.secret, gateway.public));
      expect(respond(mode, gateway, msg1).devicePublic).toEqual(s.public);

      const { initiator, msg1: real } = Initiator.start(mode, Keypair.generate(), gateway.public, hello);
      const answer = Keypair.generate();
      initiator.finish(forgeMsg2(mode, gateway, real, answer.public, answer.secret));
    });

    test('responder refuses a low-order device ephemeral or static', () => {
      const gateway = Keypair.generate();
      for (const point of LOW_ORDER) {
        const s = Keypair.generate();
        expectHandshakeError(() =>
          respond(mode, gateway, forgeMsg1(mode, gateway.public, point, ZERO, s.public, dh(s.secret, gateway.public))),
        );
        const e = Keypair.generate();
        expectHandshakeError(() =>
          respond(mode, gateway, forgeMsg1(mode, gateway.public, e.public, dh(e.secret, gateway.public), point, ZERO)),
        );
      }
    });

    test('initiator refuses a low-order gateway ephemeral', () => {
      const gateway = Keypair.generate();
      const device = Keypair.generate();
      for (const point of LOW_ORDER) {
        const { initiator, msg1 } = Initiator.start(mode, device, gateway.public, hello);
        expectHandshakeError(() => initiator.finish(forgeMsg2(mode, gateway, msg1, point, null)));
      }
    });
  });
}
