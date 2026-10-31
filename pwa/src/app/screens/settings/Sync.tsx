// Project sync: the synced projects (parts with sizes, session opt-out, back up now, remove), adding
// a project through the desktop-folder picker, the stored backups by project → host → version
// with delete, and the restore wizard (target, parts, plan preview with the overwrite warning,
// a keep/overwrite/both choice per conflicting session) → apply.

import { type ReactNode, useCallback, useEffect, useState } from 'react';
import type { api } from '../../../remote/index.ts';
import { FolderPicker } from '../fleet/FolderBrowser.tsx';
import { age, perform, succeeded } from '../../ui/format.ts';
import { useClient, useFeed, useNow, useOnline } from '../../ui/hooks.ts';
import { Card, ConfirmSheet, Empty, KeyValues, NeedsS4, Sheet } from '../../ui/kit.tsx';
import { PlanList } from './Drive.tsx';
import { ReadOnlyNote, SectionScreen, ToggleRow, ask, useControl, useManage } from './common.tsx';
import {
  INTERVAL_LABEL,
  type ProjectBackups,
  backupParts,
  bytes,
  conflictList,
  defaultChoices,
  groupSyncBackups,
  isConflict,
  latestFor,
  summarizeProjectPlan,
  toggleExcluded,
  totalSize,
  when,
} from './logic.ts';

type ProjectPlan = Extract<api.RestorePlanView, { type: 'project' }>;

export function SyncSection(): ReactNode {
  return (
    <SectionScreen eyebrow="Backup" title="Project sync">
      <NeedsS4 what="Project sync">
        <SyncBody />
      </NeedsS4>
    </SectionScreen>
  );
}

function SyncBody(): ReactNode {
  const client = useClient();
  const online = useOnline();
  const manage = useManage();
  const sync = useFeed({ feed: 'sync' }, 'sync');
  const [backups, setBackups] = useState<api.SyncBackupView[] | null>(null);
  const [truncated, setTruncated] = useState(false);
  const [loading, setLoading] = useState(false);
  const [adding, setAdding] = useState(false);
  const [addBusy, setAddBusy] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    const reply = await ask(client, { op: 'sync_backups' }, 'Project backups');
    setLoading(false);
    if (reply?.type === 'sync_backups') {
      setBackups(reply.backups);
      setTruncated(reply.truncated);
    }
  }, [client]);
  useEffect(() => {
    if (online && backups === null) void load();
  }, [online, backups, load]);
  // A finished backup changes a row's last backup time: list the new version.
  const stamp = sync?.rows.map((row) => row.last_backup_unix_ms ?? 0).join(',') ?? '';
  useEffect(() => {
    if (online && backups !== null) void load();
  }, [stamp]);

  const add = async (dir: string, display: string) => {
    setAddBusy(true);
    const view = await perform(client, { op: 'sync_add', dir }, `Sync ${display}`, `${display} is synced now.`);
    setAddBusy(false);
    if (succeeded(view)) setAdding(false);
  };

  return (
    <>
      {online && !manage && <ReadOnlyNote need="manage" />}
      <div className="set-summary">
        <span className="chip">{sync?.rows.length ?? 0} projects</span>
        <span className="chip accent">⏱ {sync === undefined ? '—' : INTERVAL_LABEL[sync.interval]}</span>
        {sync?.busy != null && (
          <span className="chip warn">
            <span className="busy" /> {sync.busy}
          </span>
        )}
      </div>
      {sync?.truncated === true && <p className="muted small note">Only the first projects are listed.</p>}
      {sync !== undefined && sync.rows.length === 0 && <Empty title="No synced projects">Add a project folder to back it up.</Empty>}
      {sync?.rows.map((row) => (
        <SyncRowCard key={row.project} row={row} latest={latestFor(backups ?? [], row.display)} busyText={sync.busy} />
      ))}
      <button type="button" className="button block add-project" disabled={!manage} onClick={() => setAdding(true)}>
        ＋ Add a project
      </button>
      <BackupsCard backups={backups} truncated={truncated} loading={loading} onReload={() => void load()} />
      {adding && <FolderPicker title="Sync a project" confirm="Sync this folder" busy={addBusy} onPick={(dir, display) => void add(dir, display)} onClose={() => setAdding(false)} />}
    </>
  );
}

// ---- synced projects ----------------------------------------------------------------------------

