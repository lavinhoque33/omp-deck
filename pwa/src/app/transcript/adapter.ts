// Maps the gateway's flat transcript items (`api.Item`) onto the wire entries the vendored
// collab-web renderers expect (mobile-pwa.md "Vendored upstream rendering"):
// - consecutive assistant-side items (text, thinking, tool calls, tool results) form one assistant
//   turn, as one `AssistantMessage`;
// - tool results pair with their calls by `call_id`, across turns too;
// - structured arguments (`args_value`), result `details` and image references are used when a
//   newer gateway sends them; an older gateway's text-only items fall back to the call's
//   pretty-printed `args` text (salvaged field by field when it was cut) and its `summary`.

import type { api } from '../../remote/index.ts';
import { CUT_MARK } from '../../remote/api.ts';
import type {
  AssistantContent,
  AssistantMessage,
  TextContent,
  ToolCallContent,
  ToolResultMessage,
  UserMessage,
} from '../../vendor/omp-collab/wire.ts';
import { isRecord } from '../../vendor/omp-collab/tool-render/util.ts';

/** An image the gateway references but does not inline (fetched with the `blob` op). */
export type ImageRef = api.ImageRef;

export interface ToolCallInfo {
  /** The gateway's one-line summary of the call. */
  summary: string;
  /** Whether `arguments` holds the call's complete arguments. */
  complete: boolean;
}

interface EntryBase {
  /** Stable React key: the transcript index of the entry's first item. */
  key: string;
  /** The items this entry was built from (identity drives re-rendering). */
  source: readonly api.Item[];
}

export type TranscriptEntry = EntryBase &
  (
    | { kind: 'user'; message: UserMessage; fromAgent: boolean; images: readonly ImageRef[] }
    | {
        kind: 'assistant';
        message: AssistantMessage;
        /** Results paired with this turn's calls, by call id. */
        results: ReadonlyMap<string, ToolResultMessage>;
        /** Per call id: how complete its arguments are. */
        calls: ReadonlyMap<string, ToolCallInfo>;
        /** Image references per call id's result. */
        images: ReadonlyMap<string, readonly ImageRef[]>;
      }
    /** A result whose call is outside the loaded window. */
    | { kind: 'tool_result'; result: ToolResultMessage; images: readonly ImageRef[] }
    | { kind: 'notice'; noticeKind: string; text: string }
    | { kind: 'irc'; from: string | null; agent: string | null; text: string; note: string | null }
    | { kind: 'compaction'; tokensBefore: number | null; tokensAfter: number | null; summary: string }
    | { kind: 'model_change'; model: string }
    | { kind: 'error'; message: string }
    | { kind: 'exit'; reason: string }
    | { kind: 'unknown'; name: string }
  );

/**
 * Best-effort arguments from the gateway's pretty-printed `args` text. A whole JSON object parses
 * as is; a text cut at its cap (ending in {@link CUT_MARK}) still yields its complete top-level
 * fields and the cut one as a string.
 */
export function salvageArgs(text: string): { args: Record<string, unknown>; complete: boolean } {
  try {
    const parsed: unknown = JSON.parse(text);
    if (isRecord(parsed)) return { args: parsed, complete: true };
    return { args: {}, complete: false };
  } catch {
    // Cut or not JSON: salvage the top-level fields below.
  }
  const args: Record<string, unknown> = {};
  const field = /^ {2}"((?:[^"\\]|\\.)+)": (.*)$/;
  for (const line of text.split('\n')) {
    const match = field.exec(line);
    if (match === null) continue;
    const name = match[1];
    const raw = match[2];
    if (name === undefined || raw === undefined) continue;
    let key: string;
    try {
      key = JSON.parse(`"${name}"`) as string;
    } catch {
      continue;
    }
    const value = raw.endsWith(',') ? raw.slice(0, -1) : raw;
    try {
      args[key] = JSON.parse(value);
      continue;
    } catch {
      // A cut string value: keep what arrived.
    }
    if (value.startsWith('"')) {
      const body = value.slice(1).replace(new RegExp(`${CUT_MARK}$`), '');
      try {
        args[key] = `${JSON.parse(`"${body.replace(/\\$/, '')}"`) as string}${CUT_MARK}`;
      } catch {
        args[key] = `${body}${CUT_MARK}`;
      }
    }
  }
  return { args, complete: false };
}

/** Whether a structured value holds a string cut at the gateway's cap. */
function hasCut(value: unknown): boolean {
  if (typeof value === 'string') return value.endsWith(CUT_MARK);
  if (Array.isArray(value)) return value.some(hasCut);
  if (isRecord(value)) return Object.values(value).some(hasCut);
  return false;
}

function callArguments(item: Extract<api.Item, { type: 'tool_call' }>): { args: Record<string, unknown>; complete: boolean } {
  if (isRecord(item.args_value)) return { args: item.args_value, complete: !hasCut(item.args_value) };
  return salvageArgs(item.args);
}

const NO_USAGE = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { total: 0 } };

interface Turn {
  key: string;
  source: api.Item[];
  content: AssistantContent[];
  results: Map<string, ToolResultMessage>;
  calls: Map<string, ToolCallInfo>;
  images: Map<string, readonly ImageRef[]>;
  model: string;
  timestamp: number;
}

