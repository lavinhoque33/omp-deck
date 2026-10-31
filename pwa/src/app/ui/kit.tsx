// Shared building blocks: sheets (styled header + body), destructive confirmations, key/value
// rows, the connection pill, toasts, the bottom nav and the app banners.

import { type ReactNode, useEffect, useId, useSyncExternalStore } from 'react';
import { applyUpdate, cancelUpdate, updates } from '../update.ts';
import type { Tab } from '../nav.ts';
import { hasS4 } from '../client.ts';
import { useClient, useClientState, useNow } from './hooks.ts';

// ---- sheets ---------------------------------------------------------------------------------------

export function Sheet({
  title,
  icon,
  destructive = false,
  onClose,
  children,
}: {
  title: string;
  icon?: ReactNode;
  destructive?: boolean;
  onClose: () => void;
  children: ReactNode;
}): ReactNode {
  const titleId = useId();
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);
  return (
    <>
      <div className="backdrop" onClick={onClose} aria-hidden="true" />
      <section className={`sheet${destructive ? ' destructive' : ''}`} role="dialog" aria-modal="true" aria-labelledby={titleId}>
        <div className="handle" />
        <header>
          {icon !== undefined && <span className={destructive ? 'danger-icon' : 'sheet-icon'}>{icon}</span>}
          <h3 id={titleId}>{title}</h3>
          <button type="button" className="icon-btn" onClick={onClose} aria-label="Close">
            ✕
          </button>
        </header>
        <div className="sheet-body">{children}</div>
      </section>
    </>
  );
}

/**
 * A destructive confirmation: names exactly what goes (`facts`), with a red primary button. The
 * gateway does not ask again (R4); this sheet is the confirmation.
 */
export function ConfirmSheet({
  title,
  message,
  facts,
  confirm,
  busy = false,
  disabled = false,
  onConfirm,
  onCancel,
}: {
  title: string;
  message: ReactNode;
  facts: readonly (readonly [string, ReactNode])[];
  confirm: string;
  busy?: boolean;
  disabled?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}): ReactNode {
  return (
    <Sheet title={title} icon="⌫" destructive onClose={onCancel}>
      <p>{message}</p>
      {facts.map(([key, value]) => (
        <div className="reclaim" key={key}>
          <span>{key}</span>
          <strong>{value}</strong>
        </div>
      ))}
      <div className="buttons pair">
        <button type="button" className="button" onClick={onCancel}>
          Cancel
        </button>
        <button type="button" className="button danger" onClick={onConfirm} disabled={busy || disabled}>
          {busy ? 'Working…' : confirm}
        </button>
      </div>
    </Sheet>
  );
}

// ---- cards and rows -------------------------------------------------------------------------------

export function Card({ title, aside, children }: { title: string; aside?: ReactNode; children: ReactNode }): ReactNode {
  return (
    <section className="card">
      <header>
        <span className="title">{title}</span>
        {aside}
      </header>
      <div className="card-body">{children}</div>
    </section>
  );
}

/** Key/value rows: keys as small caps labels, values as text or chips. */
export function KeyValues({ rows }: { rows: readonly (readonly [string, ReactNode] | null)[] }): ReactNode {
  return (
    <dl className="kv">
      {rows.map((row) =>
        row === null ? null : (
          <KeyValueRow key={row[0]} name={row[0]}>
            {row[1]}
          </KeyValueRow>
        ),
      )}
    </dl>
  );
}

function KeyValueRow({ name, children }: { name: string; children: ReactNode }): ReactNode {
  return (
    <>
      <dt>{name}</dt>
      <dd>{children}</dd>
    </>
  );
}

export function Empty({ title, children }: { title: string; children?: ReactNode }): ReactNode {
  return (
    <div className="empty">
      <b>{title}</b>
      {children}
    </div>
  );
}

/** An on/off switch with depth and a press state; `label` names it for screen readers. */
export function Switch({
  on,
  label,
  disabled = false,
  onChange,
}: {
  on: boolean;
  label: string;
  disabled?: boolean;
  onChange: (on: boolean) => void;
}): ReactNode {
  return (
    <button type="button" role="switch" className="switch" aria-checked={on} aria-label={label} disabled={disabled} onClick={() => onChange(!on)}>
      <i aria-hidden="true" />
    </button>
  );
}

/** Whether the connected desktop speaks slice 4 (`Welcome.features` lists `s4`). */
export function useS4(): boolean {
  return useClientState((state) => hasS4(state.welcome));
}

/**
 * Stands in for a slice-4 surface when the desktop runs an older OmpDeck: says what is missing
 * and how to get it, instead of failing requests. Renders `children` when the desktop is new
 * enough (or nothing is known yet: offline before the first connection).
 */
export function NeedsS4({ what, children }: { what: string; children?: ReactNode }): ReactNode {
  const welcome = useClientState((state) => state.welcome);
  if (welcome === null || hasS4(welcome)) return children;
  return (
    <div className="empty needs-s4" role="status">
      <b>{what} needs a newer OmpDeck on the desktop</b>
      <span>
        The desktop runs OmpDeck {welcome.gateway_version}, which does not offer this yet. Update OmpDeck on the desktop;
        this phone reconnects by itself.
      </span>
    </div>
  );
}

// ---- connection -----------------------------------------------------------------------------------

