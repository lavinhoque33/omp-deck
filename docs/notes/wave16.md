# Wave 16 — theme gallery (16 dark styles, Blueprint default)

Source of truth for every value: `docs/notes/theme-mockups.html` (one page, `#t1`…`#t16` select the
style; base CSS + per-style `body.tN` tokens + `.tN …` structure rules). Rendered references (1600×1000):
`~/.cache/theme-mockups/{1-zinc-contrast … 6-graphite-islands, 07-aurora … 16-two-tone-frame}.png`,
overviews `0-overview.png` (1–6) and `0b-overview-7-16.png` (7–16). Each shows the same scene: title bar +
tabs (active, hovered, plain), sidebar (Open tabs, scope chip, search, groups, selected + hovered row),
Agents dashboard (active card with the subagent accordion expanded, hovered busy card, needs-input card,
ended card), status bar.

## Decisions (user-approved: "add all of them as separate theme choices, default Blueprint")

- 16 new `UiStyle`s next to Neutral and Neon (18 total). The new ones are **dark-only**; Neutral and Neon
  keep light variants. With a dark-only style active the Light choice is disabled everywhere with the reason
  "<Style> is dark-only"; the effective theme is dark (`AppConfig::dark()`).
- Default style **Blueprint**. One-time migration (deck-core `parse_config`): a state file without
  `style_rev` whose style is Neutral (or absent) and whose theme is not Light becomes Blueprint; light-mode
  Neutral users keep Neutral. `style_rev` = 1 afterwards, so a later explicit Neutral choice sticks.