function SyncRowCard({ row, latest, busyText }: { row: api.SyncRowView; latest: api.SyncBackupView | null; busyText: string | null }): ReactNode {
  const client = useClient();
  const now = useNow();
  const control = useControl();
  const manage = useManage();
  const [busy, setBusy] = useState<string | null>(null);
  const [remove, setRemove] = useState(false);
  const [choose, setChoose] = useState(false);

  const save = async (parts: api.SyncParts, excluded: string[]) => {
    setBusy('set');
    await perform(client, { op: 'sync_set', project: row.project, parts, excluded }, `Sync settings of ${row.display}`);
    setBusy(null);
  };
  const part = (key: keyof api.SyncParts, title: string, size: string | null) => (
    <ToggleRow
      title={title}
      hint={size ?? 'no backup yet'}
      on={row.parts[key]}
      disabled={!manage || busy !== null}
      onChange={(on) => void save({ ...row.parts, [key]: on }, row.excluded)}
    />
  );

  return (
    <section className="card sync-row">
      <header>
        <span className="title">{row.display.split('/').at(-1) || row.display}</span>
        <span className="chip">{row.last_backup_unix_ms === null ? 'never backed up' : `${age(row.last_backup_unix_ms, now)} ago`}</span>
      </header>
      <div className="card-body">
        <p className="mono muted path">{row.display}</p>
        {part('omp', '.omp folder', latest?.omp_bytes != null ? bytes(latest.omp_bytes) : null)}
        {part(
          'sessions',
          'Sessions',
          latest?.session_bytes != null ? `${latest.sessions ?? 0} sessions · ${bytes(latest.session_bytes)}` : null,
        )}
        {part('memory', 'Memory', latest?.memory_bytes != null ? bytes(latest.memory_bytes) : null)}
        <button type="button" className="button block nav-link" disabled={!row.parts.sessions} onClick={() => setChoose(true)}>
          <span className="grow">Sessions backed up</span>
          <span className={`chip ${row.excluded.length > 0 ? 'warn' : 'ok'}`}>{row.excluded.length === 0 ? 'all' : `${row.excluded.length} excluded`}</span>
          <span aria-hidden="true">›</span>
        </button>
        <KeyValues rows={[['Last backup', when(row.last_backup_unix_ms)], latest !== null ? ['Size', bytes(latest.size)] : null]} />
        <div className="buttons pair spaced">
          <button type="button" className="button danger-outline" disabled={!manage || busy !== null} onClick={() => setRemove(true)}>
            Remove
          </button>
          <button
            type="button"
            className="button primary"
            disabled={!control || busy !== null || busyText !== null}
            onClick={() => {
              setBusy('backup');
              void perform(client, { op: 'sync_backup', project: row.project }, `Back up ${row.display}`, 'Backed up.').finally(() => setBusy(null));
            }}
          >
            {busy === 'backup' ? 'Backing up…' : '⤒ Back up now'}
          </button>
        </div>
      </div>
      {remove && (
        <ConfirmSheet
          title="Stop syncing this project?"
          message="The project is no longer backed up. Its stored backups stay and can still be restored or deleted below."
          facts={[
            ['Project', <span className="mono">{row.display}</span>],
            ['Parts', [row.parts.omp && '.omp', row.parts.sessions && 'sessions', row.parts.memory && 'memory'].filter(Boolean).join(', ') || 'none'],
          ]}
          confirm="Stop syncing"
          busy={busy === 'remove'}
          onConfirm={() => {
            setBusy('remove');
            void perform(client, { op: 'sync_remove', project: row.project }, `Stop syncing ${row.display}`).finally(() => {
              setBusy(null);
              setRemove(false);
            });
          }}
          onCancel={() => setRemove(false)}
        />
      )}
      {choose && (
        <ExcludeSheet
          row={row}
          disabled={!manage || busy !== null}
          onSave={async (excluded) => {
            await save(row.parts, excluded);
            setChoose(false);
          }}
          onClose={() => setChoose(false)}
        />
      )}
    </section>
  );
}

