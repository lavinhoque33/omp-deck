// Pure fleet logic: session-manager filters, sorting and totals; running totals; the session
// tree's mobile layout (indent, active-path rail); worktree grouping; folder breadcrumbs.

import type { api } from '../../../remote/index.ts';

// ---- sizes and ages --------------------------------------------------------------------------------

/** "0 B", "812 B", "4.2 KB", "13 MB", "1.4 GB" (decimal units, as the desktop manager). */
export function bytes(count: number): string {
  if (count < 1000) return `${count} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let value = count / 1000;
  let unit = 0;
  while (value >= 1000 && unit < units.length - 1) {
    value /= 1000;
    unit++;
  }
  return `${value.toFixed(value < 10 ? 1 : 0)} ${units[unit]}`;
}

/** "45s", "12m", "3h 20m", "2d 4h". */
export function uptime(ms: number): string {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return minutes % 60 === 0 ? `${hours}h` : `${hours}h ${minutes % 60}m`;
  const days = Math.floor(hours / 24);
  return hours % 24 === 0 ? `${days}d` : `${days}d ${hours % 24}h`;
}

export function plural(count: number, one: string, many = `${one}s`): string {
  return `${count} ${count === 1 ? one : many}`;
}

// ---- session manager (inventory) -------------------------------------------------------------------

export function entrySize(entry: api.InventoryView): number {
  return entry.jsonl_bytes + entry.artifact_bytes;
}

export function entryMessages(entry: api.InventoryView): number {
  return entry.stats === null ? 0 : entry.stats.user_messages + entry.stats.assistant_turns;
}

export function entryTokens(entry: api.InventoryView): number {
  return entry.stats === null ? 0 : entry.stats.input_tokens + entry.stats.output_tokens;
}

/** Own cost plus subagent cost; null when neither is known. */
export function entryCost(entry: api.InventoryView): number | null {
  const stats = entry.stats;
  if (stats === null || (stats.cost === null && stats.subagent_cost === null)) return null;
  return (stats.cost ?? 0) + (stats.subagent_cost ?? 0);
}

export function entryActivity(entry: api.InventoryView): number {
  return entry.stats?.last_activity_unix_ms ?? entry.modified_unix_ms;
}

export function entryTitle(entry: { title: string | null; first_prompt: string | null }): string {
  return entry.title ?? entry.first_prompt ?? 'Untitled session';
}

export type InventorySort = 'activity' | 'created' | 'size' | 'cost' | 'tokens' | 'messages';

export const INVENTORY_SORTS: readonly (readonly [InventorySort, string])[] = [
  ['activity', 'Last activity'],
  ['created', 'Created'],
  ['size', 'Size'],
  ['cost', 'Cost'],
  ['tokens', 'Tokens'],
  ['messages', 'Messages'],
];

export interface InventoryFilter {
  text: string;
  /** Only sessions whose last activity is older than this many days (0 = any). */
  olderThanDays: number;
  /** Only sessions larger than this many bytes (0 = any). */
  largerThan: number;
  stubsOnly: boolean;
  missingOnly: boolean;
  notRunningOnly: boolean;
}

export const NO_FILTER: InventoryFilter = {
  text: '',
  olderThanDays: 0,
  largerThan: 0,
  stubsOnly: false,
  missingOnly: false,
  notRunningOnly: false,
};

const DAY_MS = 86_400_000;

export function matchesFilter(entry: api.InventoryView, filter: InventoryFilter, now: number): boolean {
  if (filter.stubsOnly && !entry.stub) return false;
  if (filter.missingOnly && entry.cwd_exists) return false;
  if (filter.notRunningOnly && entry.running) return false;
  if (filter.olderThanDays > 0 && now - entryActivity(entry) < filter.olderThanDays * DAY_MS) return false;
  if (filter.largerThan > 0 && entrySize(entry) <= filter.largerThan) return false;
  const needle = filter.text.trim().toLowerCase();
  if (needle.length === 0) return true;
  return [entry.title, entry.first_prompt, entry.display, entry.id, entry.stats?.model ?? null].some(
    (field) => field !== null && field.toLowerCase().includes(needle),
  );
}

function sortValue(entry: api.InventoryView, sort: InventorySort): number {
  switch (sort) {
    case 'activity':
      return entryActivity(entry);
    case 'created':
      return entry.created_unix_ms ?? entry.modified_unix_ms;
    case 'size':
      return entrySize(entry);
    case 'cost':
      return entryCost(entry) ?? -1;
    case 'tokens':
      return entryTokens(entry);
    case 'messages':
      return entryMessages(entry);
  }
}

/** Filters, then sorts descending (largest/newest first) or ascending; ties keep newest first. */
export function filterInventory(
  entries: readonly api.InventoryView[],
  filter: InventoryFilter,
  sort: InventorySort,
  ascending: boolean,
  now: number,
): api.InventoryView[] {
  const kept = entries.filter((entry) => matchesFilter(entry, filter, now));
  return kept.sort((a, b) => {
    const delta = sortValue(a, sort) - sortValue(b, sort);
    if (delta !== 0) return ascending ? delta : -delta;
    return b.modified_unix_ms - a.modified_unix_ms;
  });
}

export function activeFilters(filter: InventoryFilter): number {
  return (
    (filter.text.trim().length > 0 ? 1 : 0) +
    (filter.olderThanDays > 0 ? 1 : 0) +
    (filter.largerThan > 0 ? 1 : 0) +
    (filter.stubsOnly ? 1 : 0) +
    (filter.missingOnly ? 1 : 0) +
    (filter.notRunningOnly ? 1 : 0)
  );
}

export interface SelectionTotals {
  count: number;
  bytes: number;
  messages: number;
  tokens: number;
  /** Sum of the known costs. */
  cost: number;
  running: number;
  stubs: number;
  subagents: number;
}

export function selectionTotals(entries: readonly api.InventoryView[]): SelectionTotals {
  const totals: SelectionTotals = { count: 0, bytes: 0, messages: 0, tokens: 0, cost: 0, running: 0, stubs: 0, subagents: 0 };
  for (const entry of entries) {
    totals.count++;
    totals.bytes += entrySize(entry);
    totals.messages += entryMessages(entry);
    totals.tokens += entryTokens(entry);
    totals.cost += entryCost(entry) ?? 0;
    totals.subagents += entry.subagents;
    if (entry.running) totals.running++;
    if (entry.stub) totals.stubs++;
  }
  return totals;
}

/** The sessions forked from `id` (their `parent`). */
export function forksOf(entries: readonly api.InventoryView[], id: string): api.InventoryView[] {
  return entries.filter((entry) => entry.parent === id);
}

// ---- running ---------------------------------------------------------------------------------------

/** A running mux session no session file maps to (a bare tool, or omp before its first save). */
export function isUnmapped(entry: api.RunningView): boolean {
  return entry.sessions.length === 0;
}

/** Bytes freed by killing `entries`, and how many of them have no RAM figure. */
export function memoryFreed(entries: readonly api.RunningView[]): { bytes: number; unknown: number } {
  let total = 0;
  let unknown = 0;
  for (const entry of entries) {
    if (entry.memory_bytes === null) unknown++;
    else total += entry.memory_bytes;
  }
  return { bytes: total, unknown };
}

// ---- session tree ----------------------------------------------------------------------------------

export interface TreeRow {
  node: api.TreeNode;
  /** Indent level: grows only where the conversation branches. */
  depth: number;
  /** On the path from the root to the active leaf. */
  active: boolean;
  /** The active leaf ("you are here"). */
  here: boolean;
  /** The first row of a branch that is not the first child (draws a fork elbow). */
  branchStart: boolean;
  /** Number of children (a fork point when > 1). */
  children: number;
}

/**
 * Lays the tree's nodes out as an indented list: children follow their parent depth-first in the
 * order received; a node with several children indents each child's branch one level deeper.
 * Nodes whose parent is not in the page are treated as roots.
 */
export function layoutTree(nodes: readonly api.TreeNode[], activeLeaf: string | null): TreeRow[] {
  const byId = new Map<string, api.TreeNode>();
  for (const node of nodes) byId.set(node.id, node);
  const children = new Map<string, api.TreeNode[]>();
  const roots: api.TreeNode[] = [];
  for (const node of nodes) {
    if (node.parent !== null && byId.has(node.parent) && node.parent !== node.id) {
      const list = children.get(node.parent);
      if (list === undefined) children.set(node.parent, [node]);
      else list.push(node);
    } else {
      roots.push(node);
    }
  }
  const activePath = new Set<string>();
  let cursor = activeLeaf === null ? undefined : byId.get(activeLeaf);
  while (cursor !== undefined && !activePath.has(cursor.id)) {
    activePath.add(cursor.id);
    cursor = cursor.parent === null ? undefined : byId.get(cursor.parent);
  }
  const rows: TreeRow[] = [];
  const seen = new Set<string>();
  // Iterative depth-first walk (sessions can be thousands of entries deep).
  const stack: { node: api.TreeNode; depth: number; branchStart: boolean }[] = [];
  for (let i = roots.length - 1; i >= 0; i--) {
    const root = roots[i];
    if (root !== undefined) stack.push({ node: root, depth: 0, branchStart: i > 0 });
  }
  while (stack.length > 0) {
    const top = stack.pop();
    if (top === undefined || seen.has(top.node.id)) continue;
    seen.add(top.node.id);
    const kids = children.get(top.node.id) ?? [];
    rows.push({
      node: top.node,
      depth: top.depth,
      active: activePath.has(top.node.id),
      here: top.node.id === activeLeaf,
      branchStart: top.branchStart,
      children: kids.length,
    });
    const childDepth = kids.length > 1 ? top.depth + 1 : top.depth;
    for (let i = kids.length - 1; i >= 0; i--) {
      const kid = kids[i];
      if (kid !== undefined) stack.push({ node: kid, depth: childDepth, branchStart: kids.length > 1 && i > 0 });
    }
  }
  return rows;
}

export function searchTree(rows: readonly TreeRow[], query: string): TreeRow[] {
  const needle = query.trim().toLowerCase();
  if (needle.length === 0) return [...rows];
  return rows.filter(
    (row) => row.node.text.toLowerCase().includes(needle) || (row.node.label ?? '').toLowerCase().includes(needle),
  );
}

/** Merges a further page into the nodes already loaded (a node id appears once). */
export function mergeTreePage(existing: readonly api.TreeNode[], page: readonly api.TreeNode[]): api.TreeNode[] {
  const seen = new Set(existing.map((node) => node.id));
  return [...existing, ...page.filter((node) => !seen.has(node.id))];
}

/** Why `fork_at` would be refused for this node, or null when it can be sent. */
export function forkRefusal(node: api.TreeNode, live: api.LiveSession | null): string | null {
  if (!node.forkable) return 'Only message entries can be forked.';
  if (live !== null && live.status.state !== 'exited' && !live.live_control) {
    return 'The session runs without control, so the desktop refuses forks; restart it from OmpDeck first.';
  }
  return null;
}

/** Why `navigate` would be refused, or null when it can be sent. */
export function navigateRefusal(live: api.LiveSession | null): string | null {
  if (live === null || live.status.state === 'exited') return 'Go here needs the session running.';
  if (!live.live_control) return 'The session runs without control.';
  if (live.busy || live.status.state === 'busy') return 'Only while the session is idle.';
  if (live.status.state === 'attention') return 'omp is waiting for an answer first.';
  return null;
}

// ---- worktrees -------------------------------------------------------------------------------------

export interface WorktreeLink {
  primary: string | null;
  branch: string | null;
}

/**
 * Orders projects for the Sessions list: each linked worktree with a known main checkout follows
 * its repo (as a child); worktrees whose main checkout is not listed stay top-level.
 */
export function nestWorktrees(
  projects: readonly api.Project[],
  links: ReadonlyMap<string, WorktreeLink>,
): { project: api.Project; children: { project: api.Project; branch: string | null }[] }[] {
  const ids = new Set(projects.map((project) => project.id));
  const nested = new Map<string, { project: api.Project; branch: string | null }[]>();
  const childIds = new Set<string>();
  for (const project of projects) {
    if (!project.worktree) continue;
    const link = links.get(project.id);
    if (link?.primary == null || !ids.has(link.primary) || link.primary === project.id) continue;
    const list = nested.get(link.primary) ?? [];
    list.push({ project, branch: link.branch });
    nested.set(link.primary, list);
    childIds.add(project.id);
  }
  return projects
    .filter((project) => !childIds.has(project.id))
    .map((project) => ({ project, children: nested.get(project.id) ?? [] }));
}

/** A branch name git would accept (the gateway checks again with `git check-ref-format`). */
export function branchProblem(name: string): string | null {
  const branch = name.trim();
  if (branch.length === 0) return 'Enter a branch name.';
  if (branch.length > 200) return 'Too long.';
  // biome-ignore lint/suspicious/noControlCharactersInRegex: git refuses control characters.
  if (/[\s~^:?*[\\\x00-\x1f\x7f]/.test(branch)) return 'No spaces or ~ ^ : ? * [ \\ characters.';
  if (branch.startsWith('-') || branch.startsWith('/') || branch.endsWith('/') || branch.endsWith('.')) {
    return 'Cannot start with - or /, or end with / or .';
  }
  if (branch.includes('..') || branch.includes('//') || branch.includes('@{') || branch.endsWith('.lock') || branch === '@') {
    return 'Not a valid git branch name.';
  }
  if (branch.split('/').some((part) => part.startsWith('.'))) return 'No path part may start with a dot.';
  return null;
}

// ---- folder browser --------------------------------------------------------------------------------

export interface Crumb {
  dir: string | null;
  display: string;
}

/**
 * The breadcrumb after opening `next` from `trail` (roots = `{dir: null}` first). Opening a
 * folder already on the trail cuts back to it; opening the listing's `parent` replaces the last
 * crumb when it is not on the trail.
 */
export function pushCrumb(trail: readonly Crumb[], next: Crumb): Crumb[] {
  const at = trail.findIndex((crumb) => crumb.dir === next.dir);
  if (at >= 0) return [...trail.slice(0, at), next];
  return [...trail, next];
}

/** The short last path part of a `~` display ("~/src/omp" → "omp"; "~" stays). */
export function lastPart(display: string): string {
  const trimmed = display.replace(/[\\/]+$/, '');
  const cut = Math.max(trimmed.lastIndexOf('/'), trimmed.lastIndexOf('\\'));
  return cut < 0 || cut === trimmed.length - 1 ? trimmed || display : trimmed.slice(cut + 1);
}
