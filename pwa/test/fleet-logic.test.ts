import { describe, expect, test } from 'bun:test';
import {
  type TreeRow,
  NO_FILTER,
  activeFilters,
  branchProblem,
  bytes,
  filterInventory,
  forkRefusal,
  forksOf,
  lastPart,
  layoutTree,
  memoryFreed,
  mergeTreePage,
  navigateRefusal,
  nestWorktrees,
  pushCrumb,
  searchTree,
  runningRows,
  selectionTotals,
  treeMove,
  uptime,
} from '../src/app/screens/fleet/logic.ts';
import type { api } from '../src/remote/index.ts';

const NOW = 1_800_000_000_000;
const DAY = 86_400_000;

function entry(id: string, patch: Partial<api.InventoryView> = {}, stats: Partial<api.InventoryStats> | null = {}): api.InventoryView {
  return {
    id,
    project: 'p1',
    display: '~/src/one',
    title: `Title ${id}`,
    first_prompt: null,
    stub: false,
    created_unix_ms: NOW - 10 * DAY,
    modified_unix_ms: NOW - DAY,
    jsonl_bytes: 1000,
    artifact_bytes: 0,
    artifact_files: 0,
    subagents: 0,
    parent: null,
    cwd_exists: true,
    running: false,
    stats:
      stats === null
        ? null
        : {
            user_messages: 2,
            assistant_turns: 3,
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost: 0.5,
            subagent_cost: null,
            model: 'anthropic/claude',
            compactions: 0,
            last_activity_unix_ms: null,
            ...stats,
          },
    ...patch,
  };
}

function node(id: string, parent: string | null, patch: Partial<api.TreeNode> = {}): api.TreeNode {
  return {
    id,
    parent,
    kind: 'user',
    subtype: null,
    at_unix_ms: null,
    text: `text ${id}`,
    has_text: true,
    stop_reason: null,
    label: null,
    forkable: true,
    cost_before: null,
    cost_incomplete: false,
    ...patch,
  };
}

function live(patch: Partial<api.LiveSession> = {}): api.LiveSession {
  return {
    live: { key: 'k', generation: 1 },
    kind: 'session',
    label: 'l',
    title: null,
    session: 's',
    project: null,
    display: '~',
    activity: 'idle',
    status: { state: 'idle' },
    exit_code: null,
    busy: false,
    model: null,
    pending_switch: null,
    live_control: true,
    desktop_controlled: false,
    views: 0,
    requests: [],
    ...patch,
  };
}

describe('format', () => {
  test('bytes', () => {
    expect(bytes(0)).toBe('0 B');
    expect(bytes(999)).toBe('999 B');
    expect(bytes(4200)).toBe('4.2 KB');
    expect(bytes(13_000_000)).toBe('13 MB');
    expect(bytes(1_400_000_000)).toBe('1.4 GB');
  });
  test('uptime', () => {
    expect(uptime(45_000)).toBe('45s');
    expect(uptime(12 * 60_000)).toBe('12m');
    expect(uptime(200 * 60_000)).toBe('3h 20m');
    expect(uptime(52 * 3_600_000)).toBe('2d 4h');
    expect(uptime(-5)).toBe('0s');
  });
  test('lastPart', () => {
    expect(lastPart('~/src/omp')).toBe('omp');
    expect(lastPart('~/src/omp/')).toBe('omp');
    expect(lastPart('~')).toBe('~');
    expect(lastPart('C:\\work\\x')).toBe('x');
  });
});

