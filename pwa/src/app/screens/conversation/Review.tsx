// Review comments in the changes view: a numbered diff where a tap selects a line and a second tap
// extends it to a range, an inline comment editor, and the review sheet (comment list with edit and
// delete, a general comment, then Insert into omp or Send through `review`).

import { type ReactNode, useMemo, useState, useSyncExternalStore } from 'react';
import type { api } from '../../../remote/index.ts';
import { perform, statusLabel, succeeded } from '../../ui/format.ts';
import { useCan, useClient, useFeed } from '../../ui/hooks.ts';
import { ConfirmSheet, Sheet, useS4 } from '../../ui/kit.tsx';
import {
  type DraftComment,
  type ReviewDraft,
  diffRows,
  quoteOf,
  rangeLabel,
  reviewCount,
  reviewPayload,
  reviews,
  rowsOf,
  selectionRange,
} from './review.ts';

/** The project's review draft (re-rendering on change). */
export function useReview(project: string): ReviewDraft {
  return useSyncExternalStore(reviews.subscribe, () => reviews.get(project));
}

export function ReviewDiff({
  project,
  file,
  side,
  text,
}: {
  project: string;
  file: string;
  side: api.DiffSide;
  text: string;
}): ReactNode {
  const s4 = useS4();
  const draft = useReview(project);
  const rows = useMemo(() => diffRows(text), [text]);
  const [anchor, setAnchor] = useState<number | null>(null);
  const [focus, setFocus] = useState<number | null>(null);
  const [editing, setEditing] = useState<{ id: number | null; text: string } | null>(null);
  const mine = useMemo(() => draft.comments.filter((entry) => entry.file === file && entry.side === side), [draft, file, side]);
  const marked = useMemo(() => {
    const out = new Set<number>();
    for (const comment of mine) for (const row of rowsOf(rows, comment)) out.add(row);
    return out;
  }, [mine, rows]);
  const range = anchor === null ? null : selectionRange(rows, anchor, focus ?? anchor);
  const selected = anchor === null ? null : { from: Math.min(anchor, focus ?? anchor), to: Math.max(anchor, focus ?? anchor) };
  const edited = editing?.id == null ? undefined : mine.find((entry) => entry.id === editing.id);
  /** Where the inline editor opens: under the selection's last row, or the edited comment's end row. */
  const editorRow =
    editing === null
      ? null
      : edited === undefined
        ? (selected?.to ?? null)
        : rows.findIndex((row) => row.newLine === edited.end);

  const tap = (index: number) => {
    if (!s4 || rows[index]?.newLine == null) return;
    if (editing !== null) return;
    if (anchor === null || focus !== null) {
      setAnchor(index);
      setFocus(null);
    } else if (index === anchor) {
      setAnchor(null);
    } else {
      setFocus(index);
    }
  };
  const clear = () => {
    setAnchor(null);
    setFocus(null);
    setEditing(null);
  };
  const save = () => {
    if (editing === null) return;
    const body = editing.text.trim();
    if (editing.id !== null) {
      reviews.dispatch(project, body.length === 0 ? { type: 'delete', id: editing.id } : { type: 'edit', id: editing.id, text: body });
    } else if (range !== null && body.length > 0) {
      reviews.dispatch(project, {
        type: 'add',
        comment: { file, side, start: range.start, end: range.end, text: body, quote: quoteOf(rows, range.start, range.end) },
      });
    }
    clear();
  };

  return (
    <>
      {s4 && (
        <p className="muted small review-hint">
          {range === null ? 'Tap a line to comment; tap a second line for a range.' : `Lines ${rangeLabel(range.start, range.end)} selected.`}
        </p>
      )}
      <div className="diff-view review-diff" role="list">
        {rows.map((row, index) => {
          const inSelection = selected !== null && index >= selected.from && index <= selected.to;
          const ending = row.newLine === null ? [] : mine.filter((entry) => entry.end === row.newLine && entry.id !== editing?.id);
          return (
            <div key={index} role="listitem">
              <div
                className={`drow ${row.kind}${inSelection ? ' selected' : ''}${marked.has(index) ? ' commented' : ''}`}
                onClick={() => tap(index)}
              >
                <span className="gutter">{row.oldLine ?? ''}</span>
                <span className="gutter">{row.newLine ?? ''}</span>
                <span className="code">{row.text.length === 0 ? ' ' : row.text}</span>
              </div>
              {ending.map((comment) => (
                <button key={comment.id} type="button" className="inline-comment" onClick={() => setEditing({ id: comment.id, text: comment.text })}>
                  <span className="chip accent">✎ {rangeLabel(comment.start, comment.end)}</span>
                  <span className="grow">{comment.text}</span>
                </button>
              ))}
              {editorRow === index && editing !== null && (
                <div className="comment-editor">
                  <header>
                    <span className="chip accent">
                      {file}:{edited !== undefined ? rangeLabel(edited.start, edited.end) : range !== null ? rangeLabel(range.start, range.end) : ''}
                    </span>
                    <span className="grow" />
                    <span className="muted small">{editing.id === null ? 'New comment' : 'Edit comment'}</span>
                  </header>
                  <textarea
                    className="input"
                    rows={3}
                    autoFocus
                    value={editing.text}
                    placeholder="What should change here?"
                    onChange={(event) => setEditing({ ...editing, text: event.target.value })}
                  />
                  <div className="buttons">
                    <button type="button" className="button primary" disabled={editing.text.trim().length === 0 && editing.id === null} onClick={save}>
                      {editing.id === null ? 'Add comment' : editing.text.trim().length === 0 ? 'Delete' : 'Save'}
                    </button>
                    <button type="button" className="button" onClick={clear}>
                      Cancel
                    </button>
                  </div>
                </div>
              )}
            </div>
          );
        })}
      </div>
      {range !== null && editing === null && (
        <div className="selection-bar">
          <span className="chip accent">Lines {rangeLabel(range.start, range.end)}</span>
          <span className="grow" />
          <button type="button" className="button" onClick={clear}>
            Clear
          </button>
          <button type="button" className="button primary" onClick={() => setEditing({ id: null, text: '' })}>
            ✎ Comment
          </button>
        </div>
      )}
    </>
  );
}

