// Native Android push (FCM) for the app inside the APK, behind the same settings as Web Push.
//
// The shell owns Firebase, the notification permission, the FCM token and one random AES key per
// desktop (Android Keystore); this controller owns the rest of the flow: whether push is on for a
// desktop, and the encrypted `push_android_subscribe` registration the gateway keeps beside Web
// Push (prefs, unsubscribe and the device feed are shared). The shell cannot say which desktops
// have push on, so that intent is kept here, per desktop (`PushMarkers`).
//
// - Requests are gated on `Welcome.features` listing `android_push`: only a desktop with a
//   configured FCM sender lists it; any other gets the setup instructions, never a request.
// - A registration is re-sent after each Welcome and when the shell reports a changed token,
//   configuration or permission (`sync`, never prompting). Prefs are never touched by it.
// - Turning push off (or forgetting the desktop) drops that desktop's gateway subscription and
//   exactly that desktop's key in the shell; other desktops are never touched.

import type { api } from '../remote/index.ts';
import type { PushClientLike, PushControllerLike, PushState } from './push-client.ts';
import { feedKey, hasS4 } from './client.ts';
import type { NativeHost, NativePushState } from './native.ts';
import { FEATURE_ANDROID_PUSH } from '../remote/api.ts';

export const DESKTOP_SENDER_HELP =
  'This desktop cannot send Android notifications yet: Firebase Cloud Messaging is not set up there. On the desktop, put your Firebase project’s service-account JSON in OmpDeck’s secrets folder as fcm-service-account.json (or point OMP_DECK_FCM_CREDENTIALS at it), restart OmpDeck, then turn push on again.';
export const DESKTOP_CONFIG_HELP =
  'This desktop has no Android app configuration to give this phone. On the desktop, put your Firebase project’s google-services.json (Android app io.github.lavinhoque33.ompdeck) in OmpDeck’s secrets folder (or point OMP_DECK_FCM_ANDROID_CONFIG at it), then restart the OmpDeck gateway (update OmpDeck on the desktop if it is older) and turn push on again.';

/** How long a tap waits for the desktop's device feed when it has not arrived yet. */
const FEED_WAIT_MS = 10_000;

/** What the native controller needs of the app's client: requests, the welcome, and the device feed. */
export interface NativePushClient extends PushClientLike {
  getState(): { welcome: api.Welcome | null; feeds: ReadonlyMap<string, api.FeedValue> };
  follow(feed: api.Feed): (() => void) | null;
  subscribeState(listener: () => void): () => void;
}

/** What the native controller reads of the shell (`NativeHost.request`; a seam for tests). */
export type NativePushHost = Pick<NativeHost, 'request'>;

/** Per desktop: whether the user turned push on for it. */
export interface PushMarkers {
  has(desktop: string): boolean;
  set(desktop: string): void;
  clear(desktop: string): void;
}

const MARKER_PREFIX = 'ompdeck.android-push.';

/** Markers in `storage` (localStorage); in memory for the page's life when there is none. */
export function storageMarkers(storage: Pick<Storage, 'getItem' | 'setItem' | 'removeItem'> | null): PushMarkers {
  const memory = new Set<string>();
  return {
    has: (desktop) => {
      try {
        return storage !== null ? storage.getItem(MARKER_PREFIX + desktop) === '1' : memory.has(desktop);
      } catch {
        return memory.has(desktop);
      }
    },
    set: (desktop) => {
      memory.add(desktop);
      try {
        storage?.setItem(MARKER_PREFIX + desktop, '1');
      } catch {
        // Kept in memory: push stays on for this run.
      }
    },
    clear: (desktop) => {
      memory.delete(desktop);
      try {
        storage?.removeItem(MARKER_PREFIX + desktop);
      } catch {
        // Nothing stored that could be cleared.
      }
    },
  };
}

/** The desktop the controller works for. */
export interface NativePushDesktop {
  id: string;
  /** The label the switcher shows. */
  label(): string;
}

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** Whether `welcome` is from a desktop whose FCM sender is configured. */
export function advertisesAndroidPush(welcome: api.Welcome | null): boolean {
  return hasS4(welcome) && welcome?.features.includes(FEATURE_ANDROID_PUSH) === true;
}

