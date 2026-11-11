// The Android shell bridge: request/answer correlation, every method's params and result checks,
// every event's decoding, and what a misbehaving shell cannot do. A fake shell stands in for
// `window.OmpDeckAndroid`; time is a fake clock.

import { describe, expect, test } from 'bun:test';
import {
  MAX_FIREBASE_CONFIG_CHARS,
  MAX_SAVE_BYTES,
  NativeError,
  type NativeClock,
  NativeHost,
  type RawBridge,
  base64DecodedLength,
  rawBridgeOf,
} from '../src/app/native.ts';

const A = 'ab'.repeat(32);
const KEY = 'A'.repeat(43);

interface Sent {
  id: string;
  method: string;
  params: unknown;
}

class FakeShell implements RawBridge {
  onmessage: ((event: { data: unknown }) => void) | null = null;
  readonly sent: Sent[] = [];
  failPost = false;
  postMessage(message: string): void {
    if (this.failPost) throw new Error('detached');
    this.sent.push(JSON.parse(message) as Sent);
  }
  raw(data: unknown): void {
    this.onmessage?.({ data });
  }
  answer(index: number, body: Record<string, unknown>): void {
    this.raw(JSON.stringify({ id: this.sent[index]?.id, ...body }));
  }
  event(event: string, detail?: unknown): void {
    this.raw(JSON.stringify(detail === undefined ? { event } : { event, detail }));
  }
}

class FakeClock implements NativeClock {
  readonly timers = new Map<number, { callback: () => void; ms: number }>();
  #next = 1;
  setTimer(callback: () => void, ms: number): number {
    const id = this.#next++;
    this.timers.set(id, { callback, ms });
    return id;
  }
  clearTimer(handle: unknown): void {
    this.timers.delete(handle as number);
  }
  fire(): void {
    for (const [id, timer] of [...this.timers]) {
      this.timers.delete(id);
      timer.callback();
    }
  }
}

function setup(): { shell: FakeShell; host: NativeHost; clock: FakeClock } {
  const shell = new FakeShell();
  const clock = new FakeClock();
  return { shell, host: new NativeHost(shell, clock), clock };
}

async function failure(promise: Promise<unknown>): Promise<NativeError> {
  try {
    await promise;
  } catch (error) {
    if (error instanceof NativeError) return error;
    throw error;
  }
  throw new Error('expected a NativeError');
}

describe('requests and answers', () => {
  test('the request is {id, method, params} JSON and the answer resolves the matching promise', async () => {
    const { shell, host } = setup();
    const first = host.request('pushState', {});
    const second = host.handshake();
    expect(shell.sent).toHaveLength(2);
    expect(shell.sent[0]).toMatchObject({ method: 'pushState', params: {} });
    expect(shell.sent[0]?.id).not.toBe(shell.sent[1]?.id);
    // Answered out of order.
    shell.answer(1, { result: { platform: 'android', version: '1.2.3' } });
    shell.answer(0, { result: { permission: 'granted', configured: true, token: 'tok' } });
    expect(await second).toEqual({ platform: 'android', version: '1.2.3' });
    expect(await first).toEqual({ permission: 'granted', configured: true, token: 'tok' });
  });

  test('a shell error string rejects with it; "cancelled" is its own kind', async () => {
    const { shell, host } = setup();
    const saving = host.request('saveFile', { name: 'a.txt', mime: 'text/plain', data: 'QQ==' });
    shell.answer(0, { error: 'cancelled' });
    expect((await failure(saving)).kind).toBe('cancelled');
    const other = host.request('pushEnable', {});
    shell.answer(1, { error: 'No Google Play services' });
    const error = await failure(other);
    expect(error).toMatchObject({ kind: 'native', message: 'No Google Play services' });
  });

  test('an answer that breaks the method contract is a protocol error, never a half-typed value', async () => {
    const { shell, host } = setup();
    const bad: [Promise<unknown>, Record<string, unknown>][] = [
      [host.request('info', {}), { result: { platform: 'ios', version: '1' } }],
      [host.request('pushState', {}), { result: { permission: 'maybe', configured: true, token: null } }],
      [host.request('pushState', {}), { result: { permission: 'granted', configured: 'yes', token: null } }],
      [host.request('pushState', {}), { result: { permission: 'granted', configured: true, token: '' } }],
      [host.request('pushSubscription', { desktop: A, label: 'x' }), { result: { token: 't', key: 'short', desktop: A } }],
      [host.request('pushSubscription', { desktop: A, label: 'x' }), { result: { token: 't', key: `${KEY.slice(0, 42)}=`, desktop: A } }],
      [host.request('pushSubscription', { desktop: A, label: 'x' }), { result: { token: 't', key: KEY, desktop: 'nope' } }],
      [host.request('pushForget', { desktop: A }), { result: { forgotten: true } }],
      [host.request('finish', {}), { error: 7 }],
    ];
    const outcomes = bad.map(([promise]) => failure(promise));
    bad.forEach(([, body], index) => shell.answer(index, body));
    for (const outcome of await Promise.all(outcomes)) expect(outcome.kind).toBe('protocol');
  });

  test('empty results are null or absent; a good subscription round-trips', async () => {
    const { shell, host } = setup();
    const forget = host.request('pushForget', { desktop: A });
    shell.answer(0, { result: null });
    expect(await forget).toBeNull();
    const labels = host.request('pushLabels', { labels: { [A]: 'Work' } });
    shell.answer(1, {});
    expect(await labels).toBeNull();
    const sub = host.request('pushSubscription', { desktop: A, label: 'Work' });
    expect(shell.sent[2]?.params).toEqual({ desktop: A, label: 'Work' });
    shell.answer(2, { result: { token: 'fcm', key: KEY, desktop: A } });
    expect(await sub).toEqual({ token: 'fcm', key: KEY, desktop: A });
  });

  test('stray, late, malformed and non-string messages change nothing', async () => {
    const { shell, host } = setup();
    const pending = host.request('finish', {});
    shell.raw('not json');
    shell.raw(42);
    shell.raw(JSON.stringify([1, 2]));
    shell.raw(JSON.stringify({ id: 'n999', result: null }));
    shell.raw(JSON.stringify({ event: 'unknown_event', detail: {} }));
    shell.answer(0, { result: null });
    expect(await pending).toBeNull();
    // A second answer for a settled request is ignored.
    shell.answer(0, { error: 'late' });
  });

  test('a refused post rejects instead of hanging', async () => {
    const { shell, host, clock } = setup();
    shell.failPost = true;
    const error = await failure(host.request('finish', {}));
    expect(error.kind).toBe('native');
    expect(clock.timers.size).toBe(0);
  });
});

