// In-app navigation: a bottom-nav tab plus a stack of pushed screens. The browser back button
// pops the stack (each push adds a history entry).

import { createContext, useContext } from 'react';

export type Tab = 'sessions' | 'agents' | 'inbox' | 'settings';

export const TABS: readonly Tab[] = ['sessions', 'agents', 'inbox', 'settings'];

/** The settings sub-screens (slice 4). */
export type SettingsSection = 'notifications' | 'device' | 'usage' | 'omp' | 'updates' | 'backup' | 'sync';

export const SETTINGS_SECTIONS: readonly SettingsSection[] = [
  'notifications',
  'device',
  'usage',
  'omp',
  'updates',
  'backup',
  'sync',
];

export type Screen =
  /** A session's conversation; `liveKey` pins a live session that has no session id yet. */
  | { kind: 'conversation'; session: string | null; liveKey: string | null; title: string }
  /** A subagent's transcript inside `session` (read-only). */
  | { kind: 'subagent'; session: string; agent: string }
  | { kind: 'terminal'; liveKey: string; title: string }
  | { kind: 'changes'; project: string; display: string }
  | { kind: 'manager' }
  // slice 4
  /** A session's entry tree (fork, navigate). */
  | { kind: 'tree'; session: string; title: string }
  /** The desktop-folder browser (new session in a folder). */
  | { kind: 'browse' }
  /** A project's worktree and its parked branches. */
  | { kind: 'worktrees'; project: string; display: string }
  /** Running mux sessions with their memory. */
  | { kind: 'running' }
  | { kind: 'settings'; section: SettingsSection };

export interface Nav {
  tab: Tab;
  stack: readonly Screen[];
  setTab(tab: Tab): void;
  push(screen: Screen): void;
  /** Replaces the top screen (or pushes when none). */
  replace(screen: Screen): void;
  pop(): void;
}

export const NavContext = createContext<Nav | null>(null);

export function useNav(): Nav {
  const nav = useContext(NavContext);
  if (nav === null) throw new Error('useNav outside NavContext');
  return nav;
}

/** Reads a deep link (`#s=<session id>`, e.g. from a notification) from a URL hash. */
export function sessionFromHash(hash: string): string | null {
  const params = new URLSearchParams(hash.startsWith('#') ? hash.slice(1) : hash);
  const session = params.get('s');
  return session === null || session.length === 0 || session.length > 200 ? null : session;
}

/** Where the app opens: a tab and the screens pushed on it. */
export interface Route {
  tab: Tab;
  stack: Screen[];
}

/**
 * What each browser history entry carries: the whole route, so Back restores the screen that was
 * there. `base` marks the entry the app started on: Back from it leaves the app, so the app's own
 * navigation there rewrites the entry (`replaceState`) instead of pushing.
 */
export interface HistoryEntry {
  depth: number;
  tab: Tab;
  stack: Screen[];
  base?: true;
}

export function historyEntry(tab: Tab, stack: readonly Screen[], base = false): HistoryEntry {
  const entry: HistoryEntry = { depth: stack.length, tab, stack: [...stack] };
  if (base) entry.base = true;
  return entry;
}

/** The entries a deep link pushes: one per screen on its stack (or one for a bare tab), each with its prefix of the route. */
export function deepLinkEntries(route: Route): HistoryEntry[] {
  if (route.stack.length === 0) return [historyEntry(route.tab, [])];
  return route.stack.map((_, index) => historyEntry(route.tab, route.stack.slice(0, index + 1)));
}

/** The route a history entry's state holds, or null for an entry without one (the initial page). */
export function routeFromHistory(state: unknown): Route | null {
  if (typeof state !== 'object' || state === null || !('tab' in state) || !('stack' in state)) return null;
  const { tab, stack } = state;
  if (typeof tab !== 'string' || !TABS.includes(tab as Tab) || !Array.isArray(stack)) return null;
  return { tab: tab as Tab, stack: stack as Screen[] };
}

/** Whether the entry is the app's starting entry (marked, or never written by the app). */
export function isBaseEntry(state: unknown): boolean {
  if (routeFromHistory(state) === null) return true;
  return typeof state === 'object' && state !== null && 'base' in state && state.base === true;
}

