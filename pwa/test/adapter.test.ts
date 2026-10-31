import { describe, expect, test } from 'bun:test';
import { adaptTranscript, salvageArgs, subagentIds } from '../src/app/transcript/adapter.ts';
import type { api } from '../src/remote/index.ts';

const user = (text: string, at = 1000): api.Item => ({ type: 'user', text, from_agent: false, at_unix_ms: at });
const assistant = (text: string, at = 2000): api.Item => ({ type: 'assistant', text, model: 'anthropic/claude-opus', at_unix_ms: at });
const call = (id: string, name: string, args: string, extra: Record<string, unknown> = {}): api.Item =>
  ({ type: 'tool_call', call_id: id, name, intent: 'Checking', summary: `${name} summary`, args, ...extra }) as api.Item;
const result = (id: string, name: string, text: string, extra: Record<string, unknown> = {}): api.Item =>
  ({ type: 'tool_result', call_id: id, name, text, is_error: false, truncated: false, ...extra }) as api.Item;

describe('adaptTranscript', () => {
  test('groups an assistant turn and pairs calls with results by call id', () => {
    const entries = adaptTranscript(
      [
        user('hi'),
        { type: 'thinking', text: 'hmm' },
        assistant('Let me look.'),
        call('c1', 'bash', '{\n  "command": "ls"\n}'),
        call('c2', 'read', '{\n  "path": "a.ts"\n}'),
        result('c2', 'read', 'file a'),
        result('c1', 'bash', 'x\ny'),
        assistant('Done.'),
        user('thanks', 3000),
      ],
      10,
    );
    expect(entries.map((entry) => entry.kind)).toEqual(['user', 'assistant', 'user']);
    const turn = entries[1];
    if (turn?.kind !== 'assistant') throw new Error('expected an assistant turn');
    expect(turn.key).toBe('11');
    expect(turn.message.content.map((block) => block.type)).toEqual(['thinking', 'text', 'toolCall', 'toolCall', 'text']);
    expect(turn.message.model).toBe('anthropic/claude-opus');
    expect(turn.results.get('c1')?.content[0]).toEqual({ type: 'text', text: 'x\ny' });
    expect(turn.results.get('c2')?.toolName).toBe('read');
    const first = turn.message.content[2];
    if (first?.type !== 'toolCall') throw new Error('expected a call');
    expect(first.arguments).toEqual({ command: 'ls' });
    expect(first.intent).toBe('Checking');
    expect(turn.calls.get('c1')).toEqual({ summary: 'bash summary', complete: true });
    expect(turn.source).toHaveLength(7);
  });

  test('uses structured args, details and image refs from a newer gateway', () => {
    const entries = adaptTranscript(
      [
        call('c1', 'edit', 'cut…', { args_value: { path: 'x.rs', edits: [1, 2] } }),
        result('c1', 'edit', 'ok', { details: { diff: '+a' }, images: [{ blob: 'b1', mime: 'image/png', bytes: 2048 }] }),
      ],
      0,
    );
    const turn = entries[0];
    if (turn?.kind !== 'assistant') throw new Error('expected an assistant turn');
    const block = turn.message.content[0];
    if (block?.type !== 'toolCall') throw new Error('expected a call');
    expect(block.arguments).toEqual({ path: 'x.rs', edits: [1, 2] });
    expect(turn.calls.get('c1')?.complete).toBe(true);
    const paired = turn.results.get('c1');
    expect(paired?.details).toEqual({ diff: '+a' });
    expect(paired?.content).toEqual([{ type: 'text', text: 'ok' }]);
    expect(turn.images.get('c1')).toEqual([{ blob: 'b1', mime: 'image/png', bytes: 2048 }]);
  });

  test('marks structured args with a cut string incomplete', () => {
    const entries = adaptTranscript([call('c1', 'write', '{}', { args_value: { path: 'a', content: 'abc…' } })], 0);
    const turn = entries[0];
    if (turn?.kind !== 'assistant') throw new Error('expected an assistant turn');
    expect(turn.calls.get('c1')?.complete).toBe(false);
  });

  test('falls back to salvaged text args and marks them incomplete', () => {
    const cut = '{\n  "path": "src/main.rs",\n  "content": "fn main() {\\n    println!(\\"hi\\")…';
    const entries = adaptTranscript([call('c1', 'write', cut)], 0);
    const turn = entries[0];
    if (turn?.kind !== 'assistant') throw new Error('expected an assistant turn');
    const block = turn.message.content[0];
    if (block?.type !== 'toolCall') throw new Error('expected a call');
    expect(block.arguments['path']).toBe('src/main.rs');
    expect(block.arguments['content']).toBe('fn main() {\n    println!("hi")…');
    expect(turn.calls.get('c1')).toEqual({ summary: 'write summary', complete: false });
  });

  test('a result whose call is outside the window stands alone', () => {
    const entries = adaptTranscript([result('old', 'bash', 'late output'), user('next')], 40);
    expect(entries[0]).toMatchObject({ kind: 'tool_result', key: '40' });
    expect(entries[1]).toMatchObject({ kind: 'user', key: '41' });
  });

  test('a result arriving after a later turn still joins its call', () => {
    const entries = adaptTranscript(
      [call('c1', 'task', '{}'), { type: 'notice', kind: 'info', text: 'n' }, result('c1', 'task', 'done')],
      0,
    );
    expect(entries.map((entry) => entry.kind)).toEqual(['assistant', 'notice']);
    const turn = entries[0];
    if (turn?.kind !== 'assistant') throw new Error('expected an assistant turn');
    expect(turn.results.get('c1')?.content[0]).toEqual({ type: 'text', text: 'done' });
    expect(turn.source).toHaveLength(2);
  });

  test('maps markers and unknown items in place', () => {
    const entries = adaptTranscript(
      [
        { type: 'compaction', tokens_before: 100_000, tokens_after: 9000, summary: 's' },
        { type: 'model_change', model: 'm' },
        { type: 'error', message: 'boom' },
        { type: 'irc', from: 'a', agent: 'b', text: 'hello', note: null },
        { type: 'exit', reason: 'quit' },
        { type: 'unknown_item', name: 'future' },
      ],
      0,
    );
    expect(entries.map((entry) => entry.kind)).toEqual(['compaction', 'model_change', 'error', 'irc', 'exit', 'unknown']);
    expect(entries.map((entry) => entry.key)).toEqual(['0', '1', '2', '3', '4', '5']);
  });
});

