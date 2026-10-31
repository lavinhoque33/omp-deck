// Updates: omp's version with check and the update jobs (omp, plugins, release channel), which run
// in a session the phone opens as a terminal; OmpDeck's own update (check, download, apply where
// the install allows it from a phone).

import { type ReactNode, useState } from 'react';
import type { api } from '../../../remote/index.ts';
import { useNav } from '../../nav.ts';
import { age, perform, succeeded } from '../../ui/format.ts';
import { useClient, useFeed, useNow, useOnline } from '../../ui/hooks.ts';
import { Card, KeyValues, NeedsS4, Sheet } from '../../ui/kit.tsx';
import { Bar, ReadOnlyNote, SectionScreen, ask, useControl, useManage } from './common.tsx';
import { bytes } from './logic.ts';

export function UpdatesSection(): ReactNode {
  return (
    <SectionScreen eyebrow="Settings" title="Updates">
      <NeedsS4 what="Updates">
        <UpdatesBody />
      </NeedsS4>
    </SectionScreen>
  );
}

const RUNS: readonly { kind: api.UpdateKind; label: string; detail: string }[] = [
  { kind: 'omp', label: 'Update omp', detail: 'Installs the newest omp release' },
  { kind: 'plugins', label: 'Update plugins', detail: 'Updates every installed omp plugin' },
  { kind: 'stable', label: 'Switch to stable', detail: 'Follows tested releases' },
  { kind: 'canary', label: 'Switch to canary', detail: 'Follows the newest builds' },
];

const INSTALL_LABEL: Record<api.InstallKind, string> = {
  user_dir: 'tarball / install.sh',
  app_image: 'AppImage',
  deb: '.deb package',
  rpm: '.rpm package',
  windows_dir: 'Windows folder',
  dev: 'development build',
  unsupported: 'unsupported install',
};

/** Why OmpDeck cannot be applied from the phone, by install kind. */
function applyBlocked(install: api.InstallKind): string {
  switch (install) {
    case 'deb':
    case 'rpm':
      return 'This install updates through a system password prompt (polkit), which appears on the desktop.';
    case 'dev':
      return 'A development build is updated from its source checkout.';
    default:
      return 'This install cannot replace itself from a phone.';
  }
}

