// Record framing of the device application protocol, mirroring Rust `deck_remote::api`
// (`Record`, `encode`, `decode`; docs/notes/mobile-relay.md, "R4 contract"). One application
// message is a non-empty sequence of records `kind u8 | len u32 BE | body[len]`. Control records
// carry JSON bodies, kept as bytes here (typed decoding lives in `api.ts`); terminal records are
// binary.

import type { Bytes } from './bytes.ts';

/** Largest application message (Noise plaintext), both directions. */
export const MAX_APP_MESSAGE = 1 << 20;
/** `kind` + `len` in front of every record body. */
export const RECORD_HEADER = 5;
/** Bytes of one INPUT record (after `view` and `flags`). */
export const MAX_INPUT = 64 * 1024;
/** Bytes of one T_SNAPSHOT chunk (after `view` and `last`). */
export const MAX_SNAPSHOT_CHUNK = 256 * 1024;

/** Record kinds. Device → gateway: 0x01–0x04; gateway → device: 0x10–0x12, 0x20–0x23. */
export const KIND = {
  REQUEST: 0x01,
  ACK: 0x02,
  INPUT: 0x03,
  RESIZE: 0x04,
  WELCOME: 0x10,
  REPLY: 0x11,
  EVENT: 0x12,
  T_SNAPSHOT: 0x20,
  T_OUTPUT: 0x21,
  T_RESIZE: 0x22,
  T_END: 0x23,
} as const;

/** INPUT flag bit 0: the bytes are a Ctrl+C. */
export const INPUT_CTRL_C = 0x01;

export type AppRecord =
  /** Device → gateway: JSON `Request`. */
  | { kind: 'request'; body: Bytes }
  /** Device → gateway: cumulative count of gateway messages processed. */
  | { kind: 'ack'; messages: number }
  | { kind: 'input'; view: number; ctrlC: boolean; bytes: Bytes }
  /** Control views only. */
  | { kind: 'resize'; view: number; cols: number; rows: number }
  /** Gateway → device: JSON `Welcome`, the first record of a session. */
  | { kind: 'welcome'; body: Bytes }
  | { kind: 'reply'; body: Bytes }
  | { kind: 'event'; body: Bytes }
  /** Chunks of one snapshot are consecutive for that view; `last` marks its end. */
  | { kind: 't_snapshot'; view: number; last: boolean; bytes: Bytes }
  | { kind: 't_output'; view: number; bytes: Bytes }
  | { kind: 't_resize'; view: number; cols: number; rows: number }
  /** JSON `TermEnd` (see `decodeTermEnd`). */
  | { kind: 't_end'; view: number; end: Bytes };

export type RecordKindName = AppRecord['kind'];

const KIND_BY_NAME: Readonly<{ [K in RecordKindName]: number }> = {
  request: KIND.REQUEST,
  ack: KIND.ACK,
  input: KIND.INPUT,
  resize: KIND.RESIZE,
  welcome: KIND.WELCOME,
  reply: KIND.REPLY,
  event: KIND.EVENT,
  t_snapshot: KIND.T_SNAPSHOT,
  t_output: KIND.T_OUTPUT,
  t_resize: KIND.T_RESIZE,
  t_end: KIND.T_END,
};

const DEVICE_KINDS: ReadonlySet<number> = new Set([KIND.REQUEST, KIND.ACK, KIND.INPUT, KIND.RESIZE]);
const GATEWAY_KINDS: ReadonlySet<number> = new Set([
  KIND.WELCOME,
  KIND.REPLY,
  KIND.EVENT,
  KIND.T_SNAPSHOT,
  KIND.T_OUTPUT,
  KIND.T_RESIZE,
  KIND.T_END,
]);

/** Who sent a message being decoded. */
export type Origin = 'device' | 'gateway';

/** Mirrors Rust `api::RecordError` (snake_case variant names). */
export type RecordErrorCode =
  | 'empty'
  | 'too_large'
  | 'truncated'
  | 'unknown_kind'
  | 'wrong_direction'
  | 'malformed'
  | 'oversized';

/** A refused application message; the device closes with `CLOSE.APP_PROTOCOL`. */
export class RecordError extends Error {
  readonly code: RecordErrorCode;

  constructor(code: RecordErrorCode, message: string) {
    super(message);
    this.name = 'RecordError';
    this.code = code;
  }
}

function hexKind(kind: number): string {
  return `0x${kind.toString(16).padStart(2, '0')}`;
}

function bodyLen(record: AppRecord): number {
  switch (record.kind) {
    case 'request':
    case 'welcome':
    case 'reply':
    case 'event':
      return record.body.length;
    case 'ack':
    case 'resize':
    case 't_resize':
      return 8;
    case 'input':
    case 't_snapshot':
      return 5 + record.bytes.length;
    case 't_output':
      return 4 + record.bytes.length;
    case 't_end':
      return 4 + record.end.length;
  }
}

