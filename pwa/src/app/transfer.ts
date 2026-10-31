// Downloads and uploads over the device protocol (mobile-pwa.md "Downloads are pull-based JSON",
// "Uploads use the same scheme in reverse"): JSON requests carrying ≤ 512 KiB of raw bytes as
// base64 each, no new record kind.
//
// - Download: `blob`/`export` answer a handle; `download { offset }` is pulled until `eof`, the
//   chunks become one Blob, saved through an `<a download>` object URL.
// - Upload: `upload_begin` → `upload_chunk`s in order → the id goes into `compose`. A lost
//   connection restarts the upload from the start once the phone is back online (the gateway
//   does not resume); an abort sends `upload_cancel`.

import { api } from '../remote/index.ts';
import { MAX_FRAGMENT_DATA } from '../remote/protocol.ts';
import { TAG_LEN } from '../remote/noise.ts';
import { ClientError } from './client.ts';

/** What transfers need from the client. */
export interface TransferClient {
  request(op: api.Op): Promise<api.Resp>;
  /** Resolves once online again (rejects when `signal` aborts). */
  whenOnline(signal?: AbortSignal): Promise<void>;
}

export interface TransferOptions {
  /** Bytes done so far and the total; uploads add the estimated seconds left (pacing included). */
  onProgress?: (done: number, total: number, etaSeconds?: number) => void;
  signal?: AbortSignal;
  /** Uploads only: the clock and sleep the pacer uses (tests). */
  pacing?: PacingClock;
}

/** Time source of an {@link UploadPacer}. */
export interface PacingClock {
  now(): number;
  sleep(ms: number, signal?: AbortSignal): Promise<void>;
}

const realClock: PacingClock = {
  now: () => performance.now(),
  sleep: (ms, signal) =>
    new Promise((resolve, reject) => {
      if (signal?.aborted) return reject(aborted());
      const timer = setTimeout(() => {
        signal?.removeEventListener('abort', onAbort);
        resolve();
      }, ms);
      const onAbort = () => {
        clearTimeout(timer);
        reject(aborted());
      };
      signal?.addEventListener('abort', onAbort, { once: true });
    }),
};

// The stock relay (`RateLimit::DEVICE` in crates/deck-relay) charges every device→relay message by
// its byte length: 1 MiB/s sustained, 4 MiB burst, and closes with 4029 when the bucket runs dry.
// Uploads are paced with a token bucket of our own that stays under it.
const RELAY_RATE = 1024 * 1024;
const RELAY_BURST = 4 * 1024 * 1024;
/** Share of the relay's sustained rate an upload may use (the rest: acks, pings, jitter). */
export const PACE_SHARE = 0.85;
/** Burst an upload spends up front: the relay's, minus headroom for earlier traffic. */
export const PACE_BURST = 0.75 * RELAY_BURST;
/** Request envelope (record header, ids, op name, offset) over the base64 data, rounded up. */
const ENVELOPE_BYTES = 512;
/** Rate multiplier after a lost connection, and its floor. */
const BACKOFF_FACTOR = 0.7;
/** Losses closer together than this are one event (concurrent uploads dying with one link). */
const BACKOFF_DEBOUNCE_MS = 2000;
const MIN_RATE = 0.2 * RELAY_RATE;

const pacers = new WeakMap<object, UploadPacer>();

/** The client's one pacer (its uplink is one relay bucket). */
export function pacerFor(client: object, clock: PacingClock): UploadPacer {
  let pacer = pacers.get(client);
  if (pacer === undefined) {
    pacer = new UploadPacer(clock);
    pacers.set(client, pacer);
  }
  return pacer;
}

/** Bytes one `upload_chunk` of `raw` file bytes puts on the relay link: base64, JSON, and Noise frames. */
export function chunkWireBytes(raw: number): number {
  const message = 4 * Math.ceil(raw / 3) + ENVELOPE_BYTES;
  const frames = Math.max(1, Math.ceil(message / MAX_FRAGMENT_DATA));
  return message + frames * (TAG_LEN + 1);
}

/**
 * A token bucket over outbound wire bytes. The relay has one bucket per device connection, so there
 * is one pacer per client, shared by all its uploads and kept across reconnects.
 */
export class UploadPacer {
  #rate: number;
  #burst: number;
  #tokens: number;

  constructor(
    private readonly clock: PacingClock,
    rate = RELAY_RATE * PACE_SHARE,
    burst = PACE_BURST,
  ) {
    this.#rate = rate;
    this.#burst = burst;
    this.#tokens = burst;
    this.#last = clock.now();
  }

  #last: number;
  #backedOffAt: number | null = null;

