// Display helpers: ages, sizes, token counts, and readable request outcomes.

import type { api } from '../../remote/index.ts';
import {
  type Client,
  ClientError,
  type RequestView,
  describeApiError,
  describeRejection,
} from '../client.ts';

/** "now", "4m", "3h", "2d", "5w" (as the mockups' session rows). */
export function age(unixMs: number, now: number): string {
  const seconds = Math.max(0, Math.round((now - unixMs) / 1000));
  if (seconds < 60) return 'now';
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours}h`;
  const days = Math.round(hours / 24);
  if (days < 14) return `${days}d`;
  return `${Math.round(days / 7)}w`;
}

export function tokens(count: number): string {
  if (count < 1000) return String(count);
  if (count < 1_000_000) return `${(count / 1000).toFixed(count < 10_000 ? 1 : 0)}K`;
  return `${(count / 1_000_000).toFixed(1)}M`;
}

export function dollars(cost: number): string {
  return `$${cost.toFixed(cost < 10 ? 2 : 1)}`;
}

/** The short model name ("anthropic/claude-opus-4-5" → "claude-opus-4-5"). */
export function shortModel(model: string | null): string {
  if (model === null) return 'model unknown';
  const slash = model.lastIndexOf('/');
  return slash < 0 ? model : model.slice(slash + 1);
}

/** A live session's state; "Starting" while omp has not drawn its title (it takes no prompt yet). */
export function statusLabel(live: Pick<api.LiveSession, 'status' | 'activity'>): string {
  const status = live.status;
  switch (status.state) {
    case 'idle':
      return live.activity === 'unknown' ? 'Starting' : 'Idle';
    case 'busy':
      return 'Working';
    case 'attention':
      return 'Needs input';
    case 'exited':
      return status.clean ? 'Exited' : 'Crashed';
  }
}

export const UNOBSERVED_TEXT = 'Sent, but the desktop could not confirm omp received it — check the session before retrying.';

export function describeDelivery(delivery: api.DeliveryView): string | null {
  switch (delivery.state) {
    case 'refused':
      return delivery.reason === 'dialog_open'
        ? 'A dialog is open in omp; answer it first.'
        : delivery.reason === 'no_bracketed_paste'
          ? 'The terminal does not accept pasted text right now.'
          : 'omp was not ready for it.';
    case 'declined':
      return 'Declined.';
    case 'aborted':
      return 'Aborted before it was sent.';
    case 'stalled':
      return 'omp did not take it in time.';
    case 'cancelled':
      return `Cancelled (${delivery.reason.replace('_', ' ')}).`;
    case 'unobserved':
      return UNOBSERVED_TEXT;
    default:
      return null;
  }
}

/** A sentence for a finished request, or null when it simply succeeded. */
export function describeResult(view: RequestView): { text: string; tone: 'info' | 'warn' | 'error' } | null {
  switch (view.status) {
    case 'done': {
      const result = view.outcome?.state === 'done' ? view.outcome.result : null;
      if (result?.type === 'submitted') {
        const problem = describeDelivery(result.delivery);
        return problem === null ? null : { text: `${view.label}: ${problem}`, tone: 'warn' };
      }
      if (result?.type === 'deleted') {
        const failed = result.results.filter((entry) => entry.error !== null);
        if (failed.length > 0) {
          return { text: `${failed.length} of ${result.results.length} could not be deleted: ${failed[0]?.error ?? ''}`, tone: 'error' };
        }
      }
      if (result?.type === 'answered' && !result.applied) return { text: `${view.label}: it was already answered.`, tone: 'warn' };
      return null;
    }
    case 'failed':
      return { text: `${view.label} failed: ${view.outcome?.state === 'failed' ? view.outcome.message : 'unknown error'}`, tone: 'error' };
    case 'rejected':
      return {
        text: `${view.label}: ${view.outcome?.state === 'rejected' ? describeRejection(view.outcome.reason) : 'refused'}`,
        tone: 'error',
      };
    case 'error':
      return { text: `${view.label}: ${view.error === null ? 'refused' : describeApiError(view.error)}`, tone: 'error' };
    case 'cancelled':
      return { text: `${view.label} was cancelled.`, tone: 'warn' };
    case 'uncertain':
      return { text: `${view.label}: the desktop could not tell whether it took effect. Check before retrying.`, tone: 'warn' };
    case 'recorded':
      return view.outcome?.state === 'recorded' && view.outcome.succeeded
        ? null
        : { text: `${view.label} did not succeed.`, tone: 'error' };
    case 'unknown':
      return { text: `${view.label} never reached the desktop. Resend it from Inbox if still wanted.`, tone: 'warn' };
    case 'expired':
      return { text: `${view.label}: its result is no longer known.`, tone: 'warn' };
    default:
      return null;
  }
}

export function requestStatusText(view: RequestView): string {
  switch (view.status) {
    case 'sending':
      return 'Sending';
    case 'accepted':
      return 'Accepted';
    case 'running':
      return view.progress === null
        ? 'Running'
        : `${view.progress.stage} ${view.progress.done}${view.progress.total === null ? '' : `/${view.progress.total}`}`;
    case 'done':
      return 'Done';
    case 'failed':
      return 'Failed';
    case 'rejected':
      return 'Refused';
    case 'cancelled':
      return 'Cancelled';
    case 'uncertain':
      return 'Uncertain';
    case 'recorded':
      return 'Recorded';
    case 'unknown':
      return 'Not delivered';
    case 'expired':
      return 'Expired';
    case 'error':
      return 'Refused';
  }
}

/**
 * Sends a journaled mutation and reports its result as a notice. Resolves the final view, or
 * null when it could not be sent (offline, not permitted).
 */
export async function perform(client: Client, op: api.Op, label: string, success?: string): Promise<RequestView | null> {
  try {
    const view = await client.mutate(op, label);
    const problem = describeResult(view);
    if (problem !== null) client.notify(problem.text, problem.tone);
    else if (success !== undefined) client.notify(success);
    return view;
  } catch (error) {
    client.notify(error instanceof ClientError || error instanceof Error ? error.message : String(error), 'error');
    return null;
  }
}

/** Whether a finished request succeeded. */
export function succeeded(view: RequestView | null): boolean {
  if (view === null) return false;
  if (view.status === 'done') return describeResult(view) === null;
  return view.status === 'recorded' && view.outcome?.state === 'recorded' && view.outcome.succeeded;
}
