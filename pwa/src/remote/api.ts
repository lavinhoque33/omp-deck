// The device application protocol v1, mirroring Rust `deck_remote::api`
// (crates/deck-remote/src/api.rs is the source of truth; docs/notes/mobile-relay.md, "R4
// contract"). JSON enums are internally tagged (`op`, `feed`, `type`, `state`, `code`,
// `reason`), snake_case, struct variants only; `None` is `null`.
//
// Gateway → device JSON (`Welcome`, `Reply`, `Event`, `TermEnd`) is validated at runtime:
// unknown fields are ignored, wrong types and unknown variants refused. Device → gateway JSON
// (`Request`) is built here and serialized canonically (Rust field order, `null` for absent
// optionals), so it is byte-identical to Rust's serialization.

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
}

export type Class = 'view' | 'control' | 'manage';
export type ViewMode = 'observe' | 'input' | 'control';
export type DiffSide = 'staged' | 'unstaged' | 'untracked';
export type CompactMode = 'default' | 'soft' | 'snapcompact' | 'focus';

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
  | { feed: 'changes'; project: string };

/** Operations v1 (tag `op`). */
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
  | { op: 'delete_untracked'; project: string; files: string[] };

export type OpName = Op['op'];

export interface Request {
  id: number;
  /** Required exactly for journaled ops ({@link opJournaled}). */
  seq?: number | null;
  op: Op;
}

type FieldsOf<U, Tag extends string> = {
  [K in U extends { [T in Tag]: infer N extends string } ? N : never]: readonly Exclude<
    keyof Extract<U, { [T in Tag]: K }>,
    Tag
  >[];
};

/** Field order of every op, as Rust declares (and serializes) them. */
const OP_FIELDS: FieldsOf<Op, 'op'> = {
  subscribe: ['feed'],
  unsubscribe: ['feed'],
  outcome: ['seq'],
  models: [],
  diff: ['project', 'file', 'side'],
  attach: ['live', 'mode'],
  detach: ['view'],
  ping: [],
  new_session: ['project'],
  resume: ['session'],
  restart: ['live'],
  stop: ['live'],
  kill: ['live'],
  prompt: ['live', 'text'],
  interrupt: ['live'],
  switch_model: ['live', 'selector', 'level'],
  set_effort: ['live', 'level'],
  action: ['live', 'action'],
  answer: ['live', 'request', 'proceed'],
  cancel: ['seq'],
  inbox_open: ['live'],
  inbox_dismiss: ['live'],
  inbox_clear: [],
  stage: ['project', 'files'],
  unstage: ['project', 'files'],
  delete_sessions: ['sessions'],
  copy_session: ['session', 'project'],
  move_session: ['session', 'project'],
  discard: ['project', 'files'],
  delete_untracked: ['project', 'files'],
};

const FEED_FIELDS: FieldsOf<Feed, 'feed'> = {
  projects: [],
  sessions: ['project', 'limit'],
  live: [],
  inbox: [],
  cards: ['sessions'],
  transcript: ['session', 'agent', 'tail'],
  changes: ['project'],
};

const ACTION_FIELDS: FieldsOf<SessionAction, 'type'> = {
  handoff: ['focus'],
  compact: ['mode', 'focus'],
  rename: ['name'],
  auto_name: [],
  plan: [],
  retry: [],
};

/** Every op tag. */
export const OP_NAMES: readonly OpName[] = Object.keys(OP_FIELDS) as OpName[];

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
};

/** What an op needs beyond the session itself (Rust `Op::class`). */
export function opClass(op: Op): Class {
  if (op.op === 'attach') return op.mode === 'observe' ? 'view' : 'control';
  return OP_CLASS[op.op];
}

/** Journaled (needs a `seq`): every Control/Manage op except `attach`/`detach`. */
export function opJournaled(op: Op): boolean {
  return op.op !== 'attach' && op.op !== 'detach' && opClass(op) !== 'view';
}

/** Whether `permissions` allow `op` (Rust `Op::allowed`). */
export function opAllowed(op: Op, permissions: Permissions): boolean {
  const need = opClass(op);
  return need === 'view' || (need === 'control' ? permissions.control : permissions.manage);
}

function ordered(value: { [key: string]: unknown }, tag: string, fields: readonly string[]): { [key: string]: unknown } {
  const out: { [key: string]: unknown } = { [tag]: value[tag] };
  for (const field of fields) {
    const v = value[field];
    out[field] = v === undefined ? null : field === 'live' ? liveJson(v) : nested(field, v);
  }
  return out;
}

function liveJson(value: unknown): unknown {
  const live = value as LiveRef; // typed by `Op`; re-ordered for canonical bytes
  return { key: live.key, generation: live.generation };
}

function nested(field: string, value: unknown): unknown {
  if (field === 'feed') {
    const feed = value as Feed; // typed by `Op`
    return ordered(feed, 'feed', FEED_FIELDS[feed.feed]);
  }
  if (field === 'action') {
    const action = value as SessionAction; // typed by `Op`
    return ordered(action, 'type', ACTION_FIELDS[action.type]);
  }
  return value;
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
 * {@link MAX_CARDS} card sessions, transcript `tail` ≤ {@link MAX_TAIL}). Returns the problem,
 * or null.
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
    default:
      return null;
  }
}

