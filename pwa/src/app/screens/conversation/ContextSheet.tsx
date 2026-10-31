// The context meter's breakdown sheet: the `context` feed (omp's `/context`: categories, free space,
// auto-compact buffer and threshold, top tool schemas) plus the session card's token and cost
// counters. A desktop without slice 4 shows the card counters only.

import type { ReactNode } from 'react';
import type { api } from '../../../remote/index.ts';
import { age, dollars, shortModel, tokens } from '../../ui/format.ts';
import { useFeed, useNow } from '../../ui/hooks.ts';
import { KeyValues, Sheet, useS4 } from '../../ui/kit.tsx';
import { contextSegments, percent, thresholdMark, topTools, usedShare } from './context.ts';

export function ContextSheet({
  live,
  card,
  model,
  effort,
  contextWindow,
  onClose,
}: {
  live: api.LiveSession | null;
  card: api.Card | null;
  model: string | null;
  effort: string | null;
  contextWindow: number | null;
  onClose: () => void;
}): ReactNode {
  const s4 = useS4();
  const now = useNow(5000);
  const feed = useFeed(s4 && live !== null ? { feed: 'context', live: live.live } : null, 'context');
  const view = feed?.context ?? null;
  const counters: (readonly [string, ReactNode])[] = [
    ['Model', <span className="chip accent">{shortModel(view?.model ?? model)}</span>],
    ['Effort', <span className="chip">{effort ?? 'default'}</span>],
    ['Input', card !== null ? `${tokens(card.input_tokens)} tokens` : 'unknown'],
    ['Output', card !== null ? `${tokens(card.output_tokens)} tokens` : 'unknown'],
    ['Cost', card?.cost != null ? <span className="chip ok">{dollars(card.cost)}</span> : 'unknown'],
  ];

  return (
    <Sheet title="Context" icon="◔" onClose={onClose}>
      {view !== null ? (
        <ContextBreakdown view={view} now={now} />
      ) : (
        <>
          <KeyValues
            rows={[
              ['Context', card?.context_tokens != null ? `${tokens(card.context_tokens)} tokens` : 'unknown'],
              ['Window', contextWindow !== null ? `${tokens(contextWindow)} tokens` : 'unknown'],
            ]}
          />
          <p className="muted small">
            {!s4
              ? 'The detailed breakdown needs a newer OmpDeck on the desktop.'
              : live === null
                ? 'Resume the session to see its breakdown.'
                : !live.live_control
                  ? 'The breakdown needs live control of this session (start it from OmpDeck).'
                  : feed === undefined
                    ? 'Loading the breakdown…'
                    : 'omp has not reported its context yet.'}
          </p>
        </>
      )}
      <h4 className="sheet-section">Session totals</h4>
      <KeyValues rows={counters} />
    </Sheet>
  );
}

function ContextBreakdown({ view, now }: { view: api.ContextView; now: number }): ReactNode {
  const segments = contextSegments(view);
  const mark = thresholdMark(view);
  const tools = topTools(view);
  const toolMax = tools[0]?.tokens ?? 1;
  return (
    <div className="ctx">
      <div className="ctx-head">
        <strong>{tokens(view.used_tokens)}</strong>
        <span className="muted">of {tokens(view.context_window)} tokens</span>
        <span className="chip accent">{percent(usedShare(view))} used</span>
      </div>
      <div className="ctx-bar" role="img" aria-label={`${percent(usedShare(view))} of the context window used`}>
        {segments.map((segment, index) => (
          <i
            key={segment.id}
            className={`seg ${segment.kind}${segment.kind === 'category' ? ` c${index % 6}` : ''}`}
            style={{ width: `${segment.share * 100}%` }}
          />
        ))}
        {mark !== null && <b className="ctx-mark" style={{ left: `${mark * 100}%` }} title="Auto-compact threshold" />}
      </div>
      {mark !== null && view.threshold_tokens !== null && (
        <div className="ctx-mark-label" style={{ paddingLeft: `min(${mark * 100}%, calc(100% - 150px))` }}>
          ▲ auto-compact at {tokens(view.threshold_tokens)}
        </div>
      )}
      <ul className="ctx-legend">
        {segments.map((segment, index) => (
          <li key={segment.id}>
            <i
              className={`swatch ${segment.kind}${segment.kind === 'category' ? ` c${index % 6}` : ''}`}
              aria-hidden="true"
            />
            <span className="grow">{segment.label}</span>
            <span className="ctx-tokens">{tokens(segment.tokens)}</span>
            <span className="chip">{percent(segment.share)}</span>
          </li>
        ))}
      </ul>
      <div className="line wrap ctx-facts">
        {view.anchored && <span className="chip ok">anchored</span>}
        {view.snapcompact_saved !== null && <span className="chip accent">snapcompact saved {tokens(view.snapcompact_saved)}</span>}
        <span className="chip">{view.skills} skills</span>
        <span className="chip">measured {age(view.computed_unix_ms, now) === 'now' ? 'just now' : `${age(view.computed_unix_ms, now)} ago`}</span>
      </div>
      {tools.length > 0 && (
        <>
          <h4 className="sheet-section">Largest tool schemas</h4>
          <ul className="ctx-tools">
            {tools.map((tool) => (
              <li key={tool.name}>
                <span className="mono grow">{tool.name}</span>
                <span className="ctx-tool-track" aria-hidden="true">
                  <i style={{ width: `${(tool.tokens / toolMax) * 100}%` }} />
                </span>
                <span className="ctx-tokens">{tokens(tool.tokens)}</span>
              </li>
            ))}
          </ul>
        </>
      )}
    </div>
  );
}
