// Pairing invite acceptance against crates/deck-remote/tests/vectors/invite-v1.json, which the
// Rust tests (tests/invite.rs) check too: the canonical relay URL grammar and whole invite texts.

import { describe, expect, test } from 'bun:test';

import { fromHex } from '../src/remote/bytes.ts';
import { encodePairingInvite, NoiseError, type PairingInvite, parsePairingInvite } from '../src/remote/protocol.ts';
import { inviteVectors, recordedSha256 } from './vectors.ts';

const vectors = inviteVectors();

function invite(url: string): PairingInvite {
  return {
    url,
    room: fromHex(vectors.fields.room),
    token: fromHex(vectors.fields.token),
    gatewayPublic: fromHex(vectors.fields.gateway_public),
    psk: fromHex(vectors.fields.psk),
  };
}

/** `'invite'` when `run` fails with an invite error, else what it returned. */
function outcome<T>(run: () => T): T | 'invite' {
  try {
    return run();
  } catch (error) {
    if (error instanceof NoiseError && error.code === 'invite') return 'invite';
    throw error;
  }
}

describe('invite vectors', () => {
  test('relay URLs', () => {
    expect(vectors.urls.length).toBeGreaterThan(90);
    for (const vector of vectors.urls) {
      const expected = invite(vector.url);
      if (vector.accept) {
        expect([vector.case, encodePairingInvite(expected)]).toEqual([vector.case, vector.invite!]);
        expect([vector.case, parsePairingInvite(vector.invite!)]).toEqual([vector.case, expected]);
      } else {
        expect([vector.case, outcome(() => encodePairingInvite(expected))]).toEqual([vector.case, 'invite']);
        if (vector.invite !== null) {
          const text = vector.invite;
          expect([vector.case, outcome(() => parsePairingInvite(text))]).toEqual([vector.case, 'invite']);
        }
      }
    }
  });

  test('invite texts', () => {
    for (const vector of vectors.invites) {
      if (vector.accept) {
        const parsed = parsePairingInvite(vector.text);
        expect([vector.case, parsed]).toEqual([vector.case, invite(parsed.url)]);
        expect(encodePairingInvite(parsed)).toBe(vector.text);
      } else {
        expect([vector.case, outcome(() => parsePairingInvite(vector.text))]).toEqual([vector.case, 'invite']);
      }
    }
  });

  test('the file matches its recorded SHA-256', () => {
    const { actual, recorded } = recordedSha256('invite-v1.json');
    expect(actual).toBe(recorded ?? 'not recorded in README.md');
  });
});
