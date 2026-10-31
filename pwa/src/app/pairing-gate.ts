// When the app has finished consuming a `#pair=` invite: only when an actual pairing flow ran to
// completion (the stored pairing changed while a flow was open), never because the stored pairing
// was merely adopted at startup or a failed flow was dismissed.

export interface InviteTracker {
  /** Whether a pairing flow was open at the last observation. */
  readonly flow: boolean;
  /** The stored pairing when that flow opened. */
  readonly base: unknown;
}

export const INITIAL_TRACKER: InviteTracker = { flow: false, base: null };

export function observePairing(
  tracker: InviteTracker,
  pairing: unknown,
  flowOpen: boolean,
): { tracker: InviteTracker; consumed: boolean } {
  if (flowOpen) return { tracker: tracker.flow ? tracker : { flow: true, base: pairing }, consumed: false };
  const consumed = tracker.flow && pairing !== null && pairing !== tracker.base;
  return { tracker: { flow: false, base: pairing }, consumed };
}
