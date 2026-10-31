// The device application protocol v1 plus the slice-4 additions, mirroring Rust
// `deck_remote::api` (crates/deck-remote/src/api.rs is the source of truth;
// docs/notes/mobile-relay.md, "R4 contract"; docs/notes/mobile-pwa.md, "Wire additions"). JSON
// enums are internally tagged (`op`, `feed`, `type`, `state`, `code`, `reason`), snake_case,
// struct variants only; `None` is `null`, except the fields slice 4 added to R5 types
// (`Welcome.push_key`, `SessionSummary.stub`, `LiveSession.pending_ui`, `images`/`args_value`/
// `details` of transcript items), which are absent when empty and optional here. A device uses
// the slice-4 ops and feeds only when `Welcome.features` lists {@link FEATURE_S4}.
//
// Gateway → device JSON (`Welcome`, `Reply`, `Event`, `TermEnd`) is validated at runtime:
// unknown fields are ignored, wrong types and unknown variants refused, except that an unknown
// `Event` or transcript `Item` type decodes to `unknown_event` / `unknown_item` (a newer gateway's
// additions are skipped, not fatal). Device → gateway JSON (`Request`) is built here and
// serialized canonically by the same validators (Rust field order, `null` for absent optionals,
// free-form JSON values with sorted object keys as Rust's `serde_json::Value` writes them), so it
// is byte-identical to Rust's serialization (except integer-like object keys inside a free-form
// value, which JavaScript always orders first).

import { fromUtf8, utf8 } from './bytes.ts';
import type { AppRecord } from './records.ts';

/** Device application protocol version (`Welcome.protocol`). */
export const PROTOCOL = 1;
/** Largest JSON reply body. */
export const MAX_REPLY = (1 << 20) - 5;
/** A snapshot above this ends its view with `TermEnd` `too_large`. */
export const MAX_SNAPSHOT = 16 << 20;
/** `Limits` defaults. */
export const WINDOW_BYTES = 1 << 20;
export const WINDOW_MESSAGES = 32;
export const MAX_INFLIGHT = 32;
export const MAX_VIEWS = 4;
export const MAX_FEEDS = 16;
/** No ack for this long while messages are outstanding closes the device (`CLOSE.STALLED`). */
export const STALL_SECS = 60;
/** `transcript` feed `tail` at most. */
export const MAX_TAIL = 500;
/** `cards` feed sessions at most. */
export const MAX_CARDS = 64;
/** `SessionSummary.first_prompt` characters at most. */
export const FIRST_PROMPT_CHARS = 200;
/** `sessions` feed `limit` range: 1..=this. */
export const MAX_SESSIONS_LIMIT = 500;
/**
 * Every value fits one message: a `transcript` value carries at most this many bytes of items
 * (a large tail arrives as several values, `start` continuing, `reset` only on the first).
 */
export const MAX_TRANSCRIPT_BYTES = 512 * 1024;
/** Bytes of one transcript item's text field at most, then {@link CUT_MARK}. */
export const MAX_ITEM_TEXT = 64 * 1024;
/** Ends a cut transcript text. */
export const CUT_MARK = '\u2026';
/** `diff` reply text is cut to this many bytes, with `truncated: true`. */
export const MAX_DIFF_TEXT = 768 * 1024;

// ---- slice 4 bounds -------------------------------------------------------------------------

/** The slice-4 feature name (`Welcome.features`, `DeviceHello.features`). */
export const FEATURE_S4 = 's4';
/** `last_reply` text bytes at most, then `truncated`. */
export const MAX_LAST_REPLY = 256 * 1024;
/** Raw bytes of one download/upload chunk at most (sent as base64). */
export const MAX_TRANSFER_CHUNK = 512 * 1024;
/** Base64 characters of one `upload_chunk` `data` at most. */
export const MAX_TRANSFER_CHUNK_B64 = Math.ceil(MAX_TRANSFER_CHUNK / 3) * 4;
/** Open download handles per device. */
export const MAX_DOWNLOADS = 8;
/** Open uploads per device. */
export const MAX_UPLOADS = 4;
/** A download or upload unused for this long expires. */
export const TRANSFER_IDLE_SECS = 600;
/** Bytes of one uploaded file at most. */
export const MAX_UPLOAD_FILE = 20 * 1024 * 1024;
/** Bytes of a device's attachments at most. */
export const MAX_UPLOAD_TOTAL = 200 * 1024 * 1024;
/** `tree` `limit` range: 1..=this. */
export const MAX_TREE_LIMIT = 2000;
/** `usage_activity` `days` range: 1..=this. */
export const MAX_ACTIVITY_DAYS = 371;
/** `inventory` feed `limit` range: 1..=this. */
export const MAX_INVENTORY_LIMIT = 500;
/** `tool_call.args_value`: each string cut to this many bytes … */
export const MAX_ARGS_STRING = 16 * 1024;
/** … and the whole value at most this many bytes serialized, else absent. */
export const MAX_ARGS_VALUE = 64 * 1024;
/** `tool_result.details` at most this many bytes serialized, else absent. */
export const MAX_DETAILS = 32 * 1024;
/** `LiveSession.pending_ui` entries at most. */
export const MAX_PENDING_UI = 8;
/** Bytes of each string of a UI request at most. */
export const MAX_UI_TEXT = 4 * 1024;
/** Entries of each option list of a UI request at most. */
export const MAX_UI_OPTIONS = 50;
/** Dir tokens a device holds at most. */
export const MAX_DIR_TOKENS = 4096;
/** `TreeNode.text` characters at most. */
export const TREE_TEXT_CHARS = 200;

// ---- shared ---------------------------------------------------------------------------------

/** A controlled session: the gateway's opaque session key and its generation. */
export interface LiveRef {
  key: string;
  generation: number;
}

export interface Permissions {
  control: boolean;
  manage: boolean;
}

export interface Limits {
  max_message: number;
  window_bytes: number;
  window_messages: number;
  max_inflight: number;
  max_views: number;
  max_feeds: number;
}

/** The first record of every session. */
export interface Welcome {
  protocol: number;
  gateway_version: string;
  /** The gateway instance (32 hex); live refs and seqs are bound to it. */
  epoch: string;
  /** This device's registry id (`d-<16 hex>`). */
  device: string;
  name: string;
  permissions: Permissions;
  /** New journaled ops use `seq >= next_seq`; lower seqs only for `outcome` lookups. */
  next_seq: number;
  limits: Limits;
  features: string[];
  /** Slice 4, absent when empty: the Web Push VAPID public key (base64url, 65-byte P-256 point). */
  push_key?: string;
}

export type Class = 'view' | 'control' | 'manage';
export type ViewMode = 'observe' | 'input' | 'control';
export type DiffSide = 'staged' | 'unstaged' | 'untracked';
export type CompactMode = 'default' | 'soft' | 'snapcompact' | 'focus';

// ---- slice-4 request types ------------------------------------------------------------------

/** How `compose` delivers its text. */
export type ComposeMode = 'send' | 'steer' | 'follow_up' | 'insert';
export type TreeFilter = 'conversation' | 'default' | 'all' | 'labeled';
export type Split = 'none' | 'right' | 'down';
export type ContinueWhere = 'worktree' | 'main' | 'switch_back';
export type UpdateKind = 'omp' | 'plugins' | 'stable' | 'canary';
export type AppUpdateStep = 'download' | 'apply';
export type ConflictResolution = 'keep' | 'overwrite' | 'both';

/** One `review` comment: lines `start..=end` of a repo-relative changed file. */
export interface ReviewComment {
  file: string;
  side: DiffSide;
  start: number;
  end: number;
  text: string;
}

export interface SyncParts {
  omp: boolean;
  sessions: boolean;
  memory: boolean;
}

/** Where `sync_restore_prepare` restores to (tag `type`). */
export type RestoreTarget = { type: 'project'; project: string } | { type: 'dir'; dir: string };

export interface ConflictChoice {
  session: string;
  choice: ConflictResolution;
}

export interface PushPrefs {
  turn_finished: boolean;
  attention: boolean;
  ended: boolean;
  show_titles: boolean;
}

/** A tool-approval decision (tag `type`). */
export type ApproveDecision =
  | { type: 'once' }
  | { type: 'session' }
  | { type: 'deny' }
  | { type: 'grant'; index: number; always: boolean };

export interface UiAskAnswer {
  id: string;
  selected: number[];
  custom?: string | null;
}

/** An answer to a pending UI request (tag `type`). */
export type UiAnswer =
  | { type: 'choice'; index: number }
  | { type: 'choices'; indices: number[] }
  | { type: 'text'; text: string }
  | { type: 'confirm'; yes: boolean }
  | { type: 'ask'; answers: UiAskAnswer[] }
  | { type: 'approve'; decision: ApproveDecision }
  | { type: 'cancel' };

/** Any JSON value (Rust `serde_json::Value`). */
export type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

// ---- requests (device → gateway) ------------------------------------------------------------

/** A session runtime command (`action` op, tag `type`). */
export type SessionAction =
  | { type: 'handoff'; focus?: string | null }
  | { type: 'compact'; mode: CompactMode; focus?: string | null }
  | { type: 'rename'; name: string }
  | { type: 'auto_name' }
  | { type: 'plan' }
  | { type: 'retry' };

/** A subscription (tag `feed`). */
export type Feed =
  | { feed: 'projects' }
  /** One project's sessions, newest first: at most `limit` (1..={@link MAX_SESSIONS_LIMIT}). */
  | { feed: 'sessions'; project: string; limit: number }
  | { feed: 'live' }
  | { feed: 'inbox' }
  | { feed: 'cards'; sessions: string[] }
  | { feed: 'transcript'; session: string; agent?: string | null; tail: number }
  | { feed: 'changes'; project: string }
  // slice 4
  | { feed: 'running' }
  /** Session files, newest first: at most `limit` (1..={@link MAX_INVENTORY_LIMIT}). */
  | { feed: 'inventory'; project?: string | null; limit: number }
  | { feed: 'context'; live: LiveRef }
  | { feed: 'usage' }
  | { feed: 'agents'; session: string }
  | { feed: 'worktrees'; project: string }
  | { feed: 'backup' }
  | { feed: 'sync' }
  | { feed: 'updates' }
  | { feed: 'device' };

