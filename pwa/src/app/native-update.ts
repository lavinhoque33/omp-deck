// The Android app's own update (GitHub release APK, downloaded and verified by the shell), as the UI
// sees it. Distinct from the desktop gateway/omp updates (`update_check` ops) and from the browser's
// service-worker update (update.ts): it needs neither a desktop nor a connection, only the shell.
//
// The shell owns the check, download, verification and the system installer; this controller keeps
// the last `NativeUpdateState` it announced, follows the `update` event only while something looks
// at it (no listener or timer outlives its last subscriber), and turns a failed bridge call into a
// message instead of losing it. Installing is always a user action (`install`).

import { type NativeCalls, type NativeHost, type NativeUpdateState, nativeHost } from './native.ts';

type UpdateMethod = 'updateState' | 'updateCheck' | 'updateInstall' | 'updateConfigure';

/** What the controller reads of the shell (`NativeHost`; a seam for tests). */
export type NativeUpdateHost = Pick<NativeHost, 'request' | 'on'>;

export interface NativeUpdateView {
  /** The shell's last answer or event (null: not heard yet). */
  state: NativeUpdateState | null;
  /** A call the user started is in flight (its answer is immediate; progress arrives as events). */
  busy: boolean;
  /** The last bridge call that failed (the shell's own failures arrive as `state.error`). */
  failure: string | null;
}


export class NativeUpdateController {
  readonly #host: NativeUpdateHost;
  readonly #listeners = new Set<() => void>();
  #stop: (() => void) | null = null;
  /** Events heard so far: a reply that left before one arrived is older than it. */
  #events = 0;
  #view: NativeUpdateView = { state: null, busy: false, failure: null };

  constructor(host: NativeUpdateHost) {
    this.#host = host;
  }

  readonly subscribe = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    if (this.#stop === null) this.#attach();
    return () => {
      this.#listeners.delete(listener);
      if (this.#listeners.size === 0) this.#detach();
    };
  };

  readonly get = (): NativeUpdateView => this.#view;

  /** Asks for a fresh check; progress and the result arrive as events. */
  check(): Promise<void> {
    return this.#run('updateCheck', {}, true);
  }

  /** User action: downloads and verifies the release, then opens Android's installer. */
  install(): Promise<void> {
    return this.#run('updateInstall', {}, true);
  }

  setAutoCheck(autoCheck: boolean): Promise<void> {
    return this.#run('updateConfigure', { auto_check: autoCheck }, true);
  }

  /** Re-reads the shell's state (a call that fails leaves what is shown as it was). */
  refresh(): Promise<void> {
    return this.#run('updateState', {}, false);
  }

  #attach(): void {
    const offUpdate = this.#host.on('update', (detail) => {
      this.#events++;
      this.#set({ state: detail, failure: null });
    });
    // The Activity may have been stopped meanwhile: ask again when it is back.
    const offVisibility = this.#host.on('visibility', (detail) => {
      if (detail.visible) void this.refresh();
    });
    this.#stop = () => {
      offUpdate();
      offVisibility();
    };
    void this.refresh();
  }

  #detach(): void {
    this.#stop?.();
    this.#stop = null;
  }

  async #run<K extends UpdateMethod>(method: K, params: NativeCalls[K]['params'], user: boolean): Promise<void> {
    const sent = this.#events;
    if (user) this.#set({ busy: true, failure: null });
    try {
      const state = await this.#host.request(method, params);
      // An event heard while this was in flight is newer than the answer.
      if (this.#events === sent) this.#set({ state });
      if (user) this.#set({ busy: false });
    } catch (error) {
      // A background read only reports when there is nothing to show yet.
      if (user || this.#view.state === null) this.#set({ busy: false, failure: error instanceof Error ? error.message : String(error) });
    }
  }

  #set(patch: Partial<NativeUpdateView>): void {
    this.#view = { ...this.#view, ...patch };
    for (const listener of [...this.#listeners]) listener();
  }
}

/** Whether `latest` is a strictly newer `x.y.z` than `current` (anything else is not an update). */
export function isNewerVersion(latest: string | null, current: string): boolean {
  if (latest === null) return false;
  const parse = (version: string): number[] | null => {
    const match = /^v?(\d+)\.(\d+)\.(\d+)/.exec(version);
    return match === null ? null : [Number(match[1]), Number(match[2]), Number(match[3])];
  };
  const a = parse(latest);
  const b = parse(current);
  if (a === null || b === null) return false;
  for (let i = 0; i < 3; i++) {
    const delta = (a[i] ?? 0) - (b[i] ?? 0);
    if (delta !== 0) return delta > 0;
  }
  return false;
}

/** What the user can start: `update` from a found release, `install` of a verified download (or the installer again), `retry` after a failure. */
export type UpdateAction = 'update' | 'install' | 'retry';

export const ACTION_LABEL: Record<UpdateAction, string> = { update: 'Update', install: 'Install', retry: 'Retry' };

export interface UpdateOffer {
  /** The newer version on offer. */
  latest: string;
  /** What the user can do about it right now (null: the shell is working on it). */
  action: UpdateAction | null;
}

/**
 * What a state offers the user (null: nothing newer is known). A failed check or download keeps the
 * last release it found, so an error with a newer `latest` offers a retry of that release.
 */
export function updateOffer(state: NativeUpdateState | null): UpdateOffer | null {
  if (state === null || !isNewerVersion(state.latest, state.current)) return null;
  const latest = state.latest as string;
  switch (state.phase) {
    case 'available':
    case 'idle':
      return { latest, action: 'update' };
    case 'ready':
    case 'installing':
      return { latest, action: 'install' };
    case 'error':
      return { latest, action: 'retry' };
    case 'checking':
    case 'downloading':
      return { latest, action: null };
  }
}

/** Download progress as a 0..1 fraction (null: the size is not known yet). */
export function downloadFraction(state: Pick<NativeUpdateState, 'downloaded' | 'total'>): number | null {
  if (state.total === null || state.total <= 0) return null;
  return Math.min(1, Math.max(0, state.downloaded / state.total));
}

const controllers = new WeakMap<NativeUpdateHost, NativeUpdateController>();

/** The one controller of `host` (null: in a browser, where there is no native updater). */
export function nativeUpdates(host: NativeUpdateHost | null = nativeHost()): NativeUpdateController | null {
  if (host === null) return null;
  let controller = controllers.get(host);
  if (controller === undefined) {
    controller = new NativeUpdateController(host);
    controllers.set(host, controller);
  }
  return controller;
}
