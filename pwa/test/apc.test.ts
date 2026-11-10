import { describe, expect, test } from 'bun:test';
import { ApcStripper } from '../src/app/apc.ts';

const enc = new TextEncoder();
const dec = new TextDecoder();

/** Feeds `chunks` through one stripper and returns what xterm would get, as text. */
function strip(...chunks: string[]): string {
  const apc = new ApcStripper();
  return chunks.map((c) => dec.decode(apc.filter(enc.encode(c)), { stream: true })).join('');
}

describe('ApcStripper', () => {
  test('drops a TSP frame with non-ASCII JSON and keeps the text and escapes around it', () => {
    const frame = '\x1b_tsp;f;{"t":"Working…","k":"←"}\x1b\\';
    expect(strip(`a\x1b[1mb${frame}c\x1b[0m…`)).toBe('a\x1b[1mbc\x1b[0m…');
  });

  test('a frame split anywhere, even between ESC and its final byte, is still dropped', () => {
    expect(strip('x\x1b', '_tsp;f;{"a":"é', '…"}\x1b', '\\y')).toBe('xy');
    expect(strip('x\x1b', '[31my')).toBe('x\x1b[31my');
  });

  test('a UTF-8 continuation byte 0x9c (C1 ST in Latin-1) does not end the string', () => {
    // "œ" is C5 93, "ś" C5 9B, "Ŝ" C5 9C: the last one carries a 0x9c byte.
    expect(strip('\x1b_tsp;f;"Ŝ hidden"\x1b\\Ŝ')).toBe('Ŝ');
  });

  test('another ESC ends the string and starts a real escape; CAN aborts it', () => {
    expect(strip('\x1b_junk\x1b[2Jz')).toBe('\x1b[2Jz');
    expect(strip('\x1b_junk\x18z')).toBe('z');
    expect(strip('\x1b_one\x1b_two\x1b\\z')).toBe('z');
  });

  test('plain output and lone escapes pass through byte for byte', () => {
    expect(strip('\x1b\x1b[Aok\x1b]0;title\x07')).toBe('\x1b\x1b[Aok\x1b]0;title\x07');
  });
});
