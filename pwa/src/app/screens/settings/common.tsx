// Shared pieces of the settings sections: the pushed-screen frame, rows, bars and the request
// helper for the non-journaled (View) ops.

import type { ReactNode } from 'react';
import type { api } from '../../../remote/index.ts';
import { ClientError, type Client, describeApiError } from '../../client.ts';
import { useNav } from '../../nav.ts';
import { useCan } from '../../ui/hooks.ts';
import { ConnectionPill, StaleBanner, Switch } from '../../ui/kit.tsx';

/** A settings section pushed over the hub: back button, eyebrow, title and the pill. */
export function SectionScreen({
  eyebrow,
  title,
  aside,
  children,
}: {
  eyebrow: string;
  title: string;
  aside?: ReactNode;
  children: ReactNode;
}): ReactNode {
  const nav = useNav();
  return (
    <div className="screen pushed settings">
      <header className="top">
        <div className="line">
          <button type="button" className="back" onClick={nav.pop} aria-label="Back">
            ‹
          </button>
          <div className="grow">
            <div className="eyebrow">{eyebrow}</div>
            <h2>{title}</h2>
          </div>
          {aside ?? <ConnectionPill />}
        </div>
      </header>
      <div className="body">
        <StaleBanner />
        {children}
      </div>
    </div>
  );
}

/**
 * Sends a non-journaled request and returns its reply, or null after reporting the problem as a
 * notice (offline, not permitted, refused).
 */
export async function ask(client: Client, op: api.Op, what: string): Promise<api.Resp | null> {
  try {
    return await client.request(op);
  } catch (error) {
    const text =
      error instanceof ClientError && error.api !== null
        ? describeApiError(error.api)
        : error instanceof Error
          ? error.message
          : String(error);
    client.notify(`${what}: ${text}`, 'error');
    return null;
  }
}

/** Whether this phone may send Manage ops now (online and permitted). */
export function useManage(): boolean {
  const can = useCan();
  return can({ op: 'omp_reset', key: '' });
}

/** Whether this phone may send Control ops now. */
export function useControl(): boolean {
  const can = useCan();
  return can({ op: 'backup_run' });
}

/** A labelled switch row: title and hint on the left, the switch on the right. */
export function ToggleRow({
  title,
  hint,
  on,
  disabled = false,
  onChange,
}: {
  title: string;
  hint?: ReactNode;
  on: boolean;
  disabled?: boolean;
  onChange: (on: boolean) => void;
}): ReactNode {
  return (
    <div className="set-row">
      <div className="grow">
        <b>{title}</b>
        {hint !== undefined && <small>{hint}</small>}
      </div>
      <Switch on={on} label={title} disabled={disabled} onChange={onChange} />
    </div>
  );
}

/** A remaining/used bar; `fraction` 0..1 fills it, the tone colours it. */
export function Bar({ fraction, tone }: { fraction: number; tone: 'ok' | 'warn' | 'danger' | 'accent' }): ReactNode {
  return (
    <span className={`bar ${tone}`} role="presentation">
      <i style={{ width: `${Math.round(Math.max(0, Math.min(1, fraction)) * 1000) / 10}%` }} />
    </span>
  );
}

/** Explains why a section is read-only on this phone. */
export function ReadOnlyNote({ need }: { need: 'manage' | 'control' }): ReactNode {
  return (
    <div className="banner warn" role="note">
      <span className="grow">
        This phone has no <b>{need === 'manage' ? 'Manage' : 'Control'}</b> permission, so this is read-only. Grant it on the
        desktop in Settings → Mobile access.
      </span>
    </div>
  );
}
