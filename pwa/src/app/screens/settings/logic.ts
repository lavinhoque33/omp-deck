// Pure helpers of the settings, backup, usage and notification screens: validation, formatting,
// the activity heatmap layout, the omp settings editors' parsing, and restore-plan bookkeeping.

import { isDisplaySafe } from '../../../remote/index.ts';
import type { api } from '../../../remote/index.ts';

// ---- device -------------------------------------------------------------------------------------

/** Characters of a device name at most (`noise::MAX_NAME_CHARS`). */
export const MAX_NAME_CHARS = 64;

/** Why `name` would be refused as a device name (the gateway's `valid_name`), or null. */
export function deviceNameProblem(name: string): string | null {
  const trimmed = name.trim();
  if (trimmed.length === 0) return 'Give the phone a name.';
  if ([...trimmed].length > MAX_NAME_CHARS) return `A device name has at most ${MAX_NAME_CHARS} characters.`;
  if (!isDisplaySafe(trimmed)) return 'Remove control or invisible characters.';
  return null;
}

export const MODE_LABEL: Record<api.GatewayMode, string> = {
  off: 'Off',
  while_open: 'While OmpDeck is open',
  background: 'Always on (background)',
};

// ---- formatting ---------------------------------------------------------------------------------

/** "0 B", "812 B", "4.2 KB", "13 MB", "1.4 GB" (decimal units, as the desktop). */
export function bytes(count: number): string {
  if (count < 1000) return `${count} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let value = count / 1000;
  let unit = 0;
  while (value >= 1000 && unit < units.length - 1) {
    value /= 1000;
    unit += 1;
  }
  return `${value.toFixed(value < 10 ? 1 : 0)} ${units[unit]}`;
}

export const INTERVAL_LABEL: Record<api.SyncInterval, string> = {
  off: 'Off',
  ten_minutes: 'Every 10 minutes',
  thirty_minutes: 'Every 30 minutes',
  hourly: 'Hourly',
  six_hours: 'Every 6 hours',
  daily: 'Daily',
};

/** "in 45m", "in 3h 20m", "in 2d 4h"; "now" once passed. */
export function untilText(unixMs: number, now: number): string {
  const minutes = Math.round((unixMs - now) / 60_000);
  if (minutes <= 0) return 'now';
  if (minutes < 60) return `in ${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return minutes % 60 === 0 ? `in ${hours}h` : `in ${hours}h ${minutes % 60}m`;
  const days = Math.floor(hours / 24);
  return hours % 24 === 0 ? `in ${days}d` : `in ${days}d ${hours % 24}h`;
}

/** A date and time in the phone's locale ("—" when unknown). */
export function when(unixMs: number | null): string {
  if (unixMs === null) return '—';
  return new Date(unixMs).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' });
}

/** The tone of a remaining quota fraction: under 15 % is danger, under 40 % a warning. */
export function remainingTone(fraction: number | null): 'ok' | 'warn' | 'danger' {
  if (fraction === null) return 'ok';
  if (fraction < 0.15) return 'danger';
  if (fraction < 0.4) return 'warn';
  return 'ok';
}

export function percent(fraction: number): string {
  return `${Math.round(Math.max(0, Math.min(1, fraction)) * 100)}%`;
}

// ---- activity heatmap ---------------------------------------------------------------------------

/** Weeks the heatmap shows (omp's `/usage` layout). */
export const HEATMAP_WEEKS = 53;
/** Days `usage_activity` is asked for: enough to fill {@link HEATMAP_WEEKS} weeks. */
export const ACTIVITY_DAYS = 371;

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
const DAY_MS = 86_400_000;

export interface HeatCell {
  day: string;
  /** 0 (quiet) to 4. */
  level: number;
  cost: number;
  requests: number;
  tokens: number;
}

export interface Heatmap {
  /** `weeks[w][row]`, row 0 = Monday … 6 = Sunday; null after today. */
  weeks: (HeatCell | null)[][];
  /** Per week: the month's abbreviation where it differs from the previous week's. */
  months: (string | null)[];
  /** Levels by cost when any day has a cost, else by request count. */
  byCost: boolean;
  totalCost: number;
  totalRequests: number;
  totalTokens: number;
  activeDays: number;
  longestStreak: number;
  busiest: HeatCell | null;
}

/** `YYYY-MM-DD` of a local date. */
export function localDay(date: Date): string {
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
}

function dayNumber(day: string): number {
  const [y, m, d] = day.split('-').map(Number);
  return Math.floor(Date.UTC(y ?? 1970, (m ?? 1) - 1, d ?? 1) / DAY_MS);
}

function dayString(number: number): string {
  return new Date(number * DAY_MS).toISOString().slice(0, 10);
}

/**
 * The desktop's `Heatmap::build` (omp's `buildHeatmapLayout`): 7 × `weeks` cells ending in
 * `today`'s week, intensity `ceil(sqrt(v / max) · 4)`.
 */
export function buildHeatmap(days: readonly api.DayCount[], today: string, weeks = HEATMAP_WEEKS): Heatmap {
  const count = Math.max(1, weeks);
  const todayN = dayNumber(today);
  // 1970-01-01 was a Thursday (Monday-based weekday 3).
  const weekday = (((todayN + 3) % 7) + 7) % 7;
  const start = todayN - weekday - (count - 1) * 7;
  const byDay = new Map<number, api.DayCount>();
  for (const entry of days) {
    const n = dayNumber(entry.day);
    if (n >= start && n <= todayN) byDay.set(n, entry);
  }
  const inGrid = [...byDay.entries()].sort((a, b) => a[0] - b[0]);
  const byCost = inGrid.some(([, entry]) => (entry.cost ?? 0) > 0);
  const value = (entry: api.DayCount | undefined) => (entry === undefined ? 0 : byCost ? (entry.cost ?? 0) : entry.requests);
  const max = inGrid.reduce((best, [, entry]) => Math.max(best, value(entry)), 0);
  const level = (v: number) => (v <= 0 || max <= 0 ? 0 : Math.min(4, Math.max(1, Math.ceil(Math.sqrt(v / max) * 4))));

  const grid: (HeatCell | null)[][] = [];
  const months: (string | null)[] = [];
  let previousMonth = -1;
  for (let w = 0; w < count; w += 1) {
    const monday = start + w * 7;
    const month = new Date(monday * DAY_MS).getUTCMonth();
    months.push(month === previousMonth ? null : (MONTHS[month] ?? null));
    previousMonth = month;
    const column: (HeatCell | null)[] = [];
    for (let row = 0; row < 7; row += 1) {
      const n = monday + row;
      if (n > todayN) {
        column.push(null);
        continue;
      }
      const entry = byDay.get(n);
      column.push({
        day: dayString(n),
        level: level(value(entry)),
        cost: entry?.cost ?? 0,
        requests: entry?.requests ?? 0,
        tokens: entry?.tokens ?? 0,
      });
    }
    grid.push(column);
  }

  let totalCost = 0;
  let totalRequests = 0;
  let totalTokens = 0;
  let activeDays = 0;
  let streak = 0;
  let longestStreak = 0;
  let previousActive: number | null = null;
  let busiest: HeatCell | null = null;
  for (const [n, entry] of inGrid) {
    totalCost += entry.cost ?? 0;
    totalRequests += entry.requests;
    totalTokens += entry.tokens;
    if ((entry.cost ?? 0) > 0 || entry.requests > 0) {
      activeDays += 1;
      streak = previousActive === n - 1 ? streak + 1 : 1;
      longestStreak = Math.max(longestStreak, streak);
      previousActive = n;
    }
    const v = value(entry);
    if (v > 0 && (busiest === null || v > (byCost ? busiest.cost : busiest.requests))) {
      busiest = { day: dayString(n), level: 4, cost: entry.cost ?? 0, requests: entry.requests, tokens: entry.tokens };
    }
  }
  return { weeks: grid, months, byCost, totalCost, totalRequests, totalTokens, activeDays, longestStreak, busiest };
}

// ---- omp settings -------------------------------------------------------------------------------

/** The desktop's label for a key: its last segment humanized ("statusLine.compactThinkingLevel" → "Compact thinking level"). */
export function settingLabel(key: string): string {
  const segment = key.slice(key.lastIndexOf('.') + 1);
  const chars = [...segment];
  const words: string[] = [];
  let current = '';
  const isUpper = (c: string) => c !== c.toLowerCase() && c === c.toUpperCase();
  const isLower = (c: string) => c !== c.toUpperCase() && c === c.toLowerCase();
  chars.forEach((c, i) => {
    if (c === '_' || c === '-') {
      if (current.length > 0) words.push(current);
      current = '';
      return;
    }
    const prev = chars[i - 1];
    const next = chars[i + 1];
    const boundary =
      isUpper(c) &&
      prev !== undefined &&
      (isLower(prev) || /[0-9]/.test(prev) || (isUpper(prev) && next !== undefined && isLower(next)));
    if (boundary && current.length > 0) {
      words.push(current);
      current = '';
    }
    current += c;
  });
  if (current.length > 0) words.push(current);
  return words
    .map((word, i) => {
      const acronym = [...word].length > 1 && ![...word].some(isLower);
      if (acronym) return word;
      if (i === 0) return word.charAt(0).toUpperCase() + word.slice(1).toLowerCase();
      return word.toLowerCase();
    })
    .join(' ');
}

export type Parsed = { ok: true; value: api.JsonValue } | { ok: false; error: string };

/** A number field's text → a JSON number (integers stay integers). */
export function parseNumber(text: string): Parsed {
  const trimmed = text.trim();
  if (trimmed.length === 0) return { ok: false, error: 'Enter a number' };
  if (!/^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$/.test(trimmed)) return { ok: false, error: 'Not a valid number' };
  const value = Number(trimmed);
  if (!Number.isFinite(value)) return { ok: false, error: 'Number must be finite' };
  return { ok: true, value };
}

/** JSON editor text → a value, which must be an array (`array`) or an object (`record`). */
export function parseJsonFor(kind: api.SettingKind, text: string): Parsed {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch (error) {
    return { ok: false, error: `Invalid JSON: ${error instanceof Error ? error.message : String(error)}` };
  }
  const isArray = Array.isArray(value);
  const isObject = typeof value === 'object' && value !== null && !isArray;
  if (kind.type === 'array') return isArray ? { ok: true, value: value as api.JsonValue } : { ok: false, error: 'Expected a JSON array ([ … ])' };
  if (kind.type === 'record') return isObject ? { ok: true, value: value as api.JsonValue } : { ok: false, error: 'Expected a JSON object ({ … })' };
  return { ok: false, error: 'This setting does not take JSON' };
}

/** Text of a number/string field for the stored value. */
export function valueText(value: api.JsonValue): string {
  if (value === null) return '';
  if (typeof value === 'string') return value;
  return JSON.stringify(value);
}

/** One-line compact JSON, cut to `max` characters. */
export function preview(value: api.JsonValue, max: number): string {
  if (value === null) return 'empty';
  if (Array.isArray(value) && value.length === 0) return '[ ]';
  if (typeof value === 'object' && !Array.isArray(value) && Object.keys(value).length === 0) return '{ }';
  const text = JSON.stringify(value);
  const chars = [...text];
  return chars.length <= max ? text : `${chars.slice(0, Math.max(0, max - 1)).join('')}…`;
}

/** Pretty JSON for the editor sheet (an empty array/object when unset). */
export function prettyJson(kind: api.SettingKind, value: api.JsonValue): string {
  const fallback: api.JsonValue = kind.type === 'record' ? {} : [];
  return JSON.stringify(value ?? fallback, null, 2);
}

/** The groups whose entries match `query` (key, label, description or group label), empty groups dropped. */
export function filterGroups(groups: readonly api.SettingsGroup[], query: string): api.SettingsGroup[] {
  const words = query.toLowerCase().split(/\s+/).filter((word) => word.length > 0);
  if (words.length === 0) return [...groups];
  const out: api.SettingsGroup[] = [];
  for (const group of groups) {
    const entries = group.entries.filter((entry) => {
      const haystack = `${entry.key} ${settingLabel(entry.key)} ${entry.description} ${group.label}`.toLowerCase();
      return words.every((word) => haystack.includes(word));
    });
    if (entries.length > 0) out.push({ ...group, entries });
  }
  return out;
}

// ---- backups and restores -----------------------------------------------------------------------

export interface HostVersions<T> {
  host: string;
  versions: T[];
}

export interface ProjectBackups {
  /** The `~` display the backups were taken from. */
  display: string;
  hosts: HostVersions<api.SyncBackupView>[];
  /** Bytes of every version. */
  size: number;
  count: number;
}

const newestFirst = (a: { created_unix_ms: number | null }, b: { created_unix_ms: number | null }) =>
  (b.created_unix_ms ?? 0) - (a.created_unix_ms ?? 0);

/** Project backups grouped project → host → versions (newest first); projects and hosts by name. */
export function groupSyncBackups(backups: readonly api.SyncBackupView[]): ProjectBackups[] {
  const projects = new Map<string, Map<string, api.SyncBackupView[]>>();
  for (const backup of backups) {
    const hosts = projects.get(backup.display) ?? new Map<string, api.SyncBackupView[]>();
    projects.set(backup.display, hosts);
    const list = hosts.get(backup.host) ?? [];
    hosts.set(backup.host, list);
    list.push(backup);
  }
  return [...projects.entries()]
    .sort((a, b) => a[0].localeCompare(b[0]))
    .map(([display, hosts]) => {
      const grouped = [...hosts.entries()]
        .sort((a, b) => a[0].localeCompare(b[0]))
        .map(([host, versions]) => ({ host, versions: [...versions].sort(newestFirst) }));
      const all = grouped.flatMap((entry) => entry.versions);
      return { display, hosts: grouped, size: all.reduce((sum, b) => sum + b.size, 0), count: all.length };
    });
}

/** Drive hosts with their versions newest first. */
export function sortDriveHosts(hosts: readonly api.DriveHost[]): HostVersions<api.DriveBackupView>[] {
  return [...hosts]
    .sort((a, b) => a.host.localeCompare(b.host))
    .map((host) => ({ host: host.host, versions: [...host.backups].sort(newestFirst) }));
}

/** The parts a project backup holds (a part it lacks reports no size). */
export function backupParts(backup: api.SyncBackupView): api.SyncParts {
  return { omp: backup.omp_bytes !== null, sessions: backup.sessions !== null, memory: backup.memory_bytes !== null };
}

/** The newest backup of a display among `backups`, or null. */
export function latestFor(backups: readonly api.SyncBackupView[], display: string): api.SyncBackupView | null {
  let best: api.SyncBackupView | null = null;
  for (const backup of backups) {
    if (backup.display !== display) continue;
    if (best === null || (backup.created_unix_ms ?? 0) > (best.created_unix_ms ?? 0)) best = backup;
  }
  return best;
}

/** Sizes of `ids` among `items` (for confirmation facts). */
export function totalSize(items: readonly { id: string; size: number }[], ids: ReadonlySet<string>): number {
  return items.reduce((sum, item) => (ids.has(item.id) ? sum + item.size : sum), 0);
}

/** Whether a planned session collides with local copies. */
export function isConflict(session: api.SessionPlanView): boolean {
  return session.local.length > 0;
}

/** The default per-session choices: keep the local copy for every conflict. */
export function defaultChoices(plan: Extract<api.RestorePlanView, { type: 'project' }>): Record<string, api.ConflictResolution> {
  const out: Record<string, api.ConflictResolution> = {};
  for (const session of plan.sessions) if (isConflict(session)) out[session.id] = 'keep';
  return out;
}

/** The `sync_restore_apply` conflict list: one choice per conflicting session, keep when unset. */
export function conflictList(
  plan: Extract<api.RestorePlanView, { type: 'project' }>,
  choices: Readonly<Record<string, api.ConflictResolution>>,
): api.ConflictChoice[] {
  return plan.sessions.filter(isConflict).map((session) => ({ session: session.id, choice: choices[session.id] ?? 'keep' }));
}

export interface ProjectPlanSummary {
  /** Files overwritten in `.omp` and the memory (both existing). */
  overwritten: number;
  added: number;
  sessionsAdded: number;
  conflicts: number;
  overwrite: number;
  keep: number;
  both: number;
  /** Whether applying overwrites anything (shows the overwrite warning). */
  destructive: boolean;
}

export function summarizeProjectPlan(
  plan: Extract<api.RestorePlanView, { type: 'project' }>,
  choices: Readonly<Record<string, api.ConflictResolution>>,
): ProjectPlanSummary {
  const list = conflictList(plan, choices);
  const overwrite = list.filter((c) => c.choice === 'overwrite').length;
  const overwritten = plan.omp_overwritten.length + plan.memory_overwritten.length;
  return {
    overwritten,
    added: plan.omp_added.length + plan.memory_added.length,
    sessionsAdded: plan.sessions.filter((s) => !isConflict(s) && s.problem === null).length,
    conflicts: list.length,
    overwrite,
    keep: list.filter((c) => c.choice === 'keep').length,
    both: list.filter((c) => c.choice === 'both').length,
    destructive: overwritten > 0 || overwrite > 0 || plan.omp_exists || plan.memory_exists,
  };
}

/** Sessions to exclude after toggling `session` in a row's opt-out list. */
export function toggleExcluded(excluded: readonly string[], session: string): string[] {
  return excluded.includes(session) ? excluded.filter((id) => id !== session) : [...excluded, session];
}
