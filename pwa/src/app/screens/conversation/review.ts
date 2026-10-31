// Review comments on a project's changes: unified-diff line numbering (comments name lines of the
// file after the change, as deck-service's review prompt does), line/range selection, and the
// per-project comment store (memory plus sessionStorage, so a reload keeps the draft review).

import type { api } from '../../../remote/index.ts';

// ---- diff rows ------------------------------------------------------------------------------

export type RowKind = 'meta' | 'hunk' | 'plus' | 'minus' | 'context';

export interface DiffRow {
  text: string;
  kind: RowKind;
  /** The line number in the file after the change (plus and context rows). */
  newLine: number | null;
  /** The line number in the file before the change (minus and context rows). */
  oldLine: number | null;
}

const HUNK = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/;

/** Numbers each line of a unified diff. */
export function diffRows(text: string): DiffRow[] {
  const rows: DiffRow[] = [];
  let oldLine = 0;
  let newLine = 0;
  let inHunk = false;
  const lines = text.split('\n');
  if (lines.length > 0 && lines[lines.length - 1] === '') lines.pop();
  for (const line of lines) {
    const hunk = HUNK.exec(line);
    if (hunk !== null) {
      oldLine = Number(hunk[1]);
      newLine = Number(hunk[2]);
      inHunk = true;
      rows.push({ text: line, kind: 'hunk', newLine: null, oldLine: null });
      continue;
    }
    if (!inHunk || line.startsWith('\\')) {
      rows.push({ text: line, kind: 'meta', newLine: null, oldLine: null });
      continue;
    }
    if (line.startsWith('+')) {
      rows.push({ text: line, kind: 'plus', newLine, oldLine: null });
      newLine += 1;
    } else if (line.startsWith('-')) {
      rows.push({ text: line, kind: 'minus', newLine: null, oldLine });
      oldLine += 1;
    } else if (line.startsWith(' ') || line === '') {
      rows.push({ text: line, kind: 'context', newLine, oldLine });
      newLine += 1;
      oldLine += 1;
    } else {
      // A file header of the next file in a combined diff.
      inHunk = false;
      rows.push({ text: line, kind: 'meta', newLine: null, oldLine: null });
    }
  }
  return rows;
}

/**
 * The new-file line range rows `a..=b` (either order) cover, or null when it holds no such line
 * or crosses a hunk header.
 */
export function selectionRange(rows: readonly DiffRow[], a: number, b: number): { start: number; end: number } | null {
  const from = Math.min(a, b);
  const to = Math.max(a, b);
  let start: number | null = null;
  let end: number | null = null;
  for (let index = from; index <= to; index += 1) {
    const row = rows[index];
    if (row === undefined) return null;
    if (row.kind === 'hunk' || row.kind === 'meta') {
      if (index !== from && index !== to) return null;
      continue;
    }
    if (row.newLine === null) continue;
    start ??= row.newLine;
    end = row.newLine;
  }
  return start === null || end === null ? null : { start, end };
}

/** The rows a comment covers in this diff (to mark them). */
export function rowsOf(rows: readonly DiffRow[], comment: { start: number; end: number }): Set<number> {
  const out = new Set<number>();
  rows.forEach((row, index) => {
    if (row.newLine !== null && row.newLine >= comment.start && row.newLine <= comment.end) out.add(index);
  });
  return out;
}

/** The quoted text of a range (without diff markers), at most `max` lines. */
export function quoteOf(rows: readonly DiffRow[], start: number, end: number, max = 3): string[] {
  const out: string[] = [];
  for (const row of rows) {
    if (row.newLine === null || row.newLine < start || row.newLine > end) continue;
    out.push(row.text.slice(1));
    if (out.length === max) break;
  }
  return out;
}

/** "12", "12–15". */
export function rangeLabel(start: number, end: number): string {
  return start === end ? String(start) : `${start}–${end}`;
}

// ---- comments -------------------------------------------------------------------------------

export interface DraftComment {
  id: number;
  file: string;
  side: api.DiffSide;
  start: number;
  end: number;
  text: string;
  /** The first quoted lines, for the list. */
  quote: string[];
}

export interface ReviewDraft {
  comments: readonly DraftComment[];
  general: string;
}

export const EMPTY_REVIEW: ReviewDraft = { comments: [], general: '' };

export type ReviewAction =
  | { type: 'add'; comment: Omit<DraftComment, 'id'> }
  | { type: 'edit'; id: number; text: string }
  | { type: 'delete'; id: number }
  | { type: 'general'; text: string }
  | { type: 'clear' };

