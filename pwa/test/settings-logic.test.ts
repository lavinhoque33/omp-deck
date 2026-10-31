import { describe, expect, test } from 'bun:test';
import type { api } from '../src/remote/index.ts';
import {
  buildHeatmap,
  backupDestinationLabel,
  bytes,
  conflictList,
  defaultChoices,
  deviceNameProblem,
  filterGroups,
  groupSyncBackups,
  latestFor,
  parseJsonFor,
  parseNumber,
  preview,
  prettyJson,
  remainingTone,
  settingLabel,
  sortBackupHosts,
  summarizeProjectPlan,
  toggleExcluded,
  totalSize,
  untilText,
  valueText,
} from '../src/app/screens/settings/logic.ts';

describe('device name', () => {
  test('mirrors the gateway rules', () => {
    expect(deviceNameProblem('  Pixel 9  ')).toBeNull();
    expect(deviceNameProblem('   ')).not.toBeNull();
    expect(deviceNameProblem('x'.repeat(65))).not.toBeNull();
    expect(deviceNameProblem('é'.repeat(64))).toBeNull();
    expect(deviceNameProblem('Pixel\u001b[2J')).not.toBeNull();
    expect(deviceNameProblem('Pixel\nApprove')).not.toBeNull();
    expect(deviceNameProblem('Pi\u202exel')).not.toBeNull();
  });
});

describe('formatting', () => {
  test('bytes', () => {
    expect(bytes(0)).toBe('0 B');
    expect(bytes(999)).toBe('999 B');
    expect(bytes(4200)).toBe('4.2 KB');
    expect(bytes(13_400_000)).toBe('13 MB');
    expect(bytes(1_400_000_000)).toBe('1.4 GB');
  });
  test('until', () => {
    const now = 1_000_000_000_000;
    expect(untilText(now - 1, now)).toBe('now');
    expect(untilText(now + 45 * 60_000, now)).toBe('in 45m');
    expect(untilText(now + 200 * 60_000, now)).toBe('in 3h 20m');
    expect(untilText(now + 3 * 3_600_000, now)).toBe('in 3h');
    expect(untilText(now + 52 * 3_600_000, now)).toBe('in 2d 4h');
  });
  test('remaining tone', () => {
    expect(remainingTone(null)).toBe('ok');
    expect(remainingTone(0.82)).toBe('ok');
    expect(remainingTone(0.31)).toBe('warn');
    expect(remainingTone(0.07)).toBe('danger');
  });
});

describe('heatmap', () => {
  const day = (d: string, cost: number | null, requests: number): api.DayCount => ({ day: d, cost, requests, tokens: requests * 10 });

  test('levels, grid shape and future cells (the desktop test)', () => {
    // 2026-10-22 is a Thursday.
    const map = buildHeatmap([day('2026-10-22', 4, 5), day('2026-10-21', 1, 1), day('2026-10-20', 0.01, 1), day('2026-10-19', 0, 0)], '2026-10-22', 53);
    expect(map.weeks.length).toBe(53);
    const last = map.weeks[52]!;
    expect(last[3]?.level).toBe(4);
    expect(last[2]?.level).toBe(2);
    expect(last[1]?.level).toBe(1);
    expect(last[0]?.level).toBe(0);
    expect(last[4]).toBeNull();
    expect(last[6]).toBeNull();
    expect(map.byCost).toBe(true);
    expect(map.activeDays).toBe(3);
    expect(map.longestStreak).toBe(3);
    expect(map.busiest?.day).toBe('2026-10-22');
    // The first column starts on a Monday 52 weeks before this week's.
    expect(map.weeks[0]![0]?.day).toBe('2025-10-20');
  });

  test('requests backup the levels without any cost', () => {
    const map = buildHeatmap([day('2026-10-22', null, 4), day('2026-10-21', 0, 1)], '2026-10-22', 53);
    expect(map.byCost).toBe(false);
    expect(map.weeks[52]![3]?.level).toBe(4);
    expect(map.weeks[52]![2]?.level).toBe(2);
  });

  test('empty history is all level 0, days outside the grid are ignored', () => {
    const map = buildHeatmap([day('2020-01-01', 9, 9)], '2026-10-22', 53);
    expect(map.weeks.flat().every((cell) => cell === null || cell.level === 0)).toBe(true);
    expect(map.activeDays).toBe(0);
    expect(map.busiest).toBeNull();
  });

  test('month labels appear where the month changes', () => {
    const map = buildHeatmap([], '2026-10-22', 53);
    expect(map.months[0]).toBe('Oct');
    expect(map.months.filter((m) => m !== null).length).toBeGreaterThanOrEqual(12);
  });
});

