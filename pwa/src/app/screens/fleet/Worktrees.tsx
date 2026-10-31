// Worktrees of one project (`worktrees{project}`): the repository's state, create a worktree
// (branch, base, open a session), move a running session into a new worktree, finish (the
// gateway's commit → merge → remove flow, with progress), remove or park (confirmations that say
// what happens), and continue parked branches (new worktree, main checkout, or switch back).

import { type ReactNode, useState } from 'react';
import type { api } from '../../../remote/index.ts';
import { isFinal } from '../../client.ts';
import { useNav } from '../../nav.ts';
import { age, perform, requestStatusText, succeeded } from '../../ui/format.ts';
import { useCan, useClient, useClientState, useFeed, useNow, useProjectDisplay } from '../../ui/hooks.ts';
import { ConfirmSheet, Empty, NeedsS4, Sheet, StaleBanner, Switch, useS4 } from '../../ui/kit.tsx';
import { doneResult } from './common.ts';
import { branchProblem, lastPart, plural } from './logic.ts';
import '../../../styles/fleet.css';

type Pending =
  | { kind: 'create' }
  | { kind: 'move' }
  | { kind: 'finish' }
  | { kind: 'remove'; park: boolean }
  | { kind: 'continue'; parked: api.ParkedView };

export function WorktreesScreen({ project, display: routeDisplay }: { project: string; display: string }): ReactNode {
  const { text: display, resolved } = useProjectDisplay(project, routeDisplay);
  const client = useClient();
  const nav = useNav();
  const can = useCan();
  const s4 = useS4();
  const now = useNow();
  const feed = useFeed(s4 ? { feed: 'worktrees', project } : null, 'worktrees');
  const liveFeed = useFeed({ feed: 'live' }, 'live');
  const requests = useClientState((state) => state.requests);
  const [pending, setPending] = useState<Pending | null>(null);
  const [busy, setBusy] = useState(false);

  const wt = feed?.worktree ?? null;
  const here = (liveFeed?.sessions ?? []).filter((live) => live.project === project && live.status.state !== 'exited');
  const movable = here.filter((live) => live.kind === 'session' && live.live_control);
  const finishing = requests.find(
    (request) => request.op.op === 'worktree_finish' && request.op.project === project && !isFinal(request.status),
  );

  const run = async (op: api.Op, label: string, success?: string): Promise<api.DoneView | null> => {
    setBusy(true);
    const view = await perform(client, op, label, success);
    setBusy(false);
    if (!succeeded(view)) return null;
    setPending(null);
    return doneResult(view);
  };
  const openSession = (result: api.DoneView | null, title: string) => {
    if (result?.type === 'session') nav.push({ kind: 'conversation', session: null, liveKey: result.live.key, title });
  };

  return (
    <div className="screen pushed worktrees">
      <header className="top">
        <div className="eyebrow">Worktrees</div>
        <div className="line">
          <button type="button" className="back" onClick={nav.pop} aria-label="Back">
            ‹
          </button>
          <h2 className={`grow ellipsis${resolved ? '' : ' muted'}`}>{lastPart(display)}</h2>
          {wt?.branch != null && <span className="chip accent">⎇ {wt.branch}</span>}
        </div>
      </header>
      <div className="body">
        <StaleBanner />
        <NeedsS4 what="Worktrees">
          {feed === undefined ? (
            <Empty title="Loading…" />
          ) : wt === null ? (
            <Empty title="Not a git repository">{resolved ? display : 'This folder'} has no git repository, so it has no worktrees.</Empty>
          ) : (
            <>
              <section className="card wt-card">
                <header>
                  <span className="title">{wt.linked ? 'Linked worktree' : 'Main checkout'}</span>
                  <span className={`chip ${wt.linked ? 'accent' : 'ok'}`}>{wt.linked ? 'worktree' : 'repository'}</span>
                </header>
                <div className="card-body">
                  <dl className="facts wide">
                    <div>
                      <dt>Folder</dt>
                      <dd className={`mono${resolved ? '' : ' muted'}`}>{display}</dd>
                    </div>
                    <div>
                      <dt>Branch</dt>
                      <dd>{wt.branch === null ? <span className="chip warn">detached</span> : <span className="chip accent">⎇ {wt.branch}</span>}</dd>
                    </div>
                    {wt.linked && (
                      <div>
                        <dt>Merges into</dt>
                        <dd>{wt.base === null ? <span className="muted">not recorded</span> : <span className="chip">{wt.base}</span>}</dd>
                      </div>
                    )}
                    {wt.linked && (
                      <div>
                        <dt>Main checkout</dt>
                        <dd className="mono">{wt.primary_display}</dd>
                      </div>
                    )}
                    <div>
                      <dt>Running here</dt>
                      <dd>{plural(here.length, 'session')}</dd>
                    </div>
                  </dl>
                  {wt.linked && wt.primary !== null && (
                    <button
                      type="button"
                      className="button ghost block"
                      onClick={() => nav.replace({ kind: 'worktrees', project: wt.primary ?? '', display: wt.primary_display })}
                    >
                      Open the main checkout ›
                    </button>
                  )}
                </div>
              </section>

              {finishing !== undefined && (
                <div className="banner progress-banner" role="status">
                  <span className="busy" aria-hidden="true" />
                  <span className="grow">
                    <b>Finishing</b> · {finishing.progress?.stage ?? requestStatusText(finishing)}
                  </span>
                  <button
                    type="button"
                    className="button"
                    onClick={() => void perform(client, { op: 'cancel', seq: finishing.seq }, 'Cancel finishing', 'Finishing cancelled.')}
                  >
                    Cancel
                  </button>
                </div>
              )}

              <div className="action-grid">
                <ActionTile
                  icon="+"
                  title="New worktree"
                  hint="A branch in its own folder"
                  disabled={!can({ op: 'worktree_create', project, branch: 'x', open: true })}
                  onClick={() => setPending({ kind: 'create' })}
                />
                <ActionTile
                  icon="⇢"
                  title="Move session"
                  hint={movable.length === 0 ? 'No controllable session runs here' : 'Into a new worktree'}
                  disabled={movable.length === 0}
                  onClick={() => setPending({ kind: 'move' })}
                />
                {wt.linked && (
                  <ActionTile
                    icon="✓"
                    title="Finish"
                    hint="Commit, merge, then remove"
                    disabled={finishing !== undefined || !can({ op: 'worktree_finish', project })}
                    onClick={() => setPending({ kind: 'finish' })}
                  />
                )}
                {wt.linked && (
                  <ActionTile
                    icon="⏸"
                    title="Park"
                    hint="Keep the branch, drop the folder"
                    disabled={!can({ op: 'worktree_remove', project, park: true })}
                    onClick={() => setPending({ kind: 'remove', park: true })}
                  />
                )}
                {wt.linked && (
                  <ActionTile
                    icon="⌫"
                    title="Remove"
                    hint="Merged work only"
                    danger
                    disabled={!can({ op: 'worktree_remove', project, park: false })}
                    onClick={() => setPending({ kind: 'remove', park: false })}
                  />
                )}
              </div>

              <h3 className="section-title">
                Parked branches <span className="count">{feed.parked.length}</span>
              </h3>
              {feed.parked.length === 0 ? (
                <p className="muted small">Parking a worktree keeps its branch here to continue later.</p>
              ) : (
                feed.parked.map((parked) => (
                  <article key={parked.id} className="fleet-card parked">
                    <div className="line spread">
                      <span className="chip accent">⎇ {parked.branch}</span>
                      <span className="muted small">parked {age(parked.parked_unix_ms, now)} ago</span>
                    </div>
                    <div className="badges">
                      <span className="chip">from {parked.base}</span>
                      <span className="chip">{plural(parked.sessions, 'session')}</span>
                      {parked.merged && <span className="chip ok">merged</span>}
                      {parked.checked_out && <span className="chip warn">checked out</span>}
                      {!parked.exists && <span className="chip danger">branch gone</span>}
                    </div>
                    <button
                      type="button"
                      className="button block"
                      disabled={!parked.exists}
                      onClick={() => setPending({ kind: 'continue', parked })}
                    >
                      Continue…
                    </button>
                  </article>
                ))
              )}
              {feed.truncated && <p className="muted small">Only part of the list was sent.</p>}
            </>
          )}
        </NeedsS4>
      </div>

      {pending?.kind === 'create' && wt !== null && (
        <CreateSheet
          wt={wt}
          busy={busy}
          onClose={() => setPending(null)}
          onCreate={async (branch, base, open) =>
            openSession(
              await run({ op: 'worktree_create', project, branch, base, open }, `New worktree ${branch}`, open ? undefined : `Created worktree ${branch}.`),
              branch,
            )
          }
        />
      )}
      {pending?.kind === 'move' && (
        <MoveSheet
          sessions={movable}
          busy={busy}
          onClose={() => setPending(null)}
          onMove={(live, branch) =>
            void run({ op: 'worktree_move', live: live.live, branch }, `Move “${live.title ?? live.label}” to ${branch}`, `Moving into worktree ${branch}.`)
          }
        />
      )}
      {pending?.kind === 'finish' && wt !== null && (
        <Sheet title="Finish this worktree?" icon="✓" onClose={() => setPending(null)}>
          <p className="sheet-lead">
            The worktree’s session is asked to commit and merge <b>{wt.branch ?? 'its branch'}</b> into <b>{wt.base ?? 'its base'}</b>. After
            each of its turns the worktree is checked; once clean and merged it is removed and its sessions move to the main checkout.
          </p>
          <dl className="facts wide">
            <div>
              <dt>Branch</dt>
              <dd>{wt.branch ?? '—'}</dd>
            </div>
            <div>
              <dt>Merges into</dt>
              <dd>{wt.base ?? 'not recorded'}</dd>
            </div>
            <div>
              <dt>Running here</dt>
              <dd>{plural(here.length, 'session')}</dd>
            </div>
          </dl>
          <div className="buttons pair">
            <button type="button" className="button" onClick={() => setPending(null)}>
              Not now
            </button>
            <button
              type="button"
              className="button primary"
              disabled={busy}
              onClick={() => {
                setPending(null);
                void perform(client, { op: 'worktree_finish', project }, `Finish ${wt.branch ?? 'worktree'}`, 'The worktree is finished.');
              }}
            >
              Finish
            </button>
          </div>
        </Sheet>
      )}
      {pending?.kind === 'remove' && wt !== null && (
        <ConfirmSheet
          title={pending.park ? `Park ${wt.branch ?? 'this worktree'}?` : `Remove ${wt.branch ?? 'this worktree'}?`}
          message={
            pending.park
              ? 'The worktree folder is deleted; the branch is kept (it must be clean and pushed) and listed under Parked branches to continue later. Running sessions move to the main checkout, the rest are stopped.'
              : 'The worktree folder and its merged branch are deleted (it must be clean and merged). Running sessions move to the main checkout, the rest are stopped; session files move there too.'
          }
          facts={[
            ['Folder deleted', resolved ? display : '—'],
            ['Branch', pending.park ? `${wt.branch ?? '—'} (kept)` : `${wt.branch ?? '—'} (deleted)`],
            ['Sessions affected', here.length],
            ['Sessions go to', wt.primary_display],
          ]}
          confirm={pending.park ? 'Park' : 'Remove'}
          busy={busy}
          onConfirm={() =>
            void run(
              { op: 'worktree_remove', project, park: pending.park },
              `${pending.park ? 'Park' : 'Remove'} ${wt.branch ?? 'worktree'}`,
              pending.park ? 'Parked; the branch is kept.' : 'Worktree removed.',
            ).then((result) => {
              if (result !== null && wt.primary !== null) nav.replace({ kind: 'worktrees', project: wt.primary, display: wt.primary_display });
            })
          }
          onCancel={() => setPending(null)}
        />
      )}
      {pending?.kind === 'continue' && wt !== null && (
        <ContinueSheet
          parked={pending.parked}
          primary={wt.linked ? wt.primary_display : resolved ? display : 'this folder'}
          busy={busy}
          onClose={() => setPending(null)}
          onContinue={async (where) =>
            openSession(
              await run(
                { op: 'worktree_continue', parked: pending.parked.id, where },
                `Continue ${pending.parked.branch}`,
                where === 'switch_back' ? `Switched back to ${pending.parked.branch}.` : undefined,
              ),
              pending.parked.branch,
            )
          }
        />
      )}
    </div>
  );
}