/** The session opt-out: every session is backed up unless switched off here. */
function ExcludeSheet({
  row,
  disabled,
  onSave,
  onClose,
}: {
  row: api.SyncRowView;
  disabled: boolean;
  onSave: (excluded: string[]) => Promise<void>;
  onClose: () => void;
}): ReactNode {
  const now = useNow();
  const feed = useFeed({ feed: 'sessions', project: row.project, limit: 200 }, 'sessions');
  const [excluded, setExcluded] = useState<string[]>(row.excluded);
  const [busy, setBusy] = useState(false);
  const known = new Set(feed?.sessions.map((s) => s.id) ?? []);
  const unknown = excluded.filter((id) => !known.has(id));
  return (
    <Sheet title="Sessions to back up" icon="☰" onClose={onClose}>
      <p>New sessions are included automatically. Switch off the ones to leave out of {row.display}’s backups.</p>
      {feed === undefined && <Empty title="Loading sessions…" />}
      {feed?.sessions.length === 0 && <Empty title="No sessions in this project" />}
      <div className="exclude-list">
        {feed?.sessions.map((session) => (
          <ToggleRow
            key={session.id}
            title={session.title ?? session.first_prompt ?? session.id}
            hint={`${age(session.modified_unix_ms, now)} ago`}
            on={!excluded.includes(session.id)}
            disabled={disabled || busy}
            onChange={() => setExcluded(toggleExcluded(excluded, session.id))}
          />
        ))}
        {unknown.length > 0 && <p className="muted small note">{unknown.length} excluded session(s) are not listed here and stay excluded.</p>}
        {feed?.truncated === true && <p className="muted small note">Showing the newest {feed.sessions.length} of {feed.total}.</p>}
      </div>
      <div className="buttons pair spaced">
        <button type="button" className="button" onClick={onClose}>
          Cancel
        </button>
        <button
          type="button"
          className="button primary"
          disabled={disabled || busy}
          onClick={() => {
            setBusy(true);
            void onSave(excluded).finally(() => setBusy(false));
          }}
        >
          {busy ? 'Saving…' : `Save · ${excluded.length} excluded`}
        </button>
      </div>
    </Sheet>
  );
}

// ---- stored backups -----------------------------------------------------------------------------

function BackupsCard({
  backups,
  truncated,
  loading,
  onReload,
}: {
  backups: api.SyncBackupView[] | null;
  truncated: boolean;
  loading: boolean;
  onReload: () => void;
}): ReactNode {
  const client = useClient();
  const online = useOnline();
  const manage = useManage();
  const [selected, setSelected] = useState<ReadonlySet<string>>(new Set());
  const [confirm, setConfirm] = useState(false);
  const [busy, setBusy] = useState(false);
  const [restore, setRestore] = useState<api.SyncBackupView | null>(null);
  const grouped: ProjectBackups[] = backups === null ? [] : groupSyncBackups(backups);
  const all = backups ?? [];
  const chosen = all.filter((b) => selected.has(b.id));
  const toggle = (id: string) => {
    const next = new Set(selected);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setSelected(next);
  };
  const remove = async () => {
    setBusy(true);
    const view = await perform(client, { op: 'sync_delete', ids: chosen.map((b) => b.id) }, `Delete ${chosen.length} project backup${chosen.length === 1 ? '' : 's'}`, 'Deleted.');
    setBusy(false);
    setConfirm(false);
    if (succeeded(view)) setSelected(new Set());
    onReload();
  };

  return (
    <Card
      title="Backups"
      aside={
        <button type="button" className="icon-btn" disabled={!online || loading} onClick={onReload} aria-label="Reload backups">
          {loading ? <span className="busy" /> : '↻'}
        </button>
      }
    >
      {backups === null && <Empty title={loading ? 'Loading backups…' : 'Not loaded'} />}
      {backups !== null && backups.length === 0 && <Empty title="No project backups yet" />}
      {truncated && <p className="muted small note">Only the newest backups are listed.</p>}
      {grouped.map((project) => (
        <details key={project.display} className="backup-project" open={grouped.length <= 2}>
          <summary>
            <span className="grow mono">{project.display}</span>
            <span className="chip">
              {project.count} · {bytes(project.size)}
            </span>
          </summary>
          {project.hosts.map((host) => (
            <div key={host.host} className="host-group">
              <div className="host-head">
                <span className="chip accent">🖥 {host.host}</span>
              </div>
              {host.versions.map((version) => (
                <div key={version.id} className={`version${selected.has(version.id) ? ' selected' : ''}`}>
                  <button type="button" className="check" aria-pressed={selected.has(version.id)} aria-label={`Select ${version.name}`} disabled={!manage} onClick={() => toggle(version.id)}>
                    {selected.has(version.id) ? '✓' : ''}
                  </button>
                  <div className="grow">
                    <b>{when(version.created_unix_ms)}</b>
                    <small className="parts-line">
                      {version.omp_bytes !== null && <span className="chip">.omp {bytes(version.omp_bytes)}</span>}
                      {version.session_bytes !== null && (
                        <span className="chip">
                          {version.sessions ?? 0} sessions {bytes(version.session_bytes)}
                        </span>
                      )}
                      {version.memory_bytes !== null && <span className="chip">memory {bytes(version.memory_bytes)}</span>}
                    </small>
                  </div>
                  <button type="button" className="button" disabled={!manage} onClick={() => setRestore(version)}>
                    Restore…
                  </button>
                </div>
              ))}
            </div>
          ))}
        </details>
      ))}
      {chosen.length > 0 && (
        <div className="select-bar">
          <span className="grow">
            {chosen.length} selected · {bytes(totalSize(all, selected))}
          </span>
          <button type="button" className="button ghost" onClick={() => setSelected(new Set())}>
            Clear
          </button>
          <button type="button" className="button danger" disabled={!manage || busy} onClick={() => setConfirm(true)}>
            Delete {chosen.length}
          </button>
        </div>
      )}
      {confirm && (
        <ConfirmSheet
          title={`Delete ${chosen.length} backup${chosen.length === 1 ? '' : 's'} permanently?`}
          message="The selected versions are deleted from backup storage for good."
          facts={[
            ['Versions', `${chosen.length}`],
            ['Size', bytes(totalSize(all, selected))],
            ...chosen.slice(0, 6).map((b) => [`${b.display} · ${when(b.created_unix_ms)}`, b.host] as const),
            ...(chosen.length > 6 ? [['…', `${chosen.length - 6} more`] as const] : []),
          ]}
          confirm="Delete permanently"
          busy={busy}
          onConfirm={() => void remove()}
          onCancel={() => setConfirm(false)}
        />
      )}
      {restore !== null && <RestoreWizard backup={restore} onClose={() => setRestore(null)} />}
    </Card>
  );
}

