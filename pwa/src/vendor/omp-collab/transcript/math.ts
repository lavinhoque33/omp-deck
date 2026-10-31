// Vendored from oh-my-pi packages/collab-web/src/components/transcript/math.ts @ 1ff12a7873c8d2eda8bb4447dd9df4dbd10377ce (MIT, see pwa/THIRD_PARTY_LICENSES).
// Local change: KaTeX loads lazily (`loadKatex`); until it is loaded a span renders as its escaped
// source and `takeMathPending()` tells the caller to load it and render again.
import type { MarkedExtension, Tokens } from "../pi-utils/marked";
import {
	type MathSpan,
	mathBlockInContext,
	mathSpanInContext,
	mathStartIndex,
} from "../pi-utils/math-delimiters";
import { escapeHtml } from "../lib/format";
import type { renderToString as RenderToString } from "katex";

let renderToString: typeof RenderToString | null = null;
let loading: Promise<void> | null = null;
let pending = false;

/** Loads KaTeX once (a separate chunk). */
export function loadKatex(): Promise<void> {
	loading ??= import("katex").then(module => {
		renderToString = module.renderToString;
	});
	return loading;
}

/** Whether a render since the last call met math while KaTeX was not loaded yet. */
export function takeMathPending(): boolean {
	const was = pending;
	pending = false;
	return was;
}

/**
 * A backtick between the delimiters means the run is prose that merely looks
 * like math (`$x `code` y$`): KaTeX cannot express a code span, so typesetting
 * would swallow it and print an error instead.
 */
function typesettable(span: MathSpan): boolean {
	return !span.body.includes("`");
}

function renderMath(token: Tokens.Generic): string | false {
	if (token.type !== "math" || typeof token.text !== "string" || typeof token.display !== "boolean") return false;
	if (renderToString === null) {
		pending = true;
		return `<span class="tr-math-src">${escapeHtml(typeof token.raw === "string" ? token.raw : token.text)}</span>`;
	}
	try {
		const math = renderToString(token.text, {
			displayMode: token.display,
			// KaTeX's HTML output needs katex.min.css, which bundles to ~940 KB
			// gzipped here because Bun inlines its 60 @font-face sources.
			output: "mathml",
			throwOnError: false,
			// `trust: false` refuses \href, \htmlClass, \includegraphics.
			strict: false,
			trust: false,
		});
		// A display equation gets a wrapper to scroll in (`.tr-math`), because
		// scrolling the `math` element itself traps a few pixels of vertical scroll:
		// its ink — fraction bars, radical overlines — overflows its own box.
		return token.display ? `<span class="tr-math">${math}</span>` : math;
	} catch {
		// KaTeX threw outside its own error handling (e.g. macro expansion limit):
		// show the source rather than let one span blank the whole message.
		return escapeHtml(typeof token.raw === "string" ? token.raw : token.text);
	}
}

/**
 * Renders LaTeX in transcript Markdown: `$…$` and `\(…\)` inline, `$$…$$` and
 * `\[…\]` in display mode, plus own-line `$$`/`\[` blocks. Delimiters and the
 * scan hint come from `@oh-my-pi/pi-utils/math-delimiters`, so this renderer and
 * the TUI agree on what counts as math; only presentation policy lives here.
 *
 * Two limits follow from that shared behavior, both matching the TUI: a rejected
 * opener hides later spans on its line ("it costs $5, and the growth is $x^2$"
 * typesets nothing, since marked drops a start hint at its own position), and a
 * display block whose body contains a blank line must be preceded by one —
 * attached blocks are tokenized by the inline rule, which a blank line ends.
 */
export const mathExtension: MarkedExtension = {
	extensions: [
		{
			name: "math",
			level: "block",
			// No `start` hint: this parser probes block extensions only at a block
			// boundary and never consults their hints.
			tokenizer(source) {
				const block = mathBlockInContext(this, source);
				if (!block) return undefined;
				return { type: "math", raw: block.raw, text: block.body, display: true };
			},
			renderer: renderMath,
		},
		{
			name: "math",
			level: "inline",
			startFrom: mathStartIndex,
			tokenizer(source) {
				const span = mathSpanInContext(this, source);
				if (!span || !typesettable(span)) return undefined;
				return { type: "math", raw: source.slice(0, span.end), text: span.body, display: span.display };
			},
			renderer: renderMath,
		},
	],
};
