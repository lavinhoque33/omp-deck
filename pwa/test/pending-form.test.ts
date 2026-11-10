// The phone's HTML-form `ask` editor logic (pending.ts): starting drafts, radio/checkbox/Other
// toggles, omp's `fieldError` messages, and the `form` answer the gateway checks.

import { describe, expect, test } from 'bun:test';
import type { api } from '../src/remote/index.ts';
import {
  type FormDraft,
  answerLabel,
  formAnswer,
  formFieldError,
  initialFormDrafts,
  pickFormOption,
  pickFormOther,
  setFormOther,
  toggleFormOption,
} from '../src/app/screens/conversation/pending.ts';

const spans = (text: string): api.UiSpan[] => [{ text, style: null }];

type Choice = Extract<api.UiFormFieldView, { kind: 'choice' }>;
type Text = Extract<api.UiFormFieldView, { kind: 'text' }>;

const choice = (patch: Partial<Choice> = {}): Choice => ({
  kind: 'choice',
  name: 'color',
  question: 'Pick a color?',
  label: [],
  multi: false,
  select: false,
  other: false,
  required: false,
  options: ['red', 'blue', 'green'].map((label) => ({ label: spans(label), checked: false, disabled: false })),
  ...patch,
});

const text = (patch: Partial<Text> = {}): Text => ({
  kind: 'text',
  name: 'notes',
  question: 'Notes',
  label: [],
  input_type: 'text',
  multiline: false,
  value: '',
  placeholder: '',
  required: false,
  min: null,
  max: null,
  pattern: null,
  min_length: null,
  max_length: null,
  rows: 1,
  ...patch,
});

const formOf = (...fields: api.UiFormFieldView[]): api.UiFormView => ({
  sections: [{ title: [], blocks: fields.map((_, index) => ({ kind: 'field' as const, index })) }],
  fields,
});

const textDraft = (value: string): FormDraft => ({ kind: 'text', text: value });

describe('form drafts', () => {
  test('start from checked enabled options (last checked radio wins) and prefilled text', () => {
    const radios = choice({
      options: [
        { label: spans('a'), checked: true, disabled: false },
        { label: spans('b'), checked: true, disabled: false },
        { label: spans('c'), checked: true, disabled: true },
      ],
    });
    const boxes = choice({ name: 'b', multi: true, options: radios.options });
    expect(initialFormDrafts(formOf(radios, boxes, text({ value: 'hi' })))).toEqual([
      { kind: 'choice', checked: [1], other: '', otherPicked: false },
      { kind: 'choice', checked: [0, 1], other: '', otherPicked: false },
      { kind: 'text', text: 'hi' },
    ]);
  });

  test('radio picks exclusively and never unchecks; Other sets the pick aside both ways', () => {
    const field = choice({ other: true });
    let draft = initialFormDrafts(formOf(field))[0]!;
    draft = toggleFormOption(field, draft, 0);
    draft = toggleFormOption(field, draft, 1);
    expect(draft).toMatchObject({ checked: [1] });
    expect(toggleFormOption(field, draft, 1)).toMatchObject({ checked: [1] });
    draft = pickFormOther(field, draft);
    expect(draft).toMatchObject({ checked: [], otherPicked: true });
    draft = setFormOther(field, toggleFormOption(field, draft, 2), 'teal');
    expect(draft).toMatchObject({ checked: [], other: 'teal', otherPicked: true });
    expect(formAnswer(formOf(field), [draft])).toEqual({ type: 'form', fields: [{ type: 'choice', checked: [], other: 'teal' }] });
    // Picking an option again drops Other from the answer but keeps the typed text.
    draft = toggleFormOption(field, draft, 0);
    expect(draft).toMatchObject({ checked: [0], other: 'teal', otherPicked: false });
    expect(formAnswer(formOf(field), [draft])).toEqual({ type: 'form', fields: [{ type: 'choice', checked: [0], other: null }] });
  });

  test('checkbox toggles; its Other joins the checked options', () => {
    const field = choice({ multi: true, other: true });
    let draft = initialFormDrafts(formOf(field))[0]!;
    draft = toggleFormOption(field, toggleFormOption(field, draft, 2), 0);
    expect(draft).toMatchObject({ checked: [0, 2] });
    draft = setFormOther(field, toggleFormOption(field, draft, 2), 'teal');
    expect(formAnswer(formOf(field), [draft])).toEqual({ type: 'form', fields: [{ type: 'choice', checked: [0], other: 'teal' }] });
    // Blank Other text is no answer.
    expect(formAnswer(formOf(field), [setFormOther(field, draft, '   ')])?.fields[0]).toEqual({ type: 'choice', checked: [0], other: null });
  });

  test('disabled options cannot be picked; Other needs the field to offer it', () => {
    const field = choice({ options: [{ label: spans('x'), checked: false, disabled: true }] });
    const draft = initialFormDrafts(formOf(field))[0]!;
    expect(toggleFormOption(field, draft, 0)).toBe(draft);
    expect(toggleFormOption(field, draft, 5)).toBe(draft);
    expect(setFormOther(field, draft, 'x')).toBe(draft);
    expect(pickFormOther(field, draft)).toBe(draft);
  });

  test('a dropdown picks one option or none', () => {
    const field = choice({ select: true });
    const draft = pickFormOption(field, initialFormDrafts(formOf(field))[0]!, 2);
    expect(draft).toMatchObject({ checked: [2] });
    expect(pickFormOption(field, draft, null)).toMatchObject({ checked: [] });
  });
});

