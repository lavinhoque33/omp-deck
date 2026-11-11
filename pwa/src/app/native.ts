// The Android app's native bridge (see the shared contract in docs/notes: "Native JS bridge protocol").
//
// Inside the APK the page runs from a bundled asset origin and the shell injects
// `window.OmpDeckAndroid` (a main-frame, origin-scoped WebMessageListener): `postMessage(string)`
// carries requests `{id, method, params}` to the app, and everything comes back through
// `onmessage` as JSON strings: replies `{id, result?, error?}` and events `{event, detail}`.
// Nothing here exists in a browser: `nativeHost()` is null there and every caller keeps its
// browser path.
//
// Every method and event has an entry in the exhaustive registries below (params checked before
// anything is posted, results and event details decoded before anyone sees them), so a malformed
// shell message becomes a `NativeError`, never an `undefined` deep in the UI.

import { isDesktopId } from '../push.ts';

/** `window.OmpDeckAndroid`, as the WebMessageListener injects it. */
export interface RawBridge {
  postMessage(message: string): void;
  onmessage: ((event: { data: unknown }) => void) | null;
}

export type NativePermission = 'default' | 'granted' | 'denied';

export interface NativeInfo {
  platform: 'android';
  version: string;
}

export interface NativePushState {
  permission: NativePermission;
  /** Firebase is set up on this phone. */
  configured: boolean;
  /** The current FCM registration token, once Firebase has issued one. */
  token: string | null;
}

export interface NativeSubscription {
  token: string;
  /** Base64url (unpadded) of the 32-byte AES key shared with desktop `desktop`. */
  key: string;
  desktop: string;
}

export type NativeUpdatePhase = 'idle' | 'checking' | 'available' | 'downloading' | 'ready' | 'installing' | 'error';

/** The Android app's own updater (GitHub release APK), distinct from the desktop's gateway/omp updates. */
export interface NativeUpdateState {
  current: string;
  latest: string | null;
  phase: NativeUpdatePhase;
  /** Bytes downloaded so far. */
  downloaded: number;
  /** The APK's size, once known. */
  total: number | null;
  checked_unix_ms: number | null;
  auto_check: boolean;
  error: string | null;
}

/** Decoded bytes of one `saveFile` at most (the shell refuses more). */
export const MAX_SAVE_BYTES = 20 * 1024 * 1024;

export interface SaveFileParams {
  name: string;
  mime: string;
  /** Standard (padded) base64. */
  data: string;
}

/** The shell's methods: params in, decoded result out. */
export interface NativeCalls {
  info: { params: Record<string, never>; result: NativeInfo };
  pushState: { params: Record<string, never>; result: NativePushState };
  /** `config`: the desktop's public google-services.json, consumed only while Firebase is not set up on this phone. */
  pushEnable: { params: { config?: string }; result: NativePushState };
  pushSubscription: { params: { desktop: string; label: string }; result: NativeSubscription };
  pushForget: { params: { desktop: string }; result: null };
  pushLabels: { params: { labels: Record<string, string> }; result: null };
  setPresence: { params: { desktop: string | null; visible: boolean }; result: null };
  saveFile: { params: SaveFileParams; result: null };
  finish: { params: Record<string, never>; result: null };
  updateState: { params: Record<string, never>; result: NativeUpdateState };
  updateCheck: { params: Record<string, never>; result: NativeUpdateState };
  updateInstall: { params: Record<string, never>; result: NativeUpdateState };
  updateConfigure: { params: { auto_check: boolean }; result: NativeUpdateState };
}

export type NativeMethod = keyof NativeCalls;

/** What the shell announces on its own. */
export interface NativeEvents {
  /** The token, Firebase configuration or notification permission changed. */
  push_changed: Record<string, never>;
  /** The Activity's real visibility (the document may stay "visible" while the Activity is stopped). */
  visibility: { visible: boolean };
  /** A notification tap or deep link: path and hash, e.g. `/#s=<session>&d=<desktop>`. */
  open: { url: string };
  /** The system Back button. */
  back: Record<string, never>;
  /** The updater's state changed (a check, download progress, an error). */
  update: NativeUpdateState;
}

