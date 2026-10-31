import { describe, expect, test } from 'bun:test';
import type { api } from '../src/remote/index.ts';
import { MAX_UPLOADS, MAX_UPLOAD_FILE } from '../src/remote/api.ts';
import {
  type Attachment,
  attachProblem,
  attachmentsReducer,
  composeProblem,
  fileSize,
  followMode,
  modesFor,
  phaseOf,
  uploadIds,
} from '../src/app/screens/conversation/compose.ts';
import { contextSegments, percent, thresholdMark, topTools } from '../src/app/screens/conversation/context.ts';
import { ImageCache, autoLoads } from '../src/app/screens/conversation/images.ts';
import {
  EMPTY_TRACKER,
  GONE_MS,
  approveChoices,
  askAnswer,
  countdown,
  dismissGone,
  initialAskDraft,
  noteAnswer,
  noteSending,
  remainingMs,
  selectAnswer,
  textAnswer,
  toggleAsk,
  trackPending,
} from '../src/app/screens/conversation/pending.ts';
import {
  EMPTY_REVIEW,
  ReviewStore,
  diffRows,
  quoteOf,
  reviewCount,
  reviewPayload,
  reviewReducer,
  rowsOf,
  selectionRange,
} from '../src/app/screens/conversation/review.ts';

const request = (patch: Partial<api.UiRequestView>): api.UiRequestView => ({
  request: 'u-1',
  kind: 'select',
  title: 'Pick',
  message: null,
  timeout_ms: null,
  options: [],
  multi: false,
  initial: null,
  prefill: null,
  questions: [],
  approve: null,
  ...patch,
});

const question = (patch: Partial<api.UiQuestion>): api.UiQuestion => ({
  id: 'q',
  question: 'Which?',
  header: null,
  options: [
    { label: 'a', description: null },
    { label: 'b', description: 'second' },
    { label: 'c', description: null },
  ],
  multi: false,
  recommended: null,
  ...patch,
});

describe('pending UI answers', () => {
  test('select answers a choice, a checkbox select one toggle', () => {
    const options = [
      { label: 'main', description: null },
      { label: 'dev', description: null },
    ];
    expect(selectAnswer(request({ options }), 1)).toEqual({ type: 'choice', index: 1 });
    expect(selectAnswer(request({ options, multi: true }), 0)).toEqual({ type: 'choices', indices: [0] });
    expect(selectAnswer(request({ options }), 2)).toBeNull();
  });

  test('input flattens newlines, editor keeps them', () => {
    expect(textAnswer('input', 'a\nb')).toEqual({ type: 'text', text: 'a b' });
    expect(textAnswer('editor', 'a\nb')).toEqual({ type: 'text', text: 'a\nb' });
  });

  test('ask starts at the recommendation and toggles per question kind', () => {
    const single = question({ recommended: 1 });
    let draft = initialAskDraft(single);
    expect(draft.selected).toEqual([1]);
    draft = toggleAsk(single, draft, 2);
    expect(draft.selected).toEqual([2]);
    draft = toggleAsk(single, draft, 2);
    expect(draft.selected).toEqual([]);
    const multi = question({ id: 'm', multi: true, recommended: 9 });
    let many = initialAskDraft(multi);
    expect(many.selected).toEqual([]);
    many = toggleAsk(multi, toggleAsk(multi, many, 2), 0);
    expect(many.selected).toEqual([0, 2]);
    expect(toggleAsk(multi, many, 2).selected).toEqual([0]);
  });

  test('ask answers only when every question has a selection or own text', () => {
    const questions = [question({ id: 'scope' }), question({ id: 'extras', multi: true })];
    const drafts = new Map([
      ['scope', { selected: [0], custom: '' }],
      ['extras', { selected: [], custom: '  ' }],
    ]);
    expect(askAnswer(questions, drafts)).toBeNull();
    drafts.set('extras', { selected: [], custom: ' docs too ' });
    expect(askAnswer(questions, drafts)).toEqual({
      type: 'ask',
      answers: [
        { id: 'scope', selected: [0], custom: null },
        { id: 'extras', selected: [], custom: 'docs too' },
      ],
    });
  });

  test('approve offers once, session, grants with always, deny', () => {
    const choices = approveChoices({
      full: 'bash: rm -rf target',
      compact: 'rm -rf target',
      session_label: 'Allow for this session',
      grants: [{ scope: 'folder', label: 'Allow in ~/src', always_label: 'Always allow in ~/src' }],
    });
    expect(choices.map((choice) => choice.decision)).toEqual([
      { type: 'once' },
      { type: 'session' },
      { type: 'grant', index: 0, always: false },
      { type: 'grant', index: 0, always: true },
      { type: 'deny' },
    ]);
    expect(approveChoices({ full: 'x', compact: 'x', session_label: null, grants: [] }).map((choice) => choice.label)).toEqual([
      'Allow once',
      'Deny',
    ]);
  });

  test('countdown', () => {
    expect(remainingMs(null, 0, 10)).toBeNull();
    expect(remainingMs(30_000, 1000, 6000)).toBe(25_000);
    expect(remainingMs(30_000, 0, 99_000)).toBe(0);
    expect(countdown(65_000)).toBe('1:05');
    expect(countdown(8_200)).toBe('0:09');
  });
});

