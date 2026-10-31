import { beforeEach, describe, expect, test } from 'bun:test';
import { Client, ClientError, type PairingStore, type ViewSink } from '../src/app/client.ts';
import type { StoredPairing } from '../src/app/keystore.ts';
import type { Dialer, DialTarget, LinkEvents } from '../src/app/link.ts';
import { type AppRecord, CLOSE, type api } from '../src/remote/index.ts';

// ---- fakes ------------------------------------------------------------------------------------

class FakeClock {
  time = 1_000_000;
  readonly now = (): number => this.time;
  #next = 1;
  readonly timers = new Map<number, { at: number; run: () => void }>();
  readonly setTimer = (run: () => void, ms: number): number => {
    const handle = this.#next++;
    this.timers.set(handle, { at: this.time + ms, run });
    return handle;
  };
  readonly clearTimer = (handle: number): void => {
    this.timers.delete(handle);
  };
  /** Moves time forward, running every timer that falls due (in order). */
  async advance(ms: number): Promise<void> {
    const until = this.time + ms;
    for (;;) {
      const due = [...this.timers.entries()].filter(([, timer]) => timer.at <= until).sort((a, b) => a[1].at - b[1].at)[0];
      if (due === undefined) break;
      this.timers.delete(due[0]);
      this.time = Math.max(this.time, due[1].at);
      due[1].run();
      await settle();
    }
    this.time = until;
  }
}

class FakeStore implements PairingStore {
  constructor(public value: StoredPairing | null) {}
  async load(): Promise<StoredPairing | null> {
    return this.value === null ? null : { ...this.value };
  }
  async save(pairing: StoredPairing): Promise<void> {
    this.value = { ...pairing, secret: Uint8Array.from(pairing.secret) };
  }
  async reserveTimestamp(nowMs: number): Promise<number> {
    if (this.value === null) throw new Error('not paired');
    this.value.last = Math.max(nowMs, this.value.last + 1);
    return this.value.last;
  }
  async clear(): Promise<void> {
    this.value = null;
  }
}

interface Dial {
  target: DialTarget;
  events: LinkEvents;
  sent: AppRecord[];
  closedWith: number[];
}

const encoder = new TextEncoder();
const decoder = new TextDecoder();
/** Lets the client's promise chains (store reads, replies) run to completion; no wall-clock wait. */
async function settle(): Promise<void> {
  for (let i = 0; i < 20; i++) await Promise.resolve();
}

function stored(): StoredPairing {
  return {
    secret: new Uint8Array(32).fill(5),
    gatewayPublic: new Uint8Array(32).fill(6),
    url: 'wss://relay.example',
    room: new Uint8Array(16).fill(1),
    token: new Uint8Array(32).fill(2),
    device: 'd-0123456789abcdef',
    gateway: 'desk',
    name: 'Pixel',
    last: 10,
    pairedUnixMs: 10,
  };
}

const WELCOME: api.Welcome = {
  protocol: 1,
  gateway_version: '0.9.0',
  epoch: '00112233445566778899aabbccddeeff',
  device: 'd-0123456789abcdef',
  name: 'Pixel',
  permissions: { control: true, manage: true },
  next_seq: 28,
  limits: { max_message: 1 << 20, window_bytes: 1 << 20, window_messages: 32, max_inflight: 32, max_views: 4, max_feeds: 16 },
  features: [],
};

const LIVE: api.LiveRef = { key: 'tmux:8:omp-deck:omp-1a2b', generation: 7 };

function liveSession(requests: api.OwnRequest[] = []): api.LiveSession {
  return {
    live: LIVE,
    kind: 'session',
    label: 'Fix the parser',
    title: 'Fix the parser',
    session: '01HZX3J6V0Q8',
    project: 'p-0123456789abcdef',
    display: '~/src/app',
    activity: 'idle',
    status: { state: 'idle' },
    exit_code: null,
    busy: false,
    model: 'anthropic/claude-sonnet-4',
    pending_switch: null,
    live_control: true,
    desktop_controlled: false,
    views: 0,
    requests,
  };
}

