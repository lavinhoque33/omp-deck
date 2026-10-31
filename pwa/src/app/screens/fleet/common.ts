// Small shared fleet helpers that need the client (pure logic lives in logic.ts).

import type { api } from '../../../remote/index.ts';
import { ClientError, type RequestView, describeApiError } from '../../client.ts';

/** A readable sentence for a failed view request. */
export function errorText(error: unknown): string {
  if (error instanceof ClientError) return error.api === null ? error.message : describeApiError(error.api);
  return error instanceof Error ? error.message : String(error);
}

/** The final `DoneView` of a finished journaled request, if it succeeded with one. */
export function doneResult(view: RequestView | null): api.DoneView | null {
  return view?.outcome?.state === 'done' ? view.outcome.result : null;
}
