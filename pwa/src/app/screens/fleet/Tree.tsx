// The session tree (`tree`, paged): omp's entries as an indented list for a phone, with the
// active path drawn as a rail and a "you are here" marker at the active leaf. Filters as omp's
// tree selector (conversation/default/all/labeled) plus a text search. Per entry: Fork here
// (`fork_at`, disabled with the reason the desktop would refuse it) and Go here (`navigate`,
// idle only).

import { type CSSProperties, type ReactNode, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { api } from '../../../remote/index.ts';
import { composerDraftKey } from '../../client.ts';
import { useNav, useScreenNavigationGuard } from '../../nav.ts';
import { age, dollars, perform, succeeded } from '../../ui/format.ts';
import { useCan, useClient, useClientState, useLiveFor, useNow } from '../../ui/hooks.ts';
import { Empty, NeedsS4, Sheet, StaleBanner, useS4 } from '../../ui/kit.tsx';
import { mergeComposerPrefill } from '../conversation/compose.ts';
import { doneResult, errorText, resumeFork } from './common.ts';
import { type TreeRow, forkRefusal, layoutTree, mergeTreePage, navigateRefusal, searchTree, treeMove } from './logic.ts';
import '../../../styles/fleet.css';

const PAGE = 500;

const FILTERS: readonly (readonly [api.TreeFilter, string])[] = [
  ['default', 'Default'],
  ['conversation', 'Conversation'],
  ['labeled', 'Labeled'],
  ['all', 'All'],
];

const KIND: Record<api.TreeNodeKind, readonly [string, string]> = {
  user: ['›', 'You'],
  assistant: ['◆', 'Assistant'],
  tool_result: ['⚙', 'Tool result'],
  developer: ['⌘', 'Developer'],
  bash_execution: ['$', 'Shell'],
  other_role: ['·', 'Message'],
  custom_message: ['✉', 'Custom message'],
  compaction: ['⇊', 'Compaction'],
  branch_summary: ['⑂', 'Branch summary'],
  model_change: ['⇄', 'Model change'],
  model_usage: ['Σ', 'Model usage'],
  thinking_change: ['◐', 'Thinking change'],
  label: ['⚑', 'Label'],
  custom: ['✦', 'Custom'],
  setting: ['⚙', 'Setting'],
};

interface TreeState {
  nodes: api.TreeNode[];
  activeLeaf: string | null;
  prev: string | null;
  next: string | null;
  total: number | null;
  truncated: boolean;
}

type Page = 'initial' | 'earlier' | 'later';
type TreeScroll = 'here' | { row: HTMLElement; top: number };

function visibleAnchor(body: HTMLDivElement): TreeScroll | null {
  const viewportTop = body.getBoundingClientRect().top;
  for (const row of body.querySelectorAll<HTMLElement>('.tree-row')) {
    const { top, bottom } = row.getBoundingClientRect();
    if (bottom > viewportTop) return { row, top };
  }
  return null;
}

export function TreeScreen({ session, title }: { session: string; title: string }): ReactNode {
  const client = useClient();
  const nav = useNav();
  const ownsTop = useScreenNavigationGuard(nav);
  const s4 = useS4();
  const treeNav = useClientState((state) => state.welcome?.features.includes(api.FEATURE_TREE_NAV) === true);
  const online = useClientState((state) => state.conn.phase === 'online');
  const epoch = useClientState((state) => state.welcome?.epoch ?? null);
  const live = useLiveFor(session);
  const [filter, setFilter] = useState<api.TreeFilter>('default');
  const [query, setQuery] = useState('');
  const [tree, setTree] = useState<TreeState | null>(null);
  const [loading, setLoading] = useState<Page | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [picked, setPicked] = useState<TreeRow | null>(null);
  const [reload, setReload] = useState(0);
  const body = useRef<HTMLDivElement>(null);
  const pendingScroll = useRef<TreeScroll | null>(null);
  const requestId = useRef(0);

  const load = useCallback(
    async (page: Page, cursor: string | null, base: TreeState | null) => {
      const id = ++requestId.current;
      setLoading(page);
      setError(null);
      try {
        const resp = await client.request({
          op: 'tree', session, filter, cursor, limit: PAGE,
          ...(treeNav && cursor === null ? { at_leaf: true } : {}),
        });
        if (id !== requestId.current || resp.type !== 'tree') return;
        if (page === 'initial' && treeNav) pendingScroll.current = 'here';
        else if (page === 'earlier' && body.current !== null) pendingScroll.current = visibleAnchor(body.current);
        setTree({
          nodes: base === null ? resp.nodes : page === 'earlier' ? mergeTreePage(resp.nodes, base.nodes) : mergeTreePage(base.nodes, resp.nodes),
          activeLeaf: treeNav ? resp.active_leaf : resp.active_leaf ?? base?.activeLeaf ?? null,
          prev: !treeNav ? null : page === 'later' ? base?.prev ?? null : resp.prev_cursor ?? null,
          next: page === 'earlier' ? base?.next ?? null : resp.next_cursor,
          total: treeNav ? resp.total ?? base?.total ?? null : null,
          truncated: resp.truncated,
        });
      } catch (failure) {
        if (id === requestId.current) setError(errorText(failure));
      } finally {
        if (id === requestId.current) setLoading(null);
      }
    },
    [client, session, filter, treeNav],
  );

  useEffect(() => {
    if (!online || !s4) return;
    pendingScroll.current = null;
    setTree(null);
    void load('initial', null, null);
    return () => {
      requestId.current++;
    };
  }, [load, online, s4, epoch, reload]);

  const rows = useMemo(() => (tree === null ? [] : layoutTree(tree.nodes, tree.activeLeaf)), [tree]);
  const shown = useMemo(() => searchTree(rows, query), [rows, query]);
  const hereIndex = shown.findIndex((row) => row.here);

  useLayoutEffect(() => {
    const scroll = pendingScroll.current;
    pendingScroll.current = null;
    const element = body.current;
    if (element === null || scroll === null) return;
    if (scroll === 'here') element.querySelector('#tree-here')?.scrollIntoView({ block: 'center' });
    else if (element.contains(scroll.row)) element.scrollTop += scroll.row.getBoundingClientRect().top - scroll.top;
  }, [shown]);

  const goToConversation = (liveKey: string | null) => {
    if (!ownsTop()) return;
    const previous = nav.stack.at(-2);
    if (previous?.kind === 'conversation' &&
        (previous.session === session || (liveKey !== null && previous.session === null && previous.liveKey === liveKey))) {
      nav.pop();
    } else {
      nav.replace({ kind: 'conversation', session, liveKey, title });
    }
  };

  return (
    <div className="screen pushed tree-screen">
      <header className="top">
        <div className="eyebrow">Session tree</div>
        <div className="line">
          <button type="button" className="back" onClick={nav.pop} aria-label="Back">
            ‹
          </button>
          <h2 className="grow ellipsis">{title}</h2>
          <span className="count">{tree?.total ?? tree?.nodes.length ?? 0}</span>
        </div>
        <div className="segmented" style={{ marginTop: 10 }}>
          {FILTERS.map(([id, label]) => (
            <button key={id} type="button" aria-pressed={filter === id} onClick={() => setFilter(id)}>
              {label}
            </button>
          ))}
        </div>
      </header>
      <div className="body" ref={body}>
        <StaleBanner />
        <NeedsS4 what="The session tree">
          <label className="search">
            <span aria-hidden="true">⌕</span>
            <input type="search" placeholder="Search entries" value={query} onChange={(event) => setQuery(event.target.value)} aria-label="Search entries" />
            {hereIndex >= 0 && (
              <button
                type="button"
                className="chip accent"
                onClick={() => body.current?.querySelector('#tree-here')?.scrollIntoView({ block: 'center', behavior: 'smooth' })}
              >
                ◉ Here
              </button>
            )}
          </label>
          {tree?.prev != null && (
            <button type="button" className="more-row" disabled={loading !== null} onClick={() => void load('earlier', tree.prev, tree)}>
              {loading === 'earlier' ? 'Loading…' : 'Load earlier entries'}
            </button>
          )}
          {error !== null ? (
            <Empty title="Could not load the tree">
              <span>{error}</span>
              <button type="button" className="button" style={{ marginTop: 10 }} onClick={() => setReload(reload + 1)}>
                Try again
              </button>
            </Empty>
          ) : tree === null ? (
            <Empty title="Loading the tree…" />
          ) : shown.length === 0 ? (
            <Empty title={query.trim().length > 0 ? 'No entry matches' : 'No entries'}>
              {filter === 'labeled' ? 'No entry carries a label.' : undefined}
            </Empty>
          ) : (
            <ol className="tree" aria-label="Session entries">
              {shown.map((row) => (
                <TreeEntry key={row.node.id} row={row} onPick={() => setPicked(row)} />
              ))}
            </ol>
          )}
          {tree?.next != null && (
            <button type="button" className="more-row" disabled={loading !== null} onClick={() => void load('later', tree.next, tree)}>
              {loading === 'later' ? 'Loading…' : 'Load more entries'}
            </button>
          )}
          {tree?.truncated === true && <p className="muted small">Some entry texts were shortened.</p>}
        </NeedsS4>
      </div>
      {picked !== null && (
        <NodeSheet
          row={picked}
          session={session}
          live={live}
          onClose={() => setPicked(null)}
          onForked={(forked) => {
            if (ownsTop()) nav.push({ kind: 'conversation', ...forked, title: 'Fork' });
          }}
          onMoved={goToConversation}
          onLegacyMoved={() => {
            if (!ownsTop()) return;
            setPicked(null);
            setReload((count) => count + 1);
          }}
        />
      )}
    </div>
  );
}

function TreeEntry({ row, onPick }: { row: TreeRow; onPick: () => void }): ReactNode {
  const now = useNow();
  const [glyph, kind] = KIND[row.node.kind];
  return (
    <li
      id={row.here ? 'tree-here' : undefined}
      className={`tree-row${row.active ? ' on-path' : ''}${row.here ? ' here' : ''}${row.branchStart ? ' branch' : ''}`}
      style={{ '--depth': Math.min(row.depth, 8) } as CSSProperties}
    >
      <span className="rail" aria-hidden="true" />
      <button type="button" className="tree-node" onClick={onPick} aria-label={`${kind}: ${row.node.text}`}>
        <span className={`glyph k-${row.node.kind}`} aria-hidden="true">
          {glyph}
        </span>
        <span className="copy">
          <span className="tree-meta">
            <span className="kind">{kind}</span>
            {row.node.label !== null && <span className="chip accent">⚑ {row.node.label}</span>}
            {row.children > 1 && <span className="chip">⑂ {row.children}</span>}
            {row.here && <span className="chip here-chip">◉ you are here</span>}
            {row.node.at_unix_ms !== null && <span className="when">{age(row.node.at_unix_ms, now)}</span>}
          </span>
          <span className="text">{row.node.text.length > 0 ? row.node.text : row.node.subtype ?? '—'}</span>
        </span>
      </button>
    </li>
  );
}

function NodeSheet({
  row,
  session,
  live,
  onClose,
  onForked,
  onMoved,
  onLegacyMoved,
}: {
  row: TreeRow;
  session: string;
  live: api.LiveSession | null;
  onClose: () => void;
  onForked: (forked: { session: string; liveKey: string | null }) => void;
  onMoved: (liveKey: string | null) => void;
  onLegacyMoved: () => void;
}): ReactNode {
  const client = useClient();
  const can = useCan();
  const [busy, setBusy] = useState<'forking' | 'starting' | 'moving' | null>(null);
  const node = row.node;
  const [glyph, kind] = KIND[node.kind];
  const forkWhy = forkRefusal(node, live);
  const goWhy = row.here ? 'You are already here.' : navigateRefusal(live);
  const forkOp: api.Op = { op: 'fork_at', session, entry: node.id };

  const fork = async () => {
    setBusy('forking');
    const result = doneResult(await perform(client, forkOp, 'Fork at this entry', 'Forked into a new session.'));
    const forked = result?.type === 'file' ? await resumeFork(client, result.session, () => setBusy('starting')) : null;
    setBusy(null);
    if (result?.type === 'file') onClose();
    if (forked !== null) onForked(forked);
  };
  const go = async () => {
    if (live === null) return;
    const epoch = client.getState().welcome?.epoch ?? null;
    setBusy('moving');
    const view = await perform(client, { op: 'navigate', live: live.live, entry: node.id }, 'Go to this entry');
    setBusy(null);
    const result = doneResult(view);
    if (result?.type !== 'moved') {
      if (succeeded(view)) onLegacyMoved();
      return;
    }
    if (result.message !== null) {
      client.notify(result.message);
      return;
    }
    const state = client.getState();
    const feed = state.feeds.get('live');
    const target = live.live;
    const current = client.acceptsLiveDraft(target, epoch) && feed?.feed === 'live' &&
      feed.sessions.some((entry) => entry.session === session && entry.status.state !== 'exited' &&
        entry.live.key === target.key && entry.live.generation === target.generation);
    let decision = treeMove(result, current);
    let restore = current;
    if (decision.prefill !== null) {
      const draft = state.composerDrafts.get(composerDraftKey(target))?.text ?? '';
      restore = client.setComposerDraft(target, mergeComposerPrefill(draft, decision.prefill), epoch);
      if (!restore) decision = treeMove(result, false);
    }
    client.notify(decision.notice);
    onMoved(restore ? target.key : null);
  };

  return (
    <Sheet title={kind} icon={glyph} onClose={onClose}>
      <div className="node-text">{node.text.length > 0 ? node.text : '(no text)'}</div>
      <dl className="facts wide">
        {node.label !== null && (
          <div>
            <dt>Label</dt>
            <dd>
              <span className="chip accent">⚑ {node.label}</span>
            </dd>
          </div>
        )}
        <div>
          <dt>When</dt>
          <dd>{node.at_unix_ms === null ? '—' : new Date(node.at_unix_ms).toLocaleString()}</dd>
        </div>
        <div>
          <dt>Spent before</dt>
          <dd>{node.cost_before === null ? '—' : `${node.cost_incomplete ? '≥ ' : ''}${dollars(node.cost_before)}`}</dd>
        </div>
        {node.stop_reason !== null && (
          <div>
            <dt>Stop reason</dt>
            <dd>{node.stop_reason}</dd>
          </div>
        )}
        <div>
          <dt>Path</dt>
          <dd>{row.here ? <span className="chip here-chip">◉ you are here</span> : row.active ? <span className="chip ok">active path</span> : <span className="chip">side branch</span>}</dd>
        </div>
      </dl>
      <div className="node-actions">
        <div className="action-block">
          <button type="button" className="button primary block" disabled={busy !== null || forkWhy !== null || !can(forkOp)} onClick={() => void fork()}>
            {busy === 'starting' ? 'Starting the fork…' : busy === 'forking' ? 'Forking…' : '⑂ Fork here'}
          </button>
          <small className={forkWhy === null ? 'muted' : 'refusal'}>{forkWhy ?? 'Starts a new session branching off at this entry.'}</small>
        </div>
        <div className="action-block">
          <button
            type="button"
            className="button block"
            disabled={busy !== null || goWhy !== null || live === null || !can({ op: 'navigate', live: live.live, entry: node.id })}
            onClick={() => void go()}
          >
            ↪ Go here
          </button>
          <small className={goWhy === null ? 'muted' : 'refusal'}>
            {goWhy ?? (node.kind === 'user' ? 'Move just before this prompt so you can edit it.' : 'Continue the running session from this point.')}
          </small>
        </div>
      </div>
    </Sheet>
  );
}