export function reviewReducer(state: ReviewDraft, action: ReviewAction): ReviewDraft {
  switch (action.type) {
    case 'add': {
      const id = state.comments.reduce((max, entry) => Math.max(max, entry.id), 0) + 1;
      const comments = [...state.comments, { ...action.comment, id }].sort(
        (a, b) => a.file.localeCompare(b.file) || a.start - b.start || a.id - b.id,
      );
      return { ...state, comments };
    }
    case 'edit':
      return { ...state, comments: state.comments.map((entry) => (entry.id === action.id ? { ...entry, text: action.text } : entry)) };
    case 'delete':
      return { ...state, comments: state.comments.filter((entry) => entry.id !== action.id) };
    case 'general':
      return { ...state, general: action.text };
    case 'clear':
      return EMPTY_REVIEW;
  }
}

/** The `review` op's comments and general text; null when there is nothing to send. */
export function reviewPayload(draft: ReviewDraft): { comments: api.ReviewComment[]; general: string | null } | null {
  const comments = draft.comments
    .filter((entry) => entry.text.trim().length > 0)
    .map((entry) => ({ file: entry.file, side: entry.side, start: entry.start, end: entry.end, text: entry.text.trim() }));
  const general = draft.general.trim();
  if (comments.length === 0 && general.length === 0) return null;
  return { comments, general: general.length > 0 ? general : null };
}

/** Number of comments that would be sent (line comments plus the general one). */
export function reviewCount(draft: ReviewDraft): number {
  return draft.comments.filter((entry) => entry.text.trim().length > 0).length + (draft.general.trim().length > 0 ? 1 : 0);
}

// ---- store ----------------------------------------------------------------------------------

const STORAGE_PREFIX = 'ompdeck.review.';

function isDraft(value: unknown): value is ReviewDraft {
  if (typeof value !== 'object' || value === null) return false;
  const { comments, general } = value as Record<string, unknown>;
  return (
    typeof general === 'string' &&
    Array.isArray(comments) &&
    comments.every((entry: unknown) => {
      if (typeof entry !== 'object' || entry === null) return false;
      const c = entry as Record<string, unknown>;
      return (
        typeof c['id'] === 'number' &&
        typeof c['file'] === 'string' &&
        (c['side'] === 'staged' || c['side'] === 'unstaged' || c['side'] === 'untracked') &&
        typeof c['start'] === 'number' &&
        typeof c['end'] === 'number' &&
        typeof c['text'] === 'string' &&
        Array.isArray(c['quote']) &&
        (c['quote'] as unknown[]).every((line) => typeof line === 'string')
      );
    })
  );
}

/** Per-project review drafts, kept in memory and mirrored to `storage` (sessionStorage). */
export class ReviewStore {
  readonly #storage: Pick<Storage, 'getItem' | 'setItem' | 'removeItem'> | null;
  readonly #drafts = new Map<string, ReviewDraft>();
  readonly #listeners = new Set<() => void>();

  constructor(storage: Pick<Storage, 'getItem' | 'setItem' | 'removeItem'> | null) {
    this.#storage = storage;
  }

  readonly subscribe = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  };

  get(project: string): ReviewDraft {
    const cached = this.#drafts.get(project);
    if (cached !== undefined) return cached;
    let draft = EMPTY_REVIEW;
    try {
      const raw = this.#storage?.getItem(STORAGE_PREFIX + project) ?? null;
      if (raw !== null) {
        const parsed: unknown = JSON.parse(raw);
        if (isDraft(parsed)) draft = parsed;
      }
    } catch {
      // Unreadable storage: start empty.
    }
    this.#drafts.set(project, draft);
    return draft;
  }

  dispatch(project: string, action: ReviewAction): void {
    const next = reviewReducer(this.get(project), action);
    this.#drafts.set(project, next);
    try {
      if (next.comments.length === 0 && next.general.length === 0) this.#storage?.removeItem(STORAGE_PREFIX + project);
      else this.#storage?.setItem(STORAGE_PREFIX + project, JSON.stringify(next));
    } catch {
      // Storage full or unavailable: the draft stays in memory.
    }
    for (const listener of this.#listeners) listener();
  }
}

function sessionStore(): Pick<Storage, 'getItem' | 'setItem' | 'removeItem'> | null {
  try {
    return typeof sessionStorage === 'undefined' ? null : sessionStorage;
  } catch {
    return null;
  }
}

/** The app's review drafts. */
export const reviews = new ReviewStore(sessionStore());
