// The Web Push client (mobile-pwa.md "Web Push"): subscribes this browser with the gateway's VAPID
// key (`Welcome.push_key`) and registers the subscription with the desktop (`push_subscribe`).
//
// - Permission is only requested from a tap (`enable`); iOS needs the app on the Home Screen first.
// - At every start (each Welcome) an existing subscription is re-sent, and replaced when the
//   gateway's key changed. That is also how a subscription the service worker renewed
//   (`pushsubscriptionchange`) reaches the desktop: the worker cannot speak Noise, so it tells
//   open windows (`push_subscription_changed`) and the app re-syncs; a closed app re-syncs on its
//   next start.

import { useEffect, useMemo, useSyncExternalStore } from 'react';
import { api, fromBase64Url, toBase64Url } from '../remote/index.ts';
import type { Bytes } from '../remote/bytes.ts';
import { PUSH_CHANGED_MESSAGE, PUSH_KEY_URL, PUSH_STATE_CACHE } from '../push.ts';
import { hasS4 } from './client.ts';
import { useClient, useClientState } from './ui/hooks.ts';

export type PushSupport =
  | 'supported'
  /** No service worker, Push API or Notification API here. */
  | 'unsupported'
  /** iOS Safari outside the Home Screen app: push needs "Add to Home Screen" first. */
  | 'ios_home_screen';

export type PushPermission = NotificationPermission | 'unsupported';

/** The browser facilities push needs (a seam for tests). */
export interface PushEnv {
  support: PushSupport;
  permission(): PushPermission;
  /** Must be called from a user gesture. */
  requestPermission(): Promise<NotificationPermission>;
  /** The active service worker registration, or null when there is none. */
  registration(): Promise<ServiceWorkerRegistration | null>;
  /** Keeps the VAPID key for the service worker's `pushsubscriptionchange` resubscription. */
  rememberKey(key: Bytes): Promise<void>;
}

/** What push needs from the client. */
export interface PushClientLike {
  request(op: api.Op): Promise<api.Resp>;
  getState(): { welcome: api.Welcome | null };
}

export interface PushState {
  support: PushSupport;
  permission: PushPermission;
  /** This browser holds a push subscription (registered with the desktop by `enable`/sync). */
  subscribed: boolean;
  busy: boolean;
  error: string | null;
}

export interface PushControl extends PushState {
  /** From a tap: asks permission, subscribes, registers with the desktop. */
  enable(): Promise<void>;
  /** Unregisters at the desktop and drops this browser's subscription. */
  disable(): Promise<void>;
  setPrefs(prefs: api.PushPrefs): Promise<void>;
}

/** Whether push can work here, from the user agent, the APIs present and standalone mode. */
export function detectSupport(env: { userAgent: string; maxTouchPoints: number; standalone: boolean; hasServiceWorker: boolean; hasPushManager: boolean; hasNotification: boolean }): PushSupport {
  const ios = /iPhone|iPad|iPod/.test(env.userAgent) || (/Macintosh/.test(env.userAgent) && env.maxTouchPoints > 1);
  if (ios && !env.standalone) return 'ios_home_screen';
  return env.hasServiceWorker && env.hasPushManager && env.hasNotification ? 'supported' : 'unsupported';
}

/** The `push_subscribe` fields of a subscription (`p256dh`/`auth` base64url, unpadded). */
export function subscriptionOp(subscription: Pick<PushSubscription, 'endpoint' | 'getKey'>): Extract<api.Op, { op: 'push_subscribe' }> {
  const key = (name: PushEncryptionKeyName): string => {
    const buffer = subscription.getKey(name);
    if (buffer === null) throw new Error(`the push subscription has no ${name} key`);
    return toBase64Url(new Uint8Array(buffer));
  };
  return { op: 'push_subscribe', endpoint: subscription.endpoint, p256dh: key('p256dh'), auth: key('auth') };
}