describe("omp's fieldError messages", () => {
  const empty: FormDraft = { kind: 'choice', checked: [], other: '', otherPicked: false };

  test('required choices: a group needs one, a radio or lone checkbox is required', () => {
    expect(formFieldError(choice(), empty)).toBeNull();
    expect(formFieldError(choice({ required: true }), empty)).toBe('Required');
    expect(formFieldError(choice({ required: true, multi: true }), empty)).toBe('Select at least one');
    const lone = choice({ required: true, multi: true, options: [{ label: spans('I agree'), checked: false, disabled: false }] });
    expect(formFieldError(lone, empty)).toBe('Required');
    expect(formFieldError(choice({ required: true, multi: true }), { ...empty, checked: [1] })).toBeNull();
    expect(formFieldError(choice({ required: true, other: true }), { ...empty, other: 'teal', otherPicked: true })).toBeNull();
    expect(formFieldError(choice({ required: true, other: true }), { ...empty, other: '  ', otherPicked: true })).toBe('Required');
    // An unnamed field submits nothing, so omp never checks it.
    expect(formFieldError(choice({ required: true, name: '' }), empty)).toBeNull();
  });

  test('number bounds are inclusive and use the decimal text', () => {
    const field = text({ input_type: 'number', min: '1', max: '2.5', required: true });
    expect(formFieldError(field, textDraft(''))).toBe('Required');
    expect(formFieldError(field, textDraft('abc'))).toBe('Enter a number');
    expect(formFieldError(field, textDraft('0.99'))).toBe('Must be at least 1');
    expect(formFieldError(field, textDraft('1'))).toBeNull();
    expect(formFieldError(field, textDraft('2.5'))).toBeNull();
    expect(formFieldError(field, textDraft('2.51'))).toBe('Must be at most 2.5');
    expect(formFieldError(text({ input_type: 'number' }), textDraft('  '))).toBeNull();
  });

  test('lengths, email and url shapes', () => {
    expect(formFieldError(text({ min_length: 3 }), textDraft('ab'))).toBe('Needs at least 3 characters');
    expect(formFieldError(text({ min_length: 3 }), textDraft('abc'))).toBeNull();
    expect(formFieldError(text({ max_length: 3 }), textDraft('abcd'))).toBe('At most 3 characters');
    expect(formFieldError(text({ input_type: 'email' }), textDraft('a@b'))).toBeNull();
    expect(formFieldError(text({ input_type: 'email' }), textDraft('a b@c'))).toBe('Enter an email address');
    expect(formFieldError(text({ input_type: 'url' }), textDraft('example.com'))).toBe('Enter a full URL');
    expect(formFieldError(text({ input_type: 'url' }), textDraft('https://example.com'))).toBeNull();
  });

  test('pattern is anchored; an invalid pattern is ignored; long ones are cut in the message', () => {
    const digits = text({ pattern: '[0-9]+' });
    expect(formFieldError(digits, textDraft('123'))).toBeNull();
    expect(formFieldError(digits, textDraft('123a'))).toBe('Must match [0-9]+');
    expect(formFieldError(text({ pattern: 'a|b' }), textDraft('ab'))).toBe('Must match a|b');
    expect(formFieldError(text({ pattern: '(' }), textDraft('x'))).toBeNull();
    const long = 'x'.repeat(41);
    expect(formFieldError(text({ pattern: long }), textDraft('y'))).toBe(`Must match ${'x'.repeat(39)}…`);
  });
});

describe('form answer', () => {
  test('one entry per field in order; null while a field fails', () => {
    const form = formOf(choice({ required: true }), text({ input_type: 'number', value: '3' }), text({ name: 'n', multiline: true }));
    let drafts = initialFormDrafts(form);
    expect(formAnswer(form, drafts)).toBeNull();
    drafts = [toggleFormOption(form.fields[0]!, drafts[0]!, 1), drafts[1]!, textDraft('line 1\nline 2')];
    expect(formAnswer(form, drafts)).toEqual({
      type: 'form',
      fields: [
        { type: 'choice', checked: [1], other: null },
        { type: 'text', text: '3' },
        { type: 'text', text: 'line 1\nline 2' },
      ],
    });
  });

  test('a form label names its first question', () => {
    const request: api.UiRequestView = {
      request: 'u-1',
      kind: 'form',
      title: 'Ask',
      message: null,
      timeout_ms: null,
      options: [],
      multi: false,
      initial: null,
      prefill: null,
      questions: [],
      approve: null,
      form: formOf(choice()),
    };
    expect(answerLabel(request, { type: 'form', fields: [] })).toBe('Answer “Pick a color?”');
    expect(answerLabel(request, { type: 'cancel' })).toBe('Dismiss “Pick a color?”');
    expect(answerLabel({ ...request, form: formOf() }, { type: 'cancel' })).toBe('Dismiss “Ask”');
  });
});