describe('inventory', () => {
  const entries = [
    entry('a', { stub: true, jsonl_bytes: 200 }, null),
    entry('b', { modified_unix_ms: NOW - 40 * DAY, jsonl_bytes: 5_000_000 }, { cost: 2, subagent_cost: 1 }),
    entry('c', { cwd_exists: false, running: true, title: 'Fix relay' }, { last_activity_unix_ms: NOW - 2 * DAY }),
    entry('d', { parent: 'b', artifact_bytes: 9000 }),
  ];

  test('filters', () => {
    const ids = (filter: Partial<typeof NO_FILTER>) =>
      filterInventory(entries, { ...NO_FILTER, ...filter }, 'activity', false, NOW).map((e) => e.id);
    expect(ids({ stubsOnly: true })).toEqual(['a']);
    expect(ids({ missingOnly: true })).toEqual(['c']);
    expect(ids({ notRunningOnly: true }).sort()).toEqual(['a', 'b', 'd']);
    expect(ids({ olderThanDays: 30 })).toEqual(['b']);
    expect(ids({ largerThan: 1_000_000 })).toEqual(['b']);
    expect(ids({ text: 'RELAY' })).toEqual(['c']);
    expect(ids({ text: 'claude' }).sort()).toEqual(['b', 'c', 'd']);
  });

  test('sorts', () => {
    const sorted = (sort: Parameters<typeof filterInventory>[2], asc = false) =>
      filterInventory(entries, NO_FILTER, sort, asc, NOW).map((e) => e.id);
    expect(sorted('size')).toEqual(['b', 'd', 'c', 'a']);
    expect(sorted('size', true)).toEqual(['a', 'c', 'd', 'b']);
    expect(sorted('cost')[0]).toBe('b');
    expect(sorted('cost').at(-1)).toBe('a');
    expect(sorted('activity')).toEqual(['a', 'd', 'c', 'b']);
  });

  test('totals and forks', () => {
    const totals = selectionTotals(entries);
    expect(totals).toEqual({
      count: 4,
      bytes: 200 + 5_000_000 + 1000 + 10_000,
      messages: 15,
      tokens: 450,
      cost: 3 + 0.5 + 0.5,
      running: 1,
      stubs: 1,
      subagents: 0,
    });
    expect(forksOf(entries, 'b').map((e) => e.id)).toEqual(['d']);
    expect(activeFilters(NO_FILTER)).toBe(0);
    expect(activeFilters({ ...NO_FILTER, text: ' x ', stubsOnly: true })).toBe(2);
  });
});

describe('running', () => {
  const process = (id: string, patch: Partial<api.RunningView> = {}): api.RunningView => ({
    id, sessions: [], project: null, display: null, title: null, backend: 'host',
    started_unix_ms: null, pid: null, memory_bytes: null, processes: null, live: null,
    ...patch,
  });

  test('the top section merges backend and managed sessions without duplicates or exited rows', () => {
    const saved = live({ title: 'Saved', session: 'saved' });
    const fresh = live({ live: { key: 'fresh', generation: 1 }, session: null, title: 'Fresh', desktop_controlled: true });
    const ended = live({ live: { key: 'ended', generation: 1 }, status: { state: 'exited', clean: true } });
    const rows = runningRows([saved, fresh, ended], [
      process('saved-process', { sessions: ['saved'] }),
      process('fresh-process', { live: fresh.live }),
      process('ended-process', { live: ended.live }),
      process('unmanaged', { sessions: ['other'], title: 'Detached', started_unix_ms: 2 }),
      process('unmapped', { started_unix_ms: 1 }),
    ]);
    expect(rows.map((row) => [row.key, row.session])).toEqual([
      ['k', 'saved'], ['fresh', null], ['unmanaged', 'other'], ['unmapped', null],
    ]);
    expect(rows[1]?.live?.desktop_controlled).toBe(true);
    expect(rows[2]?.title).toBe('Detached');
    expect(rows[3]?.title).toBe('Unmapped process');
  });

  test('orders active and idle sessions by recency regardless of management', () => {
    const idle = live({ live: { key: 'managed-idle', generation: 1 } });
    const busy = live({ live: { key: 'managed-busy', generation: 1 }, status: { state: 'busy' } });
    const attention = live({ live: { key: 'managed-attention', generation: 1 }, status: { state: 'attention' } });
    const rows = runningRows([idle, busy, attention], [
      process('idle-unknown', { started_unix_ms: 500 }),
      process('unmanaged-idle', { started_unix_ms: 100, recent_unix_ms: 60, activity: 'idle' }),
      process('unmanaged-newest-idle', { started_unix_ms: 1, recent_unix_ms: 80, activity: 'idle' }),
      process('idle-process', { live: idle.live, recent_unix_ms: 70, activity: 'busy' }),
      process('busy-process', { live: busy.live, recent_unix_ms: 20, activity: 'idle' }),
      process('attention-process', { live: attention.live, recent_unix_ms: 40 }),
      process('unmanaged-busy', { recent_unix_ms: 50, activity: 'busy' }),
      process('unmanaged-attention', { recent_unix_ms: 20, activity: 'attention' }),
      process('active-unknown', { activity: 'busy' }),
      process('idle-at-epoch', { recent_unix_ms: 0 }),
    ]);
    expect(rows.map(({ key }) => key)).toEqual([
      'unmanaged-busy', 'managed-attention', 'managed-busy', 'unmanaged-attention', 'active-unknown',
      'unmanaged-newest-idle', 'managed-idle', 'unmanaged-idle', 'idle-at-epoch', 'idle-unknown',
    ]);
    expect(rows.find(({ key }) => key === 'unmanaged-attention')?.activity).toBe('attention');
  });

  test('without running-feed activity or recency, active managed rows lead and ties preserve input order', () => {
    const idle = live({ live: { key: 'idle', generation: 1 } });
    const desktop = live({ live: { key: 'desktop', generation: 1 }, desktop_controlled: true });
    const busy = live({ live: { key: 'busy', generation: 1 }, busy: true });
    const attention = live({ live: { key: 'attention', generation: 1 }, status: { state: 'attention' } });
    const rows = runningRows([idle, desktop, busy, attention], [
      process('old', { started_unix_ms: 1 }),
      process('new', { started_unix_ms: 2 }),
    ]);
    expect(rows.map(({ key }) => key)).toEqual(['busy', 'attention', 'idle', 'desktop', 'new', 'old']);
  });

  test('fresh and direct sessions stay available without backend discovery', () => {
    const fresh = live({ session: null });
    expect(runningRows([fresh], [])[0]).toMatchObject({ key: 'k', live: fresh, session: null });
    expect(runningRows([live({ status: { state: 'exited', clean: false } })], [])).toEqual([]);
  });

  test('memoryFreed', () => {
    const run = (memory: number | null): api.RunningView => ({
      id: 'r',
      sessions: [],
      project: null,
      display: null,
      title: null,
      backend: 'tmux',
      started_unix_ms: null,
      pid: null,
      memory_bytes: memory,
      processes: null,
      live: null,
    });
    expect(memoryFreed([run(100), run(null), run(50)])).toEqual({ bytes: 150, unknown: 1 });
  });
});

