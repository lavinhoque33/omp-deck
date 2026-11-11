// Native Android push: what is gated on the gateway's `android_push`, which transitions send a
// registration (and which never do), that a refresh keeps the prefs, and that turning push off or
// forgetting a desktop drops exactly that desktop's gateway subscription and key.

import { describe, expect, test, vi } from 'bun:test';
import { DESKTOP_CONFIG_HELP, DESKTOP_SENDER_HELP, NativePushController, advertisesAndroidPush, dropNativePush, storageMarkers, type PushMarkers } from '../src/app/native-push.ts';
import type { NativeCalls, NativeMethod, NativePushState } from '../src/app/native.ts';
import type { api } from '../src/remote/index.ts';

const A = 'ab'.repeat(32);
const B = 'cd'.repeat(32);
const KEY = 'B'.repeat(43);

function welcome(features: string[] = ['s4', 'android_push']): api.Welcome {
  return {
    protocol: 1,
    gateway_version: '0.27.3',
    epoch: '00112233445566778899aabbccddeeff',
    device: 'd-0123456789abcdef',
    name: 'Pixel',
    permissions: { control: true, manage: true },
    next_seq: 1,
    limits: { max_message: 1 << 20, window_bytes: 1 << 20, window_messages: 32, max_inflight: 32, max_views: 4, max_feeds: 16 },
    features,
  };
}

const CONFIG = '{"project_info":{"project_id":"demo"},"client":[]}';

class FakeClient {
  readonly sent: api.Op[] = [];
  fail: Error | null = null;
  readonly feeds = new Map<string, api.FeedValue>();
  readonly listeners = new Set<() => void>();
  followed = 0;
  released = 0;
  constructor(public welcome: api.Welcome | null) {}
  getState = () => ({ welcome: this.welcome, feeds: this.feeds });
  follow = (): (() => void) => {
    this.followed++;
    return () => void this.released++;
  };
  subscribeState = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => void this.listeners.delete(listener);
  };
  /** The device feed arrives, with the desktop's public Android config when it has one. */
  deliver(config: string | undefined): void {
    const push: api.PushStateView = { subscribed: false, prefs: { turn_finished: true, attention: true, ended: true, show_titles: false }, since_unix_ms: null };
    if (config !== undefined) push.android_config = config;
    this.feeds.set('device', { feed: 'device', name: 'Pixel', permissions: { control: true, manage: true }, push, gateway_version: '0.27.3', mode: 'background' });
    for (const listener of [...this.listeners]) listener();
  }
  request = async (op: api.Op): Promise<api.Resp> => {
    if (this.fail !== null) throw this.fail;
    this.sent.push(op);
    return { type: 'unit' };
  };
}

const READY: NativePushState = { permission: 'granted', configured: true, token: 'fcm-token-1' };

class FakeShell {
  readonly calls: { method: NativeMethod; params: unknown }[] = [];
  state: NativePushState = { permission: 'default', configured: false, token: null };
  /** What `pushEnable` leaves behind. */
  afterEnable: NativePushState = READY;
  forgetFails = false;
  request<K extends NativeMethod>(method: K, params: NativeCalls[K]['params']): Promise<NativeCalls[K]['result']> {
    this.calls.push({ method, params });
    const result = this.#answer(method, params);
    return result instanceof Error ? Promise.reject(result) : (Promise.resolve(result) as Promise<NativeCalls[K]['result']>);
  }
  #answer(method: NativeMethod, params: unknown): unknown {
    switch (method) {
      case 'pushState':
        return this.state;
      case 'pushEnable':
        this.state = this.afterEnable;
        return this.state;
      case 'pushSubscription': {
        const { desktop } = params as NativeCalls['pushSubscription']['params'];
        return { token: this.state.token ?? 'none', key: KEY, desktop };
      }
      case 'pushForget':
        return this.forgetFails ? new Error('keystore busy') : null;
      default:
        return null;
    }
  }
  methods(): NativeMethod[] {
    return this.calls.map((call) => call.method);
  }
}

