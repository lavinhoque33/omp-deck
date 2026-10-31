// The omp settings editor: every group omp reports, searchable, with a typed editor per setting
// (switch, number, choice, text, JSON for arrays and records), a modified marker with reset, and
// secrets that can only be set or cleared (their value never reaches the phone). Edits need the
// Manage permission; without it the list is read-only.

import { type ReactNode, useCallback, useEffect, useState } from 'react';
import type { api } from '../../../remote/index.ts';
import { ClientError } from '../../client.ts';
import { describeResult } from '../../ui/format.ts';
import { useClient, useOnline } from '../../ui/hooks.ts';
import { Card, ConfirmSheet, Empty, NeedsS4, Sheet, Switch } from '../../ui/kit.tsx';
import { ReadOnlyNote, SectionScreen, ask, useManage } from './common.tsx';
import { type Parsed, filterGroups, parseJsonFor, parseNumber, prettyJson, preview, settingLabel, valueText } from './logic.ts';

export function OmpSettingsSection(): ReactNode {
  return (
    <SectionScreen eyebrow="Settings" title="omp settings">
      <NeedsS4 what="The omp settings editor">
        <OmpSettingsBody />
      </NeedsS4>
    </SectionScreen>
  );
}

type Edit = { setting: api.SettingView; mode: 'json' | 'secret' } | null;

function OmpSettingsBody(): ReactNode {
  const client = useClient();
  const online = useOnline();
  const manage = useManage();
  const [groups, setGroups] = useState<api.SettingsGroup[] | null>(null);
  const [truncated, setTruncated] = useState(false);
  const [loading, setLoading] = useState(false);
  const [query, setQuery] = useState('');
  const [pending, setPending] = useState<ReadonlySet<string>>(new Set());
  const [errors, setErrors] = useState<Readonly<Record<string, string>>>({});
  const [edit, setEdit] = useState<Edit>(null);
  const [clear, setClear] = useState<api.SettingView | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    const reply = await ask(client, { op: 'omp_settings' }, 'omp settings');
    setLoading(false);
    if (reply?.type === 'omp_settings') {
      setGroups(reply.groups);
      setTruncated(reply.truncated);
    }
  }, [client]);
  useEffect(() => {
    if (online && groups === null) void load();
  }, [online, groups, load]);

  /** Runs `op` for `key`; a refusal or failure shows under the row. Resolves whether it took effect. */
  const run = async (key: string, op: api.Op, label: string): Promise<boolean> => {
    setPending((current) => new Set(current).add(key));
    setErrors(({ [key]: _, ...rest }) => rest);
    let problem: string | null = null;
    try {
      const view = await client.mutate(op, label);
      problem = describeResult(view)?.text ?? null;
    } catch (error) {
      problem = error instanceof ClientError || error instanceof Error ? error.message : String(error);
    }
    setPending((current) => {
      const next = new Set(current);
      next.delete(key);
      return next;
    });
    if (problem !== null) setErrors((current) => ({ ...current, [key]: problem }));
    await load();
    return problem === null;
  };
  const set = (setting: api.SettingView, value: api.JsonValue) =>
    run(setting.key, { op: 'omp_set', key: setting.key, value }, `Set ${settingLabel(setting.key)}`);
  const reset = (setting: api.SettingView) =>
    run(setting.key, { op: 'omp_reset', key: setting.key }, `${setting.secret ? 'Clear' : 'Reset'} ${settingLabel(setting.key)}`);

  const shown = groups === null ? [] : filterGroups(groups, query);
  const total = groups?.reduce((sum, group) => sum + group.entries.length, 0) ?? 0;
  const modified = groups?.reduce((sum, group) => sum + group.entries.filter((e) => e.modified).length, 0) ?? 0;

  return (
    <>
      {online && !manage && <ReadOnlyNote need="manage" />}
      <div className="search">
        <span aria-hidden="true">⌕</span>
        <input type="search" placeholder={`Search ${total} settings`} value={query} onChange={(event) => setQuery(event.target.value)} />
        {query.length > 0 && (
          <button type="button" className="icon-btn" aria-label="Clear search" onClick={() => setQuery('')}>
            ✕
          </button>
        )}
      </div>
      <div className="set-summary">
        <span className="chip">{total} settings</span>
        <span className="chip accent">● {modified} modified</span>
        {truncated && <span className="chip warn">list cut</span>}
        <button type="button" className="icon-btn" disabled={!online || loading} onClick={() => void load()} aria-label="Reload">
          {loading ? <span className="busy" /> : '↻'}
        </button>
      </div>
      {groups === null && <Empty title={loading ? 'Loading omp settings…' : 'Not loaded'}>{online ? null : 'Connect to the desktop to read them.'}</Empty>}
      {groups !== null && shown.length === 0 && <Empty title="No setting matches">Try another word, or a key such as “model”.</Empty>}
      {shown.map((group) => (
        <Card key={group.id} title={group.label} aside={<span className="count">{group.entries.length}</span>}>
          {group.entries.map((setting) => (
            <SettingRow
              key={setting.key}
              setting={setting}
              editable={manage && !pending.has(setting.key)}
              pending={pending.has(setting.key)}
              error={errors[setting.key] ?? null}
              onSet={(value) => void set(setting, value)}
              onReset={() => (setting.secret ? setClear(setting) : void reset(setting))}
              onEdit={(mode) => setEdit({ setting, mode })}
            />
          ))}
        </Card>
      ))}
      {edit !== null && edit.mode === 'json' && (
        <JsonSheet
          setting={edit.setting}
          onClose={() => setEdit(null)}
          onSave={async (value) => {
            if (await set(edit.setting, value)) setEdit(null);
          }}
        />
      )}
      {edit !== null && edit.mode === 'secret' && (
        <SecretSheet
          setting={edit.setting}
          onClose={() => setEdit(null)}
          onSave={async (value) => {
            if (await set(edit.setting, value)) setEdit(null);
          }}
        />
      )}
      {clear !== null && (
        <ConfirmSheet
          title="Clear this secret?"
          message="omp forgets the stored value. Anything that uses it stops working until it is set again."
          facts={[
            ['Setting', settingLabel(clear.key)],
            ['Key', <span className="mono">{clear.key}</span>],
          ]}
          confirm="Clear"
          onConfirm={() => {
            const setting = clear;
            setClear(null);
            void reset(setting);
          }}
          onCancel={() => setClear(null)}
        />
      )}
    </>
  );
}

