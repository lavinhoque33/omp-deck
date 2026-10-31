// omp's pending UI requests as inline cards at the bottom of the conversation: one editor per kind
// (select, confirm, input, editor, ask, approve), each answering with `answer_ui`; a countdown
// when the request has a timeout; and an "answered on desktop" card when it leaves unanswered.

import { type ReactNode, useState } from 'react';
import type { api } from '../../../remote/index.ts';
import { perform } from '../../ui/format.ts';
import { useCan, useClient, useNow } from '../../ui/hooks.ts';
import {
  type AskDraft,
  type GoneRequest,
  answerLabel,
  approveChoices,
  askAnswer,
  askAnswered,
  countdown,
  initialAskDraft,
  remainingMs,
  selectAnswer,
  textAnswer,
  toggleAsk,
} from './pending.ts';

const KIND_HEADER: Record<api.UiKind, string> = {
  select: 'CHOICE REQUIRED',
  confirm: 'CONFIRMATION REQUIRED',
  input: 'INPUT REQUIRED',
  editor: 'TEXT REQUIRED',
  ask: 'QUESTIONS FROM OMP',
  approve: 'APPROVAL REQUIRED',
};

/** The fixed titles omp's ctl extension gives these kinds; the header already says as much. */
const CTL_TITLE: Partial<Record<api.UiKind, string>> = { ask: 'Ask', approve: 'Approval' };

export function PendingUiCard({
  live,
  request,
  seenMs,
  onAnswered,
}: {
  live: api.LiveRef;
  request: api.UiRequestView;
  /** When this phone first saw the request (the countdown's start). */
  seenMs: number;
  /** Called with `sending` as the answer goes out, then with its outcome (`applied` null: none). */
  onAnswered: (request: api.UiRequestView, applied: boolean | null, sending: boolean) => void;
}): ReactNode {
  const client = useClient();
  const can = useCan();
  const [busy, setBusy] = useState(false);
  const allowed = can({ op: 'answer_ui', live, request: request.request, value: { type: 'cancel' } }) && !busy;

  const answer = async (value: api.UiAnswer) => {
    setBusy(true);
    onAnswered(request, null, true);
    const view = await perform(client, { op: 'answer_ui', live, request: request.request, value }, answerLabel(request, value));
    setBusy(false);
    const result = view?.outcome?.state === 'done' ? view.outcome.result : null;
    onAnswered(request, result?.type === 'answered' ? result.applied : null, false);
  };

  return (
    <section className={`approval ui-card kind-${request.kind}`} aria-label={request.title}>
      <header>
        <span className="grow">△ {KIND_HEADER[request.kind]}</span>
        {request.timeout_ms !== null && <Countdown timeoutMs={request.timeout_ms} seenMs={seenMs} />}
      </header>
      <div className="approval-body">
        {request.title.length > 0 && request.title !== CTL_TITLE[request.kind] && <p className="ui-title">{request.title}</p>}
        {request.message !== null && request.kind !== 'input' && <p className="ui-message">{request.message}</p>}
        <Editor request={request} allowed={allowed} onAnswer={(value) => void answer(value)} />
        <div className="ui-foot">
          <button type="button" className="button ghost" disabled={!allowed} onClick={() => void answer({ type: 'cancel' })}>
            Dismiss
          </button>
          {busy && (
            <span className="muted small">
              <span className="busy" /> Sending…
            </span>
          )}
        </div>
      </div>
    </section>
  );
}

function Countdown({ timeoutMs, seenMs }: { timeoutMs: number; seenMs: number }): ReactNode {
  const now = useNow(1000);
  const left = remainingMs(timeoutMs, seenMs, now) ?? 0;
  return (
    <span className={`chip ${left < 10_000 ? 'danger' : 'warn'}`} role="timer" aria-label="Time left">
      ⏱ {countdown(left)}
    </span>
  );
}

