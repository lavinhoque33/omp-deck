import { describe, expect, test } from 'bun:test';
import {
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
  selectionTotals,
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