describe('tree', () => {
  // r → a → b → c (active) ; a → x → y (side branch)
  const nodes = [node('r', null), node('a', 'r'), node('b', 'a'), node('c', 'b'), node('x', 'a'), node('y', 'x')];

  test('layout indents branches and marks the active path', () => {
    const rows = layoutTree(nodes, 'c');
    expect(rows.map((row) => [row.node.id, row.depth, row.active, row.here, row.branchStart])).toEqual([
      ['r', 0, true, false, false],
      ['a', 0, true, false, false],
      ['b', 1, true, false, false],
      ['c', 1, true, true, false],
      ['x', 1, false, false, true],
      ['y', 1, false, false, false],
    ]);
    expect(rows[1]?.children).toBe(2);
  });

  test('orphans become roots; cycles do not loop', () => {
    const rows = layoutTree([node('q', 'missing'), node('s', 's')], null);
    expect(rows.map((row) => row.node.id)).toEqual(['q', 's']);
    expect(rows.every((row) => !row.active)).toBe(true);
  });

  test('a mid-file window marks the loaded part of the active path, and prepending reconnects it', () => {
    const window = nodes.slice(2);
    const describe = (rows: readonly TreeRow[]) =>
      rows.map((row) => [row.node.id, row.depth, row.active, row.here]);
    const windowRows = layoutTree(window, 'c');
    expect(describe(windowRows)).toEqual([
      ['b', 0, true, false],
      ['c', 0, true, true],
      ['x', 0, false, false],
      ['y', 0, false, false],
    ]);
    expect(searchTree(windowRows, 'TEXT C').find((row) => row.here)?.node.id).toBe('c');
    expect(searchTree(windowRows, 'TEXT X').some((row) => row.here)).toBe(false);
    const joined = mergeTreePage(nodes.slice(0, 3), window);
    expect(joined.map((n) => n.id)).toEqual(['r', 'a', 'b', 'c', 'x', 'y']);
    expect(describe(layoutTree(joined, 'c'))).toEqual([
      ['r', 0, true, false],
      ['a', 0, true, false],
      ['b', 1, true, false],
      ['c', 1, true, true],
      ['x', 1, false, false],
      ['y', 1, false, false],
    ]);
  });

  test('deep chains do not overflow the stack', () => {
    const chain = Array.from({ length: 20_000 }, (_, i) => node(`n${i}`, i === 0 ? null : `n${i - 1}`));
    const rows = layoutTree(chain, 'n19999');
    expect(rows.length).toBe(20_000);
    expect(rows.at(-1)?.here).toBe(true);
  });

  test('search and paging', () => {
    const rows = layoutTree(nodes, null);
    expect(searchTree(rows, 'TEXT Y').map((row) => row.node.id)).toEqual(['y']);
    expect(searchTree(rows, '  ').length).toBe(6);
    expect(mergeTreePage([node('a', null)], [node('a', null), node('b', 'a')]).map((n) => n.id)).toEqual(['a', 'b']);
  });

  test('Go here distinguishes a no-op, a move, and a prompt restored to either editor', () => {
    const result: Extract<api.DoneView, { type: 'moved' }> = {
      type: 'moved', leaf: 'c', before_prompt: false, editor_text: null, dropped_images: 0, message: null,
    };
    expect(treeMove({ ...result, message: 'Already at this point' }, false)).toEqual({
      moved: false, notice: 'Already at this point', prefill: null,
    });
    expect(treeMove(result, true)).toEqual({
      moved: true, notice: 'Moved there — new prompts continue from this point', prefill: null,
    });
    expect(treeMove({ ...result, before_prompt: true, editor_text: 'Edit me', dropped_images: 1 }, true)).toEqual({
      moved: true,
      notice: 'Moved before that prompt — its text is in the composer · 1 image of that prompt was not carried over',
      prefill: 'Edit me',
    });
    expect(treeMove({ ...result, before_prompt: true, leaf: null, editor_text: '', dropped_images: 2 }, true)).toEqual({
      moved: true,
      notice: 'Moved before that prompt — its text is in the composer · 2 images of that prompt were not carried over',
      prefill: '',
    });
    expect(treeMove({ ...result, before_prompt: true }, true)).toEqual({
      moved: true, notice: "Moved before that prompt — its text is in omp's editor on the desktop", prefill: null,
    });
    expect(treeMove({ ...result, before_prompt: true, editor_text: 'Edit me', dropped_images: 1 }, false)).toEqual({
      moved: true,
      notice: 'Moved before that prompt — its text was not applied because the session is no longer running there · 1 image of that prompt was not carried over',
      prefill: null,
    });
  });

  test('refusals', () => {
    expect(forkRefusal(node('a', null, { forkable: false }), null)).toContain('message');
    expect(forkRefusal(node('a', null), null)).toBeNull();
    expect(forkRefusal(node('a', null), live({ live_control: false }))).toContain('without control');
    expect(forkRefusal(node('a', null), live({ live_control: false, status: { state: 'exited', clean: true } }))).toBeNull();
    expect(navigateRefusal(null)).toContain('running');
    expect(navigateRefusal(live({ busy: true }))).toContain('idle');
    expect(navigateRefusal(live())).toBeNull();
  });
});

