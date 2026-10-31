import { describe, expect, test } from 'bun:test';
import { inviteFromHash, inviteFromText, sessionFromHash } from '../src/app/nav.ts';
import { DEFAULT_THEME, THEMES, loadTheme } from '../src/app/theme.ts';
import { GENERIC_PUSH, parsePush, safeNavigate } from '../src/push.ts';

describe('deep links', () => {
  test('reads the pairing invite from the hash', () => {
    expect(inviteFromHash('#pair=v1.abc_-')).toBe('v1.abc_-');
    expect(inviteFromHash('#s=1&pair=v1.x')).toBe('v1.x');
    expect(inviteFromHash('#s=1')).toBeNull();
    expect(inviteFromHash('')).toBeNull();
  });

  test('accepts a pasted link or a bare invite', () => {
    expect(inviteFromText('  https://relay.example/#pair=v1.AbC-_9  ')).toBe('v1.AbC-_9');
    expect(inviteFromText('v1.AbC-_9')).toBe('v1.AbC-_9');
    expect(inviteFromText('hello')).toBeNull();
  });

  test('reads a session deep link', () => {
    expect(sessionFromHash('#s=01HZX3J6V0Q8')).toBe('01HZX3J6V0Q8');
    expect(sessionFromHash('#s=')).toBeNull();
    expect(sessionFromHash('#pair=v1.x')).toBeNull();
  });
});

describe('themes', () => {
  test('six themes, Aurora glass by default', () => {
    expect(THEMES.map((theme) => theme.id).sort()).toEqual(['aurora', 'blueprint', 'field', 'margin', 'signal', 'tidal']);
    expect(DEFAULT_THEME).toBe('aurora');
    expect(loadTheme(null)).toBe('aurora');
    expect(loadTheme({ getItem: () => 'tidal' })).toBe('tidal');
    expect(loadTheme({ getItem: () => 'neon' })).toBe('aurora');
    expect(
      loadTheme({
        getItem: () => {
          throw new Error('denied');
        },
      }),
    ).toBe('aurora');
  });
});

const ORIGIN = 'https://relay.example';

describe('push payloads', () => {
  test('reads the declarative payload', () => {
    const payload = parsePush(
      JSON.stringify({ web_push: 8030, notification: { title: 'Fix the parser', body: 'Finished', navigate: '/#s=01HZ', tag: 't1', silent: true } }),
      ORIGIN,
    );
    expect(payload).toEqual({ title: 'Fix the parser', body: 'Finished', navigate: '/#s=01HZ', tag: 't1', silent: true });
  });

  test('keeps navigation on this origin', () => {
    const at = (navigate: string) => parsePush(JSON.stringify({ notification: { title: 'x', navigate } }), ORIGIN)?.navigate;
    expect(at('https://evil.example/')).toBe('/');
    expect(at('//evil.example/')).toBe('/');
    expect(at('//evil')).toBe('/');
    expect(at('/\\evil.example')).toBe('/');
    expect(at('/#s=1')).toBe('/#s=1');
    // The gateway's absolute same-origin URL deep-links (path and hash kept, query dropped).
    expect(at(`${ORIGIN}/#s=01HZ`)).toBe('/#s=01HZ');
    expect(at(`${ORIGIN}/app?x=1#s=2`)).toBe('/app#s=2');
    expect(at('https://relay.example.evil/#s=1')).toBe('/');
    expect(at('javascript:alert(1)')).toBe('/');
    expect(safeNavigate(42, ORIGIN)).toBe('/');
  });

  test('malformed payloads yield null (the worker shows the generic one)', () => {
    expect(parsePush('not json', ORIGIN)).toBeNull();
    expect(parsePush('{}', ORIGIN)).toBeNull();
    expect(parsePush(JSON.stringify({ notification: { body: 'no title' } }), ORIGIN)).toBeNull();
    expect(GENERIC_PUSH.title).toBe('OmpDeck');
  });
});
