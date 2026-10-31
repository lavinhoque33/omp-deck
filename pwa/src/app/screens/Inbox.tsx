// Inbox: the desktop's finished/attention/error entries (open, dismiss, clear) and this phone's
// own requests with their recovered outcomes (resend the ones that never arrived).

import { type ReactNode, useState } from 'react';
import type { api } from '../../remote/index.ts';
import { isFinal } from '../client.ts';
import { useNav } from '../nav.ts';
import { age, describeResult, perform, requestStatusText } from '../ui/format.ts';
import { useCan, useClient, useClientState, useFeed, useNow } from '../ui/hooks.ts';
import { Card, ConfirmSheet, ConnectionPill, Empty, StaleBanner } from '../ui/kit.tsx';

function kindLabel(kind: api.InboxKind): { icon: string; text: string; tone: string } {
  switch (kind.type) {
    case 'finished':
      return { icon: '✓', text: 'Finished', tone: 'ok' };
    case 'attention':
      return { icon: '⚠', text: 'Needs input', tone: 'warn' };
    case 'error':
      return { icon: '✕', text: kind.message ?? 'Error', tone: 'danger' };
    case 'exited':
      return { icon: '■', text: `Exited${kind.code === null ? '' : ` (${kind.code})`}`, tone: 'danger' };
  }
}

export function InboxScreen(): ReactNode {
  const client = useClient();
  const nav = useNav();
  const can = useCan();
  const now = useNow();
  const inbox = useFeed({ feed: 'inbox' }, 'inbox');
  const live = useFeed({ feed: 'live' }, 'live');
  const requests = useClientState((state) => state.requests);
  const [confirmClear, setConfirmClear] = useState(false);
  const entries = inbox?.inbox.entries ?? [];

  const open = async (entry: api.InboxItem) => {
    const session = live?.sessions.find((candidate) => candidate.live.key === entry.live.key) ?? null;
    void perform(client, { op: 'inbox_open', live: entry.live }, `Open “${entry.title}”`);
    nav.push({ kind: 'conversation', session: session?.session ?? null, liveKey: entry.live.key, title: entry.title });
  };

  return (
    <div className="screen">
      <header className="top">
        <div className="line spread">
          <h2>Inbox</h2>
          <button
            type="button"
            className="icon-btn"
            disabled={entries.length === 0 || !can({ op: 'inbox_clear' })}
            onClick={() => setConfirmClear(true)}
          >
            Clear all
          </button>
        </div>
        <ConnectionPill />
      </header>
      <div className="body">
        <StaleBanner />
        {inbox === undefined ? (
          <Empty title="Loading…" />
        ) : entries.length === 0 ? (
          <Empty title="All caught up">Finished turns and questions from omp show up here.</Empty>
        ) : (
          entries.map((entry) => {
            const kind = kindLabel(entry.kind);
            return (
              <article key={`${entry.live.key}:${entry.at_unix_ms}`} className={`session${entry.kind.type === 'attention' ? ' attention' : ''}`}>
                <span className={`chip ${kind.tone}`} aria-hidden="true">
                  {kind.icon}
                </span>
                <button type="button" className="copy" style={{ textAlign: 'left' }} onClick={() => void open(entry)}>
                  <b>{entry.title}</b>
                  <small>
                    {kind.text} · {entry.project}
                  </small>
                  {entry.body.length > 0 && <small>{entry.body}</small>}
                </button>
                <span className="age">{age(entry.at_unix_ms, now)}</span>
                <button
                  type="button"
                  className="row-btn"
                  disabled={!can({ op: 'inbox_dismiss', live: entry.live })}
                  onClick={() => void perform(client, { op: 'inbox_dismiss', live: entry.live }, `Dismiss “${entry.title}”`)}
                  aria-label={`Dismiss ${entry.title}`}
                >
                  ✕
                </button>
              </article>
            );
          })
        )}
        {requests.length > 0 && (
          <Card title="This phone’s requests" aside={<span className="count">{requests.length}</span>}>
            {requests.map((request) => {
              const problem = isFinal(request.status) ? describeResult(request) : null;
              const tone =
                problem?.tone === 'error' ? 'danger' : problem?.tone === 'warn' ? 'warn' : isFinal(request.status) ? 'ok' : 'accent';
              return (
                <div key={request.seq} className="line" style={{ padding: '6px 0', borderBottom: '1px solid var(--border)' }}>
                  <span className="grow">
                    <span style={{ display: 'block', fontSize: 11 }}>{request.label}</span>
                    {problem !== null && <span className="muted small">{problem.text}</span>}
                  </span>
                  <span className={`chip ${tone}`}>{requestStatusText(request)}</span>
                  {request.status === 'unknown' && (
                    <button
                      type="button"
                      className="button"
                      disabled={!can(request.op)}
                      onClick={() => {
                        const resent = client.resend(request.seq);
                        resent?.then((view) => {
                          const result = describeResult(view);
                          if (result !== null) client.notify(result.text, result.tone);
                        });
                      }}
                    >
                      Resend
                    </button>
                  )}
                  {isFinal(request.status) && (
                    <button type="button" className="row-btn" onClick={() => client.dismissRequest(request.seq)} aria-label="Dismiss">
                      ✕
                    </button>
                  )}
                </div>
              );
            })}
          </Card>
        )}
      </div>
      {confirmClear && (
        <ConfirmSheet
          title={`Clear ${entries.length} inbox entr${entries.length === 1 ? 'y' : 'ies'}?`}
          message="They are dismissed on the desktop too."
          facts={[['Entries', entries.length]]}
          confirm="Clear all"
          onConfirm={() => {
            setConfirmClear(false);
            void perform(client, { op: 'inbox_clear' }, 'Clear the inbox');
          }}
          onCancel={() => setConfirmClear(false)}
        />
      )}
    </div>
  );
}
