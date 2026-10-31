// A subagent's transcript inside its parent session (read-only).

import { type ReactNode, useMemo, useState } from 'react';
import { api } from '../../remote/index.ts';
import type { ToolRenderHost } from '../../vendor/omp-collab/tool-render/index.ts';
import { useNav } from '../nav.ts';
import { adaptTranscript } from '../transcript/adapter.ts';
import { useNow, useTranscript } from '../ui/hooks.ts';
import { Empty, StaleBanner } from '../ui/kit.tsx';
import { TranscriptList } from './Transcript.tsx';

export function SubagentScreen({ session, agent }: { session: string; agent: string }): ReactNode {
  const nav = useNav();
  const now = useNow();
  const [tail, setTail] = useState(200);
  const transcript = useTranscript({ feed: 'transcript', session, agent, tail });
  const entries = useMemo(
    () => (transcript === undefined ? [] : adaptTranscript(transcript.items, transcript.start)),
    [transcript],
  );
  const host = useMemo<ToolRenderHost>(
    () => ({ openAgent: (child: string) => nav.push({ kind: 'subagent', session, agent: child }) }),
    [nav, session],
  );
  const source = useMemo(() => ({ session, agent }), [session, agent]);
  return (
    <div className="screen pushed chat">
      <header className="top">
        <div className="eyebrow">Subagent</div>
        <div className="line">
          <button type="button" className="back" onClick={nav.pop} aria-label="Back">
            ‹
          </button>
          <h2 className="grow">{agent}</h2>
        </div>
      </header>
      <div className="chat-content" style={{ paddingBottom: 32 }}>
        <StaleBanner />
        {transcript !== undefined && transcript.start > 0 && tail < api.MAX_TAIL && (
          <button type="button" className="more-row" onClick={() => setTail(api.MAX_TAIL)}>
            Load earlier messages
          </button>
        )}
        {transcript === undefined ? (
          <Empty title="Loading the subagent…" />
        ) : entries.length === 0 ? (
          <Empty title="No messages">This subagent has not written anything yet.</Empty>
        ) : (
          <TranscriptList entries={entries} busy={false} host={host} now={now} source={source} />
        )}
      </div>
    </div>
  );
}