/**
 * Whether the gateway would parse and admit `request` rather than answer `bad_request`: every
 * field's type and integer range (u32/u64 as far as JavaScript numbers are exact), the seq rule
 * (present exactly for journaled ops) and {@link checkFeed}. Returns the problem, or null.
 */
export function checkRequest(request: Request): string | null {
  const shape = problem(() => requestV(request, 'request'));
  if (shape !== null) return shape;
  const hasSeq = request.seq !== null && request.seq !== undefined;
  if (hasSeq !== opJournaled(request.op)) {
    return `${request.op.op} ${hasSeq ? 'takes no seq' : 'needs a seq'}`;
  }
  const { op } = request;
  return op.op === 'subscribe' || op.op === 'unsubscribe' ? checkFeed(op.feed) : null;
}

/**
 * The canonical JSON text of a request (Rust field order, `null` for absent optionals). Throws
 * {@link ApiRequestError} when {@link checkRequest} fails.
 */
export function requestJson(request: Request): string {
  const bad = checkRequest(request);
  if (bad !== null) throw new ApiRequestError(bad);
  const op = ordered(request.op, 'op', OP_FIELDS[request.op.op]);
  return JSON.stringify({ id: request.id, seq: request.seq ?? null, op });
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
  modified_ms: number;
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
export type CancelReason = 'exited' | 'detached' | 'closing' | 'session_changed';

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
  at_ms: number;
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
  cost: number;
  last_reply: string | null;
  last_turn_failed: boolean;
  git: GitCounts | null;
  agents: AgentCounts;
}

/** A transcript item (tag `type`). */
export type Item =
  | { type: 'user'; text: string; from_agent: boolean; at_ms: number | null }
  | { type: 'assistant'; text: string; model: string | null; at_ms: number | null }
  | { type: 'thinking'; text: string }
  | { type: 'tool_call'; call_id: string; name: string; intent: string | null; summary: string; args: string }
  | { type: 'tool_result'; call_id: string; name: string; text: string; is_error: boolean; truncated: boolean }
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
  | { feed: 'changes'; changes: Changes | null; error: string | null; truncated: boolean };

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
  | { type: 'file'; session: string | null };

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
  | { type: 'attached'; view: number; mode: ViewMode };

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

/** Something the device reacts to (tag `type`). */
export type Event =
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

function readFields(value: { [key: string]: unknown }, fields: { [key: string]: V<unknown> }, path: string, out: { [key: string]: unknown }): void {
  for (const [name, check] of Object.entries(fields)) out[name] = check(value[name], `${path}.${name}`);
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

function tagged<U, const Tag extends string>(tag: Tag, variants: Variants<U, Tag>): V<U> {
  const table: { [name: string]: { [key: string]: V<unknown> } } = variants;
  return (value, path) => {
    if (!isObject(value)) throw new ApiDecodeError(path, 'expected an object');
    const name = value[tag];
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
});

const sessionSummary = struct<SessionSummary>({
  id: str,
  title: nullable(str),
  first_prompt: nullable(str),
  modified_ms: u64,
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
  cancelled: { reason: oneOf<CancelReason>(['exited', 'detached', 'closing', 'session_changed']) },
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
  at_ms: u64,
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
  cost: f64,
  last_reply: nullable(str),
  last_turn_failed: bool,
  git: nullable(gitCounts),
  agents: agentCounts,
});
const item = tagged<Item, 'type'>('type', {
  user: { text: str, from_agent: bool, at_ms: nullable(u64) },
  assistant: { text: str, model: nullable(str), at_ms: nullable(u64) },
  thinking: { text: str },
  tool_call: { call_id: str, name: str, intent: nullable(str), summary: str, args: str },
  tool_result: { call_id: str, name: str, text: str, is_error: bool, truncated: bool },
  notice: { kind: str, text: str },
  irc: { from: nullable(str), agent: nullable(str), text: str, note: nullable(str) },
  compaction: { tokens_before: nullable(u64), tokens_after: nullable(u64), summary: str },
  model_change: { model: str },
  error: { message: str },
  exit: { reason: str },
});
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
const event = tagged<Event, 'type'>('type', {
  feed: { feed, value: feedValue },
  outcome: { seq: u64, outcome: outcomeView },
  progress: { seq: u64, stage: str, done: u64, total: nullable(u64) },
  confirm: { live: liveRef, request: u64, label: str, draft: str },
  term_state: { view: u32, input_held: bool },
  input_refused: { view: u32, reason: inputRefusal },
  model_unconfirmed: { live: liveRef, selector: nullable(str), level: nullable(str) },
  resync: {},
});
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

/** Decodes a WELCOME body; refuses a protocol version other than {@link PROTOCOL}. */
export function decodeWelcome(body: Uint8Array): Welcome {
  const value = parse(body, welcome, 'welcome');
  if (value.protocol !== PROTOCOL) {
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