class MemoryMarkers implements PushMarkers {
  readonly on = new Set<string>();
  has = (desktop: string) => this.on.has(desktop);
  set = (desktop: string) => void this.on.add(desktop);
  clear = (desktop: string) => void this.on.delete(desktop);
}

/** `config`: the public Android config the desktop's device feed carries (`null`: none; `'pending'`: the feed has not arrived). */
function setup(options: { welcome?: api.Welcome | null; marked?: string[]; id?: string; config?: string | null | 'pending' } = {}) {
  const client = new FakeClient(options.welcome === undefined ? welcome() : options.welcome);
  if (options.config !== 'pending') client.deliver(options.config === null ? undefined : (options.config ?? CONFIG));
  const shell = new FakeShell();
  const markers = new MemoryMarkers();
  for (const id of options.marked ?? []) markers.set(id);
  const push = new NativePushController(client, shell, { id: options.id ?? A, label: () => 'Work' }, markers);
  return { client, shell, markers, push };
}

const subscribeOp = (token = 'fcm-token-1', desktop = A): api.Op => ({ op: 'push_android_subscribe', token, key: KEY, desktop });

describe('what the desktop advertises', () => {
  test('only a configured FCM sender lists android_push, and only on an s4 gateway', () => {
    expect(advertisesAndroidPush(welcome())).toBe(true);
    expect(advertisesAndroidPush(welcome(['s4']))).toBe(false);
    expect(advertisesAndroidPush(welcome(['android_push']))).toBe(false);
    expect(advertisesAndroidPush(null)).toBe(false);
  });

  test('the state shows the setup status before anything is read', () => {
    expect(setup().push.getState()).toMatchObject({ support: 'supported', native: { desktopSender: true }, subscribed: false });
    expect(setup({ welcome: welcome(['s4']) }).push.getState().native?.desktopSender).toBe(false);
  });
});

