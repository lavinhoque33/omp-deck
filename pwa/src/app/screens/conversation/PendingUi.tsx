// omp's pending UI requests as inline cards at the bottom of the conversation: one editor per kind
// (select, confirm, input, editor, ask, approve, form), each answering with `answer_ui`; a
// countdown when the request has a timeout; and an "answered on desktop" card when it leaves
// unanswered.

import { Fragment, type ReactNode, useState } from 'react';
import type { api } from '../../../remote/index.ts';
import { describeResult, perform } from '../../ui/format.ts';
import { useCan, useClient, useNow } from '../../ui/hooks.ts';
import {
  type AskDraft,
  type FormDraft,
  type GoneRequest,
  answerLabel,
  approveChoices,
  askAnswer,
  askAnswered,
  countdown,
  formAnswer,
  formFieldError,
  initialAskDraft,
  initialFormDrafts,
  otherChosen,
  pickFormOption,
  pickFormOther,
  remainingMs,
  selectAnswer,
  setFormOther,
  textAnswer,
  toggleAsk,
  toggleFormOption,
} from './pending.ts';

const KIND_HEADER: Record<api.UiKind, string> = {
  select: 'CHOICE REQUIRED',
  confirm: 'CONFIRMATION REQUIRED',
  input: 'INPUT REQUIRED',
  editor: 'TEXT REQUIRED',
  ask: 'QUESTIONS FROM OMP',
  approve: 'APPROVAL REQUIRED',
  form: 'QUESTIONS FROM OMP',
};