function UpdatesBody(): ReactNode {
  const client = useClient();
  const nav = useNav();
  const now = useNow();
  const online = useOnline();
  const control = useControl();
  const manage = useManage();
  const feed = useFeed({ feed: 'updates' }, 'updates');
  const [running, setRunning] = useState<api.UpdateKind | null>(null);
  const [confirm, setConfirm] = useState<(typeof RUNS)[number] | null>(null);
  const [appBusy, setAppBusy] = useState<api.AppUpdateStep | null>(null);
  const omp = feed?.omp ?? null;
  const app = feed?.app ?? null;
  const ompNewer = omp?.latest != null && omp.current != null && omp.latest !== omp.current;
  const appNewer = app?.latest != null && app.latest !== app.current;
  const checking = omp?.checking === true || app?.phase.state === 'checking';

  const check = () => void ask(client, { op: 'update_check' }, 'Check for updates');
  const run = async (entry: (typeof RUNS)[number]) => {
    setConfirm(null);
    setRunning(entry.kind);
    const view = await perform(client, { op: 'update_run', kind: entry.kind }, entry.label);
    setRunning(null);
    const result = view?.outcome?.state === 'done' ? view.outcome.result : null;
    if (succeeded(view) && result?.type === 'session') nav.push({ kind: 'terminal', liveKey: result.live.key, title: entry.label });
  };
  const appStep = async (step: api.AppUpdateStep) => {
    setAppBusy(step);
    await perform(client, { op: 'app_update', step }, step === 'download' ? 'Download OmpDeck update' : 'Apply OmpDeck update');
    setAppBusy(null);
  };

  return (
    <>
      {online && !manage && <ReadOnlyNote need="manage" />}
      <Card
        title="omp"
        aside={
          <button type="button" className="button" disabled={!control || checking} onClick={check}>
            {checking ? <span className="busy" /> : '↻'} Check
          </button>
        }
      >
        <KeyValues
          rows={[
            ['Installed', <span className="chip">{omp?.current ?? omp?.version ?? '—'}</span>],
            ['Latest', <span className={`chip ${ompNewer ? 'accent' : 'ok'}`}>{omp?.latest ?? '—'}</span>],
            ['Status', ompNewer ? <span className="chip warn">update available</span> : omp?.latest != null ? <span className="chip ok">up to date</span> : '—'],
            ['Checked', omp?.checked_unix_ms != null ? `${age(omp.checked_unix_ms, now)} ago` : 'never'],
          ]}
        />
        {omp?.error != null && <p className="field-error">{omp.error}</p>}
        <div className="run-list">
          {RUNS.map((entry) => (
            <button
              key={entry.kind}
              type="button"
              className={`button run${entry.kind === 'omp' && ompNewer ? ' primary' : ''}`}
              disabled={!manage || running !== null}
              onClick={() => setConfirm(entry)}
            >
              <span className="grow">
                <b>{entry.label}</b>
                <small>{entry.detail}</small>
              </span>
              {running === entry.kind ? <span className="busy" /> : <span aria-hidden="true">›</span>}
            </button>
          ))}
        </div>
        <p className="muted small note">
          Each job runs in a terminal session on the desktop that opens here. Running omp sessions keep the old version until
          they are restarted.
        </p>
      </Card>

      <Card title="OmpDeck" aside={<span className="chip">{app === null ? '—' : INSTALL_LABEL[app.install]}</span>}>
        <KeyValues
          rows={[
            ['Installed', <span className="chip">{app?.current ?? '—'}</span>],
            ['Latest', <span className={`chip ${appNewer ? 'accent' : 'ok'}`}>{app?.latest ?? '—'}</span>],
            ['Checked', app?.checked_unix_ms != null ? `${age(app.checked_unix_ms, now)} ago` : 'never'],
            app?.asset != null ? ['Asset', <span className="mono">{app.asset}</span>] : null,
            app?.page_url != null
              ? [
                  'Release',
                  <a className="link" href={app.page_url} target="_blank" rel="noreferrer noopener">
                    release notes ↗
                  </a>,
                ]
              : null,
          ]}
        />
        {app !== null && <AppPhase phase={app.phase} />}
        {app !== null && !app.apply_allowed && (
          <div className="banner warn" role="note">
            <span className="grow">
              <b>Apply at the desktop.</b> {applyBlocked(app.install)}
            </span>
          </div>
        )}
        <div className="buttons">
          <button type="button" className="button" disabled={!control || checking} onClick={check}>
            ↻ Check
          </button>
          <button
            type="button"
            className="button"
            disabled={!manage || !appNewer || appBusy !== null || app?.phase.state === 'downloading'}
            onClick={() => void appStep('download')}
          >
            {appBusy === 'download' ? 'Starting…' : '⤓ Download'}
          </button>
          {app?.apply_allowed === true && (
            <button
              type="button"
              className="button primary"
              disabled={!manage || appBusy !== null || !appNewer}
              onClick={() => void appStep('apply')}
            >
              {appBusy === 'apply' ? 'Applying…' : 'Apply & restart'}
            </button>
          )}
        </div>
        <p className="muted small note">Applying replaces the desktop gateway; this phone reconnects by itself.</p>
      </Card>

      {confirm !== null && (
        <Sheet title={confirm.label} icon="⤒" onClose={() => setConfirm(null)}>
          <p>{confirm.detail}. The job runs on the desktop in a new terminal session, which opens on this phone.</p>
          <KeyValues
            rows={[
              ['Installed omp', omp?.current ?? '—'],
              ['Running sessions', 'keep the old version until restarted'],
            ]}
          />
          <div className="buttons pair spaced">
            <button type="button" className="button" onClick={() => setConfirm(null)}>
              Cancel
            </button>
            <button type="button" className="button primary" onClick={() => void run(confirm)}>
              Run
            </button>
          </div>
        </Sheet>
      )}
    </>
  );
}

function AppPhase({ phase }: { phase: api.UpdatePhase }): ReactNode {
  switch (phase.state) {
    case 'idle':
      return null;
    case 'checking':
      return <p className="muted small note">Checking…</p>;
    case 'downloading':
      return (
        <div className="phase">
          <div className="line spread small">
            <span>Downloading</span>
            <span className="muted">
              {bytes(phase.done)}
              {phase.total !== null && ` / ${bytes(phase.total)}`}
            </span>
          </div>
          <Bar fraction={phase.total === null || phase.total === 0 ? 0 : phase.done / phase.total} tone="accent" />
        </div>
      );
    case 'applied':
      return (
        <div className="banner" role="status">
          <span className="grow">Applied. The desktop restarts OmpDeck; this phone reconnects.</span>
        </div>
      );
    case 'needs_manual':
      return (
        <div className="banner warn" role="status">
          <span className="grow">Downloaded. Install it at the desktop (OmpDeck shows the steps there).</span>
        </div>
      );
    case 'failed':
      return (
        <div className="banner danger" role="alert">
          <span className="grow">{phase.message}</span>
        </div>
      );
  }
}
