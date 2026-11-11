import { describe, expect, test } from 'bun:test';
import { buildModelList, type ModelListView } from '../src/app/screens/conversation/modelList.ts';
import type { api } from '../src/remote/index.ts';

const model = (provider: string, id: string, name: string): api.Model => ({
  selector: `${provider}/${id}`,
  provider,
  id,
  name,
  context_window: null,
  max_tokens: null,
  reasoning: false,
  thinking: [],
  input: ['text'],
  cost_input: null,
  cost_output: null,
});

// As the gateway sends them: providers alphabetical, newest first inside each.
const CATALOG = [
  model('fake', 'fake-10', 'Fake Ten'),
  model('fake', 'fake-2', 'Fake Two'),
  model('fake', 'fake-1', 'Fake One'),
  model('Other', 'other-1', 'Other One'),
];

const ids = (view: ModelListView) => view.groups.map((group) => group.models.map((entry) => entry.id));

describe('model list', () => {
  test('keeps server order and distinct provider identifiers', () => {
    const view = buildModelList([...CATALOG, model('FAKE', 'fake-0', 'Fake Zero')], [], null, '');
    expect(view.providers).toEqual([
      { value: 'fake', count: 3 },
      { value: 'Other', count: 1 },
      { value: 'FAKE', count: 1 },
    ]);
    expect(ids(view)).toEqual([['fake-10', 'fake-2', 'fake-1'], ['other-1'], ['fake-0']]);
  });

  test('an unknown saved provider reads as all providers', () => {
    const view = buildModelList(CATALOG, [], 'vanished', '');
    expect(view.provider).toBeNull();
    expect(ids(view)).toEqual([['fake-10', 'fake-2', 'fake-1'], ['other-1']]);
  });

  test('a saved provider filters by the exact identifier, as on desktop', () => {
    const view = buildModelList(CATALOG, [], 'Other', '');
    expect(view.provider).toBe('Other');
    expect(ids(view)).toEqual([['other-1']]);
  });

  test('favorites lead in starring order, repeat under their provider, and keep missing ones', () => {
    const view = buildModelList(CATALOG, ['Other/other-1', 'gone/old', 'fake/fake-1', 'gone/old'], null, '');
    expect(view.favorites).toEqual([
      { selector: 'Other/other-1', model: CATALOG[3] ?? null },
      { selector: 'gone/old', model: null },
      { selector: 'fake/fake-1', model: CATALOG[2] ?? null },
    ]);
    expect(ids(view)[0]).toEqual(['fake-10', 'fake-2', 'fake-1']);
  });

  test('search is AND over provider, id and name, and narrows favorites too; sorting is not redone', () => {
    const view = buildModelList(CATALOG, ['fake/fake-1', 'gone/fake-ten'], null, '  FAKE  t ');
    expect(ids(view)).toEqual([['fake-10', 'fake-2']]);
    expect(view.favorites).toEqual([{ selector: 'gone/fake-ten', model: null }]);
    expect(buildModelList(CATALOG, [], null, 'fake other').groups).toEqual([]);
  });

  test('the provider filter also hides favorites of other providers', () => {
    const view = buildModelList(CATALOG, ['fake/fake-1', 'gone/old'], 'fake', '');
    expect(view.favorites.map((entry) => entry.selector)).toEqual(['fake/fake-1']);
  });
});
