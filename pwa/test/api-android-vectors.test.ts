// Reproduces crates/deck-remote/tests/vectors/api-android.json (written by the Rust side): the native
// Android push registration encodes exactly as the gateway decodes it, and the feature that gates it
// is the gateway's, never one the phone announces.

import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';

import { DEVICE_FEATURES } from '../src/app/client.ts';
import {
  FEATURE_ANDROID_PUSH,
  FEATURE_S4,
  OP_NAMES,
  PROTOCOL,
  type Request,
  checkRequest,
  opClass,
  opJournaled,
  requestJson,
} from '../src/remote/api.ts';

interface AndroidVectors {
  protocol: number;
  features: string[];
  messages: { type: 'request'; case: string; json: string }[];
}

const vectors: AndroidVectors = JSON.parse(
  readFileSync(new URL('../../crates/deck-remote/tests/vectors/api-android.json', import.meta.url), 'utf8'),
);

describe('android push vectors', () => {
  test('protocol and the gateway features they assume', () => {
    expect(vectors.protocol).toBe(PROTOCOL);
    expect(vectors.features).toEqual([FEATURE_S4, FEATURE_ANDROID_PUSH]);
  });

  test('the phone never announces android_push: only the gateway lists it', () => {
    expect(DEVICE_FEATURES).not.toContain(FEATURE_ANDROID_PUSH);
  });

  for (const vector of vectors.messages) {
    test(vector.case, () => {
      const request: Request = JSON.parse(vector.json);
      expect(checkRequest(request)).toBeNull();
      expect(requestJson(request)).toBe(vector.json);
      expect(request.op.op).toBe('push_android_subscribe');
      // A view op: not journaled (no seq), like push_subscribe.
      expect(opClass(request.op)).toBe('view');
      expect(opJournaled(request.op)).toBe(false);
      expect(OP_NAMES).toContain(request.op.op);
    });
  }

  test('fields keep Rust order whatever order the caller built them in', () => {
    const request: Request = {
      id: 9,
      op: { desktop: 'ab'.repeat(32), key: 'a'.repeat(43), token: 't', op: 'push_android_subscribe' },
    };
    expect(requestJson(request)).toBe(
      `{"id":9,"seq":null,"op":{"op":"push_android_subscribe","token":"t","key":"${'a'.repeat(43)}","desktop":"${'ab'.repeat(32)}"}}`,
    );
  });
});