/** Operations (tag `op`): v1 and the slice-4 additions. */
export type Op =
  | { op: 'subscribe'; feed: Feed }
  | { op: 'unsubscribe'; feed: Feed }
  | { op: 'outcome'; seq: number }
  | { op: 'models' }
  | { op: 'diff'; project: string; file: string; side: DiffSide }
  | { op: 'attach'; live: LiveRef; mode: ViewMode }
  | { op: 'detach'; view: number }
  | { op: 'ping' }
  | { op: 'new_session'; project: string }
  | { op: 'resume'; session: string }
  | { op: 'restart'; live: LiveRef }
  | { op: 'stop'; live: LiveRef }
  | { op: 'kill'; live: LiveRef }
  | { op: 'prompt'; live: LiveRef; text: string }
  | { op: 'interrupt'; live: LiveRef }
  | { op: 'switch_model'; live: LiveRef; selector: string; level?: string | null }
  | { op: 'set_effort'; live: LiveRef; level: string }
  | { op: 'action'; live: LiveRef; action: SessionAction }
  | { op: 'answer'; live: LiveRef; request: number; proceed: boolean }
  | { op: 'cancel'; seq: number }
  | { op: 'inbox_open'; live: LiveRef }
  | { op: 'inbox_dismiss'; live: LiveRef }
  | { op: 'inbox_clear' }
  | { op: 'stage'; project: string; files: string[] }
  | { op: 'unstage'; project: string; files: string[] }
  | { op: 'delete_sessions'; sessions: string[] }
  | { op: 'copy_session'; session: string; project: string }
  | { op: 'move_session'; session: string; project: string }
  | { op: 'discard'; project: string; files: string[] }
  | { op: 'delete_untracked'; project: string; files: string[] }
  // slice 4: view
  | { op: 'last_reply'; session: string }
  | { op: 'export'; session: string }
  | { op: 'blob'; session: string; agent?: string | null; blob: string }
  | { op: 'download'; download: string; offset: number }
  | { op: 'tree'; session: string; filter: TreeFilter; cursor?: string | null; limit: number }
  | { op: 'browse'; dir?: string | null }
  | { op: 'usage_activity'; days: number }
  | { op: 'usage_refresh' }
  | { op: 'omp_settings' }
  | { op: 'backup_list' }
  | { op: 'sync_backups'; project?: string | null }
  | { op: 'presence'; visible: boolean }
  | { op: 'push_subscribe'; endpoint: string; p256dh: string; auth: string }
  | { op: 'push_unsubscribe' }
  | { op: 'push_prefs'; prefs: PushPrefs }
  | { op: 'device_rename'; name: string }
  | { op: 'unpair' }
  // slice 4: control
  | { op: 'upload_begin'; name: string; mime: string; size: number }
  | { op: 'upload_chunk'; upload: string; offset: number; data: string }
  | { op: 'upload_cancel'; upload: string }
  | { op: 'compose'; live: LiveRef; text: string; attachments: string[]; mode: ComposeMode }
  | { op: 'review'; live: LiveRef; comments: ReviewComment[]; general?: string | null; send: boolean }
  | { op: 'answer_ui'; live: LiveRef; request: string; value: UiAnswer }
  | { op: 'agent_kill'; live: LiveRef; agent: string }
  | { op: 'agent_revive'; live: LiveRef; agent: string }
  | { op: 'agent_chat'; live: LiveRef; agent: string; text: string }
  | { op: 'fork_at'; session: string; entry: string }
  | { op: 'navigate'; live: LiveRef; entry: string }
  | { op: 'new_session_in'; dir: string }
  | { op: 'running_kill'; ids: string[] }
  | { op: 'worktree_create'; project: string; branch: string; base?: string | null; open: boolean }
  | { op: 'worktree_move'; live: LiveRef; branch: string }
  | { op: 'worktree_finish'; project: string }
  | { op: 'worktree_continue'; parked: string; where: ContinueWhere }
  | { op: 'update_check' }
  | { op: 'backup_run' }
  | { op: 'sync_backup'; project: string }
  | { op: 'open_on_desktop'; session: string; split: Split }
  | { op: 'close_on_desktop'; session: string }
  // slice 4: manage
  | { op: 'worktree_remove'; project: string; park: boolean }
  | { op: 'omp_set'; key: string; value: JsonValue }
  | { op: 'omp_reset'; key: string }
  | { op: 'update_run'; kind: UpdateKind }
  | { op: 'app_update'; step: AppUpdateStep }
  | { op: 'backup_connect' }
  | { op: 'backup_delete'; ids: string[] }
  | { op: 'backup_restore_prepare'; id: string }
  | { op: 'backup_restore_apply'; handle: number }
  | { op: 'backup_restore_discard'; handle: number }
  | { op: 'sync_add'; dir: string }
  | { op: 'sync_set'; project: string; parts: SyncParts; excluded: string[] }
  | { op: 'sync_remove'; project: string }
  | { op: 'sync_delete'; ids: string[] }
  | { op: 'sync_restore_prepare'; backup: string; target: RestoreTarget; parts: SyncParts }
  | { op: 'sync_restore_apply'; handle: number; conflicts: ConflictChoice[]; excluded?: string[] };

export type OpName = Op['op'];

export interface Request {
  id: number;
  /** Required exactly for journaled ops ({@link opJournaled}). */
  seq?: number | null;
  op: Op;
}

/** Class of every op (Rust `Op::class`); `attach` depends on its mode. */
const OP_CLASS: { readonly [K in OpName]: Class } = {
  subscribe: 'view',
  unsubscribe: 'view',
  outcome: 'view',
  models: 'view',
  diff: 'view',
  attach: 'view',
  detach: 'view',
  ping: 'view',
  new_session: 'control',
  resume: 'control',
  restart: 'control',
  stop: 'control',
  kill: 'control',
  prompt: 'control',
  interrupt: 'control',
  switch_model: 'control',
  set_effort: 'control',
  action: 'control',
  answer: 'control',
  cancel: 'control',
  inbox_open: 'control',
  inbox_dismiss: 'control',
  inbox_clear: 'control',
  stage: 'control',
  unstage: 'control',
  delete_sessions: 'manage',
  copy_session: 'manage',
  move_session: 'manage',
  discard: 'manage',
  delete_untracked: 'manage',
  last_reply: 'view',
  export: 'view',
  blob: 'view',
  download: 'view',
  tree: 'view',
  browse: 'view',
  usage_activity: 'view',
  usage_refresh: 'view',
  omp_settings: 'view',
  backup_list: 'view',
  sync_backups: 'view',
  presence: 'view',
  push_subscribe: 'view',
  push_unsubscribe: 'view',
  push_prefs: 'view',
  device_rename: 'view',
  unpair: 'view',
  upload_begin: 'control',
  upload_chunk: 'control',
  upload_cancel: 'control',
  compose: 'control',
  review: 'control',
  answer_ui: 'control',
  agent_kill: 'control',
  agent_revive: 'control',
  agent_chat: 'control',
  fork_at: 'control',
  navigate: 'control',
  new_session_in: 'control',
  running_kill: 'control',
  worktree_create: 'control',
  worktree_move: 'control',
  worktree_finish: 'control',
  worktree_continue: 'control',
  update_check: 'control',
  backup_run: 'control',
  sync_backup: 'control',
  open_on_desktop: 'control',
  close_on_desktop: 'control',
  worktree_remove: 'manage',
  omp_set: 'manage',
  omp_reset: 'manage',
  update_run: 'manage',
  app_update: 'manage',
  backup_connect: 'manage',
  backup_delete: 'manage',
  backup_restore_prepare: 'manage',
  backup_restore_apply: 'manage',
  backup_restore_discard: 'manage',
  sync_add: 'manage',
  sync_set: 'manage',
  sync_remove: 'manage',
  sync_delete: 'manage',
  sync_restore_prepare: 'manage',
  sync_restore_apply: 'manage',
};

/** Every op tag, in Rust `Op::NAMES` order. */
export const OP_NAMES: readonly OpName[] = Object.keys(OP_CLASS) as OpName[];

/** What an op needs beyond the session itself (Rust `Op::class`). */
export function opClass(op: Op): Class {
  if (op.op === 'attach') return op.mode === 'observe' ? 'view' : 'control';
  return OP_CLASS[op.op];
}

/** Control ops that take no `seq` (Rust `Op::journaled`). */
const UNJOURNALED: Partial<Record<OpName, true>> = {
  attach: true,
  detach: true,
  upload_begin: true,
  upload_chunk: true,
  upload_cancel: true,
  update_check: true,
};

/** Journaled (needs a `seq`): every Control/Manage op except `attach`/`detach`, the uploads and `update_check`. */
export function opJournaled(op: Op): boolean {
  return UNJOURNALED[op.op] !== true && opClass(op) !== 'view';
}

/** Whether `permissions` allow `op` (Rust `Op::allowed`). */
export function opAllowed(op: Op, permissions: Permissions): boolean {
  const need = opClass(op);
  return need === 'view' || (need === 'control' ? permissions.control : permissions.manage);
}

/** A device request the gateway would refuse (`bad_request`); never sent. */
export class ApiRequestError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'ApiRequestError';
  }
}

function problem(run: () => unknown): string | null {
  try {
    run();
    return null;
  } catch (error) {
    if (error instanceof ApiDecodeError) return error.message;
    throw error;
  }
}

/**
 * Whether the gateway accepts this subscription: field types and integer ranges as serde reads
 * them, then Rust `Feed::check` (sessions `limit` 1..={@link MAX_SESSIONS_LIMIT}, at most
 * {@link MAX_CARDS} card sessions, transcript `tail` ≤ {@link MAX_TAIL}, inventory `limit`
 * 1..={@link MAX_INVENTORY_LIMIT}). Returns the problem, or null.
 */
