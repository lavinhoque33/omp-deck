// The smoke gateway's answers decode with the device's own wire validators: every feed, every
// view op, and the journaled flow (accepted → outcome event).

import { describe, expect, test } from 'bun:test';
import { type AppRecord, api } from '../src/remote/index.ts';
import * as fx from './smoke/fixtures.ts';
import { SmokeGateway } from './smoke/gateway.ts';

const encoder = new TextEncoder();

class Recorder {
  readonly replies: api.Reply[] = [];
  readonly events: api.Event[] = [];
  readonly term: AppRecord[] = [];
  welcome: api.Welcome | null = null;
  #waiters: (() => void)[] = [];

  readonly send = (records: AppRecord[]): void => {
    for (const record of records) {
      if (record.kind === 'welcome') this.welcome = api.decodeWelcome(record.body);
      else if (record.kind === 'reply') this.replies.push(api.decodeReply(record.body));
      else if (record.kind === 'event') this.events.push(api.decodeEvent(record.body));
      else this.term.push(record);
    }
    for (const wake of this.#waiters.splice(0)) wake();
  };

  /** Resolves once `ready()` holds (re-checked after every message). */
  async until(ready: () => boolean): Promise<void> {
    while (!ready()) {
      const { promise, resolve } = Promise.withResolvers<void>();
      this.#waiters.push(resolve);
      await promise;
    }
  }

  reply(id: number): api.Reply {
    const found = this.replies.find((reply) => reply.id === id);
    if (found === undefined) throw new Error(`no reply ${id}`);
    return found;
  }
}

function setup(s4 = true) {
  const gateway = new SmokeGateway({ s4, png: Uint8Array.of(0x89, 0x50, 0x4e, 0x47), pushKey: s4 ? 'BAAA' : null, uploadDelayMs: 0, outcomeDelayMs: 0 });
  const recorder = new Recorder();
  const conn = gateway.connect(recorder.send, 'd-0123456789abcdef', 'Pixel 9');
  let id = 0;
  const send = (op: api.Op, seq: number | null = null): number => {
    const request: api.Request = { id: ++id, seq, op };
    expect(api.checkRequest(request)).toBeNull();
    conn.receive([{ kind: 'request', body: encoder.encode(api.requestJson(request)) }]);
    return request.id;
  };
  return { gateway, recorder, send };
}

const LIVE = fx.liveRef(fx.LIVE[1] as fx.LiveSpec);

const FEEDS: api.Feed[] = [
  { feed: 'projects' },
  { feed: 'sessions', project: 'p-ompdeck', limit: 50 },
  { feed: 'live' },
  { feed: 'inbox' },
  { feed: 'cards', sessions: ['s-convo', 's-idle'] },
  { feed: 'transcript', session: 's-convo', tail: 200 },
  { feed: 'transcript', session: 's-convo', agent: 'PwaHarness', tail: 200 },
  { feed: 'changes', project: 'p-ompdeck' },
  { feed: 'changes', project: 'p-infra' },
  { feed: 'running' },
  { feed: 'inventory', limit: 100 },
  { feed: 'context', live: LIVE },
  { feed: 'usage' },
  { feed: 'agents', session: 's-convo' },
  { feed: 'worktrees', project: 'p-ompdeck-wt' },
  { feed: 'backup' },
  { feed: 'sync' },
  { feed: 'updates' },
  { feed: 'device' },
];

const VIEW_OPS: [api.Op, api.Resp['type']][] = [
  [{ op: 'models' }, 'models'],
  [{ op: 'diff', project: 'p-ompdeck', file: 'pwa/src/app/client.ts', side: 'unstaged' }, 'diff'],
  [{ op: 'attach', live: LIVE, mode: 'control' }, 'attached'],
  [{ op: 'ping' }, 'pong'],
  [{ op: 'last_reply', session: 's-convo' }, 'text'],
  [{ op: 'export', session: 's-convo' }, 'download'],
  [{ op: 'blob', session: 's-convo', blob: fx.IMAGE_BLOB }, 'download'],
  [{ op: 'tree', session: 's-convo', filter: 'default', limit: 10 }, 'tree'],
  [{ op: 'browse' }, 'dir'],
  [{ op: 'browse', dir: 'dir:~/PersProject' }, 'dir'],
  [{ op: 'usage_activity', days: 371 }, 'activity'],
  [{ op: 'omp_settings' }, 'omp_settings'],
  [{ op: 'backup_list' }, 'backup_list'],
  [{ op: 'sync_backups' }, 'sync_backups'],
  [{ op: 'presence', visible: true }, 'unit'],
  [{ op: 'push_prefs', prefs: fx.DEFAULT_PREFS }, 'unit'],
  [{ op: 'device_rename', name: 'Work phone' }, 'unit'],
  [{ op: 'upload_begin', name: 'a.png', mime: 'image/png', size: 3 }, 'upload'],
];

describe('smoke gateway', () => {
  test('the Welcome announces s4 and a push key, or neither for an old desktop', () => {
    expect(setup().recorder.welcome).toMatchObject({ features: ['s4'], push_key: 'BAAA' });
    const old = setup(false).recorder.welcome;
    expect(old?.features).toEqual([]);
    expect(old?.push_key).toBeUndefined();
  });

  test('every feed value decodes', async () => {
    const { recorder, send } = setup();
    for (const feed of FEEDS) send({ op: 'subscribe', feed });
    await recorder.until(() => recorder.events.length >= FEEDS.length);
    const feeds = recorder.events.flatMap((event) => (event.type === 'feed' ? [event.value.feed] : []));
    expect(new Set(feeds)).toEqual(new Set(FEEDS.map((feed) => feed.feed)));
    const transcript = recorder.events.find((event) => event.type === 'feed' && event.value.feed === 'transcript');
    expect(transcript?.type === 'feed' && transcript.value.feed === 'transcript' && transcript.value.items.some((item) => item.type === 'tool_call' && item.args_value !== undefined)).toBe(true);
    const live = recorder.events.find((event) => event.type === 'feed' && event.value.feed === 'live');
    const kinds = live?.type === 'feed' && live.value.feed === 'live' ? live.value.sessions.flatMap((s) => (s.pending_ui ?? []).map((u) => `${u.kind}${u.multi ? '+' : ''}`)) : [];
    expect(kinds.sort()).toEqual(['approve', 'ask', 'confirm', 'editor', 'input', 'select', 'select+']);
  });

  test('every view op answers with its reply type', async () => {
    const { recorder, send } = setup();
    const ids = VIEW_OPS.map(([op]) => send(op));
    await recorder.until(() => recorder.replies.length >= ids.length);
    VIEW_OPS.forEach(([op, type], index) => {
      const reply = recorder.reply(ids[index] ?? 0);
      expect([op.op, 'ok' in reply ? reply.ok.type : reply.err.code]).toEqual([op.op, type]);
    });
    await recorder.until(() => recorder.term.length >= 2);
    expect(recorder.term.map((record) => record.kind)).toEqual(['t_resize', 't_snapshot']);
  });

  test('a download is pulled in chunks to eof', async () => {
    const { recorder, send } = setup();
    const id = send({ op: 'export', session: 's-convo' });
    await recorder.until(() => recorder.replies.length === 1);
    const reply = recorder.reply(id);
    if (!('ok' in reply) || reply.ok.type !== 'download') throw new Error('no download');
    const { download, size } = reply.ok;
    let offset = 0;
    for (;;) {
      const chunkId = send({ op: 'download', download, offset });
      await recorder.until(() => recorder.replies.some((r) => r.id === chunkId));
      const chunk = recorder.reply(chunkId);
      if (!('ok' in chunk) || chunk.ok.type !== 'chunk') throw new Error('no chunk');
      offset += Buffer.from(chunk.ok.data, 'base64').length;
      if (chunk.ok.eof) break;
    }
    expect(size).toBeGreaterThan(api.MAX_TRANSFER_CHUNK);
    expect(offset).toBe(size);
  });

  test('journaled ops answer accepted, then their outcome event', async () => {
    const { recorder, send } = setup();
    const journaled: api.Op[] = [
      { op: 'answer_ui', live: fx.liveRef(fx.LIVE[2] as fx.LiveSpec), request: 'u-11', value: { type: 'choice', index: 1 } },
      { op: 'prompt', live: LIVE, text: 'hello' },
      { op: 'running_kill', ids: ['r-00ff00ff00ff00ff'] },
      { op: 'sync_restore_prepare', backup: 'sb-p-ompdeck-laptop', target: { type: 'project', project: 'p-ompdeck' }, parts: { omp: true, sessions: true, memory: true } },
      { op: 'backup_restore_prepare', id: 'drv-laptop-3' },
      { op: 'delete_sessions', sessions: ['s-old-2'] },
      { op: 'new_session_in', dir: 'dir:~/code/website' },
    ];
    journaled.forEach((op, index) => send(op, 100 + index));
    await recorder.until(() => recorder.events.filter((event) => event.type === 'outcome').length >= journaled.length);
    for (const reply of recorder.replies) expect('ok' in reply && reply.ok.type).toBe('accepted');
    const results = recorder.events.flatMap((event) => (event.type === 'outcome' && event.outcome.state === 'done' ? [event.outcome.result.type] : []));
    expect(results.sort()).toEqual((['answered', 'deleted', 'killed', 'restore_plan', 'restore_plan', 'session', 'submitted'] as const).toSorted());
  });

  test('an old desktop refuses s4 ops as unsupported', async () => {
    const { recorder, send } = setup(false);
    const id = send({ op: 'omp_settings' });
    await recorder.until(() => recorder.replies.length === 1);
    expect(recorder.reply(id)).toEqual({ id, err: { code: 'unsupported' } });
  });
});