export function ReviewSheet({ project, display, onClose }: { project: string; display: string; onClose: () => void }): ReactNode {
  const client = useClient();
  const can = useCan();
  const draft = useReview(project);
  const live = useFeed({ feed: 'live' }, 'live');
  const targets = (live?.sessions ?? []).filter((entry) => entry.project === project && entry.status.state !== 'exited' && entry.kind === 'session');
  const [targetKey, setTargetKey] = useState<string | null>(null);
  const target = targets.find((entry) => entry.live.key === targetKey) ?? targets[0] ?? null;
  const [editing, setEditing] = useState<{ id: number; text: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirmDiscard, setConfirmDiscard] = useState(false);
  const payload = reviewPayload(draft);
  const count = reviewCount(draft);
  const targetBusy = target !== null && (target.busy || target.status.state === 'busy');

  const submit = async (send: boolean) => {
    if (target === null || payload === null) return;
    setBusy(true);
    const view = await perform(
      client,
      { op: 'review', live: target.live, comments: payload.comments, general: payload.general, send },
      send ? `Send review (${count})` : `Insert review (${count})`,
      send ? 'Review sent to omp.' : 'Review inserted into omp’s input.',
    );
    setBusy(false);
    if (succeeded(view)) {
      reviews.dispatch(project, { type: 'clear' });
      onClose();
    }
  };
  const allowed = target !== null && payload !== null && !busy && can({ op: 'review', live: target.live, comments: [], send: false });

  const byFile = new Map<string, DraftComment[]>();
  for (const comment of draft.comments) byFile.set(comment.file, [...(byFile.get(comment.file) ?? []), comment]);

  if (confirmDiscard) {
    const files = new Set(draft.comments.map((entry) => entry.file));
    return (
      <ConfirmSheet
        title={`Discard ${count} review comment${count === 1 ? '' : 's'}?`}
        message="The draft review for this project is thrown away; nothing reaches omp."
        facts={[
          ['Line comments', draft.comments.length],
          ['Files', files.size],
          ['General comment', draft.general.trim().length > 0 ? 'yes' : 'none'],
        ]}
        confirm="Discard"
        onConfirm={() => {
          reviews.dispatch(project, { type: 'clear' });
          setConfirmDiscard(false);
        }}
        onCancel={() => setConfirmDiscard(false)}
      />
    );
  }

  return (
    <Sheet title={`Review · ${count} comment${count === 1 ? '' : 's'}`} icon="✎" onClose={onClose}>
      <div className="review-sheet">
        {draft.comments.length === 0 && <p className="muted small">No line comments yet: open a file’s diff and tap a line.</p>}
        {[...byFile].map(([file, comments]) => (
          <section key={file} className="review-file">
            <header className="mono">{file}</header>
            {comments.map((comment) =>
              editing?.id === comment.id ? (
                <div key={comment.id} className="review-item editing">
                  <span className="chip accent">{rangeLabel(comment.start, comment.end)}</span>
                  <textarea className="input" rows={3} autoFocus value={editing.text} onChange={(event) => setEditing({ ...editing, text: event.target.value })} />
                  <div className="buttons">
                    <button
                      type="button"
                      className="button primary"
                      disabled={editing.text.trim().length === 0}
                      onClick={() => {
                        reviews.dispatch(project, { type: 'edit', id: comment.id, text: editing.text.trim() });
                        setEditing(null);
                      }}
                    >
                      Save
                    </button>
                    <button type="button" className="button" onClick={() => setEditing(null)}>
                      Cancel
                    </button>
                  </div>
                </div>
              ) : (
                <div key={comment.id} className="review-item">
                  <div className="line">
                    <span className="chip accent">
                      {rangeLabel(comment.start, comment.end)}
                    </span>
                    <span className="chip">{comment.side}</span>
                    <span className="grow" />
                    <button type="button" className="icon-btn" onClick={() => setEditing({ id: comment.id, text: comment.text })} aria-label="Edit comment">
                      ✎
                    </button>
                    <button
                      type="button"
                      className="icon-btn danger"
                      onClick={() => reviews.dispatch(project, { type: 'delete', id: comment.id })}
                      aria-label="Delete comment"
                    >
                      🗑
                    </button>
                  </div>
                  {comment.quote.length > 0 && <pre className="review-quote">{comment.quote.join('\n')}</pre>}
                  <p>{comment.text}</p>
                </div>
              ),
            )}
          </section>
        ))}
        <label className="field">
          <span>General comment</span>
          <textarea
            className="input"
            rows={3}
            value={draft.general}
            placeholder="Anything not tied to a line"
            onChange={(event) => reviews.dispatch(project, { type: 'general', text: event.target.value })}
          />
        </label>
        <div className="field">
          <span>Session</span>
          {targets.length === 0 ? (
            <p className="muted small">No omp session runs in {display}; start or resume one to hand it the review.</p>
          ) : (
            <div className="option-list">
              {targets.map((entry) => (
                <button
                  key={entry.live.key}
                  type="button"
                  className="option"
                  aria-pressed={target?.live.key === entry.live.key}
                  onClick={() => setTargetKey(entry.live.key)}
                >
                  <span className="grow">
                    {entry.title ?? entry.label}
                    <small>{statusLabel(entry)}</small>
                  </span>
                  {target?.live.key === entry.live.key && <span className="chip accent">✓</span>}
                </button>
              ))}
            </div>
          )}
        </div>
        <div className="buttons pair">
          <button type="button" className="button" disabled={!allowed} onClick={() => void submit(false)}>
            ⇥ Insert into omp
          </button>
          <button
            type="button"
            className="button primary"
            disabled={!allowed || targetBusy}
            title={targetBusy ? 'omp is working; send once it is idle, or insert now.' : undefined}
            onClick={() => void submit(true)}
          >
            ↑ Send
          </button>
        </div>
        {targetBusy && <p className="muted small">omp is working: Send waits for idle; Insert puts the review in its input now.</p>}
        {count > 0 && (
          <button type="button" className="button ghost danger-text" onClick={() => setConfirmDiscard(true)}>
            Discard all comments
          </button>
        )}
      </div>
    </Sheet>
  );
}
