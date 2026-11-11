// What the Android shell changes about the app: notification taps wait for a ready app and name the
// right desktop, Back steps back before the Activity closes, visibility is the Activity's, and the
// shell hears which desktop is really on screen.

import { describe, expect, test } from 'bun:test';
import { setImmediate } from 'node:timers/promises';
import {
  OpenRouter,
  PresenceSource,
  type PresenceClient,
  type WatchedDesktops,
  linkFromOpenUrl,
  nativeLabels,
  pressBack,
  watchNativeLabels,
  watchNativePresence,
} from '../src/app/native-app.ts';
import type { NativeCalls } from '../src/app/native.ts';

const A = 'ab'.repeat(32);
const B = 'cd'.repeat(32);
const BASE = 'https://appassets.androidplatform.net/assets/www/index.html';

describe('notification taps', () => {
  test('the url is path and hash; the hash names desktop and session', () => {
    const link = linkFromOpenUrl(`/#s=sess-1&d=${A}`, BASE);
    expect(link?.desktop).toBe(A);
    expect(link?.route?.stack[0]).toMatchObject({ kind: 'conversation', session: 'sess-1' });
    expect(linkFromOpenUrl(`/#d=${B}`, BASE)).toEqual({ desktop: B, route: null });
  });

  test('urls that name nothing (or are not urls) open nothing', () => {
    expect(linkFromOpenUrl('/', BASE)).toBeNull();
    expect(linkFromOpenUrl('/#d=not-a-desktop', BASE)).toBeNull();
    expect(linkFromOpenUrl('http://[', BASE)).toBeNull();
  });

  test('a tap before the app is ready waits for the desktops AND the handler, then opens once', () => {
    const router = new OpenRouter(BASE);
    const opened: unknown[] = [];
    router.open(`/#s=one&d=${A}`);
    router.attach((link) => opened.push(link));
    expect(opened).toEqual([]);
    router.ready();
    expect(opened).toHaveLength(1);
    expect(opened[0]).toMatchObject({ desktop: A });
    // Replaying handlers or readiness never re-delivers it.
    router.attach((link) => opened.push(link));
    router.ready();
    expect(opened).toHaveLength(1);
  });

  test('only the newest waiting tap is acted on, each for its own desktop', () => {
    const router = new OpenRouter(BASE);
    const opened: (string | null)[] = [];
    router.open(`/#s=one&d=${A}`);
    router.open(`/#s=two&d=${B}`);
    router.open('/#d=garbage');
    router.attach((link) => opened.push(link.desktop));
    router.ready();
    expect(opened).toEqual([B]);
    // Once ready, taps go straight through.
    router.open(`/#s=three&d=${A}`);
    expect(opened).toEqual([B, A]);
  });

  test('a handler that went away holds the next tap for its replacement', () => {
    const router = new OpenRouter(BASE);
    const opened: (string | null)[] = [];
    router.ready();
    router.attach(null);
    router.open(`/#d=${A}`);
    expect(opened).toEqual([]);
    router.attach((link) => opened.push(link.desktop));
    expect(opened).toEqual([A]);
  });
});

describe('Back', () => {
  test('the app steps back first; the Activity finishes only when it could not', () => {
    const log: string[] = [];
    pressBack(() => true, () => log.push('finish'));
    expect(log).toEqual([]);
    pressBack(() => false, () => log.push('finish'));
    pressBack(null, () => log.push('finish'));
    expect(log).toEqual(['finish', 'finish']);
  });
});

describe('visibility', () => {
  test('before the shell reports, the document decides; after, both must agree', () => {
    const source = new PresenceSource(true);
    const seen: boolean[] = [];
    source.subscribe(() => seen.push(source.get()));
    source.setNative(false);
    // The document stays "visible" while the Activity is stopped: still hidden.
    expect(source.get()).toBe(false);
    source.setDocument(true);
    expect(source.get()).toBe(false);
    source.setNative(true);
    expect(source.get()).toBe(true);
    source.setDocument(false);
    expect(source.get()).toBe(false);
    source.setDocument(false);
    expect(seen).toEqual([false, true, false]);
  });

  test('a hidden app stays hidden for a client built after a desktop switch', () => {
    const source = new PresenceSource(true);
    source.setNative(false);
    // `createClient` reads the source, not the (still "visible") document.
    expect(source.get()).toBe(false);
  });

  test('a hidden document is hidden whatever the shell last said', () => {
    const source = new PresenceSource(false);
    source.setNative(true);
    expect(source.get()).toBe(false);
  });
});

class FakeClient implements PresenceClient {
  online = false;
  readonly listeners = new Set<() => void>();
  subscribeState(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }
  set(online: boolean): void {
    this.online = online;
    for (const listener of [...this.listeners]) listener();
  }
}

class FakeDesktops implements WatchedDesktops {
  loaded = true;
  activeId: string | null = null;
  client: FakeClient | null = null;
  desktops: { id: string; gateway: string }[] = [];
  readonly listeners = new Set<() => void>();
  getState() {
    return { loaded: this.loaded, activeId: this.activeId, client: this.client, desktops: this.desktops };
  }
  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }
  change(patch: Partial<Pick<FakeDesktops, 'loaded' | 'activeId' | 'client' | 'desktops'>>): void {
    Object.assign(this, patch);
    for (const listener of [...this.listeners]) listener();
  }
}

type Call = { method: keyof NativeCalls; params: unknown };

class FakeHost {
  readonly calls: Call[] = [];
  fail = false;
  request(method: keyof NativeCalls, params: unknown): Promise<never> {
    this.calls.push({ method, params });
    return this.fail ? Promise.reject(new Error('down')) : (Promise.resolve(null) as Promise<never>);
  }
}