describe('pending tracker', () => {
  const a = request({ request: 'u-1' });
  const b = request({ request: 'u-2' });

  test('keeps first-seen times and reports requests that left unanswered', () => {
    let tracker = trackPending(EMPTY_TRACKER, [a], 100);
    tracker = trackPending(tracker, [a, b], 200);
    expect(tracker.seen.get('u-1')).toBe(100);
    expect(tracker.seen.get('u-2')).toBe(200);
    const same = trackPending(tracker, [a, b], 300);
    expect(same).toBe(tracker);
    tracker = trackPending(tracker, [b], 400);
    expect(tracker.gone.map((entry) => entry.request.request)).toEqual(['u-1']);
    tracker = trackPending(tracker, [b], 400 + GONE_MS);
    expect(tracker.gone).toEqual([]);
  });

  test('a request vanishing while this phone answers it is the phone’s, whichever event comes first', () => {
    let tracker = trackPending(EMPTY_TRACKER, [a, b], 0);
    tracker = noteSending(tracker, 'u-1');
    tracker = trackPending(tracker, [b], 10);
    expect(tracker.gone).toEqual([]);
    tracker = noteAnswer(tracker, a, true, 20);
    tracker = trackPending(tracker, [b], 30);
    expect(tracker.gone).toEqual([]);
    // The desktop won the race: the outcome says so even after the request vanished.
    tracker = noteSending(tracker, 'u-2');
    tracker = trackPending(tracker, [], 40);
    tracker = noteAnswer(tracker, b, false, 50);
    expect(tracker.gone.map((entry) => entry.request.request)).toEqual(['u-2']);
  });

  test('a failed answer releases the request', () => {
    let tracker = noteSending(trackPending(EMPTY_TRACKER, [a], 0), 'u-1');
    tracker = noteAnswer(tracker, a, null, 10);
    tracker = trackPending(tracker, [], 20);
    expect(tracker.gone.map((entry) => entry.request.request)).toEqual(['u-1']);
  });

  test('an applied answer vanishes quietly; a late one shows as answered on desktop', () => {
    let tracker = trackPending(EMPTY_TRACKER, [a, b], 0);
    tracker = noteAnswer(tracker, a, true, 10);
    tracker = trackPending(tracker, [b], 20);
    expect(tracker.gone).toEqual([]);
    tracker = noteAnswer(tracker, b, false, 30);
    expect(tracker.gone.map((entry) => entry.request.request)).toEqual(['u-2']);
    tracker = trackPending(tracker, [], 40);
    expect(tracker.gone).toHaveLength(1);
    expect(dismissGone(tracker, 'u-2').gone).toEqual([]);
  });
});

