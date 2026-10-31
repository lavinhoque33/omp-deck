import { describe, expect, test } from 'bun:test';
import { ClientError } from '../src/app/client.ts';
import { chunkWireBytes, download, fromBase64, isAbort, rasterBlob, sanitizeFileName, type PacingClock, type TransferClient, toBase64, upload, UPLOAD_RESTARTS } from '../src/app/transfer.ts';
import { api } from '../src/remote/index.ts';

/** A fake desktop: serves `file` as download `dl-1` and records upload chunks. */
class FakeDesktop implements TransferClient {
  readonly ops: api.Op[] = [];
  readonly uploads = new Map<string, Uint8Array[]>();
  /** Throws `disconnected` on the nth upload_chunk (1-based), once. */
  dropOnChunk: number | null = null;
  onlineWaits = 0;
  #chunks = 0;
  #uploads = 0;

  constructor(
    readonly file: Uint8Array,
    readonly chunk = api.MAX_TRANSFER_CHUNK,
  ) {}

  async request(op: api.Op): Promise<api.Resp> {
    this.ops.push(op);
    switch (op.op) {
      case 'blob':
      case 'export':
        return { type: 'download', download: 'dl-1', name: 'shot.png', mime: 'image/png', size: this.file.length };
      case 'download': {
        const end = Math.min(this.file.length, op.offset + this.chunk);
        return { type: 'chunk', data: toBase64(this.file.subarray(op.offset, end)), eof: end >= this.file.length };
      }
      case 'upload_begin': {
        const id = `up-${++this.#uploads}`;
        this.uploads.set(id, []);
        return { type: 'upload', upload: id };
      }
      case 'upload_chunk': {
        this.#chunks++;
        if (this.dropOnChunk === this.#chunks) {
          this.dropOnChunk = null;
          throw new ClientError('disconnected', 'lost');
        }
        const raw = fromBase64(op.data);
        expect(raw.length).toBeLessThanOrEqual(api.MAX_TRANSFER_CHUNK);
        expect(op.data.length).toBeLessThanOrEqual(api.MAX_TRANSFER_CHUNK_B64);
        const parts = this.uploads.get(op.upload) ?? [];
        expect(op.offset).toBe(parts.reduce((sum, part) => sum + part.length, 0));
        parts.push(raw);
        return { type: 'unit' };
      }
      case 'upload_cancel':
        this.uploads.delete(op.upload);
        return { type: 'unit' };
      default:
        throw new Error(`unexpected ${op.op}`);
    }
  }

  async whenOnline(): Promise<void> {
    this.onlineWaits++;
  }
}

function bytes(length: number): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(length);
  for (let i = 0; i < length; i++) out[i] = (i * 31 + 7) & 0xff;
  return out;
}

function named(data: Uint8Array<ArrayBuffer>, name = 'notes.bin', type = 'application/octet-stream'): Blob & { name: string } {
  return Object.assign(new Blob([data], { type }), { name });
}

describe('base64', () => {
  test('round-trips every byte value and long inputs', () => {
    const data = bytes(200_000);
    expect(fromBase64(toBase64(data))).toEqual(data);
    expect(toBase64(Uint8Array.of(0, 255, 128))).toBe(Buffer.from([0, 255, 128]).toString('base64'));
    expect(toBase64(new Uint8Array(0))).toBe('');
  });
});