describe('salvageArgs', () => {
  test('parses whole JSON as complete', () => {
    expect(salvageArgs('{"a":1,"b":[2]}')).toEqual({ args: { a: 1, b: [2] }, complete: true });
  });

  test('keeps complete top-level fields of a cut object', () => {
    const { args, complete } = salvageArgs('{\n  "n": 3,\n  "flag": true,\n  "nested": {\n    "x": 1\n  },\n  "s": "abc…');
    expect(complete).toBe(false);
    expect(args).toEqual({ n: 3, flag: true, s: 'abc…' });
  });

  test('non-object JSON yields no arguments', () => {
    expect(salvageArgs('[1,2]')).toEqual({ args: {}, complete: false });
    expect(salvageArgs('not json')).toEqual({ args: {}, complete: false });
  });
});

describe('subagentIds', () => {
  test('collects task ids (or names) in order, without duplicates', () => {
    const items: api.Item[] = [
      call('t1', 'task', JSON.stringify({ tasks: [{ id: 'RelayTests' }, { name: 'SecurityReview' }] }, null, 2)),
      call('t2', 'task', JSON.stringify({ tasks: [{ id: 'RelayTests' }, { id: 'Docs' }] }, null, 2)),
      call('b', 'bash', '{"command":"ls"}'),
    ];
    expect(subagentIds(items)).toEqual(['RelayTests', 'SecurityReview', 'Docs']);
  });
});