describe('composer', () => {
  const attachment = (patch: Partial<Attachment>): Attachment => ({
    id: 1,
    name: 'a.png',
    mime: 'image/png',
    size: 1000,
    status: 'done',
    sent: 1000,
    upload: 'up-1',
    error: null,
    ...patch,
  });

  test('modes per phase and following busy/idle', () => {
    expect(modesFor('idle')).toEqual(['send', 'insert']);
    expect(modesFor('busy')).toEqual(['steer', 'follow_up', 'insert']);
    expect(modesFor('exited')).toEqual([]);
    expect(followMode('send', 'idle', 'busy')).toBe('steer');
    expect(followMode('follow_up', 'busy', 'idle')).toBe('send');
    expect(followMode('insert', 'idle', 'busy')).toBe('insert');
    expect(followMode('follow_up', 'busy', 'busy')).toBe('follow_up');
    expect(followMode('send', 'busy', 'busy')).toBe('steer');
  });

  test('the phase follows omp’s activity, as the gateway gates compose on it', () => {
    const live = (activity: api.Activity, state: api.Status['state'] = 'idle') =>
      ({ activity, status: { state } }) as Pick<api.LiveSession, 'status' | 'activity'>;
    expect(phaseOf(live('idle'))).toBe('idle');
    expect(phaseOf(live('busy'))).toBe('busy');
    // A session whose omp has not drawn its title yet refuses send at the gateway (Precondition).
    expect(phaseOf(live('unknown'))).toBe('starting');
    expect(phaseOf(live('attention'))).toBe('attention');
    expect(phaseOf(live('idle', 'exited'))).toBe('exited');
    expect(modesFor('starting')).toEqual(['insert']);
    expect(modesFor('attention')).toEqual(['insert']);
    expect(followMode('send', 'idle', 'starting')).toBe('insert');
    // Insert was forced while starting/waiting, not chosen: the next phase takes its default.
    expect(followMode('insert', 'starting', 'idle')).toBe('send');
    expect(followMode('insert', 'attention', 'busy')).toBe('steer');
    expect(followMode('insert', 'idle', 'busy')).toBe('insert');
    expect(composeProblem('send', 'starting', 'x', [])).toContain('starting');
    expect(composeProblem('send', 'attention', 'x', [])).toContain('answer');
    expect(composeProblem('insert', 'starting', 'x', [])).toBeNull();
  });

  test('steer and follow-up refuse empty text; send takes attachments alone', () => {
    expect(composeProblem('steer', 'busy', '  ', [])).toContain('needs text');
    expect(composeProblem('follow_up', 'busy', '', [attachment({})])).toContain('needs text');
    expect(composeProblem('steer', 'busy', 'go left', [])).toBeNull();
    expect(composeProblem('send', 'idle', '', [attachment({})])).toBeNull();
    expect(composeProblem('send', 'idle', '', [])).not.toBeNull();
    expect(composeProblem('send', 'busy', 'x', [])).not.toBeNull();
    expect(composeProblem('send', 'idle', 'x', [attachment({ status: 'uploading' })])).toContain('upload');
    expect(composeProblem('insert', 'exited', 'x', [])).toContain('exited');
  });

  test('attachment limits', () => {
    expect(attachProblem({ name: 'big', size: MAX_UPLOAD_FILE + 1 }, [])).toContain('limit');
    expect(attachProblem({ name: 'empty', size: 0 }, [])).toContain('empty');
    const full = Array.from({ length: MAX_UPLOADS }, (_, id) => attachment({ id }));
    expect(attachProblem({ name: 'x', size: 10 }, full)).toContain(`${MAX_UPLOADS}`);
    expect(attachProblem({ name: 'x', size: 10 }, [attachment({ status: 'error' })])).toBeNull();
  });

  test('attachment reducer tracks progress and collects finished ids in order', () => {
    let state = attachmentsReducer([], { type: 'add', attachment: attachment({ id: 1, status: 'uploading', sent: 0, upload: null }) });
    state = attachmentsReducer(state, { type: 'add', attachment: attachment({ id: 2, status: 'uploading', sent: 0, upload: null }) });
    state = attachmentsReducer(state, { type: 'progress', id: 1, sent: 5000 });
    expect(state[0]?.sent).toBe(1000);
    state = attachmentsReducer(state, { type: 'done', id: 2, upload: 'up-2' });
    expect(uploadIds(state)).toEqual(['up-2']);
    state = attachmentsReducer(state, { type: 'failed', id: 1, error: 'too large' });
    expect(state[0]?.status).toBe('error');
    state = attachmentsReducer(state, { type: 'remove', id: 1 });
    expect(state.map((entry) => entry.id)).toEqual([2]);
  });

  test('file sizes', () => {
    expect(fileSize(812)).toBe('812 B');
    expect(fileSize(4300)).toBe('4.2 KB');
    expect(fileSize(13 * 1024 * 1024)).toBe('13 MB');
  });
});

