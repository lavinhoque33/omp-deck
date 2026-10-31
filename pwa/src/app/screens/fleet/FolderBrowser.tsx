// The desktop-folder browser (`browse`): roots first, then sub-folders with repo/project badges,
// a breadcrumb that follows `parent`. Two uses: the pushed Browse screen (new session in a folder,
// open a project's worktrees) and `FolderPicker`, a sheet that hands a folder's dir token back
// (sync add, restore target). Dir tokens are per connection; a stale one starts over at the roots.

import { type ReactNode, useCallback, useEffect, useState } from 'react';
import type { api } from '../../../remote/index.ts';
import { ClientError } from '../../client.ts';
import { useNav } from '../../nav.ts';
import { perform } from '../../ui/format.ts';
import { useCan, useClient, useClientState } from '../../ui/hooks.ts';
import { Empty, NeedsS4, Sheet, StaleBanner, useS4 } from '../../ui/kit.tsx';
import { doneResult, errorText } from './common.ts';
import { type Crumb, lastPart, pushCrumb } from './logic.ts';
import '../../../styles/fleet.css';

type Listing = Extract<api.Resp, { type: 'dir' }>;

const ROOTS: Crumb = { dir: null, display: 'Folders' };

interface Browser {
  trail: readonly Crumb[];
  current: Crumb;
  /** The current folder's listing, once it arrived. */
  listing: Listing | null;
  error: { text: string; stale: boolean } | null;
  loading: boolean;
  open(crumb: Crumb): void;
  up(): void;
  restart(): void;
}

/** The folder trail and the current listing; reloads after a reconnect (tokens are per connection). */
function useBrowser(): Browser {
  const client = useClient();
  const s4 = useS4();
  const epoch = useClientState((state) => state.welcome?.epoch ?? null);
  const online = useClientState((state) => state.conn.phase === 'online');
  const [trail, setTrail] = useState<readonly Crumb[]>([ROOTS]);
  const [listing, setListing] = useState<Listing | null>(null);
  const [error, setError] = useState<{ text: string; stale: boolean } | null>(null);
  const [loading, setLoading] = useState(false);
  const current = trail.at(-1) ?? ROOTS;

  useEffect(() => {
    if (!online || !s4) return;
    let cancelled = false;
    setLoading(true);
    setError(null);
    client.request({ op: 'browse', dir: current.dir }).then(
      (resp) => {
        if (cancelled || resp.type !== 'dir') return;
        setListing(resp);
        setLoading(false);
        setTrail((now) => {
          const last = now.at(-1);
          if (last === undefined || last.dir !== resp.dir || last.dir === null || last.display === resp.display) return now;
          return [...now.slice(0, -1), { dir: resp.dir, display: resp.display }];
        });
      },
      (failure: unknown) => {
        if (cancelled) return;
        setLoading(false);
        const stale = failure instanceof ClientError && failure.api?.code === 'not_found';
        setError({ text: stale ? 'That folder link expired after a reconnect.' : errorText(failure), stale });
      },
    );
    return () => {
      cancelled = true;
    };
  }, [client, online, s4, current.dir, epoch]);

  // A new gateway connection forgets every dir token.
  useEffect(() => {
    setTrail([ROOTS]);
  }, [epoch]);

  const open = useCallback((crumb: Crumb) => setTrail((now) => pushCrumb(now, crumb)), []);
  const up = useCallback(() => {
    setTrail((now) => {
      const parent = listing?.parent ?? null;
      if (parent === null) return now.length > 1 ? now.slice(0, -1) : now;
      const before = now.at(-2);
      if (before?.dir === parent) return now.slice(0, -1);
      return [...now.slice(0, -1), { dir: parent, display: '…' }];
    });
  }, [listing]);
  const restart = useCallback(() => setTrail([ROOTS]), []);
  const shown = listing !== null && listing.dir === current.dir ? listing : null;
  return { trail, current, listing: shown, error, loading, open, up, restart };
}

function Breadcrumb({ browser }: { browser: Browser }): ReactNode {
  return (
    <nav className="crumbs" aria-label="Folder path">
      {browser.trail.map((crumb, index) => {
        const last = index === browser.trail.length - 1;
        return (
          <span key={crumb.dir ?? 'roots'} className="crumb-wrap">
            {index > 0 && <span className="crumb-sep">›</span>}
            <button type="button" className={`crumb${last ? ' here' : ''}`} disabled={last} onClick={() => browser.open(crumb)}>
              {crumb.dir === null ? '⌂ Folders' : lastPart(crumb.display)}
            </button>
          </span>
        );
      })}
    </nav>
  );
}