export type NativeEvent = keyof NativeEvents;

export type NativeErrorKind =
  /** The shell answered with an error string. */
  | 'native'
  /** The user dismissed the shell's own dialog (the shell's error `cancelled`). */
  | 'cancelled'
  /** No answer within the method's limit. */
  | 'timeout'
  /** The shell's answer broke the contract. */
  | 'protocol'
  /** The call itself was malformed (nothing was sent). */
  | 'invalid';

export class NativeError extends Error {
  readonly kind: NativeErrorKind;
  constructor(kind: NativeErrorKind, message: string) {
    super(message);
    this.name = 'NativeError';
    this.kind = kind;
  }
}

const CANCELLED = 'cancelled';

// ---- decoding ---------------------------------------------------------------------------------

function record(value: unknown, what: string): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) throw new NativeError('protocol', `${what} is not an object`);
  return value as Record<string, unknown>;
}

function text(value: unknown, what: string): string {
  if (typeof value !== 'string') throw new NativeError('protocol', `${what} is not a string`);
  return value;
}

function bool(value: unknown, what: string): boolean {
  if (typeof value !== 'boolean') throw new NativeError('protocol', `${what} is not a boolean`);
  return value;
}

function nothing(value: unknown, what: string): null {
  if (value !== null && value !== undefined) throw new NativeError('protocol', `${what} should be empty`);
  return null;
}

const BASE64URL_KEY = /^[A-Za-z0-9_-]{43}$/;

function decodeInfo(value: unknown): NativeInfo {
  const fields = record(value, 'info');
  if (fields.platform !== 'android') throw new NativeError('protocol', 'info.platform is not android');
  return { platform: 'android', version: text(fields.version, 'info.version') };
}

function decodePushState(value: unknown): NativePushState {
  const fields = record(value, 'push state');
  const permission = fields.permission;
  if (permission !== 'default' && permission !== 'granted' && permission !== 'denied') throw new NativeError('protocol', 'push state permission is not known');
  const token = fields.token;
  if (token !== null && (typeof token !== 'string' || token.length === 0)) throw new NativeError('protocol', 'push state token is not a string or null');
  return { permission, configured: bool(fields.configured, 'push state configured'), token };
}

function decodeSubscription(value: unknown): NativeSubscription {
  const fields = record(value, 'push subscription');
  const token = text(fields.token, 'subscription token');
  const key = text(fields.key, 'subscription key');
  const desktop = text(fields.desktop, 'subscription desktop');
  if (token.length === 0) throw new NativeError('protocol', 'the subscription token is empty');
  if (!BASE64URL_KEY.test(key)) throw new NativeError('protocol', 'the subscription key is not 32 bytes of unpadded base64url');
  if (!isDesktopId(desktop)) throw new NativeError('protocol', 'the subscription desktop is not a desktop id');
  return { token, key, desktop };
}

function decodeEmpty(value: unknown): Record<string, never> {
  record(value, 'event detail');
  return {};
}

const UPDATE_PHASES: readonly NativeUpdatePhase[] = ['idle', 'checking', 'available', 'downloading', 'ready', 'installing', 'error'];

function count(value: unknown, what: string): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) throw new NativeError('protocol', `${what} is not a non-negative integer`);
  return value;
}

function optional<T>(value: unknown, read: (value: unknown) => T): T | null {
  return value === null ? null : read(value);
}

