// What the client does after its connection closed. A close code is an unauthenticated hint
// (mobile-relay.md "Protocol v1 freeze"): it decides whether and when to retry and what to show,
// and never touches the stored pairing.

import { CLOSE } from '../remote/index.ts';

/** Why the client stopped retrying on its own; each needs a user action. */
export type Stop =
  /** 4003: the relay does not admit this device (revoked, or the room was removed). */
  | 'unauthorized'
  /** 4026: the gateway needs a newer app. */
  | 'upgrade'
  /** 4029 for longer than {@link RATE_LIMIT_WINDOW_MS}. */
  | 'rate_limited'
  /** 1000 from the gateway: a newer session of this device (another tab) replaced this one. */
  | 'replaced';

export type RetryDecision = { retry: true; delayMs: number } | { retry: false; stop: Stop };

/** 4029 is retried for about this long after the first one, then the client stops. */
export const RATE_LIMIT_WINDOW_MS = 75_000;
export const BACKOFF_BASE_MS = 1_000;
export const BACKOFF_MAX_MS = 30_000;
/** Rate limiting starts higher: the relay's buckets refill in seconds. */
const RATE_LIMIT_BASE_MS = 3_000;

export interface RetryContext {
  /** Failed attempts since the last successful session (0 for the first retry). */
  attempt: number;
  /** When the first 4029 of the current run arrived, or null. */
  rateLimitedSince: number | null;
  now: number;
  /** In [0, 1): jitter source. */
  random: () => number;
}

/** Exponential backoff with ±20 % jitter, capped. */
export function backoffMs(attempt: number, base: number, random: () => number): number {
  const raw = Math.min(BACKOFF_MAX_MS, base * 2 ** Math.min(attempt, 10));
  return Math.round(raw * (0.8 + 0.4 * random()));
}

/**
 * The decision after a close with `code`. `sessionOpened` tells whether the handshake had
 * completed: only a NORMAL close of an established session means "replaced".
 */
export function decideRetry(code: number, sessionOpened: boolean, context: RetryContext): RetryDecision {
  switch (code) {
    case CLOSE.UNAUTHORIZED:
      return { retry: false, stop: 'unauthorized' };
    case CLOSE.UPGRADE_REQUIRED:
      return { retry: false, stop: 'upgrade' };
    case CLOSE.RATE_LIMITED: {
      const since = context.rateLimitedSince ?? context.now;
      const left = since + RATE_LIMIT_WINDOW_MS - context.now;
      if (left <= 0) return { retry: false, stop: 'rate_limited' };
      return { retry: true, delayMs: Math.min(left, backoffMs(context.attempt, RATE_LIMIT_BASE_MS, context.random)) };
    }
    case CLOSE.NORMAL:
      if (sessionOpened) return { retry: false, stop: 'replaced' };
      return { retry: true, delayMs: backoffMs(context.attempt, BACKOFF_BASE_MS, context.random) };
    default:
      return { retry: true, delayMs: backoffMs(context.attempt, BACKOFF_BASE_MS, context.random) };
  }
}