describe('image cache', () => {
  const urls = () => {
    const revoked: string[] = [];
    let next = 0;
    return { revoked, factory: { create: () => `blob:${next++}`, revoke: (url: string) => revoked.push(url) } };
  };

  test('shares one fetch and evicts least recently used, never retained images', async () => {
    const { revoked, factory } = urls();
    const cache = new ImageCache(factory, 10);
    let fetches = 0;
    const fetch = (size: number) => () => {
      fetches += 1;
      return Promise.resolve(new Blob([new Uint8Array(size)]));
    };
    const [one, again] = await Promise.all([cache.load('a', fetch(4)), cache.load('a', fetch(4))]);
    expect(one).toBe(again);
    expect(fetches).toBe(1);
    cache.retain('a');
    await cache.load('b', fetch(4));
    await cache.load('c', fetch(4));
    // Over budget: "a" is retained, so the next least recent ("b") goes.
    expect(revoked).toEqual(['blob:1']);
    expect(cache.peek('b')).toBeUndefined();
    cache.release('a');
    expect(cache.bytes).toBeLessThanOrEqual(10);
  });

  test('a failed fetch can be retried', async () => {
    const { factory } = urls();
    const cache = new ImageCache(factory, 100);
    await expect(cache.load('x', () => Promise.reject(new Error('gone')))).rejects.toThrow('gone');
    expect(await cache.load('x', () => Promise.resolve(new Blob(['ok'])))).toBe('blob:0');
  });

  test('large images wait for a tap', () => {
    expect(autoLoads({ blob: 'b', mime: 'image/png', bytes: 0 })).toBe(true);
    expect(autoLoads({ blob: 'b', mime: 'image/png', bytes: 50 * 1024 * 1024 })).toBe(false);
  });
});

describe('context breakdown', () => {
  const view: api.ContextView = {
    model: 'opus',
    context_window: 200_000,
    used_tokens: 90_000,
    anchored: false,
    categories: [
      { id: 'sys', label: 'System prompt', tokens: 10_000 },
      { id: 'msg', label: 'Messages', tokens: 80_000 },
      { id: 'none', label: 'Empty', tokens: 0 },
    ],
    auto_compact_buffer: 30_000,
    free_tokens: 80_000,
    threshold_tokens: 170_000,
    snapcompact_saved: null,
    tools: [
      { name: 'bash', tokens: 900 },
      { name: 'edit', tokens: 1500 },
    ],
    skills: 3,
    computed_unix_ms: 0,
  };

  test('segments: categories largest first, then free and buffer', () => {
    const segments = contextSegments(view);
    expect(segments.map((segment) => segment.id)).toEqual(['msg', 'sys', 'free', 'buffer']);
    expect(segments[0]?.share).toBe(0.4);
    expect(thresholdMark(view)).toBe(0.85);
    expect(thresholdMark({ ...view, threshold_tokens: null })).toBeNull();
    expect(topTools(view, 1)).toEqual([{ name: 'edit', tokens: 1500 }]);
    expect(percent(0.051)).toBe('5.1%');
    expect(percent(0.4)).toBe('40%');
  });
});