function FolderRows({
  browser,
  actions,
}: {
  browser: Browser;
  /** Trailing per-row buttons (hover buttons on the row itself). */
  actions?: (entry: api.DirEntry) => ReactNode;
}): ReactNode {
  const { listing, error, loading } = browser;
  if (error !== null) {
    return (
      <Empty title={error.stale ? 'Folder link expired' : 'Could not list this folder'}>
        <span>{error.text}</span>
        <button type="button" className="button" style={{ marginTop: 10 }} onClick={browser.restart}>
          Start over at the top
        </button>
      </Empty>
    );
  }
  if (listing === null) return <Empty title={loading ? 'Listing folders…' : 'Waiting for the desktop…'} />;
  return (
    <>
      {listing.dir !== null && (
        <button type="button" className="session folder up" onClick={browser.up}>
          <span className="folder-icon" aria-hidden="true">
            ↰
          </span>
          <span className="copy">
            <b>Up one level</b>
            <small>{listing.parent === null ? 'Back to the top' : 'Parent folder'}</small>
          </span>
        </button>
      )}
      {listing.entries.length === 0 ? (
        <Empty title="No sub-folders">{listing.dir === null ? 'The desktop shares no folders.' : 'This folder has no sub-folders.'}</Empty>
      ) : (
        listing.entries.map((entry) => (
          <div key={entry.dir} className={`session folder${entry.repo ? ' repo' : ''}`}>
            <button
              type="button"
              className="folder-main"
              onClick={() =>
                browser.open({
                  dir: entry.dir,
                  display: listing.dir === null ? entry.name : `${listing.display.replace(/[\\/]$/, '')}/${entry.name}`,
                })
              }
            >
              <span className="folder-icon" aria-hidden="true">
                {entry.repo ? '⎇' : '▸'}
              </span>
              <span className="copy">
                <b>{entry.name}</b>
                <span className="badges">
                  {entry.repo && <span className="chip accent">git repo</span>}
                  {entry.project !== null && <span className="chip ok">omp project</span>}
                </span>
              </span>
              <span className="go" aria-hidden="true">
                ›
              </span>
            </button>
            {actions?.(entry)}
          </div>
        ))
      )}
      {listing.truncated && <p className="muted small">Only the first folders are listed.</p>}
    </>
  );
}

/** The pushed Browse screen: open a session in any desktop folder. */
export function BrowseScreen(): ReactNode {
  const client = useClient();
  const nav = useNav();
  const can = useCan();
  const browser = useBrowser();
  const [busy, setBusy] = useState<string | null>(null);
  const canNew = can({ op: 'new_session_in', dir: 'f-0000000000000000' });

  const newSessionIn = async (dir: string, display: string) => {
    setBusy(dir);
    const view = await perform(client, { op: 'new_session_in', dir }, `New session in ${display}`);
    setBusy(null);
    const result = doneResult(view);
    if (result?.type === 'session') nav.push({ kind: 'conversation', session: null, liveKey: result.live.key, title: lastPart(display) });
  };

  const here = browser.listing;
  return (
    <div className="screen pushed folders">
      <header className="top">
        <div className="eyebrow">Desktop folders</div>
        <div className="line">
          <button type="button" className="back" onClick={nav.pop} aria-label="Back">
            ‹
          </button>
          <h2 className="grow">Browse</h2>
        </div>
        <Breadcrumb browser={browser} />
      </header>
      <div className="body">
        <StaleBanner />
        <NeedsS4 what="The folder browser">
          {here !== null && here.dir !== null && (
            <div className="fleet-card here-card">
              <div className="copy">
                <span className="eyebrow">This folder</span>
                <b className="mono">{here.display}</b>
              </div>
              <button
                type="button"
                className="button primary"
                disabled={!canNew || busy !== null}
                onClick={() => void newSessionIn(here.dir ?? '', here.display)}
              >
                {busy === here.dir ? 'Starting…' : '+ New session here'}
              </button>
            </div>
          )}
          <FolderRows
            browser={browser}
            actions={(entry) => (
              <span className="folder-actions">
                {entry.project !== null && (
                  <button
                    type="button"
                    className="row-btn"
                    aria-label={`Worktrees of ${entry.name}`}
                    title="Worktrees"
                    onClick={() => nav.push({ kind: 'worktrees', project: entry.project ?? '', display: entry.name })}
                  >
                    ⎇
                  </button>
                )}
                <button
                  type="button"
                  className="row-btn"
                  aria-label={`New session in ${entry.name}`}
                  title="New session here"
                  disabled={!canNew || busy !== null}
                  onClick={() => void newSessionIn(entry.dir, entry.name)}
                >
                  {busy === entry.dir ? '…' : '+'}
                </button>
              </span>
            )}
          />
        </NeedsS4>
      </div>
    </div>
  );
}

/**
 * A folder picker sheet: browse, then "Use this folder" hands back the open folder's dir token and
 * its `~` display. Picking at the roots level is not possible (the roots are a list, not a folder).
 */
export function FolderPicker({
  title,
  confirm = 'Use this folder',
  busy = false,
  onPick,
  onClose,
}: {
  title: string;
  confirm?: string;
  busy?: boolean;
  onPick: (dir: string, display: string) => void;
  onClose: () => void;
}): ReactNode {
  const browser = useBrowser();
  const here = browser.listing;
  return (
    <Sheet title={title} icon="▣" onClose={onClose}>
      <NeedsS4 what="The folder browser">
        <Breadcrumb browser={browser} />
        <div className="picker-list">
          <FolderRows browser={browser} />
        </div>
        <div className="picker-foot">
          <span className="muted small grow">{here?.dir != null ? here.display : 'Open a folder to choose it.'}</span>
          <button
            type="button"
            className="button primary"
            disabled={busy || here === null || here.dir === null}
            onClick={() => here !== null && here.dir !== null && onPick(here.dir, here.display)}
          >
            {busy ? 'Working…' : confirm}
          </button>
        </div>
      </NeedsS4>
    </Sheet>
  );
}
