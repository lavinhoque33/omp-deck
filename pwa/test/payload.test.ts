// Handshake payload acceptance against crates/deck-remote/tests/vectors/payload-v1.json, which
// the Rust tests (tests/payload.rs) check too: both sides accept and reject exactly the same bytes.

import { describe, expect, test } from 'bun:test';

import { fromHex, toHex } from '../src/remote/bytes.ts';
import { decodeDeviceHello, decodeWelcome, NoiseError } from '../src/remote/protocol.ts';
import { payloadVectors } from './vectors.ts';

const vectors = payloadVectors();

function rejects(run: () => unknown): boolean {
  try {
    run();
    return false;
  } catch (error) {
    if (error instanceof NoiseError && error.code === 'payload') return true;
    throw error;
  }
}

describe('payload vectors', () => {
  test('hello', () => {
    expect(vectors.hello.length).toBeGreaterThan(40);
    for (const vector of vectors.hello) {
      const bytes = fromHex(vector.hex);
      if (vector.accept) {
        expect([vector.case, decodeDeviceHello(bytes)]).toEqual([vector.case, vector.expect]);
      } else {
        expect([vector.case, rejects(() => decodeDeviceHello(bytes))]).toEqual([vector.case, true]);
      }
    }
  });

  test('welcome', () => {
    expect(vectors.welcome.length).toBeGreaterThan(20);
    for (const vector of vectors.welcome) {
      const bytes = fromHex(vector.hex);
      if (vector.accept) {
        const welcome = decodeWelcome(bytes);
        const got = {
          device: welcome.device,
          gateway: welcome.gateway,
          token: welcome.token === undefined ? null : toHex(welcome.token),
        };
        expect([vector.case, got]).toEqual([vector.case, vector.expect]);
      } else {
        expect([vector.case, rejects(() => decodeWelcome(bytes))]).toEqual([vector.case, true]);
      }
    }
  });
});