describe('download', () => {
  test('pulls chunks until eof and reassembles one Blob, reporting progress', async () => {
    const file = bytes(1_300_000);
    const desktop = new FakeDesktop(file);
    const progress: number[] = [];
    const got = await download(desktop, { op: 'blob', session: 's', blob: 'b-1' }, { onProgress: (done) => progress.push(done) });
    expect(got).toMatchObject({ name: 'shot.png', mime: 'image/png', size: file.length });
    expect(new Uint8Array(await got.blob.arrayBuffer())).toEqual(file);
    expect(got.blob.type).toBe('application/octet-stream');
    expect(desktop.ops.flatMap((op) => (op.op === 'download' ? [op.offset] : []))).toEqual([0, 524_288, 1_048_576]);
    expect(progress).toEqual([0, 524_288, 1_048_576, 1_300_000]);
  });

  test('an empty file is one eof chunk', async () => {
    const got = await download(new FakeDesktop(new Uint8Array(0)), { op: 'export', session: 's' });
    expect(got.size).toBe(0);
  });

  test('cancel stops pulling with an AbortError', async () => {
    const desktop = new FakeDesktop(bytes(1_300_000));
    const controller = new AbortController();
    const error = await download(desktop, { op: 'export', session: 's' }, {
      signal: controller.signal,
      onProgress: (done) => {
        if (done > 0) controller.abort();
      },
    }).catch((caught: unknown) => caught);
    expect(isAbort(error)).toBe(true);
    expect(desktop.ops.filter((op) => op.op === 'download')).toHaveLength(1);
  });

  test('a short download is an error', async () => {
    const desktop = new FakeDesktop(bytes(10));
    desktop.request = async (op) => (op.op === 'download' ? { type: 'chunk', data: toBase64(bytes(4)), eof: true } : { type: 'download', download: 'd', name: 'x', mime: 'text/plain', size: 10 });
    await expect(download(desktop, { op: 'export', session: 's' })).rejects.toThrow('ended after 4 of 10 bytes');
  });
});

describe('download safety', () => {
  const file = (mime: string) => ({ name: 'x', mime, size: 3, blob: new Blob([new Uint8Array([1, 2, 3])], { type: 'application/octet-stream' }) });

  test('only raster image types are re-typed for display', () => {
    for (const mime of ['image/png', 'image/jpeg', 'image/gif', 'image/webp', ' IMAGE/PNG ']) {
      expect(rasterBlob(file(mime))?.type).toBe(mime.trim().toLowerCase());
    }
    for (const mime of ['image/svg+xml', 'text/html', 'application/octet-stream', 'image/png; x=1', '']) {
      expect(rasterBlob(file(mime))).toBeNull();
    }
  });

  test('file names lose paths, control characters and leading dots, and are capped', () => {
    expect(sanitizeFileName('../../etc/passwd')).toBe('passwd');
    expect(sanitizeFileName('C:\\Users\\me\\a.txt')).toBe('a.txt');
    expect(sanitizeFileName('.bashrc')).toBe('bashrc');
    expect(sanitizeFileName('...hidden.txt')).toBe('hidden.txt');
    expect(sanitizeFileName('a\u0000b\nc\u007f.png')).toBe('abc.png');
    expect(sanitizeFileName('dir/')).toBe('download');
    expect(sanitizeFileName('')).toBe('download');
    expect(sanitizeFileName('a'.repeat(300))).toHaveLength(120);
    expect(sanitizeFileName('report.html')).toBe('report.html');
  });
});