export function checkFeed(subscription: Feed): string | null {
  const shape = problem(() => feed(subscription, 'feed'));
  if (shape !== null) return shape;
  switch (subscription.feed) {
    case 'sessions':
      return subscription.limit >= 1 && subscription.limit <= MAX_SESSIONS_LIMIT
        ? null
        : `sessions limit must be 1..=${MAX_SESSIONS_LIMIT}`;
    case 'cards':
      return subscription.sessions.length > MAX_CARDS ? `at most ${MAX_CARDS} cards` : null;
    case 'transcript':
      return subscription.tail > MAX_TAIL ? `transcript tail must be at most ${MAX_TAIL}` : null;
    case 'inventory':
      return subscription.limit >= 1 && subscription.limit <= MAX_INVENTORY_LIMIT
        ? null
        : `inventory limit must be 1..=${MAX_INVENTORY_LIMIT}`;
    default:
      return null;
  }
}

/** Rust `Op::check` beyond the feed bounds. */
function checkOp(op: Op): string | null {
  switch (op.op) {
    case 'subscribe':
    case 'unsubscribe':
      return checkFeed(op.feed);
    case 'tree':
      return op.limit >= 1 && op.limit <= MAX_TREE_LIMIT ? null : `tree limit must be 1..=${MAX_TREE_LIMIT}`;
    case 'usage_activity':
      return op.days >= 1 && op.days <= MAX_ACTIVITY_DAYS ? null : `activity days must be 1..=${MAX_ACTIVITY_DAYS}`;
    case 'upload_chunk':
      return op.data.length <= MAX_TRANSFER_CHUNK_B64
        ? null
        : `an upload chunk carries at most ${MAX_TRANSFER_CHUNK} bytes`;
    default:
      return null;
  }
}

/**
 * Whether the gateway would parse and admit `request` rather than answer `bad_request`: every
 * field's type and integer range (u32/u64 as far as JavaScript numbers are exact), the seq rule
 * (present exactly for journaled ops) and Rust `Op::check` ({@link checkFeed}, tree `limit`,
 * activity `days`, upload chunk size). Returns the problem, or null.
 */
export function checkRequest(request: Request): string | null {
  const shape = problem(() => requestV(request, 'request'));
  if (shape !== null) return shape;
  const hasSeq = request.seq !== null && request.seq !== undefined;
  if (hasSeq !== opJournaled(request.op)) {
    return `${request.op.op} ${hasSeq ? 'takes no seq' : 'needs a seq'}`;
  }
  return checkOp(request.op);
}

/**
 * The canonical JSON text of a request (Rust field order, `null` for absent optionals). Throws
 * {@link ApiRequestError} when {@link checkRequest} fails.
 */
export function requestJson(request: Request): string {
  const bad = checkRequest(request);
  if (bad !== null) throw new ApiRequestError(bad);
  // The validator rebuilds every object in Rust's field order.
  return JSON.stringify(requestV(request, 'request'));
}

/** A REQUEST record carrying `request`. */
export function requestRecord(request: Request): AppRecord {
  return { kind: 'request', body: utf8(requestJson(request)) };
}

/**
 * Builds requests of one session: fresh ids, and seqs (from `Welcome.next_seq` on) exactly for
 * journaled ops. Keep the builder across reconnects to the same gateway epoch only.
 */
export class RequestBuilder {
  #nextId: number;
  #nextSeq: number;

  constructor(welcome: Pick<Welcome, 'next_seq'>, firstId = 1) {
    this.#nextSeq = welcome.next_seq;
    this.#nextId = firstId;
  }