let clock: FakeClock;
let store: FakeStore;
let dials: Dial[];
let client: Client;

const dial: Dialer = (target, events) => {
  const entry: Dial = { target, events, sent: [], closedWith: [] };
  dials.push(entry);
  return {
    send: (records) => entry.sent.push(...records),
    close: (code) => entry.closedWith.push(code),
  };
};

function last(): Dial {
  const entry = dials.at(-1);
  if (entry === undefined) throw new Error('nothing dialled');
  return entry;
}

function deliver(entry: Dial, records: AppRecord[]): void {
  entry.events.message(records, records.length * 64);
}

function welcome(entry: Dial, patch: Partial<api.Welcome> = {}): void {
  entry.events.open();
  deliver(entry, [{ kind: 'welcome', body: encoder.encode(JSON.stringify({ ...WELCOME, ...patch })) }]);
}

function reply(entry: Dial, id: number, ok: unknown): void {
  deliver(entry, [{ kind: 'reply', body: encoder.encode(JSON.stringify({ id, ok })) }]);
}

function event(entry: Dial, value: unknown): void {
  deliver(entry, [{ kind: 'event', body: encoder.encode(JSON.stringify(value)) }]);
}

interface SentRequest {
  id: number;
  seq: number | null;
  op: { op: string } & Record<string, unknown>;
}

function requests(entry: Dial): SentRequest[] {
  return entry.sent.flatMap((record) =>
    record.kind === 'request' ? [JSON.parse(decoder.decode(record.body)) as SentRequest] : [],
  );
}

async function connected(): Promise<Dial> {
  await client.start();
  await settle();
  const entry = last();
  welcome(entry);
  return entry;
}

beforeEach(() => {
  clock = new FakeClock();
  store = new FakeStore(stored());
  dials = [];
  client = new Client({
    store,
    dial,
    pair: () => Promise.reject(new Error('not in this test')),
    clock,
    random: () => 0.5,
    platform: 'Test',
    persistStorage: async () => true,
  });
});

// ---- tests ------------------------------------------------------------------------------------