describe('enable', () => {
  test('without a configured sender nothing is asked or sent: the setup instructions are the answer', async () => {
    const { push, shell, client } = setup({ welcome: welcome(['s4']) });
    await push.enable();
    expect(push.getState()).toMatchObject({ error: DESKTOP_SENDER_HELP, busy: false, subscribed: false, native: { desktopSender: false } });
    expect(shell.calls).toEqual([]);
    expect(client.sent).toEqual([]);
  });

  test('an old desktop (no s4) is told to update, nothing sent', async () => {
    const { push, shell } = setup({ welcome: welcome([]) });
    await push.enable();
    expect(shell.calls).toEqual([]);
  });

  test('permission and token are requested by the shell, then the key and token reach the desktop, and the choice is remembered', async () => {
    const { push, shell, client, markers } = setup();
    await push.enable();
    expect(shell.methods()).toEqual(['pushState', 'pushEnable', 'pushSubscription']);
    expect(shell.calls[2]?.params).toEqual({ desktop: A, label: 'Work' });
    expect(client.sent).toEqual([subscribeOp()]);
    expect(markers.has(A)).toBe(true);
    expect(push.getState()).toMatchObject({ subscribed: true, busy: false, error: null, permission: 'granted' });
  });

  test('a refused permission explains and registers nothing', async () => {
    const denied = setup();
    denied.shell.afterEnable = { permission: 'denied', configured: true, token: null };
    await denied.push.enable();
    expect(denied.push.getState()).toMatchObject({ subscribed: false, busy: false, permission: 'denied' });
    const dismissed = setup();
    dismissed.shell.afterEnable = { permission: 'default', configured: true, token: null };
    await dismissed.push.enable();
    for (const attempt of [denied, dismissed]) {
      expect(attempt.client.sent).toEqual([]);
      expect(attempt.markers.has(A)).toBe(false);
    }
  });

  test('a phone whose Firebase setup did not take, or without a token yet, registers nothing', async () => {
    const bare = setup();
    bare.shell.afterEnable = { permission: 'granted', configured: false, token: null };
    await bare.push.enable();
    expect(bare.push.getState().error).not.toBeNull();
    expect(bare.push.getState().native?.phoneConfigured).toBe(false);
    const waiting = setup();
    waiting.shell.afterEnable = { permission: 'granted', configured: true, token: null };
    await waiting.push.enable();
    expect(waiting.push.getState().native?.hasToken).toBe(false);
    for (const attempt of [bare, waiting]) {
      expect(attempt.client.sent).toEqual([]);
      expect(attempt.markers.has(A)).toBe(false);
    }
  });


  test('a phone with Firebase already set up is given no config and the desktop feed is not even read', async () => {
    const { push, shell, client } = setup({ config: null });
    shell.state = READY;
    await push.enable();
    expect(shell.calls.find((call) => call.method === 'pushEnable')?.params).toEqual({});
    expect(client.followed).toBe(0);
    expect(push.getState().subscribed).toBe(true);
  });

  test('a desktop without a config: no permission prompt, no registration, instructions on the desktop', async () => {
    const { push, shell, client, markers } = setup({ config: null });
    await push.enable();
    expect(shell.methods()).toEqual(['pushState']);
    expect(push.getState()).toMatchObject({ busy: false, subscribed: false, error: DESKTOP_CONFIG_HELP });
    expect(client.sent).toEqual([]);
    expect(markers.has(A)).toBe(false);
  });

  test('a device feed still loading is waited for, then released', async () => {
    const { push, shell, client } = setup({ config: 'pending' });
    const enabling = push.enable();
    while (client.followed === 0) await Promise.resolve();
    expect(shell.methods()).toEqual(['pushState']);
    expect(push.getState().busy).toBe(true);
    client.deliver(CONFIG);
    await enabling;
    expect(shell.calls[1]).toEqual({ method: 'pushEnable', params: { config: CONFIG } });
    expect(client.released).toBe(client.followed);
    expect(client.listeners.size).toBe(0);
    expect(push.getState().subscribed).toBe(true);
  });

  test('a cached device feed delivered synchronously by follow is not missed', async () => {
    const { push, client } = setup({ config: 'pending' });
    const follow = client.follow;
    client.follow = () => {
      const release = follow();
      client.deliver(CONFIG);
      return release;
    };
    await push.enable();
    expect(push.getState()).toMatchObject({ subscribed: true, busy: false, error: null });
    expect(client.released).toBe(client.followed);
    expect(client.listeners.size).toBe(0);
  });

  test('a device feed that never arrives ends in an error, not a prompt, and a later tap can retry', async () => {
    vi.useFakeTimers();
    try {
      const { push, shell, client } = setup({ config: 'pending' });
      const enabling = push.enable();
      while (client.followed === 0) await Promise.resolve();
      vi.advanceTimersByTime(60_000);
      await enabling;
      expect(shell.methods()).toEqual(['pushState']);
      expect(push.getState()).toMatchObject({ busy: false, subscribed: false });
      expect(push.getState().error).not.toBeNull();
      expect(client.released).toBe(client.followed);
      client.deliver(CONFIG);
      await push.enable();
      expect(push.getState().subscribed).toBe(true);
    } finally {
      vi.useRealTimers();
    }
  });

  test('a failed gateway registration leaves push off, with no marker', async () => {
    const { push, client, markers } = setup();
    client.fail = new Error('offline');
    await push.enable();
    expect(push.getState()).toMatchObject({ subscribed: false, busy: false });
    expect(markers.has(A)).toBe(false);
  });

  test('a key prepared for another desktop is refused', async () => {
    const { push, shell, client } = setup();
    const request = shell.request.bind(shell);
    shell.request = ((method: NativeMethod, params: never) =>
      method === 'pushSubscription' ? Promise.resolve({ token: 't', key: KEY, desktop: B }) : request(method, params)) as typeof shell.request;
    await push.enable();
    expect(client.sent).toEqual([]);
  });
});

