// Active-destination config backup: desktop setup, back
// up now, the stored versions by host with multi-select delete, and restore: prepare → plan
// preview → apply (with the overwrite warning) or discard.

import { type ReactNode, useCallback, useEffect, useRef, useState } from 'react';
import type { api } from '../../../remote/index.ts';
import { age, perform, succeeded } from '../../ui/format.ts';
import { useClient, useFeed, useNow, useOnline } from '../../ui/hooks.ts';
import { Card, ConfirmSheet, Empty, KeyValues, NeedsS4, Sheet } from '../../ui/kit.tsx';
import { ReadOnlyNote, SectionScreen, ask, useControl, useManage } from './common.tsx';
import { INTERVAL_LABEL, type HostVersions, bytes, sortBackupHosts, totalSize, when, backupDestinationLabel } from './logic.ts';

type BackupPlan = Extract<api.RestorePlanView, { type: 'backup' }>;

export function BackupSection(): ReactNode {
  const backup = useFeed({ feed: 'backup' }, 'backup');
  // A destination switch is a new listing scope, including selection and restore handles.
  const destinationKey = JSON.stringify([backup?.destination.kind, backup?.destination.label]);
  return (
    <SectionScreen eyebrow="Backup" title="Settings backup">
      <NeedsS4 what="Settings backup">
        <BackupBody key={destinationKey} backup={backup} />
      </NeedsS4>
    </SectionScreen>
  );
}

