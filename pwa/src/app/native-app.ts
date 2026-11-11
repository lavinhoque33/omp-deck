// What the Android shell changes about the app's own behavior, apart from push (native-push.ts):
// where notification taps and Back go, what "visible" means, and what the shell is told about the
// paired desktops. Each piece takes its collaborators as parameters, so the tests drive them with
// fakes and no WebView.

import { type Link, linkFromHash } from './nav.ts';
import type { NativeHost } from './native.ts';

// ---- notification taps ------------------------------------------------------------------------

/** The link a shell `open` url names (`/#s=<session>&d=<desktop>`), or null when it names nothing. */
export function linkFromOpenUrl(url: string, base: string): Link | null {
  let parsed: URL;
  try {
    parsed = new URL(url, base);
  } catch {
    return null;
  }
  return linkFromHash(parsed.hash);
}

/**
 * Holds the shell's `open` events until the app can act on them: the desktops are read (a link for
 * a desktop that is not paired yet means nothing) and the app has registered its handler. Only the
 * newest waiting link is kept: a later tap replaces an earlier one that nobody has acted on.
 */
export class OpenRouter {
  readonly #base: string;
  #handler: ((link: Link) => void) | null = null;
  #ready = false;
  #waiting: Link | null = null;

  constructor(base: string) {
    this.#base = base;
  }

  /** A shell `open` event. */
  open(url: string): void {
    const link = linkFromOpenUrl(url, this.#base);
    if (link === null) return;
    this.#waiting = link;
    this.#flush();
  }

  /** The app's handler (null: it went away). */
  attach(handler: ((link: Link) => void) | null): void {
    this.#handler = handler;
    this.#flush();
  }

  /** The desktops are loaded. */
  ready(): void {
    this.#ready = true;
    this.#flush();
  }

  #flush(): void {
    const link = this.#waiting;
    if (!this.#ready || this.#handler === null || link === null) return;
    this.#waiting = null;
    this.#handler(link);
  }
}

// ---- Back -------------------------------------------------------------------------------------

/** The shell's Back: the app's own back first (it answers whether it had somewhere to go), the Activity closes only when it did not. */
export function pressBack(handler: (() => boolean) | null, finish: () => void): void {
  if (handler?.() !== true) finish();
}

// ---- visibility -------------------------------------------------------------------------------

/**
 * Whether the app counts as visible. In the shell the document can stay "visible" while the Activity
 * is stopped, so once the shell has reported, both must agree. One source feeds every client, so a
 * desktop switch while the app is in the background starts the new client hidden, never visible.
 */
export class PresenceSource {
  readonly #listeners = new Set<() => void>();
  #document: boolean;
  #native: boolean | null = null;
  #value: boolean;

  constructor(documentVisible: boolean) {
    this.#document = documentVisible;
    this.#value = documentVisible;
  }

  readonly subscribe = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  };

  get(): boolean {
    return this.#value;
  }

  setDocument(visible: boolean): void {
    this.#document = visible;
    this.#update();
  }

  setNative(visible: boolean): void {
    this.#native = visible;
    this.#update();
  }

  #update(): void {
    const value = this.#document && this.#native !== false;
    if (value === this.#value) return;
    this.#value = value;
    for (const listener of [...this.#listeners]) listener();
  }
}

// ---- what the shell is told -------------------------------------------------------------------

/** The part of a `Client` the shell's presence follows. */
export interface PresenceClient {
  readonly online: boolean;
  subscribeState(listener: () => void): () => void;
}

/** The part of `Desktops` the shell's presence and labels follow. */
export interface WatchedDesktops {
  getState(): {
    /** The pairings are read (before that the list is empty, not "none paired"). */
    loaded: boolean;
    activeId: string | null;
    client: PresenceClient | null;
    desktops: readonly { id: string; gateway: string }[];
  };
  subscribe(listener: () => void): () => void;
}

type Sender = Pick<NativeHost, 'request'>;

/**
 * Tells the shell which desktop is on screen and whether the phone is really looking at it
 * (`setPresence`), so the shell mutes only that desktop's notifications: the app is in the
 * foreground (the Activity's own visibility, via {@link PresenceSource}) AND connected to the
 * desktop, as the gateway counts it. A desktop that is not active, or an offline view, never mutes.
 * A report that failed is repeated at the next change.
 */
export function watchNativePresence(host: Sender, desktops: WatchedDesktops, source: PresenceSource): () => void {
  let last: string | null = null;
  let watched: PresenceClient | null = null;
  let unwatchClient: (() => void) | null = null;
  const report = () => {
    const { activeId, client } = desktops.getState();
    if (client !== watched) {
      unwatchClient?.();
      watched = client;
      unwatchClient = client?.subscribeState(report) ?? null;
    }
    const visible = activeId !== null && client !== null && client.online && source.get();
    const key = `${activeId ?? ''}|${visible}`;
    if (key === last) return;
    last = key;
    host.request('setPresence', { desktop: activeId, visible }).catch(() => {
      if (last === key) last = null;
    });
  };
  const unwatchDesktops = desktops.subscribe(report);
  const unwatchSource = source.subscribe(report);
  report();
  return () => {
    unwatchDesktops();
    unwatchSource();
    unwatchClient?.();
  };
}

/** Every paired desktop's id → the label the switcher shows (the shell labels notifications only while 2+ are paired). */
export function nativeLabels(desktops: readonly { id: string; gateway: string }[]): Record<string, string> {
  return Object.fromEntries(desktops.map((desktop) => [desktop.id, desktop.gateway]));
}

/**
 * Keeps the shell's desktop list in step with the pairings (added, replaced, forgotten, renamed).
 * A report that failed is repeated at the next change.
 */
export function watchNativeLabels(host: Sender, desktops: WatchedDesktops): () => void {
  let last: string | null = null;
  const report = () => {
    const state = desktops.getState();
    if (!state.loaded) return;
    const labels = nativeLabels(state.desktops);
    const key = JSON.stringify(labels);
    if (key === last) return;
    last = key;
    host.request('pushLabels', { labels }).catch(() => {
      if (last === key) last = null;
    });
  };
  report();
  return desktops.subscribe(report);
}
