// Transcript images (`ImageRef`s) fetched with the `blob` op and shown as object URLs. A small LRU
// cache keeps the URLs within a byte budget; images on screen are retained and never revoked
// under them. Large images wait for a tap.

import type { api } from '../../../remote/index.ts';
import { ClientError } from '../../client.ts';

/** Images up to this size load by themselves; larger ones (or unknown sizes above it) wait for a tap. */
export const AUTO_LOAD_BYTES = 4 * 1024 * 1024;
/** Bytes of cached images at most (images on screen may exceed it until they leave). */
export const CACHE_BYTES = 32 * 1024 * 1024;

export function imageKey(session: string, agent: string | null, blob: string): string {
  return `${session}\u0000${agent ?? ''}\u0000${blob}`;
}

/** Whether a reference loads without a tap (`bytes` 0 = unknown: load, the gateway caps transfers). */
export function autoLoads(image: api.ImageRef): boolean {
  return image.bytes <= AUTO_LOAD_BYTES;
}

interface Entry {
  url: string;
  size: number;
  refs: number;
}

export interface UrlFactory {
  create(blob: Blob): string;
  revoke(url: string): void;
}

export const browserUrls: UrlFactory = {
  create: (blob) => URL.createObjectURL(blob),
  revoke: (url) => URL.revokeObjectURL(url),
};

export class ImageCache {
  readonly #urls: UrlFactory;
  readonly #budget: number;
  /** Insertion order = least recently used first. */
  readonly #entries = new Map<string, Entry>();
  readonly #loading = new Map<string, Promise<string>>();
  #bytes = 0;

  constructor(urls: UrlFactory = browserUrls, budget = CACHE_BYTES) {
    this.#urls = urls;
    this.#budget = budget;
  }

  get bytes(): number {
    return this.#bytes;
  }

  get size(): number {
    return this.#entries.size;
  }

  /** The cached URL (marking it recently used), or undefined. */
  peek(key: string): string | undefined {
    const entry = this.#entries.get(key);
    if (entry === undefined) return undefined;
    this.#entries.delete(key);
    this.#entries.set(key, entry);
    return entry.url;
  }

  /** The URL for `key`, fetching it once (concurrent callers share the fetch). */
  load(key: string, fetch: () => Promise<Blob>): Promise<string> {
    const cached = this.peek(key);
    if (cached !== undefined) return Promise.resolve(cached);
    const pending = this.#loading.get(key);
    if (pending !== undefined) return pending;
    const promise = fetch().then(
      (blob) => {
        this.#loading.delete(key);
        const url = this.#urls.create(blob);
        this.#entries.set(key, { url, size: blob.size, refs: 0 });
        this.#bytes += blob.size;
        this.#evict();
        return url;
      },
      (error: unknown) => {
        this.#loading.delete(key);
        throw error;
      },
    );
    this.#loading.set(key, promise);
    return promise;
  }

  /** Marks `key` on screen: it is not revoked until released. */
  retain(key: string): void {
    const entry = this.#entries.get(key);
    if (entry !== undefined) entry.refs += 1;
  }

  release(key: string): void {
    const entry = this.#entries.get(key);
    if (entry === undefined) return;
    entry.refs = Math.max(0, entry.refs - 1);
    this.#evict();
  }

  /** Revokes every unretained URL (e.g. when the pairing is forgotten). */
  clear(): void {
    for (const [key, entry] of this.#entries) {
      if (entry.refs > 0) continue;
      this.#drop(key, entry);
    }
  }

  #evict(): void {
    if (this.#bytes <= this.#budget) return;
    for (const [key, entry] of this.#entries) {
      if (this.#bytes <= this.#budget) return;
      if (entry.refs > 0) continue;
      this.#drop(key, entry);
    }
  }

  #drop(key: string, entry: Entry): void {
    this.#entries.delete(key);
    this.#bytes -= entry.size;
    this.#urls.revoke(entry.url);
  }
}

/** The app's shared transcript image cache. */
export const transcriptImages = new ImageCache();

/** The gateway serves two blob requests per device at once. */
export const BLOB_CONCURRENCY = 2;
const BUSY_RETRIES = 8;
const BUSY_BACKOFF_MS = 300;
const BUSY_BACKOFF_MAX_MS = 5_000;

function tooManyRequests(error: unknown): boolean {
  return error instanceof ClientError && error.api?.code === 'too_many_requests';
}

/**
 * Runs automatic blob loads at most `concurrency` at a time. A `too_many_requests` answer (another
 * view of the device holds the gateway's slots) is waited out with backoff and tried again outside
 * the slot, so it never surfaces as an image failure unless it persists.
 */
export class LoadQueue {
  readonly #concurrency: number;
  readonly #sleep: (ms: number) => Promise<void>;
  readonly #waiting: Array<() => void> = [];
  #active = 0;

  constructor(concurrency = BLOB_CONCURRENCY, sleep: (ms: number) => Promise<void> = (ms) => new Promise((done) => setTimeout(done, ms))) {
    this.#concurrency = concurrency;
    this.#sleep = sleep;
  }

  async run<T>(task: () => Promise<T>): Promise<T> {
    for (let attempt = 0; ; attempt += 1) {
      await this.#acquire();
      try {
        return await task();
      } catch (error) {
        if (!tooManyRequests(error) || attempt >= BUSY_RETRIES) throw error;
      } finally {
        this.#release();
      }
      await this.#sleep(Math.min(BUSY_BACKOFF_MAX_MS, BUSY_BACKOFF_MS * 2 ** attempt));
    }
  }

  async #acquire(): Promise<void> {
    if (this.#active < this.#concurrency) {
      this.#active += 1;
      return;
    }
    await new Promise<void>((resume) => this.#waiting.push(resume));
  }

  #release(): void {
    const next = this.#waiting.shift();
    if (next === undefined) this.#active -= 1;
    else next();
  }
}

/** The shared queue of automatic transcript image loads. */
export const imageLoads = new LoadQueue();