export function decodeUpdateState(value: unknown): NativeUpdateState {
  const fields = record(value, 'update state');
  const phase = fields.phase;
  if (typeof phase !== 'string' || !UPDATE_PHASES.includes(phase as NativeUpdatePhase)) throw new NativeError('protocol', 'update state phase is not known');
  return {
    current: text(fields.current, 'update state current'),
    latest: optional(fields.latest, (v) => text(v, 'update state latest')),
    phase: phase as NativeUpdatePhase,
    downloaded: count(fields.downloaded, 'update state downloaded'),
    total: optional(fields.total, (v) => count(v, 'update state total')),
    checked_unix_ms: optional(fields.checked_unix_ms, (v) => count(v, 'update state checked_unix_ms')),
    auto_check: bool(fields.auto_check, 'update state auto_check'),
    error: optional(fields.error, (v) => text(v, 'update state error')),
  };
}

// ---- params -----------------------------------------------------------------------------------

function checkNoParams(params: Record<string, never>): void {
  if (Object.keys(params).length > 0) throw new NativeError('invalid', 'this call takes no parameters');
}

/** Far above any sanitized single-client google-services.json; bounds what crosses the bridge. */
export const MAX_FIREBASE_CONFIG_CHARS = 48 * 1024;

function checkPushEnable(params: { config?: string }): void {
  for (const key of Object.keys(params)) {
    if (key !== 'config') throw new NativeError('invalid', `pushEnable takes no ${key}`);
  }
  const config: unknown = params.config;
  if (config === undefined) return;
  if (typeof config !== 'string' || config.trim().length === 0 || config.length > MAX_FIREBASE_CONFIG_CHARS) {
    throw new NativeError('invalid', 'the Firebase config is not a non-empty string of a sane size');
  }
}

function checkDesktop(desktop: unknown): asserts desktop is string {
  if (!isDesktopId(desktop)) throw new NativeError('invalid', 'the desktop is not a 64-digit lowercase hex id');
}

/** Standard padded base64 of at most {@link MAX_SAVE_BYTES} decoded bytes. */
export function base64DecodedLength(data: string): number | null {
  if (data.length % 4 !== 0 || !/^[A-Za-z0-9+/]*={0,2}$/.test(data)) return null;
  const padding = data.endsWith('==') ? 2 : data.endsWith('=') ? 1 : 0;
  return (data.length / 4) * 3 - padding;
}

// ---- registries -------------------------------------------------------------------------------

interface MethodSpec<K extends NativeMethod> {
  /** Milliseconds to wait for the answer; null: the user is in a native dialog, wait for them. */
  timeoutMs: number | null;
  check(params: NativeCalls[K]['params']): void;
  decode(value: unknown): NativeCalls[K]['result'];
}

const QUICK_MS = 15_000;
/** A token fetch crosses the network (Google), and the first one may follow a config import. */
const TOKEN_MS = 45_000;