/** Lets rejected promises run their handlers (an event-loop turn, no real time). */
const flush = () => setImmediate();

describe('presence reported to the shell', () => {
  test('only the active desktop, only while the app is foreground AND connected to it', () => {
    const host = new FakeHost();
    const desktops = new FakeDesktops();
    const source = new PresenceSource(true);
    const first = new FakeClient();
    desktops.change({ activeId: A, client: first });
    watchNativePresence(host, desktops, source);
    // Not connected yet: the gateway would still push, so the shell must not mute.
    first.set(true);
    first.set(false);
    first.set(true);
    source.setNative(false);
    source.setNative(true);
    expect(host.calls.map((call) => call.params)).toEqual([
      { desktop: A, visible: false },
      { desktop: A, visible: true },
      { desktop: A, visible: false },
      { desktop: A, visible: true },
      { desktop: A, visible: false },
      { desktop: A, visible: true },
    ]);
  });

  test('switching desktops moves the presence and keeps a background state hidden', () => {
    const host = new FakeHost();
    const desktops = new FakeDesktops();
    const source = new PresenceSource(true);
    const first = new FakeClient();
    first.online = true;
    desktops.change({ activeId: A, client: first });
    watchNativePresence(host, desktops, source);
    source.setNative(false);
    host.calls.length = 0;
    // The user opens a notification for B while the app comes to the foreground: B is not connected yet.
    const second = new FakeClient();
    desktops.change({ activeId: B, client: second });
    expect(host.calls.map((call) => call.params)).toEqual([{ desktop: B, visible: false }]);
    source.setNative(true);
    expect(host.calls).toHaveLength(1);
    second.set(true);
    expect(host.calls.at(-1)?.params).toEqual({ desktop: B, visible: true });
    // The old client's late state changes are no longer followed.
    first.set(false);
    expect(host.calls).toHaveLength(2);
  });

  test('no desktop at all reports nothing on screen; repeats are not re-sent', () => {
    const host = new FakeHost();
    const desktops = new FakeDesktops();
    const source = new PresenceSource(true);
    watchNativePresence(host, desktops, source);
    desktops.change({ activeId: null, client: null });
    desktops.change({});
    expect(host.calls).toEqual([{ method: 'setPresence', params: { desktop: null, visible: false } }]);
  });

  test('a report the shell refused is repeated at the next change, not assumed delivered', async () => {
    const host = new FakeHost();
    const desktops = new FakeDesktops();
    const source = new PresenceSource(true);
    const client = new FakeClient();
    client.online = true;
    desktops.change({ activeId: A, client });
    host.fail = true;
    watchNativePresence(host, desktops, source);
    await flush();
    host.fail = false;
    // Same state again: re-sent because the first one never arrived.
    desktops.change({});
    expect(host.calls).toHaveLength(2);
    expect(host.calls[1]?.params).toEqual({ desktop: A, visible: true });
    desktops.change({});
    expect(host.calls).toHaveLength(2);
  });

  test('stopping the watch stops every subscription', () => {
    const host = new FakeHost();
    const desktops = new FakeDesktops();
    const source = new PresenceSource(true);
    const client = new FakeClient();
    desktops.change({ activeId: A, client });
    const stop = watchNativePresence(host, desktops, source);
    stop();
    client.set(true);
    source.setNative(false);
    desktops.change({ activeId: B });
    expect(host.calls).toHaveLength(1);
    expect(client.listeners.size).toBe(0);
    expect(desktops.listeners.size).toBe(0);
  });
});

describe('labels reported to the shell', () => {
  test('every paired desktop, even a single one, once the pairings are read', () => {
    expect(nativeLabels([])).toEqual({});
    expect(nativeLabels([{ id: A, gateway: 'Work' }])).toEqual({ [A]: 'Work' });
    const host = new FakeHost();
    const desktops = new FakeDesktops();
    desktops.loaded = false;
    watchNativeLabels(host, desktops);
    // Before the pairings are read an empty list would mean "none paired".
    expect(host.calls).toEqual([]);
    desktops.change({ loaded: true, desktops: [{ id: A, gateway: 'Work' }] });
    expect(host.calls).toEqual([{ method: 'pushLabels', params: { labels: { [A]: 'Work' } } }]);
  });

  test('added, renamed and forgotten desktops are re-sent; unchanged lists are not', () => {
    const host = new FakeHost();
    const desktops = new FakeDesktops();
    desktops.desktops = [{ id: A, gateway: 'Work' }];
    watchNativeLabels(host, desktops);
    desktops.change({ activeId: A });
    desktops.change({ desktops: [{ id: A, gateway: 'Work' }, { id: B, gateway: 'Home' }] });
    desktops.change({ desktops: [{ id: A, gateway: 'Work · ab12' }, { id: B, gateway: 'Home' }] });
    desktops.change({ desktops: [{ id: B, gateway: 'Home' }] });
    expect(host.calls.map((call) => call.params)).toEqual([
      { labels: { [A]: 'Work' } },
      { labels: { [A]: 'Work', [B]: 'Home' } },
      { labels: { [A]: 'Work · ab12', [B]: 'Home' } },
      { labels: { [B]: 'Home' } },
    ]);
  });

  test('a refused report is repeated at the next change', async () => {
    const host = new FakeHost();
    const desktops = new FakeDesktops();
    desktops.desktops = [{ id: A, gateway: 'Work' }];
    host.fail = true;
    watchNativeLabels(host, desktops);
    await flush();
    host.fail = false;
    desktops.change({});
    expect(host.calls).toHaveLength(2);
  });
});