  #refill(): void {
    const now = this.clock.now();
    this.#tokens = Math.min(this.#burst, this.#tokens + ((now - this.#last) / 1000) * this.#rate);
    this.#last = now;
  }

  /**
   * Waits until `bytes` may be sent. The bytes are reserved at once (the bucket may go into debt),
   * so concurrent callers queue behind each other instead of all seeing the same tokens.
   */
  async take(bytes: number, signal?: AbortSignal): Promise<void> {
    this.#refill();
    this.#tokens -= bytes;
    if (this.#tokens >= 0) return;
    try {
      await this.clock.sleep(Math.ceil((-this.#tokens / this.#rate) * 1000), signal);
    } catch (error) {
      this.#tokens += bytes;
      throw error;
    }
  }

  /** Seconds until `bytes` more have been sent. */
  etaSeconds(bytes: number): number {
    this.#refill();
    return Math.max(0, (bytes - this.#tokens) / this.#rate);
  }

  /**
   * The connection was lost mid-upload (maybe a 4029): slow down, and start with an empty bucket.
   * Uploads that fail together on one loss back off once.
   */
  backOff(): void {
    const now = this.clock.now();
    if (this.#backedOffAt !== null && now - this.#backedOffAt < BACKOFF_DEBOUNCE_MS) return;
    this.#backedOffAt = now;
    this.#rate = Math.max(MIN_RATE, this.#rate * BACKOFF_FACTOR);
    this.#burst = Math.min(this.#burst, chunkWireBytes(api.MAX_TRANSFER_CHUNK));
    this.#tokens = Math.min(this.#tokens, 0);
    this.#last = now;
  }
}

export interface Downloaded {
  name: string;
  mime: string;
  size: number;
  blob: Blob;
}

/** How many times an upload restarts after a lost connection. */
export const UPLOAD_RESTARTS = 3;

/** Standard (padded) base64 of `bytes`, as the gateway decodes it. */
export function toBase64(bytes: Uint8Array): string {
  let binary = '';
  for (let offset = 0; offset < bytes.length; offset += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + 0x8000));
  }
  return btoa(binary);
}

/** Decodes standard base64; throws on anything else. */
export function fromBase64(text: string): Uint8Array<ArrayBuffer> {
  const binary = atob(text);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
  return out;
}

function aborted(): DOMException {
  return new DOMException('The transfer was cancelled.', 'AbortError');
}

/** Whether `error` is a cancelled transfer. */
export function isAbort(error: unknown): boolean {
  return error instanceof DOMException && error.name === 'AbortError';
}

function expect<T extends api.Resp['type']>(resp: api.Resp, type: T): Extract<api.Resp, { type: T }> {
  if (resp.type !== type) throw new ClientError('invalid', `unexpected answer ${resp.type} (wanted ${type})`);
  return resp as Extract<api.Resp, { type: T }>;
}

/** Fetches an image (`blob`) or a session export (`export`) as one Blob. */
export async function download(
  client: TransferClient,
  op: Extract<api.Op, { op: 'blob' | 'export' }>,
  options: TransferOptions = {},
): Promise<Downloaded> {
  const { signal, onProgress } = options;
  if (signal?.aborted) throw aborted();
  const handle = expect(await client.request(op), 'download');
  const parts: Uint8Array<ArrayBuffer>[] = [];
  let offset = 0;
  onProgress?.(0, handle.size);
  for (;;) {
    if (signal?.aborted) throw aborted();
    const chunk = expect(await client.request({ op: 'download', download: handle.download, offset }), 'chunk');
    const bytes = fromBase64(chunk.data);
    parts.push(bytes);
    offset += bytes.length;
    onProgress?.(offset, Math.max(handle.size, offset));
    if (chunk.eof) break;
    if (bytes.length === 0) throw new ClientError('invalid', 'the desktop sent an empty chunk before the end');
  }
  if (handle.size > 0 && offset !== handle.size) {
    throw new ClientError('invalid', `the download ended after ${offset} of ${handle.size} bytes`);
  }
  // Typed as opaque bytes: whatever the desktop's metadata claims, a download is never a document the
  // browser could render (html, svg). Images are re-typed by `rasterBlob`.
  return { name: handle.name, mime: handle.mime, size: offset, blob: new Blob(parts, { type: 'application/octet-stream' }) };
}

const RASTER_TYPES: ReadonlySet<string> = new Set(['image/png', 'image/jpeg', 'image/gif', 'image/webp']);

/** The image bytes of `file` typed as their raster type, or null when it is not a raster image (svg, html, …). */
export function rasterBlob(file: Downloaded): Blob | null {
  const mime = file.mime.trim().toLowerCase();
  return RASTER_TYPES.has(mime) ? file.blob.slice(0, file.blob.size, mime) : null;
}

const MAX_FILE_NAME = 120;

/** A file name that is safe to hand to `<a download>`: no path parts, control characters or leading dots. */
export function sanitizeFileName(name: string): string {
  const base = name.split(/[\\/]/).pop() ?? '';
  // biome-ignore lint/suspicious/noControlCharactersInRegex: stripping control characters is the point
  const cleaned = base.replace(/[\u0000-\u001f\u007f-\u009f]/g, '').replace(/^[.\s]+/, '').trim();
  const capped = Array.from(cleaned).slice(0, MAX_FILE_NAME).join('');
  return capped.length > 0 ? capped : 'download';
}

/** Saves `blob` as a file named `name` (an `<a download>` click on an object URL). */
export function saveBlob(blob: Blob, name: string): void {
  const url = URL.createObjectURL(blob.type === 'application/octet-stream' ? blob : blob.slice(0, blob.size, 'application/octet-stream'));
  const link = document.createElement('a');
  link.href = url;
  link.download = sanitizeFileName(name);
  link.rel = 'noopener';
  link.style.display = 'none';
  document.body.append(link);
  link.click();
  link.remove();
  // Give the browser time to start the save before the URL goes away.
  setTimeout(() => URL.revokeObjectURL(url), 60_000);
}

/** {@link download}, then {@link saveBlob}. */
export async function downloadAndSave(
  client: TransferClient,
  op: Extract<api.Op, { op: 'blob' | 'export' }>,
  options: TransferOptions = {},
): Promise<Downloaded> {
  const file = await download(client, op, options);
  saveBlob(file.blob, file.name);
  return file;
}

function lostConnection(error: unknown): boolean {
  return error instanceof ClientError && (error.kind === 'disconnected' || error.kind === 'offline');
}

async function uploadOnce(client: TransferClient, file: Blob & { name: string }, options: TransferOptions, pacer: UploadPacer): Promise<string> {
  const { signal, onProgress } = options;
  const begun = expect(
    await client.request({ op: 'upload_begin', name: file.name, mime: file.type.length > 0 ? file.type : 'application/octet-stream', size: file.size }),
    'upload',
  );
  const upload = begun.upload;
  try {
    onProgress?.(0, file.size);
    for (let offset = 0; offset < file.size; offset += api.MAX_TRANSFER_CHUNK) {
      if (signal?.aborted) throw aborted();
      const bytes = new Uint8Array(await file.slice(offset, offset + api.MAX_TRANSFER_CHUNK).arrayBuffer());
      await pacer.take(chunkWireBytes(bytes.length), signal);
      expect(await client.request({ op: 'upload_chunk', upload, offset, data: toBase64(bytes) }), 'unit');
      const done = offset + bytes.length;
      const left = file.size - done;
      const wireLeft = Math.floor(left / api.MAX_TRANSFER_CHUNK) * chunkWireBytes(api.MAX_TRANSFER_CHUNK) + (left % api.MAX_TRANSFER_CHUNK > 0 ? chunkWireBytes(left % api.MAX_TRANSFER_CHUNK) : 0);
      onProgress?.(done, file.size, pacer.etaSeconds(wireLeft));
    }
    if (signal?.aborted) throw aborted();
    return upload;
  } catch (error) {
    // Free the desktop's slot; on a lost connection the upload is gone with it anyway.
    if (!lostConnection(error)) client.request({ op: 'upload_cancel', upload }).catch(() => undefined);
    throw error;
  }
}

/**
 * Uploads `file` to the desktop for a later `compose`; resolves with the upload id. Files over
 * {@link api.MAX_UPLOAD_FILE} are refused before anything is sent.
 */
export async function upload(client: TransferClient, file: Blob & { name: string }, options: TransferOptions = {}): Promise<string> {
  if (file.size > api.MAX_UPLOAD_FILE) {
    throw new ClientError('invalid', `${file.name} is larger than ${api.MAX_UPLOAD_FILE / (1024 * 1024)} MiB.`);
  }
  const pacer = pacerFor(client, options.pacing ?? realClock);
  for (let attempt = 0; ; attempt++) {
    if (options.signal?.aborted) throw aborted();
    try {
      return await uploadOnce(client, file, options, pacer);
    } catch (error) {
      if (!lostConnection(error) || attempt >= UPLOAD_RESTARTS) throw error;
      pacer.backOff();
      await client.whenOnline(options.signal);
    }
  }
}