// ---- restore wizard -----------------------------------------------------------------------------

type Target = { type: 'project'; project: string; display: string } | { type: 'dir'; dir: string; display: string };

function RestoreWizard({ backup, onClose }: { backup: api.SyncBackupView; onClose: () => void }): ReactNode {
  const client = useClient();
  const manage = useManage();
  const projects = useFeed({ feed: 'projects' }, 'projects');
  const available = backupParts(backup);
  const [parts, setParts] = useState<api.SyncParts>(available);
  const [target, setTarget] = useState<Target | null>(null);
  const [mode, setMode] = useState<'project' | 'dir'>('project');
  const [picking, setPicking] = useState(false);
  const [busy, setBusy] = useState(false);
  const [prepared, setPrepared] = useState<{ handle: number; plan: ProjectPlan; truncated: boolean } | null>(null);

  // Default target: the project the backup was taken from, when it exists here.
  useEffect(() => {
    if (target !== null || projects === undefined) return;
    const same = projects.projects.find((p) => p.display === backup.display);
    if (same !== undefined) setTarget({ type: 'project', project: same.id, display: same.display });
  }, [projects]);

  const prepare = async () => {
    if (target === null) return;
    setBusy(true);
    const wire: api.RestoreTarget = target.type === 'project' ? { type: 'project', project: target.project } : { type: 'dir', dir: target.dir };
    const view = await perform(client, { op: 'sync_restore_prepare', backup: backup.id, target: wire, parts }, `Prepare restore to ${target.display}`);
    setBusy(false);
    const result = view?.outcome?.state === 'done' ? view.outcome.result : null;
    if (result?.type === 'restore_plan' && result.plan.type === 'project') setPrepared({ handle: result.handle, plan: result.plan, truncated: result.truncated });
  };

  if (prepared !== null) return <ProjectPlanSheet backup={backup} prepared={prepared} onBack={() => setPrepared(null)} onDone={onClose} />;
  if (picking) {
    return (
      <FolderPicker
        title="Restore into a folder"
        confirm="Restore here"
        onPick={(dir, display) => {
          setTarget({ type: 'dir', dir, display });
          setPicking(false);
        }}
        onClose={() => setPicking(false)}
      />
    );
  }
  const anyPart = parts.omp || parts.sessions || parts.memory;
  return (
    <Sheet title="Restore a project backup" icon="⤓" onClose={onClose}>
      <KeyValues
        rows={[
          ['Backup', <span className="mono">{backup.display}</span>],
          ['From', <span className="chip accent">🖥 {backup.host}</span>],
          ['Taken', when(backup.created_unix_ms)],
        ]}
      />
      <div className="wizard-step">
        <div className="step-head">
          <span className="step-no">1</span>
          <b>Restore into</b>
        </div>
        <div className="segmented" role="group" aria-label="Target kind">
          <button type="button" aria-pressed={mode === 'project'} onClick={() => setMode('project')}>
            A project
          </button>
          <button type="button" aria-pressed={mode === 'dir'} onClick={() => setMode('dir')}>
            A folder
          </button>
        </div>
        {mode === 'project' ? (
          <select
            className="input spaced"
            aria-label="Project"
            value={target?.type === 'project' ? target.project : ''}
            onChange={(event) => {
              const project = projects?.projects.find((p) => p.id === event.target.value);
              if (project !== undefined) setTarget({ type: 'project', project: project.id, display: project.display });
            }}
          >
            <option value="" disabled>
              {projects === undefined ? 'Loading projects…' : 'Choose a project'}
            </option>
            {projects?.projects.map((project) => (
              <option key={project.id} value={project.id}>
                {project.display}
              </option>
            ))}
          </select>
        ) : (
          <button type="button" className="button block nav-link spaced" onClick={() => setPicking(true)}>
            <span className="grow mono">{target?.type === 'dir' ? target.display : 'Choose a desktop folder…'}</span>
            <span aria-hidden="true">›</span>
          </button>
        )}
      </div>
      <div className="wizard-step">
        <div className="step-head">
          <span className="step-no">2</span>
          <b>Parts</b>
        </div>
        <ToggleRow
          title=".omp folder"
          hint={backup.omp_bytes !== null ? bytes(backup.omp_bytes) : 'not in this backup'}
          on={parts.omp}
          disabled={!available.omp}
          onChange={(on) => setParts({ ...parts, omp: on })}
        />
        <ToggleRow
          title="Sessions"
          hint={backup.session_bytes !== null ? `${backup.sessions ?? 0} sessions · ${bytes(backup.session_bytes)}` : 'not in this backup'}
          on={parts.sessions}
          disabled={!available.sessions}
          onChange={(on) => setParts({ ...parts, sessions: on })}
        />
        <ToggleRow
          title="Memory"
          hint={backup.memory_bytes !== null ? bytes(backup.memory_bytes) : 'not in this backup'}
          on={parts.memory}
          disabled={!available.memory}
          onChange={(on) => setParts({ ...parts, memory: on })}
        />
      </div>
      <div className="buttons pair spaced">
        <button type="button" className="button" onClick={onClose}>
          Cancel
        </button>
        <button
          type="button"
          className="button primary"
          disabled={!manage || busy || target === null || (mode === 'project' ? target.type !== 'project' : target.type !== 'dir') || !anyPart}
          onClick={() => void prepare()}
        >
          {busy ? 'Preparing…' : 'Preview plan'}
        </button>
      </div>
    </Sheet>
  );
}