const METHODS: { readonly [K in NativeMethod]: MethodSpec<K> } = {
  info: { timeoutMs: QUICK_MS, check: checkNoParams, decode: decodeInfo },
  pushState: { timeoutMs: QUICK_MS, check: checkNoParams, decode: decodePushState },
  // The notification permission prompt is the user's: no deadline.
  pushEnable: { timeoutMs: null, check: checkPushEnable, decode: decodePushState },
  pushSubscription: {
    timeoutMs: TOKEN_MS,
    check: (params) => {
      checkDesktop(params.desktop);
      if (typeof params.label !== 'string') throw new NativeError('invalid', 'the label is not a string');
    },
    decode: decodeSubscription,
  },
  pushForget: { timeoutMs: QUICK_MS, check: (params) => checkDesktop(params.desktop), decode: (value) => nothing(value, 'pushForget result') },
  pushLabels: {
    timeoutMs: QUICK_MS,
    check: (params) => {
      for (const [desktop, label] of Object.entries(params.labels)) {
        checkDesktop(desktop);
        if (typeof label !== 'string') throw new NativeError('invalid', 'a label is not a string');
      }
    },
    decode: (value) => nothing(value, 'pushLabels result'),
  },
  setPresence: {
    timeoutMs: QUICK_MS,
    check: (params) => {
      if (params.desktop !== null) checkDesktop(params.desktop);
      if (typeof params.visible !== 'boolean') throw new NativeError('invalid', 'visible is not a boolean');
    },
    decode: (value) => nothing(value, 'setPresence result'),
  },
  // The save dialog is the user's too.
  saveFile: {
    timeoutMs: null,
    check: (params) => {
      if (typeof params.name !== 'string' || params.name.length === 0) throw new NativeError('invalid', 'the file name is empty');
      if (typeof params.mime !== 'string') throw new NativeError('invalid', 'the mime type is not a string');
      const size = typeof params.data === 'string' ? base64DecodedLength(params.data) : null;
      if (size === null) throw new NativeError('invalid', 'the file data is not standard base64');
      if (size > MAX_SAVE_BYTES) throw new NativeError('invalid', `the file is larger than ${MAX_SAVE_BYTES / (1024 * 1024)} MiB, the most Android can save from here`);
    },
    decode: (value) => nothing(value, 'saveFile result'),
  },
  finish: { timeoutMs: QUICK_MS, check: checkNoParams, decode: (value) => nothing(value, 'finish result') },
  // A check only starts work and answers at once; progress comes as `update` events.
  updateState: { timeoutMs: QUICK_MS, check: checkNoParams, decode: decodeUpdateState },
  updateCheck: { timeoutMs: QUICK_MS, check: checkNoParams, decode: decodeUpdateState },
  // The shell answers before the download; the installer itself is the user's.
  updateInstall: { timeoutMs: QUICK_MS, check: checkNoParams, decode: decodeUpdateState },
  updateConfigure: {
    timeoutMs: QUICK_MS,
    check: (params) => {
      if (typeof params.auto_check !== 'boolean') throw new NativeError('invalid', 'auto_check is not a boolean');
    },
    decode: decodeUpdateState,
  },
};

const EVENTS: { readonly [K in NativeEvent]: (detail: unknown) => NativeEvents[K] } = {
  push_changed: decodeEmpty,
  visibility: (detail) => ({ visible: bool(record(detail, 'visibility').visible, 'visibility.visible') }),
  open: (detail) => {
    const url = text(record(detail, 'open').url, 'open.url');
    if (url.length === 0 || url.length > 4096) throw new NativeError('protocol', 'open.url has an unusable length');
    return { url };
  },
  back: decodeEmpty,
  update: decodeUpdateState,
};

function isEvent(name: string): name is NativeEvent {
  return Object.hasOwn(EVENTS, name);
}

// ---- the host ---------------------------------------------------------------------------------

export interface NativeClock {
  setTimer(callback: () => void, ms: number): unknown;
  clearTimer(handle: unknown): void;
}

const realClock: NativeClock = {
  setTimer: (callback, ms) => setTimeout(callback, ms),
  clearTimer: (handle) => clearTimeout(handle as number),
};

type Listener<K extends NativeEvent> = (detail: NativeEvents[K]) => void;

interface Pending {
  method: NativeMethod;
  resolve(value: unknown): void;
  reject(error: Error): void;
  timer: unknown;
}

/** The typed, promise-based face of the shell's bridge. */
export class NativeHost {
  readonly #raw: RawBridge;
  readonly #clock: NativeClock;
  readonly #pending = new Map<string, Pending>();
  readonly #listeners = new Map<NativeEvent, Set<(detail: never) => void>>();
  #next = 0;

  constructor(raw: RawBridge, clock: NativeClock = realClock) {
    this.#raw = raw;
    this.#clock = clock;
    raw.onmessage = (event) => this.#receive(event.data);
  }

  /**
   * Tells the shell this page is up: the shell holds `open` events back until the first `info`, so
   * a notification that started the app is delivered to the listeners attached before this call.
   */
  handshake(): Promise<NativeInfo> {
    return this.request('info', {});
  }