describe('omp settings', () => {
  test('labels as the desktop', () => {
    expect(settingLabel('statusLine.compactThinkingLevel')).toBe('Compact thinking level');
    expect(settingLabel('simple')).toBe('Simple');
    expect(settingLabel('a.apiURLPath')).toBe('Api URL path');
    expect(settingLabel('retry_max-count')).toBe('Retry max count');
  });
  test('numbers', () => {
    expect(parseNumber(' 7 ')).toEqual({ ok: true, value: 7 });
    expect(parseNumber('0.25')).toEqual({ ok: true, value: 0.25 });
    expect(parseNumber('-3e2')).toEqual({ ok: true, value: -300 });
    expect(parseNumber('').ok).toBe(false);
    expect(parseNumber('12px').ok).toBe(false);
    expect(parseNumber('Infinity').ok).toBe(false);
    expect(parseNumber('1e999').ok).toBe(false);
  });
  test('JSON by kind', () => {
    expect(parseJsonFor({ type: 'array' }, '["a", 1]')).toEqual({ ok: true, value: ['a', 1] });
    expect(parseJsonFor({ type: 'record' }, '{"a": {"b": 1}}')).toEqual({ ok: true, value: { a: { b: 1 } } });
    expect(parseJsonFor({ type: 'array' }, '{}')).toEqual({ ok: false, error: 'Expected a JSON array ([ … ])' });
    expect(parseJsonFor({ type: 'record' }, '[]')).toEqual({ ok: false, error: 'Expected a JSON object ({ … })' });
    const bad = parseJsonFor({ type: 'record' }, '{a:');
    expect(bad.ok === false && bad.error.startsWith('Invalid JSON')).toBe(true);
  });
  test('previews and text', () => {
    expect(preview(null, 20)).toBe('empty');
    expect(preview([], 20)).toBe('[ ]');
    expect(preview({}, 20)).toBe('{ }');
    expect(preview({ a: 1 }, 20)).toBe('{"a":1}');
    expect(preview(['abcdefghij'], 6)).toBe('["abc…');
    expect(valueText(7)).toBe('7');
    expect(valueText('s')).toBe('s');
    expect(valueText(null)).toBe('');
    expect(prettyJson({ type: 'record' }, null)).toBe('{}');
    expect(prettyJson({ type: 'array' }, null)).toBe('[]');
  });
  test('search filters entries and drops empty groups', () => {
    const entry = (key: string, description = ''): api.SettingView => ({
      key,
      kind: { type: 'bool' },
      description,
      value: true,
      default: true,
      modified: false,
      secret: false,
      set: false,
    });
    const groups: api.SettingsGroup[] = [
      { id: 'a', label: 'Appearance', entries: [entry('theme.dark'), entry('statusLine.compactThinkingLevel', 'Compact thinking')] },
      { id: 'p', label: 'Providers', entries: [entry('providers.openai.apiKey', 'API key')] },
    ];
    expect(filterGroups(groups, '').length).toBe(2);
    expect(filterGroups(groups, 'thinking level').map((g) => g.entries.map((e) => e.key))).toEqual([['statusLine.compactThinkingLevel']]);
    expect(filterGroups(groups, 'providers').map((g) => g.id)).toEqual(['p']);
    expect(filterGroups(groups, 'nothing-like-this')).toEqual([]);
  });
});

