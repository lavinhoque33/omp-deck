// The Settings hub: appearance (six live theme previews), links to every settings section with a
// one-glance status, and the connection to the desktop gateway.

import type { ReactNode } from 'react';
import { type SettingsSection, useNav } from '../nav.ts';
import { DEFAULT_THEME, THEMES, type ThemeId } from '../theme.ts';
import { age } from '../ui/format.ts';
import { useClient, useClientState, useFeed, useNow } from '../ui/hooks.ts';
import { Card, ConnectionPill, InstallHint, KeyValues, UpdateBanner, useS4 } from '../ui/kit.tsx';
import '../../styles/settings.css';
import { MODE_LABEL, percent, remainingTone } from './settings/logic.ts';

export function SettingsScreen({
  theme,
  onTheme,
  standalone,
}: {
  theme: ThemeId;
  onTheme: (theme: ThemeId) => void;
  standalone: boolean;
}): ReactNode {
  const client = useClient();
  const nav = useNav();
  const now = useNow();
  const s4 = useS4();
  const pairing = useClientState((state) => state.pairing);
  const welcome = useClientState((state) => state.welcome);
  const conn = useClientState((state) => state.conn);
  const device = useFeed({ feed: 'device' }, 'device');
  const updates = useFeed({ feed: 'updates' }, 'updates');
  const drive = useFeed({ feed: 'drive' }, 'drive');
  const sync = useFeed({ feed: 'sync' }, 'sync');
  const usage = useFeed({ feed: 'usage' }, 'usage');
  const manage = (device?.permissions ?? welcome?.permissions)?.manage ?? false;

  const tightest = usage?.usage?.tightest_remaining ?? null;
  const ompNewer = updates?.omp.latest != null && updates.omp.current != null && updates.omp.latest !== updates.omp.current;
  const appNewer = updates?.app.latest != null && updates.app.latest !== updates.app.current;
  const row = (section: SettingsSection, icon: ReactNode, title: string, value: ReactNode) => (
    <HubRow key={section} icon={icon} title={title} value={value} disabled={!s4} onClick={() => nav.push({ kind: 'settings', section })} />
  );

  return (
    <div className="screen settings">
      <header className="top">
        <div className="line">
          <h2 className="grow">Settings</h2>
          <ConnectionPill />
        </div>
      </header>
      <div className="body">
        <UpdateBanner />
        <InstallHint standalone={standalone} />

        <Card title="Appearance" aside={<span className="count">6 themes</span>}>
          <div className="theme-tiles" role="radiogroup" aria-label="Theme">
            {THEMES.map((entry) => (
              <button
                key={entry.id}
                type="button"
                role="radio"
                aria-checked={theme === entry.id}
                className="theme-tile"
                onClick={() => onTheme(entry.id)}
              >
                <ThemePreview id={entry.id} />
                <span className="tile-name">
                  {entry.name}
                  {entry.id === DEFAULT_THEME && <span className="chip">default</span>}
                </span>
              </button>
            ))}
          </div>
        </Card>

        {!s4 && welcome !== null && (
          <div className="banner warn" role="note">
            <span className="grow">
              The desktop runs OmpDeck {welcome.gateway_version}; update it there for notifications, usage, backups and the omp
              settings.
            </span>
          </div>
        )}

        <Card title="This phone">
          <div className="hub-list">
            {row(
              'notifications',
              '✉',
              'Notifications',
              <span className={`chip ${device?.push.subscribed === true ? 'ok' : ''}`}>{device === undefined ? '—' : device.push.subscribed ? 'push on' : 'push off'}</span>,
            )}
            {row('device', <PhoneIcon />, 'This device', <span className="hub-value">{device?.name ?? pairing?.device ?? '—'}</span>)}
          </div>
        </Card>

        <Card title="omp">
          <div className="hub-list">
            {row(
              'usage',
              '▤',
              'Usage',
              <span className={`chip ${remainingTone(tightest)}`}>{tightest === null ? '—' : `${percent(tightest)} left`}</span>,
            )}
            {row('omp', '⚙', 'omp settings', <span className="chip">{manage ? 'editable' : 'read-only'}</span>)}
            {row(
              'updates',
              '⤒',
              'Updates',
              updates === undefined ? (
                <span className="chip">—</span>
              ) : ompNewer || appNewer ? (
                <span className="chip warn">{ompNewer && appNewer ? '2 updates' : ompNewer ? 'omp update' : 'OmpDeck update'}</span>
              ) : (
                <span className="chip ok">up to date</span>
              ),
            )}
          </div>
        </Card>

        <Card title="Backup">
          <div className="hub-list">
            {row(
              'drive',
              '☁',
              'Google Drive',
              drive === undefined ? (
                <span className="chip">—</span>
              ) : !drive.available ? (
                <span className="chip">unavailable</span>
              ) : drive.signed_in ? (
                <span className="chip ok">{drive.last_backup_unix_ms === null ? 'signed in' : `${age(drive.last_backup_unix_ms, now)} ago`}</span>
              ) : (
                <span className="chip warn">signed out</span>
              ),
            )}
            {row(
              'sync',
              '⇅',
              'Project sync',
              <span className="chip">{sync === undefined ? '—' : `${sync.rows.length} project${sync.rows.length === 1 ? '' : 's'}`}</span>,
            )}
          </div>
        </Card>

        <Card
          title="Connection"
          aside={
            <button type="button" className="icon-btn" disabled={conn.phase === 'online' || conn.phase === 'connecting'} onClick={() => client.reconnectNow()}>
              Reconnect
            </button>
          }
        >
          <KeyValues
            rows={[
              ['Desktop', welcome?.name ?? pairing?.name ?? '—'],
              ['Status', <span className={`chip ${conn.phase === 'online' ? 'ok' : conn.phase === 'stopped' ? 'danger' : 'warn'}`}>{conn.phase}</span>],
              welcome !== null ? ['Gateway', <span className="chip accent">{welcome.gateway_version}</span>] : null,
              device !== undefined ? ['Mode', <span className="chip">{MODE_LABEL[device.mode]}</span>] : null,
              welcome !== null ? ['Protocol', <span className="chip">v{welcome.protocol}{welcome.features.length > 0 ? ` · ${welcome.features.join(' ')}` : ''}</span>] : null,
              ['Relay', <span className="mono">{pairing?.url ?? '—'}</span>],
              ['Fingerprint', <span className="mono">{pairing?.gatewayFingerprint ?? '—'}</span>],
              conn.lastClose !== null ? ['Last close', <span className="mono">{`${conn.lastClose.code} ${conn.lastClose.reason}`.trim()}</span>] : null,
            ]}
          />
        </Card>
      </div>
    </div>
  );
}

