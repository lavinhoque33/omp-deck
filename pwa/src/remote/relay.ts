// Device side of the relay wire format, mirroring Rust `deck_remote::relay`
// (crates/deck-remote/src/relay.rs). After the hello, every binary WebSocket message is one
// opaque Noise message of at most MAX_PAYLOAD bytes.

import type { Bytes } from './bytes.ts';

/** Relay protocol version carried in every hello. */
export const VERSION = 1;
export const ROOM_LEN = 16;
export const TOKEN_LEN = 32;
/** Largest device payload: one Noise message. */
export const MAX_PAYLOAD = 65_535;

export const DEVICE_PATH = '/v1/device';
export const HEALTH_PATH = '/healthz';

/** The relay's answer to an accepted hello. */
export const HELLO_OK: Readonly<Bytes> = Uint8Array.of(0);

const HELLO_DEVICE = 0x44; // 'D'

/** WebSocket close codes the relay uses. */
export const CLOSE = {
  /** Normal close. */
  NORMAL: 1000,
  /** The relay is shutting down. */
  GOING_AWAY: 1001,
  /** A text message or an undecodable message. */
  PROTOCOL: 1002,
  /** The first message is not a valid hello or did not arrive in time. */
  BAD_HELLO: 4001,
  /** Unknown room, wrong room secret, unknown or revoked device token. */
  UNAUTHORIZED: 4003,
  /**
   * The gateway closed a device that broke the application protocol (undecodable record,
   * unknown record kind, message over the device limit).
   */
  APP_PROTOCOL: 4002,
  /** A device connected while its room's gateway is not connected. */
  GATEWAY_OFFLINE: 4004,
  /** The peer did not read fast enough; its queue filled up. */
  SLOW_CONSUMER: 4008,
  /** A newer gateway connection for the same room took over. */
  REPLACED: 4009,
  /** The gateway closed a device that stopped acknowledging its messages. */
  STALLED: 4010,
  /** A message exceeded its size limit. */
  TOO_LARGE: 4013,
  /** Too many connections, devices or bytes/messages per second. */
  RATE_LIMITED: 4029,
} as const;

/** A device's first message: `'D' ‖ version ‖ room 16 ‖ admission token 32`. */
export function encodeRelayHello(room: Uint8Array, token: Uint8Array): Bytes {
  if (room.length !== ROOM_LEN) throw new Error('room must be 16 bytes');
  if (token.length !== TOKEN_LEN) throw new Error('token must be 32 bytes');
  const out = new Uint8Array(2 + ROOM_LEN + TOKEN_LEN);
  out[0] = HELLO_DEVICE;
  out[1] = VERSION;
  out.set(room, 2);
  out.set(token, 2 + ROOM_LEN);
  return out;
}