function SettingRow({
  setting,
  editable,
  pending,
  error,
  onSet,
  onReset,
  onEdit,
}: {
  setting: api.SettingView;
  editable: boolean;
  pending: boolean;
  error: string | null;
  onSet: (value: api.JsonValue) => void;
  onReset: () => void;
  onEdit: (mode: 'json' | 'secret') => void;
}): ReactNode {
  const label = settingLabel(setting.key);
  const resettable = setting.secret ? setting.set : setting.modified;
  return (
    <div className={`omp-row${setting.modified ? ' modified' : ''}${error !== null ? ' failed' : ''}`}>
      <div className="omp-head">
        <div className="grow">
          <b>
            {setting.modified && <span className="mod-dot" title="Modified" aria-label="Modified" />}
            {label}
          </b>
          <span className="mono omp-key">{setting.key}</span>
        </div>
        {pending && <span className="busy" aria-label="Saving" />}
        {resettable && (
          <button type="button" className="icon-btn small-btn" disabled={!editable} onClick={onReset} aria-label={setting.secret ? `Clear ${label}` : `Reset ${label} to default`}>
            {setting.secret ? '⌫' : '↺'}
          </button>
        )}
      </div>
      {setting.description.length > 0 && <p className="omp-desc">{setting.description}</p>}
      <div className="omp-editor">
        <Editor setting={setting} editable={editable} onSet={onSet} onEdit={onEdit} />
      </div>
      {setting.modified && !setting.secret && (
        <div className="omp-default">
          <span>default</span> <span className="mono">{preview(setting.default, 40)}</span>
        </div>
      )}
      {error !== null && (
        <p className="field-error" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}

function Editor({
  setting,
  editable,
  onSet,
  onEdit,
}: {
  setting: api.SettingView;
  editable: boolean;
  onSet: (value: api.JsonValue) => void;
  onEdit: (mode: 'json' | 'secret') => void;
}): ReactNode {
  if (setting.secret) {
    return (
      <div className="line">
        <span className={`chip ${setting.set ? 'ok' : ''}`}>{setting.set ? '🔒 •••••• set' : 'not set'}</span>
        <span className="grow" />
        <button type="button" className="button" disabled={!editable} onClick={() => onEdit('secret')}>
          {setting.set ? 'Replace…' : 'Set…'}
        </button>
      </div>
    );
  }
  switch (setting.kind.type) {
    case 'bool':
      return (
        <div className="line">
          <span className="chip">{setting.value === true ? 'on' : 'off'}</span>
          <span className="grow" />
          <Switch on={setting.value === true} label={settingLabel(setting.key)} disabled={!editable} onChange={(on) => onSet(on)} />
        </div>
      );
    case 'enum': {
      const current = typeof setting.value === 'string' ? setting.value : '';
      return (
        <select
          className="input"
          value={current}
          disabled={!editable}
          aria-label={settingLabel(setting.key)}
          onChange={(event) => {
            if (event.target.value !== current) onSet(event.target.value);
          }}
        >
          {current === '' && <option value="">—</option>}
          {setting.kind.options.map((option) => (
            <option key={option} value={option}>
              {option}
            </option>
          ))}
        </select>
      );
    }
    case 'number':
      return <TextEditor setting={setting} editable={editable} number onSet={onSet} />;
    case 'string':
      return <TextEditor setting={setting} editable={editable} number={false} onSet={onSet} />;
    case 'array':
    case 'record':
      return (
        <div className="line">
          <code className="json-preview">{preview(setting.value, 60)}</code>
          <button type="button" className="button" disabled={!editable} onClick={() => onEdit('json')}>
            ✎ Edit
          </button>
        </div>
      );
  }
}

/** A number or text field: saved on Enter or when it loses focus with a changed, valid value. */
function TextEditor({
  setting,
  editable,
  number,
  onSet,
}: {
  setting: api.SettingView;
  editable: boolean;
  number: boolean;
  onSet: (value: api.JsonValue) => void;
}): ReactNode {
  const stored = valueText(setting.value);
  const [text, setText] = useState<string | null>(null);
  const shown = text ?? stored;
  const parsed: Parsed = number ? parseNumber(shown) : { ok: true, value: shown };
  const commit = () => {
    if (text === null || text === stored) {
      setText(null);
      return;
    }
    if (!parsed.ok) return;
    onSet(parsed.value);
    setText(null);
  };
  return (
    <>
      <input
        className={`input${text !== null && !parsed.ok ? ' invalid' : ''}`}
        value={shown}
        inputMode={number ? 'decimal' : undefined}
        placeholder={valueText(setting.default) || (number ? '0' : '')}
        disabled={!editable}
        aria-label={settingLabel(setting.key)}
        onChange={(event) => setText(event.target.value)}
        onBlur={commit}
        onKeyDown={(event) => {
          if (event.key === 'Enter') commit();
          if (event.key === 'Escape') setText(null);
        }}
      />
      {text !== null && !parsed.ok && <p className="field-error">{parsed.error}</p>}
    </>
  );
}

function JsonSheet({
  setting,
  onClose,
  onSave,
}: {
  setting: api.SettingView;
  onClose: () => void;
  onSave: (value: api.JsonValue) => Promise<void>;
}): ReactNode {
  const [text, setText] = useState(() => prettyJson(setting.kind, setting.value));
  const [busy, setBusy] = useState(false);
  const parsed = parseJsonFor(setting.kind, text);
  return (
    <Sheet title={settingLabel(setting.key)} icon="{ }" onClose={onClose}>
      <p>
        <span className="mono">{setting.key}</span> · a JSON {setting.kind.type === 'array' ? 'array' : 'object'}
      </p>
      <textarea
        className={`input json-edit${parsed.ok ? '' : ' invalid'}`}
        value={text}
        rows={10}
        spellCheck={false}
        autoCapitalize="off"
        autoCorrect="off"
        onChange={(event) => setText(event.target.value)}
      />
      {!parsed.ok ? <p className="field-error">{parsed.error}</p> : <p className="field-ok">Valid JSON</p>}
      <div className="buttons pair">
        <button type="button" className="button" onClick={onClose}>
          Cancel
        </button>
        <button
          type="button"
          className="button primary"
          disabled={!parsed.ok || busy}
          onClick={() => {
            if (!parsed.ok) return;
            setBusy(true);
            void onSave(parsed.value).finally(() => setBusy(false));
          }}
        >
          {busy ? 'Saving…' : 'Save'}
        </button>
      </div>
    </Sheet>
  );
}

/** "Set secret": the typed text lives only in this sheet and is never shown back. */
function SecretSheet({
  setting,
  onClose,
  onSave,
}: {
  setting: api.SettingView;
  onClose: () => void;
  onSave: (value: api.JsonValue) => Promise<void>;
}): ReactNode {
  const record = setting.kind.type === 'record';
  const [text, setText] = useState('');
  const [busy, setBusy] = useState(false);
  const parsed: Parsed = record ? parseJsonFor(setting.kind, text) : { ok: true, value: text };
  const empty = text.length === 0;
  return (
    <Sheet title={`Set ${settingLabel(setting.key)}`} icon="🔒" onClose={onClose}>
      <p>
        Sent encrypted to the desktop and stored by omp. It is never shown again, here or there. <span className="mono">{setting.key}</span>
      </p>
      {record ? (
        <textarea
          className={`input json-edit${!empty && !parsed.ok ? ' invalid' : ''}`}
          value={text}
          rows={6}
          spellCheck={false}
          autoCapitalize="off"
          autoCorrect="off"
          autoComplete="off"
          placeholder="{ … }"
          onChange={(event) => setText(event.target.value)}
        />
      ) : (
        <input
          className="input"
          type="password"
          value={text}
          autoComplete="off"
          autoCapitalize="off"
          spellCheck={false}
          placeholder="Paste the secret"
          onChange={(event) => setText(event.target.value)}
        />
      )}
      {/* A parse error never echoes the text. */}
      {record && !empty && !parsed.ok && <p className="field-error">Not a JSON object.</p>}
      <div className="buttons pair">
        <button type="button" className="button" onClick={onClose}>
          Cancel
        </button>
        <button
          type="button"
          className="button primary"
          disabled={empty || !parsed.ok || busy}
          onClick={() => {
            if (!parsed.ok) return;
            setBusy(true);
            const value = parsed.value;
            setText('');
            void onSave(value).finally(() => setBusy(false));
          }}
        >
          {busy ? 'Saving…' : 'Save secret'}
        </button>
      </div>
    </Sheet>
  );
}