function checkUint(value: number, max: number, what: string, kind: number): void {
  if (!Number.isInteger(value) || value < 0 || value > max) {
    throw new RecordError('malformed', `${what} of record ${hexKind(kind)} out of range`);
  }
}

function oversized(kind: number, len: number, max: number): void {
  if (len > max) {
    throw new RecordError('oversized', `record of kind ${hexKind(kind)} carries ${len} bytes, more than ${max}`);
  }
}

/** One application message holding `records` in order (any direction, like Rust `encode`). */
export function encodeRecords(records: readonly AppRecord[]): Bytes {
  if (records.length === 0) throw new RecordError('empty', 'empty application message');
  let total = 0;
  for (const record of records) total += RECORD_HEADER + bodyLen(record);
  if (total > MAX_APP_MESSAGE) {
    throw new RecordError('too_large', `application message of ${total} bytes exceeds ${MAX_APP_MESSAGE}`);
  }
  const out = new Uint8Array(total);
  const view = new DataView(out.buffer);
  let at = 0;
  for (const record of records) {
    const kind = KIND_BY_NAME[record.kind];
    const len = bodyLen(record);
    oversized(kind, len, MAX_APP_MESSAGE - RECORD_HEADER);
    view.setUint8(at, kind);
    view.setUint32(at + 1, len);
    at += RECORD_HEADER;
    switch (record.kind) {
      case 'request':
      case 'welcome':
      case 'reply':
      case 'event':
        out.set(record.body, at);
        break;
      case 'ack':
        if (!Number.isSafeInteger(record.messages) || record.messages < 0) {
          throw new RecordError('malformed', 'ack count out of range');
        }
        view.setBigUint64(at, BigInt(record.messages));
        break;
      case 'input':
        checkUint(record.view, 0xffff_ffff, 'view', kind);
        oversized(kind, record.bytes.length, MAX_INPUT);
        view.setUint32(at, record.view);
        view.setUint8(at + 4, record.ctrlC ? INPUT_CTRL_C : 0);
        out.set(record.bytes, at + 5);
        break;
      case 'resize':
      case 't_resize':
        checkUint(record.view, 0xffff_ffff, 'view', kind);
        checkUint(record.cols, 0xffff, 'cols', kind);
        checkUint(record.rows, 0xffff, 'rows', kind);
        view.setUint32(at, record.view);
        view.setUint16(at + 4, record.cols);
        view.setUint16(at + 6, record.rows);
        break;
      case 't_snapshot':
        checkUint(record.view, 0xffff_ffff, 'view', kind);
        oversized(kind, record.bytes.length, MAX_SNAPSHOT_CHUNK);
        view.setUint32(at, record.view);
        view.setUint8(at + 4, record.last ? 1 : 0);
        out.set(record.bytes, at + 5);
        break;
      case 't_output':
        checkUint(record.view, 0xffff_ffff, 'view', kind);
        view.setUint32(at, record.view);
        out.set(record.bytes, at + 4);
        break;
      case 't_end':
        checkUint(record.view, 0xffff_ffff, 'view', kind);
        view.setUint32(at, record.view);
        out.set(record.end, at + 4);
        break;
    }
    at += len;
  }
  return out;
}

