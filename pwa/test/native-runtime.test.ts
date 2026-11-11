// The Android app's runtime differences that are not push: saving files through the shell instead of
// an `<a download>`, and never keeping a service worker (or its caches) beside the bundled assets.

import { describe, expect, test } from 'bun:test';
import { ClientError } from '../src/app/client.ts';
import { MAX_SAVE_BYTES, NativeError, type NativeCalls, type NativeMethod } from '../src/app/native.ts';
import { downloadAndSave, fromBase64, isAbort, saveMime, saveNative, toBase64, type TransferClient } from '../src/app/transfer.ts';
import { retireServiceWorkers } from '../src/app/update.ts';
import type { api } from '../src/remote/index.ts';

class FakeShell {
  readonly saved: NativeCalls['saveFile']['params'][] = [];
  failure: NativeError | null = null;
  request<K extends NativeMethod>(method: K, params: NativeCalls[K]['params']): Promise<NativeCalls[K]['result']> {
    if (method !== 'saveFile') return Promise.reject(new Error(`unexpected ${method}`));
    this.saved.push(params as NativeCalls['saveFile']['params']);
    return this.failure === null ? Promise.resolve(null as NativeCalls[K]['result']) : Promise.reject(this.failure);
  }
}

function desktop(bytes: Uint8Array, name: string, mime: string): TransferClient {
  return {
    whenOnline: async () => undefined,
    request: async (op: api.Op): Promise<api.Resp> => {
      if (op.op === 'export' || op.op === 'blob') return { type: 'download', download: 'dl-1', name, mime, size: bytes.length };
      if (op.op === 'download') return { type: 'chunk', data: toBase64(bytes), eof: true };
      throw new Error(`unexpected ${op.op}`);
    },
  };
}

const SESSION = { op: 'export', session: 's-1' } as const;

describe('saving through the shell', () => {
  test('an export is pulled, then handed over whole as standard base64 with a safe name and a real type', async () => {
    const shell = new FakeShell();
    const bytes = Uint8Array.from([0, 1, 2, 250, 251, 255]);
    const file = await downloadAndSave(desktop(bytes, '../../etc/.session one.html', 'text/html'), SESSION, { host: shell });
    expect(file.size).toBe(6);
    expect(shell.saved).toHaveLength(1);
    expect(shell.saved[0]).toMatchObject({ name: 'session one.html', mime: 'text/html' });
    expect(Array.from(fromBase64(shell.saved[0]?.data ?? ''))).toEqual(Array.from(bytes));
  });

  test('a malformed mime type from the desktop is saved as opaque bytes', () => {
    expect(saveMime(' Text/HTML ')).toBe('text/html');
    expect(saveMime('text/html; charset=utf-8')).toBe('application/octet-stream');
    expect(saveMime('')).toBe('application/octet-stream');
    expect(saveMime('nonsense')).toBe('application/octet-stream');
    expect(saveMime('a/b/c')).toBe('application/octet-stream');
  });

  test('dismissing the save dialog is a cancelled transfer, not a failure', async () => {
    const shell = new FakeShell();
    shell.failure = new NativeError('cancelled', 'cancelled');
    const outcome = await downloadAndSave(desktop(new Uint8Array(3), 'a.html', 'text/html'), SESSION, { host: shell }).catch((error: unknown) => error);
    expect(isAbort(outcome)).toBe(true);
  });

  test('any other shell failure surfaces as it is', async () => {
    const shell = new FakeShell();
    shell.failure = new NativeError('native', 'No space left on device');
    const outcome = await downloadAndSave(desktop(new Uint8Array(3), 'a.html', 'text/html'), SESSION, { host: shell }).catch((error: unknown) => error);
    expect(outcome).toBeInstanceOf(NativeError);
    expect(outcome instanceof Error ? outcome.message : '').toBe('No space left on device');
  });

  test('a file beyond what Android can save from here is refused before it is encoded', async () => {
    const shell = new FakeShell();
    const big = { name: 'big.bin', mime: 'application/octet-stream', size: MAX_SAVE_BYTES + 1, blob: new Blob([new Uint8Array(MAX_SAVE_BYTES + 1)]) };
    const outcome = await saveNative(shell, big).catch((error: unknown) => error);
    expect(outcome).toBeInstanceOf(ClientError);
    expect(shell.saved).toEqual([]);
    // The limit itself is fine.
    await saveNative(shell, { ...big, size: MAX_SAVE_BYTES, blob: new Blob([new Uint8Array(MAX_SAVE_BYTES)]) });
    expect(shell.saved).toHaveLength(1);
  });
});

describe('no service worker beside the bundled assets', () => {
  test('every leftover registration is unregistered and every cache deleted', async () => {
    const unregistered: string[] = [];
    const registrations = ['/', '/d/x/'].map((scope) => ({ unregister: async () => (unregistered.push(scope), true) }));
    const container = { getRegistrations: async () => registrations as unknown as readonly ServiceWorkerRegistration[] };
    const caches = new Set(['app-shell', 'push-state']);
    await retireServiceWorkers(container, { keys: async () => [...caches], delete: async (name) => caches.delete(name) });
    expect(unregistered).toEqual(['/', '/d/x/']);
    expect(caches.size).toBe(0);
  });

  test('a failing step never blocks the other, and nothing is thrown', async () => {
    const caches = new Set(['a']);
    await retireServiceWorkers(
      {
        getRegistrations: async () => {
          throw new Error('blocked');
        },
      },
      { keys: async () => [...caches], delete: async (name) => caches.delete(name) },
    );
    expect(caches.size).toBe(0);
    await retireServiceWorkers(undefined, {
      keys: async () => {
        throw new Error('blocked');
      },
      delete: async () => false,
    });
    await retireServiceWorkers(undefined, undefined);
  });
});