describe('review comments', () => {
  const diff = [
    'diff --git a/src/a.rs b/src/a.rs',
    '--- a/src/a.rs',
    '+++ b/src/a.rs',
    '@@ -10,4 +10,5 @@ fn main() {',
    ' let a = 1;',
    '-let b = 2;',
    '+let b = 3;',
    '+let c = 4;',
    ' run(a, b);',
    '@@ -40,2 +41,2 @@',
    ' end();',
    '',
  ].join('\n');

  test('numbers new-file lines across hunks', () => {
    const rows = diffRows(diff);
    expect(rows.map((row) => [row.kind, row.newLine, row.oldLine])).toEqual([
      ['meta', null, null],
      ['meta', null, null],
      ['meta', null, null],
      ['hunk', null, null],
      ['context', 10, 10],
      ['minus', null, 11],
      ['plus', 11, null],
      ['plus', 12, null],
      ['context', 13, 12],
      ['hunk', null, null],
      ['context', 41, 40],
    ]);
  });

  test('selection ranges skip removed lines and stop at hunk borders', () => {
    const rows = diffRows(diff);
    expect(selectionRange(rows, 7, 4)).toEqual({ start: 10, end: 12 });
    expect(selectionRange(rows, 5, 5)).toBeNull();
    expect(selectionRange(rows, 8, 10)).toBeNull();
    expect([...rowsOf(rows, { start: 11, end: 12 })]).toEqual([6, 7]);
    expect(quoteOf(rows, 11, 13, 2)).toEqual(['let b = 3;', 'let c = 4;']);
  });

  test('reducer sorts, edits and deletes; payload drops blanks', () => {
    let draft = reviewReducer(EMPTY_REVIEW, {
      type: 'add',
      comment: { file: 'b.rs', side: 'unstaged', start: 3, end: 3, text: 'x', quote: [] },
    });
    draft = reviewReducer(draft, { type: 'add', comment: { file: 'a.rs', side: 'staged', start: 9, end: 12, text: ' y ', quote: [] } });
    expect(draft.comments.map((entry) => entry.file)).toEqual(['a.rs', 'b.rs']);
    draft = reviewReducer(draft, { type: 'edit', id: 1, text: '   ' });
    expect(reviewPayload(draft)).toEqual({ comments: [{ file: 'a.rs', side: 'staged', start: 9, end: 12, text: 'y' }], general: null });
    draft = reviewReducer(draft, { type: 'general', text: 'Overall fine' });
    expect(reviewCount(draft)).toBe(2);
    draft = reviewReducer(reviewReducer(draft, { type: 'delete', id: 2 }), { type: 'general', text: '' });
    expect(reviewPayload(draft)).toBeNull();
  });

  test('store keeps drafts per project and survives a reload through storage', () => {
    const backing = new Map<string, string>();
    const storage = {
      getItem: (key: string) => backing.get(key) ?? null,
      setItem: (key: string, value: string) => void backing.set(key, value),
      removeItem: (key: string) => void backing.delete(key),
    };
    const store = new ReviewStore(storage);
    let calls = 0;
    store.subscribe(() => (calls += 1));
    store.dispatch('p1', { type: 'general', text: 'hello' });
    expect(calls).toBe(1);
    expect(store.get('p2')).toBe(EMPTY_REVIEW);
    const reloaded = new ReviewStore(storage);
    expect(reloaded.get('p1').general).toBe('hello');
    reloaded.dispatch('p1', { type: 'clear' });
    expect(backing.size).toBe(0);
    backing.set('ompdeck.review.bad', '{"comments":[{"id":"x"}],"general":""}');
    expect(new ReviewStore(storage).get('bad')).toBe(EMPTY_REVIEW);
  });
});
