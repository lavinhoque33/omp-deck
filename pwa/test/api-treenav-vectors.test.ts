// Rust owns api-treenav.json; earlier vectors remain byte-identical.
import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import {
  ApiDecodeError, ApiRequestError, checkRequest, decodeEvent, decodeReply, decodeWelcome,
  FEATURE_S4, FEATURE_TREE_NAV, PROTOCOL, type Request, requestJson,
} from '../src/remote/api.ts';
import { utf8 } from '../src/remote/bytes.ts';

interface TreeNavVectors {
  protocol: number;
  features: string[];
  messages: { type: 'request' | 'reply' | 'event' | 'welcome'; case: string; json: string }[];
  rejected_requests: unknown[];
}

const vectors: TreeNavVectors = JSON.parse(
  readFileSync(new URL('../../crates/deck-remote/tests/vectors/api-treenav.json', import.meta.url), 'utf8'),
);

test('tree navigation is an additive feature', () => {
  expect(vectors.protocol).toBe(PROTOCOL);
  expect(vectors.features).toEqual([FEATURE_S4, FEATURE_TREE_NAV]);
  expect(vectors.rejected_requests).toEqual([]);
  expect(new Set(vectors.messages.map(({ type }) => type))).toEqual(new Set(['request', 'reply', 'event', 'welcome']));
});

describe('tree navigation messages match their canonical Rust encoding', () => {
  for (const vector of vectors.messages) {
    test(`${vector.type} ${vector.case}`, () => {
      if (vector.type === 'request') {
        const request: Request = JSON.parse(vector.json);
        expect(checkRequest(request)).toBeNull();
        expect(requestJson(request)).toBe(vector.json);
        return;
      }
      const wire = utf8(vector.json);
      const decoded = vector.type === 'event' ? decodeEvent(wire)
        : vector.type === 'reply' ? decodeReply(wire) : decodeWelcome(wire);
      expect(decoded).toEqual(JSON.parse(vector.json));
      expect(JSON.stringify(decoded)).toBe(vector.json);
      if ('features' in decoded) expect(decoded.features).toEqual(vectors.features);
    });
  }
});

test('tree requests omit a false or absent at_leaf and reject non-booleans', () => {
  const tree = { op: 'tree', session: 's', filter: 'all', limit: 5 } as const;
  const original = '{"id":1,"seq":null,"op":{"op":"tree","session":"s","filter":"all","cursor":null,"limit":5}}';
  expect(requestJson({ id: 1, op: tree })).toBe(original);
  expect(requestJson({ id: 1, op: { ...tree, at_leaf: false } })).toBe(original);
  const malformed: Request = JSON.parse(original.replace('"limit":5', '"limit":5,"at_leaf":1'));
  expect(() => requestJson(malformed)).toThrow(ApiRequestError);
});

test('old tree replies omit both additions; explicit nulls are accepted', () => {
  const ok = { type: 'tree' as const, nodes: [], active_leaf: null, next_cursor: null, truncated: false };
  for (const extra of [{}, { prev_cursor: null, total: null }, { total: 0 }]) {
    const reply = { id: 1, ok: { ...ok, ...extra } };
    expect(decodeReply(utf8(JSON.stringify(reply)))).toEqual(reply);
  }
  for (const extra of [{ prev_cursor: false }, { total: -1 }, { total: 0.5 }]) {
    expect(() => decodeReply(utf8(JSON.stringify({ id: 1, ok: { ...ok, ...extra } })))).toThrow(ApiDecodeError);
  }
});

test('moved results validate the leaf, prompt text, image count and message', () => {
  const result = { type: 'moved', leaf: null, before_prompt: true, editor_text: 'Prompt', dropped_images: 0, message: null };
  for (const patch of [{ leaf: 7 }, { before_prompt: 'true' }, { editor_text: [] }, { dropped_images: -1 }, { dropped_images: 2 ** 32 }, { message: true }]) {
    const event = { type: 'outcome', seq: 1, outcome: { state: 'done', result: { ...result, ...patch } } };
    expect(() => decodeEvent(utf8(JSON.stringify(event)))).toThrow(ApiDecodeError);
  }
});
