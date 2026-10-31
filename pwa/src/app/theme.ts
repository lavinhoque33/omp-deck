// The six phone themes (the user's mockup directions 01–06). The theme is applied as
// `data-theme` on <html> and remembered on this device.

export interface ThemeInfo {
  id: ThemeId;
  name: string;
  /** Background, surface, accent and text colours for the settings swatch and `theme-color`. */
  swatch: readonly [string, string, string, string];
}

export type ThemeId = 'blueprint' | 'aurora' | 'signal' | 'margin' | 'tidal' | 'field';

export const THEMES: readonly ThemeInfo[] = [
  { id: 'aurora', name: 'Aurora glass', swatch: ['#0b0c19', '#292c49', '#b4a1ff', '#e4e4fa'] },
  { id: 'blueprint', name: 'Blueprint mobile', swatch: ['#08090b', '#1a1e25', '#22d3ee', '#cdd5df'] },
  { id: 'signal', name: 'Signal / 01', swatch: ['#090d0b', '#203126', '#c9ff45', '#e0efdc'] },
  { id: 'margin', name: 'Margin', swatch: ['#101010', '#2d2d2a', '#ff8a65', '#eeeae2'] },
  { id: 'tidal', name: 'Tidal', swatch: ['#11131b', '#343e50', '#a4c8ff', '#e4ebf8'] },
  { id: 'field', name: 'Field console', swatch: ['#0e100b', '#2c301e', '#d2e875', '#e4e9bf'] },
];

export const DEFAULT_THEME: ThemeId = 'aurora';

const STORAGE_KEY = 'ompdeck.theme';

export function isThemeId(value: unknown): value is ThemeId {
  return THEMES.some((theme) => theme.id === value);
}

export function themeInfo(id: ThemeId): ThemeInfo {
  return THEMES.find((theme) => theme.id === id) ?? (THEMES[0] as ThemeInfo);
}

/** The remembered theme, or the default when none (or an unknown one) is stored. */
export function loadTheme(storage: Pick<Storage, 'getItem'> | null): ThemeId {
  try {
    const stored = storage?.getItem(STORAGE_KEY) ?? null;
    return isThemeId(stored) ? stored : DEFAULT_THEME;
  } catch {
    return DEFAULT_THEME;
  }
}

export function saveTheme(storage: Pick<Storage, 'setItem'> | null, id: ThemeId): void {
  try {
    storage?.setItem(STORAGE_KEY, id);
  } catch {
    // Private mode or a full quota: the theme still applies for this visit.
  }
}

/** Applies `id` to the document (attribute and browser chrome colour). */
export function applyTheme(doc: Document, id: ThemeId): void {
  doc.documentElement.dataset['theme'] = id;
  const meta = doc.querySelector('meta[name="theme-color"]');
  if (meta !== null) meta.setAttribute('content', themeInfo(id).swatch[0]);
}