function ActionTile({
  icon,
  title,
  hint,
  danger = false,
  disabled,
  onClick,
}: {
  icon: string;
  title: string;
  hint: string;
  danger?: boolean;
  disabled: boolean;
  onClick: () => void;
}): ReactNode {
  return (
    <button type="button" className={`action-tile${danger ? ' danger' : ''}`} disabled={disabled} onClick={onClick}>
      <span className="tile-icon" aria-hidden="true">
        {icon}
      </span>
      <b>{title}</b>
      <small>{hint}</small>
    </button>
  );
}

function BranchField({ value, onChange }: { value: string; onChange: (value: string) => void }): ReactNode {
  const problem = value.length === 0 ? null : branchProblem(value);
  return (
    <label className="field">
      <span>Branch name</span>
      <input
        className="input mono"
        value={value}
        autoCapitalize="off"
        autoCorrect="off"
        spellCheck={false}
        placeholder="feat/my-change"
        onChange={(event) => onChange(event.target.value)}
      />
      {problem !== null && <small className="refusal">{problem}</small>}
    </label>
  );
}

function CreateSheet({
  wt,
  busy,
  onClose,
  onCreate,
}: {
  wt: api.WorktreeView;
  busy: boolean;
  onClose: () => void;
  onCreate: (branch: string, base: string | null, open: boolean) => void;
}): ReactNode {
  const [branch, setBranch] = useState('');
  const [base, setBase] = useState<string | null>(null);
  const [open, setOpen] = useState(true);
  return (
    <Sheet title="New worktree" icon="⎇" onClose={onClose}>
      <BranchField value={branch} onChange={setBranch} />
      <label className="field">
        <span>Start from</span>
        <select className="input" value={base ?? ''} onChange={(event) => setBase(event.target.value === '' ? null : event.target.value)}>
          <option value="">{wt.branch === null ? 'Current commit (HEAD)' : `Current branch (${wt.branch})`}</option>
          {wt.branches.map((name) => (
            <option key={name} value={name}>
              {name}
            </option>
          ))}
        </select>
      </label>
      {!wt.has_commits && <p className="refusal">The repository has no commits yet; make a first commit on the desktop.</p>}
      <div className="switch-row">
        <span className="grow">
          <b>Open a session in it</b>
          <small>Starts omp in the new folder</small>
        </span>
        <Switch on={open} label="Open a session in it" onChange={setOpen} />
      </div>
      <div className="buttons pair">
        <button type="button" className="button" onClick={onClose}>
          Cancel
        </button>
        <button
          type="button"
          className="button primary"
          disabled={busy || branchProblem(branch) !== null || !wt.has_commits}
          onClick={() => onCreate(branch.trim(), base, open)}
        >
          {busy ? 'Creating…' : 'Create'}
        </button>
      </div>
    </Sheet>
  );
}