describe('connection', () => {
  test('without a pairing the client stays unpaired and dials nothing', async () => {
    store.value = null;
    await client.start();
    expect(client.getState().conn.phase).toBe('unpaired');
    expect(dials).toHaveLength(0);
  });

  test('dials with the stored key and a reserved, persisted timestamp; online after Welcome', async () => {
    clock.time = 5;
    await client.start();
    await settle();
    expect(client.getState().conn.phase).toBe('connecting');
    expect(last().target.hello.ts).toBe(11);
    expect(store.value?.last).toBe(11);
    expect(last().target.hello.features).toEqual(['s4']);
    welcome(last());
    expect(client.getState().conn.phase).toBe('online');
    expect(client.getState().welcome?.next_seq).toBe(28);
  });

  test('backs off after a drop, then reconnects and resubscribes every feed', async () => {
    const first = await connected();
    const release = client.follow({ feed: 'live' });
    expect(release).not.toBeNull();
    expect(requests(first).map((request) => request.op.op)).toEqual(['subscribe']);
    first.events.closed({ code: CLOSE.GATEWAY_OFFLINE, reason: '', stage: 'session', error: null });
    const conn = client.getState().conn;
    expect(conn.phase).toBe('waiting');
    expect(conn.retryAt).toBe(clock.time + 1000);
    await clock.advance(999);
    expect(dials).toHaveLength(1);
    await clock.advance(1);
    expect(dials).toHaveLength(2);
    welcome(last());
    expect(requests(last()).map((request) => request.op)).toEqual([{ op: 'subscribe', feed: { feed: 'live' } }]);
  });

  test('consecutive failures double the delay', async () => {
    await client.start();
    await settle();
    last().events.closed({ code: 1006, reason: '', stage: 'socket', error: null });
    expect(client.getState().conn.retryAt).toBe(clock.time + 1000);
    await clock.advance(1000);
    last().events.closed({ code: 1006, reason: '', stage: 'socket', error: null });
    expect(client.getState().conn.retryAt).toBe(clock.time + 2000);
  });

  test('4003 stops without deleting the pairing', async () => {
    const entry = await connected();
    entry.events.closed({ code: CLOSE.UNAUTHORIZED, reason: '', stage: 'admission', error: null });
    expect(client.getState().conn).toMatchObject({ phase: 'stopped', stop: 'unauthorized' });
    expect(clock.timers.size).toBe(0);
    expect(store.value).not.toBeNull();
    client.reconnectNow();
    await settle();
    expect(dials).toHaveLength(2);
  });

  test('4029 is retried for about 75 seconds, then stops', async () => {
    await client.start();
    await settle();
    let elapsed = 0;
    for (;;) {
      last().events.closed({ code: CLOSE.RATE_LIMITED, reason: '', stage: 'admission', error: null });
      const conn = client.getState().conn;
      if (conn.phase === 'stopped') {
        expect(conn.stop).toBe('rate_limited');
        break;
      }
      const wait = (conn.retryAt ?? 0) - clock.time;
      elapsed += wait;
      await clock.advance(wait);
    }
    expect(elapsed).toBe(75_000);
  });

  test('a normal close of an open session means another tab took over', async () => {
    const entry = await connected();
    entry.events.closed({ code: CLOSE.NORMAL, reason: '', stage: 'session', error: null });
    expect(client.getState().conn).toMatchObject({ phase: 'stopped', stop: 'replaced' });
  });

  test('events of a link closed on purpose are ignored', async () => {
    const entry = await connected();
    await client.forget();
    expect(entry.closedWith).toEqual([CLOSE.NORMAL]);
    entry.events.closed({ code: CLOSE.NORMAL, reason: '', stage: 'session', error: null });
    expect(client.getState().conn.phase).toBe('unpaired');
    expect(store.value).toBeNull();
  });
});

describe('forgetting stops notifications', () => {
  let dropped: number;
  beforeEach(() => {
    dropped = 0;
    client = new Client({
      store,
      dial,
      pair: () => Promise.reject(new Error('not in this test')),
      clock,
      random: () => 0.5,
      platform: 'Test',
      persistStorage: async () => true,
      dropPush: async () => {
        dropped++;
      },
    });
  });

  test('online: tells the desktop, then drops the browser subscription and the pairing', async () => {
    await client.start();
    await settle();
    const entry = last();
    welcome(entry, { features: ['s4'] });
    const done = client.forget();
    await settle();
    const sent = requests(entry).filter((request) => request.op.op === 'push_unsubscribe');
    expect(sent).toHaveLength(1);
    expect(dropped).toBe(0);
    reply(entry, sent[0]?.id ?? -1, { type: 'unit' });
    await done;
    expect(dropped).toBe(1);
    expect(store.value).toBeNull();
  });

  test('a failing browser unsubscribe is reported as a warning and the pairing is still dropped', async () => {
    client = new Client({
      store,
      dial,
      pair: () => Promise.reject(new Error('not in this test')),
      clock,
      random: () => 0.5,
      platform: 'Test',
      persistStorage: async () => true,
      dropPush: async () => {
        throw new Error('push service unreachable');
      },
    });
    await client.start();
    await settle();
    await client.forget();
    expect(store.value).toBeNull();
    const notice = client.getState().notices.at(-1);
    expect(notice?.tone).toBe('warn');
    expect(notice?.text).toContain('push service unreachable');
  });

  test('a desktop that never answers is waited for only briefly', async () => {
    await client.start();
    await settle();
    welcome(last(), { features: ['s4'] });
    const done = client.forget();
    await settle();
    await clock.advance(3000);
    await done;
    expect(dropped).toBe(1);
    expect(store.value).toBeNull();
  });

  test('offline: the browser subscription is dropped anyway', async () => {
    await client.start();
    await settle();
    await client.forget();
    expect(dropped).toBe(1);
    expect(store.value).toBeNull();
  });
});

