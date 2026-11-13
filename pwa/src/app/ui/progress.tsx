import type { ReactNode } from 'react';
import type { api } from '../../remote/index.ts';
import { uptime } from '../screens/fleet/logic.ts';
import { tokens } from './format.ts';
import { KeyValues, Sheet } from './kit.tsx';
import { goalStatus, progressSummary } from './progress-model.ts';
import '../../styles/progress.css';


export function ProgressCount({ progress }: { progress: api.ProgressView | null | undefined }): ReactNode {
  if (progress == null) return null;
  const summary = progressSummary(progress);
  if (!summary.hasTasks) return null;
  return <span className="progress-count" aria-label={`${summary.completed} of ${summary.total} tasks completed`}>{summary.completed}/{summary.total}</span>;
}

export function ProgressPill({ progress, onClick }: { progress: api.ProgressView; onClick: () => void }): ReactNode {
  const summary = progressSummary(progress);
  if (!summary.hasTasks && progress.goal === null) return null;
  return (
    <button type="button" className="pill progress-pill" aria-label="Session progress" onClick={onClick}>
      <span aria-hidden="true">☷</span> {summary.hasTasks ? `${summary.completed}/${summary.total}` : goalStatus(progress.goal!.status)}
    </button>
  );
}

export function ProgressLine({ progress }: { progress: api.ProgressView | null | undefined }): ReactNode {
  if (progress == null) return null;
  const summary = progressSummary(progress);
  if (!summary.hasTasks && progress.goal === null) return null;
  return (
    <div className="progress-summary">
      {summary.hasTasks && <div className="progress-line">
        <span className="progress-track" role="progressbar" aria-label="Tasks completed" aria-valuemin={0} aria-valuemax={summary.total} aria-valuenow={summary.completed}>
          <i style={{ width: `${summary.total === 0 ? 0 : summary.completed / summary.total * 100}%` }} />
        </span>
        <ProgressCount progress={progress} />
        {summary.current !== null && <span className="progress-current" title={summary.current.content}>{summary.current.content}</span>}
      </div>}
      {progress.goal !== null && <p className="progress-goal"><span className={`chip${progress.goal.status === 'budget_limited' ? ' warn' : ' accent'}`}>{goalStatus(progress.goal.status)}</span> {progress.goal.objective}</p>}
      {progress.truncated && <small className="muted">More progress omitted</small>}
    </div>
  );
}

const TASK_ICONS: Record<api.TodoStatus, string> = { pending: '○', in_progress: '◉', completed: '✓', abandoned: '−', blocked: '⚠', unknown: '?' };
const TASK_LABELS: Record<api.TodoStatus, string> = { pending: 'Pending', in_progress: 'In progress', completed: 'Completed', abandoned: 'Abandoned', blocked: 'Blocked', unknown: 'Unknown' };

export function ProgressSheet({ progress, onClose }: { progress: api.ProgressView; onClose: () => void }): ReactNode {
  const goal = progress.goal;
  return (
    <Sheet title="Progress" icon="☷" onClose={onClose}>
      <div className="progress-details">
        {goal !== null && <section className="progress-objective" aria-label="Goal">
          <h4>Goal</h4>
          <p>{goal.objective}</p>
          <KeyValues rows={[
            ['Status', <span className={`chip${goal.status === 'budget_limited' ? ' warn' : ' accent'}`}>{goalStatus(goal.status)}</span>],
            ['Tokens', <span title={`${goal.tokens_used.toLocaleString()}${goal.token_budget === null ? '' : ` / ${goal.token_budget.toLocaleString()}`}`}>{tokens(goal.tokens_used)} / {goal.token_budget === null ? 'No budget' : tokens(goal.token_budget)}</span>],
            ['Elapsed', uptime(goal.time_used_seconds * 1000)],
          ]} />
        </section>}
        <div className="line spread progress-tasks-heading"><h4>Tasks</h4><ProgressCount progress={progress} /></div>
        {progress.phases.map((phase, index) => <section className="progress-phase" key={index}>
          <h5>{phase.name || 'Tasks'}</h5>
          <ul>{phase.tasks.map((task, taskIndex) => <li className={`progress-task ${task.status}`} key={taskIndex}>
            <span className="progress-task-icon" aria-label={TASK_LABELS[task.status]} title={TASK_LABELS[task.status]}>{TASK_ICONS[task.status]}</span>
            <span>{task.content}</span>
          </li>)}</ul>
        </section>)}
        {!progress.phases.some((phase) => phase.tasks.length > 0) && <p className="muted small">No tasks yet.</p>}
        {progress.truncated && <p className="progress-truncated" role="note">More progress omitted — this view is truncated.</p>}
      </div>
    </Sheet>
  );
}