function BackupBody({ backup }: { backup: Extract<api.FeedValue, { feed: 'backup' }> | undefined }): ReactNode {
  const client = useClient();
  const now = useNow();
  const online = useOnline();
  const control = useControl();
  const manage = useManage();
  const active = useRef(true);
  useEffect(() => {
    active.current = true;
    return () => { active.current = false; };
  }, []);
  const [hosts, setHosts] = useState<HostVersions<api.BackupFileView>[] | null>(null);
  const [truncated, setTruncated] = useState(false);
  const [loading, setLoading] = useState(false);
  const [selected, setSelected] = useState<ReadonlySet<string>>(new Set());
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [plan, setPlan] = useState<{ handle: number; plan: BackupPlan; truncated: boolean } | null>(null);

  const connected = backup?.connected === true;
  const destination = backupDestinationLabel(backup?.destination);
  const load = useCallback(async () => {
    setLoading(true);
    const reply = await ask(client, { op: 'backup_list' }, 'Backup versions');
    if (!active.current) return;
    setLoading(false);
    if (reply?.type === 'backup_list') {
      setHosts(sortBackupHosts(reply.hosts));
      setTruncated(reply.truncated);
    }
  }, [client]);
  useEffect(() => {
    if (online && connected && hosts === null) void load();
  }, [online, connected, hosts, load]);
  // A finished backup adds a version.
  const lastBackup = backup?.last_backup_unix_ms ?? null;
  useEffect(() => {
    if (online && connected && lastBackup !== null) void load();
  }, [lastBackup]);

  const all = hosts?.flatMap((host) => host.versions) ?? [];
  const chosen = all.filter((version) => selected.has(version.id));
  const toggle = (id: string) => {
    const next = new Set(selected);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setSelected(next);
  };

  const step = async (key: string, op: api.Op, label: string, success?: string) => {
    setBusy(key);
    const view = await perform(client, op, label, success);
    setBusy(null);
    return view;
  };
  const remove = async () => {
    const view = await step('delete', { op: 'backup_delete', ids: chosen.map((v) => v.id) }, `Delete ${chosen.length} backup${chosen.length === 1 ? '' : 's'}`, 'Deleted.');
    setConfirmDelete(false);
    if (succeeded(view)) setSelected(new Set());
    await load();
  };
  const prepare = async (version: api.BackupFileView) => {
    const view = await step(`restore:${version.id}`, { op: 'backup_restore_prepare', id: version.id }, `Prepare restore of ${version.name}`);
    const result = view?.outcome?.state === 'done' ? view.outcome.result : null;
    if (result?.type === 'restore_plan' && result.plan.type === 'backup') setPlan({ handle: result.handle, plan: result.plan, truncated: result.truncated });
  };

  if (backup !== undefined && !backup.available) {
    return <Empty title="Backup destination is not available">Set up a supported backup destination in the desktop app’s Settings → Backup.</Empty>;
  }

  return (
    <>
      {online && !manage && <ReadOnlyNote need="manage" />}
      <Card title="Destination" aside={<span className={`chip ${connected ? 'ok' : 'warn'}`}>{connected ? 'connected' : 'not configured'}</span>}>
        <KeyValues
          rows={[
            ['Destination', destination],
            backup?.account != null ? ['Account', backup.account] : null,
            ['Last backup', backup?.last_backup_unix_ms != null ? `${age(backup.last_backup_unix_ms, now)} ago` : 'never'],
            ['Schedule', <span className="chip">{backup === undefined ? '—' : INTERVAL_LABEL[backup.interval]}</span>],
            backup?.busy != null ? ['Working', <span className="chip accent"><span className="busy" /> {backup.busy}</span>] : null,
          ]}
        />
        {backup?.error != null && <p className="field-error">{backup.error}</p>}
        {connected ? (
          <button
            type="button"
            className="button primary block spaced"
            disabled={!control || busy !== null || backup?.busy != null}
            onClick={() => void step('backup', { op: 'backup_run' }, `Back up to ${destination}`, `Backed up to ${destination}.`)}
          >
            {busy === 'backup' || backup?.busy != null ? 'Backing up…' : `⤒ Back up to ${destination}`}
          </button>
        ) : (
          <>
            <p className="muted small note">
              {backup?.destination.kind === 'drive'
                ? 'Sign in to Google Drive from the desktop app; the sign-in opens in the desktop’s browser.'
                : `Set up the server in the desktop app’s Settings → Backup (${destination}).`}
            </p>
            {backup?.destination.kind === 'drive' && (
              <button
                type="button"
                className="button primary block spaced"
                disabled={!manage || busy !== null || backup.available === false}
                onClick={() => void step('signin', { op: 'backup_connect' }, 'Sign in on the desktop', 'Continue in the desktop’s browser.')}
              >
                {busy === 'signin' ? 'Opening…' : '🖥 Sign in on the desktop'}
              </button>
            )}
          </>
        )}
      </Card>

      {connected && (
        <Card
          title="Versions"
          aside={
            <button type="button" className="icon-btn" disabled={!online || loading} onClick={() => void load()} aria-label="Reload versions">
              {loading ? <span className="busy" /> : '↻'}
            </button>
          }
        >
          {hosts === null && <Empty title={loading ? 'Loading versions…' : 'Not loaded'} />}
          {hosts !== null && all.length === 0 && <Empty title="No backups at this destination yet">Back up now to store the first version.</Empty>}
          {truncated && <p className="muted small note">Only the newest versions are listed.</p>}
          {hosts?.map((host) => (
            <div key={host.host} className="host-group">
              <div className="host-head">
                <span className="chip accent">🖥 {host.host}</span>
                <span className="muted small">
                  {host.versions.length} version{host.versions.length === 1 ? '' : 's'} · {bytes(host.versions.reduce((s, v) => s + v.size, 0))}
                </span>
              </div>
              {host.versions.map((version, index) => (
                <div key={version.id} className={`version${selected.has(version.id) ? ' selected' : ''}`}>
                  <button type="button" className="check" aria-pressed={selected.has(version.id)} aria-label={`Select ${version.name}`} disabled={!manage} onClick={() => toggle(version.id)}>
                    {selected.has(version.id) ? '✓' : ''}
                  </button>
                  <div className="grow">
                    <b>
                      {when(version.created_unix_ms)}
                      {index === 0 && <span className="chip ok">newest</span>}
                    </b>
                    <small className="mono">
                      {version.name} · {bytes(version.size)}
                    </small>
                  </div>
                  <button type="button" className="button" disabled={!manage || busy !== null} onClick={() => void prepare(version)}>
                    {busy === `restore:${version.id}` ? <span className="busy" /> : 'Restore…'}
                  </button>
                </div>
              ))}
            </div>
          ))}
          {chosen.length > 0 && (
            <div className="select-bar">
              <span className="grow">
                {chosen.length} selected · {bytes(totalSize(all, selected))}
              </span>
              <button type="button" className="button ghost" onClick={() => setSelected(new Set())}>
                Clear
              </button>
              <button type="button" className="button danger" disabled={!manage || busy !== null} onClick={() => setConfirmDelete(true)}>
                Delete {chosen.length}
              </button>
            </div>
          )}
        </Card>
      )}

      {confirmDelete && (
        <ConfirmSheet
          title={`Delete ${chosen.length} backup${chosen.length === 1 ? '' : 's'} permanently?`}
          message="They are removed from the active destination for good; they cannot be restored afterwards."
          facts={[
            ['Versions', `${chosen.length}`],
            ['Size', bytes(totalSize(all, selected))],
            ...chosen.slice(0, 6).map((v) => [when(v.created_unix_ms), <span className="mono">{v.name}</span>] as const),
            ...(chosen.length > 6 ? [['…', `${chosen.length - 6} more`] as const] : []),
          ]}
          confirm="Delete permanently"
          busy={busy === 'delete'}
          onConfirm={() => void remove()}
          onCancel={() => setConfirmDelete(false)}
        />
      )}
      {plan !== null && <BackupPlanSheet prepared={plan} onDone={() => setPlan(null)} />}
    </>
  );
}

