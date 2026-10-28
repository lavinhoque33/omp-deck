// Strict reader of the handshake payload grammar, mirroring Rust `deck_remote::noise::payload`
// (shared accept/reject vectors: crates/deck-remote/tests/vectors/payload-v1.json).
//
// The payload is one flat JSON object of valid UTF-8 (no BOM). Values are strings or integers
// only; integers are bare digits (no sign, fraction or exponent, no leading zeros except `0`)
// up to 2^53 − 1. Strings must be valid Unicode (a lone surrogate escape is rejected). Any
// duplicate key, known or unknown, compared after unescaping, is rejected. JSON.parse is not
// used: it keeps the last duplicate and accepts floats, nesting and lone surrogates.

export type PayloadValue = string | number;

// `ignoreBOM` keeps a leading U+FEFF in the text, where the grammar rejects it.
const decoder = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });

/** Parses a payload, or returns `null` if it is not in the grammar. */
export function parsePayload(bytes: Uint8Array): Map<string, PayloadValue> | null {
  let text: string;
  try {
    text = decoder.decode(bytes);
  } catch {
    return null;
  }
  const scanner = new Scanner(text);
  const fields = scanner.object();
  if (fields === null) return null;
  scanner.skipWs();
  return scanner.pos === text.length ? fields : null;
}

const QUOTE = 0x22;
const BACKSLASH = 0x5c;

class Scanner {
  readonly #text: string;
  pos = 0;

  constructor(text: string) {
    this.#text = text;
  }

  #peek(): number {
    // NaN past the end: matches nothing.
    return this.#text.charCodeAt(this.pos);
  }

  #next(): number {
    return this.#text.charCodeAt(this.pos++);
  }

  skipWs(): void {
    for (;;) {
      const c = this.#peek();
      if (c !== 0x20 && c !== 0x09 && c !== 0x0a && c !== 0x0d) return;
      this.pos++;
    }
  }

  object(): Map<string, PayloadValue> | null {
    const fields = new Map<string, PayloadValue>();
    this.skipWs();
    if (this.#next() !== 0x7b) return null;
    this.skipWs();
    if (this.#peek() === 0x7d) {
      this.pos++;
      return fields;
    }
    for (;;) {
      this.skipWs();
      if (this.#next() !== QUOTE) return null;
      const key = this.#string();
      if (key === null || fields.has(key)) return null;
      this.skipWs();
      if (this.#next() !== 0x3a) return null;
      this.skipWs();
      const c = this.#peek();
      let value: PayloadValue | null;
      if (c === QUOTE) {
        this.pos++;
        value = this.#string();
      } else if (c >= 0x30 && c <= 0x39) {
        value = this.#integer();
      } else {
        return null;
      }
      if (value === null) return null;
      fields.set(key, value);
      this.skipWs();
      const end = this.#next();
      if (end === 0x7d) return fields;
      if (end !== 0x2c) return null;
    }
  }

  /** After the opening quote; the text came from strict UTF-8, so it is well formed. */
  #string(): string | null {
    let out = '';
    for (;;) {
      const start = this.pos;
      let c = this.#peek();
      while (c === c && c !== QUOTE && c !== BACKSLASH && c >= 0x20) c = this.#text.charCodeAt(++this.pos);
      out += this.#text.slice(start, this.pos);
      c = this.#next();
      if (c === QUOTE) return out;
      if (c !== BACKSLASH) return null;
      const escaped = this.#escape();
      if (escaped === null) return null;
      out += escaped;
    }
  }

  #escape(): string | null {
    switch (this.#next()) {
      case QUOTE:
        return '"';
      case BACKSLASH:
        return '\\';
      case 0x2f:
        return '/';
      case 0x62:
        return '\b';
      case 0x66:
        return '\f';
      case 0x6e:
        return '\n';
      case 0x72:
        return '\r';
      case 0x74:
        return '\t';
      case 0x75: {
        const unit = this.#hex4();
        if (unit === null || (unit >= 0xdc00 && unit <= 0xdfff)) return null;
        if (unit < 0xd800 || unit > 0xdbff) return String.fromCharCode(unit);
        if (this.#next() !== BACKSLASH || this.#next() !== 0x75) return null;
        const low = this.#hex4();
        if (low === null || low < 0xdc00 || low > 0xdfff) return null;
        return String.fromCharCode(unit, low);
      }
      default:
        return null;
    }
  }

  #hex4(): number | null {
    let unit = 0;
    for (let i = 0; i < 4; i++) {
      const c = this.#next();
      let digit: number;
      if (c >= 0x30 && c <= 0x39) digit = c - 0x30;
      else if (c >= 0x41 && c <= 0x46) digit = c - 0x37;
      else if (c >= 0x61 && c <= 0x66) digit = c - 0x57;
      else return null;
      unit = (unit << 4) | digit;
    }
    return unit;
  }

  #integer(): number | null {
    if (this.#peek() === 0x30) {
      this.pos++;
      const c = this.#peek();
      return c >= 0x30 && c <= 0x39 ? null : 0;
    }
    let value = 0;
    for (let c = this.#peek(); c >= 0x30 && c <= 0x39; c = this.#peek()) {
      this.pos++;
      // Exact up to 2^53 − 1; anything larger compares above it, rounded or not.
      value = value * 10 + (c - 0x30);
      if (value > Number.MAX_SAFE_INTEGER) return null;
    }
    return value;
  }
}
