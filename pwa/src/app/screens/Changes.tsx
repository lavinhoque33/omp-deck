// A project's git changes: the file list, diffs with review comments (line, range, general →
// insert or send to omp), and stage/unstage/discard/delete untracked with confirmation sheets
// for the destructive ones.

import { type ReactNode, useEffect, useState } from 'react';
import type { api } from '../../remote/index.ts';
import { ClientError } from '../client.ts';
import { useNav } from '../nav.ts';
import { perform, succeeded } from '../ui/format.ts';
import { useCan, useClient, useFeed, useProjectDisplay } from '../ui/hooks.ts';
import { ConfirmSheet, Empty, Sheet, StaleBanner, useS4 } from '../ui/kit.tsx';
import '../../styles/convo.css';
import { ReviewDiff, ReviewSheet, useReview } from './conversation/Review.tsx';
import { reviewCount } from './conversation/review.ts';

const KIND_LETTER: Record<api.ChangeKind, string> = {
  modified: 'M',
  added: 'A',
  deleted: 'D',
  renamed: 'R',
  copied: 'C',
  type_changed: 'T',
};

function sides(file: api.ChangedFile): api.DiffSide[] {
  const out: api.DiffSide[] = [];
  if (file.unstaged !== null) out.push('unstaged');
  if (file.staged !== null) out.push('staged');
  if (file.untracked) out.push('untracked');
  return out;
}

export function ChangesScreen({ project, display: routeDisplay }: { project: string; display: string }): ReactNode {
  const { text: display, resolved } = useProjectDisplay(project, routeDisplay);
  const client = useClient();
  const nav = useNav();
  const can = useCan();
  const feed = useFeed({ feed: 'changes', project }, 'changes');
  const [selected, setSelected] = useState<ReadonlySet<string>>(new Set());
  const [diffOf, setDiffOf] = useState<api.ChangedFile | null>(null);
  const [confirm, setConfirm] = useState<'discard' | 'delete' | null>(null);
  const [busy, setBusy] = useState(false);
  const [reviewOpen, setReviewOpen] = useState(false);
  const s4 = useS4();
  const review = useReview(project);
  const comments = reviewCount(review);
  const files = feed?.changes?.files ?? [];
  const chosen = files.filter((file) => selected.has(file.path));
  const tracked = chosen.filter((file) => !file.untracked);
  const untracked = chosen.filter((file) => file.untracked);

  // Drop selections of files that left the list.
  useEffect(() => {
    const present = new Set(files.map((file) => file.path));
    if ([...selected].some((path) => !present.has(path))) setSelected(new Set([...selected].filter((path) => present.has(path))));
  }, [feed]);

  const toggle = (path: string) => {
    const next = new Set(selected);
    if (next.has(path)) next.delete(path);
    else next.add(path);
    setSelected(next);
  };
  const run = async (op: api.Op, label: string) => {
    setBusy(true);
    const view = await perform(client, op, label);
    setBusy(false);
    setConfirm(null);
    if (succeeded(view)) setSelected(new Set());
  };
  const paths = (list: readonly api.ChangedFile[]) => list.map((file) => file.path);
  const permitted = can({ op: 'stage', project, files: [] });

  return (
    <div className="screen pushed">
      <header className="top">
        <div className="eyebrow">Changes</div>
        <div className="line">
          <button type="button" className="back" onClick={nav.pop} aria-label="Back">
            ‹
          </button>
          <h2 className={`grow${resolved ? '' : ' muted'}`}>{display}</h2>
        </div>
        <div className="line wrap" style={{ marginTop: 8 }}>
          {feed?.changes?.branch != null && <span className="pill">⎇ {feed.changes.branch}</span>}
          {s4 && (
            <button type="button" className={`pill review-pill${comments > 0 ? ' has' : ''}`} onClick={() => setReviewOpen(true)}>
              ✎ Review{comments > 0 ? ` · ${comments}` : ''}
            </button>
          )}
        </div>
      </header>
      <div className="body" style={{ paddingBottom: 120 }}>
        <StaleBanner />
        {feed === undefined ? (
          <Empty title="Loading changes…" />
        ) : feed.error !== null ? (
          <Empty title="No changes available">{feed.error}</Empty>
        ) : files.length === 0 ? (
          <Empty title="Working tree clean">Nothing to stage or discard.</Empty>
        ) : (
          files.map((file) => {
            const isSelected = selected.has(file.path);
            return (
              <div key={file.path} className={`session${isSelected ? ' selected' : ''}${file.conflicted ? ' attention' : ''}`}>
                <button type="button" className="check" aria-pressed={isSelected} onClick={() => toggle(file.path)} aria-label={`Select ${file.path}`}>
                  {isSelected ? '✓' : ''}
                </button>
                <button type="button" className="copy" style={{ textAlign: 'left' }} onClick={() => setDiffOf(file)}>
                  <b className="mono">{file.path}</b>
                  {file.orig_path !== null && <small>from {file.orig_path}</small>}
                </button>
                <span className="line">
                  {file.conflicted && <span className="chip danger">conflict</span>}
                  {file.staged !== null && <span className="chip ok">S·{KIND_LETTER[file.staged]}</span>}
                  {file.unstaged !== null && <span className="chip warn">W·{KIND_LETTER[file.unstaged]}</span>}
                  {file.untracked && <span className="chip accent">new</span>}
                </span>
              </div>
            );
          })
        )}
        {feed?.truncated === true && <p className="muted small">The list was cut; only the first files are shown.</p>}
      </div>
      <div className="fab bulk-bar" role="toolbar" aria-label="Selected files">
        <span>{chosen.length} selected</span>
        <button
          type="button"
          className="button"
          disabled={chosen.length === 0 || !permitted || busy}
          onClick={() => void run({ op: 'stage', project, files: paths(chosen) }, `Stage ${chosen.length} file${chosen.length === 1 ? '' : 's'}`)}
        >
          Stage
        </button>
        <button
          type="button"
          className="button"
          disabled={chosen.every((file) => file.staged === null) || !permitted || busy}
          onClick={() =>
            void run(
              { op: 'unstage', project, files: paths(chosen.filter((file) => file.staged !== null)) },
              'Unstage files',
            )
          }
        >
          Unstage
        </button>
        <button type="button" className="button danger" disabled={tracked.length === 0 || !permitted || busy} onClick={() => setConfirm('discard')}>
          Discard
        </button>
        <button type="button" className="button danger" disabled={untracked.length === 0 || !permitted || busy} onClick={() => setConfirm('delete')}>
          Delete
        </button>
      </div>
      {confirm === 'discard' && (
        <ConfirmSheet
          title={`Discard changes in ${tracked.length} file${tracked.length === 1 ? '' : 's'}?`}
          message="Their uncommitted edits are thrown away on the desktop. This cannot be undone."
          facts={[
            ['Files', tracked.length],
            ...tracked.slice(0, 5).map((file, index) => [`#${index + 1}`, file.path] as const),
            ...(tracked.length > 5 ? [['More', `${tracked.length - 5} others`] as const] : []),
          ]}
          confirm="Discard"
          busy={busy}
          onConfirm={() => void run({ op: 'discard', project, files: paths(tracked) }, 'Discard changes')}
          onCancel={() => setConfirm(null)}
        />
      )}
      {confirm === 'delete' && (
        <ConfirmSheet
          title={`Delete ${untracked.length} untracked file${untracked.length === 1 ? '' : 's'}?`}
          message="They are removed from disk on the desktop. This cannot be undone."
          facts={[
            ['Files', untracked.length],
            ...untracked.slice(0, 5).map((file, index) => [`#${index + 1}`, file.path] as const),
            ...(untracked.length > 5 ? [['More', `${untracked.length - 5} others`] as const] : []),
          ]}
          confirm="Delete"
          busy={busy}
          onConfirm={() => void run({ op: 'delete_untracked', project, files: paths(untracked) }, 'Delete untracked files')}
          onCancel={() => setConfirm(null)}
        />
      )}
      {diffOf !== null && (
        <DiffSheet
          project={project}
          file={diffOf}
          onClose={() => setDiffOf(null)}
          onReview={() => {
            setDiffOf(null);
            setReviewOpen(true);
          }}
        />
      )}
      {reviewOpen && <ReviewSheet project={project} display={resolved ? display : 'this folder'} onClose={() => setReviewOpen(false)} />}
    </div>
  );
}

