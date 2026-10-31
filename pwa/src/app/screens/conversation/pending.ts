// Pure logic of omp's pending UI requests (`LiveSession.pending_ui`, mobile-pwa.md "Structured UI
// requests"): the answer each editor builds, whether it is complete, the timeout countdown, and
// which requests left without this phone answering them ("answered on desktop").

import type { api } from '../../../remote/index.ts';

/** One question's draft in an `ask` editor. */
export interface AskDraft {
  selected: readonly number[];
  custom: string;
}

/** The starting draft of an `ask` question: its recommended option, if any. */
export function initialAskDraft(question: api.UiQuestion): AskDraft {
  const recommended = question.recommended;
  const valid = recommended !== null && recommended >= 0 && recommended < question.options.length;
  return { selected: valid ? [recommended] : [], custom: '' };
}

/** Toggles option `index` of a question: single choice replaces, multi choice adds/removes. */
export function toggleAsk(question: api.UiQuestion, draft: AskDraft, index: number): AskDraft {
  if (index < 0 || index >= question.options.length) return draft;
  if (!question.multi) {
    return { ...draft, selected: draft.selected.length === 1 && draft.selected[0] === index ? [] : [index] };
  }
  const selected = draft.selected.includes(index)
    ? draft.selected.filter((entry) => entry !== index)
    : [...draft.selected, index].sort((a, b) => a - b);
  return { ...draft, selected };
}

/** A question is answered by at least one option or a non-blank own text. */
export function askAnswered(draft: AskDraft | undefined): boolean {
  return draft !== undefined && (draft.selected.length > 0 || draft.custom.trim().length > 0);
}

/** The `ask` answer for every question; null while one is still unanswered. */
export function askAnswer(
  questions: readonly api.UiQuestion[],
  drafts: ReadonlyMap<string, AskDraft>,
): Extract<api.UiAnswer, { type: 'ask' }> | null {
  const answers: api.UiAskAnswer[] = [];
  for (const question of questions) {
    const draft = drafts.get(question.id);
    if (draft === undefined || !askAnswered(draft)) return null;
    const custom = draft.custom.trim();
    const selected = draft.selected.filter((index) => index >= 0 && index < question.options.length);
    if (!question.multi && selected.length > 1) return null;
    answers.push({ id: question.id, selected: [...selected], custom: custom.length > 0 ? custom : null });
  }
  return { type: 'ask', answers };
}

/**
 * The answer to a `select`. A single select answers its option; omp's checkbox (multi) select
 * toggles exactly one option per answer and asks again, so it answers one index at a time.
 */
export function selectAnswer(request: api.UiRequestView, index: number): api.UiAnswer | null {
  if (index < 0 || index >= request.options.length) return null;
  return request.multi ? { type: 'choices', indices: [index] } : { type: 'choice', index };
}

/** The text answer of an `input` (single line: newlines become spaces) or `editor`. */
export function textAnswer(kind: api.UiKind, text: string): Extract<api.UiAnswer, { type: 'text' }> {
  return { type: 'text', text: kind === 'input' ? text.replace(/\r?\n/g, ' ') : text };
}

/** The approval decisions a request offers, in display order. */
export function approveChoices(
  view: api.ApproveView,
): { label: string; decision: api.ApproveDecision; tone: 'primary' | 'plain' | 'danger' }[] {
  const out: { label: string; decision: api.ApproveDecision; tone: 'primary' | 'plain' | 'danger' }[] = [
    { label: 'Allow once', decision: { type: 'once' }, tone: 'primary' },
  ];
  if (view.session_label !== null) out.push({ label: view.session_label, decision: { type: 'session' }, tone: 'plain' });
  view.grants.forEach((grant, index) => {
    out.push({ label: grant.label, decision: { type: 'grant', index, always: false }, tone: 'plain' });
    out.push({ label: grant.always_label, decision: { type: 'grant', index, always: true }, tone: 'plain' });
  });
  out.push({ label: 'Deny', decision: { type: 'deny' }, tone: 'danger' });
  return out;
}

/** A label for the answer (the journaled request's label). */
export function answerLabel(request: api.UiRequestView, answer: api.UiAnswer): string {
  const title = request.title.length > 0 ? request.title : request.kind;
  switch (answer.type) {
    case 'cancel':
      return `Dismiss “${title}”`;
    case 'approve':
      return answer.decision.type === 'deny' ? `Deny “${title}”` : `Approve “${title}”`;
    case 'confirm':
      return `${answer.yes ? 'Yes' : 'No'} to “${title}”`;
    default:
      return `Answer “${title}”`;
  }
}

