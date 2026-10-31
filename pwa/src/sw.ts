/// <reference lib="webworker" />
// The service worker: precaches the hashed app shell (cache-first, navigations fall back to the
// cached index.html), never caches `/v1/*`, shows push notifications, and routes notification
// taps. Updates are user-confirmed: the app posts SKIP_WAITING.

import { GENERIC_PUSH, PUSH_CHANGED_MESSAGE, PUSH_KEY_URL, PUSH_STATE_CACHE, parsePush, safeNavigate } from './push.ts';

declare const self: ServiceWorkerGlobalScope;

/** Injected by build.ts: the app shell's URLs, and a version that changes with them. */
declare const __PRECACHE__: string[];
declare const __VERSION__: string;

const CACHE = `ompdeck-${__VERSION__}`;
const SHELL = '/index.html';

self.addEventListener('install', (event) => {
  event.waitUntil(
    (async () => {
      const cache = await caches.open(CACHE);
      await cache.addAll(__PRECACHE__);
    })(),
  );
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    (async () => {
      for (const name of await caches.keys()) {
        if (name.startsWith('ompdeck-') && name !== CACHE) await caches.delete(name);
      }
      await self.clients.claim();
    })(),
  );
});

self.addEventListener('message', (event) => {
  const data: unknown = event.data;
  if (typeof data === 'object' && data !== null && 'type' in data && data.type === 'SKIP_WAITING') {
    void self.skipWaiting();
  }
});

self.addEventListener('fetch', (event) => {
  const request = event.request;
  if (request.method !== 'GET') return;
  const url = new URL(request.url);
  // Only this origin's app files: never the relay endpoints or anything cross-origin.
  if (url.origin !== self.location.origin || url.pathname.startsWith('/v1/') || url.pathname === '/healthz') return;
  if (request.mode === 'navigate') {
    event.respondWith(
      (async () => {
        try {
          return await fetch(request);
        } catch {
          const cached = await caches.match(SHELL, { cacheName: CACHE });
          return cached ?? Response.error();
        }
      })(),
    );
    return;
  }
  event.respondWith(
    (async () => {
      const cached = await caches.match(request, { cacheName: CACHE });
      return cached ?? fetch(request);
    })(),
  );
});

self.addEventListener('push', (event) => {
  // Every push shows a notification (browsers penalise silent pushes); bad JSON gets a generic one.
  const payload = event.data === null ? null : parsePush(event.data.text(), self.location.origin);
  const shown = payload ?? GENERIC_PUSH;
  const options: NotificationOptions = {
    body: shown.body,
    icon: '/icon-192.png',
    badge: '/icon-192.png',
    data: { navigate: shown.navigate },
    silent: shown.silent,
  };
  if (shown.tag !== null) options.tag = shown.tag;
  event.waitUntil(self.registration.showNotification(shown.title, options));
});

self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const data: unknown = event.notification.data;
  const navigate = typeof data === 'object' && data !== null && 'navigate' in data ? data.navigate : '/';
  const target = new URL(safeNavigate(navigate, self.location.origin), self.location.origin).href;
  event.waitUntil(
    (async () => {
      const windows = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
      const open = windows.find((client) => new URL(client.url).origin === self.location.origin);
      if (open !== undefined) {
        await open.focus();
        open.postMessage({ type: 'navigate', url: target });
        return;
      }
      await self.clients.openWindow(target);
    })(),
  );
});

self.addEventListener('pushsubscriptionchange', (event) => {
  // Keep receiving: subscribe again with the VAPID key the app stored when it subscribed (the
  // old subscription's own key when the browser still has it). The worker cannot speak Noise, so
  // the desktop learns the new endpoint from the app: open windows are told at once, and a closed
  // app re-sends its subscription (`push_subscribe`) at its next start.
  const change = event as Event & { oldSubscription?: PushSubscription | null; waitUntil(promise: Promise<unknown>): void };
  change.waitUntil(
    (async () => {
      const stored = await (await caches.open(PUSH_STATE_CACHE)).match(PUSH_KEY_URL);
      const key = change.oldSubscription?.options.applicationServerKey ?? (stored === undefined ? null : await stored.arrayBuffer());
      if (key === null) return;
      await self.registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key });
      for (const client of await self.clients.matchAll({ type: 'window', includeUncontrolled: true })) {
        client.postMessage({ type: PUSH_CHANGED_MESSAGE });
      }
    })(),
  );
});