describe('upload', () => {
  test('sends ≤512 KiB chunks in order and answers the upload id', async () => {
    const data = bytes(1_200_000);
    const desktop = new FakeDesktop(new Uint8Array(0));
    const progress: [number, number][] = [];
    const id = await upload(desktop, named(data, 'shot.png', 'image/png'), { onProgress: (done, total) => progress.push([done, total]) });
    expect(id).toBe('up-1');
    expect(desktop.ops[0]).toEqual({ op: 'upload_begin', name: 'shot.png', mime: 'image/png', size: 1_200_000 });
    const parts = desktop.uploads.get('up-1') ?? [];
    expect(parts.map((part) => part.length)).toEqual([524_288, 524_288, 151_424]);
    const joined = new Uint8Array(await new Blob(parts as BlobPart[]).arrayBuffer());
    expect(joined).toEqual(data);
    expect(progress.at(-1)).toEqual([1_200_000, 1_200_000]);
  });

  test('a missing type is sent as application/octet-stream', async () => {
    const desktop = new FakeDesktop(new Uint8Array(0));
    await upload(desktop, named(bytes(3), 'x', ''));
    expect(desktop.ops[0]).toMatchObject({ mime: 'application/octet-stream' });
  });

  test('abort sends upload_cancel and rejects with an AbortError', async () => {
    const desktop = new FakeDesktop(new Uint8Array(0));
    const controller = new AbortController();
    const error = await upload(desktop, named(bytes(1_200_000)), {
      signal: controller.signal,
      onProgress: (done) => {
        if (done > 0) controller.abort();
      },
    }).catch((caught: unknown) => caught);
    expect(isAbort(error)).toBe(true);
    expect(desktop.ops.at(-1)).toEqual({ op: 'upload_cancel', upload: 'up-1' });
  });

  test('a lost connection restarts the upload from the start once online', async () => {
    const data = bytes(700_000);
    const desktop = new FakeDesktop(new Uint8Array(0));
    desktop.dropOnChunk = 2;
    const id = await upload(desktop, named(data));
    expect(id).toBe('up-2');
    expect(desktop.onlineWaits).toBe(1);
    expect(desktop.ops.filter((op) => op.op === 'upload_begin')).toHaveLength(2);
    expect(desktop.ops.some((op) => op.op === 'upload_cancel')).toBe(false);
    const joined = new Uint8Array(await new Blob((desktop.uploads.get('up-2') ?? []) as BlobPart[]).arrayBuffer());
    expect(joined).toEqual(data);
  });

  test('restarts are bounded', async () => {
    const desktop = new FakeDesktop(new Uint8Array(0));
    desktop.request = async (op) => {
      if (op.op === 'upload_begin') return { type: 'upload', upload: 'u' };
      throw new ClientError('disconnected', 'lost');
    };
    await expect(upload(desktop, named(bytes(10)))).rejects.toThrow('lost');
    expect(desktop.onlineWaits).toBe(UPLOAD_RESTARTS);
  });

  test('a file over the limit is refused before anything is sent', async () => {
    const desktop = new FakeDesktop(new Uint8Array(0));
    const big = Object.assign(new Blob([]), { name: 'huge.mov' });
    Object.defineProperty(big, 'size', { value: api.MAX_UPLOAD_FILE + 1 });
    await expect(upload(desktop, big)).rejects.toThrow('larger than 20 MiB');
    expect(desktop.ops).toHaveLength(0);
  });

  test('a desktop error cancels the upload', async () => {
    const desktop = new FakeDesktop(new Uint8Array(0));
    const inner = desktop.request.bind(desktop);
    desktop.request = async (op) => {
      if (op.op === 'upload_chunk') throw new ClientError('api', 'quota', { code: 'too_large' });
      return inner(op);
    };
    await expect(upload(desktop, named(bytes(10)))).rejects.toThrow('quota');
    expect(desktop.ops.at(-1)).toEqual({ op: 'upload_cancel', upload: 'up-1' });
  });
});

/** The stock relay's device bucket (RateLimit::DEVICE), charged per message by wire bytes. */
class SimulatedRelay {
  tokens = 4 * 1024 * 1024;
  last = 0;
  lowest = Infinity;
  closes = 0;
  constructor(readonly clock: { t: number }) {}
  /** `false` (4029) when the message does not fit. */
  take(bytes: number): boolean {
    this.tokens = Math.min(4 * 1024 * 1024, this.tokens + ((this.clock.t - this.last) / 1000) * 1024 * 1024);
    this.last = this.clock.t;
    if (this.tokens < bytes) {
      this.closes++;
      return false;
    }
    this.tokens -= bytes;
    this.lowest = Math.min(this.lowest, this.tokens);
    return true;
  }
  reconnect(): void {
    this.tokens = 4 * 1024 * 1024;
    this.last = this.clock.t;
  }
}

