// The session tree (`tree`, paged): omp's entries as an indented list for a phone, with the
// active path drawn as a rail and a "you are here" marker at the active leaf. Filters as omp's
// tree selector (conversation/default/all/labeled) plus a text search. Per entry: Fork here
// (`fork_at`, disabled with the reason the desktop would refuse it) and Go here (`navigate`,
// idle only).

import { type CSSProperties, type ReactNode, useCallback, useEffect, useMemo, useState } from 'react';
import type { api } from '../../../remote/index.ts';
import { useNav } from '../../nav.ts';
import { age, dollars, perform, succeeded } from '../../ui/format.ts';
import { useCan, useClient, useClientState, useLiveFor, useNow } from '../../ui/hooks.ts';
import { Empty, NeedsS4, Sheet, StaleBanner, useS4 } from '../../ui/kit.tsx';
import { doneResult, errorText } from './common.ts';
import { type TreeRow, forkRefusal, layoutTree, mergeTreePage, navigateRefusal, searchTree } from './logic.ts';
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
  cursor: string | null;
  truncated: boolean;
}

export function TreeScreen({ session, title }: { session: string; title: string }): ReactNode {
  const client = useClient();
  const nav = useNav();
  const s4 = useS4();
  const online = useClientState((state) => state.conn.phase === 'online');
  const epoch = useClientState((state) => state.welcome?.epoch ?? null);
  const live = useLiveFor(session);
  const [filter, setFilter] = useState<api.TreeFilter>('default');
  const [query, setQuery] = useState('');
  const [tree, setTree] = useState<TreeState | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [picked, setPicked] = useState<TreeRow | null>(null);
  const [reload, setReload] = useState(0);

  const load = useCallback(
    async (cursor: string | null, base: TreeState | null) => {
      setLoading(true);
      setError(null);
      try {
        const resp = await client.request({ op: 'tree', session, filter, cursor, limit: PAGE });
        if (resp.type !== 'tree') return;
        setTree({
          nodes: base === null ? resp.nodes : mergeTreePage(base.nodes, resp.nodes),
          activeLeaf: resp.active_leaf ?? base?.activeLeaf ?? null,
          cursor: resp.next_cursor,
          truncated: resp.truncated,
        });
      } catch (failure) {
        setError(errorText(failure));
      } finally {
        setLoading(false);
      }
    },
    [client, session, filter],
  );

  useEffect(() => {
    if (!online || !s4) return;
    setTree(null);
    void load(null, null);
  }, [load, online, s4, epoch, reload]);

  const rows = useMemo(() => (tree === null ? [] : layoutTree(tree.nodes, tree.activeLeaf)), [tree]);
  const shown = useMemo(() => searchTree(rows, query), [rows, query]);
  const hereIndex = shown.findIndex((row) => row.here);

  return (
    <div className="screen pushed tree-screen">
      <header className="top">
        <div className="eyebrow">Session tree</div>
        <div className="line">
          <button type="button" className="back" onClick={nav.pop} aria-label="Back">
            ‹
          </button>
          <h2 className="grow ellipsis">{title}</h2>
          <span className="count">{tree?.nodes.length ?? 0}</span>
        </div>
        <div className="segmented" style={{ marginTop: 10 }}>
          {FILTERS.map(([id, label]) => (
            <button key={id} type="button" aria-pressed={filter === id} onClick={() => setFilter(id)}>
              {label}
            </button>
          ))}
        </div>
      </header>
      <div className="body">
        <StaleBanner />
        <NeedsS4 what="The session tree">
          <label className="search">
            <span aria-hidden="true">⌕</span>
            <input type="search" placeholder="Search entries" value={query} onChange={(event) => setQuery(event.target.value)} aria-label="Search entries" />
            {hereIndex >= 0 && (
              <button
                type="button"
                className="chip accent"
                onClick={() => document.getElementById('tree-here')?.scrollIntoView({ block: 'center', behavior: 'smooth' })}
              >
                ◉ Here
              </button>
            )}
          </label>
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
          {tree !== null && tree.cursor !== null && (
            <button type="button" className="more-row" disabled={loading} onClick={() => void load(tree.cursor, tree)}>
              {loading ? 'Loading…' : 'Load more entries'}
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
          onMoved={() => {
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
  onMoved,
}: {
  row: TreeRow;
  session: string;
  live: api.LiveSession | null;
  onClose: () => void;
  onMoved: () => void;
}): ReactNode {
  const client = useClient();
  const nav = useNav();
  const can = useCan();
  const [busy, setBusy] = useState(false);
  const node = row.node;
  const [glyph, kind] = KIND[node.kind];
  const forkWhy = forkRefusal(node, live);
  const goWhy = row.here ? 'You are already here.' : navigateRefusal(live);
  const forkOp: api.Op = { op: 'fork_at', session, entry: node.id };

  const fork = async () => {
    setBusy(true);
    const view = await perform(client, forkOp, 'Fork at this entry', 'Forked into a new session.');
    setBusy(false);
    const result = doneResult(view);
    if (result?.type === 'file') {
      onClose();
      if (result.session !== null) nav.push({ kind: 'conversation', session: result.session, liveKey: null, title: 'Fork' });
    }
  };
  const go = async () => {
    if (live === null) return;
    setBusy(true);
    const view = await perform(client, { op: 'navigate', live: live.live, entry: node.id }, 'Go to this entry', 'Moved the session here.');
    setBusy(false);
    if (succeeded(view)) onMoved();
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
          <button type="button" className="button primary block" disabled={busy || forkWhy !== null || !can(forkOp)} onClick={() => void fork()}>
            ⑂ Fork here
          </button>
          <small className={forkWhy === null ? 'muted' : 'refusal'}>{forkWhy ?? 'A new session file branches off at this entry.'}</small>
        </div>
        <div className="action-block">
          <button
            type="button"
            className="button block"
            disabled={busy || goWhy !== null || live === null || !can({ op: 'navigate', live: live.live, entry: node.id })}
            onClick={() => void go()}
          >
            ↪ Go here
          </button>
          <small className={goWhy === null ? 'muted' : 'refusal'}>{goWhy ?? 'The running session continues from this entry.'}</small>
        </div>
      </div>
    </Sheet>
  );
}
