// Agents: a dashboard card per live session (status, model, context, cost, last reply, changed
// files) with a distinct subagent accordion that loads the session's subagents on demand.

import { type ReactNode, useEffect, useMemo, useState } from 'react';
import type { api } from '../../remote/index.ts';
import { useNav } from '../nav.ts';
import { subagentIds } from '../transcript/adapter.ts';
import { age, dollars, perform, shortModel, statusLabel, succeeded, tokens } from '../ui/format.ts';
import { useCan, useClient, useClientState, useFeed, useNow, useTranscript } from '../ui/hooks.ts';
import { ConfirmSheet, ConnectionPill, Empty, StaleBanner, useS4 } from '../ui/kit.tsx';
import { findModel } from './Conversation.tsx';
import '../../styles/fleet.css';

export function AgentsScreen(): ReactNode {
  const client = useClient();
  const liveFeed = useFeed({ feed: 'live' }, 'live');
  const online = useClientState((state) => state.conn.phase === 'online');
  const models = useClientState((state) => state.models);
  const sessions = useMemo(
    () => (liveFeed?.sessions ?? []).filter((live) => live.kind === 'session' && live.session !== null),
    [liveFeed],
  );
  const ids = sessions.map((live) => live.session ?? '').sort();
  const cards = useFeed(ids.length === 0 ? null : { feed: 'cards', sessions: ids }, 'cards');
  useEffect(() => {
    if (online) client.loadModels().catch(() => undefined);
  }, [client, online]);
  const working = sessions.filter((live) => live.busy || live.status.state === 'busy').length;

  return (
    <div className="screen fleet">
      <header className="top">
        <div className="line spread">
          <h2>Agents</h2>
          <span className="pill">
            {sessions.length} live{working > 0 ? ` · ${working} working` : ''}
          </span>
        </div>
        <div className="line" style={{ marginTop: 6 }}>
          <ConnectionPill />
        </div>
      </header>
      <div className="body">
        <StaleBanner />
        {liveFeed === undefined ? (
          <Empty title="Loading…" />
        ) : sessions.length === 0 ? (
          <Empty title="No sessions running">Open or resume a session from Sessions.</Empty>
        ) : (
          sessions.map((live) => (
            <FleetCard
              key={live.live.key}
              live={live}
              card={cards?.cards.find((card) => card.session === live.session) ?? null}
              models={models?.models}
            />
          ))
        )}
      </div>
    </div>
  );
}

function FleetCard({
  live,
  card,
  models,
}: {
  live: api.LiveSession;
  card: api.Card | null;
  models: readonly api.Model[] | undefined;
}): ReactNode {
  const client = useClient();
  const nav = useNav();
  const can = useCan();
  const [open, setOpen] = useState(false);
  const title = live.title ?? live.label;
  const model = card?.model ?? live.model;
  const info = findModel(models, model);
  const share =
    card?.context_tokens != null && info?.context_window != null && info.context_window > 0
      ? Math.min(1, card.context_tokens / info.context_window)
      : null;
  const busy = live.busy || live.status.state === 'busy';
  const changed = card?.git == null ? 0 : card.git.staged + card.git.unstaged + card.git.untracked;
  const openConversation = () => nav.push({ kind: 'conversation', session: live.session, liveKey: live.live.key, title });
  return (
    <article className="fleet-card">
      <div className="line spread fleet-title">
        <button type="button" className="grow" style={{ textAlign: 'left' }} onClick={openConversation}>
          {title}
        </button>
        <span className={`pill${live.status.state === 'attention' ? ' warn' : ''}`}>{statusLabel(live)}</span>
      </div>
      <div className="model">
        <span>
          {shortModel(model)}
          {card?.thinking != null ? ` · ${card.thinking}` : ''}
        </span>
        <strong>{card?.cost != null ? dollars(card.cost) : ''}</strong>
      </div>
      <div className="stats">
        <span className="track" aria-hidden="true">
          <i style={{ width: `${Math.round((share ?? 0) * 100)}%` }} />
        </span>
        <span>{share !== null ? `${Math.round(share * 100)}% context` : card?.context_tokens != null ? `${tokens(card.context_tokens)} context` : 'context unknown'}</span>
        <span style={{ marginLeft: 'auto' }}>
          {card !== null ? `↑${tokens(card.input_tokens)} ↓${tokens(card.output_tokens)}` : ''}
        </span>
      </div>
      {card?.last_reply != null && (
        <div className={`snippet${card.last_turn_failed ? ' warn' : ''}`}>{card.last_reply}</div>
      )}
      <div className="line wrap">
        {changed > 0 && live.project !== null && (
          <button
            type="button"
            className="files"
            onClick={() => nav.push({ kind: 'changes', project: live.project ?? '', display: live.display })}
          >
            ± {changed} changed file{changed === 1 ? '' : 's'}
          </button>
        )}
        {card?.git != null && <span className="files">⎇ {card.git.branch}</span>}
      </div>
      <div className="card-actions">
        <button type="button" className="button" onClick={openConversation}>
          Chat
        </button>
        <button type="button" className="button" onClick={() => nav.push({ kind: 'terminal', liveKey: live.live.key, title })}>
          Terminal
        </button>
        {busy && (
          <button
            type="button"
            className="button danger"
            disabled={!can({ op: 'interrupt', live: live.live })}
            onClick={() => void perform(client, { op: 'interrupt', live: live.live }, `Interrupt “${title}”`)}
          >
            Interrupt
          </button>
        )}
      </div>
      {card !== null && card.agents.total > 0 && live.session !== null && (
        <div className={`accordion${open ? ' open' : ''}`}>
          <button type="button" className="acc-head" aria-expanded={open} onClick={() => setOpen(!open)}>
            <span>
              {open ? '⌄' : '›'} {card.agents.total} subagent{card.agents.total === 1 ? '' : 's'} · {card.agents.running} running
              {card.agents.failed > 0 ? ` · ${card.agents.failed} failed` : ''}
            </span>
            <span className="acc-toggle">{open ? 'Hide' : 'Show'}</span>
          </button>
          {open && <Subagents session={live.session} live={live} />}
        </div>
      )}
    </article>
  );
}

