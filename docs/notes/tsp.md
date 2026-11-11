# Tern Surface Protocol (TSP) — facts for OmpDeck's native chat view

Sources: spec https://docs.stencil.so/tern/protocol/ (transport.md, handshake.md, documents.md, surfaces.md,
operations.md, input.md); omp source (local checkout `~/PersProject/oh-my-pi` is **18.12.6**; installed omp is
**18.15.0** → wire types must come from tag `v18.15.0`, `packages/wire/src/tsp.ts`, fetched from
`https://raw.githubusercontent.com/can1357/oh-my-pi/v18.15.0/packages/wire/src/tsp.ts`); omp-studio
(https://github.com/DawnJson/omp-studio, MIT, Electron) `docs/architecture/tsp-migration.md`,
`tsp-real-omp-probes.md`, `src/main/tsp/{tsp-session,stream}.ts`, `src/shared/tsp/document.ts`.
Line numbers below are the 18.12.6 checkout unless stated.

## What it is
omp (any build since 18.4) can describe its UI as a tree of semantic nodes instead of painting ANSI cells. A
terminal that answers the `hello` probe gets the whole interactive omp (transcript, tool cards, composer, pickers,
approvals, subagents, `/btw`, `/settings` pages) as JSON frames and draws it natively. Anything omp cannot describe
is sent as a `rows` node (pre-rendered ANSI lines at `cols`), so nothing is lost. Licenses: omp MIT, Tern SDKs MIT
(`stencil-hq/tern-sdk`), spec has no explicit license; omp-studio (MIT) already plays the terminal role.

## Framing (transport.md; `packages/tui/src/native/encode.ts`)
- `ESC _ tsp ; <verb> [; key=value]* ; <body> ESC \` — APC string, 7-bit ST only. Body UTF-8 JSON, base64 for `b`.
- Program → terminal: `q` query, `o` open surface, `f` frame, `b` blob, `s` stylesheet, `x` close (`t` palette:
  obsolete, ignore). Terminal → program (written to the PTY input): `r` reply, `e` event; never chunked.
- Windows ConPTY may drop APC on **input**; the spec's alternative is `ESC ] 877 ; tsp ; … ESC \` (or BEL); omp
  accepts both (`terminal.ts:50-54, 90-96`). omp-studio found APC input works through the sideloaded ConPTY.
- Chunking: bodies over the negotiated `apc` limit (default 65 536) are split; every chunk has `c=<id>`, all but the
  last `m=1`; chunks are back to back; a sequence is dropped after 1 s idle or when another TSP message interrupts
  it. One APC string ≤ 262 144 bytes. Chunks may split inside a UTF-8 character (parse bytes, not strings).
- Unknown verbs/fields ignored both ways; "new fields are optional forever"; incompatible change bumps `v`.

## Handshake (handshake.md; `terminal.ts:1939-2030`, `tui.ts:1640-1760`)
- omp writes `ESC _ tsp;q;{"q":"hello","v":[1],"app":"omp","ver":…,"features":["edit","undo","send"]} ESC \`
  followed by `ESC [ c` (DA1). A TSP terminal must send `tsp;r;{"r":"hello","v":1,…}` **before** its DA1 reply;
  DA1 first ⇒ no TSP. Reply `v != 1` ⇒ no TSP.
- **omp probes in every terminal** outside tmux unless blocked: `PI_TUI_NATIVE=0`, a `tspPathBlocker` (screen,
  zellij, devenv reload shell), the bun test runtime. `PI_TUI_NATIVE=1` forces the probe past blockers (not tmux).
  ⇒ Today every omp in an OmpDeck host session already probes; the host's alacritty answers DA1 only, so omp stays
  classic. Whether a session is native is decided by **whether the terminal answers hello**.
- `TERM_PROGRAM=tern` ⇒ optimistic start (first frame already native; falls back after 1 s without hello).
  Without it omp holds rendering ≤ 300 ms waiting for the reply.
- Inside tmux omp requires the session option `@tern-tsp` (`tmux show-options -qv @tern-tsp`), sets
  `allow-set-title off` on the pane, sends hello without DA1 and waits 1 s. No DCS passthrough wrapping; tmux does
  not forward the APC to an ordinary attached client ⇒ **the tmux backend cannot carry TSP as OmpDeck runs it**.
- omp-studio's hello reply: `r:"hello", v:1, term, ver, kinds: TSP_KINDS minus json, features:
  ["blobs","settle","adopt","dock","reduce-motion"], apc:65536, credits:2, cols, dark, reduceMotion, hour12`.
  Never write TSP events into the PTY before hello is answered (they get shredded; fold initial cols/theme into the
  reply instead).
- New agent dirs draw a setup wizard (`omp.app.setup`) in the first session; omp-studio sets
  `startup.setupWizard false` once.

## Documents and frames (documents.md, surfaces.md, operations.md; `wire/src/tsp.ts`)
- Node `{"id","k","p"?,"c"?}`; ids program-chosen, unique per surface (omp: base-36 counter, **not stable across
  runs** — find nodes by `k` + `p.role`, e.g. `k=="editor" && p.sendable`).
- Surfaces: `inline` (omp session, role `omp.session`), `screen` (full-screen pages, `/settings`), `flow`. Each root
  has regions `main` (transcript), `dock` (composer, status, HUD), `layer` (overlays, pickers, toasts).
- Frame `{"sf","s","ops":[…]}` applied atomically per op; a bad op is rejected alone with an `error` event carrying
  its `op` index. Ops: `add`, `set` (shallow merge, `null` deletes a prop), `text` (append/replace), `splice`
  (**UTF-16 offsets**), `move`, `del`, `settle`, `focus`, `reveal`, `scroll`, `suspend`, `resume`.
- Kinds (18.12.6 `TSP_KINDS`, 43): col row card section rule spacer text md code diff ansi math image media kv table
  tree badge kbd icon spinner shimmer elapsed progress rate list item tabs editor input status seg overlay toast rows
  picker prefs tool checklist agent chart meter effort (18.14 adds `json`). omp actually emits ~30.
- Flow control: ≤ `credits` unacknowledged frames per surface (default 2); `ack {sf,s}` covers every frame ≤ s.
  omp coalesces pending ops while waiting; after 5 s it warns ("terminal stopped acknowledging frames") and resumes.
  **A detached host must keep acking.**
- Blobs: `ESC _ tsp;b;mime=image/png;<base64> ESC \`, id = lowercase hex SHA-256 of the bytes, ≤ 16 MiB; query
  `{"q":"blobs","ids":[…]}` asks which the terminal already has; optional `TERN_BLOB_DIR` shared directory.
- Reattach (spec "Persistence"): replay a synthetic `hello`, each `o`, `s`, one full `f` + `settle`s, `x keep`.

## Input and events (input.md; `TspEvent` in `wire/src/tsp.ts`, 16 kinds)
- The composer is omp's `editor` node in `dock`; plain keys still go to omp as PTY bytes and omp answers with
  `text`/`splice`/`set`. With omp's features: `edit {id,from,to,text,cursor,len}`, `undo {id}`, `send {id,text}`
  (only while `sendable:true`), `focus {id}`.
- Pointer: `toggle`, `select`, `activate`, `action {act,value?,mods?}`, `change`; a node's `actions` prop maps
  gestures (click, dblclick, menu) to action names. View: `ack`, `resize`, `theme`, `motion`, `visible`, `error`,
  `gone`. Scrolling/hover/folding/copy are terminal-local.
- Observed in omp-studio: approvals = list + buttons (`activate` / `action approve`); ask (≤ 18.17) = `activate`; model picker
  = `action status.model` then `activate`; thinking = `action thinking.cycle`; `/` menu = overlay at the caret;
  omp's global shortcuts need raw key bytes (⇧Tab `ESC [ Z`, ⌥P `ESC p`).
- `--resume` makes omp re-send the whole conversation as nodes (per-turn usage footers not replayed since 18.12.4).
- Other side channels omp-studio parses: OSC 7501 run state (`state=idle|working|done|error|blocked:kind=…`),
  OSC 1337 `SetUserVar=omp_session_file=<b64>`, OSC 0/2, OSC 7.

## Recording
`PI_TUI_TSP_RECORD=<file>` appends JSONL `{"t":ms,"dir":"out"|"in","verb","params","body"}` per message (blobs as
full base64), only once TSP is live. omp-studio records fixtures in a temp `PI_CODING_AGENT_DIR` with a fake
hello/ack harness and replays them through a fake omp.

## OmpDeck integration facts
- alacritty/vte 0.15 swallows APC silently (`SosPmApcString` state, no `Perform` callback) ⇒ TSP must be scanned
  beside the parser: deck-term `scan.rs` `OutputScan` (has an `ApcScanner` for kitty graphics, `graphics.rs:178`),
  deck-host `Core::feed` (`daemon.rs:615`).
- deck-host answers every terminal query itself (`Core::answer_queries` → `Core::replies` → `send_replies`), attached
  or not ⇒ the host is where hello must be answered (ahead of the DA1 reply from the same chunk) and frames acked.
- Host snapshots carry only the alacritty grid; TSP state must be kept beside it for reattach (later step).

## Pages: omp's fullscreen overlays are `screen` surfaces (`chat_view/page.rs`)
- Fullscreen overlays (the git app from the composer's git segment `status.git`, `/git`, `/models`, `/settings`) make
  omp's native backend (`packages/tui/src/native/backend.ts` `render()`) open a **second surface with
  `mode:"screen"`**, same `role:"omp.session"`, with its own `main`/`dock`/`layer` regions. The inline `s:1` stays
  open but frozen, and omp routes **every key and click to the overlay**: a view that keeps drawing the inline surface
  looks dead until the overlay is closed (this was the "branch icon freezes the chat" bug).
- The agents hub is *not* one of these pages: with `picker` advertised it is a `picker` node in the inline `layer`
  (contracts "Wave 22").
- The view therefore keeps two `Snap`+`Local` pairs (node ids such as `main`, `layer`, `s` collide across surfaces):
  `Follow::Session` = the open inline `omp.session`, `Follow::Page` = the newest open non-inline surface. While a page
  is open (and the process has not exited) `show()` draws it instead of the transcript and dock, and TSP events are
  addressed to the page's `sf` (`Cx::sf()`). Keys stay raw PTY bytes through `Terminal::route_keyboard`.
- The page is drawn by the same node renderers as the session, plus `page.rs`: `button` (omp 18.15.0 sends kind
  `button` for toolbar icons/quiet buttons; 18.12.6 used `icon` + role `omp.app.ibtn`), `omp.app.git` as a two-pane card
  layout (main: bar + diff, side: changes + commit form), `row justify:"between"`, segmented `tabs` (`select` events whose
  key ends `views` are handled by omp). Anything omp could not describe arrives as `rows` (ANSI lines) and is drawn too.
- An OmpDeck-owned strip along the top is always drawn (page title + "Back to chat"). Its button sends omp an Esc,
  exactly as typing it does (omp's overlays close on Esc/`q`; the git page also has its own `close` action), so a page
  nothing here can render is still dismissible. omp closes the page with `x s:2`; the transcript and composer return.
- Grid columns (`resize`) follow the page body width while a page is open (omp's git layout switches to
  `omp.app.git-narrow` on `cols`).
- Not covered: `deck-service` injection (`composer::TspComposer::of`) still reads the frozen inline surface while a
  page is open.

## Fold chevrons (`tools::foldable`, `render::card`/`section`, `tools::agent`)
- omp sets `collapsible: true` on every tool with a body (`tool-execution.ts` `collapsible: hasBody`), but its terminal
  only offers to expand where something is left out ("… N more lines (ctrl+o to expand)"). The chevron (and the
  pointer cursor) is therefore drawn only when folding changes what the node shows:
  - tool `preview: "none"`: always (the fold hides the body);
  - tool `{lines}`/`{tail}`/`"children"`: the measured body is taller than the tool clamp, an `ansi`/`code` block in
    it is longer than its own `preview` (reported via `Cx::note_fold_cut` while the body is measured), or omp shortened
    the folded body itself — a `text` node counting what was left out (`3 more files`, `+2 more`, `4 earlier`: read
    previews, grep, find, eval status) or a nested collapsible node. Open tools reuse the answer from when they were
    last drawn folded (`Local::fold_seen`); a tool never seen folded keeps the chevron;
  - card/section: folded, or open with a non-blank body (omp may omit a folded body, e.g. synthetic user messages);
  - agent: a non-blank body (omp describes it folded or not).
- A head click still toggles a collapsible node without a chevron (a tool's declared click action wins), so renderers
  that show more unfolded without saying so (`find`'s extra ranges) stay reachable.

## The ask form (omp 18.18+, `packages/tui/src/html-form/dialog.ts` `#describeForm`/`handleNativeEvent`)
- omp 18.18's `ask` tool takes one `html` `<form>` and draws `HtmlFormDialog` in the dock:
  `col omp.editor.ask` > `row omp.ask.head` (`tabs omp.ask.questions`, one tab `t<N>` per question + `review`) >
  `list omp.ask.options` > `col omp.ask.form` > `section`/`col omp.ask.section` > `col omp.ask.field` > (`text
  omp.ask.label` | context nodes: table, code, bullets | `list omp.ask.options` of `item omp.ask.option` (radio,
  also `<select>`) / `item omp.ask.check` (checkbox), checked = tone `accent`, Other = item `…/other` | `text
  omp.ask.well[.multi]` (an unfocused text field, `lines` = rows) | the live `input`), then `row omp.actions`.
  Hidden: `row` with `omp.ask.summary` (Tern grows it) + Cancel/Show. Review tab: kv rows + notice.
- The outer `omp.ask.options` list holds a `col`, not items: `render.rs::list()` draws non-item children as nodes
  (Tern does) — drawing them as row labels was the "ask shows no options" bug. That list scrolls, capped at half the
  chat height (Tern: 60vh), so the action bar stays in view.
- Events: `select {id: inner list, item: full item id}` picks a radio, **toggles** a checkbox, or (Other) focuses the
  Other input; `activate` does not toggle. Tabs: `select` on the tabs node, item `t<N>`/`review` (the tabs node
  carries `actions.click: select`, which must not become a node-wide click). Wells: `action edit:<field index>`;
  omp answers with a live `input` node and the surface `focus` on it; typing = raw keys. Buttons: `action`
  `next`/`submit`/`cancel`/`hide`/`show`.
- Fixture `crates/omp-deck/src/chat_view/ask-form.json` (recorded from omp 18.18.6, ids prefixed `q`): cases
  `initial`, `check_go`, `name_typed`, `other_typed`, `review`, `hidden`.
- Completed answers render as a previewed `omp.tool.ask` tool containing a section and answer rows. Measure that
  body invisibly with its normal layout, not an egui sizing pass: sizing removes cross-axis centering and makes
  the preview clip shorter than the rendered answer. Copy collection discards the probe's text runs.
  `ask-answer.json` captures a completed response; the clipping regression checks the full answer and following
  reply at wide and narrow pane widths. Native Xvfb smoke on omp 18.18.7 also shows the subsequent eval result and
  final reply in full.
- The phone answers it through the ctl extension's `formDialog` wrap, not TSP (`docs/notes/mobile-pwa.md`
  "HTML-form ask").
