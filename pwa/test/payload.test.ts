// Handshake payload acceptance against crates/deck-remote/tests/vectors/payload-v1.json (the
// grammar) and hello-v1.json (display-safe text, `api`, `features`, versions), which the Rust
// tests (tests/payload.rs) check too: both sides accept and reject exactly the same bytes, with
// the same error kind.

import { describe, expect, test } from 'bun:test';

import { fromHex, toHex, utf8 } from '../src/remote/bytes.ts';
import {
  type DeviceHello,
  decodeDeviceHello,
  decodeWelcome,
  encodeDeviceHello,
  isDisplaySafe,
  NoiseError,
} from '../src/remote/protocol.ts';
import { helloVectors, payloadVectors, recordedSha256 } from './vectors.ts';

const vectors = payloadVectors();
const hello = helloVectors();

/** The error code `run` fails with (vector names are `snake_case`). */
function failure(run: () => unknown): string {
  try {
    run();
    return 'accepted';
  } catch (error) {
    if (error instanceof NoiseError) return error.code.replaceAll('-', '_');
    throw error;
  }
}

describe('payload vectors', () => {
  test('hello', () => {
    expect(vectors.hello.length).toBeGreaterThan(40);
    for (const vector of vectors.hello) {
      const bytes = fromHex(vector.hex);
      if (vector.accept) {
        const { name, platform, ts } = decodeDeviceHello(bytes);
        expect([vector.case, { name, platform, ts }]).toEqual([vector.case, vector.expect]);
      } else {
        expect([vector.case, failure(() => decodeDeviceHello(bytes))]).toEqual([
          vector.case,
          vector.error ?? 'payload',
        ]);
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
        expect([vector.case, failure(() => decodeWelcome(bytes))]).toEqual([vector.case, vector.error ?? 'payload']);
      }
    }
  });
});

describe('hello vectors', () => {
  test('display-safe text', () => {
    expect(hello.display_safe.length).toBeGreaterThan(130);
    for (const vector of hello.display_safe) {
      expect([vector.case, isDisplaySafe(vector.text)]).toEqual([vector.case, vector.safe]);
    }
  });

  test('api, features and versions decode and re-encode canonically', () => {
    expect(hello.hello.length).toBeGreaterThan(40);
    for (const vector of hello.hello) {
      const bytes = utf8(vector.payload);
      if (vector.accept) {
        const decoded = decodeDeviceHello(bytes);
        expect([vector.case, decoded]).toEqual([vector.case, vector.expect]);
        expect([vector.case, toHex(encodeDeviceHello(decoded))]).toEqual([vector.case, toHex(utf8(vector.encoded))]);
      } else {
        expect([vector.case, failure(() => decodeDeviceHello(bytes))]).toEqual([vector.case, vector.error]);
      }
    }
  });

  test('encode refuses what decode refuses', () => {
    const base: DeviceHello = { name: 'a', platform: 'android', ts: 5 };
    const cases: [string, DeviceHello][] = [
      ['unsafe name', { ...base, name: 'a\u202eb' }],
      ['unsafe platform', { ...base, platform: 'x\u200b' }],
      ['api 0', { ...base, api: 0 }],
      ['api 2^53', { ...base, api: 2 ** 53 }],
      ['fractional api', { ...base, api: 1.5 }],
      ['empty feature', { ...base, features: [''] }],
      ['comma inside a feature', { ...base, features: ['a,b'] }],
      ['duplicate feature', { ...base, features: ['a', 'a'] }],
      ['33 features', { ...base, features: Array.from({ length: 33 }, (_, i) => `f${i}`) }],
    ];
    for (const [name, bad] of cases) {
      expect([name, failure(() => encodeDeviceHello(bad))]).toEqual([name, 'payload']);
    }
  });
});

test('hand-maintained vector files match their recorded SHA-256', () => {
  for (const name of ['payload-v1.json', 'hello-v1.json']) {
    const { actual, recorded } = recordedSha256(name);
    expect([name, actual]).toEqual([name, recorded ?? 'not recorded in README.md']);
  }
});