describe('requests', () => {
  test('mutations are refused offline and nothing is queued', async () => {
    await client.start();
    await settle();
    await expect(client.mutate({ op: 'interrupt', live: LIVE }, 'Interrupt')).rejects.toBeInstanceOf(ClientError);
    await expect(client.request({ op: 'models' })).rejects.toMatchObject({ kind: 'offline' });
    expect(client.canSend({ op: 'interrupt', live: LIVE })).toBe(false);
    welcome(last());
    expect(requests(last())).toEqual([]);
    expect(client.canSend({ op: 'interrupt', live: LIVE })).toBe(true);
  });

  test('permissions gate mutations', async () => {
    await client.start();
    await settle();
    welcome(last(), { permissions: { control: true, manage: false } });
    expect(client.canSend({ op: 'delete_sessions', sessions: ['a'] })).toBe(false);
    await expect(client.mutate({ op: 'delete_sessions', sessions: ['a'] }, 'Delete')).rejects.toBeInstanceOf(ClientError);
  });

  test('a mutation resolves with its outcome event', async () => {
    const entry = await connected();
    const pending = client.mutate({ op: 'interrupt', live: LIVE }, 'Interrupt');
    const [sent] = requests(entry);
    expect(sent).toMatchObject({ seq: 28, op: { op: 'interrupt' } });
    reply(entry, sent?.id ?? 0, { type: 'accepted', seq: 28, duplicate: false });
    await settle();
    expect(client.getState().requests[0]?.status).toBe('accepted');
    event(entry, { type: 'outcome', seq: 28, outcome: { state: 'done', result: { type: 'unit' } } });
    const view = await pending;
    expect(view.status).toBe('done');
    expect(client.openRequests).toBe(0);
  });

  test('an open request is looked up after a reconnect', async () => {
    const first = await connected();
    const pending = client.mutate({ op: 'prompt', live: LIVE, text: 'hi' }, 'Prompt');
    expect(requests(first)[0]?.seq).toBe(28);
    first.events.closed({ code: CLOSE.GATEWAY_OFFLINE, reason: '', stage: 'session', error: null });
    await clock.advance(1000);
    const second = last();
    welcome(second, { next_seq: 29 });
    const [lookup] = requests(second);
    expect(lookup?.op).toEqual({ op: 'outcome', seq: 28 });
    reply(second, lookup?.id ?? 0, {
      type: 'outcome',
      outcome: { state: 'done', result: { type: 'submitted', request: 3, label: 'Prompt', delivery: { state: 'delivered' } } },
    });
    const view = await pending;
    expect(view.status).toBe('done');
  });

  test('resync re-reads open requests, and running ones are polled', async () => {
    const entry = await connected();
    void client.mutate({ op: 'kill', live: LIVE }, 'Kill');
    event(entry, { type: 'resync' });
    const lookup = requests(entry).at(-1);
    expect(lookup?.op).toEqual({ op: 'outcome', seq: 28 });
    reply(entry, lookup?.id ?? 0, { type: 'outcome', outcome: { state: 'running' } });
    await settle();
    expect(client.getState().requests[0]?.status).toBe('running');
    const before = requests(entry).length;
    await clock.advance(5000);
    expect(requests(entry).length).toBe(before + 1);
    expect(requests(entry).at(-1)?.op).toEqual({ op: 'outcome', seq: 28 });
  });

  test('an unknown outcome can be resent with a new seq', async () => {
    const entry = await connected();
    const pending = client.mutate({ op: 'interrupt', live: LIVE }, 'Interrupt');
    event(entry, { type: 'outcome', seq: 28, outcome: { state: 'unknown' } });
    expect((await pending).status).toBe('unknown');
    const again = client.resend(28);
    expect(again).not.toBeNull();
    expect(requests(entry).at(-1)).toMatchObject({ seq: 29, op: { op: 'interrupt' } });
    expect(client.resend(28)).toBeNull();
  });

  test('an api error finishes the request', async () => {
    const entry = await connected();
    const pending = client.mutate({ op: 'interrupt', live: LIVE }, 'Interrupt');
    deliver(entry, [{ kind: 'reply', body: encoder.encode(JSON.stringify({ id: requests(entry)[0]?.id, err: { code: 'maintenance' } })) }]);
    const view = await pending;
    expect(view.status).toBe('error');
    expect(view.error).toEqual({ code: 'maintenance' });
  });
});

