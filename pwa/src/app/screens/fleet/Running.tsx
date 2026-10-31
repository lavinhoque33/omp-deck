// Running: every live mux session the desktop has (mapped to a session file or not), with RAM,
// uptime and badges, plus the server's own RAM. Kill acts on the selected sessions only
// (`running_kill`), after a confirmation that states the RAM it frees.

import { type ReactNode, useMemo, useState } from 'react';
import type { api } from '../../../remote/index.ts';
import { useNav } from '../../nav.ts';
import { perform, shortModel, statusLabel } from '../../ui/format.ts';
import { useCan, useClient, useFeed, useNow } from '../../ui/hooks.ts';
import { ConfirmSheet, Empty, NeedsS4, StaleBanner, useS4 } from '../../ui/kit.tsx';
import { doneResult } from './common.ts';
import { bytes, isUnmapped, memoryFreed, plural, uptime } from './logic.ts';
import '../../../styles/fleet.css';

export function RunningScreen(): ReactNode {
  const client = useClient();
  const nav = useNav();
  const can = useCan();
  const s4 = useS4();
  const now = useNow(10_000);
  const running = useFeed(s4 ? { feed: 'running' } : null, 'running');
  const liveFeed = useFeed({ feed: 'live' }, 'live');
  const [selected, setSelected] = useState<ReadonlySet<string>>(new Set());
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);

  const sessions = useMemo(
    () => [...(running?.sessions ?? [])].sort((a, b) => (b.memory_bytes ?? -1) - (a.memory_bytes ?? -1)),
    [running],
  );
  const liveByKey = useMemo(() => {
    const map = new Map<string, api.LiveSession>();
    for (const live of liveFeed?.sessions ?? []) map.set(live.live.key, live);
    return map;
  }, [liveFeed]);
  // Selections of sessions that are gone drop out.
  const chosen = sessions.filter((entry) => selected.has(entry.id));
  const freed = memoryFreed(chosen);
  const total = memoryFreed(sessions);
  const canKill = can({ op: 'running_kill', ids: [] });

  const toggle = (id: string) => {
    const next = new Set(selected);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setSelected(next);
  };

  const kill = async () => {
    setBusy(true);
    const ids = chosen.map((entry) => entry.id);
    const view = await perform(client, { op: 'running_kill', ids }, `Kill ${plural(ids.length, 'running session')}`);
    setBusy(false);
    setConfirming(false);
    const result = doneResult(view);
    if (result?.type !== 'killed') return;
    const failed = result.results.filter((entry) => entry.error !== null);
    const killed = result.results.length - failed.length;
    if (failed.length === 0) client.notify(`Killed ${plural(killed, 'session')}; about ${bytes(freed.bytes)} freed.`);
    else client.notify(`${failed.length} of ${result.results.length} could not be killed: ${failed[0]?.error ?? ''}`, 'error');
    setSelected(new Set(failed.map((entry) => entry.id)));
  };

  const title = (entry: api.RunningView): string => entry.title ?? entry.display ?? 'Unmapped process';

  return (
    <div className="screen pushed running">
      <header className="top">
        <div className="eyebrow">Session manager</div>
        <div className="line">
          <button type="button" className="back" onClick={nav.pop} aria-label="Back">
            ‹
          </button>
          <h2 className="grow">Running</h2>
          <span className="count">{sessions.length}</span>
        </div>
        <div className="meter-row">
          <span className="metric">
            <span className="k">Sessions RAM</span>
            <b>{bytes(total.bytes)}</b>
          </span>
          <span className="metric">
            <span className="k">Server RAM</span>
            <b>{running?.server_bytes == null ? '—' : bytes(running.server_bytes)}</b>
          </span>
          <button
            type="button"
            className="chip"
            disabled={sessions.length === 0}
            onClick={() => setSelected(chosen.length === sessions.length ? new Set() : new Set(sessions.map((entry) => entry.id)))}
          >
            {chosen.length === sessions.length && sessions.length > 0 ? 'Clear' : 'Select all'}
          </button>
        </div>
      </header>
      <div className="body">
        <StaleBanner />
        <NeedsS4 what="The running view">
          {running === undefined ? (
            <Empty title="Loading…" />
          ) : sessions.length === 0 ? (
            <Empty title="Nothing is running">Sessions you start on the desktop or here show up at once.</Empty>
          ) : (
            sessions.map((entry) => {
              const live = entry.live === null ? null : (liveByKey.get(entry.live.key) ?? null);
              const isSelected = selected.has(entry.id);
              const name = title(entry);
              return (
                <article key={entry.id} className={`fleet-card run-card${isSelected ? ' selected' : ''}`}>
                  <div className="run-head">
                    <button
                      type="button"
                      className="check-btn"
                      role="checkbox"
                      aria-checked={isSelected}
                      aria-label={`Select ${name}`}
                      onClick={() => toggle(entry.id)}
                    >
                      <span className="check">{isSelected ? '✓' : ''}</span>
                    </button>
                    <button
                      type="button"
                      className="run-title"
                      onClick={() => {
                        if (live === null) {
                          toggle(entry.id);
                          return;
                        }
                        if (live.kind === 'session') nav.push({ kind: 'conversation', session: live.session, liveKey: live.live.key, title: name });
                        else nav.push({ kind: 'terminal', liveKey: live.live.key, title: name });
                      }}
                    >
                      <b>{name}</b>
                      <small className="mono">{entry.display ?? 'no working folder'}</small>
                    </button>
                    <span className="ram">
                      <b>{entry.memory_bytes === null ? '—' : bytes(entry.memory_bytes)}</b>
                      <small>RAM</small>
                    </span>
                  </div>
                  <div className="badges">
                    <span className="chip">{entry.backend}</span>
                    {isUnmapped(entry) ? (
                      <span className="chip warn">unmapped</span>
                    ) : (
                      <span className="chip ok">{plural(entry.sessions.length, 'session file')}</span>
                    )}
                    {live !== null && (
                      <span className={`chip${live.status.state === 'attention' ? ' warn' : ' accent'}`}>
                        {live.kind === 'tool' ? 'tool' : shortModel(live.model)} · {statusLabel(live)}
                      </span>
                    )}
                    {live?.desktop_controlled === true && <span className="chip">desktop control</span>}
                    {entry.live === null && <span className="chip">not attachable</span>}
                  </div>
                  <dl className="facts">
                    <div>
                      <dt>Uptime</dt>
                      <dd>{entry.started_unix_ms === null ? '—' : uptime(now - entry.started_unix_ms)}</dd>
                    </div>
                    <div>
                      <dt>PID</dt>
                      <dd className="mono">{entry.pid ?? '—'}</dd>
                    </div>
                    <div>
                      <dt>Processes</dt>
                      <dd>{entry.processes ?? '—'}</dd>
                    </div>
                  </dl>
                </article>
              );
            })
          )}
          {running?.truncated === true && <p className="muted small">Only part of the list was sent.</p>}
        </NeedsS4>
      </div>
      {chosen.length > 0 && (
        <div className="fab bulk-bar" role="toolbar" aria-label="Selected running sessions">
          <span>
            {chosen.length} selected · {bytes(freed.bytes)}
          </span>
          <button type="button" className="button" onClick={() => setSelected(new Set())}>
            Clear
          </button>
          <button type="button" className="button danger" disabled={!canKill} onClick={() => setConfirming(true)}>
            Kill
          </button>
        </div>
      )}
      {confirming && (
        <ConfirmSheet
          title={`Kill ${plural(chosen.length, 'running session')}?`}
          message="Each selected process is terminated at once; anything unsaved in it is lost. Other sessions keep running."
          facts={[
            ['RAM freed', `${bytes(freed.bytes)}${freed.unknown > 0 ? ` + ${freed.unknown} unmeasured` : ''}`],
            ['Sessions', chosen.length],
            ...chosen.slice(0, 4).map((entry, index) => [`#${index + 1}`, `${title(entry)} · ${entry.memory_bytes === null ? '—' : bytes(entry.memory_bytes)}`] as const),
            ...(chosen.length > 4 ? [['More', `${chosen.length - 4} others`] as const] : []),
          ]}
          confirm={`Kill ${chosen.length}`}
          busy={busy}
          onConfirm={() => void kill()}
          onCancel={() => setConfirming(false)}
        />
      )}
    </div>
  );
}
