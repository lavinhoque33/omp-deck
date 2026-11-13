import type { api } from '../../remote/index.ts';

/** Abandoned tasks do not count; active work wins over an earlier pending task. */
export function progressSummary(progress: api.ProgressView): { completed: number; total: number; current: api.TodoTask | null; hasTasks: boolean } {
  let completed = 0;
  let total = 0;
  let active: api.TodoTask | null = null;
  let pending: api.TodoTask | null = null;
  let hasTasks = false;
  for (const phase of progress.phases) {
    for (const task of phase.tasks) {
      hasTasks = true;
      if (task.status !== 'abandoned') total++;
      if (task.status === 'completed') completed++;
      if (active === null && task.status === 'in_progress') active = task;
      if (pending === null && task.status === 'pending') pending = task;
    }
  }
  return { completed, total, current: active ?? pending, hasTasks };
}

export function goalStatus(status: api.GoalStatus): string {
  return status === 'budget_limited' ? 'Budget limited' : status[0]!.toUpperCase() + status.slice(1);
}