describe('feeds', () => {
  test('follow is reference counted and unsubscribes on the last release', async () => {
    const entry = await connected();
    const a = client.follow({ feed: 'inbox' });
    const b = client.follow({ feed: 'inbox' });
    expect(requests(entry)).toHaveLength(1);
    a?.();
    expect(requests(entry)).toHaveLength(1);
    b?.();
    expect(requests(entry).at(-1)?.op).toEqual({ op: 'unsubscribe', feed: { feed: 'inbox' } });
  });

  test('follow refuses past max_feeds', async () => {
    await connected();
    for (let i = 0; i < 16; i++) expect(client.follow({ feed: 'changes', project: `p${i}` })).not.toBeNull();
    expect(client.follow({ feed: 'changes', project: 'p16' })).toBeNull();
  });

  test('transcript feeds accumulate appended items and reset', async () => {
    const entry = await connected();
    const feed: api.Feed = { feed: 'transcript', session: 's', agent: null, tail: 50 };
    client.follow(feed);
    const user = (text: string) => ({ type: 'user', text, from_agent: false, at_unix_ms: null });
    event(entry, { type: 'feed', feed, value: { feed: 'transcript', start: 10, items: [user('a'), user('b')], reset: true } });
    event(entry, { type: 'feed', feed, value: { feed: 'transcript', start: 11, items: [user('B'), user('c')], reset: false } });
    const state = [...client.getState().transcripts.values()][0];
    expect(state?.start).toBe(10);
    expect(state?.items.map((item) => (item.type === 'user' ? item.text : ''))).toEqual(['a', 'B', 'c']);
  });

  test('pending confirmations are re-learnt from live and cleared by the answer', async () => {
    const entry = await connected();
    client.follow({ feed: 'live' });
    const awaiting: api.OwnRequest = { request: 12, seq: 6, label: 'Send prompt', delivery: { state: 'awaiting_confirm', draft: 'half typed' } };
    event(entry, { type: 'feed', feed: { feed: 'live' }, value: { feed: 'live', sessions: [liveSession([awaiting])], truncated: false } });
    expect(client.getState().confirms).toEqual([{ live: LIVE, request: 12, label: 'Send prompt', draft: 'half typed' }]);
    const pending = client.mutate({ op: 'answer', live: LIVE, request: 12, proceed: true }, 'Approve');
    event(entry, { type: 'outcome', seq: 28, outcome: { state: 'done', result: { type: 'answered', applied: true } } });
    await pending;
    expect(client.getState().confirms).toEqual([]);
  });
});