function Editor({
  request,
  allowed,
  onAnswer,
}: {
  request: api.UiRequestView;
  allowed: boolean;
  onAnswer: (value: api.UiAnswer) => void;
}): ReactNode {
  switch (request.kind) {
    case 'select':
      return <SelectEditor request={request} allowed={allowed} onAnswer={onAnswer} />;
    case 'confirm':
      return (
        <div className="buttons">
          <button type="button" className="button primary" disabled={!allowed} onClick={() => onAnswer({ type: 'confirm', yes: true })}>
            ✓ Yes
          </button>
          <button type="button" className="button" disabled={!allowed} onClick={() => onAnswer({ type: 'confirm', yes: false })}>
            No
          </button>
        </div>
      );
    case 'input':
    case 'editor':
      return <TextEditor request={request} allowed={allowed} onAnswer={onAnswer} />;
    case 'ask':
      return <AskEditor request={request} allowed={allowed} onAnswer={onAnswer} />;
    case 'approve':
      return request.approve === null ? null : <ApproveEditor view={request.approve} allowed={allowed} onAnswer={onAnswer} />;
  }
}

function SelectEditor({
  request,
  allowed,
  onAnswer,
}: {
  request: api.UiRequestView;
  allowed: boolean;
  onAnswer: (value: api.UiAnswer) => void;
}): ReactNode {
  const [choice, setChoice] = useState<number | null>(request.initial);
  const send = (index: number | null) => {
    const value = index === null ? null : selectAnswer(request, index);
    if (value !== null) onAnswer(value);
  };
  return (
    <>
      {request.multi && <p className="muted small ui-hint">Each tap toggles one option; omp then asks again with the list updated.</p>}
      <div className="option-list ui-options" role={request.multi ? 'group' : 'radiogroup'}>
        {request.options.map((option, index) => (
          <button
            key={index}
            type="button"
            className="option"
            role={request.multi ? undefined : 'radio'}
            aria-checked={request.multi ? undefined : choice === index}
            aria-pressed={request.multi ? undefined : choice === index}
            disabled={!allowed}
            onClick={() => (request.multi ? send(index) : setChoice(index))}
          >
            <span className={`mark${request.multi ? ' square' : ''}`} aria-hidden="true">
              {!request.multi && choice === index ? '●' : ''}
            </span>
            <span className="grow">
              {option.label}
              {option.description !== null && <small>{option.description}</small>}
            </span>
            {request.multi && <span className="chip">toggle</span>}
          </button>
        ))}
      </div>
      {!request.multi && (
        <div className="buttons">
          <button type="button" className="button primary" disabled={!allowed || choice === null} onClick={() => send(choice)}>
            ✓ Choose{choice !== null && request.options[choice] !== undefined ? ` “${request.options[choice].label}”` : ''}
          </button>
        </div>
      )}
    </>
  );
}

function TextEditor({
  request,
  allowed,
  onAnswer,
}: {
  request: api.UiRequestView;
  allowed: boolean;
  onAnswer: (value: api.UiAnswer) => void;
}): ReactNode {
  const [text, setText] = useState(request.prefill ?? '');
  return (
    <>
      {request.kind === 'input' ? (
        <input
          className="input"
          value={text}
          placeholder={request.message ?? ''}
          onChange={(event) => setText(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === 'Enter' && allowed) onAnswer(textAnswer('input', text));
          }}
          aria-label={request.title}
        />
      ) : (
        <textarea className="input ui-editor" rows={6} value={text} onChange={(event) => setText(event.target.value)} aria-label={request.title} />
      )}
      <div className="buttons">
        <button type="button" className="button primary" disabled={!allowed} onClick={() => onAnswer(textAnswer(request.kind, text))}>
          ✓ Submit
        </button>
        {request.prefill !== null && text !== request.prefill && (
          <button type="button" className="button" onClick={() => setText(request.prefill ?? '')}>
            Reset
          </button>
        )}
      </div>
    </>
  );
}