/** A phone outline (the bundled fonts have no glyph for one). */
function PhoneIcon(): ReactNode {
  return (
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" focusable="false">
      <rect x="4" y="1.5" width="8" height="13" rx="1.8" />
      <path d="M7 12h2" />
    </svg>
  );
}

function HubRow({ icon, title, value, disabled, onClick }: { icon: ReactNode; title: string; value: ReactNode; disabled: boolean; onClick: () => void }): ReactNode {
  return (
    <button type="button" className="hub-row" disabled={disabled} onClick={onClick}>
      <span className="hub-icon" aria-hidden="true">
        {icon}
      </span>
      <b className="grow">{title}</b>
      {value}
      <span className="chev" aria-hidden="true">
        ›
      </span>
    </button>
  );
}

/** A miniature screen painted with the theme's own tokens (`data-theme` scopes them). */
function ThemePreview({ id }: { id: ThemeId }): ReactNode {
  return (
    <span className="theme-preview" data-theme={id} aria-hidden="true">
      <span className="tp-top">
        <i className="tp-title" />
        <i className="tp-pill" />
      </span>
      <span className="tp-card">
        <i className="tp-dot" />
        <span className="tp-lines">
          <i />
          <i className="short" />
        </span>
      </span>
      <span className="tp-card">
        <i className="tp-dot warn" />
        <span className="tp-lines">
          <i className="short" />
          <i />
        </span>
      </span>
      <span className="tp-nav">
        <i className="tp-btn" />
        <i className="tp-tab" />
        <i className="tp-tab" />
      </span>
    </span>
  );
}