describe('paced upload', () => {
  function rig() {
    const time = { t: 0 };
    const pacing: PacingClock = {
      now: () => time.t,
      sleep: async (ms) => {
        time.t += ms;
      },
    };
    const relay = new SimulatedRelay(time);
    return { time, pacing, relay };
  }

  /** A desktop behind the simulated relay; the wire size is measured from the real JSON, independent of the pacer's estimate. */
  function behind(relay: SimulatedRelay): FakeDesktop {
    const desktop = new FakeDesktop(new Uint8Array(0));
    const inner = desktop.request.bind(desktop);
    desktop.request = async (op) => {
      if (op.op === 'upload_chunk') {
        const wire = JSON.stringify({ id: 1, ...op }).length + 64 + 2 * 33;
        if (!relay.take(wire)) {
          relay.reconnect();
          throw new ClientError('disconnected', 'closed 4029');
        }
      }
      return inner(op);
    };
    return desktop;
  }

  test('a 20 MiB upload never exceeds the relay bucket and stays under its sustained rate', async () => {
    const { time, pacing, relay } = rig();
    const desktop = behind(relay);
    const data = new Uint8Array(api.MAX_UPLOAD_FILE);
    const etas: number[] = [];
    await upload(desktop, named(data), { pacing, onProgress: (_d, _t, eta) => { if (eta !== undefined) etas.push(eta); } });
    expect(relay.closes).toBe(0);
    expect(desktop.onlineWaits).toBe(0);
    // Sustained rate: everything beyond the 4 MiB burst took at least (20-4) MiB / 1 MiB/s.
    expect(time.t / 1000).toBeGreaterThan(16);
    // ETA counts down towards zero and is realistic from the start of the paced phase.
    expect(etas.at(-1)).toBe(0);
    expect(etas.every((eta) => eta >= 0)).toBe(true);
    expect(etas[0]).toBeGreaterThan(10);
  });

  test('four concurrent 5 MiB uploads share one bucket and never exceed the relay', async () => {
    const time = { t: 0 };
    const sleepers: { at: number; wake: () => void }[] = [];
    const pacing: PacingClock = {
      now: () => time.t,
      sleep: (ms) => new Promise<void>((wake) => sleepers.push({ at: time.t + ms, wake })),
    };
    const relay = new SimulatedRelay(time);
    const desktop = behind(relay);
    const files = Array.from({ length: 4 }, (_, i) => named(new Uint8Array(5 * 1024 * 1024), `f${i}`));
    let finished = 0;
    const all = Promise.all(files.map((file) => upload(desktop, file, { pacing }).then(() => finished++)));
    // Virtual time: advance to the earliest sleeper only once every task is idle in a sleep.
    while (finished < files.length) {
      for (let i = 0; i < 8; i++) await new Promise((resolve) => setTimeout(resolve, 0));
      sleepers.sort((a, b) => a.at - b.at);
      const next = sleepers.shift();
      if (next === undefined) continue;
      time.t = Math.max(time.t, next.at);
      next.wake();
    }
    await all;
    expect(relay.closes).toBe(0);
    expect(desktop.onlineWaits).toBe(0);
    // 20 MiB total: at most the 4 MiB burst is free, the rest at the sustained rate.
    expect(time.t / 1000).toBeGreaterThan(16);
  });

  test('a small file goes out in the burst without waiting', async () => {
    const { time, pacing, relay } = rig();
    await upload(behind(relay), named(new Uint8Array(2 * 1024 * 1024)), { pacing });
    expect(time.t).toBe(0);
  });

  test('a 4029 close backs off and keeps pacing instead of restarting at full speed', async () => {
    const { time, pacing, relay } = rig();
    const desktop = behind(relay);
    // Another channel already spent most of the burst: the first chunks overrun the relay.
    relay.tokens = 600_000;
    await upload(desktop, named(new Uint8Array(6 * 1024 * 1024)), { pacing });
    expect(relay.closes).toBe(1);
    expect(desktop.onlineWaits).toBe(1);
    // After the close the restart did not burst: it was paced at <= the sustained rate.
    expect(time.t / 1000).toBeGreaterThan(5);
    const second = desktop.ops.filter((op) => op.op === 'upload_begin');
    expect(second).toHaveLength(2);
  });

  test('the pacer assumes more wire bytes than a real chunk takes', () => {
    const op = { op: 'upload_chunk' as const, upload: 'up-1', offset: 20_000_000, data: 'A'.repeat(api.MAX_TRANSFER_CHUNK_B64) };
    expect(chunkWireBytes(api.MAX_TRANSFER_CHUNK)).toBeGreaterThan(JSON.stringify({ id: 1, ...op }).length + 64 + 2 * 33);
  });
});