function MoveSheet({
  sessions,
  busy,
  onClose,
  onMove,
}: {
  sessions: readonly api.LiveSession[];
  busy: boolean;
  onClose: () => void;
  onMove: (live: api.LiveSession, branch: string) => void;
}): ReactNode {
  const [chosen, setChosen] = useState<string | null>(sessions.length === 1 ? (sessions[0]?.live.key ?? null) : null);
  const [branch, setBranch] = useState('');
  const live = sessions.find((entry) => entry.live.key === chosen) ?? null;
  return (
    <Sheet title="Move a session to a new worktree" icon="⇢" onClose={onClose}>
      <p className="sheet-lead">omp’s /wt moves the running session into a new worktree on this branch; its current branch becomes the merge target.</p>
      <div className="option-list">
        {sessions.map((entry) => (
          <button
            key={entry.live.key}
            type="button"
            className="option"
            aria-pressed={entry.live.key === chosen}
            onClick={() => setChosen(entry.live.key)}
          >
            <span className="grow">
              {entry.title ?? entry.label}
              <small>{entry.busy ? 'Working — moves after this turn' : 'Idle'}</small>
            </span>
          </button>
        ))}
      </div>
      <BranchField value={branch} onChange={setBranch} />
      <div className="buttons pair">
        <button type="button" className="button" onClick={onClose}>
          Cancel
        </button>
        <button
          type="button"
          className="button primary"
          disabled={busy || live === null || branchProblem(branch) !== null}
          onClick={() => live !== null && onMove(live, branch.trim())}
        >
          {busy ? 'Moving…' : 'Move'}
        </button>
      </div>
    </Sheet>
  );
}

