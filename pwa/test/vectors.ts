// Loads the shared vector files (crates/deck-remote/tests/vectors).

import { readFileSync } from 'node:fs';

const DIR = new URL('../../crates/deck-remote/tests/vectors/', import.meta.url);

export interface FrameVector {
  flags: number;
  len: number;
  sha256: string;
  hex?: string;
}

export interface StepVector {
  from: 'device' | 'gateway';
  plaintext_hex?: string;
  plaintext_generated?: number;
  frames: FrameVector[];
}

export interface HandshakeVector {
  mode: 'pairing' | 'reconnect';
  pattern: string;
  welcome: { device: string; gateway: string; token: string | null };
  welcome_payload: string;
  msg1: string;
  msg2: string;
  handshake_hash: string;
  transport: StepVector[];
}

export interface SharedVectors {
  prologue: string;
  plaintext_generator: string;
  max_fragment_data: number;
  rekey_messages: number;
  device_static: string;
  device_public: string;
  device_fingerprint: string;
  device_ephemeral: string;
  gateway_static: string;
  gateway_public: string;
  gateway_fingerprint: string;
  gateway_ephemeral: string;
  psk: string;
  hello: { name: string; platform: string; ts: number };
  hello_payload: string;
  invite: { url: string; room: string; token: string; gateway_public: string; psk: string; encoded: string };
  relay_hello: string;
  handshakes: HandshakeVector[];
}

export interface OfficialVector {
  protocol_name: string;
  init_prologue: string;
  init_static: string;
  init_ephemeral: string;
  init_remote_static: string;
  init_psks?: string[];
  resp_prologue: string;
  resp_static: string;
  resp_ephemeral: string;
  resp_psks?: string[];
  handshake_hash: string;
  messages: { payload: string; ciphertext: string }[];
}

export type PayloadVector<T> = { case: string; hex: string } & ({ accept: true; expect: T } | { accept: false });

export interface PayloadVectors {
  hello: PayloadVector<{ name: string; platform: string; ts: number }>[];
  welcome: PayloadVector<{ device: string; gateway: string; token: string | null }>[];
}

// Fixtures checked into this repository; the tests themselves fail on any shape mismatch.
export function sharedVectors(): SharedVectors {
  const file: SharedVectors = JSON.parse(readFileSync(new URL('noise-v1.json', DIR), 'utf8'));
  return file;
}

export function officialVectors(): OfficialVector[] {
  const file: { vectors: OfficialVector[] } = JSON.parse(readFileSync(new URL('cacophony-ik.json', DIR), 'utf8'));
  return file.vectors;
}

export function payloadVectors(): PayloadVectors {
  const file: PayloadVectors = JSON.parse(readFileSync(new URL('payload-v1.json', DIR), 'utf8'));
  return file;
}

/** Byte i = (7 * i + 3) mod 256, as documented in noise-v1.json. */
export function generated(len: number): Uint8Array {
  const out = new Uint8Array(len);
  for (let i = 0; i < len; i++) out[i] = (7 * i + 3) & 0xff;
  return out;
}