describe('sync (after each Welcome, a shell change, or activation)', () => {
  test('never turns push on by itself and never prompts', async () => {
    const { push, shell, client } = setup();
    shell.state = READY;
    await push.sync();
    expect(shell.methods()).toEqual(['pushState']);
    expect(client.sent).toEqual([]);
    expect(push.getState().subscribed).toBe(false);
  });

  test('re-sends the registration with the current token, and never touches the prefs', async () => {
    const { push, shell, client } = setup({ marked: [A] });
    shell.state = { ...READY, token: 'rotated-token' };
    await push.sync();
    expect(shell.methods()).toEqual(['pushState', 'pushSubscription']);
    expect(client.sent).toEqual([subscribeOp('rotated-token')]);
    expect(client.sent.some((op) => op.op === 'push_prefs')).toBe(false);
    expect(push.getState()).toMatchObject({ subscribed: true, error: null });
  });

  test('keeps the choice while permission, Firebase or the desktop sender is missing, and sends nothing', async () => {
    const revoked = setup({ marked: [A] });
    revoked.shell.state = { permission: 'denied', configured: true, token: null };
    await revoked.push.sync();
    const unconfigured = setup({ marked: [A] });
    unconfigured.shell.state = { permission: 'granted', configured: false, token: null };
    await unconfigured.push.sync();
    const noSender = setup({ marked: [A], welcome: welcome(['s4']) });
    noSender.shell.state = READY;
    await noSender.push.sync();
    for (const attempt of [revoked, unconfigured, noSender]) {
      expect(attempt.client.sent).toEqual([]);
      expect(attempt.markers.has(A)).toBe(true);
      expect(attempt.push.getState().subscribed).toBe(false);
    }
    expect(revoked.push.getState().permission).toBe('denied');
    // Once the user grants it in Android's settings, the next sync registers again.
    revoked.shell.state = READY;
    await revoked.push.sync();
    expect(revoked.client.sent).toEqual([subscribeOp()]);
    expect(revoked.push.getState().subscribed).toBe(true);
  });

  test('an old desktop is left alone', async () => {
    const { push, shell } = setup({ marked: [A], welcome: welcome([]) });
    await push.sync();
    expect(shell.calls).toEqual([]);
  });

  test('a failure is reported and the next sync retries', async () => {
    const { push, shell, client } = setup({ marked: [A] });
    shell.state = READY;
    client.fail = new Error('connection lost');
    await push.sync();
    expect(push.getState().error).toContain('Could not renew notifications: connection lost');
    client.fail = null;
    await push.sync();
    expect(push.getState().error).toBeNull();
    expect(client.sent).toEqual([subscribeOp()]);
  });

  test('another desktop’s marker does not make this desktop register', async () => {
    const { push, shell, client } = setup({ marked: [B] });
    shell.state = READY;
    await push.sync();
    expect(client.sent).toEqual([]);
    expect(shell.methods()).toEqual(['pushState']);
  });
});

describe('disable', () => {
  test('drops the gateway subscription first, then exactly this desktop’s key', async () => {
    const { push, shell, client, markers } = setup({ marked: [A, B] });
    shell.state = READY;
    await push.sync();
    shell.calls.length = 0;
    client.sent.length = 0;
    await push.disable();
    expect(client.sent).toEqual([{ op: 'push_unsubscribe' }]);
    expect(shell.calls).toEqual([{ method: 'pushForget', params: { desktop: A } }]);
    expect(markers.has(A)).toBe(false);
    expect(markers.has(B)).toBe(true);
    expect(push.getState()).toMatchObject({ subscribed: false, busy: false, error: null });
  });

  test('when the desktop cannot be reached nothing is dropped: push stays on', async () => {
    const { push, shell, client, markers } = setup({ marked: [A] });
    client.fail = new Error('offline');
    await push.disable();
    expect(shell.calls).toEqual([]);
    expect(markers.has(A)).toBe(true);
    expect(push.getState().error).toContain('offline');
  });

  test('a key the shell could not remove still stays off: a later sync does not register again', async () => {
    const { push, shell, client, markers } = setup({ marked: [A] });
    shell.state = READY;
    shell.forgetFails = true;
    await push.disable();
    expect(push.getState()).toMatchObject({ subscribed: false, busy: false });
    expect(push.getState().error).toContain('keystore busy');
    expect(markers.has(A)).toBe(false);
    client.sent.length = 0;
    await push.sync();
    expect(client.sent).toEqual([]);
  });
});