describe('deadlines', () => {
  test('quick methods time out; the ones waiting on the user never do', async () => {
    const { host, clock } = setup();
    const quick = host.request('pushState', {});
    const enable = host.request('pushEnable', {});
    const save = host.request('saveFile', { name: 'a', mime: 'text/plain', data: '' });
    expect(clock.timers.size).toBe(1);
    clock.fire();
    expect((await failure(quick)).kind).toBe('timeout');
    // The user's dialogs stay pending (no timer was ever set for them).
    const outcome = await Promise.race([enable.then(() => 'settled', () => 'settled'), save.then(() => 'settled', () => 'settled'), Promise.resolve('pending')]);
    expect(outcome).toBe('pending');
  });

  test('an answer clears its deadline', async () => {
    const { shell, host, clock } = setup();
    const pending = host.request('pushState', {});
    shell.answer(0, { result: { permission: 'default', configured: false, token: null } });
    await pending;
    expect(clock.timers.size).toBe(0);
  });
});

describe('params are checked before anything is posted', () => {
  async function invalid(call: Promise<unknown>, shell: FakeShell): Promise<void> {
    expect((await failure(call)).kind).toBe('invalid');
    expect(shell.sent).toHaveLength(0);
  }

  test('desktop ids are 64 lowercase hex digits', async () => {
    const { shell, host } = setup();
    await invalid(host.request('pushForget', { desktop: A.toUpperCase() }), shell);
    await invalid(host.request('pushForget', { desktop: A.slice(1) }), shell);
    await invalid(host.request('pushSubscription', { desktop: 'x', label: 'l' }), shell);
    await invalid(host.request('pushLabels', { labels: { nope: 'l' } }), shell);
    await invalid(host.request('setPresence', { desktop: 'nope', visible: true }), shell);
  });

  test('setPresence takes null (nothing on screen) and a boolean', async () => {
    const { shell, host } = setup();
    void host.request('setPresence', { desktop: null, visible: false });
    void host.request('setPresence', { desktop: A, visible: true });
    expect(shell.sent.map((entry) => entry.params)).toEqual([
      { desktop: null, visible: false },
      { desktop: A, visible: true },
    ]);
    await invalid(host.request('setPresence', { desktop: A, visible: 'yes' as unknown as boolean }), new FakeShell());
  });

  test('pushEnable takes nothing or one bounded Firebase config string, never anything else', async () => {
    const { shell, host } = setup();
    await invalid(host.request('pushEnable', { config: '  \n' }), shell);
    await invalid(host.request('pushEnable', { config: 'x'.repeat(MAX_FIREBASE_CONFIG_CHARS + 1) }), shell);
    await invalid(host.request('pushEnable', { config: 7 as unknown as string }), shell);
    await invalid(host.request('pushEnable', { path: '/x' } as never), shell);
    void host.request('pushEnable', {});
    void host.request('pushEnable', { config: '{"client":[]}' });
    expect(shell.sent.map((entry) => entry.params)).toEqual([{}, { config: '{"client":[]}' }]);
  });

  test('saveFile needs a name and standard base64 within the size limit', async () => {
    const { shell, host } = setup();
    await invalid(host.request('saveFile', { name: '', mime: 'text/plain', data: 'QQ==' }), shell);
    await invalid(host.request('saveFile', { name: 'a', mime: 'text/plain', data: 'not base64!' }), shell);
    await invalid(host.request('saveFile', { name: 'a', mime: 'text/plain', data: 'QQ' }), shell);
    const wholeGroups = 'A'.repeat(4 * Math.floor(MAX_SAVE_BYTES / 3));
    await invalid(host.request('saveFile', { name: 'a', mime: 'text/plain', data: `${wholeGroups}AAAA` }), shell);
    // Exactly the limit is accepted.
    void host.request('saveFile', { name: 'a', mime: 'text/plain', data: `${wholeGroups}AAA=` });
    void host.request('saveFile', { name: 'a', mime: 'text/plain', data: 'QQ==' });
    expect(shell.sent).toHaveLength(2);
  });

  test('base64 length: padding counts, the limit is on decoded bytes', () => {
    expect(base64DecodedLength('')).toBe(0);
    expect(base64DecodedLength('QQ==')).toBe(1);
    expect(base64DecodedLength('QUI=')).toBe(2);
    expect(base64DecodedLength('QUJD')).toBe(3);
    expect(base64DecodedLength('QUJD=')).toBeNull();
    expect(base64DecodedLength('QU-D')).toBeNull();
    // 20 MiB = 6 990 506 whole groups plus two bytes: the last group is padded with one "=".
    const wholeGroups = 'A'.repeat(4 * Math.floor(MAX_SAVE_BYTES / 3));
    expect(base64DecodedLength(`${wholeGroups}AAA=`)).toBe(MAX_SAVE_BYTES);
    expect(base64DecodedLength(`${wholeGroups}AAAA`)).toBe(MAX_SAVE_BYTES + 1);
  });

  test('no-parameter calls refuse stray parameters', async () => {
    const { shell, host } = setup();
    await invalid(host.request('finish', { extra: 1 } as unknown as Record<string, never>), shell);
  });
});