describe('worktrees', () => {
  const project = (id: string, worktree = false): api.Project => ({
    id,
    display: `~/${id}`,
    worktree,
    session_count: 1,
    modified_unix_ms: 0,
  });

  test('nests linked worktrees under their repo', () => {
    const projects = [project('wt1', true), project('repo'), project('wt2', true), project('other')];
    const links = new Map([
      ['wt1', { primary: 'repo', branch: 'feat/a' }],
      ['wt2', { primary: 'gone', branch: 'feat/b' }],
    ]);
    const nested = nestWorktrees(projects, links);
    expect(nested.map((group) => group.project.id)).toEqual(['repo', 'wt2', 'other']);
    expect(nested[0]?.children.map((child) => [child.project.id, child.branch])).toEqual([['wt1', 'feat/a']]);
  });

  test('branch names', () => {
    expect(branchProblem('feat/mobile-relay')).toBeNull();
    expect(branchProblem('')).not.toBeNull();
    expect(branchProblem('a b')).not.toBeNull();
    expect(branchProblem('a..b')).not.toBeNull();
    expect(branchProblem('-x')).not.toBeNull();
    expect(branchProblem('x.lock')).not.toBeNull();
    expect(branchProblem('a/.b')).not.toBeNull();
  });
});

describe('breadcrumb', () => {
  test('pushes and cuts back', () => {
    const roots = { dir: null, display: 'Folders' };
    const a = { dir: 'f-a', display: '~/a' };
    const b = { dir: 'f-b', display: '~/a/b' };
    expect(pushCrumb([roots], a)).toEqual([roots, a]);
    expect(pushCrumb([roots, a, b], a)).toEqual([roots, a]);
    expect(pushCrumb([roots, a, b], roots)).toEqual([roots]);
  });
});