function Subagents({ session, live }: { session: string; live: api.LiveSession }): ReactNode {
  const s4 = useS4();
  return s4 ? <AgentList session={session} live={live} /> : <TranscriptAgents session={session} />;
}

const STATUS_TONE: Record<api.AgentStatus, string> = {
  running: 'accent',
  done: 'ok',
  failed: 'danger',
  aborted: 'warn',
  interrupted: 'warn',
  parked: '',
};

/** The `agents{session}` feed: every subagent with status, model, tokens, cost and its controls. */
function AgentList({ session, live }: { session: string; live: api.LiveSession }): ReactNode {
  const feed = useFeed({ feed: 'agents', session }, 'agents');
  const [expanded, setExpanded] = useState<string | null>(null);
  if (feed === undefined) return <div className="subagent muted">Loading subagents…</div>;
  if (feed.agents.length === 0) return <div className="subagent muted">No subagents.</div>;
  return (
    <div className="agent-list">
      {feed.agents.map((agent) => (
        <AgentRow
          key={agent.id}
          agent={agent}
          session={session}
          live={live}
          open={expanded === agent.id}
          onToggle={() => setExpanded(expanded === agent.id ? null : agent.id)}
        />
      ))}
      {feed.truncated && <div className="subagent muted">Only part of the list was sent.</div>}
    </div>
  );
}

