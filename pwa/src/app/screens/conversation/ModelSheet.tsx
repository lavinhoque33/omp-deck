// The model and effort picker. The catalog, the favorites and the provider filter come from the
// desktop's shared preferences (read with `models`, written with `model_favorite`/`model_provider`),
// so a star or a provider set here shows on the desktop and the other way round.

import { type ReactNode, useCallback, useEffect, useMemo, useState } from 'react';
import { api } from '../../../remote/index.ts';
import { perform, succeeded, tokens } from '../../ui/format.ts';
import { useCan, useClient, useClientState } from '../../ui/hooks.ts';
import { Sheet } from '../../ui/kit.tsx';
import { buildModelList } from './modelList.ts';

export function ModelSheet({
  live,
  current,
  effort,
  onClose,
}: {
  live: api.LiveSession;
  current: api.Model | null;
  effort: string | null;
  onClose: () => void;
}): ReactNode {
  const client = useClient();
  const can = useCan();
  const models = useClientState((state) => state.models);
  const [filter, setFilter] = useState('');
  const [busy, setBusy] = useState(false);
  // Preference writes are serialized on their own flag: a model switch never blocks a star, and a
  // second write cannot start before the first one's refreshed catalog is in.
  const [prefBusy, setPrefBusy] = useState(false);
  const [refreshing, setRefreshing] = useState(true);
  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      await client.loadModels(true);
    } catch (error) {
      client.notify(error instanceof Error ? error.message : String(error), 'error');
    } finally {
      setRefreshing(false);
    }
  }, [client]);
  useEffect(() => {
    void refresh();
  }, [refresh]);
  const run = async (op: api.Op, label: string) => {
    setBusy(true);
    const view = await perform(client, op, label);
    setBusy(false);
    if (succeeded(view)) onClose();
  };
  const preference = async (op: api.Op, label: string) => {
    if (prefBusy) return;
    setPrefBusy(true);
    try {
      await perform(client, op, label);
      await refresh();
    } finally {
      setPrefBusy(false);
    }
  };
  const favorites = models?.favorites ?? [];
  const view = useMemo(
    () => buildModelList(models?.models ?? [], favorites, models?.provider ?? null, filter),
    [models, favorites, filter],
  );
  const canSwitch = can({ op: 'switch_model', live: live.live, selector: '' }) && !busy;
  const canPrefer = can({ op: 'model_favorite', selector: '', favorite: true }) && !prefBusy && !refreshing;
  const pending = live.pending_switch;
  const total = models?.models.length ?? 0;
  const empty = view.favorites.length === 0 && view.groups.length === 0;

  const star = (selector: string, name: string): ReactNode => {
    const on = favorites.includes(selector);
    return (
      <button
        type="button"
        className="chip"
        aria-pressed={on}
        aria-label={`${on ? 'Unstar' : 'Star'} ${name}`}
        title={on ? 'Remove from favorites' : 'Add to favorites'}
        disabled={!canPrefer}
        onClick={() => void preference({ op: 'model_favorite', selector, favorite: !on }, `${on ? 'Unstar' : 'Star'} ${name}`)}
      >
        {on ? '★' : '☆'}
      </button>
    );
  };
  const row = (model: api.Model, scope: string): ReactNode => (
    <div className="line" key={`${scope}:${model.selector}`}>
      <button
        type="button"
        className="option grow"
        aria-pressed={current?.selector === model.selector}
        disabled={!canSwitch}
        onClick={() =>
          void run(
            { op: 'switch_model', live: live.live, selector: model.selector, level: effort !== null && model.thinking.includes(effort) ? effort : null },
            `Switch to ${model.name}`,
          )
        }
      >
        <span className="grow">
          {model.name}
          <small>
            {model.provider}
            {model.context_window !== null ? ` · ${tokens(model.context_window)} ctx` : ''}
            {model.thinking.length > 0 ? ` · ${model.thinking.length} effort levels` : ''}
          </small>
        </span>
        {current?.selector === model.selector && <span className="chip accent">current</span>}
      </button>
      {star(model.selector, model.name)}
    </div>
  );

  return (
    <Sheet title="Model and effort" icon="◇" onClose={onClose}>
      {pending !== null && (
        <div className="banner">
          <span className="busy" />
          <span className="grow">
            Switching to {pending.selector ?? 'a model'}
            {pending.level !== null ? ` · ${pending.level}` : ''}…
          </span>
        </div>
      )}
      {current !== null && current.thinking.length > 0 && (
        <label className="field">
          <span>Effort</span>
          <div className="line wrap">
            {current.thinking.map((level) => (
              <button
                key={level}
                type="button"
                className="chip"
                aria-pressed={effort === level}
                disabled={!canSwitch}
                onClick={() => void run({ op: 'set_effort', live: live.live, level }, `Effort ${level}`)}
              >
                {level}
              </button>
            ))}
          </div>
        </label>
      )}
      <select
        className="input"
        aria-label="Provider"
        value={view.provider ?? ''}
        disabled={models === null || !canPrefer}
        onChange={(event) =>
          void preference({ op: 'model_provider', provider: event.target.value === '' ? null : event.target.value }, 'Provider filter')
        }
        style={{ marginBottom: 10 }}
      >
        <option value="">All providers ({total})</option>
        {view.providers.map((entry) => (
          <option key={entry.value} value={entry.value}>
            {entry.value} ({entry.count})
          </option>
        ))}
      </select>
      <input className="input" placeholder="Filter models" aria-label="Filter models" value={filter} onChange={(event) => setFilter(event.target.value)} style={{ marginBottom: 10 }} />
      {models === null ? (
        <p className="muted small">Loading models…</p>
      ) : (
        <div className="option-list" aria-busy={refreshing}>
          {view.favorites.length > 0 && (
            <div className="option-list" role="group" aria-label="Favorites">
              <span className="eyebrow">Favorites</span>
              {view.favorites.map((entry) =>
                entry.model !== null ? (
                  row(entry.model, 'fav')
                ) : (
                  <div className="line" key={`fav:${entry.selector}`}>
                    <button type="button" className="option grow" disabled>
                      <span className="grow">
                        {entry.selector}
                        <small>Not available now</small>
                      </span>
                    </button>
                    {star(entry.selector, entry.selector)}
                  </div>
                ),
              )}
            </div>
          )}
          {view.groups.map((group) => (
            <div className="option-list" key={group.provider} role="group" aria-label={group.provider}>
              <span className="eyebrow">{group.provider}</span>
              {group.models.map((model) => row(model, 'all'))}
            </div>
          ))}
          {empty && <p className="muted small">No models match.</p>}
          {models.truncated && <p className="muted small">The list was cut; filter to find others.</p>}
        </div>
      )}
    </Sheet>
  );
}