/** Native push for one desktop (see the module comment). */
export class NativePushController implements PushControllerLike {
  readonly #client: NativePushClient;
  readonly #feedWaitMs: number;
  readonly #host: NativePushHost;
  readonly #desktop: NativePushDesktop;
  readonly #markers: PushMarkers;
  readonly #listeners = new Set<() => void>();
  /** The shell's last answer (null: not asked yet). */
  #native: NativePushState | null = null;
  /** Flows run one at a time, in order: a sync never interleaves with a tap. */
  #chain: Promise<unknown> = Promise.resolve();
  #state: PushState;

  constructor(client: NativePushClient, host: NativePushHost, desktop: NativePushDesktop, markers: PushMarkers, feedWaitMs: number = FEED_WAIT_MS) {
    this.#feedWaitMs = feedWaitMs;
    this.#client = client;
    this.#host = host;
    this.#desktop = desktop;
    this.#markers = markers;
    this.#state = {
      support: 'supported',
      permission: 'default',
      subscribed: markers.has(desktop.id),
      busy: false,
      error: null,
      native: { phoneConfigured: true, hasToken: true, desktopSender: advertisesAndroidPush(client.getState().welcome) },
    };
  }

  readonly subscribe = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  };

  readonly getState = (): PushState => this.#state;

  #set(patch: Partial<PushState>): void {
    const native = this.#native;
    this.#state = {
      ...this.#state,
      ...patch,
      permission: native?.permission ?? 'default',
      native: {
        phoneConfigured: native?.configured ?? true,
        hasToken: native === null || native.token !== null,
        desktopSender: advertisesAndroidPush(this.#client.getState().welcome),
      },
    };
    for (const listener of this.#listeners) listener();
  }

  #run<T>(job: () => Promise<T>): Promise<T> {
    const next = this.#chain.then(job, job);
    this.#chain = next.catch(() => undefined);
    return next;
  }

  /** Whether the shell can register now: permission granted, Firebase configured, a token issued. */
  static ready(state: NativePushState): boolean {
    return state.permission === 'granted' && state.configured && state.token !== null;
  }

  async #read(): Promise<NativePushState> {
    const state = await this.#host.request('pushState', {});
    this.#native = state;
    return state;
  }

  /** Prepares this desktop's key with the shell and registers it, with the current token, at the gateway. */
  async #register(): Promise<void> {
    const id = this.#desktop.id;
    const subscription = await this.#host.request('pushSubscription', { desktop: id, label: this.#desktop.label() });
    if (subscription.desktop !== id) throw new Error('Android prepared the notification key for another desktop');
    await this.#client.request({ op: 'push_android_subscribe', token: subscription.token, key: subscription.key, desktop: subscription.desktop });
  }

  /**
   * After each Welcome, when the shell reports a change, and when this desktop becomes active:
   * reads the shell's state and, when push is on for this desktop and possible, re-sends the
   * registration (the gateway keeps its prefs). Never prompts, never turns push on by itself.
   */
  sync(): Promise<void> {
    return this.#run(async () => {
      const welcome = this.#client.getState().welcome;
      if (!hasS4(welcome)) return;
      try {
        const state = await this.#read();
        const on = this.#markers.has(this.#desktop.id);
        if (!on) {
          this.#set({ subscribed: false });
          return;
        }
        // Push stays on (the user's choice) while the shell or the desktop cannot do it yet.
        if (!advertisesAndroidPush(welcome) || !NativePushController.ready(state)) {
          this.#set({ subscribed: false });
          return;
        }
        await this.#register();
        this.#set({ subscribed: true, error: null });
      } catch (error) {
        this.#set({ error: `Could not renew notifications: ${message(error)}` });
      }
    });
  }

  /**
   * The desktop's public google-services.json from its device feed. The feed may still be loading
   * (a fresh connection): it is followed for the wait and released. `null`: the desktop has none.
   */
  async #desktopConfig(): Promise<string | null> {
    const read = (): api.PushStateView | null => {
      const value = this.#client.getState().feeds.get(feedKey({ feed: 'device' }));
      return value?.feed === 'device' ? value.push : null;
    };
    let push = read();
    if (push === null) {
      const release = this.#client.follow({ feed: 'device' });
      if (release === null) throw new Error('the app cannot follow another feed to read the desktop’s setup');
      try {
        const { promise, resolve, reject } = Promise.withResolvers<api.PushStateView>();
        const timer = setTimeout(() => reject(new Error('the desktop has not sent its notification setup yet; check the connection and try again')), this.#feedWaitMs);
        const stop = this.#client.subscribeState(() => {
          const now = read();
          if (now !== null) resolve(now);
        });
        const available = read();
        if (available !== null) resolve(available);
        try {
          push = await promise;
        } finally {
          clearTimeout(timer);
          stop();
        }
      } finally {
        release();
      }
    }
    return push.android_config ?? null;
  }

  enable(): Promise<void> {
    // Queued behind any running sync: flows never interleave.
    return this.#run(async () => {
      const welcome = this.#client.getState().welcome;
      if (!hasS4(welcome)) {
        this.#set({ error: 'Notifications need a newer OmpDeck on the desktop.' });
        return;
      }
      if (!advertisesAndroidPush(welcome)) {
        this.#set({ error: DESKTOP_SENDER_HELP });
        return;
      }
      this.#set({ busy: true, error: null });
      try {
        // A phone with Firebase already set up is never given (or reset to) a config.
        const current = await this.#read();
        let params: { config?: string } = {};
        if (!current.configured) {
          const config = await this.#desktopConfig();
          if (config === null) {
            this.#set({ busy: false, error: DESKTOP_CONFIG_HELP });
            return;
          }
          params = { config };
        }
        // The notification permission prompt runs inside this call.
        const state = await this.#host.request('pushEnable', params);
        this.#native = state;
        if (state.permission !== 'granted') {
          this.#set({
            busy: false,
            error: state.permission === 'denied' ? 'Notifications are blocked for OmpDeck; allow them in Android’s app settings.' : 'Notifications were not allowed.',
          });
          return;
        }
        if (!state.configured) {
          this.#set({ busy: false, error: 'Android did not accept the desktop’s Firebase setup. Update OmpDeck on the desktop and try again.' });
          return;
        }
        if (state.token === null) {
          this.#set({ busy: false, error: 'Firebase has not issued this phone a notification token yet; try again in a moment.' });
          return;
        }
        await this.#register();
        this.#markers.set(this.#desktop.id);
        this.#set({ busy: false, subscribed: true });
      } catch (error) {
        this.#set({ busy: false, error: `Could not turn on notifications: ${message(error)}` });
      }
    });
  }

  disable(): Promise<void> {
    return this.#run(async () => {
      const id = this.#desktop.id;
      this.#set({ busy: true, error: null });
      try {
        await this.#client.request({ op: 'push_unsubscribe' });
      } catch (error) {
        this.#set({ busy: false, error: `Could not turn off notifications: ${message(error)}` });
        return;
      }
      // The desktop no longer pushes: the intent goes first, so a failing key removal can never be re-registered by a later sync.
      this.#markers.clear(id);
      this.#set({ subscribed: false });
      try {
        await this.#host.request('pushForget', { desktop: id });
        this.#set({ busy: false });
      } catch (error) {
        this.#set({ busy: false, error: `Notifications are off, but Android kept this desktop’s key: ${message(error)}` });
      }
    });
  }

  async setPrefs(prefs: api.PushPrefs): Promise<void> {
    this.#set({ busy: true, error: null });
    try {
      await this.#client.request({ op: 'push_prefs', prefs });
      this.#set({ busy: false });
    } catch (error) {
      this.#set({ busy: false, error: `Could not save the notification settings: ${message(error)}` });
    }
  }
}

/**
 * Drops this phone's native push state for desktop `id` (forget/unpair): the marker, and exactly
 * that desktop's key and registration in the shell. The gateway side is `Client.dropPush`'s
 * `push_unsubscribe`. Other desktops are never touched; a failure is rethrown after both steps ran.
 */
export async function dropNativePush(host: NativePushHost, markers: PushMarkers, id: string): Promise<void> {
  let failure: unknown = null;
  try {
    markers.clear(id);
  } catch (error) {
    failure = error;
  }
  try {
    await host.request('pushForget', { desktop: id });
  } catch (error) {
    failure ??= error;
  }
  if (failure !== null) throw failure;
}

let markers: PushMarkers | undefined;

/** The page's markers (localStorage), shared by the controller and the unpair path. */
export function appMarkers(): PushMarkers {
  if (markers === undefined) {
    let storage: Storage | null = null;
    try {
      storage = window.localStorage;
    } catch {
      storage = null;
    }
    markers = storageMarkers(storage);
  }
  return markers;
}