  /** Throws {@link ApiRequestError} (consuming no id or seq) when {@link checkRequest} fails. */
  build(op: Op): Request {
    const request: Request = opJournaled(op)
      ? { id: this.#nextId, seq: this.#nextSeq, op }
      : { id: this.#nextId, seq: null, op };
    const bad = checkRequest(request);
    if (bad !== null) throw new ApiRequestError(bad);
    this.#nextId++;
    if (opJournaled(op)) this.#nextSeq++;
    return request;
  }

  /** The next seq a journaled op would get. */
  get nextSeq(): number {
    return this.#nextSeq;
  }
}

// ---- feeds ----------------------------------------------------------------------------------

export interface SessionSummary {
  /** omp's session id. */
  id: string;
  title: string | null;
  first_prompt: string | null;
  modified_unix_ms: number;
  /** Slice 4, absent when false: no title and no user prompt. */
  stub?: boolean;
}

/** A project without its sessions (see the `sessions` feed). */
export interface Project {
  id: string;
  /** `~`-abbreviated folder. */
  display: string;
  /** The folder is a linked git worktree. */
  worktree: boolean;
  session_count: number;
  /** Its newest session's last modification. */
  modified_unix_ms: number;
}

export type LiveKind = 'session' | 'tool';
export type Activity = 'unknown' | 'idle' | 'busy' | 'attention';

/** A session's indicator (tag `state`). */
export type Status =
  | { state: 'idle' }
  | { state: 'busy' }
  | { state: 'attention' }
  | { state: 'exited'; clean: boolean };

export interface SwitchView {
  selector: string | null;
  level: string | null;
}

export type Refusal = 'precondition' | 'dialog_open' | 'no_bracketed_paste';
export type CancelReason = 'exited' | 'detached' | 'closing' | 'session_changed' | 'not_allowed';

/** How far a command got toward the terminal (tag `state`). */
export type DeliveryView =
  | { state: 'queued' }
  | { state: 'awaiting_confirm'; draft: string }
  | { state: 'writing' }
  | { state: 'delivered' }
  | { state: 'refused'; reason: Refusal }
  | { state: 'declined' }
  | { state: 'aborted' }
  | { state: 'stalled' }
  | { state: 'cancelled'; reason: CancelReason }
  | { state: 'unobserved' };

export interface OwnRequest {
  request: number;
  seq: number;
  label: string;
  delivery: DeliveryView;
}

export interface LiveSession {
  live: LiveRef;
  kind: LiveKind;
  label: string;
  title: string | null;
  session: string | null;
  project: string | null;
  display: string;
  activity: Activity;
  status: Status;
  exit_code: number | null;
  busy: boolean;
  model: string | null;
  pending_switch: SwitchView | null;
  live_control: boolean;
  desktop_controlled: boolean;
  views: number;
  requests: OwnRequest[];
  /** Slice 4, absent when empty: omp's pending UI requests, oldest first. */
  pending_ui?: UiRequestView[];
}

/** An image of a transcript message, fetched with the `blob` op. */
export interface ImageRef {
  blob: string;
  mime: string;
  /** `0` when unknown. */
  bytes: number;
}

export type UiKind = 'select' | 'confirm' | 'input' | 'editor' | 'ask' | 'approve';

export interface UiOption {
  label: string;
  description: string | null;
}

export interface UiQuestion {
  id: string;
  question: string;
  header: string | null;
  options: UiOption[];
  multi: boolean;
  recommended: number | null;
}

export interface ApproveGrant {
  scope: string;
  label: string;
  always_label: string;
}

export interface ApproveView {
  full: string;
  compact: string;
  session_label: string | null;
  grants: ApproveGrant[];
}

/** A pending omp UI request; fields its kind does not use are empty/null. */
export interface UiRequestView {
  /** `u-` + decimal id, what `answer_ui` takes. */
  request: string;
  kind: UiKind;
  title: string;
  message: string | null;
  timeout_ms: number | null;
  options: UiOption[];
  multi: boolean;
  initial: number | null;
  prefill: string | null;
  questions: UiQuestion[];
  approve: ApproveView | null;
}

/** Tag `type`. */
export type InboxKind =
  | { type: 'finished' }
  | { type: 'error'; message: string | null }
  | { type: 'attention' }
  | { type: 'exited'; code: number | null };

export interface InboxItem {
  live: LiveRef;
  kind: InboxKind;
  title: string;
  project: string;
  body: string;
  at_unix_ms: number;
  armed: boolean;
}

export interface Inbox {
  entries: InboxItem[];
  warns: boolean;
  badge: string | null;
}

export interface GitCounts {
  branch: string;
  ahead: number;
  behind: number;
  staged: number;
  unstaged: number;
  untracked: number;
}

export interface AgentCounts {
  running: number;
  done: number;
  failed: number;
  total: number;
}

export interface Card {
  session: string;
  model: string | null;
  thinking: string | null;
  context_tokens: number | null;
  input_tokens: number;
  output_tokens: number;
  cost: number | null;
  last_reply: string | null;
  last_turn_failed: boolean;
  git: GitCounts | null;
  agents: AgentCounts;
}

/**
 * A transcript item of a type this client does not know (a newer gateway). It takes the
 * item's place so `FeedValue.items` keeps its indices (`start + i`); render it as nothing or a
 * placeholder. Its tag is not a gateway tag, so it never matches a real {@link Item} variant.
 */
export interface UnknownItem {
  type: 'unknown_item';
  /** The gateway's `type` string. */
  name: string;
}

/** A transcript item (tag `type`), or an {@link UnknownItem}. */
export type Item =
  | UnknownItem
  | { type: 'user'; text: string; from_agent: boolean; at_unix_ms: number | null; images?: ImageRef[] }
  | { type: 'assistant'; text: string; model: string | null; at_unix_ms: number | null }
  | { type: 'thinking'; text: string }
  | {
      type: 'tool_call';
      call_id: string;
      name: string;
      intent: string | null;
      summary: string;
      args: string;
      /** Slice 4, absent when too large or not recorded. */
      args_value?: JsonValue;
    }
  | {
      type: 'tool_result';
      call_id: string;
      name: string;
      text: string;
      is_error: boolean;
      truncated: boolean;
      details?: JsonValue;
      images?: ImageRef[];
    }
  | { type: 'notice'; kind: string; text: string }
  | { type: 'irc'; from: string | null; agent: string | null; text: string; note: string | null }
  | { type: 'compaction'; tokens_before: number | null; tokens_after: number | null; summary: string }
  | { type: 'model_change'; model: string }
  | { type: 'error'; message: string }
  | { type: 'exit'; reason: string };

export type ChangeKind = 'modified' | 'added' | 'deleted' | 'renamed' | 'copied' | 'type_changed';

export interface ChangedFile {
  /** Repo-relative, `/`-separated (untracked directories end in `/`). */
  path: string;
  orig_path: string | null;
  staged: ChangeKind | null;
  unstaged: ChangeKind | null;
  untracked: boolean;
  conflicted: boolean;
  is_dir: boolean;
}

export interface Changes {
  branch: string | null;
  files: ChangedFile[];
}

export interface BackupDestinationView {
  kind: 'drive' | 'ftp' | 'ftps' | 'sftp';
  label: string;
}

/** A feed's value (tag `feed`). */
export type FeedValue =
  | { feed: 'projects'; projects: Project[]; truncated: boolean }
  /** `total` sessions exist; `sessions` holds the newest (at most the feed's `limit`). */
  | { feed: 'sessions'; project: string; total: number; sessions: SessionSummary[]; truncated: boolean }
  | { feed: 'live'; sessions: LiveSession[]; truncated: boolean }
  | { feed: 'inbox'; inbox: Inbox; truncated: boolean }
  | { feed: 'cards'; cards: Card[]; truncated: boolean }
  /** Items `start..start + items.length`; `reset` = forget earlier items. */
  | { feed: 'transcript'; start: number; items: Item[]; reset: boolean }
  | { feed: 'changes'; changes: Changes | null; error: string | null; truncated: boolean }
  // slice 4
  | { feed: 'running'; sessions: RunningView[]; server_bytes: number | null; truncated: boolean }
  | { feed: 'inventory'; project: string | null; total: number; entries: InventoryView[]; truncated: boolean }
  | { feed: 'context'; live: LiveRef; context: ContextView | null }
  | {
      feed: 'usage';
      usage: UsageView | null;
      fetched_unix_ms: number | null;
      error: string | null;
      fetching: boolean;
      truncated: boolean;
    }
  | { feed: 'agents'; session: string; agents: AgentView[]; truncated: boolean }
  | { feed: 'worktrees'; project: string; worktree: WorktreeView | null; parked: ParkedView[]; truncated: boolean }
  | {
      feed: 'backup';
      destination: BackupDestinationView;
      available: boolean;
      connected: boolean;
      account: string | null;
      last_backup_unix_ms: number | null;
      interval: SyncInterval;
      busy: string | null;
      error: string | null;
    }
  | { feed: 'sync'; rows: SyncRowView[]; interval: SyncInterval; busy: string | null; truncated: boolean }
  | { feed: 'updates'; omp: OmpUpdateView; app: AppUpdateView }
  | {
      feed: 'device';
      name: string;
      permissions: Permissions;
      push: PushStateView;
      gateway_version: string;
      mode: GatewayMode;
    };

export type MuxBackend = 'tmux' | 'host';

/** A running mux session. */
export interface RunningView {
  /** Running id (`r-` + 16 hex). */
  id: string;
  sessions: string[];
  project: string | null;
  display: string | null;
  title: string | null;
  backend: MuxBackend;
  started_unix_ms: number | null;
  pid: number | null;
  memory_bytes: number | null;
  processes: number | null;
  live: LiveRef | null;
}

export interface InventoryStats {
  user_messages: number;
  assistant_turns: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  cost: number | null;
  subagent_cost: number | null;
  model: string | null;
  compactions: number;
  last_activity_unix_ms: number | null;
}

/** One session file. */
export interface InventoryView {
  id: string;
  project: string;
  display: string;
  title: string | null;
  first_prompt: string | null;
  stub: boolean;
  created_unix_ms: number | null;
  modified_unix_ms: number;
  jsonl_bytes: number;
  artifact_bytes: number;
  artifact_files: number;
  subagents: number;
  parent: string | null;
  cwd_exists: boolean;
  running: boolean;
  stats: InventoryStats | null;
}

export interface ContextCategory {
  id: string;
  label: string;
  tokens: number;
}

export interface ToolTokens {
  name: string;
  tokens: number;
}

/** omp's `/context` breakdown. */
export interface ContextView {
  model: string | null;
  context_window: number;
  used_tokens: number;
  anchored: boolean;
  categories: ContextCategory[];
  auto_compact_buffer: number;
  free_tokens: number;
  threshold_tokens: number | null;
  snapcompact_saved: number | null;
  tools: ToolTokens[];
  skills: number;
  computed_unix_ms: number;
}

export interface UsageLimitView {
  id: string;
  label: string;
  window: string | null;
  resets_unix_ms: number | null;
  used: number | null;
  limit: number | null;
  remaining: number | null;
  used_fraction: number | null;
  remaining_fraction: number | null;
  unit: string;
  status: string;
}

export interface UsageReportView {
  provider: string;
  account: string | null;
  plan: string | null;
  fetched_unix_ms: number | null;
  limits: UsageLimitView[];
  notes: string[];
}

export interface UsageView {
  reports: UsageReportView[];
  tightest_remaining: number | null;
  unavailable: string[];
}

export type AgentStatus = 'running' | 'done' | 'failed' | 'aborted' | 'interrupted' | 'parked';

/** A subagent. */
export interface AgentView {
  id: string;
  parent: string | null;
  depth: number;
  label: string;
  agent: string | null;
  status: AgentStatus;
  model: string | null;
  thinking: string | null;
  error: string | null;
  current: string | null;
  created_unix_ms: number | null;
  last_activity_unix_ms: number | null;
  duration_ms: number | null;
  requests: number;
  tool_calls: number;
  tokens: number;
  cost: number | null;
  context_tokens: number | null;
  has_output: boolean;
  live: boolean;
}

export interface WorktreeView {
  linked: boolean;
  branch: string | null;
  base: string | null;
  primary: string | null;
  primary_display: string;
  branches: string[];
  has_commits: boolean;
}

/** A parked branch. */
export interface ParkedView {
  id: string;
  branch: string;
  base: string;
  sessions: number;
  parked_unix_ms: number;
  exists: boolean;
  merged: boolean;
  checked_out: boolean;
}

export type SyncInterval = 'off' | 'ten_minutes' | 'thirty_minutes' | 'hourly' | 'six_hours' | 'daily';

export interface SyncRowView {
  project: string;
  display: string;
  parts: SyncParts;
  excluded: string[];
  last_backup_unix_ms: number | null;
}

export interface OmpUpdateView {
  version: string | null;
  current: string | null;
  latest: string | null;
  error: string | null;
  checked_unix_ms: number | null;
  checking: boolean;
}

export type InstallKind = 'user_dir' | 'app_image' | 'deb' | 'rpm' | 'windows_dir' | 'dev' | 'unsupported';

/** OmpDeck's own update (tag `state`). */
export type UpdatePhase =
  | { state: 'idle' }
  | { state: 'checking' }
  | { state: 'downloading'; done: number; total: number | null }
  | { state: 'applied' }
  | { state: 'needs_manual' }
  | { state: 'failed'; message: string };

export interface AppUpdateView {
  install: InstallKind;
  apply_allowed: boolean;
  current: string;
  latest: string | null;
  page_url: string | null;
  asset: string | null;
  checked_unix_ms: number | null;
  phase: UpdatePhase;
}

export interface PushStateView {
  subscribed: boolean;
  prefs: PushPrefs;
  since_unix_ms: number | null;
}

export type GatewayMode = 'off' | 'while_open' | 'background';

export interface Diff {
  text: string;
  binary: boolean;
  truncated: boolean;
  added: number;
  removed: number;
}

export interface Model {
  selector: string;
  provider: string;
  id: string;
  name: string;
  context_window: number | null;
  max_tokens: number | null;
  reasoning: boolean | null;
  /** Effort levels; empty = no thinking. */
  thinking: string[];
  input: string[];
  cost_input: number | null;
  cost_output: number | null;
}

// ---- replies --------------------------------------------------------------------------------

export interface SessionResult {
  session: string;
  error: string | null;
}

/** A final result (tag `type`). */
export type DoneView =
  | { type: 'unit' }
  | { type: 'session'; live: LiveRef }
  | { type: 'submitted'; request: number; label: string; delivery: DeliveryView }
  | { type: 'answered'; applied: boolean }
  | { type: 'deleted'; results: SessionResult[] }
  | { type: 'file'; session: string | null }
  // slice 4
  | { type: 'text'; text: string }
  | { type: 'killed'; results: KillResult[] }
  | { type: 'restore_plan'; handle: number; plan: RestorePlanView; truncated: boolean };

/** Per running id, `null` = killed. */
export interface KillResult {
  id: string;
  error: string | null;
}

export interface LocalCopyView {
  project: string | null;
  display: string | null;
  modified_unix_ms: number;
  bytes: number;
}

/** A session a project restore adds; non-empty `local` = a conflict. */
export interface SessionPlanView {
  id: string;
  title: string;
  modified_unix_ms: number;
  bytes: number;
  local: LocalCopyView[];
  problem: string | null;
}

/** What a prepared restore would change (tag `type`); paths are `~` displays. */
export type RestorePlanView =
  | {
      type: 'backup';
      name: string;
      created_unix_ms: number | null;
      host: string;
      replaced: string[];
      added: string[];
      dirs: string[];
      removed: string[];
      safety_display: string;
    }
  | {
      type: 'project';
      target_display: string;
      create_target: boolean;
      omp_exists: boolean;
      omp_overwritten: string[];
      omp_added: string[];
      omp_skipped: string[];
      memory_exists: boolean;
      memory_overwritten: string[];
      memory_added: string[];
      memory_skipped: string[];
      sessions: SessionPlanView[];
    };

export type TreeNodeKind =
  | 'user'
  | 'assistant'
  | 'tool_result'
  | 'developer'
  | 'bash_execution'
  | 'other_role'
  | 'custom_message'
  | 'compaction'
  | 'branch_summary'
  | 'model_change'
  | 'model_usage'
  | 'thinking_change'
  | 'label'
  | 'custom'
  | 'setting';

/** One session-tree entry. */
export interface TreeNode {
  id: string;
  parent: string | null;
  kind: TreeNodeKind;
  subtype: string | null;
  at_unix_ms: number | null;
  /** At most {@link TREE_TEXT_CHARS} characters. */
  text: string;
  has_text: boolean;
  stop_reason: string | null;
  label: string | null;
  forkable: boolean;
  cost_before: number | null;
  cost_incomplete: boolean;
}

/** One sub-folder of a `dir` listing. */
export interface DirEntry {
  name: string;
  /** Its dir token. */
  dir: string;
  project: string | null;
  repo: boolean;
}

export interface DayCount {
  /** `YYYY-MM-DD`. */
  day: string;
  cost: number | null;
  requests: number;
  tokens: number;
}

/** An omp setting's type (tag `type`). */
export type SettingKind =
  | { type: 'bool' }
  | { type: 'number' }
  | { type: 'enum'; options: string[] }
  | { type: 'string' }
  | { type: 'array' }
  | { type: 'record' };

/** One omp setting; a secret's `value` is `null` and `set` tells whether one is stored. */
export interface SettingView {
  key: string;
  kind: SettingKind;
  description: string;
  value: JsonValue;
  default: JsonValue;
  modified: boolean;
  secret: boolean;
  set: boolean;
}

export interface SettingsGroup {
  id: string;
  label: string;
  entries: SettingView[];
}

export interface BackupFileView {
  id: string;
  name: string;
  size: number;
  created_unix_ms: number | null;
}

export interface BackupHost {
  host: string;
  backups: BackupFileView[];
}

export interface SyncBackupView {
  id: string;
  name: string;
  host: string;
  display: string;
  size: number;
  created_unix_ms: number | null;
  omp_bytes: number | null;
  memory_bytes: number | null;
  sessions: number | null;
  session_bytes: number | null;
}

/** Why the gateway did not run a command (tag `code`). */
export type RejectedView =
  | { code: 'stale'; current: LiveRef | null }
  | { code: 'exited' }
  | { code: 'read_only' }
  | { code: 'detached' }
  | { code: 'busy' }
  | { code: 'no_bracketed_paste' }
  | { code: 'not_injected' }
  | { code: 'empty' }
  | { code: 'no_change' }
  | { code: 'model_unknown' }
  | { code: 'key_reused' }
  | { code: 'expired' }
  | { code: 'input_held' }
  | { code: 'bad_path' };

/** A request's record (tag `state`). */
export type OutcomeView =
  | { state: 'running' }
  | { state: 'done'; result: DoneView }
  | { state: 'failed'; message: string }
  | { state: 'rejected'; reason: RejectedView }
  | { state: 'cancelled' }
  | { state: 'uncertain' }
  | { state: 'recorded'; succeeded: boolean }
  | { state: 'unknown' }
  | { state: 'expired' };

/** A successful reply (tag `type`). */
export type Resp =
  | { type: 'unit' }
  | { type: 'pong' }
  | { type: 'accepted'; seq: number; duplicate: boolean }
  | { type: 'outcome'; outcome: OutcomeView }
  | { type: 'models'; models: Model[]; truncated: boolean }
  | { type: 'diff'; diff: Diff }
  | { type: 'attached'; view: number; mode: ViewMode }
  // slice 4
  | { type: 'text'; text: string; truncated: boolean }
  | { type: 'download'; download: string; name: string; mime: string; size: number }
  | { type: 'chunk'; data: string; eof: boolean }
  | { type: 'tree'; nodes: TreeNode[]; active_leaf: string | null; next_cursor: string | null; truncated: boolean }
  | { type: 'dir'; dir: string | null; display: string; parent: string | null; entries: DirEntry[]; truncated: boolean }
  | { type: 'activity'; days: DayCount[] }
  | { type: 'omp_settings'; groups: SettingsGroup[]; truncated: boolean }
  | { type: 'backup_list'; hosts: BackupHost[]; truncated: boolean }
  | { type: 'sync_backups'; backups: SyncBackupView[]; truncated: boolean }
  | { type: 'upload'; upload: string };

/** A refused request (tag `code`). */
export type ApiError =
  | { code: 'unsupported' }
  | { code: 'not_allowed'; need: Class }
  | { code: 'bad_request'; message: string }
  | { code: 'not_found'; what: string }
  | { code: 'stale'; current: LiveRef | null }
  | { code: 'rejected'; reason: RejectedView }
  | { code: 'geometry_owned' }
  | { code: 'too_many_requests' }
  | { code: 'too_large' }
  | { code: 'maintenance' }
  | { code: 'key_reused' }
  | { code: 'expired' }
  | { code: 'failed'; message: string };

export type Reply = { id: number; ok: Resp } | { id: number; err: ApiError };

// ---- events ---------------------------------------------------------------------------------

export type InputRefusal = 'not_allowed' | 'input_held' | 'exited' | 'read_only' | 'detached' | 'stale' | 'rejected';

/**
 * An event of a type this client does not know (a newer gateway), returned by `decodeEvent`
 * instead of throwing so the session survives; ignore it. Its tag is not a gateway tag, so it
 * never matches a real {@link Event} variant.
 */
export interface UnknownEvent {
  type: 'unknown_event';
  /** The gateway's `type` string. */
  name: string;
}

/** Something the device reacts to (tag `type`), or an {@link UnknownEvent}. */
export type Event =
  | UnknownEvent
  | { type: 'feed'; feed: Feed; value: FeedValue }
  | { type: 'outcome'; seq: number; outcome: OutcomeView }
  | { type: 'progress'; seq: number; stage: string; done: number; total: number | null }
  | { type: 'confirm'; live: LiveRef; request: number; label: string; draft: string }
  | { type: 'term_state'; view: number; input_held: boolean }
  | { type: 'input_refused'; view: number; reason: InputRefusal }
  | { type: 'model_unconfirmed'; live: LiveRef; selector: string | null; level: string | null }
  | { type: 'resync' };

/** Why a terminal view ended (`T_END`, tag `reason`). */
export type TermEnd =
  | { reason: 'exited'; code: number | null }
  | { reason: 'closed' }
  | { reason: 'stale' }
  | { reason: 'overflow' }
  | { reason: 'disconnected' }
  | { reason: 'shutdown' }
  | { reason: 'detached' }
  | { reason: 'not_allowed' }
  | { reason: 'too_large' };

// ---- runtime validation ---------------------------------------------------------------------

/** A gateway JSON body that is not valid UTF-8 JSON of the expected shape. */
export class ApiDecodeError extends Error {
  readonly path: string;

  constructor(path: string, message: string) {
    super(`${path}: ${message}`);
    this.name = 'ApiDecodeError';
    this.path = path;
  }
}

/** Validates `value` (at JSON `path`) and returns a fresh value holding only known fields. */
type V<T> = (value: unknown, path: string) => T;
type Fields<T> = { [K in keyof T]-?: V<T[K]> };

function isObject(value: unknown): value is { [key: string]: unknown } {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

const str: V<string> = (value, path) => {
  if (typeof value !== 'string') throw new ApiDecodeError(path, 'expected a string');
  return value;
};

const bool: V<boolean> = (value, path) => {
  if (typeof value !== 'boolean') throw new ApiDecodeError(path, 'expected a boolean');
  return value;
};

function int(min: number, max: number): V<number> {
  return (value, path) => {
    if (typeof value !== 'number' || !Number.isInteger(value) || value < min || value > max) {
      throw new ApiDecodeError(path, `expected an integer in ${min}..=${max}`);
    }
    return value;
  };
}

/** u64 as far as JavaScript numbers are exact. */
const u64 = int(0, Number.MAX_SAFE_INTEGER);
const u32 = int(0, 0xffff_ffff);
const i32 = int(-0x8000_0000, 0x7fff_ffff);

const f64: V<number> = (value, path) => {
  if (typeof value !== 'number' || !Number.isFinite(value)) throw new ApiDecodeError(path, 'expected a number');
  return value;
};

/** `null` (or a missing field) → `null`. */
function nullable<T>(inner: V<T>): V<T | null> {
  return (value, path) => (value === null || value === undefined ? null : inner(value, path));
}

/**
 * A field slice 4 added to an R5 type, which Rust omits when empty: a missing one stays missing
 * ({@link readFields} leaves it out, so the decoded object matches the wire), a present one is
 * read by `inner`. Only for `readFields` checkers: it returns `undefined` despite `V<T>`.
 */
function optional<T>(inner: V<T>): V<T> {
  return ((value: unknown, path: string) => (value === undefined ? undefined : inner(value, path))) as V<T>;
}

/** UTF-8 byte order (= code point order), how Rust's `serde_json::Map` sorts its keys. */
function byCodePoint(a: string, b: string): number {
  const left = [...a];
  const right = [...b];
  for (let i = 0; i < Math.min(left.length, right.length); i++) {
    const diff = left[i]!.codePointAt(0)! - right[i]!.codePointAt(0)!;
    if (diff !== 0) return diff;
  }
  return left.length - right.length;
}

/** Any JSON value (Rust `serde_json::Value`), copied with object keys in Rust's sorted order. */
const jsonValue: V<JsonValue> = (value, path) => {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return value;
  if (typeof value === 'number') return f64(value, path);
  if (Array.isArray(value)) return value.map((item, i) => jsonValue(item, `${path}[${i}]`));
  if (!isObject(value)) throw new ApiDecodeError(path, 'expected a JSON value');
  const out: { [key: string]: JsonValue } = {};
  for (const key of Object.keys(value).sort(byCodePoint)) {
    // defineProperty, so a `__proto__` key stays an own data property.
    Object.defineProperty(out, key, {
      value: jsonValue(value[key], `${path}.${key}`),
      enumerable: true,
      writable: true,
      configurable: true,
    });
  }
  return out;
};

function list<T>(inner: V<T>): V<T[]> {
  return (value, path) => {
    if (!Array.isArray(value)) throw new ApiDecodeError(path, 'expected an array');
    return value.map((item, i) => inner(item, `${path}[${i}]`));
  };
}

function oneOf<const T extends string>(values: readonly T[]): V<T> {
  const known: ReadonlySet<string> = new Set(values);
  return (value, path) => {
    if (typeof value !== 'string' || !known.has(value)) {
      throw new ApiDecodeError(path, `expected one of ${values.join(', ')}`);
    }
    return value as T; // membership checked above
  };
}

/** Fields a validator leaves `undefined` ({@link optional}) stay absent. */
function readFields(value: { [key: string]: unknown }, fields: { [key: string]: V<unknown> }, path: string, out: { [key: string]: unknown }): void {
  for (const [name, check] of Object.entries(fields)) {
    const field = check(value[name], `${path}.${name}`);
    if (field !== undefined) out[name] = field;
  }
}

function struct<T>(fields: Fields<T>): V<T> {
  return (value, path) => {
    if (!isObject(value)) throw new ApiDecodeError(path, 'expected an object');
    const out: { [key: string]: unknown } = {};
    readFields(value, fields, path, out);
    return out as T; // every field of T was validated by its checker
  };
}

type Variants<U, Tag extends string> = {
  [K in U extends { [T in Tag]: infer N extends string } ? N : never]: Fields<
    Omit<Extract<U, { [T in Tag]: K }>, Tag>
  >;
};

/**
 * A tagged union. `unknown` (with `X` the variant it builds) answers a tag string outside
 * `variants` instead of throwing; a non-string tag still throws.
 */
function tagged<U, const Tag extends string, X extends U = never>(
  tag: Tag,
  variants: Variants<Exclude<U, X>, Tag>,
  unknown?: (name: string) => X,
): V<U> {
  const table: { [name: string]: { [key: string]: V<unknown> } } = variants;
  return (value, path) => {
    if (!isObject(value)) throw new ApiDecodeError(path, 'expected an object');
    const name = value[tag];
    if (typeof name === 'string' && unknown !== undefined && !Object.hasOwn(table, name)) {
      return unknown(name);
    }
    if (typeof name !== 'string' || !Object.hasOwn(table, name)) {
      throw new ApiDecodeError(`${path}.${tag}`, `unknown variant ${JSON.stringify(name)}`);
    }
    const out: { [key: string]: unknown } = { [tag]: name };
    readFields(value, table[name]!, path, out);
    return out as U; // the variant's fields were validated by its checkers
  };
}

const liveRef = struct<LiveRef>({ key: str, generation: u64 });
const permissions = struct<Permissions>({ control: bool, manage: bool });
const limits = struct<Limits>({
  max_message: u32,
  window_bytes: u32,
  window_messages: u32,
  max_inflight: u32,
  max_views: u32,
  max_feeds: u32,
});
const welcome = struct<Welcome>({
  protocol: u32,
  gateway_version: str,
  epoch: str,
  device: str,
  name: str,
  permissions,
  next_seq: u64,
  limits,
  features: list(str),
  push_key: optional(str),
});

const classV = oneOf<Class>(['view', 'control', 'manage']);
const viewMode = oneOf<ViewMode>(['observe', 'input', 'control']);

/** A subscription: in requests, and echoed by the gateway in `Event.feed`. */
const feed = tagged<Feed, 'feed'>('feed', {
  projects: {},
  sessions: { project: str, limit: u32 },
  live: {},
  inbox: {},
  cards: { sessions: list(str) },
  transcript: { session: str, agent: nullable(str), tail: u32 },
  changes: { project: str },
  running: {},
  inventory: { project: nullable(str), limit: u32 },
  context: { live: liveRef },
  usage: {},
  agents: { session: str },
  worktrees: { project: str },
  backup: {},
  sync: {},
  updates: {},
  device: {},
});

// Slice-4 shapes of both directions, ahead of their users.
const diffSide = oneOf<DiffSide>(['staged', 'unstaged', 'untracked']);
const syncParts = struct<SyncParts>({ omp: bool, sessions: bool, memory: bool });
const pushPrefs = struct<PushPrefs>({ turn_finished: bool, attention: bool, ended: bool, show_titles: bool });
const treeFilter = oneOf<TreeFilter>(['conversation', 'default', 'all', 'labeled']);
const imageRef = struct<ImageRef>({ blob: str, mime: str, bytes: u64 });
const uiOption = struct<UiOption>({ label: str, description: nullable(str) });
const uiRequestView = struct<UiRequestView>({
  request: str,
  kind: oneOf<UiKind>(['select', 'confirm', 'input', 'editor', 'ask', 'approve']),
  title: str,
  message: nullable(str),
  timeout_ms: nullable(u64),
  options: list(uiOption),
  multi: bool,
  initial: nullable(u32),
  prefill: nullable(str),
  questions: list(
    struct<UiQuestion>({
      id: str,
      question: str,
      header: nullable(str),
      options: list(uiOption),
      multi: bool,
      recommended: nullable(u32),
    }),
  ),
  approve: nullable(
    struct<ApproveView>({
      full: str,
      compact: str,
      session_label: nullable(str),
      grants: list(struct<ApproveGrant>({ scope: str, label: str, always_label: str })),
    }),
  ),
});
const runningView = struct<RunningView>({
  id: str,
  sessions: list(str),
  project: nullable(str),
  display: nullable(str),
  title: nullable(str),
  backend: oneOf<MuxBackend>(['tmux', 'host']),
  started_unix_ms: nullable(u64),
  pid: nullable(u32),
  memory_bytes: nullable(u64),
  processes: nullable(u32),
  live: nullable(liveRef),
});
const inventoryView = struct<InventoryView>({
  id: str,
  project: str,
  display: str,
  title: nullable(str),
  first_prompt: nullable(str),
  stub: bool,
  created_unix_ms: nullable(u64),
  modified_unix_ms: u64,
  jsonl_bytes: u64,
  artifact_bytes: u64,
  artifact_files: u32,
  subagents: u32,
  parent: nullable(str),
  cwd_exists: bool,
  running: bool,
  stats: nullable(
    struct<InventoryStats>({
      user_messages: u32,
      assistant_turns: u32,
      input_tokens: u64,
      output_tokens: u64,
      cache_read_tokens: u64,
      cache_write_tokens: u64,
      cost: nullable(f64),
      subagent_cost: nullable(f64),
      model: nullable(str),
      compactions: u32,
      last_activity_unix_ms: nullable(u64),
    }),
  ),
});
const contextView = struct<ContextView>({
  model: nullable(str),
  context_window: u64,
  used_tokens: u64,
  anchored: bool,
  categories: list(struct<ContextCategory>({ id: str, label: str, tokens: u64 })),
  auto_compact_buffer: u64,
  free_tokens: u64,
  threshold_tokens: nullable(u64),
  snapcompact_saved: nullable(u64),
  tools: list(struct<ToolTokens>({ name: str, tokens: u64 })),
  skills: u32,
  computed_unix_ms: u64,
});
const usageView = struct<UsageView>({
  reports: list(
    struct<UsageReportView>({
      provider: str,
      account: nullable(str),
      plan: nullable(str),
      fetched_unix_ms: nullable(u64),
      limits: list(
        struct<UsageLimitView>({
          id: str,
          label: str,
          window: nullable(str),
          resets_unix_ms: nullable(u64),
          used: nullable(f64),
          limit: nullable(f64),
          remaining: nullable(f64),
          used_fraction: nullable(f64),
          remaining_fraction: nullable(f64),
          unit: str,
          status: str,
        }),
      ),
      notes: list(str),
    }),
  ),
  tightest_remaining: nullable(f64),
  unavailable: list(str),
});
const dayCount = struct<DayCount>({ day: str, cost: nullable(f64), requests: u64, tokens: u64 });
const agentView = struct<AgentView>({
  id: str,
  parent: nullable(str),
  depth: u32,
  label: str,
  agent: nullable(str),
  status: oneOf<AgentStatus>(['running', 'done', 'failed', 'aborted', 'interrupted', 'parked']),
  model: nullable(str),
  thinking: nullable(str),
  error: nullable(str),
  current: nullable(str),
  created_unix_ms: nullable(u64),
  last_activity_unix_ms: nullable(u64),
  duration_ms: nullable(u64),
  requests: u32,
  tool_calls: u32,
  tokens: u64,
  cost: nullable(f64),
  context_tokens: nullable(u64),
  has_output: bool,
  live: bool,
});
const worktreeView = struct<WorktreeView>({
  linked: bool,
  branch: nullable(str),
  base: nullable(str),
  primary: nullable(str),
  primary_display: str,
  branches: list(str),
  has_commits: bool,
});
const parkedView = struct<ParkedView>({
  id: str,
  branch: str,
  base: str,
  sessions: u32,
  parked_unix_ms: u64,
  exists: bool,
  merged: bool,
  checked_out: bool,
});
const syncInterval = oneOf<SyncInterval>(['off', 'ten_minutes', 'thirty_minutes', 'hourly', 'six_hours', 'daily']);
const syncRowView = struct<SyncRowView>({
  project: str,
  display: str,
  parts: syncParts,
  excluded: list(str),
  last_backup_unix_ms: nullable(u64),
});
const ompUpdateView = struct<OmpUpdateView>({
  version: nullable(str),
  current: nullable(str),
  latest: nullable(str),
  error: nullable(str),
  checked_unix_ms: nullable(u64),
  checking: bool,
});
const appUpdateView = struct<AppUpdateView>({
  install: oneOf<InstallKind>(['user_dir', 'app_image', 'deb', 'rpm', 'windows_dir', 'dev', 'unsupported']),
  apply_allowed: bool,
  current: str,
  latest: nullable(str),
  page_url: nullable(str),
  asset: nullable(str),
  checked_unix_ms: nullable(u64),
  phase: tagged<UpdatePhase, 'state'>('state', {
    idle: {},
    checking: {},
    downloading: { done: u64, total: nullable(u64) },
    applied: {},
    needs_manual: {},
    failed: { message: str },
  }),
});
const pushStateView = struct<PushStateView>({ subscribed: bool, prefs: pushPrefs, since_unix_ms: nullable(u64) });
const gatewayMode = oneOf<GatewayMode>(['off', 'while_open', 'background']);
const treeNode = struct<TreeNode>({
  id: str,
  parent: nullable(str),
  kind: oneOf<TreeNodeKind>([
    'user',
    'assistant',
    'tool_result',
    'developer',
    'bash_execution',
    'other_role',
    'custom_message',
    'compaction',
    'branch_summary',
    'model_change',
    'model_usage',
    'thinking_change',
    'label',
    'custom',
    'setting',
  ]),
  subtype: nullable(str),
  at_unix_ms: nullable(u64),
  text: str,
  has_text: bool,
  stop_reason: nullable(str),
  label: nullable(str),
  forkable: bool,
  cost_before: nullable(f64),
  cost_incomplete: bool,
});
const dirEntry = struct<DirEntry>({ name: str, dir: str, project: nullable(str), repo: bool });
const settingsGroup = struct<SettingsGroup>({
  id: str,
  label: str,
  entries: list(
    struct<SettingView>({
      key: str,
      kind: tagged<SettingKind, 'type'>('type', {
        bool: {},
        number: {},
        enum: { options: list(str) },
        string: {},
        array: {},
        record: {},
      }),
      description: str,
      value: jsonValue,
      default: jsonValue,
      modified: bool,
      secret: bool,
      set: bool,
    }),
  ),
});
const backupHost = struct<BackupHost>({
  host: str,
  backups: list(struct<BackupFileView>({ id: str, name: str, size: u64, created_unix_ms: nullable(u64) })),
});
const syncBackupView = struct<SyncBackupView>({
  id: str,
  name: str,
  host: str,
  display: str,
  size: u64,
  created_unix_ms: nullable(u64),
  omp_bytes: nullable(u64),
  memory_bytes: nullable(u64),
  sessions: nullable(u64),
  session_bytes: nullable(u64),
});
const restorePlanView = tagged<RestorePlanView, 'type'>('type', {
  backup: {
    name: str,
    created_unix_ms: nullable(u64),
    host: str,
    replaced: list(str),
    added: list(str),
    dirs: list(str),
    removed: list(str),
    safety_display: str,
  },
  project: {
    target_display: str,
    create_target: bool,
    omp_exists: bool,
    omp_overwritten: list(str),
    omp_added: list(str),
    omp_skipped: list(str),
    memory_exists: bool,
    memory_overwritten: list(str),
    memory_added: list(str),
    memory_skipped: list(str),
    sessions: list(
      struct<SessionPlanView>({
        id: str,
        title: str,
        modified_unix_ms: u64,
        bytes: u64,
        local: list(
          struct<LocalCopyView>({
            project: nullable(str),
            display: nullable(str),
            modified_unix_ms: u64,
            bytes: u64,
          }),
        ),
        problem: nullable(str),
      }),
    ),
  },
});

const sessionSummary = struct<SessionSummary>({
  id: str,
  title: nullable(str),
  first_prompt: nullable(str),
  modified_unix_ms: u64,
  stub: optional(bool),
});
const project = struct<Project>({
  id: str,
  display: str,
  worktree: bool,
  session_count: u32,
  modified_unix_ms: u64,
});
const status = tagged<Status, 'state'>('state', { idle: {}, busy: {}, attention: {}, exited: { clean: bool } });
const switchView = struct<SwitchView>({ selector: nullable(str), level: nullable(str) });
const delivery = tagged<DeliveryView, 'state'>('state', {
  queued: {},
  awaiting_confirm: { draft: str },
  writing: {},
  delivered: {},
  refused: { reason: oneOf<Refusal>(['precondition', 'dialog_open', 'no_bracketed_paste']) },
  declined: {},
  aborted: {},
  stalled: {},
  cancelled: { reason: oneOf<CancelReason>(['exited', 'detached', 'closing', 'session_changed', 'not_allowed']) },
  unobserved: {},
});
const ownRequest = struct<OwnRequest>({ request: u64, seq: u64, label: str, delivery });
const liveSession = struct<LiveSession>({
  live: liveRef,
  kind: oneOf<LiveKind>(['session', 'tool']),
  label: str,
  title: nullable(str),
  session: nullable(str),
  project: nullable(str),
  display: str,
  activity: oneOf<Activity>(['unknown', 'idle', 'busy', 'attention']),
  status,
  exit_code: nullable(i32),
  busy: bool,
  model: nullable(str),
  pending_switch: nullable(switchView),
  live_control: bool,
  desktop_controlled: bool,
  views: u32,
  requests: list(ownRequest),
  pending_ui: optional(list(uiRequestView)),
});
const inboxKind = tagged<InboxKind, 'type'>('type', {
  finished: {},
  error: { message: nullable(str) },
  attention: {},
  exited: { code: nullable(i32) },
});
const inboxItem = struct<InboxItem>({
  live: liveRef,
  kind: inboxKind,
  title: str,
  project: str,
  body: str,
  at_unix_ms: u64,
  armed: bool,
});
const inbox = struct<Inbox>({ entries: list(inboxItem), warns: bool, badge: nullable(str) });
const gitCounts = struct<GitCounts>({
  branch: str,
  ahead: u32,
  behind: u32,
  staged: u32,
  unstaged: u32,
  untracked: u32,
});
const agentCounts = struct<AgentCounts>({ running: u32, done: u32, failed: u32, total: u32 });
const card = struct<Card>({
  session: str,
  model: nullable(str),
  thinking: nullable(str),
  context_tokens: nullable(u64),
  input_tokens: u64,
  output_tokens: u64,
  cost: nullable(f64),
  last_reply: nullable(str),
  last_turn_failed: bool,
  git: nullable(gitCounts),
  agents: agentCounts,
});
const item = tagged<Item, 'type', UnknownItem>(
  'type',
  {
    user: { text: str, from_agent: bool, at_unix_ms: nullable(u64), images: optional(list(imageRef)) },
    assistant: { text: str, model: nullable(str), at_unix_ms: nullable(u64) },
    thinking: { text: str },
    tool_call: {
      call_id: str,
      name: str,
      intent: nullable(str),
      summary: str,
      args: str,
      args_value: optional(jsonValue),
    },
    tool_result: {
      call_id: str,
      name: str,
      text: str,
      is_error: bool,
      truncated: bool,
      details: optional(jsonValue),
      images: optional(list(imageRef)),
    },
    notice: { kind: str, text: str },
    irc: { from: nullable(str), agent: nullable(str), text: str, note: nullable(str) },
    compaction: { tokens_before: nullable(u64), tokens_after: nullable(u64), summary: str },
    model_change: { model: str },
    error: { message: str },
    exit: { reason: str },
  },
  (name) => ({ type: 'unknown_item', name }),
);
const changeKind = oneOf<ChangeKind>(['modified', 'added', 'deleted', 'renamed', 'copied', 'type_changed']);
const changedFile = struct<ChangedFile>({
  path: str,
  orig_path: nullable(str),
  staged: nullable(changeKind),
  unstaged: nullable(changeKind),
  untracked: bool,
  conflicted: bool,
  is_dir: bool,
});
const changes = struct<Changes>({ branch: nullable(str), files: list(changedFile) });
const feedValue = tagged<FeedValue, 'feed'>('feed', {
  projects: { projects: list(project), truncated: bool },
  sessions: { project: str, total: u32, sessions: list(sessionSummary), truncated: bool },
  live: { sessions: list(liveSession), truncated: bool },
  inbox: { inbox, truncated: bool },
  cards: { cards: list(card), truncated: bool },
  transcript: { start: u64, items: list(item), reset: bool },
  changes: { changes: nullable(changes), error: nullable(str), truncated: bool },
  running: { sessions: list(runningView), server_bytes: nullable(u64), truncated: bool },
  inventory: { project: nullable(str), total: u32, entries: list(inventoryView), truncated: bool },
  context: { live: liveRef, context: nullable(contextView) },
  usage: {
    usage: nullable(usageView),
    fetched_unix_ms: nullable(u64),
    error: nullable(str),
    fetching: bool,
    truncated: bool,
  },
  agents: { session: str, agents: list(agentView), truncated: bool },
  worktrees: { project: str, worktree: nullable(worktreeView), parked: list(parkedView), truncated: bool },
  backup: {
    destination: struct<BackupDestinationView>({ kind: oneOf(['drive', 'ftp', 'ftps', 'sftp']), label: str }),
    available: bool,
    connected: bool,
    account: nullable(str),
    last_backup_unix_ms: nullable(u64),
    interval: syncInterval,
    busy: nullable(str),
    error: nullable(str),
  },
  sync: { rows: list(syncRowView), interval: syncInterval, busy: nullable(str), truncated: bool },
  updates: { omp: ompUpdateView, app: appUpdateView },
  device: { name: str, permissions, push: pushStateView, gateway_version: str, mode: gatewayMode },
});

const diff = struct<Diff>({ text: str, binary: bool, truncated: bool, added: u32, removed: u32 });
const model = struct<Model>({
  selector: str,
  provider: str,
  id: str,
  name: str,
  context_window: nullable(u64),
  max_tokens: nullable(u64),
  reasoning: nullable(bool),
  thinking: list(str),
  input: list(str),
  cost_input: nullable(f64),
  cost_output: nullable(f64),
});
const sessionResult = struct<SessionResult>({ session: str, error: nullable(str) });
const doneView = tagged<DoneView, 'type'>('type', {
  unit: {},
  session: { live: liveRef },
  submitted: { request: u64, label: str, delivery },
  answered: { applied: bool },
  deleted: { results: list(sessionResult) },
  file: { session: nullable(str) },
  text: { text: str },
  killed: { results: list(struct<KillResult>({ id: str, error: nullable(str) })) },
  restore_plan: { handle: u64, plan: restorePlanView, truncated: bool },
});
const rejectedView = tagged<RejectedView, 'code'>('code', {
  stale: { current: nullable(liveRef) },
  exited: {},
  read_only: {},
  detached: {},
  busy: {},
  no_bracketed_paste: {},
  not_injected: {},
  empty: {},
  no_change: {},
  model_unknown: {},
  key_reused: {},
  expired: {},
  input_held: {},
  bad_path: {},
});
const outcomeView = tagged<OutcomeView, 'state'>('state', {
  running: {},
  done: { result: doneView },
  failed: { message: str },
  rejected: { reason: rejectedView },
  cancelled: {},
  uncertain: {},
  recorded: { succeeded: bool },
  unknown: {},
  expired: {},
});
const resp = tagged<Resp, 'type'>('type', {
  unit: {},
  pong: {},
  accepted: { seq: u64, duplicate: bool },
  outcome: { outcome: outcomeView },
  models: { models: list(model), truncated: bool },
  diff: { diff },
  attached: { view: u32, mode: viewMode },
  text: { text: str, truncated: bool },
  download: { download: str, name: str, mime: str, size: u64 },
  chunk: { data: str, eof: bool },
  tree: { nodes: list(treeNode), active_leaf: nullable(str), next_cursor: nullable(str), truncated: bool },
  dir: { dir: nullable(str), display: str, parent: nullable(str), entries: list(dirEntry), truncated: bool },
  activity: { days: list(dayCount) },
  omp_settings: { groups: list(settingsGroup), truncated: bool },
  backup_list: { hosts: list(backupHost), truncated: bool },
  sync_backups: { backups: list(syncBackupView), truncated: bool },
  upload: { upload: str },
});
const apiError = tagged<ApiError, 'code'>('code', {
  unsupported: {},
  not_allowed: { need: classV },
  bad_request: { message: str },
  not_found: { what: str },
  stale: { current: nullable(liveRef) },
  rejected: { reason: rejectedView },
  geometry_owned: {},
  too_many_requests: {},
  too_large: {},
  maintenance: {},
  key_reused: {},
  expired: {},
  failed: { message: str },
});
const reply: V<Reply> = (value, path) => {
  if (!isObject(value)) throw new ApiDecodeError(path, 'expected an object');
  const id = u64(value.id, `${path}.id`);
  const hasOk = value.ok !== undefined;
  const hasErr = value.err !== undefined;
  if (hasOk === hasErr) throw new ApiDecodeError(path, 'expected exactly one of ok, err');
  return hasOk ? { id, ok: resp(value.ok, `${path}.ok`) } : { id, err: apiError(value.err, `${path}.err`) };
};
const inputRefusal = oneOf<InputRefusal>([
  'not_allowed',
  'input_held',
  'exited',
  'read_only',
  'detached',
  'stale',
  'rejected',
]);
const event = tagged<Event, 'type', UnknownEvent>(
  'type',
  {
    feed: { feed, value: feedValue },
    outcome: { seq: u64, outcome: outcomeView },
    progress: { seq: u64, stage: str, done: u64, total: nullable(u64) },
    confirm: { live: liveRef, request: u64, label: str, draft: str },
    term_state: { view: u32, input_held: bool },
    input_refused: { view: u32, reason: inputRefusal },
    model_unconfirmed: { live: liveRef, selector: nullable(str), level: nullable(str) },
    resync: {},
  },
  (name) => ({ type: 'unknown_event', name }),
);
const termEnd = tagged<TermEnd, 'reason'>('reason', {
  exited: { code: nullable(i32) },
  closed: {},
  stale: {},
  overflow: {},
  disconnected: {},
  shutdown: {},
  detached: {},
  not_allowed: {},
  too_large: {},
});

// Device → gateway shapes (as Rust's strict serde reads them; unknown fields are never written).
const actionV = tagged<SessionAction, 'type'>('type', {
  handoff: { focus: nullable(str) },
  compact: { mode: oneOf<CompactMode>(['default', 'soft', 'snapcompact', 'focus']), focus: nullable(str) },
  rename: { name: str },
  auto_name: {},
  plan: {},
  retry: {},
});
const files = list(str);
const composeMode = oneOf<ComposeMode>(['send', 'steer', 'follow_up', 'insert']);
const reviewComment = struct<ReviewComment>({ file: str, side: diffSide, start: u32, end: u32, text: str });
const restoreTarget = tagged<RestoreTarget, 'type'>('type', { project: { project: str }, dir: { dir: str } });
const conflictChoice = struct<ConflictChoice>({
  session: str,
  choice: oneOf<ConflictResolution>(['keep', 'overwrite', 'both']),
});
const approveDecision = tagged<ApproveDecision, 'type'>('type', {
  once: {},
  session: {},
  deny: {},
  grant: { index: u32, always: bool },
});
const uiAnswer = tagged<UiAnswer, 'type'>('type', {
  choice: { index: u32 },
  choices: { indices: list(u32) },
  text: { text: str },
  confirm: { yes: bool },
  ask: { answers: list(struct<UiAskAnswer>({ id: str, selected: list(u32), custom: nullable(str) })) },
  approve: { decision: approveDecision },
  cancel: {},
});
const opV = tagged<Op, 'op'>('op', {
  subscribe: { feed },
  unsubscribe: { feed },
  outcome: { seq: u64 },
  models: {},
  diff: { project: str, file: str, side: oneOf<DiffSide>(['staged', 'unstaged', 'untracked']) },
  attach: { live: liveRef, mode: viewMode },
  detach: { view: u32 },
  ping: {},
  new_session: { project: str },
  resume: { session: str },
  restart: { live: liveRef },
  stop: { live: liveRef },
  kill: { live: liveRef },
  prompt: { live: liveRef, text: str },
  interrupt: { live: liveRef },
  switch_model: { live: liveRef, selector: str, level: nullable(str) },
  set_effort: { live: liveRef, level: str },
  action: { live: liveRef, action: actionV },
  answer: { live: liveRef, request: u64, proceed: bool },
  cancel: { seq: u64 },
  inbox_open: { live: liveRef },
  inbox_dismiss: { live: liveRef },
  inbox_clear: {},
  stage: { project: str, files },
  unstage: { project: str, files },
  delete_sessions: { sessions: files },
  copy_session: { session: str, project: str },
  move_session: { session: str, project: str },
  discard: { project: str, files },
  delete_untracked: { project: str, files },
  last_reply: { session: str },
  export: { session: str },
  blob: { session: str, agent: nullable(str), blob: str },
  download: { download: str, offset: u64 },
  tree: { session: str, filter: treeFilter, cursor: nullable(str), limit: u32 },
  browse: { dir: nullable(str) },
  usage_activity: { days: u32 },
  usage_refresh: {},
  omp_settings: {},
  backup_list: {},
  sync_backups: { project: nullable(str) },
  presence: { visible: bool },
  push_subscribe: { endpoint: str, p256dh: str, auth: str },
  push_unsubscribe: {},
  push_prefs: { prefs: pushPrefs },
  device_rename: { name: str },
  unpair: {},
  upload_begin: { name: str, mime: str, size: u64 },
  upload_chunk: { upload: str, offset: u64, data: str },
  upload_cancel: { upload: str },
  compose: { live: liveRef, text: str, attachments: files, mode: composeMode },
  review: { live: liveRef, comments: list(reviewComment), general: nullable(str), send: bool },
  answer_ui: { live: liveRef, request: str, value: uiAnswer },
  agent_kill: { live: liveRef, agent: str },
  agent_revive: { live: liveRef, agent: str },
  agent_chat: { live: liveRef, agent: str, text: str },
  fork_at: { session: str, entry: str },
  navigate: { live: liveRef, entry: str },
  new_session_in: { dir: str },
  running_kill: { ids: files },
  worktree_create: { project: str, branch: str, base: nullable(str), open: bool },
  worktree_move: { live: liveRef, branch: str },
  worktree_finish: { project: str },
  worktree_continue: { parked: str, where: oneOf<ContinueWhere>(['worktree', 'main', 'switch_back']) },
  update_check: {},
  backup_run: {},
  sync_backup: { project: str },
  open_on_desktop: { session: str, split: oneOf<Split>(['none', 'right', 'down']) },
  close_on_desktop: { session: str },
  worktree_remove: { project: str, park: bool },
  omp_set: { key: str, value: jsonValue },
  omp_reset: { key: str },
  update_run: { kind: oneOf<UpdateKind>(['omp', 'plugins', 'stable', 'canary']) },
  app_update: { step: oneOf<AppUpdateStep>(['download', 'apply']) },
  backup_connect: {},
  backup_delete: { ids: files },
  backup_restore_prepare: { id: str },
  backup_restore_apply: { handle: u64 },
  backup_restore_discard: { handle: u64 },
  sync_add: { dir: str },
  sync_set: { project: str, parts: syncParts, excluded: files },
  sync_remove: { project: str },
  sync_delete: { ids: files },
  sync_restore_prepare: { backup: str, target: restoreTarget, parts: syncParts },
  sync_restore_apply: { handle: u64, conflicts: list(conflictChoice), excluded: optional(files) },
});
const requestV = struct<Request>({ id: u64, seq: nullable(u64), op: opV });

function parse<T>(body: Uint8Array, check: V<T>, what: string): T {
  let value: unknown;
  try {
    value = JSON.parse(fromUtf8(body));
  } catch {
    throw new ApiDecodeError(what, 'not UTF-8 JSON');
  }
  return check(value, what);
}

/**
 * Decodes a WELCOME body. The gateway speaks min(device `api`, its own protocol), so any
 * version from 1 up to {@link PROTOCOL} is accepted; a higher (or 0) one is refused.
 */
export function decodeWelcome(body: Uint8Array): Welcome {
  const value = parse(body, welcome, 'welcome');
  if (value.protocol < 1 || value.protocol > PROTOCOL) {
    throw new ApiDecodeError('welcome.protocol', `unsupported protocol ${value.protocol}`);
  }
  return value;
}

export function decodeReply(body: Uint8Array): Reply {
  return parse(body, reply, 'reply');
}

export function decodeEvent(body: Uint8Array): Event {
  return parse(body, event, 'event');
}

export function decodeTermEnd(body: Uint8Array): TermEnd {
  return parse(body, termEnd, 'term_end');
}

