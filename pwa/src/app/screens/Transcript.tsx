// Renders adapted transcript entries with the vendored collab-web renderers (Markdown, ToolView),
// plus the entries' images (fetched with `blob`, conversation/Images.tsx).
// Tool output and diffs render as text nodes inside the renderers; only the vendored Markdown's
// escaped output and highlighter output use innerHTML.

import { type ReactNode, memo, useEffect, useState } from 'react';
import type { ToolRenderHost } from '../../vendor/omp-collab/tool-render/index.ts';
import { ToolView } from '../../vendor/omp-collab/tool-render/index.ts';
import { Markdown } from '../../vendor/omp-collab/transcript/Markdown.tsx';
import '../../vendor/omp-collab/transcript/transcript.css';
import { highlightReady, loadHighlight, onHighlightReady } from '../highlight.ts';
import type { TranscriptEntry } from '../transcript/adapter.ts';
import { age, tokens } from '../ui/format.ts';
import { type ImageSource, ImageStrip } from './conversation/Images.tsx';

/** Bumps once highlight.js is installed so code blocks render again, highlighted. */
function useHighlightEpoch(): number {
  const [epoch, setEpoch] = useState(highlightReady() ? 1 : 0);
  useEffect(() => {
    if (highlightReady()) return;
    const off = onHighlightReady(() => setEpoch(1));
    loadHighlight().catch(() => undefined);
    return off;
  }, []);
  return epoch;
}

export function TranscriptList({
  entries,
  busy,
  host,
  now,
  source,
}: {
  entries: readonly TranscriptEntry[];
  /** The session is working: the last turn's unanswered calls are still running. */
  busy: boolean;
  host: ToolRenderHost;
  now: number;
  /** Where the entries' images are fetched from (null: not fetchable). */
  source: ImageSource | null;
}): ReactNode {
  const epoch = useHighlightEpoch();
  const lastAssistant = entries.findLastIndex((entry) => entry.kind === 'assistant');
  return (
    <div key={epoch}>
      {entries.map((entry, index) => (
        <Entry key={entry.key} entry={entry} running={busy && index === lastAssistant} host={host} now={now} source={source} />
      ))}
    </div>
  );
}

const Entry = memo(function Entry({
  entry,
  running,
  host,
  now,
  source,
}: {
  entry: TranscriptEntry;
  running: boolean;
  host: ToolRenderHost;
  now: number;
  source: ImageSource | null;
}): ReactNode {
  switch (entry.kind) {
    case 'user': {
      const content = entry.message.content;
      const text =
        typeof content === 'string' ? content : content.map((block) => (block.type === 'text' ? block.text : '')).join('\n');
      return (
        <div className={`user${entry.fromAgent ? ' from-agent' : ''}`}>
          {entry.fromAgent && <div className="speaker">From agent</div>}
          <Markdown text={text} />
          <ImageStrip images={entry.images} source={source} />
        </div>
      );
    }
    case 'assistant': {
      const model = entry.message.model.length > 0 ? entry.message.model : null;
      const at = entry.message.timestamp > 0 ? age(entry.message.timestamp, now) : null;
      return (
        <div className="turn">
          <div className="speaker">
            ✳ OMP{model !== null ? ` · ${model.slice(model.lastIndexOf('/') + 1)}` : ''}
            {at !== null ? ` · ${at === 'now' ? 'just now' : `${at} ago`}` : ''}
          </div>
          {entry.message.content.map((block, index) => {
            switch (block.type) {
              case 'text':
                return (
                  <div className="answer" key={index}>
                    <Markdown text={block.text} />
                  </div>
                );
              case 'thinking':
                return (
                  <details className="thinking" key={index}>
                    <summary>Thinking</summary>
                    {block.thinking}
                  </details>
                );
              case 'toolCall': {
                const result = entry.results.get(block.id);
                const info = entry.calls.get(block.id);
                return (
                  <div key={index}>
                    <ToolView
                      name={block.name}
                      args={block.arguments}
                      result={result}
                      running={result === undefined && running}
                      intent={block.intent}
                      host={host}
                    />
                    {info !== undefined && !info.complete && <div className="tool-fallback">{info.summary}</div>}
                    <ImageStrip images={entry.images.get(block.id) ?? []} source={source} />
                  </div>
                );
              }
              default:
                return null;
            }
          })}
        </div>
      );
    }
    case 'tool_result':
      return (
        <>
          <ToolView name={entry.result.toolName} args={{}} result={entry.result} host={host} />
          <ImageStrip images={entry.images} source={source} />
        </>
      );
    case 'notice':
      return <div className="notice">{entry.text}</div>;
    case 'irc':
      return (
        <div className="notice">
          <span className="chip accent">
            {entry.from ?? 'agent'}
            {entry.agent !== null ? ` → ${entry.agent}` : ''}
          </span>{' '}
          {entry.text}
          {entry.note !== null && <div className="muted small">{entry.note}</div>}
        </div>
      );
    case 'compaction':
      return (
        <details className="notice">
          <summary className="marker">
            Context compacted
            {entry.tokensBefore !== null ? ` · ${tokens(entry.tokensBefore)}` : ''}
            {entry.tokensAfter !== null ? ` → ${tokens(entry.tokensAfter)}` : ''}
          </summary>
          <Markdown text={entry.summary} />
        </details>
      );
    case 'model_change':
      return <div className="marker">Model → {entry.model}</div>;
    case 'error':
      return <div className="notice error">{entry.message}</div>;
    case 'exit':
      return <div className="marker">Session ended · {entry.reason}</div>;
    case 'unknown':
      return <div className="marker">A newer kind of entry ({entry.name})</div>;
  }
});