const CHOICES: readonly { value: api.ConflictResolution; label: string }[] = [
  { value: 'keep', label: 'Keep mine' },
  { value: 'overwrite', label: 'Overwrite' },
  { value: 'both', label: 'Keep both' },
];

function ProjectPlanSheet({
  backup,
  prepared,
  onBack,
  onDone,
}: {
  backup: api.SyncBackupView;
  prepared: { handle: number; plan: ProjectPlan; truncated: boolean };
  onBack: () => void;
  onDone: () => void;
}): ReactNode {
  const client = useClient();
  const now = useNow();
  const manage = useManage();
  const { plan, handle } = prepared;
  const [choices, setChoices] = useState(() => defaultChoices(plan));
  const [confirm, setConfirm] = useState(false);
  const [busy, setBusy] = useState(false);
  const summary = summarizeProjectPlan(plan, choices);
  const conflicts = plan.sessions.filter(isConflict);
  const added = plan.sessions.filter((s) => !isConflict(s));

  const apply = async () => {
    setBusy(true);
    const view = await perform(
      client,
      { op: 'sync_restore_apply', handle, conflicts: conflictList(plan, choices) },
      `Restore into ${plan.target_display}`,
      'Restored.',
    );
    setBusy(false);
    setConfirm(false);
    if (succeeded(view)) onDone();
  };

  if (confirm) {
    return (
      <ConfirmSheet
        title={summary.destructive ? 'Restore and overwrite?' : 'Restore this backup?'}
        message={
          summary.destructive
            ? 'Existing files in the target are force-overwritten where the backup has the same name, and the sessions you chose to overwrite are replaced.'
            : 'The backup’s files and sessions are added to the target.'
        }
        facts={[
          ['Target', <span className="mono">{plan.target_display}</span>],
          ['Files overwritten', `${summary.overwritten}`],
          ['Files added', `${summary.added}`],
          ['Sessions added', `${summary.sessionsAdded}`],
          ['Sessions overwritten', `${summary.overwrite}`],
          ['Kept as copies', `${summary.both}`],
          ['Local kept', `${summary.keep}`],
        ]}
        confirm="Restore"
        busy={busy}
        onConfirm={() => void apply()}
        onCancel={() => setConfirm(false)}
      />
    );
  }

  return (
    <Sheet title="Restore plan" icon="⤓" onClose={onDone}>
      <KeyValues
        rows={[
          ['Backup', `${backup.host} · ${when(backup.created_unix_ms)}`],
          ['Target', <span className="mono">{plan.target_display}</span>],
          plan.create_target ? ['Folder', <span className="chip accent">created by the restore</span>] : null,
        ]}
      />
      {(plan.omp_exists || plan.memory_exists) && (
        <div className="banner danger spaced" role="note">
          <span className="grow">
            <b>Overwrite warning.</b> The target already has {[plan.omp_exists && '.omp', plan.memory_exists && 'memory'].filter(Boolean).join(' and ')}
            . Files with the same name are force-overwritten when you apply.
          </span>
        </div>
      )}
      <div className="plan">
        {(plan.omp_overwritten.length > 0 || plan.omp_added.length > 0 || plan.omp_skipped.length > 0) && (
          <>
            <PlanList title=".omp overwritten" tone="danger" items={plan.omp_overwritten} empty="nothing" />
            <PlanList title=".omp added" tone="ok" items={plan.omp_added} empty="nothing" />
            {plan.omp_skipped.length > 0 && <PlanList title=".omp skipped" tone="warn" items={plan.omp_skipped} empty="nothing" />}
          </>
        )}
        {(plan.memory_overwritten.length > 0 || plan.memory_added.length > 0 || plan.memory_skipped.length > 0) && (
          <>
            <PlanList title="Memory overwritten" tone="danger" items={plan.memory_overwritten} empty="nothing" />
            <PlanList title="Memory added" tone="ok" items={plan.memory_added} empty="nothing" />
            {plan.memory_skipped.length > 0 && <PlanList title="Memory skipped" tone="warn" items={plan.memory_skipped} empty="nothing" />}
          </>
        )}
      </div>
      {conflicts.length > 0 && (
        <div className="conflicts">
          <div className="step-head">
            <span className="chip warn">{conflicts.length}</span>
            <b>Sessions that already exist</b>
          </div>
          <p className="muted small">Keep mine is the default: nothing local is replaced unless you choose it.</p>
          {conflicts.map((session) => (
            <div key={session.id} className={`conflict ${choices[session.id] ?? 'keep'}`}>
              <b>{session.title || session.id}</b>
              <div className="conflict-facts">
                <span className="chip accent">backup · {age(session.modified_unix_ms, now)} ago · {bytes(session.bytes)}</span>
                {session.local.map((copy, index) => (
                  <span key={index} className="chip">
                    local{copy.display !== null ? ` ${copy.display}` : ''} · {age(copy.modified_unix_ms, now)} ago · {bytes(copy.bytes)}
                  </span>
                ))}
              </div>
              {session.problem !== null && <p className="field-error">{session.problem}</p>}
              <div className="segmented" role="radiogroup" aria-label={`For ${session.title || session.id}`}>
                {CHOICES.map((choice) => (
                  <button
                    key={choice.value}
                    type="button"
                    role="radio"
                    aria-checked={(choices[session.id] ?? 'keep') === choice.value}
                    aria-pressed={(choices[session.id] ?? 'keep') === choice.value}
                    onClick={() => setChoices({ ...choices, [session.id]: choice.value })}
                  >
                    {choice.label}
                  </button>
                ))}
              </div>
            </div>
          ))}
        </div>
      )}
      {added.length > 0 && (
        <PlanList
          title="Sessions added"
          tone="ok"
          items={added.map((s) => `${s.title || s.id}${s.problem !== null ? ` — ${s.problem}` : ''}`)}
          empty="none"
        />
      )}
      {prepared.truncated && <p className="muted small note">The lists are cut; the restore covers every file.</p>}
      <div className="buttons pair spaced">
        <button type="button" className="button" disabled={busy} onClick={onBack}>
          ‹ Back
        </button>
        <button type="button" className={`button ${summary.destructive ? 'danger' : 'primary'}`} disabled={!manage || busy} onClick={() => setConfirm(true)}>
          Restore…
        </button>
      </div>
    </Sheet>
  );
}
