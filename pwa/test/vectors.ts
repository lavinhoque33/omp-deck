// Loads the shared vector files (crates/deck-remote/tests/vectors).

import { readFileSync } from 'node:fs';
import { sha256 } from '@noble/hashes/sha2.js';

import { toHex } from '../src/remote/bytes.ts';

const DIR = new URL('../../crates/deck-remote/tests/vectors/', import.meta.url);

function load<T>(name: string): T {
  const file: T = JSON.parse(readFileSync(new URL(name, DIR), 'utf8'));
  return file;
}

/** A vector file's actual SHA-256 and the one README.md records for it (`null`: none recorded). */
export function recordedSha256(name: string): { actual: string; recorded: string | null } {
  const readme = readFileSync(new URL('README.md', DIR), 'utf8');
  const marker = `\`${name}\` SHA-256: \``;
  const at = readme.indexOf(marker);
  return {
    actual: toHex(sha256(readFileSync(new URL(name, DIR)))),
    recorded: at === -1 ? null : readme.slice(at + marker.length, at + marker.length + 64),
  };
}

/** The vector name of an error (`snake_case`), as NoiseError codes are kebab-case. */
export type VectorError =
  | 'handshake'
  | 'unsupported_version'
  | 'payload'
  | 'decrypt'
  | 'bad_flags'
  | 'frame_size'
  | 'too_large';

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
  hello: { name: string; platform: string; ts: number; api: number; features: string[] };
  hello_payload: string;
  invite: { url: string; room: string; token: string; gateway_public: string; psk: string; encoded: string };
  relay_hello: string;
  handshakes: HandshakeVector[];
  rejected_handshakes: RejectedHandshake[];
  rejected_frames: RejectedFrames[];
}

export type RejectedHandshake = { case: string; mode: 'pairing' | 'reconnect'; error: VectorError } & (
  | { receiver: 'gateway'; msg1: string }
  | { receiver: 'device'; msg2: string }
  | { receiver: 'device'; gateway_public: string }
);

export interface RejectedFrames {
  case: string;
  receiver: 'device' | 'gateway';
  max_message: number | null;
  frames: string[];
  error: VectorError;
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

export type PayloadVector<T> = { case: string; hex: string } & (
  | { accept: true; expect: T }
  | { accept: false; error?: 'unsupported_version' }
);

export interface PayloadVectors {
  hello: PayloadVector<{ name: string; platform: string; ts: number }>[];
  welcome: PayloadVector<{ device: string; gateway: string; token: string | null }>[];
}

export type HelloFieldVector = { case: string; payload: string } & (
  | {
      accept: true;
      expect: { name: string; platform: string; ts: number; api: number; features: string[] };
      encoded: string;
    }
  | { accept: false; error: 'payload' | 'unsupported_version' }
);

export interface HelloVectors {
  display_safe: { case: string; text: string; safe: boolean }[];
  hello: HelloFieldVector[];
}

export interface InviteVectors {
  fields: { room: string; token: string; gateway_public: string; psk: string };
  urls: { case: string; url: string; accept: boolean; invite: string | null }[];
  invites: { case: string; text: string; accept: boolean }[];
}

export interface RelayMessageVector<F> {
  case: string;
  hex?: string;
  len?: number;
  sha256?: string;
  fields: F;
}

export interface RelayVectors {
  version: number;
  paths: { gateway: string; device: string; health: string };
  max_payload: number;
  hello_ok: string;
  hello_ok_accepted: string[];
  hello_ok_rejected: string[];
  close_codes: Record<string, number>;
  hellos: RelayMessageVector<
    { role: 'device'; room: string; token: string } | { role: 'gateway'; room: string; secret: string; epoch: number }
  >[];
}

// Fixtures checked into this repository; the tests themselves fail on any shape mismatch.
export function sharedVectors(): SharedVectors {
  return load('noise-v1.json');
}

export function officialVectors(): OfficialVector[] {
  return load<{ vectors: OfficialVector[] }>('cacophony-ik.json').vectors;
}

export function payloadVectors(): PayloadVectors {
  return load('payload-v1.json');
}

export function helloVectors(): HelloVectors {
  return load('hello-v1.json');
}

export function inviteVectors(): InviteVectors {
  return load('invite-v1.json');
}

export function relayVectors(): RelayVectors {
  return load('relay-v1.json');
}

/** Byte i = (7 * i + 3) mod 256, as documented in noise-v1.json. */
export function generated(len: number): Uint8Array {
  const out = new Uint8Array(len);
  for (let i = 0; i < len; i++) out[i] = (7 * i + 3) & 0xff;
  return out;
}
