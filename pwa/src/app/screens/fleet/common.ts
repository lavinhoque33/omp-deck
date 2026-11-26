// Small shared fleet helpers that need the client (pure logic lives in logic.ts).

import type { api } from '../../../remote/index.ts';
import { type Client, ClientError, type RequestView, describeApiError } from '../../client.ts';
import { perform, succeeded } from '../../ui/format.ts';

/** A readable sentence for a failed view request. */
export function errorText(error: unknown): string {
  if (error instanceof ClientError) return error.api === null ? error.message : describeApiError(error.api);
  return error instanceof Error ? error.message : String(error);
}

/** The final `DoneView` of a finished journaled request, if it succeeded with one. */
export function doneResult(view: RequestView | null): api.DoneView | null {
  return view?.outcome?.state === 'done' ? view.outcome.result : null;
}

/** A fork is still usable as a stopped session if the new process cannot start. */
export async function resumeFork(
  client: Client,
  session: string | null,
  onStarting: () => void,
): Promise<{ session: string; liveKey: string | null } | null> {
  if (session === null) return null;
  const op = { op: 'resume', session } as const;
  if (!client.canSend(op)) {
    client.notify(`Could not start the fork: ${client.online ? 'this phone is not allowed to resume sessions.' : 'the desktop is offline.'}`, 'error');
    return { session, liveKey: null };
  }
  onStarting();
  const view = await perform(client, op, 'Start the fork');
  const result = doneResult(view);
  if (result?.type === 'session') return { session, liveKey: result.live.key };
  if (succeeded(view)) client.notify('Could not start the fork: the desktop did not return a live session.', 'error');
  return { session, liveKey: null };
}