/** The header pill: connected, reconnecting (with countdown), or stopped (with the reason). */
export function ConnectionPill(): ReactNode {
  const client = useClient();
  const conn = useClientState((state) => state.conn);
  const pairing = useClientState((state) => state.pairing);
  const welcome = useClientState((state) => state.welcome);
  const now = useNow(1000);
  const desktop = welcome?.name ?? pairing?.name ?? 'desktop';
  switch (conn.phase) {
    case 'online':
      return (
        <span className="pill connected" role="status">
          <span className="dot" />
          Connected · {desktop}
        </span>
      );
    case 'loading':
    case 'connecting':
      return (
        <span className="pill conn-wait" role="status">
          <span className="dot warn" />
          Connecting…
        </span>
      );
    case 'waiting': {
      const seconds = conn.retryAt === null ? 0 : Math.max(0, Math.ceil((conn.retryAt - now) / 1000));
      return (
        <button type="button" className="pill conn-wait" onClick={() => client.reconnectNow()}>
          <span className="dot warn" />
          Offline · retry in {seconds}s · tap to retry
        </button>
      );
    }
    case 'stopped':
      return (
        <button type="button" className="pill conn-off" onClick={() => client.reconnectNow()}>
          <span className="dot off" />
          {stopText(conn.stop)} · tap to retry
        </button>
      );
    case 'unpaired':
      return (
        <span className="pill conn-off">
          <span className="dot off" />
          Not paired
        </span>
      );
  }
}

function stopText(stop: string | null): string {
  switch (stop) {
    case 'unauthorized':
      return 'Not authorized (revoked on desktop?)';
    case 'upgrade':
      return 'App update needed';
    case 'rate_limited':
      return 'Relay busy';
    case 'replaced':
      return 'Open in another tab';
    default:
      return 'Disconnected';
  }
}

/** Shown under the header while offline: what is on screen is the last known state. */
export function StaleBanner(): ReactNode {
  const phase = useClientState((state) => state.conn.phase);
  const everOnline = useClientState((state) => state.conn.everOnline);
  if (phase === 'online' || !everOnline) return null;
  return (
    <div className="banner warn" role="status">
      <span>⚠</span>
      <span className="grow">Offline: showing the last known state. Actions are disabled until the desktop is back.</span>
    </div>
  );
}

// ---- toasts ---------------------------------------------------------------------------------------

export function Toasts(): ReactNode {
  const client = useClient();
  const notices = useClientState((state) => state.notices);
  useEffect(() => {
    const first = notices[0];
    if (first === undefined) return;
    const timer = setTimeout(() => client.dismissNotice(first.id), first.tone === 'info' ? 3500 : 7000);
    return () => clearTimeout(timer);
  }, [client, notices]);
  if (notices.length === 0) return null;
  return (
    <div className="toasts" aria-live="polite">
      {notices.map((notice) => (
        <div key={notice.id} className={`toast ${notice.tone}`}>
          <span className="grow">{notice.text}</span>
          <button type="button" className="icon-btn" onClick={() => client.dismissNotice(notice.id)} aria-label="Dismiss">
            ✕
          </button>
        </div>
      ))}
    </div>
  );
}

// ---- bottom nav -----------------------------------------------------------------------------------

const TABS: readonly (readonly [Tab, string, string])[] = [
  ['sessions', 'Sessions', '▣'],
  ['agents', 'Agents', '♧'],
  ['inbox', 'Inbox', '▱'],
  ['settings', 'Settings', '⚙'],
];

export function BottomNav({ tab, badge, onSelect }: { tab: Tab; badge: string | null; onSelect: (tab: Tab) => void }): ReactNode {
  return (
    <nav className="nav" aria-label="Main">
      {TABS.map(([id, label, glyph]) => (
        <button
          key={id}
          type="button"
          className={tab === id ? 'selected' : ''}
          aria-current={tab === id ? 'page' : undefined}
          onClick={() => onSelect(id)}
        >
          <i aria-hidden="true">{glyph}</i>
          {label}
          {id === 'inbox' && badge !== null && <span className="badge">{badge}</span>}
        </button>
      ))}
    </nav>
  );
}

// ---- banners --------------------------------------------------------------------------------------

/** iOS needs a Home Screen install for persistent storage and notifications. */
export function InstallHint({ standalone }: { standalone: boolean }): ReactNode {
  if (standalone) return null;
  return (
    <div className="banner">
      <span>⇪</span>
      <span className="grow">Add to Home Screen to keep your pairing and get notifications.</span>
    </div>
  );
}

/** "Update available → Reload": the reload waits for open requests unless forced. */
export function UpdateBanner(): ReactNode {
  const client = useClient();
  const update = useSyncExternalStore(updates.subscribe, updates.get);
  const open = useClientState((state) => state.requests.filter((request) => request.status === 'sending' || request.status === 'accepted' || request.status === 'running').length);
  if (!update.available) return null;
  const openRequests = () => client.openRequests;
  if (update.pending) {
    return (
      <div className="banner" role="status">
        <span className="busy" />
        <span className="grow">{open > 0 ? `Reloading after ${open} open request${open === 1 ? '' : 's'} finish…` : 'Reloading…'}</span>
        {open > 0 && (
          <>
            <button type="button" className="button" onClick={cancelUpdate}>
              Later
            </button>
            <button type="button" className="button danger" onClick={() => applyUpdate(openRequests, true)}>
              Reload anyway
            </button>
          </>
        )}
      </div>
    );
  }
  return (
    <div className="banner">
      <span>↻</span>
      <span className="grow">Update available.</span>
      <button type="button" className="button primary" onClick={() => applyUpdate(openRequests, false)}>
        Reload
      </button>
    </div>
  );
}