function AgentRow({
  agent,
  session,
  live,
  open,
  onToggle,
}: {
  agent: api.AgentView;
  session: string;
  live: api.LiveSession;
  open: boolean;
  onToggle: () => void;
}): ReactNode {
  const client = useClient();
  const nav = useNav();
  const can = useCan();
  const now = useNow();
  const [text, setText] = useState('');
  const [confirmKill, setConfirmKill] = useState(false);
  const [busy, setBusy] = useState(false);
  const name = agent.label.length > 0 ? agent.label : agent.id;
  const control = live.live_control && live.status.state !== 'exited';
  const run = async (op: api.Op, label: string, success: string): Promise<boolean> => {
    setBusy(true);
    const ok = succeeded(await perform(client, op, label, success));
    setBusy(false);
    return ok;
  };
  const chatOp: api.Op = { op: 'agent_chat', live: live.live, agent: agent.id, text: text.trim() };
  return (
    <div className={`agent-row${open ? ' open' : ''}`} style={{ marginLeft: Math.min(agent.depth, 4) * 10 }}>
      <button type="button" className="subagent" aria-expanded={open} onClick={onToggle}>
        <span className="agent-name">
          <span className={`agent-dot s-${agent.status}`} aria-hidden="true" />
          <b>{name}</b>
          {agent.agent !== null && <small>{agent.agent}</small>}
        </span>
        <span className={`chip ${STATUS_TONE[agent.status]}`}>{agent.status}</span>
      </button>
      <div className="agent-meta">
        <span>{shortModel(agent.model)}{agent.thinking !== null ? ` · ${agent.thinking}` : ''}</span>
        <span>{tokens(agent.tokens)} tok</span>
        <b>{agent.cost === null ? '—' : dollars(agent.cost)}</b>
      </div>
      {open && (
        <div className="agent-panel">
          {agent.current !== null && <p className="snippet">{agent.current}</p>}
          {agent.error !== null && <p className="snippet warn">{agent.error}</p>}
          <dl className="facts">
            <div>
              <dt>Requests</dt>
              <dd>{agent.requests}</dd>
            </div>
            <div>
              <dt>Tool calls</dt>
              <dd>{agent.tool_calls}</dd>
            </div>
            <div>
              <dt>Active</dt>
              <dd>{agent.last_activity_unix_ms === null ? '—' : `${age(agent.last_activity_unix_ms, now)} ago`}</dd>
            </div>
          </dl>
          <div className="card-actions">
            <button type="button" className="button" onClick={() => nav.push({ kind: 'subagent', session, agent: agent.id })}>
              Transcript
            </button>
            {agent.status === 'parked' && (
              <button
                type="button"
                className="button"
                disabled={busy || !control || !can({ op: 'agent_revive', live: live.live, agent: agent.id })}
                onClick={() => void run({ op: 'agent_revive', live: live.live, agent: agent.id }, `Revive ${name}`, `${name} revived.`)}
              >
                Revive
              </button>
            )}
            {agent.status === 'running' && (
              <button
                type="button"
                className="button danger"
                disabled={busy || !control || !can({ op: 'agent_kill', live: live.live, agent: agent.id })}
                onClick={() => setConfirmKill(true)}
              >
                Kill
              </button>
            )}
          </div>
          <form
            className="agent-chat"
            onSubmit={(event) => {
              event.preventDefault();
              void run(chatOp, `Message ${name}`, `Sent to ${name}.`).then((ok) => ok && setText(''));
            }}
          >
            <input
              className="input"
              value={text}
              placeholder={agent.status === 'parked' ? 'Message (revives it)' : agent.status === 'running' ? 'Steer this subagent' : 'Prompt this subagent'}
              onChange={(event) => setText(event.target.value)}
              aria-label={`Message ${name}`}
            />
            <button type="submit" className="button primary" disabled={busy || !control || text.trim().length === 0 || !can(chatOp)}>
              Send
            </button>
          </form>
          {!control && <small className="refusal">Needs the session running with control here.</small>}
        </div>
      )}
      {confirmKill && (
        <ConfirmSheet
          title={`Kill ${name}?`}
          message="This subagent stops at once; its unfinished work is lost. The session and its other subagents keep running."
          facts={[
            ['Subagent', name],
            ['Session', live.title ?? live.label],
            ['Spent', agent.cost === null ? `${tokens(agent.tokens)} tokens` : `${dollars(agent.cost)} · ${tokens(agent.tokens)} tokens`],
          ]}
          confirm="Kill"
          busy={busy}
          onConfirm={() =>
            void run({ op: 'agent_kill', live: live.live, agent: agent.id }, `Kill ${name}`, `${name} killed.`).then(() => setConfirmKill(false))
          }
          onCancel={() => setConfirmKill(false)}
        />
      )}
    </div>
  );
}

/** An older desktop (no `agents` feed): the subagents named in the recent conversation. */
function TranscriptAgents({ session }: { session: string }): ReactNode {
  const nav = useNav();
  const transcript = useTranscript({ feed: 'transcript', session, tail: 300 });
  const ids = useMemo(() => (transcript === undefined ? [] : subagentIds(transcript.items)), [transcript]);
  if (transcript === undefined) return <div className="subagent muted">Loading…</div>;
  if (ids.length === 0) return <div className="subagent muted">No subagents in the recent conversation.</div>;
  return (
    <>
      {ids.map((id) => (
        <button key={id} type="button" className="subagent" onClick={() => nav.push({ kind: 'subagent', session, agent: id })}>
          <span>◇ {id}</span>
          <span className="go">open ›</span>
        </button>
      ))}
    </>
  );
}