describe('events', () => {
  test('each event reaches its listeners decoded; unsubscribing stops it', () => {
    const { shell, host } = setup();
    const seen: unknown[] = [];
    const off = host.on('visibility', (detail) => seen.push(detail));
    host.on('open', (detail) => seen.push(detail));
    host.on('push_changed', (detail) => seen.push(detail));
    host.on('back', (detail) => seen.push(detail));
    shell.event('visibility', { visible: false });
    shell.event('open', { url: '/#s=abc&d=' + A });
    shell.event('push_changed');
    shell.event('back', { extra: 'ignored' });
    off();
    shell.event('visibility', { visible: true });
    expect(seen).toEqual([{ visible: false }, { url: `/#s=abc&d=${A}` }, {}, {}]);
  });

  test('a malformed event is dropped whole', () => {
    const { shell, host } = setup();
    const seen: unknown[] = [];
    for (const name of ['push_changed', 'visibility', 'open', 'back', 'update'] as const) host.on(name, (detail) => seen.push([name, detail]));
    shell.event('visibility', { visible: 'true' });
    shell.event('visibility', 5);
    shell.event('open', { url: 7 });
    shell.event('open', { url: '' });
    shell.event('open', { url: 'x'.repeat(5000) });
    shell.event('back', 'x');
    expect(seen).toEqual([]);
  });
});

describe('registries', () => {
  test('the bridge is found only when the shell injected a postMessage', () => {
    expect(rawBridgeOf({})).toBeNull();
    expect(rawBridgeOf({ OmpDeckAndroid: 'x' })).toBeNull();
    expect(rawBridgeOf({ OmpDeckAndroid: {} })).toBeNull();
    const shell = new FakeShell();
    expect(rawBridgeOf({ OmpDeckAndroid: shell })).toBe(shell);
  });
});