describe('terminal views', () => {
  test('records arriving before the attach reply are delivered in order', async () => {
    const entry = await connected();
    const seen: string[] = [];
    const sink: ViewSink = {
      snapshot: (bytes, lastChunk) => seen.push(`snapshot:${decoder.decode(bytes)}:${lastChunk}`),
      output: (bytes) => seen.push(`output:${decoder.decode(bytes)}`),
      resize: (cols, rows) => seen.push(`resize:${cols}x${rows}`),
      end: (end) => seen.push(`end:${end.reason}`),
      state: () => undefined,
      refused: () => undefined,
    };
    const attached = client.attach(LIVE, 'input', sink);
    const [request] = requests(entry);
    deliver(entry, [
      { kind: 't_resize', view: 2, cols: 80, rows: 24 },
      { kind: 't_snapshot', view: 2, last: true, bytes: encoder.encode('screen') },
      { kind: 't_output', view: 2, bytes: encoder.encode('more') },
    ]);
    reply(entry, request?.id ?? 0, { type: 'attached', view: 2, mode: 'input' });
    const view = await attached;
    expect(seen).toEqual(['resize:80x24', 'snapshot:screen:true', 'output:more']);
    expect(view.input(encoder.encode('ls\r'), false)).toBe(true);
    expect(entry.sent.at(-1)).toMatchObject({ kind: 'input', view: 2, ctrlC: false });
    expect(view.resize(100, 30)).toBe(false);
    entry.events.closed({ code: 1006, reason: '', stage: 'session', error: null });
    expect(seen.at(-1)).toBe('end:disconnected');
    expect(view.input(encoder.encode('x'), false)).toBe(false);
  });

  test('a term_state packed with the attach reply is replayed when the view registers', async () => {
    const entry = await connected();
    const held: boolean[] = [];
    const sink: ViewSink = {
      snapshot: () => undefined,
      output: () => undefined,
      resize: () => undefined,
      end: () => undefined,
      state: (value) => held.push(value),
      refused: () => undefined,
    };
    const attached = client.attach(LIVE, 'input', sink);
    const [request] = requests(entry);
    event(entry, { type: 'term_state', view: 2, input_held: true });
    event(entry, { type: 'term_state', view: 2, input_held: false });
    event(entry, { type: 'term_state', view: 2, input_held: true });
    expect(held).toEqual([]);
    reply(entry, request?.id ?? 0, { type: 'attached', view: 2, mode: 'input' });
    await attached;
    expect(held).toEqual([true]);
    event(entry, { type: 'term_state', view: 2, input_held: false });
    expect(held).toEqual([true, false]);
  });
});

describe('slice 4', () => {
  test('an older desktop: s4 ops are refused locally and s4 feeds stay unsent', async () => {
    const entry = await connected();
    expect(client.s4).toBe(false);
    expect(client.canSend({ op: 'omp_settings' })).toBe(false);
    expect(client.canSend({ op: 'models' })).toBe(true);
    const error = await client.request({ op: 'omp_settings' }).catch((caught: unknown) => caught);
    expect(error).toBeInstanceOf(ClientError);
    expect((error as ClientError).kind).toBe('unsupported');
    expect(client.follow({ feed: 'usage' })).not.toBeNull();
    client.follow({ feed: 'live' });
    client.setVisible(false);
    expect(requests(entry).map((request) => request.op.op)).toEqual(['subscribe']);
  });

  test('presence follows visibility and is re-sent after a reconnect', async () => {
    await client.start();
    await settle();
    const first = last();
    welcome(first, { features: ['s4'] });
    expect(requests(first).map((request) => request.op)).toEqual([{ op: 'presence', visible: true }]);
    client.setVisible(false);
    client.setVisible(false);
    expect(requests(first).map((request) => request.op)).toEqual([
      { op: 'presence', visible: true },
      { op: 'presence', visible: false },
    ]);
    client.follow({ feed: 'usage' });
    first.events.closed({ code: CLOSE.GATEWAY_OFFLINE, reason: '', stage: 'session', error: null });
    await clock.advance(1000);
    welcome(last(), { features: ['s4'] });
    expect(requests(last()).map((request) => request.op)).toEqual([
      { op: 'subscribe', feed: { feed: 'usage' } },
      { op: 'presence', visible: false },
    ]);
  });

  test('whenOnline resolves on the next Welcome and rejects on abort', async () => {
    await client.start();
    await settle();
    let online = false;
    void client.whenOnline().then(() => {
      online = true;
    });
    const controller = new AbortController();
    const aborted = client.whenOnline(controller.signal).catch((error: unknown) => (error as DOMException).name);
    controller.abort();
    expect(await aborted).toBe('AbortError');
    welcome(last(), { features: ['s4'] });
    await settle();
    expect(online).toBe(true);
  });
});
