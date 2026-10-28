// Public surface of the remote transport. Test seams (`startInitiator`, `respondWith`) and the raw
// Noise state machines stay in their modules.

export { fromBase64Url, toBase64Url } from './bytes.ts';
export {
  DEFAULT_MAX_REASSEMBLY,
  DEFAULT_REKEY_INTERVAL_MS,
  DEFAULT_REKEY_MESSAGES,
  DEFAULT_SESSION_CONFIG,
  type DeviceHello,
  decodeDeviceHello,
  decodeWelcome,
  encodeDeviceHello,
  encodePairingInvite,
  encodeWelcome,
  FLAG_MORE,
  FLAG_REKEY,
  fingerprint,
  INVITE_PREFIX,
  Initiator,
  Keypair,
  MAX_FRAGMENT_DATA,
  MAX_HANDSHAKE_PAYLOAD,
  type Mode,
  NoiseError,
  type NoiseErrorCode,
  nextTimestamp,
  PAIRING_PATTERN,
  type PairingInvite,
  Pending,
  PROLOGUE,
  parsePairingInvite,
  RECONNECT_PATTERN,
  respond,
  Session,
  type SessionConfig,
  timestampFresh,
  type Welcome,
} from './protocol.ts';
export { isLowOrder } from './noise.ts';
export { CLOSE, DEVICE_PATH, encodeRelayHello, HEALTH_PATH, HELLO_OK, MAX_PAYLOAD, VERSION } from './relay.ts';