// ---- timeout countdown ----------------------------------------------------------------------

/** Milliseconds left of a request first seen at `seenMs`; null without a timeout. */
export function remainingMs(timeoutMs: number | null, seenMs: number, now: number): number | null {
  if (timeoutMs === null || timeoutMs <= 0) return null;
  return Math.max(0, seenMs + timeoutMs - now);
}

/** "1:05", "0:09". */
export function countdown(ms: number): string {
  const seconds = Math.ceil(ms / 1000);
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`;
}

// ---- tracking -------------------------------------------------------------------------------

/** A request that left the list without this phone's answer applying. */
export interface GoneRequest {
  request: api.UiRequestView;
  /** When it was noticed gone. */
  atMs: number;
}

export interface PendingTracker {
  /** When each listed request was first seen (the countdown's start). */
  seen: ReadonlyMap<string, number>;
  /** The last listed requests, by id. */
  listed: ReadonlyMap<string, api.UiRequestView>;
  /**
   * Requests this phone is answering (in flight) or saw its answer applied to: their disappearance
   * is expected, whichever of the live refresh and the answer outcome arrives first.
   */
  answered: ReadonlySet<string>;
  /** Requests that were answered elsewhere (or whose answer arrived too late). */
  gone: readonly GoneRequest[];
}

export const EMPTY_TRACKER: PendingTracker = { seen: new Map(), listed: new Map(), answered: new Set(), gone: [] };

/** How long an "answered on desktop" card stays. */
export const GONE_MS = 12_000;

/** Folds the current `pending_ui` list into the tracker. */
export function trackPending(tracker: PendingTracker, current: readonly api.UiRequestView[], now: number): PendingTracker {
  const seen = new Map<string, number>();
  const listed = new Map<string, api.UiRequestView>();
  for (const request of current) {
    seen.set(request.request, tracker.seen.get(request.request) ?? now);
    listed.set(request.request, request);
  }
  const gone = tracker.gone.filter((entry) => now - entry.atMs < GONE_MS && !listed.has(entry.request.request));
  const answered = new Set<string>();
  for (const [id, request] of tracker.listed) {
    if (listed.has(id)) {
      if (tracker.answered.has(id)) answered.add(id);
      continue;
    }
    if (!tracker.answered.has(id)) gone.push({ request, atMs: now });
  }
  const unchanged =
    gone.length === tracker.gone.length &&
    gone.every((entry, index) => entry === tracker.gone[index]) &&
    sameKeys(seen, tracker.seen) &&
    answered.size === tracker.answered.size &&
    [...listed].every(([id, request]) => tracker.listed.get(id) === request);
  return unchanged ? tracker : { seen, listed, answered, gone };
}

/** Marks this phone's answer as in flight: the request vanishing now is this phone's doing. */
export function noteSending(tracker: PendingTracker, id: string): PendingTracker {
  if (tracker.answered.has(id)) return tracker;
  const answered = new Set(tracker.answered);
  answered.add(id);
  return { ...tracker, answered };
}

/**
 * Records this phone's answer outcome: applied = expected to vanish; not applied = the desktop
 * won; null = no outcome (the op failed), so a later disappearance is no longer this phone's.
 */
export function noteAnswer(tracker: PendingTracker, request: api.UiRequestView, applied: boolean | null, now: number): PendingTracker {
  if (applied === true) return noteSending(tracker, request.request);
  const answered = new Set(tracker.answered);
  answered.delete(request.request);
  if (applied === null) return { ...tracker, answered };
  if (tracker.gone.some((entry) => entry.request.request === request.request)) return { ...tracker, answered };
  const listed = new Map(tracker.listed);
  listed.delete(request.request);
  return { ...tracker, listed, answered, gone: [...tracker.gone, { request, atMs: now }] };
}

/** Drops a gone card (dismissed by the user). */
export function dismissGone(tracker: PendingTracker, id: string): PendingTracker {
  return { ...tracker, gone: tracker.gone.filter((entry) => entry.request.request !== id) };
}

function sameKeys(a: ReadonlyMap<string, number>, b: ReadonlyMap<string, number>): boolean {
  if (a.size !== b.size) return false;
  for (const [key, value] of a) if (b.get(key) !== value) return false;
  return true;
}