/** Whether `subscription` was made with a different VAPID key than `pushKey`. */
export function keyChanged(subscription: Pick<PushSubscription, 'options'>, pushKey: string): boolean {
  const current = subscription.options.applicationServerKey;
  const wanted = fromBase64Url(pushKey);
  if (current === null || wanted === null) return true;
  const bytes = new Uint8Array(current);
  return bytes.length !== wanted.length || bytes.some((value, index) => value !== wanted[index]);
}

function pushKeyOf(welcome: api.Welcome | null): Bytes | null {
  if (!hasS4(welcome) || welcome?.push_key === undefined) return null;
  return fromBase64Url(welcome.push_key);
}

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** Holds the push state for `usePush` and runs its flows. */
export class PushController {
  readonly #client: PushClientLike;
  readonly #env: PushEnv;
  readonly #listeners = new Set<() => void>();
  #state: PushState;

  constructor(client: PushClientLike, env: PushEnv) {
    this.#client = client;
    this.#env = env;
    this.#state = { support: env.support, permission: env.permission(), subscribed: false, busy: false, error: null };
  }

  readonly subscribe = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  };

  readonly getState = (): PushState => this.#state;

  #set(patch: Partial<PushState>): void {
    this.#state = { ...this.#state, ...patch, permission: this.#env.permission() };
    for (const listener of this.#listeners) listener();
  }

  async #subscription(): Promise<PushSubscription | null> {
    const registration = await this.#env.registration();
    return registration === null ? null : registration.pushManager.getSubscription();
  }

  /**
   * At start (after each Welcome) and when the service worker renewed the subscription: re-sends
   * the existing subscription, resubscribing first when the gateway's key changed. Never prompts.
   */
  async sync(): Promise<void> {
    const key = pushKeyOf(this.#client.getState().welcome);
    if (this.#env.support !== 'supported' || key === null) return;
    try {
      const registration = await this.#env.registration();
      if (registration === null) return;
      let subscription = await registration.pushManager.getSubscription();
      this.#set({ subscribed: subscription !== null });
      if (subscription === null || this.#env.permission() !== 'granted') return;
      const pushKey = this.#client.getState().welcome?.push_key ?? '';
      if (keyChanged(subscription, pushKey)) {
        await subscription.unsubscribe();
        subscription = await registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key });
      }
      await this.#env.rememberKey(key);
      await this.#client.request(subscriptionOp(subscription));
      this.#set({ subscribed: true, error: null });
    } catch (error) {
      this.#set({ error: `Could not renew notifications: ${message(error)}` });
    }
  }

  async enable(): Promise<void> {
    if (this.#env.support === 'ios_home_screen') {
      this.#set({ error: 'On iPhone and iPad, add OmpDeck to the Home Screen (Share → Add to Home Screen) and open it from there to get notifications.' });
      return;
    }
    if (this.#env.support !== 'supported') {
      this.#set({ error: 'This browser cannot receive notifications.' });
      return;
    }
    // First, synchronously inside the tap: browsers only show the prompt for a user gesture.
    const asked = this.#env.requestPermission();
    this.#set({ busy: true, error: null });
    try {
      const permission = await asked;
      if (permission !== 'granted') {
        this.#set({ busy: false, error: permission === 'denied' ? 'Notifications are blocked for this site; allow them in the browser settings.' : 'Notifications were not allowed.' });
        return;
      }
      const welcome = this.#client.getState().welcome;
      const key = pushKeyOf(welcome);
      if (key === null) {
        this.#set({ busy: false, error: hasS4(welcome) ? 'The desktop has no push key yet.' : 'Notifications need a newer OmpDeck on the desktop.' });
        return;
      }
      const registration = await this.#env.registration();
      if (registration === null) throw new Error('the service worker is not active');
      let subscription = await registration.pushManager.getSubscription();
      if (subscription !== null && keyChanged(subscription, welcome?.push_key ?? '')) {
        await subscription.unsubscribe();
        subscription = null;
      }
      subscription ??= await registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key });
      await this.#env.rememberKey(key);
      await this.#client.request(subscriptionOp(subscription));
      this.#set({ busy: false, subscribed: true });
    } catch (error) {
      this.#set({ busy: false, error: `Could not turn on notifications: ${message(error)}` });
    }
  }

  async disable(): Promise<void> {
    this.#set({ busy: true, error: null });
    try {
      await this.#client.request({ op: 'push_unsubscribe' });
      const subscription = await this.#subscription();
      await subscription?.unsubscribe();
      this.#set({ busy: false, subscribed: false });
    } catch (error) {
      this.#set({ busy: false, error: `Could not turn off notifications: ${message(error)}` });
    }
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

/** The browser's push facilities. */
export function browserPushEnv(standalone: boolean): PushEnv {
  const hasNotification = typeof Notification !== 'undefined';
  return {
    support: detectSupport({
      userAgent: navigator.userAgent,
      maxTouchPoints: navigator.maxTouchPoints,
      standalone,
      hasServiceWorker: 'serviceWorker' in navigator,
      hasPushManager: typeof PushManager !== 'undefined',
      hasNotification,
    }),
    permission: () => (hasNotification ? Notification.permission : 'unsupported'),
    requestPermission: () => Notification.requestPermission(),
    registration: async () => ('serviceWorker' in navigator ? ((await navigator.serviceWorker.getRegistration()) ?? null) : null),
    rememberKey: async (key) => {
      if (typeof caches === 'undefined') return;
      await (await caches.open(PUSH_STATE_CACHE)).put(PUSH_KEY_URL, new Response(key));
    },
  };
}

/**
 * Drops this browser's push subscription and the kept VAPID key (forget/unpair). The key is
 * deleted even when unsubscribing fails; the failure is then rethrown for the caller to report.
 */
export async function dropBrowserPush(): Promise<void> {
  try {
    if ('serviceWorker' in navigator) {
      const registration = await navigator.serviceWorker.getRegistration();
      await (await registration?.pushManager.getSubscription())?.unsubscribe();
    }
  } finally {
    if (typeof caches !== 'undefined') await (await caches.open(PUSH_STATE_CACHE)).delete(PUSH_KEY_URL);
  }
}

const controllers = new WeakMap<object, PushController>();

function isStandalone(): boolean {
  return window.matchMedia('(display-mode: standalone)').matches || (navigator as Navigator & { standalone?: boolean }).standalone === true;
}

/** The app's one push controller (per client). */
export function pushController(client: PushClientLike): PushController {
  let controller = controllers.get(client);
  if (controller === undefined) {
    controller = new PushController(client, browserPushEnv(isStandalone()));
    controllers.set(client, controller);
  }
  return controller;
}

/** Push state and actions for the notification settings. */
export function usePush(): PushControl {
  const controller = pushController(useClient());
  const state = useSyncExternalStore(controller.subscribe, controller.getState);
  return useMemo(
    () => ({
      ...state,
      enable: () => controller.enable(),
      disable: () => controller.disable(),
      setPrefs: (prefs: api.PushPrefs) => controller.setPrefs(prefs),
    }),
    [state, controller],
  );
}

/**
 * Keeps the desktop's copy of this browser's subscription current: re-sends it after each Welcome
 * (with a new key when `push_key` changed) and when the service worker renewed it. Mount once.
 */
export function usePushSync(): void {
  const client = useClient();
  const welcome = useClientState((state) => state.welcome);
  const controller = pushController(client);
  useEffect(() => {
    if (welcome !== null) void controller.sync();
  }, [controller, welcome]);
  useEffect(() => {
    const container: ServiceWorkerContainer | undefined = navigator.serviceWorker;
    if (container === undefined) return;
    const onMessage = (event: MessageEvent<unknown>) => {
      const data = event.data;
      if (typeof data === 'object' && data !== null && 'type' in data && data.type === PUSH_CHANGED_MESSAGE) void controller.sync();
    };
    container.addEventListener('message', onMessage);
    return () => container.removeEventListener('message', onMessage);
  }, [controller]);
}