/** The fixed titles omp's ctl extension gives these kinds; the header already says as much. */
const CTL_TITLE: Partial<Record<api.UiKind, string>> = { ask: 'Ask', approve: 'Approval', form: 'Ask' };

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
  /** Why the last answer did not apply (omp's own form check, a refusal), shown on the card. */
  const [error, setError] = useState<string | null>(null);
  const allowed = can({ op: 'answer_ui', live, request: request.request, value: { type: 'cancel' } }) && !busy;

  const answer = async (value: api.UiAnswer) => {
    setBusy(true);
    setError(null);
    onAnswered(request, null, true);
    const view = await perform(client, { op: 'answer_ui', live, request: request.request, value }, answerLabel(request, value));
    setBusy(false);
    if (view?.outcome?.state === 'failed') setError(view.outcome.message);
    else if (view !== null && view.status !== 'done') setError(describeResult(view)?.text ?? null);
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
        {error !== null && (
          <p className="notice error ui-error" role="alert">
            {error}
          </p>
        )}
        <div className="ui-foot">
          <button type="button" className="button ghost" disabled={!allowed} onClick={() => void answer({ type: 'cancel' })}>
            {request.kind === 'form' ? 'Cancel' : 'Dismiss'}
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
    case 'form':
      return request.form === undefined ? null : <FormEditor form={request.form} allowed={allowed} onAnswer={onAnswer} />;
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

/** Inline text of a form: `code`/`strong`/`em` runs, `\n` as a line break. */
function Spans({ spans }: { spans: readonly api.UiSpan[] }): ReactNode {
  return spans.map((span, index) => {
    const lines = span.text.split('\n');
    const text = lines.map((line, at) => (
      <Fragment key={at}>
        {at > 0 && <br />}
        {line}
      </Fragment>
    ));
    switch (span.style) {
      case 'code':
        return <code key={index}>{text}</code>;
      case 'strong':
        return <strong key={index}>{text}</strong>;
      case 'em':
        return <em key={index}>{text}</em>;
      default:
        return <Fragment key={index}>{text}</Fragment>;
    }
  });
}

/**
 * omp's HTML form (18.18 `ask`): its sections (a titled one boxed like an `ask` question), their
 * context blocks and the fields in place; Submit only once every field passes omp's checks.
 */
function FormEditor({
  form,
  allowed,
  onAnswer,
}: {
  form: api.UiFormView;
  allowed: boolean;
  onAnswer: (value: api.UiAnswer) => void;
}): ReactNode {
  const [drafts, setDrafts] = useState<readonly FormDraft[]>(() => initialFormDrafts(form));
  // Errors show once a field was changed or Submit tried, not on a form just opened.
  const [touched, setTouched] = useState<ReadonlySet<number>>(() => new Set());
  const update = (index: number, draft: FormDraft) => {
    setDrafts(drafts.map((entry, at) => (at === index ? draft : entry)));
    if (!touched.has(index)) setTouched(new Set(touched).add(index));
  };
  const value = formAnswer(form, drafts);
  const failing = form.fields.filter((field, index) => formFieldError(field, drafts[index]) !== null).length;
  const placed = new Set(form.sections.flatMap((section) => section.blocks.flatMap((block) => (block.kind === 'field' ? [block.index] : []))));

  const field = (index: number): ReactNode => {
    const view = form.fields[index];
    const draft = drafts[index];
    if (view === undefined || draft === undefined) return null;
    return (
      <FormField
        key={`f${index}`}
        field={view}
        draft={draft}
        error={touched.has(index) ? formFieldError(view, draft) : null}
        allowed={allowed}
        onChange={(next) => update(index, next)}
      />
    );
  };

  const block = (entry: api.UiFormBlock, index: number): ReactNode => {
    switch (entry.kind) {
      case 'text':
        return (
          <p key={index} className={entry.heading ? 'ui-form-heading' : 'ui-form-text'}>
            <Spans spans={entry.spans} />
          </p>
        );
      case 'list': {
        const items = entry.items.map((item, at) => (
          <li key={at}>
            <Spans spans={item} />
          </li>
        ));
        return entry.ordered ? (
          <ol key={index} className="ui-form-list">
            {items}
          </ol>
        ) : (
          <ul key={index} className="ui-form-list">
            {items}
          </ul>
        );
      }
      case 'pre':
        return <pre key={index}>{entry.text}</pre>;
      case 'table':
        return (
          <div key={index} className="ui-form-table">
            <table>
              <tbody>
                {entry.rows.map((row, at) => (
                  <tr key={at}>
                    {row.map((cell, column) =>
                      at < entry.header_rows ? (
                        <th key={column}>
                          <Spans spans={cell} />
                        </th>
                      ) : (
                        <td key={column}>
                          <Spans spans={cell} />
                        </td>
                      ),
                    )}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        );
      case 'image':
        return (
          <p key={index} className="muted small ui-form-image">
            [image{entry.alt.length > 0 ? `: ${entry.alt}` : ''}]
          </p>
        );
      case 'field':
        return field(entry.index);
    }
  };

  return (
    <form
      className="ui-form"
      noValidate
      onSubmit={(event) => {
        event.preventDefault();
        setTouched(new Set(form.fields.keys()));
        if (allowed && value !== null) onAnswer(value);
      }}
    >
      {form.sections.map((section, index) =>
        section.title.length > 0 ? (
          <fieldset key={index} className="ui-question ui-form-section">
            <legend>
              <span className="ui-form-legend">
                <Spans spans={section.title} />
              </span>
            </legend>
            {section.blocks.map(block)}
          </fieldset>
        ) : (
          <div key={index} className="ui-form-section">
            {section.blocks.map(block)}
          </div>
        ),
      )}
      {form.fields.map((_, index) => (placed.has(index) ? null : field(index)))}
      <div className="buttons">
        <button type="submit" className="button primary" disabled={!allowed || value === null}>
          ✓ Submit
        </button>
        {failing > 0 && (
          <span className="chip warn">
            {failing} {failing === 1 ? 'field needs' : 'fields need'} an answer
          </span>
        )}
      </div>
    </form>
  );
}

/** `<input type>`s drawn as such (their phone keyboards); anything else is plain text. */
const INPUT_TYPES: Record<string, true> = { number: true, email: true, url: true };

function FormField({
  field,
  draft,
  error,
  allowed,
  onChange,
}: {
  field: api.UiFormFieldView;
  draft: FormDraft;
  error: string | null;
  allowed: boolean;
  onChange: (draft: FormDraft) => void;
}): ReactNode {
  const label = field.label.length > 0 && (
    <span className="ui-form-label">
      <Spans spans={field.label} />
    </span>
  );
  const head = (label !== false || field.required) && (
    <div className="ui-form-field-head">
      {label}
      {field.required && (
        <span className="ui-required" title="Required" aria-label="required">
          *
        </span>
      )}
    </div>
  );
  const question = field.question.length > 0 ? field.question : field.name;
  let control: ReactNode;
  if (field.kind === 'text' && draft.kind === 'text') {
    control = field.multiline ? (
      <textarea
        className="input ui-form-input"
        rows={Math.max(2, field.rows)}
        value={draft.text}
        placeholder={field.placeholder}
        disabled={!allowed}
        aria-label={question}
        aria-invalid={error !== null}
        onChange={(event) => onChange({ kind: 'text', text: event.target.value })}
      />
    ) : (
      <input
        className="input ui-form-input"
        type={INPUT_TYPES[field.input_type] === true ? field.input_type : 'text'}
        inputMode={field.input_type === 'number' ? 'decimal' : undefined}
        value={draft.text}
        placeholder={field.placeholder}
        disabled={!allowed}
        aria-label={question}
        aria-invalid={error !== null}
        onChange={(event) => onChange({ kind: 'text', text: event.target.value })}
      />
    );
  } else if (field.kind === 'choice' && draft.kind === 'choice' && field.select && !field.multi) {
    control = (
      <select
        className="input ui-form-input"
        value={draft.checked[0] === undefined ? '' : String(draft.checked[0])}
        disabled={!allowed}
        aria-label={question}
        aria-invalid={error !== null}
        onChange={(event) => onChange(pickFormOption(field, draft, event.target.value === '' ? null : Number(event.target.value)))}
      >
        <option value="">—</option>
        {field.options.map((option, index) => (
          <option key={index} value={String(index)} disabled={option.disabled}>
            {option.label.map((span) => span.text).join('')}
          </option>
        ))}
      </select>
    );
  } else if (field.kind === 'choice' && draft.kind === 'choice') {
    const role = field.multi ? 'checkbox' : 'radio';
    const otherOn = field.multi ? otherChosen(field, draft) : draft.otherPicked && draft.checked.length === 0;
    control = (
      <>
        <div className="option-list ui-options" role={field.multi ? 'group' : 'radiogroup'} aria-label={question}>
          {field.options.map((option, index) => {
            const on = draft.checked.includes(index);
            return (
              <button
                key={index}
                type="button"
                className="option"
                role={role}
                aria-checked={on}
                aria-pressed={on}
                disabled={!allowed || option.disabled}
                onClick={() => onChange(toggleFormOption(field, draft, index))}
              >
                <span className={`mark${field.multi ? ' square' : ''}`} aria-hidden="true">
                  {on ? (field.multi ? '✓' : '●') : ''}
                </span>
                <span className="grow">
                  <Spans spans={option.label} />
                </span>
              </button>
            );
          })}
          {field.other && !field.multi && (
            <button
              type="button"
              className="option"
              role="radio"
              aria-checked={otherOn}
              aria-pressed={otherOn}
              disabled={!allowed}
              onClick={() => onChange(pickFormOther(field, draft))}
            >
              <span className="mark" aria-hidden="true">
                {otherOn ? '●' : ''}
              </span>
              <span className="grow">Other</span>
            </button>
          )}
        </div>
        {field.other && (
          <input
            className="input ui-custom"
            placeholder={field.multi ? 'Other (type your own)' : 'Type your own answer'}
            value={draft.other}
            disabled={!allowed}
            aria-label={`Other answer to: ${question}`}
            onChange={(event) => onChange(setFormOther(field, draft, event.target.value))}
          />
        )}
      </>
    );
  }
  return (
    <div className={`ui-form-field${error !== null ? ' invalid' : ''}`}>
      {head}
      {control}
      {error !== null && (
        <p className="ui-form-error" role="alert">
          {error}
        </p>
      )}
    </div>
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