  request<K extends NativeMethod>(method: K, params: NativeCalls[K]['params']): Promise<NativeCalls[K]['result']> {
    const spec: MethodSpec<K> = METHODS[method];
    try {
      spec.check(params);
    } catch (error) {
      return Promise.reject(error instanceof Error ? error : new NativeError('invalid', String(error)));
    }
    const id = `n${++this.#next}`;
    const { promise, resolve, reject } = Promise.withResolvers<NativeCalls[K]['result']>();
    const timer =
      spec.timeoutMs === null
        ? null
        : this.#clock.setTimer(() => {
            this.#pending.delete(id);
            reject(new NativeError('timeout', `Android did not answer ${method}`));
          }, spec.timeoutMs);
    this.#pending.set(id, {
      method,
      resolve: (value) => {
        try {
          resolve(spec.decode(value));
        } catch (error) {
          reject(error instanceof Error ? error : new NativeError('protocol', String(error)));
        }
      },
      reject,
      timer,
    });
    try {
      this.#raw.postMessage(JSON.stringify({ id, method, params }));
    } catch (error) {
      this.#settle(id)?.reject(new NativeError('native', `Android refused ${method}: ${error instanceof Error ? error.message : String(error)}`));
    }
    return promise;
  }

  /** Listens for `event`; returns the unsubscribe. */
  on<K extends NativeEvent>(event: K, listener: Listener<K>): () => void {
    let set = this.#listeners.get(event);
    if (set === undefined) {
      set = new Set();
      this.#listeners.set(event, set);
    }
    set.add(listener as (detail: never) => void);
    return () => {
      set.delete(listener as (detail: never) => void);
    };
  }

  #settle(id: string): Pending | undefined {
    const entry = this.#pending.get(id);
    if (entry === undefined) return undefined;
    this.#pending.delete(id);
    if (entry.timer !== null) this.#clock.clearTimer(entry.timer);
    return entry;
  }

  #receive(data: unknown): void {
    if (typeof data !== 'string') return;
    let message: unknown;
    try {
      message = JSON.parse(data);
    } catch {
      return;
    }
    if (typeof message !== 'object' || message === null || Array.isArray(message)) return;
    const fields = message as Record<string, unknown>;
    if (typeof fields.id === 'string') {
      const entry = this.#settle(fields.id);
      if (entry === undefined) return;
      if (typeof fields.error === 'string') {
        entry.reject(new NativeError(fields.error === CANCELLED ? 'cancelled' : 'native', fields.error));
      } else if ('error' in fields && fields.error !== null) {
        entry.reject(new NativeError('protocol', 'the error is not a string'));
      } else {
        entry.resolve(fields.result);
      }
      return;
    }
    if (typeof fields.event !== 'string' || !isEvent(fields.event)) return;
    const event = fields.event;
    let detail: unknown;
    try {
      detail = EVENTS[event](fields.detail ?? {});
    } catch {
      // A malformed event is dropped: nothing may act on a half-understood message.
      return;
    }
    for (const listener of [...(this.#listeners.get(event) ?? [])]) (listener as (detail: unknown) => void)(detail);
  }
}

/** The bridge the shell injected into `win`, or null (any browser). */
export function rawBridgeOf(win: object): RawBridge | null {
  const candidate: unknown = Reflect.get(win, 'OmpDeckAndroid');
  if (typeof candidate !== 'object' || candidate === null) return null;
  return typeof Reflect.get(candidate, 'postMessage') === 'function' ? (candidate as RawBridge) : null;
}

let shared: NativeHost | null | undefined;

/** The page's one host: null in a browser. Attach listeners before calling {@link NativeHost.handshake}. */
export function nativeHost(): NativeHost | null {
  if (shared === undefined) {
    const raw = typeof window === 'undefined' ? null : rawBridgeOf(window);
    shared = raw === null ? null : new NativeHost(raw);
  }
  return shared;
}