function decodeOne(kind: number, body: Bytes, origin: Origin): AppRecord | null {
  const mine = origin === 'device' ? DEVICE_KINDS : GATEWAY_KINDS;
  const theirs = origin === 'device' ? GATEWAY_KINDS : DEVICE_KINDS;
  if (theirs.has(kind)) {
    throw new RecordError('wrong_direction', `record kind ${hexKind(kind)} is not sent by this side`);
  }
  if (!mine.has(kind)) {
    if (origin === 'gateway') return null;
    throw new RecordError('unknown_kind', `unknown record kind ${hexKind(kind)}`);
  }
  const malformed = (): RecordError => new RecordError('malformed', `malformed record of kind ${hexKind(kind)}`);
  const data = new DataView(body.buffer, body.byteOffset, body.byteLength);
  const viewId = (): number => {
    if (body.length < 4) throw malformed();
    return data.getUint32(0);
  };
  const geometry = (): { view: number; cols: number; rows: number } => {
    if (body.length !== 8) throw malformed();
    return { view: data.getUint32(0), cols: data.getUint16(4), rows: data.getUint16(6) };
  };
  switch (kind) {
    case KIND.REQUEST:
      return { kind: 'request', body };
    case KIND.ACK: {
      if (body.length !== 8) throw malformed();
      const messages = data.getBigUint64(0);
      // Counts beyond 2^53 cannot occur in practice; refuse rather than lose precision.
      if (messages > BigInt(Number.MAX_SAFE_INTEGER)) throw malformed();
      return { kind: 'ack', messages: Number(messages) };
    }
    case KIND.INPUT: {
      const view = viewId();
      if (body.length < 5) throw malformed();
      const flags = body[4]!;
      if ((flags & ~INPUT_CTRL_C) !== 0) throw malformed();
      const bytes = body.slice(5);
      oversized(kind, bytes.length, MAX_INPUT);
      return { kind: 'input', view, ctrlC: flags === INPUT_CTRL_C, bytes };
    }
    case KIND.RESIZE:
      return { kind: 'resize', ...geometry() };
    case KIND.WELCOME:
      return { kind: 'welcome', body };
    case KIND.REPLY:
      return { kind: 'reply', body };
    case KIND.EVENT:
      return { kind: 'event', body };
    case KIND.T_SNAPSHOT: {
      const view = viewId();
      const last = body[4];
      if (last !== 0 && last !== 1) throw malformed();
      const bytes = body.slice(5);
      oversized(kind, bytes.length, MAX_SNAPSHOT_CHUNK);
      return { kind: 't_snapshot', view, last: last === 1, bytes };
    }
    case KIND.T_OUTPUT:
      return { kind: 't_output', view: viewId(), bytes: body.slice(4) };
    case KIND.T_RESIZE:
      return { kind: 't_resize', ...geometry() };
    case KIND.T_END:
      return { kind: 't_end', view: viewId(), end: body.slice(4) };
    default:
      throw new RecordError('unknown_kind', `unknown record kind ${hexKind(kind)}`);
  }
}

/**
 * The records of one application message: a whole sequence or a {@link RecordError}. From a
 * gateway, unknown kinds are skipped (a newer gateway) and device kinds refused; from a device,
 * only device kinds are accepted.
 */
export function decodeRecords(message: Uint8Array, origin: Origin): AppRecord[] {
  if (message.length === 0) throw new RecordError('empty', 'empty application message');
  if (message.length > MAX_APP_MESSAGE) {
    throw new RecordError('too_large', `application message of ${message.length} bytes exceeds ${MAX_APP_MESSAGE}`);
  }
  const data = new DataView(message.buffer, message.byteOffset, message.byteLength);
  const out: AppRecord[] = [];
  let at = 0;
  while (at < message.length) {
    if (message.length - at < RECORD_HEADER) throw new RecordError('truncated', 'truncated record');
    const kind = data.getUint8(at);
    const len = data.getUint32(at + 1);
    const start = at + RECORD_HEADER;
    if (len > message.length - start) throw new RecordError('truncated', 'truncated record');
    const record = decodeOne(kind, message.slice(start, start + len), origin);
    if (record !== null) out.push(record);
    at = start + len;
  }
  return out;
}

// ---- Acknowledgements ------------------------------------------------------------------------

/** A device acknowledges at least every this many messages, bytes or milliseconds. */
export const ACK_EVERY_MESSAGES = 8;
export const ACK_EVERY_BYTES = 256 * 1024;
export const ACK_EVERY_MS = 100;

/**
 * The device ack rule: acknowledge processed gateway messages at least every 8 messages, 256 KiB
 * or 100 ms after the first unacknowledged one. Pure state machine; the caller passes the clock
 * (ms) and sends `{ kind: 'ack', messages }` whenever a method returns a count.
 */
export class AckPolicy {
  #processed = 0;
  #acked = 0;
  #pendingBytes = 0;
  #since: number | null = null;

  /** Cumulative count of processed gateway messages. */
  get processed(): number {
    return this.#processed;
  }

  /** Records one processed gateway message of `bytes` (its plaintext size); returns a due ack. */
  onMessage(bytes: number, now: number): number | null {
    this.#processed++;
    this.#pendingBytes += bytes;
    this.#since ??= now;
    return this.due(now);
  }

  /** The ack to send now, if one is due. */
  due(now: number): number | null {
    if (this.#since === null) return null;
    if (
      this.#processed - this.#acked >= ACK_EVERY_MESSAGES ||
      this.#pendingBytes >= ACK_EVERY_BYTES ||
      now - this.#since >= ACK_EVERY_MS
    ) {
      return this.#take();
    }
    return null;
  }

  /** When {@link due} next returns an ack without further messages; null when nothing is pending. */
  deadline(): number | null {
    return this.#since === null ? null : this.#since + ACK_EVERY_MS;
  }

  /** Acks everything processed now (e.g. batched with an outgoing request); null if nothing is new. */
  flush(): number | null {
    return this.#since === null ? null : this.#take();
  }

  #take(): number {
    this.#acked = this.#processed;
    this.#pendingBytes = 0;
    this.#since = null;
    return this.#acked;
  }
}