- Every new style is a member of the flat family (Neutral's widget logic, Geist UI font) plus per-style
  structure. `theme::neutral()` becomes `theme::flat()` (= not Neon).
- Terminal colors follow the style (`deck_term::TermTheme::Custom`): background = the style's canvas `bg`,
  foreground = `text`, cursor = `text_strong`, selection = mix(bg, accent, 0.35); ANSI = Neutral dark's.
- Settings → Appearance: the Neutral/Neon segmented control becomes an 18-tile gallery (painted mini
  previews). Command palette: one "Style: <name>" command per style (`style.<key>`).

## Token mapping (CSS → `theme::Palette`)

| CSS | Palette field | Meaning |
|---|---|---|
| `--main` | `bg` | canvas behind dashboard / start screen; terminal background |
| `--side` | `panel` | sidebar fill (egui `panel_fill`) |
| `--chrome` | `bar` | title bar, tab row, status bar |
| `--win` | `window` (new) | window backdrop (visible in Islands gaps / Two-Tone frame) |
| `--card` → `--card2` | `surface` → `surface_end` (new) | card fill, top→bottom gradient when they differ |
| `--raised` | `raised` | chips, selected rows, chevron chips |
| `--line` | `stroke` | borders of cards, fields, rows |
| `--sep` | `separator` (new) | region separators (sidebar \| main, tab row, status bar) |
| `--hl` | `hover_line` (new) | hover outline (`theme::hover_stroke`) |
| `--hfill` | `hover_bg` (new) | hover fill (`theme::hover_fill`) |
| `--tray` / `--trayline` | `tray` / `tray_line` (new) | subagent accordion fill / border |
| `--text`, `--strong`, `--weak`, `--glyph` | `text`, `text_strong`, `weak`, `glyph` | |
| `--accent`, `--accent2` | `accent` (= `accent_a`), `accent_b` | `accent_b` = `--accent2` or `--accent` |
| `--ok`, `--warn`, `--danger` | same | |
| `--r` | look: card radius | rows/tabs keep 6 unless the style overrides |
| `--shadow`, `--hi` | look: card drop shadow, top highlight | |

Translucent CSS values (Obsidian Glass, Teal Signal tray, rgba glows) may be pre-blended over their backdrop
into opaque `Color32`s, except glows/washes which are painted translucent.

## Per-style structure (what must be visible; values in the CSS)

| # | Style (`key`) | Canvas | Regions | Cards | Hover | Subagent accordion | Other |
|---|---|---|---|---|---|---|---|
| 1 | Zinc Contrast (`zinc_contrast`) | flat | 1 px bright separators | flat r8 | outline `hl` + fill | **Boxed**: bordered box, header band `#1a1a1d`, divider when open, square raised chevron, inner padded rows | |
| 2 | Layered Depth (`layered_depth`) | flat | sidebar lighter than main, casting a soft shadow onto it | gradient + top highlight + drop shadow, r10 | outline | **Band**: edge-to-edge sunken footer band (top border, inner top shadow), chevron chip with highlight, rows gradient + highlight | active tab filled with the sidebar color |
| 3 | Teal Signal (`teal_signal`) | top teal wash (260 px) | separators + teal line under the tab row | flat r8; active = teal border + 3 px teal ring | teal outline | **Boxed** teal-tinted (teal tray + teal border), teal chevron chip, strong header text, rows hang off a teal gradient rail | selection = teal tint + teal border; active tab teal border/fill |
| 4 | Slate Ink (`slate_ink`) | flat | separators | blue-slate, gradient + highlight + shadow, r10 | 1 px `hl` ring | **Band** list: edge-to-edge footer, rows separated by divider lines (no row boxes), outlined chevron | |
| 5 | Mono Wire (`mono_wire`) | flat black | grey separators | outline-first, near-black fill, r6 | almost-white ring | **Dashed**: dashed divider, mono caps letter-spaced header, outlined chevron, outlined rows | inactive tabs outlined; chips/badges outlined, transparent |
| 6 | Graphite Islands (`graphite_islands`) | flat | **Islands**: sidebar and main are separate rounded (r12) panels with a 6 px gap on the window backdrop, bordered, highlight + shadow | gradient, lighter top border, big shadow, r12 | ring + deeper lift | **Pill**: pill-shaped header button (round chevron, hint inside); expanded rows in a nested bordered panel below it, joined by a connector line | active tab filled + highlight |
| 7 | Aurora (`aurora`) | violet + cyan radial glows at the top | separators | r12, highlight + shadow; active and hovered cards get a violet→cyan **gradient border**; active also a violet ring + glow | gradient border | **Boxed** r10, header washed violet→cyan, round gradient chevron (dark icon), gradient rail | gradient on: active-tab underline, selection bar, status stripe, context meter, "All" pill border, Agents chip border |
| 8 | Obsidian Glass (`obsidian_glass`) | white glow top + teal glow bottom-right | faint separators | translucent-looking panes, r16, bright top highlight, deep shadow; active = teal border + glow | white ring + deeper shadow | **Boxed** r12 glass + highlight, round chevron, rows r10 glass + highlight | tabs r9; fields/pills with highlight |
| 9 | **Blueprint** (`blueprint`, default) | **dot grid** (18 px, slate α .16) | separators | square (r3), flat; **double rule**: second outline 3 px outside every card; active = accent border + cyan outer rule | border `#67e8f9` + outer rule cyan | **Ruled**: no box; mono caps header + outlined accent chevron + dashed line filling to the right; expanded = ruled table (outer border, rows split by lines, child rows indented with a tree guide) | cyan accent `#22d3ee`; mono for labels (section headers, chips, badges, meters, paths, subagent ids); "AGENTS" title caps letter-spaced; r3 on rows, tabs, chips, pills |
| 10 | Ember (`ember`) | top orange wash (220 px) | warm separators | warm gradient + highlight + shadow, r10; active = orange border + 2 px orange top edge + glow | `hl` ring | **Boxed** r8, header orange wash, orange chevron chip | orange accent |
| 11 | Indigo Night (`indigo_night`) | flat | separators | r12 + highlight + shadow; active = indigo ring | indigo **glow** (ring + 28 px glow) | **Boxed** with a **status segment bar** in the header (one segment per subagent: running accent, done ok, failed danger, stopped warn), shown collapsed and expanded | |
| 12 | Phosphor (`phosphor`) | green **scanlines** (every 3 px) | green separators | r2; active = green glow + inner ring; badges outlined | green ring | **Dashed**: top divider, accent header text, outlined chevron, rows hang off a dashed tree line | whole UI in the monospace family (11.5 pt); Agents title "> agents" |
| 13 | Neo-Brutal (`neo_brutal`) | flat | 2 px separators | **2 px** borders, **hard offset shadows** (4,4 black; active 5,5 teal; needs-input 4,4 amber); r6 | white border + 6,6 white shadow, shifted −1,−1 | **Boxed** 2 px + 3,3 shadow, solid `#222` header, solid teal chevron block (black icon), 2 px rows | tabs 2 px boxes, active teal + 3,3 teal shadow; hovered row 2 px white |
| 14 | Rosé Noir (`rose_noir`) | rose glow top-right | separators | r14 + highlight + shadow; active = rose ring | rose ring + shadow | **Boxed** r12 drawer, rose header tint, round rose chevron, rows r10 | tabs r14; pink accent |
| 15 | Spotlight (`spotlight`) | teal spotlight glow **behind the active card** + soft top glow | **fading separators** (1 px lines that fade out at both ends) | gradient + highlight + strong shadow; idle cards dimmed to 82 %; active = teal border + 60 px glow | white ring | **Fade**: fading divider above the header, round chevron with highlight, rows float as cards with highlight + shadow | |
| 16 | Two-Tone Frame (`two_tone_frame`) | main canvas recessed: rounded top-left corner (r14), 1 px top/left highlight + inner shadow | **Frame**: title bar, tabs, sidebar and status bar share one graphite tone around a near-black canvas | **two-tone**: header band (title + path, `#222228`, bottom border) over a dark body | band lightens + border `hl` | **Band**: edge-to-edge band (top border, `#18181c`), divider when open | active tab is a folder tab joined to the canvas (canvas fill, no bottom border, 2 px accent top edge) |

Neutral (`neutral`) and Neon (`neon`) keep their current looks (Neutral includes the v0.10.1 border/hover
fix).