/**
 * Where the app starts: an incoming link wins; otherwise the route the current history entry holds
 * (a reload keeps `history.state`, while the link's hash is long stripped); otherwise the sessions tab.
 */
export function startRoute(incoming: Route | null, state: unknown): Route {
  return incoming ?? routeFromHistory(state) ?? { tab: 'sessions', stack: [] };
}

/**
 * The state the current entry should hold after the app's own navigation changed the route it
 * shows: the entry is kept in step so Back/reload restores what it showed last, not what it
 * started with. Its base flag is preserved.
 */
export function syncedEntry(state: unknown, tab: Tab, stack: readonly Screen[]): HistoryEntry {
  return historyEntry(tab, stack, isBaseEntry(state));
}

/**
 * Reads a route link from a URL hash: `#route=<tab or screen>` plus the screen's parameters
 * (`session`, `agent`, `live`, `project`, `section`), or the notification deep
 * link `#s=<session id>` (the conversation). Null when the hash names no route or misses a
 * required parameter. A link never names a screen's title or folder: a link is attacker-writable, so
 * screens look those up from the gateway's feeds by id and the route carries only a placeholder.
 */
export function routeFromHash(hash: string): Route | null {
  const params = new URLSearchParams(hash.startsWith('#') ? hash.slice(1) : hash);
  const route = params.get('route');
  if (route === null) {
    const session = sessionFromHash(hash);
    return session === null
      ? null
      : { tab: 'sessions', stack: [{ kind: 'conversation', session, liveKey: null, title: 'Session' }] };
  }
  const param = (name: string): string | null => {
    const value = params.get(name);
    return value === null || value.length === 0 || value.length > 500 ? null : value;
  };
  const session = param('session') ?? param('s');
  const project = param('project');
  const on = (tab: Tab, screen: Screen): Route => ({ tab, stack: [screen] });
  if ((TABS as readonly string[]).includes(route)) {
    const tab = route as Tab;
    const section = param('section');
    if (tab === 'settings' && section !== null) {
      return (SETTINGS_SECTIONS as readonly string[]).includes(section)
        ? on('settings', { kind: 'settings', section: section as SettingsSection })
        : null;
    }
    return { tab, stack: [] };
  }
  switch (route) {
    case 'conversation': {
      const live = param('live');
      if (session === null && live === null) return null;
      return on('sessions', { kind: 'conversation', session, liveKey: live, title: 'Session' });
    }
    case 'subagent': {
      const agent = param('agent');
      return session === null || agent === null ? null : on('agents', { kind: 'subagent', session, agent });
    }
    case 'terminal': {
      const live = param('live');
      return live === null ? null : on('sessions', { kind: 'terminal', liveKey: live, title: 'Terminal' });
    }
    case 'changes':
      return project === null ? null : on('sessions', { kind: 'changes', project, display: '' });
    case 'manager':
      return on('sessions', { kind: 'manager' });
    case 'tree':
      return session === null ? null : on('sessions', { kind: 'tree', session, title: 'Session' });
    case 'browse':
      return on('sessions', { kind: 'browse' });
    case 'worktrees':
      return project === null ? null : on('sessions', { kind: 'worktrees', project, display: '' });
    case 'running':
      return on('sessions', { kind: 'running' });
    default:
      return null;
  }
}

/** Reads the pairing invite from a URL hash (`#pair=v1.…`). */
export function inviteFromHash(hash: string): string | null {
  const match = /(?:^#|&)pair=([^&]+)/.exec(hash);
  return match?.[1] ?? null;
}

/** Extracts an invite from pasted text: a whole pairing link or the bare `v1.…` invite. */
export function inviteFromText(text: string): string | null {
  const trimmed = text.trim();
  const fromLink = /[#&]pair=([A-Za-z0-9._-]+)/.exec(trimmed);
  if (fromLink?.[1] !== undefined) return fromLink[1];
  return /^v1\.[A-Za-z0-9_-]+$/.test(trimmed) ? trimmed : null;
}