const CONTINUE: readonly (readonly [api.ContinueWhere, string, string])[] = [
  ['worktree', 'In a new worktree', 'Recreates a worktree for the branch and opens its newest session there.'],
  ['main', 'In the main checkout', 'Checks the branch out in the main checkout and opens its newest session there.'],
  ['switch_back', 'Switch the main checkout back', 'The main checkout switches to the branch; the parked entry stays, no session opens.'],
];

function ContinueSheet({
  parked,
  primary,
  busy,
  onClose,
  onContinue,
}: {
  parked: api.ParkedView;
  primary: string;
  busy: boolean;
  onClose: () => void;
  onContinue: (where: api.ContinueWhere) => void;
}): ReactNode {
  const [where, setWhere] = useState<api.ContinueWhere>('worktree');
  return (
    <Sheet title={`Continue ${parked.branch}`} icon="▶" onClose={onClose}>
      <p className="sheet-lead">
        Main checkout: <span className="mono">{primary}</span>. A checkout with uncommitted changes is refused; nothing is discarded.
      </p>
      <div className="option-list">
        {CONTINUE.map(([id, title, hint]) => (
          <button key={id} type="button" className="option" aria-pressed={where === id} onClick={() => setWhere(id)}>
            <span className="grow">
              {title}
              <small>{hint}</small>
            </span>
          </button>
        ))}
      </div>
      {parked.checked_out && where === 'worktree' && <p className="refusal">The branch is checked out somewhere already; git refuses a second worktree.</p>}
      <div className="buttons pair">
        <button type="button" className="button" onClick={onClose}>
          Cancel
        </button>
        <button type="button" className="button primary" disabled={busy} onClick={() => onContinue(where)}>
          {busy ? 'Working…' : where === 'switch_back' ? 'Switch back' : 'Continue'}
        </button>
      </div>
    </Sheet>
  );
}