function toolResult(item: Extract<api.Item, { type: 'tool_result' }>, timestamp: number): ToolResultMessage {
  const message: ToolResultMessage = {
    role: 'toolResult',
    toolCallId: item.call_id,
    toolName: item.name,
    content: [{ type: 'text', text: item.text }],
    isError: item.is_error,
    timestamp,
  };
  if (item.details !== undefined && item.details !== null) message.details = item.details;
  return message;
}

/** Maps transcript items (index `start` first) to renderable entries. */
export function adaptTranscript(items: readonly api.Item[], start: number): TranscriptEntry[] {
  const entries: TranscriptEntry[] = [];
  /** The turn holding each call id seen so far. */
  const owner = new Map<string, Turn>();
  let turn: Turn | null = null;
  let lastAt = 0;

  const close = () => {
    if (turn === null) return;
    entries.push({
      kind: 'assistant',
      key: turn.key,
      source: turn.source,
      message: {
        role: 'assistant',
        content: turn.content,
        model: turn.model,
        usage: NO_USAGE,
        stopReason: 'stop',
        timestamp: turn.timestamp,
      },
      results: turn.results,
      calls: turn.calls,
      images: turn.images,
    });
    turn = null;
  };
  const open = (index: number): Turn => {
    turn ??= {
      key: String(start + index),
      source: [],
      content: [],
      results: new Map(),
      calls: new Map(),
      images: new Map(),
      model: '',
      timestamp: lastAt,
    };
    return turn;
  };

  items.forEach((item, index) => {
    const key = String(start + index);
    switch (item.type) {
      case 'assistant': {
        const current = open(index);
        current.source.push(item);
        if (item.at_unix_ms !== null) {
          lastAt = item.at_unix_ms;
          if (current.timestamp === 0) current.timestamp = item.at_unix_ms;
        }
        if (item.model !== null) current.model = item.model;
        if (item.text.length > 0) current.content.push({ type: 'text', text: item.text });
        return;
      }
      case 'thinking': {
        const current = open(index);
        current.source.push(item);
        current.content.push({ type: 'thinking', thinking: item.text });
        return;
      }
      case 'tool_call': {
        const current = open(index);
        current.source.push(item);
        const { args, complete } = callArguments(item);
        const block: ToolCallContent = { type: 'toolCall', id: item.call_id, name: item.name, arguments: args };
        if (item.intent !== null) block.intent = item.intent;
        current.content.push(block);
        current.calls.set(item.call_id, { summary: item.summary, complete });
        owner.set(item.call_id, current);
        return;
      }
      case 'tool_result': {
        const result = toolResult(item, lastAt);
        const images = item.images ?? [];
        const holder = owner.get(item.call_id);
        if (holder === undefined) {
          close();
          entries.push({ kind: 'tool_result', key, source: [item], result, images });
          return;
        }
        holder.source.push(item);
        holder.results.set(item.call_id, result);
        if (images.length > 0) holder.images.set(item.call_id, images);
        if (holder === turn) return;
        // The result of an earlier turn's call: that entry was already emitted; rebuild its source
        // so it re-renders with the result.
        const emitted = entries.findIndex((entry) => entry.kind === 'assistant' && entry.key === holder.key);
        const previous = entries[emitted];
        if (previous !== undefined && previous.kind === 'assistant') {
          entries[emitted] = {
            ...previous,
            source: [...holder.source],
            results: new Map(holder.results),
            images: new Map(holder.images),
          };
        }
        return;
      }
      case 'user': {
        close();
        if (item.at_unix_ms !== null) lastAt = item.at_unix_ms;
        const images = item.images ?? [];
        const content: TextContent[] = [{ type: 'text', text: item.text }];
        entries.push({
          kind: 'user',
          key,
          source: [item],
          message: { role: 'user', content, timestamp: item.at_unix_ms ?? lastAt },
          fromAgent: item.from_agent,
          images,
        });
        return;
      }
      case 'notice':
        close();
        entries.push({ kind: 'notice', key, source: [item], noticeKind: item.kind, text: item.text });
        return;
      case 'irc':
        close();
        entries.push({ kind: 'irc', key, source: [item], from: item.from, agent: item.agent, text: item.text, note: item.note });
        return;
      case 'compaction':
        close();
        entries.push({
          kind: 'compaction',
          key,
          source: [item],
          tokensBefore: item.tokens_before,
          tokensAfter: item.tokens_after,
          summary: item.summary,
        });
        return;
      case 'model_change':
        close();
        entries.push({ kind: 'model_change', key, source: [item], model: item.model });
        return;
      case 'error':
        close();
        entries.push({ kind: 'error', key, source: [item], message: item.message });
        return;
      case 'exit':
        close();
        entries.push({ kind: 'exit', key, source: [item], reason: item.reason });
        return;
      default:
        close();
        entries.push({ kind: 'unknown', key, source: [item], name: item.name });
        return;
    }
  });
  close();
  return entries;
}

/** The subagent ids a session's `task` calls name (for the agents accordion and drill-down). */
export function subagentIds(items: readonly api.Item[]): string[] {
  const ids: string[] = [];
  const seen = new Set<string>();
  for (const item of items) {
    if (item.type !== 'tool_call' || item.name !== 'task') continue;
    const { args } = callArguments(item);
    const tasks = args['tasks'];
    if (!Array.isArray(tasks)) continue;
    for (const task of tasks) {
      if (!isRecord(task)) continue;
      const id = typeof task['id'] === 'string' ? task['id'] : typeof task['name'] === 'string' ? task['name'] : null;
      if (id === null || seen.has(id)) continue;
      seen.add(id);
      ids.push(id);
    }
  }
  return ids;
}
