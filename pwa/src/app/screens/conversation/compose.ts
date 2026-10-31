// Pure logic of the composer: which `compose` modes a session state allows, how the mode follows
// busy/idle, whether a draft can go, and the attachment list (upload progress, limits).

import type { api } from '../../../remote/index.ts';
import { MAX_UPLOADS, MAX_UPLOAD_FILE, MAX_UPLOAD_TOTAL } from '../../../remote/api.ts';

/**
 * omp's state as the gateway gates `compose` on it: send needs `idle`, steer/follow-up `busy`
 * (omp's `activity`, read from its terminal title). `starting`: no omp title yet; `attention`:
 * an omp dialog wants an answer. Both take insert only.
 */
export type SessionPhase = 'idle' | 'busy' | 'starting' | 'attention' | 'exited';

export function phaseOf(live: Pick<api.LiveSession, 'status' | 'activity'>): SessionPhase {
  if (live.status.state === 'exited') return 'exited';
  switch (live.activity) {
    case 'idle':
      return 'idle';
    case 'busy':
      return 'busy';
    case 'attention':
      return 'attention';
    case 'unknown':
      return 'starting';
  }
}

/** The modes a session in `phase` takes: send while idle, steer/follow-up while busy, insert while omp runs. */
export function modesFor(phase: SessionPhase): api.ComposeMode[] {
  switch (phase) {
    case 'idle':
      return ['send', 'insert'];
    case 'busy':
      return ['steer', 'follow_up', 'insert'];
    case 'starting':
    case 'attention':
      return ['insert'];
    case 'exited':
      return [];
  }
}

export const MODE_LABEL: Record<api.ComposeMode, string> = {
  send: 'Send',
  steer: 'Steer',
  follow_up: 'Follow-up',
  insert: 'Insert',
};

export const MODE_HINT: Record<api.ComposeMode, string> = {
  send: 'Sends a new prompt',
  steer: 'Redirects omp now, mid-turn',
  follow_up: 'Queued until omp finishes this turn',
  insert: 'Pastes into omp’s input without sending',
};

/**
 * The mode after a phase change: entering busy picks steer, entering idle picks send, unless the
 * user chose insert (valid in both). A mode the phase does not take falls back to its default.
 */
export function followMode(mode: api.ComposeMode, previous: SessionPhase, phase: SessionPhase): api.ComposeMode {
  const modes = modesFor(phase);
  const fallback: api.ComposeMode = phase === 'busy' ? 'steer' : phase === 'idle' ? 'send' : 'insert';
  if (previous === phase) return modes.includes(mode) ? mode : fallback;
  // Insert stays only when the user picked it: in a phase offering nothing else it was forced.
  if (mode === 'insert' && modes.includes('insert') && modesFor(previous).length > 1) return 'insert';
  return fallback;
}

/** Why the draft cannot go now, or null. Steer and follow-up refuse empty text (an empty Enter while busy aborts the turn). */
export function composeProblem(
  mode: api.ComposeMode,
  phase: SessionPhase,
  text: string,
  attachments: readonly Attachment[],
): string | null {
  if (phase === 'exited') return 'The session has exited.';
  if (!modesFor(phase).includes(mode)) {
    switch (phase) {
      case 'busy':
        return 'omp is working: steer, follow up or insert.';
      case 'starting':
        return 'omp is starting: wait to send, or insert.';
      case 'attention':
        return 'omp waits for an answer: answer it first, or insert.';
      case 'idle':
        return 'omp is idle: send or insert.';
    }
  }
  if (attachments.some((entry) => entry.status === 'uploading')) return 'Waiting for the attachments to upload.';
  if (attachments.some((entry) => entry.status === 'error')) return 'Remove the attachments that failed.';
  const empty = text.trim().length === 0;
  if ((mode === 'steer' || mode === 'follow_up') && empty) return `${MODE_LABEL[mode]} needs text.`;
  if (empty && attachments.length === 0) return 'Write a message first.';
  return null;
}

// ---- attachments ----------------------------------------------------------------------------

export interface Attachment {
  /** Local id (stable key). */
  id: number;
  name: string;
  mime: string;
  size: number;
  status: 'uploading' | 'done' | 'error';
  /** Bytes uploaded so far. */
  sent: number;
  /** Estimated seconds until the upload is done (pacing included), when known. */
  eta?: number | null;
  /** The gateway's upload id once done. */
  upload: string | null;
  error: string | null;
}

/** Why `file` cannot be attached next to `current`, or null. */
export function attachProblem(file: { name: string; size: number }, current: readonly Attachment[]): string | null {
  if (file.size > MAX_UPLOAD_FILE) return `${file.name} is ${fileSize(file.size)}; the limit is ${fileSize(MAX_UPLOAD_FILE)} per file.`;
  if (file.size === 0) return `${file.name} is empty.`;
  const kept = current.filter((entry) => entry.status !== 'error');
  if (kept.length >= MAX_UPLOADS) return `At most ${MAX_UPLOADS} attachments per message.`;
  const total = kept.reduce((sum, entry) => sum + entry.size, 0) + file.size;
  if (total > MAX_UPLOAD_TOTAL) return `Attachments may total ${fileSize(MAX_UPLOAD_TOTAL)}.`;
  return null;
}

export type AttachmentAction =
  | { type: 'add'; attachment: Attachment }
  | { type: 'progress'; id: number; sent: number; eta?: number }
  | { type: 'done'; id: number; upload: string }
  | { type: 'failed'; id: number; error: string }
  | { type: 'remove'; id: number }
  | { type: 'remove_sent'; uploads: readonly string[] };

export function attachmentsReducer(state: readonly Attachment[], action: AttachmentAction): readonly Attachment[] {
  switch (action.type) {
    case 'add':
      return [...state, action.attachment];
    case 'progress':
      return state.map((entry) =>
        entry.id === action.id && entry.status === 'uploading' ? { ...entry, sent: Math.min(entry.size, action.sent), eta: action.eta ?? null } : entry,
      );
    case 'done':
      return state.map((entry) =>
        entry.id === action.id ? { ...entry, status: 'done', sent: entry.size, upload: action.upload, error: null } : entry,
      );
    case 'failed':
      return state.map((entry) => (entry.id === action.id ? { ...entry, status: 'error', error: action.error } : entry));
    case 'remove':
      return state.filter((entry) => entry.id !== action.id);
    case 'remove_sent':
      return state.filter((entry) => entry.upload === null || !action.uploads.includes(entry.upload));
  }
}

/** The upload ids of finished attachments, in order. */
export function uploadIds(attachments: readonly Attachment[]): string[] {
  return attachments.flatMap((entry) => (entry.status === 'done' && entry.upload !== null ? [entry.upload] : []));
}

/** "~12s" style text for an ETA in seconds (null when unknown or under a second). */
export function etaText(seconds: number | null | undefined): string | null {
  if (seconds === null || seconds === undefined || !Number.isFinite(seconds) || seconds < 1) return null;
  if (seconds < 90) return `~${Math.round(seconds)}s`;
  return `~${Math.ceil(seconds / 60)} min`;
}

/** Upload progress 0..1. */
export function progress(entry: Attachment): number {
  return entry.size === 0 ? 1 : Math.min(1, entry.sent / entry.size);
}

/** "812 B", "4.2 KB", "13 MB". */
export function fileSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(bytes < 10 * 1024 ? 1 : 0)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(bytes < 10 * 1024 * 1024 ? 1 : 0)} MB`;
}
