// The model picker's list rules, shared with the desktop picker: the gateway sends models already
// grouped by provider and newest-first (never re-ranked here), favorites come first in starring
// order and again under their provider, and the provider filter and the search both narrow it.

import type { api } from '../../../remote/index.ts';

/** One provider in the dropdown. `value` is what is saved as the preference. */
export interface ProviderOption {
  value: string;
  count: number;
}

/** A favorite row: the model when it is available now, else just the saved selector. */
export interface FavoriteEntry {
  selector: string;
  model: api.Model | null;
}

export interface ProviderGroup {
  provider: string;
  models: api.Model[];
}

export interface ModelListView {
  /** Dropdown entries after "All providers", in the gateway's provider order. */
  providers: ProviderOption[];
  /** The provider filter in effect (an unknown saved provider reads as all); null = All providers. */
  provider: string | null;
  favorites: FavoriteEntry[];
  groups: ProviderGroup[];
}

/** Case-insensitive AND of whitespace-separated words over `haystack`. */
export function matchesWords(words: readonly string[], haystack: string): boolean {
  const text = haystack.toLowerCase();
  return words.every((word) => text.includes(word));
}

export function buildModelList(
  models: readonly api.Model[],
  favorites: readonly string[],
  savedProvider: string | null,
  query: string,
): ModelListView {
  const providers: ProviderOption[] = [];
  for (const model of models) {
    const entry = providers.find((candidate) => candidate.value === model.provider);
    if (entry === undefined) providers.push({ value: model.provider, count: 1 });
    else entry.count += 1;
  }
  const provider = savedProvider === null ? null : (providers.find((entry) => entry.value === savedProvider)?.value ?? null);
  const words = query.toLowerCase().split(/\s+/).filter((word) => word.length > 0);
  const visible = (model: api.Model): boolean =>
    (provider === null || model.provider === provider) &&
    matchesWords(words, `${model.provider} ${model.id} ${model.name}`);

  const starred: FavoriteEntry[] = [];
  const seen = new Set<string>();
  for (const selector of favorites) {
    if (seen.has(selector)) continue;
    seen.add(selector);
    const model = models.find((candidate) => candidate.selector === selector) ?? null;
    const shown =
      model !== null
        ? visible(model)
        : (provider === null || selector.split('/')[0] === provider) && matchesWords(words, selector);
    if (shown) starred.push({ selector, model });
  }

  const groups: ProviderGroup[] = [];
  for (const model of models) {
    if (!visible(model)) continue;
    let group = groups.find((candidate) => candidate.provider === model.provider);
    if (group === undefined) {
      group = { provider: model.provider, models: [] };
      groups.push(group);
    }
    group.models.push(model);
  }
  return { providers, provider, favorites: starred, groups };
}