describe('prefs', () => {
  test('are sent as the shared push_prefs op, whatever the transport', async () => {
    const { push, client } = setup({ marked: [A] });
    const prefs: api.PushPrefs = { turn_finished: false, attention: true, ended: true, show_titles: true };
    await push.setPrefs(prefs);
    expect(client.sent).toEqual([{ op: 'push_prefs', prefs }]);
    client.fail = new Error('offline');
    await push.setPrefs(prefs);
    expect(push.getState().error).toContain('Could not save the notification settings');
  });
});

describe('flows never interleave', () => {
  test('a sync started during a tap runs after it, on the state the tap left', async () => {
    const { push, shell, client } = setup();
    shell.state = { permission: 'default', configured: false, token: null };
    const enabling = push.enable();
    const syncing = push.sync();
    await Promise.all([enabling, syncing]);
    expect(shell.methods()).toEqual(['pushState', 'pushEnable', 'pushSubscription', 'pushState', 'pushSubscription']);
    expect(client.sent).toEqual([subscribeOp(), subscribeOp()]);
  });
});

describe('forgetting a desktop', () => {
  test('drops that desktop’s marker and key only', async () => {
    const shell = new FakeShell();
    const markers = new MemoryMarkers();
    markers.set(A);
    markers.set(B);
    await dropNativePush(shell, markers, A);
    expect(shell.calls).toEqual([{ method: 'pushForget', params: { desktop: A } }]);
    expect(markers.on).toEqual(new Set([B]));
  });

  test('a shell failure is rethrown after the marker is gone, so nothing re-registers it', async () => {
    const shell = new FakeShell();
    shell.forgetFails = true;
    const markers = new MemoryMarkers();
    markers.set(A);
    await expect(dropNativePush(shell, markers, A)).rejects.toThrow('keystore busy');
    expect(markers.has(A)).toBe(false);
  });

  test('a marker store that throws still lets the key go', async () => {
    const shell = new FakeShell();
    const broken: PushMarkers = {
      has: () => true,
      set: () => undefined,
      clear: () => {
        throw new Error('storage');
      },
    };
    await expect(dropNativePush(shell, broken, A)).rejects.toThrow('storage');
    expect(shell.calls).toEqual([{ method: 'pushForget', params: { desktop: A } }]);
  });
});

describe('markers', () => {
  test('live per desktop in storage', () => {
    const data = new Map<string, string>();
    const storage = {
      getItem: (key: string) => data.get(key) ?? null,
      setItem: (key: string, value: string) => void data.set(key, value),
      removeItem: (key: string) => void data.delete(key),
    };
    const markers = storageMarkers(storage);
    markers.set(A);
    expect(markers.has(A)).toBe(true);
    expect(markers.has(B)).toBe(false);
    // A new page load reads them back.
    expect(storageMarkers(storage).has(A)).toBe(true);
    markers.clear(A);
    expect(storageMarkers(storage).has(A)).toBe(false);
  });

  test('without usable storage they last for the page', () => {
    const none = storageMarkers(null);
    none.set(A);
    expect(none.has(A)).toBe(true);
    none.clear(A);
    expect(none.has(A)).toBe(false);
    const full = storageMarkers({
      getItem: () => {
        throw new Error('denied');
      },
      setItem: () => {
        throw new Error('quota');
      },
      removeItem: () => {
        throw new Error('denied');
      },
    });
    full.set(A);
    expect(full.has(A)).toBe(true);
    full.clear(A);
    expect(full.has(A)).toBe(false);
  });
});
