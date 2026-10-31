// The conversation composer: `compose` in send (idle), steer / follow-up (busy) or insert (any)
// mode, following busy/idle by itself; attachments from files or the camera, uploaded in chunks
// with progress chips; interrupt while busy; the context meter chip. A desktop without slice 4
// gets the v1 composer (prompt while idle, no attachments).

import { type ReactNode, useEffect, useLayoutEffect, useReducer, useRef, useState } from 'react';
import type { api } from '../../../remote/index.ts';
import { ClientError } from '../../client.ts';
import { upload } from '../../transfer.ts';
import { UNOBSERVED_TEXT, describeDelivery, perform, succeeded } from '../../ui/format.ts';
import { useCan, useClient } from '../../ui/hooks.ts';
import { useS4 } from '../../ui/kit.tsx';
import {
  type Attachment,
  MODE_HINT,
  MODE_LABEL,
  type SessionPhase,
  attachProblem,
  attachmentsReducer,
  composeProblem,
  etaText,
  fileSize,
  followMode,
  modesFor,
  phaseOf,
  progress,
  uploadIds,
} from './compose.ts';

const MODE_GLYPH: Record<api.ComposeMode, string> = { send: '↑', steer: '↯', follow_up: '⏎', insert: '⇥' };

function uploadError(error: unknown): string {
  if (error instanceof ClientError && error.api?.code === 'too_large') return 'Over the desktop’s attachment quota';
  if (error instanceof Error) return error.message;
  return String(error);
}

