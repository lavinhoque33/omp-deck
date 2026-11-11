// The Android app's own update (shown only inside the APK): the installed and latest release, a
// manual check, the automatic-check switch, download progress, and the install / retry action. It
// talks to the shell alone, so it works with the desktop offline and without any permission; the
// desktop's gateway and omp updates stay in the Updates section.

import type { ReactNode } from 'react';
import { ACTION_LABEL, downloadFraction, isNewerVersion, updateOffer } from '../../native-update.ts';
import type { NativeUpdateState } from '../../native.ts';
import { age } from '../../ui/format.ts';
import { useNativeUpdate, useNow } from '../../ui/hooks.ts';
import { Card, KeyValues } from '../../ui/kit.tsx';
import { Bar, ToggleRow } from './common.tsx';
import { bytes } from './logic.ts';

const STATUS: Record<NativeUpdateState['phase'], string> = {
  idle: 'not checked',
  checking: 'checking…',
  available: 'update available',
  downloading: 'downloading',
  ready: 'ready to install',
  installing: 'installing',
  error: 'failed',
};

const HINT: Partial<Record<NativeUpdateState['phase'], string>> = {
  ready: 'The download is verified. Install opens Android’s installer, which asks you to confirm.',
  installing: 'Confirm in Android’s installer. If you cancelled it, Install opens it again.',
};

export function AndroidUpdateCard(): ReactNode {
  const update = useNativeUpdate();
  const now = useNow();
  if (update === null) return null;
  const { view, controller } = update;
  const state = view.state;
  const offer = updateOffer(state);
  const fraction = state === null ? null : downloadFraction(state);
  const error = view.failure ?? state?.error ?? null;
  const checking = state?.phase === 'checking';
  const downloading = state?.phase === 'downloading';
  const upToDate = state !== null && !isNewerVersion(state.latest, state.current) && state.checked_unix_ms !== null;

  return (
    <Card
      title="OmpDeck for Android"
      aside={
        <button type="button" className="button" disabled={view.busy || checking || downloading} onClick={() => void controller.check()}>
          {checking ? <span className="busy" /> : '↻'} Check now
        </button>
      }
    >
      {state !== null && (
        <KeyValues
          rows={[
            ['Installed', <span className="chip">{state.current}</span>],
            ['Latest', <span className={`chip ${offer !== null ? 'accent' : state.latest !== null ? 'ok' : ''}`}>{state.latest ?? '—'}</span>],
            [
              'Status',
              <span className={`chip ${state.phase === 'error' ? 'danger' : offer !== null ? 'warn' : upToDate ? 'ok' : ''}`}>
                {state.phase === 'idle' ? (offer !== null ? 'update available' : upToDate ? 'up to date' : STATUS.idle) : STATUS[state.phase]}
              </span>,
            ],
            ['Checked', state.checked_unix_ms !== null ? `${age(state.checked_unix_ms, now)} ago` : 'never'],
          ]}
        />
      )}
      {state !== null && downloading && (
        <div className="phase">
          <div className="line spread small">
            <span>Downloading {state.latest ?? 'the update'}</span>
            <span className="muted">
              {bytes(state.downloaded)}
              {state.total !== null && ` / ${bytes(state.total)}`}
            </span>
          </div>
          <Bar fraction={fraction ?? 0} tone="accent" />
        </div>
      )}
      {error !== null && <p className="field-error">{error}</p>}
      {state !== null && HINT[state.phase] !== undefined && <p className="muted small note">{HINT[state.phase]}</p>}
      {offer?.action != null && (
        <div className="buttons">
          <button type="button" className="button primary" disabled={view.busy} onClick={() => void controller.install()}>
            {ACTION_LABEL[offer.action]} {offer.latest}
          </button>
        </div>
      )}
      {state !== null && (
        <ToggleRow
          title="Check automatically"
          hint="When the app starts and every few hours while it is open. Nothing is downloaded or installed until you choose Update."
          on={state.auto_check}
          disabled={view.busy}
          onChange={(on) => void controller.setAutoCheck(on)}
        />
      )}
      {state === null && view.failure === null && <p className="muted small note">Asking Android…</p>}
    </Card>
  );
}
