// Rust owns api-progress.json; older feature vectors remain byte-identical.
import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import { DEVICE_FEATURES } from '../src/app/client.ts';
import { ApiDecodeError, decodeEvent, decodeWelcome, FEATURE_PROGRESS, FEATURE_S4, PROTOCOL, type LiveSession } from '../src/remote/api.ts';
import { utf8 } from '../src/remote/bytes.ts';

interface ProgressVectors {
  protocol: number;
  features: string[];
  messages: { type: 'event' | 'welcome'; case: string; json: string }[];
  rejected_requests: unknown[];
}
const vectors: ProgressVectors = JSON.parse(readFileSync(new URL('../../crates/deck-remote/tests/vectors/api-progress.json', import.meta.url), 'utf8'));

test('progress feature is additive and announces no new requests', () => {
  expect(vectors.protocol).toBe(PROTOCOL);
  expect(vectors.features).toEqual([FEATURE_S4, FEATURE_PROGRESS]);
  expect(DEVICE_FEATURES).toContain(FEATURE_PROGRESS);
  expect(vectors.rejected_requests).toEqual([]);
  expect(new Set(vectors.messages.map((message) => message.type))).toEqual(new Set(['event', 'welcome']));
});

describe('progress gateway messages validate canonically', () => {
  for (const vector of vectors.messages) {
    test(`${vector.type} ${vector.case}`, () => {
      const decoded = vector.type === 'event' ? decodeEvent(utf8(vector.json)) : decodeWelcome(utf8(vector.json));
      expect(decoded).toEqual(JSON.parse(vector.json));
      expect(JSON.stringify(decoded)).toBe(JSON.stringify(JSON.parse(vector.json)));
    });
  }
});

function liveMessage(): { type: 'feed'; feed: { feed: 'live' }; value: { feed: 'live'; sessions: LiveSession[]; truncated: boolean } } {
  const vector = vectors.messages.find((message) => message.type === 'event');
  if (vector === undefined) throw new Error('progress vectors must carry a live event');
  return JSON.parse(vector.json);
}

test('old live sessions omit progress; an explicit null is also accepted', () => {
  const message = liveMessage();
  const session = message.value.sessions[0]!;
  delete session.progress;
  expect(decodeEvent(utf8(JSON.stringify(message)))).toEqual(message);
  session.progress = null;
  expect(decodeEvent(utf8(JSON.stringify(message)))).toEqual(message);
});

test('progress fields validate task status, goal status, counters and structure', () => {
  const patches = [
    { phases: 'bad' },
    { phases: [{ name: 'Phase', tasks: [{ content: 'Task', status: 'typo' }] }] },
    { goal: { objective: 'Goal', status: 'budget-limited', tokens_used: 1, token_budget: null, time_used_seconds: 2 } },
    { goal: { objective: 'Goal', status: 'active', tokens_used: -1, token_budget: null, time_used_seconds: 2 } },
    { truncated: 'true' },
  ];
  for (const patch of patches) {
    const message = liveMessage();
    Object.assign(message.value.sessions[0]!.progress!, patch);
    expect(() => decodeEvent(utf8(JSON.stringify(message)))).toThrow(ApiDecodeError);
  }
});
