// The Android app's own update on the UI side: what the codec accepts from the shell, what each
// state offers the user, and how the controller orders answers against events and cleans up.

import { describe, expect, test } from 'bun:test';
import { NativeError, type NativeCalls, type NativeEvents, type NativeMethod, type NativeUpdateState, decodeUpdateState } from '../src/app/native.ts';
import { NativeUpdateController, type NativeUpdateHost, downloadFraction, isNewerVersion, updateOffer } from '../src/app/native-update.ts';

const STATE: NativeUpdateState = {
  current: '0.27.4',
  latest: '0.28.0',
  phase: 'available',
  downloaded: 0,
  total: null,
  checked_unix_ms: 1_700_000_000_000,
  auto_check: true,
  error: null,
};

describe('decoding the shell’s update state', () => {

  test('anything off the contract is a protocol error', () => {
    const bad: Record<string, unknown>[] = [
      { ...STATE, phase: 'done' },
      { ...STATE, downloaded: -1 },
      { ...STATE, downloaded: 1.5 },
      { ...STATE, total: 'big' },
      { ...STATE, auto_check: 'yes' },
      { ...STATE, latest: 28 },
      { ...STATE, error: 7 },
      { ...STATE, checked_unix_ms: Number.POSITIVE_INFINITY },
    ];
    for (const value of bad) expect(() => decodeUpdateState(value)).toThrow(NativeError);
    expect(() => decodeUpdateState(null)).toThrow(NativeError);
  });
});

describe('what a state offers', () => {
  test('only a strictly newer release is offered', () => {
    expect(isNewerVersion('0.28.0', '0.27.4')).toBe(true);
    expect(isNewerVersion('0.27.4', '0.27.4')).toBe(false);
    expect(isNewerVersion('0.9.9', '0.27.4')).toBe(false);
    expect(isNewerVersion('0.27.10', '0.27.9')).toBe(true);
    expect(isNewerVersion('garbage', '0.27.4')).toBe(false);
    expect(isNewerVersion(null, '0.27.4')).toBe(false);
  });

  test('each phase maps to the one action the user can take', () => {
    const offer = (patch: Partial<NativeUpdateState>) => updateOffer({ ...STATE, ...patch });
    expect(offer({ phase: 'available' })).toEqual({ latest: '0.28.0', action: 'update' });
    expect(offer({ phase: 'ready' })?.action).toBe('install');
    expect(offer({ phase: 'installing' })?.action).toBe('install');
    expect(offer({ phase: 'downloading' })?.action).toBeNull();
    expect(offer({ phase: 'checking' })?.action).toBeNull();
    // A failed check keeps the release it found: retry it.
    expect(offer({ phase: 'error', error: 'offline' })?.action).toBe('retry');
    // Up to date, or a failure with nothing newer known: nothing to install.
    expect(offer({ phase: 'idle', latest: '0.27.4' })).toBeNull();
    expect(offer({ phase: 'error', latest: null, error: 'offline' })).toBeNull();
    expect(updateOffer(null)).toBeNull();
  });

  test('progress is a bounded fraction, unknown without a size', () => {
    expect(downloadFraction({ downloaded: 50, total: 200 })).toBe(0.25);
    expect(downloadFraction({ downloaded: 900, total: 200 })).toBe(1);
    expect(downloadFraction({ downloaded: 5, total: null })).toBeNull();
    expect(downloadFraction({ downloaded: 5, total: 0 })).toBeNull();
  });
});

class FakeShell implements NativeUpdateHost {
  readonly calls: { method: NativeMethod; params: unknown }[] = [];
  readonly replies: ((value: NativeUpdateState) => void)[] = [];
  readonly failures: ((error: Error) => void)[] = [];
  readonly listeners = new Map<string, Set<(detail: never) => void>>();
  request<K extends NativeMethod>(method: K, params: NativeCalls[K]['params']): Promise<NativeCalls[K]['result']> {
    this.calls.push({ method, params });
    const { promise, resolve, reject } = Promise.withResolvers<NativeCalls[K]['result']>();
    this.replies.push(resolve as (value: NativeUpdateState) => void);
    this.failures.push(reject);
    return promise;
  }
  on<K extends keyof NativeEvents>(event: K, listener: (detail: NativeEvents[K]) => void): () => void {
    const set = this.listeners.get(event) ?? new Set();
    this.listeners.set(event, set);
    set.add(listener as (detail: never) => void);
    return () => void set.delete(listener as (detail: never) => void);
  }
  emit<K extends keyof NativeEvents>(event: K, detail: NativeEvents[K]): void {
    for (const listener of this.listeners.get(event) ?? []) listener(detail as never);
  }
  count(event: string): number {
    return this.listeners.get(event)?.size ?? 0;
  }
}

// Lets the awaits inside the controller settle (all of them are microtasks).
const tick = async (): Promise<void> => {
  for (let i = 0; i < 8; i++) await Promise.resolve();
};

describe('the controller', () => {
  test('an unmounted view stops following events and remounting reads fresh state', async () => {
    const shell = new FakeShell();
    const controller = new NativeUpdateController(shell);
    const phases: unknown[] = [];
    const watch = () => phases.push(controller.get().state?.phase);
    const off = controller.subscribe(watch);
    shell.replies[0]?.(STATE);
    await tick();
    off();
    shell.emit('update', { ...STATE, phase: 'error', error: 'offline' });
    expect(phases).toEqual(['available']);
    const second = controller.subscribe(watch);
    shell.replies[1]?.({ ...STATE, phase: 'ready' });
    await tick();
    expect(phases).toEqual(['available', 'ready']);
    second();
    shell.emit('update', { ...STATE, phase: 'downloading' });
    expect(phases).toEqual(['available', 'ready']);
  });

  test('an event heard while a reply is in flight wins over that older reply', async () => {
    const shell = new FakeShell();
    const controller = new NativeUpdateController(shell);
    controller.subscribe(() => undefined);
    shell.emit('update', { ...STATE, phase: 'downloading', downloaded: 10, total: 100 });
    shell.replies[0]?.({ ...STATE, phase: 'checking' });
    await tick();
    expect(controller.get().state?.phase).toBe('downloading');
  });

  test('a failed call is reported and clears when the next one starts; the last state stays', async () => {
    const shell = new FakeShell();
    const controller = new NativeUpdateController(shell);
    controller.subscribe(() => undefined);
    shell.replies[0]?.(STATE);
    await tick();
    const install = controller.install();
    expect(controller.get().busy).toBe(true);
    shell.failures[1]?.(new NativeError('timeout', 'Android did not answer updateInstall'));
    await install;
    expect(controller.get()).toMatchObject({ busy: false, failure: 'Android did not answer updateInstall', state: STATE });
    const again = controller.check();
    expect(controller.get().failure).toBeNull();
    shell.replies[2]?.({ ...STATE, phase: 'checking' });
    await again;
    expect(controller.get().state?.phase).toBe('checking');
  });


  test('returning to the foreground recovers a download that finished while hidden', async () => {
    const shell = new FakeShell();
    const controller = new NativeUpdateController(shell);
    controller.subscribe(() => undefined);
    shell.replies[0]?.({ ...STATE, phase: 'downloading', downloaded: 10, total: 100 });
    await tick();
    shell.emit('visibility', { visible: false });
    shell.emit('visibility', { visible: true });
    shell.replies[1]?.({ ...STATE, phase: 'ready', downloaded: 100, total: 100 });
    await tick();
    expect(controller.get().state?.phase).toBe('ready');
  });
});
