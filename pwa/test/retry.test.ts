import { describe, expect, test } from 'bun:test';
import { BACKOFF_MAX_MS, RATE_LIMIT_WINDOW_MS, backoffMs, decideRetry } from '../src/app/retry.ts';
import { CLOSE } from '../src/remote/index.ts';

const mid = () => 0.5;
const context = (patch: Partial<Parameters<typeof decideRetry>[2]> = {}) => ({
  attempt: 0,
  rateLimitedSince: null,
  now: 1_000_000,
  random: mid,
  ...patch,
});

describe('backoff', () => {
  test('doubles per attempt and caps at the maximum', () => {
    expect(backoffMs(0, 1000, mid)).toBe(1000);
    expect(backoffMs(1, 1000, mid)).toBe(2000);
    expect(backoffMs(4, 1000, mid)).toBe(16000);
    expect(backoffMs(9, 1000, mid)).toBe(BACKOFF_MAX_MS);
    expect(backoffMs(50, 1000, mid)).toBe(BACKOFF_MAX_MS);
  });

  test('jitter stays within ±20 %', () => {
    expect(backoffMs(2, 1000, () => 0)).toBe(3200);
    expect(backoffMs(2, 1000, () => 0.999_999)).toBe(4800);
  });
});

describe('decideRetry', () => {
  test('unauthorized and upgrade stop without retrying', () => {
    expect(decideRetry(CLOSE.UNAUTHORIZED, false, context())).toEqual({ retry: false, stop: 'unauthorized' });
    expect(decideRetry(CLOSE.UNAUTHORIZED, true, context())).toEqual({ retry: false, stop: 'unauthorized' });
    expect(decideRetry(CLOSE.UPGRADE_REQUIRED, false, context())).toEqual({ retry: false, stop: 'upgrade' });
  });

  test('rate limiting retries within the window, then stops', () => {
    const now = 1_000_000;
    const first = decideRetry(CLOSE.RATE_LIMITED, false, context({ now }));
    expect(first).toEqual({ retry: true, delayMs: 3000 });
    const later = decideRetry(CLOSE.RATE_LIMITED, false, context({ now: now + 70_000, rateLimitedSince: now, attempt: 5 }));
    expect(later).toEqual({ retry: true, delayMs: 5000 });
    expect(decideRetry(CLOSE.RATE_LIMITED, false, context({ now: now + RATE_LIMIT_WINDOW_MS, rateLimitedSince: now }))).toEqual({
      retry: false,
      stop: 'rate_limited',
    });
  });

  test('a normal close of an established session means another tab took over', () => {
    expect(decideRetry(CLOSE.NORMAL, true, context())).toEqual({ retry: false, stop: 'replaced' });
    expect(decideRetry(CLOSE.NORMAL, false, context())).toEqual({ retry: true, delayMs: 1000 });
  });

  test('everything else backs off', () => {
    for (const code of [CLOSE.GATEWAY_OFFLINE, CLOSE.GOING_AWAY, CLOSE.STALLED, CLOSE.REPLACED, 1006]) {
      expect(decideRetry(code, true, context({ attempt: 3 }))).toEqual({ retry: true, delayMs: 8000 });
    }
  });
});
