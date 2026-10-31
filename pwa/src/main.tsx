// Boot: theme, deep links (`#pair=`, `#s=`), the key store and client, the service worker, and
// wake-ups on network/visibility changes.

import { StrictMode, useCallback, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './app/App.tsx';
import { Client, browserClock } from './app/client.ts';
import { KeyStore } from './app/keystore.ts';
import { dropBrowserPush } from './app/push-client.ts';
import { dialRelay, pairWithInvite } from './app/link.ts';
import { inviteFromHash, type Route, routeFromHash } from './app/nav.ts';
import { type ThemeId, applyTheme, loadTheme, saveTheme } from './app/theme.ts';
import { ClientContext } from './app/ui/hooks.ts';
import { registerServiceWorker } from './app/update.ts';
import './styles/app.css';
import './styles/themes.css';

function storage(): Storage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function platform(): string {
  const ua = navigator.userAgent;
  if (/iPhone|iPad|iPod/.test(ua)) return 'iOS';
  if (/Android/.test(ua)) return 'Android';
  if (/Mac OS X/.test(ua)) return 'macOS';
  if (/Windows/.test(ua)) return 'Windows';
  if (/Linux/.test(ua)) return 'Linux';
  return 'Web';
}

function isStandalone(): boolean {
  return (
    window.matchMedia('(display-mode: standalone)').matches ||
    (navigator as Navigator & { standalone?: boolean }).standalone === true
  );
}

// The invite and deep link are read once and removed from the URL at once (the invite is a
// secret; it must not stay in history or be bookmarked).
const hash = location.hash;
const initialInvite = inviteFromHash(hash);
const initialRoute = routeFromHash(hash);
if (hash.length > 0) history.replaceState(null, '', `${location.pathname}${location.search}`);

const theme = loadTheme(storage());
applyTheme(document, theme);

const rootElement = document.getElementById('root');
if (rootElement === null) throw new Error('index.html lacks #root');
const root = createRoot(rootElement);

let deepLink: ((route: Route) => void) | null = null;
const openDeepLink = (url: string) => {
  const route = routeFromHash(new URL(url, location.href).hash);
  if (route !== null) deepLink?.(route);
};
// A route link opened while the app runs (e.g. pasted, or a notification on platforms that
// navigate the existing window) opens its screen too.
let inviteLink: ((invite: string) => void) | null = null;
window.addEventListener('hashchange', () => {
  const invite = inviteFromHash(location.hash);
  if (invite !== null) {
    // The invite is a secret: off the URL at once, then on to the pairing flow (which asks before it replaces a pairing).
    history.replaceState(null, '', `${location.pathname}${location.search}`);
    inviteLink?.(invite);
    return;
  }
  const route = routeFromHash(location.hash);
  if (route === null) return;
  history.replaceState(null, '', `${location.pathname}${location.search}`);
  deepLink?.(route);
});

function Root({ client }: { client: Client }) {
  const [current, setCurrent] = useState<ThemeId>(theme);
  const onTheme = useCallback((next: ThemeId) => {
    setCurrent(next);
    applyTheme(document, next);
    saveTheme(storage(), next);
  }, []);
  const onDeepLink = useCallback((open: (route: Route) => void) => {
    deepLink = open;
  }, []);
  const onInviteLink = useCallback((open: (invite: string) => void) => {
    inviteLink = open;
  }, []);
  return (
    <ClientContext.Provider value={client}>
      <App
        initialInvite={initialInvite}
        initialRoute={initialRoute}
        theme={current}
        onTheme={onTheme}
        standalone={isStandalone()}
        onDeepLink={onDeepLink}
        onInviteLink={onInviteLink}
      />
    </ClientContext.Provider>
  );
}

function Fatal({ message }: { message: string }) {
  return (
    <div className="screen">
      <div className="body">
        <div className="pair-hero">
          <div className="mark">!</div>
          <h2>OmpDeck cannot start here</h2>
          <p className="muted">{message}</p>
        </div>
      </div>
    </div>
  );
}

async function boot(): Promise<void> {
  let store: KeyStore;
  try {
    store = await KeyStore.open();
  } catch (error) {
    root.render(
      <Fatal message={`This browser cannot keep the pairing key (IndexedDB: ${error instanceof Error ? error.message : String(error)}). Leave private browsing or use another browser.`} />,
    );
    return;
  }
  const client = new Client({
    store,
    dial: (target, events) => dialRelay(target, events),
    pair: (invite, keypair, hello, onStage) => pairWithInvite(invite, keypair, hello, onStage),
    clock: browserClock,
    random: Math.random,
    platform: platform(),
    persistStorage: () => navigator.storage?.persist?.() ?? Promise.resolve(false),
    dropPush: dropBrowserPush,
  });
  root.render(
    <StrictMode>
      <Root client={client} />
    </StrictMode>,
  );
  registerServiceWorker({ navigate: openDeepLink });
  window.addEventListener('online', () => client.wake());
  // The desktop skips push notifications while this phone is visible and connected.
  client.setVisible(document.visibilityState === 'visible');
  document.addEventListener('visibilitychange', () => {
    const visible = document.visibilityState === 'visible';
    client.setVisible(visible);
    if (visible) client.wake();
  });
  await client.start();
}

void boot();