function AskEditor({
  request,
  allowed,
  onAnswer,
}: {
  request: api.UiRequestView;
  allowed: boolean;
  onAnswer: (value: api.UiAnswer) => void;
}): ReactNode {
  const [drafts, setDrafts] = useState<ReadonlyMap<string, AskDraft>>(
    () => new Map(request.questions.map((question) => [question.id, initialAskDraft(question)])),
  );
  const update = (id: string, draft: AskDraft) => setDrafts(new Map(drafts).set(id, draft));
  const value = askAnswer(request.questions, drafts);
  const answered = request.questions.filter((question) => askAnswered(drafts.get(question.id))).length;
  return (
    <>
      {request.questions.map((question, number) => {
        const draft = drafts.get(question.id) ?? initialAskDraft(question);
        return (
          <fieldset key={question.id} className="ui-question">
            <legend>
              <span className="chip accent">{question.header ?? `Q${number + 1}`}</span>
              {question.multi && <span className="chip">multiple</span>}
            </legend>
            <p className="ui-question-text">{question.question}</p>
            <div className="option-list ui-options">
              {question.options.map((option, index) => {
                const on = draft.selected.includes(index);
                return (
                  <button
                    key={index}
                    type="button"
                    className="option"
                    aria-pressed={on}
                    disabled={!allowed}
                    onClick={() => update(question.id, toggleAsk(question, draft, index))}
                  >
                    <span className={`mark${question.multi ? ' square' : ''}`} aria-hidden="true">
                      {on ? (question.multi ? '✓' : '●') : ''}
                    </span>
                    <span className="grow">
                      {option.label}
                      {option.description !== null && <small>{option.description}</small>}
                    </span>
                    {question.recommended === index && <span className="chip ok">recommended</span>}
                  </button>
                );
              })}
            </div>
            <input
              className="input ui-custom"
              placeholder="Or write your own answer"
              value={draft.custom}
              disabled={!allowed}
              onChange={(event) => update(question.id, { ...draft, custom: event.target.value })}
              aria-label={`Own answer to: ${question.question}`}
            />
          </fieldset>
        );
      })}
      <div className="buttons">
        <button type="button" className="button primary" disabled={!allowed || value === null} onClick={() => value !== null && onAnswer(value)}>
          ✓ Submit answers
        </button>
        <span className="chip">
          {answered}/{request.questions.length} answered
        </span>
      </div>
    </>
  );
}

function ApproveEditor({
  view,
  allowed,
  onAnswer,
}: {
  view: api.ApproveView;
  allowed: boolean;
  onAnswer: (value: api.UiAnswer) => void;
}): ReactNode {
  const [full, setFull] = useState(false);
  const differs = view.full !== view.compact;
  return (
    <>
      {differs && (
        <div className="segmented ui-detail" role="group" aria-label="Detail">
          <button type="button" aria-pressed={!full} onClick={() => setFull(false)}>
            Compact
          </button>
          <button type="button" aria-pressed={full} onClick={() => setFull(true)}>
            Full
          </button>
        </div>
      )}
      <pre>{full || !differs ? view.full : view.compact}</pre>
      <div className="ui-decisions">
        {approveChoices(view).map((choice) => (
          <button
            key={choice.label + JSON.stringify(choice.decision)}
            type="button"
            className={`button${choice.tone === 'primary' ? ' primary' : choice.tone === 'danger' ? ' danger' : ''}${choice.decision.type === 'grant' ? '' : ' wide'}`}
            disabled={!allowed}
            onClick={() => onAnswer({ type: 'approve', decision: choice.decision })}
          >
            {choice.tone === 'primary' ? '✓ ' : choice.tone === 'danger' ? '✕ ' : ''}
            {choice.label}
          </button>
        ))}
      </div>
    </>
  );
}

/** A request that left without this phone's answer: omp took an answer from the desktop (or timed out). */
export function GoneCard({ gone, onDismiss }: { gone: GoneRequest; onDismiss: () => void }): ReactNode {
  return (
    <section className="approval ui-card gone" role="status">
      <header>
        <span className="grow">✓ ANSWERED ON DESKTOP</span>
        <button type="button" className="icon-btn" onClick={onDismiss} aria-label="Dismiss">
          ✕
        </button>
      </header>
      <div className="approval-body">
        <p className="ui-title">{gone.request.title}</p>
        <p className="muted small">It was answered in the desktop terminal (or timed out) before this phone’s answer arrived.</p>
      </div>
    </section>
  );
}