describe('backups and restores', () => {
  const backup = (id: string, display: string, host: string, created: number, size = 100): api.SyncBackupView => ({
    id,
    name: `${id}.tar`,
    host,
    display,
    size,
    created_unix_ms: created,
    omp_bytes: 10,
    memory_bytes: null,
    sessions: 2,
    session_bytes: 90,
  });

  test('groups project → host → newest first', () => {
    const grouped = groupSyncBackups([
      backup('1', '~/b', 'laptop', 1),
      backup('2', '~/a', 'desk', 5),
      backup('3', '~/a', 'desk', 9),
      backup('4', '~/a', 'laptop', 3),
    ]);
    expect(grouped.map((p) => p.display)).toEqual(['~/a', '~/b']);
    expect(grouped[0]!.hosts.map((h) => h.host)).toEqual(['desk', 'laptop']);
    expect(grouped[0]!.hosts[0]!.versions.map((v) => v.id)).toEqual(['3', '2']);
    expect(grouped[0]!.count).toBe(3);
    expect(grouped[0]!.size).toBe(300);
    expect(latestFor([backup('1', '~/a', 'x', 1), backup('2', '~/a', 'y', 4)], '~/a')?.id).toBe('2');
    expect(latestFor([], '~/a')).toBeNull();
    expect(totalSize([backup('1', '~/a', 'x', 1, 5), backup('2', '~/a', 'x', 1, 7)], new Set(['2']))).toBe(7);
  });

  test('backup hosts sorted with newest versions first', () => {
    const hosts = sortBackupHosts([
      { host: 'z', backups: [{ id: 'a', name: 'a', size: 1, created_unix_ms: 1 }] },
      { host: 'm', backups: [{ id: 'b', name: 'b', size: 1, created_unix_ms: 1 }, { id: 'c', name: 'c', size: 1, created_unix_ms: 7 }] },
    ]);
    expect(hosts.map((h) => h.host)).toEqual(['m', 'z']);
    expect(hosts[0]!.versions.map((v) => v.id)).toEqual(['c', 'b']);
  });

  test('conflicts default to keep and summarize', () => {
    const session = (id: string, conflicting: boolean): api.SessionPlanView => ({
      id,
      title: id,
      modified_unix_ms: 1,
      bytes: 1,
      local: conflicting ? [{ project: 'p', display: '~/p', modified_unix_ms: 2, bytes: 3 }] : [],
      problem: null,
    });
    const plan: Extract<api.RestorePlanView, { type: 'project' }> = {
      type: 'project',
      target_display: '~/p',
      create_target: false,
      omp_exists: false,
      omp_overwritten: [],
      omp_added: ['a'],
      omp_skipped: [],
      memory_exists: false,
      memory_overwritten: [],
      memory_added: [],
      memory_skipped: [],
      sessions: [session('s1', true), session('s2', true), session('s3', false)],
    };
    const choices = defaultChoices(plan);
    expect(choices).toEqual({ s1: 'keep', s2: 'keep' });
    expect(conflictList(plan, {})).toEqual([
      { session: 's1', choice: 'keep' },
      { session: 's2', choice: 'keep' },
    ]);
    expect(summarizeProjectPlan(plan, choices).destructive).toBe(false);
    const changed = { ...choices, s2: 'overwrite' as const };
    const summary = summarizeProjectPlan(plan, changed);
    expect(summary).toMatchObject({ conflicts: 2, overwrite: 1, keep: 1, both: 0, sessionsAdded: 1, added: 1, destructive: true });
    const excluded = ['s2', 's3'];
    expect(conflictList(plan, changed, excluded)).toEqual([{ session: 's1', choice: 'keep' }]);
    expect(summarizeProjectPlan(plan, changed, excluded)).toMatchObject({ overwrite: 0, sessionsAdded: 0, writable: true });
    const sessionsOnly = { ...plan, omp_added: [] };
    expect(summarizeProjectPlan(sessionsOnly, changed, excluded).writable).toBe(false);
    expect(summarizeProjectPlan(sessionsOnly, changed).writable).toBe(true);
    expect(summarizeProjectPlan({ ...sessionsOnly, sessions: [{ ...session('broken', false), problem: 'unreadable' }] }, {}).writable).toBe(false);
    expect(summarizeProjectPlan({ ...plan, omp_exists: true }, choices).destructive).toBe(true);
  });

  test('backup labels follow the active destination', () => {
    expect(backupDestinationLabel({ kind: 'drive', label: 'Google Drive' })).toBe('Google Drive');
    expect(backupDestinationLabel({ kind: 'ftp', label: 'ftp://backup.local' })).toBe('FTP server ftp://backup.local');
    expect(backupDestinationLabel({ kind: 'ftps', label: 'ftps://secure.local' })).toBe('FTPS server ftps://secure.local');
    expect(backupDestinationLabel({ kind: 'sftp', label: 'sftp://host:22' })).toBe('SFTP server sftp://host:22');
  });

  test('session opt-out toggles', () => {
    expect(toggleExcluded([], 'a')).toEqual(['a']);
    expect(toggleExcluded(['a', 'b'], 'a')).toEqual(['b']);
  });
});