export function Composer({
  live,
  busy,
  share,
  onMeter,
}: {
  live: api.LiveSession;
  busy: boolean;
  share: number | null;
  onMeter: () => void;
}): ReactNode {
  const client = useClient();
  const can = useCan();
  const s4 = useS4();
  const phase: SessionPhase = phaseOf(live);
  const [mode, setMode] = useState<api.ComposeMode>(phase === 'busy' ? 'steer' : 'send');
  const lastPhase = useRef(phase);
  const [text, setText] = useState('');
  // Bumped on every edit, so a send can tell whether the draft changed while it was in flight.
  const textRevision = useRef(0);
  const [sending, setSending] = useState(false);
  const [outcome, setOutcome] = useState<{ text: string; tone: 'ok' | 'warn' } | null>(null);
  const [attachments, dispatch] = useReducer(attachmentsReducer, []);
  const aborts = useRef(new Map<number, AbortController>());
  const nextId = useRef(1);
  const area = useRef<HTMLTextAreaElement>(null);
  const files = useRef<HTMLInputElement>(null);
  const camera = useRef<HTMLInputElement>(null);

  // Follow busy/idle: steer while busy, send while idle (insert stays).
  useEffect(() => {
    setMode((current) => followMode(current, lastPhase.current, phase));
    lastPhase.current = phase;
  }, [phase]);

  // Cancel uploads still running when the composer goes away.
  useEffect(() => () => aborts.current.forEach((controller) => controller.abort()), []);

  useLayoutEffect(() => {
    const element = area.current;
    if (element === null) return;
    element.style.height = 'auto';
    element.style.height = `${Math.min(element.scrollHeight, 140)}px`;
  }, [text]);

  const modes = s4 ? modesFor(phase) : phase === 'idle' ? (['send'] as api.ComposeMode[]) : [];
  const effective: api.ComposeMode = modes.includes(mode) ? mode : (modes[0] ?? 'send');
  const op: api.Op = s4
    ? { op: 'compose', live: live.live, text, attachments: uploadIds(attachments), mode: effective }
    : { op: 'prompt', live: live.live, text };
  const problem = s4
    ? composeProblem(effective, phase, text, attachments)
    : phase !== 'idle'
      ? composeProblem('send', phase, text, attachments)
      : text.trim().length === 0
        ? 'Write a message first.'
        : null;
  const canSend = modes.length > 0 && problem === null && can(op) && !sending;

  const addFiles = (list: FileList | null) => {
    if (list === null) return;
    let current = attachments;
    for (const file of Array.from(list)) {
      const refusal = attachProblem(file, current);
      if (refusal !== null) {
        client.notify(refusal, 'warn');
        continue;
      }
      const id = nextId.current++;
      const attachment: Attachment = {
        id,
        name: file.name.length > 0 ? file.name : 'photo.jpg',
        mime: file.type.length > 0 ? file.type : 'application/octet-stream',
        size: file.size,
        status: 'uploading',
        sent: 0,
        upload: null,
        error: null,
      };
      current = attachmentsReducer(current, { type: 'add', attachment });
      dispatch({ type: 'add', attachment });
      const controller = new AbortController();
      aborts.current.set(id, controller);
      upload(client, file, { signal: controller.signal, onProgress: (sent, _total, eta) => dispatch({ type: 'progress', id, sent, eta }) }).then(
        (uploadId) => {
          aborts.current.delete(id);
          dispatch({ type: 'done', id, upload: uploadId });
        },
        (error: unknown) => {
          aborts.current.delete(id);
          if (controller.signal.aborted) return;
          dispatch({ type: 'failed', id, error: uploadError(error) });
        },
      );
    }
  };

  const remove = (entry: Attachment) => {
    aborts.current.get(entry.id)?.abort();
    aborts.current.delete(entry.id);
    if (entry.status === 'done' && entry.upload !== null) {
      client.request({ op: 'upload_cancel', upload: entry.upload }).catch(() => undefined);
    }
    dispatch({ type: 'remove', id: entry.id });
  };

  const send = async () => {
    if (!canSend) return;
    const sentText = text.trim();
    const submittedRevision = textRevision.current;
    const submittedUploads = uploadIds(attachments);
    const label = s4 ? `${MODE_LABEL[effective]}${sentText.length > 0 ? ` “${sentText.slice(0, 40)}${sentText.length > 40 ? '…' : ''}”` : ''}` : 'Prompt';
    setSending(true);
    setOutcome(null);
    const view = await perform(
      client,
      s4 ? { op: 'compose', live: live.live, text: sentText, attachments: submittedUploads, mode: effective } : { op: 'prompt', live: live.live, text: sentText },
      label,
    );
    setSending(false);
    if (!succeeded(view)) {
      const early = view?.outcome?.state === 'done' ? view.outcome.result : null;
      if (early?.type === 'submitted' && early.delivery.state === 'unobserved') setOutcome({ text: UNOBSERVED_TEXT, tone: 'warn' });
      else if (view !== null) setOutcome({ text: `${label} did not go through.`, tone: 'warn' });
      return;
    }
    const result = view?.outcome?.state === 'done' ? view.outcome.result : null;
    const delivery = result?.type === 'submitted' ? result.delivery : null;
    // Only what was submitted goes: text typed and files attached during the send survive.
    if (textRevision.current === submittedRevision) setText('');
    dispatch({ type: 'remove_sent', uploads: submittedUploads });
    setOutcome(
      delivery?.state === 'awaiting_confirm'
        ? { text: 'omp holds the draft: confirm it above.', tone: 'warn' }
        : delivery !== null && describeDelivery(delivery) !== null
          ? { text: describeDelivery(delivery) ?? '', tone: 'warn' }
          : {
              text: { send: 'Sent.', steer: 'Steered omp.', follow_up: 'Queued as a follow-up.', insert: 'Inserted into omp’s input.' }[effective],
              tone: 'ok',
            },
    );
  };

  const exited = phase === 'exited';
  const showOutcome = outcome !== null && (text.length === 0 || outcome.tone === 'warn');
  const hint = exited
    ? 'The session has exited.'
    : !s4 && busy
      ? 'omp is working: interrupt it, or wait to send.'
      : showOutcome
        ? outcome.text
        : phase === 'starting'
          ? 'omp is starting…'
          : phase === 'attention'
            ? 'omp waits for an answer.'
            : MODE_HINT[effective];

  return (
    <div className={`composer mode-${effective}`}>
      {s4 && modes.length > 1 && (
        <div className="segmented compose-modes" role="radiogroup" aria-label="Send mode">
          {modes.map((entry) => (
            <button key={entry} type="button" role="radio" aria-checked={effective === entry} aria-pressed={effective === entry} onClick={() => setMode(entry)}>
              {MODE_GLYPH[entry]} {MODE_LABEL[entry]}
            </button>
          ))}
        </div>
      )}
      {attachments.length > 0 && (
        <div className="attach-chips">
          {attachments.map((entry) => (
            <span key={entry.id} className={`attach-chip ${entry.status}`} title={entry.error ?? undefined}>
              <span className="attach-bar" style={{ width: `${Math.round(progress(entry) * 100)}%` }} aria-hidden="true" />
              <span className="attach-name">{entry.mime.startsWith('image/') ? '▣' : '▤'} {entry.name}</span>
              <small>
                {entry.status === 'uploading'
                  ? `${Math.round(progress(entry) * 100)}% · ${fileSize(entry.size)}${etaText(entry.eta) === null ? '' : ` · ${etaText(entry.eta)}`}`
                  : entry.status === 'error'
                    ? (entry.error ?? 'failed')
                    : fileSize(entry.size)}
              </small>
              <button type="button" className="attach-x" onClick={() => remove(entry)} aria-label={`Remove ${entry.name}`}>
                ✕
              </button>
            </span>
          ))}
        </div>
      )}
      <textarea
        ref={area}
        className="compose-text"
        rows={1}
        placeholder={effective === 'steer' ? 'Steer omp…' : effective === 'follow_up' ? 'Follow up after this turn…' : 'Message omp…'}
        value={text}
        onChange={(event) => {
          textRevision.current += 1;
          setText(event.target.value);
          if (outcome !== null) setOutcome(null);
        }}
        aria-label="Message"
      />
      <div className={`compose-hint${showOutcome ? ` ${outcome.tone}` : ''}`}>{hint}</div>
      <div className="compose-tools">
        {s4 && (
          <>
            <button type="button" className="attach-btn" disabled={exited} onClick={() => files.current?.click()} aria-label="Attach files">
              ＋ <span>Attach</span>
            </button>
            <button type="button" className="attach-btn" disabled={exited} onClick={() => camera.current?.click()} aria-label="Take a photo">
              ◉
            </button>
            <input
              ref={files}
              type="file"
              multiple
              hidden
              onChange={(event) => {
                addFiles(event.target.files);
                event.target.value = '';
              }}
            />
            <input
              ref={camera}
              type="file"
              accept="image/*"
              capture="environment"
              hidden
              onChange={(event) => {
                addFiles(event.target.files);
                event.target.value = '';
              }}
            />
          </>
        )}
        {busy && (
          <button
            type="button"
            className="button danger small-btn"
            disabled={!can({ op: 'interrupt', live: live.live })}
            onClick={() => void perform(client, { op: 'interrupt', live: live.live }, 'Interrupt')}
            aria-label="Interrupt"
          >
            ■
          </button>
        )}
        <button type="button" className="meterchip" onClick={onMeter} aria-label="Context usage">
          ◔ {share === null ? 'ctx' : `${Math.round(share * 100)}%`}
        </button>
        <button
          type="button"
          className="button primary send"
          disabled={!canSend}
          title={problem ?? undefined}
          onClick={() => void send()}
          aria-label={MODE_LABEL[effective]}
        >
          {sending ? <span className="busy" /> : MODE_GLYPH[effective]}
        </button>
      </div>
    </div>
  );
}