/** The prepared restore: what is replaced, added, made to match, removed, and where the safety copy goes. */
function BackupPlanSheet({ prepared, onDone }: { prepared: { handle: number; plan: BackupPlan; truncated: boolean }; onDone: () => void }): ReactNode {
  const client = useClient();
  const manage = useManage();
  const [confirm, setConfirm] = useState(false);
  const [busy, setBusy] = useState(false);
  const { plan, handle } = prepared;

  const discard = async () => {
    setBusy(true);
    await perform(client, { op: 'backup_restore_discard', handle }, 'Discard restore');
    setBusy(false);
    onDone();
  };
  const apply = async () => {
    setBusy(true);
    const view = await perform(client, { op: 'backup_restore_apply', handle }, 'Restore settings backup', 'Restored. Restart omp sessions to pick up the settings.');
    setBusy(false);
    setConfirm(false);
    if (succeeded(view)) onDone();
  };

  if (confirm) {
    return (
      <ConfirmSheet
        title="Restore and overwrite?"
        message={`${plan.replaced.length} local file${plan.replaced.length === 1 ? ' is' : 's are'} overwritten and ${plan.removed.length} removed. A safety copy of the current files is kept first.`}
        facts={[
          ['Backup', `${plan.host} · ${when(plan.created_unix_ms)}`],
          ['Overwritten', `${plan.replaced.length} files`],
          ['Removed', `${plan.removed.length} files`],
          ['Added', `${plan.added.length} files`],
          ['Safety copy', <span className="mono">{plan.safety_display}</span>],
        ]}
        confirm="Restore"
        busy={busy}
        onConfirm={() => void apply()}
        onCancel={() => setConfirm(false)}
      />
    );
  }
  return (
    <Sheet title="Restore plan" icon="⤓" onClose={() => void discard()}>
      <p>Nothing changes until you apply. Closing discards the plan.</p>
      <KeyValues
        rows={[
          ['Backup', <span className="mono">{plan.name}</span>],
          ['From', <span className="chip accent">🖥 {plan.host}</span>],
          ['Taken', when(plan.created_unix_ms)],
        ]}
      />
      <div className="plan">
        <PlanList title="Overwritten" tone="warn" items={plan.replaced} empty="nothing" />
        <PlanList title="Added" tone="ok" items={plan.added} empty="nothing" />
        <PlanList title="Made to match the backup" tone="accent" items={plan.dirs} empty="no folders" />
        <PlanList title="Removed" tone="danger" items={plan.removed} empty="nothing" />
      </div>
      {prepared.truncated && <p className="muted small note">The lists are cut; the restore applies to every file.</p>}
      <div className="safety">
        <span className="chip ok">🛟 safety copy</span>
        <span className="mono">{plan.safety_display}</span>
      </div>
      {(plan.replaced.length > 0 || plan.removed.length > 0) && (
        <div className="banner danger" role="note">
          <span className="grow">
            Applying overwrites {plan.replaced.length} and removes {plan.removed.length} of omp’s files on the desktop.
          </span>
        </div>
      )}
      <div className="buttons pair">
        <button type="button" className="button" disabled={busy} onClick={() => void discard()}>
          Discard
        </button>
        <button type="button" className="button danger" disabled={!manage || busy} onClick={() => setConfirm(true)}>
          Apply…
        </button>
      </div>
    </Sheet>
  );
}

export function PlanList({
  title,
  tone,
  items,
  empty,
}: {
  title: string;
  tone: 'ok' | 'warn' | 'danger' | 'accent';
  items: readonly string[];
  empty: string;
}): ReactNode {
  const [open, setOpen] = useState(false);
  const shown = open ? items : items.slice(0, 4);
  return (
    <div className={`plan-list ${tone}`}>
      <div className="plan-head">
        <span className={`chip ${tone}`}>{items.length}</span>
        <b>{title}</b>
      </div>
      {items.length === 0 ? (
        <small className="muted">{empty}</small>
      ) : (
        <ul>
          {shown.map((item) => (
            <li key={item} className="mono">
              {item}
            </li>
          ))}
        </ul>
      )}
      {items.length > 4 && (
        <button type="button" className="link-btn" onClick={() => setOpen(!open)}>
          {open ? 'Show fewer' : `Show all ${items.length}`}
        </button>
      )}
    </div>
  );
}
