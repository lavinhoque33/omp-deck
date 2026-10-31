// The context breakdown (`context` feed, omp's `/context`) as bar segments and shares.

import type { api } from '../../../remote/index.ts';

export interface Segment {
  id: string;
  label: string;
  tokens: number;
  /** Share of the context window, 0..1. */
  share: number;
  kind: 'category' | 'free' | 'buffer';
}

/** Categories (non-empty, largest first), then free space and the auto-compact buffer. */
export function contextSegments(view: api.ContextView): Segment[] {
  const window = Math.max(1, view.context_window);
  const share = (tokens: number) => Math.max(0, Math.min(1, tokens / window));
  const categories = view.categories
    .filter((entry) => entry.tokens > 0)
    .sort((a, b) => b.tokens - a.tokens)
    .map<Segment>((entry) => ({ id: entry.id, label: entry.label, tokens: entry.tokens, share: share(entry.tokens), kind: 'category' }));
  const out = [...categories];
  if (view.free_tokens > 0) out.push({ id: 'free', label: 'Free', tokens: view.free_tokens, share: share(view.free_tokens), kind: 'free' });
  if (view.auto_compact_buffer > 0) {
    out.push({
      id: 'buffer',
      label: 'Auto-compact buffer',
      tokens: view.auto_compact_buffer,
      share: share(view.auto_compact_buffer),
      kind: 'buffer',
    });
  }
  return out;
}

/** Where the auto-compact threshold sits on the bar (0..1), or null when off. */
export function thresholdMark(view: api.ContextView): number | null {
  if (view.threshold_tokens === null || view.context_window <= 0) return null;
  return Math.max(0, Math.min(1, view.threshold_tokens / view.context_window));
}

/** Used share of the window, 0..1. */
export function usedShare(view: api.ContextView): number {
  return view.context_window <= 0 ? 0 : Math.max(0, Math.min(1, view.used_tokens / view.context_window));
}

/** The `n` largest tool schemas. */
export function topTools(view: api.ContextView, n = 8): api.ToolTokens[] {
  return [...view.tools].sort((a, b) => b.tokens - a.tokens).slice(0, n);
}

/** "42%" with one decimal under 10%. */
export function percent(share: number): string {
  const value = share * 100;
  return `${value > 0 && value < 10 ? value.toFixed(1) : Math.round(value)}%`;
}
