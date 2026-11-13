import { describe, expect, test } from 'bun:test';
import type { api } from '../src/remote/index.ts';
import { goalStatus, progressSummary } from '../src/app/ui/progress-model.ts';
import { progress } from './smoke/fixtures.ts';

describe('live task summary', () => {
  test('counts completed, excludes abandoned, and prefers active over pending', () => {
    const view = progress();
    view.phases[0]!.tasks.unshift({ content: 'Earlier pending', status: 'pending' });
    expect(progressSummary(view)).toMatchObject({ completed: 3, total: 8, current: { content: 'Render progress across phone screens' }, hasTasks: true });
    expect(progressSummary(progress(false)).current?.content).toBe('Render progress across phone screens');
  });

  test('empty, abandoned-only and goal-only states have zero counted tasks', () => {
    const empty: api.ProgressView = { phases: [], goal: null, truncated: false };
    expect(progressSummary(empty)).toEqual({ completed: 0, total: 0, current: null, hasTasks: false });
    const abandoned: api.ProgressView = { ...empty, phases: [{ name: 'Dropped', tasks: [{ content: 'No longer needed', status: 'abandoned' }] }] };
    expect(progressSummary(abandoned)).toEqual({ completed: 0, total: 0, current: null, hasTasks: true });
    expect(progressSummary({ ...progress(), phases: [] })).toEqual({ completed: 0, total: 0, current: null, hasTasks: false });
  });

  test('blocked and unknown remain in the denominator but are not current work', () => {
    const view: api.ProgressView = { phases: [{ name: '', tasks: [
      { content: 'Blocked', status: 'blocked' },
      { content: 'Unknown', status: 'unknown' },
      { content: 'Done', status: 'completed' },
    ] }], goal: null, truncated: false };
    expect(progressSummary(view)).toEqual({ completed: 1, total: 3, current: null, hasTasks: true });
  });

  test('uses the first active task across phases, otherwise first pending', () => {
    const view: api.ProgressView = { phases: [
      { name: 'First', tasks: [{ content: 'First pending', status: 'pending' }] },
      { name: 'Second', tasks: [{ content: 'First active', status: 'in_progress' }, { content: 'Second active', status: 'in_progress' }] },
    ], goal: null, truncated: false };
    expect(progressSummary(view).current?.content).toBe('First active');
    view.phases[1]!.tasks = [];
    expect(progressSummary(view).current?.content).toBe('First pending');
  });
});

test('all goal statuses have readable labels', () => {
  const labels: Record<api.GoalStatus, string> = { active: 'Active', paused: 'Paused', budget_limited: 'Budget limited', complete: 'Complete', dropped: 'Dropped', unknown: 'Unknown' };
  for (const [status, label] of Object.entries(labels)) expect(goalStatus(status as api.GoalStatus)).toBe(label);
});