function DiffSheet({
  project,
  file,
  onClose,
  onReview,
}: {
  project: string;
  file: api.ChangedFile;
  onClose: () => void;
  onReview: () => void;
}): ReactNode {
  const s4 = useS4();
  const comments = reviewCount(useReview(project));
  const client = useClient();
  const available = sides(file);
  const [side, setSide] = useState<api.DiffSide>(available[0] ?? 'unstaged');
  const [diff, setDiff] = useState<api.Diff | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    setDiff(null);
    setError(null);
    client.request({ op: 'diff', project, file: file.path, side }).then(
      (resp) => {
        if (live && resp.type === 'diff') setDiff(resp.diff);
      },
      (reason: unknown) => {
        if (live) setError(reason instanceof ClientError || reason instanceof Error ? reason.message : String(reason));
      },
    );
    return () => {
      live = false;
    };
  }, [client, project, file.path, side]);
  return (
    <Sheet title={file.path} icon="±" onClose={onClose}>
      {available.length > 1 && (
        <div className="segmented" style={{ marginBottom: 10 }}>
          {available.map((entry) => (
            <button key={entry} type="button" aria-pressed={side === entry} onClick={() => setSide(entry)}>
              {entry}
            </button>
          ))}
        </div>
      )}
      {error !== null ? (
        <p>{error}</p>
      ) : diff === null ? (
        <p>Loading the diff…</p>
      ) : diff.binary ? (
        <p>Binary file; no text diff.</p>
      ) : (
        <>
          <div className="line" style={{ marginBottom: 8 }}>
            <span className="chip ok">+{diff.added}</span>
            <span className="chip danger">−{diff.removed}</span>
            {diff.truncated && <span className="chip warn">cut</span>}
            <span className="grow" />
            {s4 && (
              <button type="button" className="chip" onClick={onReview}>
                ✎ Review · {comments}
              </button>
            )}
          </div>
          <ReviewDiff project={project} file={file.path} side={side} text={diff.text} />
        </>
      )}
    </Sheet>
  );
}
