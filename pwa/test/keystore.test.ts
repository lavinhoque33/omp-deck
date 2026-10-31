import { describe, expect, test } from 'bun:test';
import { IDBFactory } from 'fake-indexeddb';
import { KeyStore, type StoredPairing } from '../src/app/keystore.ts';

function pairing(patch: Partial<StoredPairing> = {}): StoredPairing {
  return {
    secret: new Uint8Array(32).fill(7),
    gatewayPublic: new Uint8Array(32).fill(9),
    url: 'wss://relay.example',
    room: new Uint8Array(16).fill(1),
    token: new Uint8Array(32).fill(2),
    device: 'd-0123456789abcdef',
    gateway: 'axel-desktop',
    name: 'Pixel',
    last: 1000,
    pairedUnixMs: 1000,
    ...patch,
  };
}

describe('KeyStore', () => {
  test('starts empty, then round-trips a pairing', async () => {
    const store = await KeyStore.open(new IDBFactory());
    expect(await store.load()).toBeNull();
    await store.save(pairing());
    const loaded = await store.load();
    expect(loaded).toEqual(pairing());
    store.close();
  });

  test('the stored pairing survives reopening the database', async () => {
    const factory = new IDBFactory();
    const first = await KeyStore.open(factory);
    await first.save(pairing({ name: 'Phone A' }));
    first.close();
    const second = await KeyStore.open(factory);
    expect((await second.load())?.name).toBe('Phone A');
    second.close();
  });

  test('reserveTimestamp is monotonic and persisted with the key', async () => {
    const factory = new IDBFactory();
    const store = await KeyStore.open(factory);
    await store.save(pairing({ last: 5000 }));
    // The clock went backwards: still strictly after the last one sent.
    expect(await store.reserveTimestamp(4000)).toBe(5001);
    expect(await store.reserveTimestamp(4000)).toBe(5002);
    expect(await store.reserveTimestamp(9000)).toBe(9000);
    store.close();
    const reopened = await KeyStore.open(factory);
    expect((await reopened.load())?.last).toBe(9000);
    expect((await reopened.load())?.secret).toEqual(new Uint8Array(32).fill(7));
    reopened.close();
  });

  test('reserveTimestamp without a pairing fails (re-pair)', async () => {
    const store = await KeyStore.open(new IDBFactory());
    await expect(store.reserveTimestamp(1)).rejects.toThrow();
    store.close();
  });

  test('clear forgets the pairing', async () => {
    const store = await KeyStore.open(new IDBFactory());
    await store.save(pairing());
    await store.clear();
    expect(await store.load()).toBeNull();
    store.close();
  });

  test('loaded byte fields are copies', async () => {
    const store = await KeyStore.open(new IDBFactory());
    await store.save(pairing());
    const first = await store.load();
    first?.secret.fill(0);
    expect((await store.load())?.secret).toEqual(new Uint8Array(32).fill(7));
    store.close();
  });
});
