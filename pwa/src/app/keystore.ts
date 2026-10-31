// The device's pairing, kept in IndexedDB (mobile-pwa.md "Keys"): the static secret, the gateway
// key, the relay URL, the room and admission token, and the `last` handshake timestamp. Everything
// lives in one record, so `last` is always persisted atomically with the key it belongs to, and it
// is persisted before the handshake that uses it is sent (a lost `last` would lock the device out
// until its clock passed the gateway's floor; mobile-relay.md R5).

import { nextTimestamp } from '../remote/index.ts';

const DB_NAME = 'ompdeck';
const DB_VERSION = 1;
const STORE = 'pairing';
const KEY = 'current';

/** One paired desktop. Byte fields are copies owned by the caller. */
export interface StoredPairing {
  /** The device's X25519 static secret (fresh for every pairing). */
  secret: Uint8Array;
  /** The gateway's static public key, from the invite. */
  gatewayPublic: Uint8Array;
  /** The canonical relay base URL (`wss://host[:port]`). */
  url: string;
  room: Uint8Array;
  /** This device's own relay admission token (from the pairing Welcome). */
  token: Uint8Array;
  /** The registry id the gateway gave this device (`d-…`). */
  device: string;
  /** The gateway's label (its desktop's name). */
  gateway: string;
  /** The name this device announced when it paired. */
  name: string;
  /** The last handshake timestamp sent with this key (unix ms). */
  last: number;
  pairedUnixMs: number;
}

export class KeyStoreError extends Error {
  override name = 'KeyStoreError';
}

function promised<T>(request: IDBRequest<T>): Promise<T> {
  const { promise, resolve, reject } = Promise.withResolvers<T>();
  request.onsuccess = () => resolve(request.result);
  request.onerror = () => reject(new KeyStoreError(request.error?.message ?? 'IndexedDB request failed'));
  return promise;
}

function finished(tx: IDBTransaction): Promise<void> {
  const { promise, resolve, reject } = Promise.withResolvers<void>();
  tx.oncomplete = () => resolve();
  tx.onabort = () => reject(new KeyStoreError(tx.error?.message ?? 'IndexedDB transaction aborted'));
  tx.onerror = () => reject(new KeyStoreError(tx.error?.message ?? 'IndexedDB transaction failed'));
  return promise;
}

function bytes(value: unknown): Uint8Array | null {
  return value instanceof Uint8Array ? Uint8Array.from(value) : null;
}

/** Reads a stored record defensively: a damaged or foreign record counts as "not paired". */
function decode(value: unknown): StoredPairing | null {
  if (typeof value !== 'object' || value === null) return null;
  const record = value as { [key: string]: unknown };
  const secret = bytes(record['secret']);
  const gatewayPublic = bytes(record['gatewayPublic']);
  const room = bytes(record['room']);
  const token = bytes(record['token']);
  const { url, device, gateway, name, last, pairedUnixMs } = record;
  if (
    secret?.length !== 32 ||
    gatewayPublic?.length !== 32 ||
    room?.length !== 16 ||
    token?.length !== 32 ||
    typeof url !== 'string' ||
    typeof device !== 'string' ||
    typeof gateway !== 'string' ||
    typeof name !== 'string' ||
    typeof last !== 'number' ||
    typeof pairedUnixMs !== 'number'
  ) {
    return null;
  }
  return { secret, gatewayPublic, url, room, token, device, gateway, name, last, pairedUnixMs };
}

function encode(pairing: StoredPairing): StoredPairing {
  return {
    ...pairing,
    secret: Uint8Array.from(pairing.secret),
    gatewayPublic: Uint8Array.from(pairing.gatewayPublic),
    room: Uint8Array.from(pairing.room),
    token: Uint8Array.from(pairing.token),
  };
}

export class KeyStore {
  readonly #db: IDBDatabase;

  private constructor(db: IDBDatabase) {
    this.#db = db;
  }

  static async open(factory: IDBFactory = indexedDB): Promise<KeyStore> {
    const request = factory.open(DB_NAME, DB_VERSION);
    request.onupgradeneeded = () => {
      if (!request.result.objectStoreNames.contains(STORE)) request.result.createObjectStore(STORE);
    };
    return new KeyStore(await promised(request));
  }

  /** The current pairing, or null (never paired, forgotten, or evicted: re-pair). */
  async load(): Promise<StoredPairing | null> {
    const tx = this.#db.transaction(STORE, 'readonly');
    const value = await promised(tx.objectStore(STORE).get(KEY));
    return decode(value);
  }

  /** Replaces the pairing (a new pairing always brings a fresh key). */
  async save(pairing: StoredPairing): Promise<void> {
    const tx = this.#db.transaction(STORE, 'readwrite');
    tx.objectStore(STORE).put(encode(pairing), KEY);
    await finished(tx);
  }

  /**
   * Reserves the next handshake timestamp for the stored key: max(now, last + 1), written in the
   * same transaction that read `last`. Resolves only after it is durable, so it is safe to send.
   */
  async reserveTimestamp(nowMs: number): Promise<number> {
    const tx = this.#db.transaction(STORE, 'readwrite');
    const store = tx.objectStore(STORE);
    let ts: number | null = null;
    const read = store.get(KEY);
    read.onsuccess = () => {
      const current = decode(read.result);
      if (current === null) {
        tx.abort();
        return;
      }
      ts = nextTimestamp(current.last, nowMs);
      store.put(encode({ ...current, last: ts }), KEY);
    };
    try {
      await finished(tx);
    } catch (error) {
      if (ts === null) throw new KeyStoreError('no pairing is stored');
      throw error;
    }
    if (ts === null) throw new KeyStoreError('no pairing is stored');
    return ts;
  }

  /** Forgets the pairing on this device (user action only; never because of a close code). */
  async clear(): Promise<void> {
    const tx = this.#db.transaction(STORE, 'readwrite');
    tx.objectStore(STORE).delete(KEY);
    await finished(tx);
  }

  close(): void {
    this.#db.close();
  }
}
