// Lazy syntax highlighting for the vendored tool renderers: highlight.js's core build with the
// languages the coding agent's tools show most, loaded on first need. The renderers read the
// highlighter from `globalThis.hljs` (the upstream `getHljs` seam).

type Listener = () => void;

let loading: Promise<void> | null = null;
let loaded = false;
const listeners = new Set<Listener>();

/** Whether the highlighter is installed. */
export function highlightReady(): boolean {
  return loaded;
}

/** Calls `listener` once the highlighter is installed (returns the unsubscribe). */
export function onHighlightReady(listener: Listener): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Loads highlight.js and its languages once; later calls share the same promise. */
export function loadHighlight(): Promise<void> {
  loading ??= (async () => {
    const [core, ts, js, py, rust, go, bash, json, yaml, css, xml, diff, sql, ini, markdown, cpp, java] =
      await Promise.all([
        import('highlight.js/lib/core'),
        import('highlight.js/lib/languages/typescript'),
        import('highlight.js/lib/languages/javascript'),
        import('highlight.js/lib/languages/python'),
        import('highlight.js/lib/languages/rust'),
        import('highlight.js/lib/languages/go'),
        import('highlight.js/lib/languages/bash'),
        import('highlight.js/lib/languages/json'),
        import('highlight.js/lib/languages/yaml'),
        import('highlight.js/lib/languages/css'),
        import('highlight.js/lib/languages/xml'),
        import('highlight.js/lib/languages/diff'),
        import('highlight.js/lib/languages/sql'),
        import('highlight.js/lib/languages/ini'),
        import('highlight.js/lib/languages/markdown'),
        import('highlight.js/lib/languages/cpp'),
        import('highlight.js/lib/languages/java'),
      ]);
    const hljs = core.default;
    const languages: [string, typeof ts.default][] = [
      ['typescript', ts.default],
      ['javascript', js.default],
      ['python', py.default],
      ['rust', rust.default],
      ['go', go.default],
      ['bash', bash.default],
      ['json', json.default],
      ['yaml', yaml.default],
      ['css', css.default],
      ['xml', xml.default],
      ['diff', diff.default],
      ['sql', sql.default],
      ['ini', ini.default],
      ['markdown', markdown.default],
      ['cpp', cpp.default],
      ['java', java.default],
    ];
    for (const [name, language] of languages) hljs.registerLanguage(name, language);
    hljs.registerAliases(['html', 'svg'], { languageName: 'xml' });
    hljs.registerAliases(['c'], { languageName: 'cpp' });
    (globalThis as { hljs?: unknown }).hljs = hljs;
    loaded = true;
    for (const listener of [...listeners]) listener();
  })().catch((error: unknown) => {
    // A failed chunk load (offline): allow a later retry; code stays plain meanwhile.
    loading = null;
    throw error;
  });
  return loading;
}
