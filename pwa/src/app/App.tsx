// The app shell: pairing gate, bottom-nav tabs, pushed screens (with browser-back support), the
// always-on `live` and `inbox` feeds, and toasts.

import { type ReactNode, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { INITIAL_TRACKER, observePairing } from './pairing-gate.ts';
import {
  type Nav,
  NavContext,
  type Route,
  type Screen,
  type Tab,
  deepLinkEntries,
  historyEntry,
  isBaseEntry,
  routeFromHistory,
  startRoute,
  syncedEntry,
} from './nav.ts';
import { AgentsScreen } from './screens/Agents.tsx';
import { ChangesScreen } from './screens/Changes.tsx';
import { ConversationScreen } from './screens/Conversation.tsx';
import { InboxScreen } from './screens/Inbox.tsx';
import { ManagerScreen } from './screens/Manager.tsx';
import { PairingScreen } from './screens/Pairing.tsx';
import { SessionsScreen } from './screens/Sessions.tsx';
import { SettingsScreen } from './screens/Settings.tsx';
import { SubagentScreen } from './screens/Subagent.tsx';
import { TerminalScreen } from './screens/Terminal.tsx';
import { BrowseScreen } from './screens/fleet/FolderBrowser.tsx';
import { RunningScreen } from './screens/fleet/Running.tsx';
import { TreeScreen } from './screens/fleet/Tree.tsx';
import { WorktreesScreen } from './screens/fleet/Worktrees.tsx';
import { SettingsSectionScreen } from './screens/settings/index.tsx';
import type { ThemeId } from './theme.ts';
import { useClientState, useFeed } from './ui/hooks.ts';
import { BottomNav, Toasts } from './ui/kit.tsx';
import { usePushSync } from './push-client.ts';

export interface AppProps {
  /** The pairing invite the page was opened with (already removed from the URL). */
  initialInvite: string | null;
  /** Where to open first (a `#route=` link or a notification's `#s=` link). */
  initialRoute: Route | null;
  theme: ThemeId;
  onTheme: (theme: ThemeId) => void;
  standalone: boolean;
  /** Registers the handler for deep links arriving while the app runs (notification taps, hash changes). */
  onDeepLink: (open: (route: Route) => void) => void;
  /** Registers the handler for a `#pair=` link opened while the app runs (the hash is already stripped). */
  onInviteLink: (open: (invite: string) => void) => void;
}

export function App(props: AppProps): ReactNode {
  const phase = useClientState((state) => state.conn.phase);
  const pairing = useClientState((state) => state.pairing);
  const flow = useClientState((state) => state.pairingFlow);
  const [invite, setInvite] = useState(props.initialInvite);
  const { onInviteLink } = props;
  useEffect(() => {
    onInviteLink(setInvite);
  }, [onInviteLink]);

  // A finished pairing flow consumes the invite; adopting the stored pairing does not.
  const tracker = useRef(INITIAL_TRACKER);
  useEffect(() => {
    const seen = observePairing(tracker.current, pairing, flow !== null);
    tracker.current = seen.tracker;
    if (seen.consumed) setInvite(null);
  }, [pairing, flow]);

  if (phase === 'loading') {
    return (
      <div className="screen">
        <div className="pair-hero" style={{ marginTop: '30dvh' }}>
          <div className="mark">◆</div>
          <span className="busy" />
        </div>
        <Toasts />
      </div>
    );
  }
  if (pairing === null || invite !== null || flow !== null) {
    return (
      <>
        <PairingScreen invite={invite} onInvite={setInvite} standalone={props.standalone} />
        <Toasts />
      </>
    );
  }
  return <Shell {...props} />;
}

function Shell({ initialRoute, theme, onTheme, standalone, onDeepLink }: AppProps): ReactNode {
  // A link wins; a reload (hash already stripped) resumes from the route the history entry holds.
  const [start] = useState(() => startRoute(initialRoute, history.state));
  const [tab, setTabState] = useState<Tab>(start.tab);
  const [stack, setStack] = useState<readonly Screen[]>(() => start.stack);
  // Always followed: running indicators, pending confirmations, and the inbox badge.
  useFeed({ feed: 'live' }, 'live');
  usePushSync();
  const inbox = useFeed({ feed: 'inbox' }, 'inbox');

  // Each pushed screen adds a history entry holding the whole route; the browser back button restores it.
  // The starting entry is kept in step with local navigation (`syncEntry`), so it never goes stale.
  const syncEntry = useCallback((at: Tab, next: readonly Screen[]) => {
    history.replaceState(syncedEntry(history.state, at, next), '');
  }, []);
  useEffect(() => {
    syncEntry(start.tab, start.stack);
  }, [syncEntry, start]);
  useEffect(() => {
    const onPop = (event: PopStateEvent) => {
      const route = routeFromHistory(event.state);
      if (route === null) {
        setStack([]);
        return;
      }
      setTabState(route.tab);
      setStack(route.stack);
    };
    window.addEventListener('popstate', onPop);
    return () => window.removeEventListener('popstate', onPop);
  }, []);
  const depth = useRef(stack.length);
  depth.current = stack.length;
  const current = useRef({ tab, stack });
  current.current = { tab, stack };

  const push = useCallback((screen: Screen) => {
    const next = [...current.current.stack, screen];
    history.pushState(historyEntry(current.current.tab, next), '');
    setStack(next);
  }, []);
  const replace = useCallback(
    (screen: Screen) => {
      const { tab: at, stack: from } = current.current;
      const next = from.length === 0 ? [screen] : [...from.slice(0, -1), screen];
      syncEntry(at, next);
      setStack(next);
    },
    [syncEntry],
  );
  const pop = useCallback(() => {
    if (depth.current === 0) return;
    if (!isBaseEntry(history.state)) {
      history.back();
      return;
    }
    // The starting entry has nothing behind it in the app: pop locally and keep the entry current.
    const next = current.current.stack.slice(0, -1);
    syncEntry(current.current.tab, next);
    setStack(next);
  }, [syncEntry]);
  const setTab = useCallback(
    (next: Tab) => {
      syncEntry(next, []);
      setTabState(next);
      setStack([]);
    },
    [syncEntry],
  );
  const nav = useMemo<Nav>(() => ({ tab, stack, setTab, push, replace, pop }), [tab, stack, setTab, push, replace, pop]);

  useEffect(() => {
    onDeepLink((route) => {
      setTabState(route.tab);
      // One history entry per screen, each holding its route, so the back button restores
      // whatever was showing before the link.
      for (const entry of deepLinkEntries(route)) history.pushState(entry, '');
      setStack(route.stack);
    });
  }, [onDeepLink]);

  const top = stack.at(-1);
  return (
    <NavContext.Provider value={nav}>
      {top === undefined ? (
        <>
          {tab === 'sessions' && <SessionsScreen standalone={standalone} />}
          {tab === 'agents' && <AgentsScreen />}
          {tab === 'inbox' && <InboxScreen />}
          {tab === 'settings' && <SettingsScreen theme={theme} onTheme={onTheme} standalone={standalone} />}
          <BottomNav tab={tab} badge={inbox?.inbox.badge ?? null} onSelect={setTab} />
        </>
      ) : (
        <PushedScreen key={`${stack.length}:${JSON.stringify(top)}`} screen={top} />
      )}
      <Toasts />
    </NavContext.Provider>
  );
}

function PushedScreen({ screen }: { screen: Screen }): ReactNode {
  switch (screen.kind) {
    case 'conversation':
      return <ConversationScreen session={screen.session} liveKey={screen.liveKey} title={screen.title} />;
    case 'subagent':
      return <SubagentScreen session={screen.session} agent={screen.agent} />;
    case 'terminal':
      return <TerminalScreen liveKey={screen.liveKey} title={screen.title} />;
    case 'changes':
      return <ChangesScreen project={screen.project} display={screen.display} />;
    case 'manager':
      return <ManagerScreen />;
    case 'tree':
      return <TreeScreen session={screen.session} title={screen.title} />;
    case 'browse':
      return <BrowseScreen />;
    case 'worktrees':
      return <WorktreesScreen project={screen.project} display={screen.display} />;
    case 'running':
      return <RunningScreen />;
    case 'settings':
      return <SettingsSectionScreen section={screen.section} />;
  }
}
