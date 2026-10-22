# tmux 3.7c behavior for OmpDeck's embedded outer terminal (empirically verified)

Harness: `/tmp/ompx/harness.py` (class `Outer` = PTY 100x30 playing the outer terminal, auto-replies to queries; `tmux()` helper on socket `omp-deck-test-<pid>`; `cleanup()` = kill-server). Per-experiment drivers `/tmp/ompx/e*.py`, inner programs `inner_log.py` (raw tty, optional mode-request prefix, hex-logs stdin), `inner_p.py` (prints bytes), `inner_q.py` (sends query, logs reply). Final conf: `/tmp/omp-deck-notes/tmux.conf` (== `/tmp/ompx/final.conf`).
Launch used everywhere: `env -u TMUX TERM=xterm-256color COLORTERM=truecolor TERM_PROGRAM=omp-deck tmux -L omp-deck-test-$$ -f <conf> new-session -A -s t1 -c /tmp -- <inner>` under a PTY, TIOCSWINSZ 30x100.
Outer replies used ("alacritty_terminal-equivalent"): DA1 `\e[?6c`, DA2 `\e[>0;1;1c`, XTVERSION `\eP>|OmpDeck 0.1\e\\` (alacritty_terminal itself emits DA1 `\e[?6c` and DA2 `\e[>0;{ver};1c` — alacritty_terminal-0.26.0/src/term/mod.rs:1261,1267; it has NO XTVERSION reply).

## 0. TL;DR (what the app must do)
1. **The outer terminal MUST answer at least one of DA1 (`CSI c`), DA2 (`CSI > c`) or XTVERSION (`CSI > q`) promptly.** tmux holds back `CSI > 4;2 m` (modifyOtherKeys), `?1004h` (focus), `?7727h` (app-escape) until the first reply; with NO reply it sends them after **5.08 s** (measured) — Shift+Enter etc. would be dead for 5 s. alacritty_terminal answers DA1/DA2 automatically (via `Event::PtyWrite`) — forward those PtyWrite bytes to the PTY, that suffices. Replying XTVERSION additionally sets `#{client_termtype}` (omp reads it: terminal.ts → tmux.ts:48 `resolveTmuxClientTerminalName`), optional.
2. **tmux 3.7c does NOT implement the kitty keyboard protocol** (`CSI > Nu` push from inner app is ignored; `pane_key_mode` stays `VT10x`). Extended keys reach the inner app ONLY via modifyOtherKeys (`CSI > 4;2 m` from inner app → `pane_key_mode = Ext 2`). omp inside tmux: sends `\e[?u\e[c` → tmux silently ignores `CSI ? u`, answers DA1 `\e[?1;2;4c` → omp's DA1 sentinel (terminal.ts:1443-1446, regex :1266) or 150 ms timer (:1921-1925) → `\e[>4;2m` (terminal.ts:1897) → tmux enters Ext 2. So omp WILL be in modifyOtherKeys mode.
3. **Outer key encoding to send for Shift/Ctrl/Alt+Enter: CSI-u `\e[13;{mod}u`** (mod: Shift=2, Alt=3, Ctrl=5) — or xterm `\e[27;{mod};13~`; tmux parses BOTH identically (exp. 2). Alt+Enter as `\e\r` also works (→ `13;3u`). Never send plain `\r` for modified Enter; tmux can't recover the modifier. With `extended-keys-format csi-u` inner gets `\e[13;2u`/`\e[13;5u`/`\e[13;3u` (omp parses CSI-u first, then `27;m;k~`: keys.ts:468-520). Recommended: keep `csi-u` format.
4. **Lone Esc is delayed ≥500 ms by tmux 3.7c regardless of `escape-time`** (0/100/300 → 0.502 s; 1000 → 1.005 s; i.e. max(escape-time, 500 ms)). Fix: send Esc as **`\x1b O [`** (application-escape-key form; tmux requested `?7727h`). tmux converts `ESC O [` → `ESC` instantly (observed 0.05 s; works even before tmux got any reply). So: Esc key → bytes `1b 4f 5b`. Alt+Esc / Esc+other still sent normally (`\e\e`). Only when attached to tmux (always, for this app).
5. Right-click menus removed, prefix removed (see conf). Plain key presses do NOT exit copy-mode (exp. 6) — see risk note.
6. Startup: `tmux -L omp-deck -f <conf> new-session -A -s <name> -c <cwd> -- <cmd...>`; `-f` is honored ONLY if this invocation starts the server (exp. 11). For an already running server run `tmux -L omp-deck source-file <conf>` first (idempotent with the final conf, rc 0).

## 1. Conf parse check (tmux 3.7c)
`tmux -L S -f /dev/null start-server \; source-file -v proposed.conf` → every line parsed, rc 0, no errors. Option readback: `extended-keys-format csi-u`, `extended-keys on`, `escape-time 0`, `set-clipboard on`, `allow-passthrough on`, `prefix None`, `default-terminal tmux-256color`. Valid value sets (man tmux): `extended-keys-format [csi-u|xterm]` (csi-u: C-S-a → `^[[65;6u`; xterm: `^[[27;6;65~`), `allow-passthrough [on|off|all]`.
**Defects found in the proposed conf (all corrected in final conf):**
- `unbind -a -T prefix` and `unbind -n X` FAIL with `table prefix doesn't exist` (rc 1) on a 2nd `source-file` when nothing is left to unbind → use `-q` (`unbind-key [-anq]`): `unbind -aq -T prefix`, `unbind -nq MouseDown3Pane` ...
- `set -as terminal-features '...'` APPENDS a duplicate entry each `source-file` (3 re-sources → indices 3..6 identical). Use fixed index: `set -s terminal-features[100] '...'` → idempotent (verified, only [100] present after 3 re-sources; defaults [0..2] kept).
- `escape-time 0` does not remove the 500 ms lone-Esc floor (exp. 2b); keep it (harmless) but use `ESC O [`.
- Added `unbind -nq M-MouseDown3Status`, `M-MouseDown3StatusLeft` (status is off, harmless). Default root table after conf has 18 mouse-only bindings, no MouseDown3*.
- `allow-rename off` valid (window name only; OSC 0/2 titles still flow because `allow-set-title` default on).
Terminal features that matter (A/B test proposed vs conf without the terminal-features line, exp. 12):
| feature | without it | with it |
|---|---|---|
| extkeys | NO `\e[>4;2m` ever sent → Shift+Enter dead | sent |
| sync | no `?2026h/l` wrapping | wrapped |
| hyperlinks | OSC 8 dropped | forwarded (rewritten `id=tmux1`) |
| usstyle | `4:3`, `58:2:...` dropped | forwarded |
| RGB | truecolor still emitted when client env has `COLORTERM=truecolor` (tmux auto-detects) | emitted |
| clipboard, title, focus | already in tmux's built-in `xterm*` defaults | — |
Keep RGB anyway. NOTE the outer TERM=xterm-256color must be set on the tmux CLIENT process env (and COLORTERM=truecolor).

### Final conf (`/tmp/omp-deck-notes/tmux.conf`)
```
set -g status off
set -g prefix None
set -g prefix2 None
unbind -aq -T prefix
set -s escape-time 0
set -g mouse on
unbind -nq MouseDown3Pane
unbind -nq M-MouseDown3Pane
unbind -nq MouseDown3Status
unbind -nq MouseDown3StatusLeft
unbind -nq M-MouseDown3Status
unbind -nq M-MouseDown3StatusLeft
set -g history-limit 50000
set -s extended-keys on
set -s extended-keys-format csi-u
set -s set-clipboard on
set -g allow-passthrough on
set -g focus-events on
set -g set-titles on
set -g set-titles-string '#T'
set -g default-terminal tmux-256color
set -s terminal-features[100] 'xterm-256color:RGB:extkeys:clipboard:hyperlinks:usstyle:sync:focus:title'
set -g allow-rename off
set -g exit-empty on
```
Verified: `new-session -d -f final.conf` rc 0; 3× `source-file final.conf` on live server rc 0, stderr empty.
(`default-terminal tmux-256color`: pane gets `TERM=tmux-256color`, `COLORTERM=truecolor` (inherited from server env), `TERM_PROGRAM=tmux`, `TERM_PROGRAM_VERSION=3.7c`, `TMUX`, `TMUX_PANE`; `infocmp tmux-256color` present on this host.) `source-file -n` is parse-only and does NOT validate option names (bad option → rc 0); real `source-file` returns rc 1 + stderr `invalid option: bogus-option` but still applies the other lines.

## 2. Exp. 1 — what tmux writes at attach (outer replies none; 787 B in 2 s)
Hex-escaped, EL-NL rows collapsed:
```
\e[?1049h \e[22;0;0t \e[?1h \e= \e[H \e[2J \e[?12l \e[?25h
\e[?1000l \e[?1002l \e[?1003l \e[?1006l \e[?1005l
\e[?2004h  \e[?2031h  \e[?996n
\e(B \e[m \e[?12l \e[?25h \e[?1006l \e[?1000l \e[?1002l \e[?1003l \e[1;1H \e[1;30r
\e[c  \e[>c  \e[>q  \e]10;?\e\\  \e]11;?\e\\            <- queries: DA1, DA2, XTVERSION, OSC10, OSC11 (ST-terminated)
\e[1;1H ...\e[?1006l\e[?1000l\e[?1002l\e[?1003l \e[?1006h \e[?1000h \e[?1002h     <- mouse ON (mouse on)
\e]0;axeleratorPC\a                                                              <- title (set-titles) = default pane title (hostname)
\e[?2026h \e[?25l ...rows... \e[?12l\e[?25h\e[H \e[?2026l                         <- (2026 wrapper present even w/o replies)
\e[18t \e[14t   ... repeated redraw ... READY
```
- Also queries `CSI ? 996 n` (color scheme; it pairs with `?2031h`), `CSI 18t`, `CSI 14t` (the 14t/18t come after the first draw).
- Does NOT send `CSI = n u`/`CSI > n u`/`CSI ? u` (no kitty at all). Does NOT send `\e[>4;2m` / `?1004h` / `?7727h` / `?69h` until a DA1/DA2/XTVERSION reply arrives (or 5.08 s). After ANY single reply (da1 only, da2 only, xtv only — all three tested) it emits: `\e[?69h` (only after DA1 reply) `\e[>4;2m \e[?1004h \e[?7727h` followed by a full redraw. Further replies re-trigger the block (harmless; reply once per query).
- Replies that change behavior: DA1/DA2/XTVERSION → release of the block above. XTVERSION string sets `#{client_termtype}` ("OmpDeck 0.1"); `#{client_features}` stays empty. OSC10/OSC11/`?996n` replies: not required (tmux proceeds without them; with them also fine; `xtvda_colors` run ok). DECRPM/`CSI ? u` replies irrelevant.
- Modes tmux sets on outer for the session's lifetime: `?1049h` (alt screen), `?1h` + `\e=` (app cursor/keypad), `?2004h`, `?2031h`, `?1000h ?1002h ?1006h` (mouse on), `?1004h`, `?7727h`, `>4;2m`, `?2026` wrapping, `?12l`. NEVER `?1003h` unless the inner app requests any-motion (omp: tui.ts:100 `?1000h?1003h?1006h`).
- The embedded terminal must therefore implement/handle: 1049, 1/DECCKM+`\e=`, 2004, 1000/1002/1006 (and 1003), 2026 (sync), 1004, `>4;2m` (just ignore; encode keys yourself), 7727 (track or just always send Esc as `ESC O [`), `?2031h`/`?996n`, `CSI 18t`/`14t` queries (replies optional), OSC 10/11 queries (optional).

## 3. Exp. 2 — key forwarding (tmux gets bytes on outer PTY, inner program logs stdin; inner modes tested: none / `\e[>4;2m` / `\e[>1u` / `\e[>5u` / `\e[>31u`; extended-keys-format csi-u; outer replied DA1+DA2+XTVERSION)
Inner program receives (hex):
| outer sends | inner none | inner modifyOtherKeys2 | inner kitty push (1/5/31) |
|---|---|---|---|
| Enter `0d` | `0d` | `0d` | `0d` |
| Shift+Enter `\e[13;2u` | `0d` (modifier LOST) | `1b 5b 31 33 3b 32 75` = `\e[13;2u` | `0d` (kitty ignored) |
| Shift+Enter `\e[27;2;13~` | `0d` | `\e[13;2u` | `0d` |
| Ctrl+Enter `\e[13;5u` / `\e[27;5;13~` | `0d` | `\e[13;5u` (both) | `0d` |
| Alt+Enter `\e\r` | `1b 0d` | `\e[13;3u` | `1b 0d` |
| Ctrl+C `03` | `03` | `\e[99;5u` | `03` |
| Ctrl+V `16` | `16` | `\e[118;5u` | `16` |
| Shift+Tab `\e[Z` or `\e[9;2u` | `\e[Z` | `\e[9;2u` | `\e[Z` |
| Ctrl+Shift+A `\e[97;6u` | `01` | `\e[97;6u` | `01` |
| Ctrl+I `\e[105;5u` | `09` | `\e[105;5u` | `09` |
| Ctrl+Space `\e[32;5u` | `00` | `\e[32;5u` | `00` |
| Alt+a `\e a` | `1b 61` | `\e[97;3u` | `1b 61` |
| Alt+Shift+A `\e[97;4u` | `1b 61` (Shift lost) | `\e[97;4u` | `1b 61` |
| Alt+Esc `\e\e` | `1b 1b` | `\e[27;3u` | `1b 1b` |
| Esc as `ESC O [` | `1b` | `1b` | `1b` |
| Tab / Backspace(`7f`) / Shift+Up `\e[1;2A` | `09`/`7f`/`\e[1;2A` | `09`/`7f`/`\e[1;2A` (unchanged, not CSI-u) | same |
- With `extended-keys-format xterm` the same inner-mok2 inputs yield `\e[27;2;13~`, `\e[27;5;13~`, `\e[27;3;13~`, `\e[27;5;99~`; Enter stays `0d`. Both parse in omp; csi-u recommended.
- Plain Enter, Tab, Backspace, Ctrl+letters-without-extkeys stay legacy bytes in all modes only when inner is NOT in mok2; in mok2 Ctrl+C/V arrive as CSI-u (omp decodes).
- Lone Esc: delivered after 0.502 s with escape-time 0/10/100/300 (1.005 s for 1000) — floor 500 ms. `\e[27u` → inner got literal `\e[27u`; `\e[27;1u` → `1b` (tmux decodes it, 0.05 s); **`ESC O [` → `1b` at 0.05 s (also before any reply)**; `ESC ESC` → `1b 1b`.
- **App key-encoding rule**: when the terminal is attached to tmux: Shift/Ctrl/Alt(+Shift)+Enter/Tab/Space/letters/etc. that legacy encoding can't express → send CSI-u `\e[<unicode>;<mod>u` (mod = 1 + shift(1) + alt(2) + ctrl(4)). Legacy-expressible keys (plain Enter 0d, Tab 09, Backspace 7f, Ctrl+C 03, arrows `\e[A`/`\e[1;2A`) may remain legacy — tmux upgrades them to CSI-u itself when the inner app is in mok2. Esc → `\x1b O [`. Alt+char → `\e` + char works.

## 4. Exp. 3 — inner queries answered by tmux (inner raw tty, replies read 2 s)
| inner sends | inner receives |
|---|---|
| `CSI ? u` | **nothing** (silently swallowed; not forwarded to outer) |
| `CSI ? u` + `CSI c` (omp probe) | `\e[?1;2;4c` (DA1 only) |
| `CSI c` | `\e[?1;2;4c` |
| `CSI > c` | `\e[>84;0;0c` |
| `CSI > q` | `\eP>|tmux 3.7c\e\\` |
| `CSI 6 n` | `\e[1;1R` |
| `CSI ? 2026 $p` | `\e[?2026;2$y` |
| `CSI ? 1004 $p` | `\e[?1004;2$y` |
| `CSI 14 t` | `\e[4;960;1600t` (no outer reply was given; tmux default cell 16x32? [UNVERIFIED origin]) |
| `CSI 18 t` | `\e[8;30;100t` |
| OSC 10 / OSC 11 `?` , OSC 99 `p=?`, `CSI ? 996 n` | nothing |
None of these queries are forwarded to the outer terminal (outer saw only redraws). ⇒ omp falls to modifyOtherKeys as described.

## 5. Exp. 4 — OSC 52 (set-clipboard on + feature clipboard)
inner `\e]52;c;aGVsbG8=\a` → outer receives **`\e]52;c;aGVsbG8=\a`** (BEL-terminated). Inner with ST terminator `\e\\` → still outer BEL form `\e]52;c;aGVsbG8=\a`. Empty selector `\e]52;;aGVsbG8=\a` → outer `\e]52;;aGVsbG8=\a`. Query `\e]52;c;?\a` → nothing emitted. Also emitted by tmux copy-mode selection (mouse drag/double-click release). App must handle OSC 52 set (alacritty_terminal: `Event::ClipboardStore(clipboard_type, text)`, term/mod.rs:1719); `ClipboardLoad` is never triggered through tmux.

## 6. Exp. 5 — titles
inner `\e]0;MyTitle\a` → outer **`\e]0;MyTitle\a`**; inner `\e]2;MyTitle2\a` → outer **`\e]0;MyTitle2\a`**. Always OSC 0 + BEL (from xterm-256color terminfo tsl/fsl), emitted only when title changes. At attach tmux emits `\e]0;<pane-title>\a` where pane-title defaults to hostname (`axeleratorPC`) until the inner app sets one. With `set-titles-string '#T'` the outer title is exactly the pane title.

## 7. Exp. 6 — mouse (`mouse on`)
- Requests at attach: `\e[?1000h \e[?1002h \e[?1006h` (always preceded by `l` resets of 1000/1002/1003/1006, and `?1005l`). 1003 stays off.
- Plain bash pane (200 lines of history), outer sends SGR wheel-up `\e[<64;10;10M`: `#{pane_in_mode}` 0→1, `#{pane_mode}`=copy-mode (binding WheelUpPane → `copy-mode -e`). Further wheel-ups scroll 5 lines each (`scroll_position` 15 after 3 more; wheel-down `\e[<65;…M` −5). Wheel-down to the bottom exits copy-mode automatically (`-e`) (pane_in_mode back to 0).
- **A plain key press does NOT exit copy-mode** (`x` → still `pane_in_mode=1`; mode-keys=emacs). `q` exits; `Esc` exits (but the 500 ms lone-Esc delay applies → use `ESC O [`).
- Inner app that enabled `?1000h?1006h` (`mouse_any_flag=1`): tmux forwards untouched, no copy-mode: wheel-up `\e[<64;10;10M` → `\e[<64;10;10M`; press `\e[<0;5;7M` → same; release `…m` → same; Shift+wheel `\e[<68;10;10M` → same (shift bit passed); drag `\e[<32;…M` and motion `\e[<35;…M` NOT forwarded unless the inner app enabled 1002/1003. Coordinates pane-relative = outer coords (status off, single pane).
- Default root-table mouse bindings left after conf (18): MouseDown1Pane, MouseDown2Pane(paste-buffer -p), MouseDrag1Pane(copy-mode -M selection), DoubleClick1Pane/TripleClick1Pane (select word/line + copy-pipe-and-cancel → OSC 52), WheelUp/DownPane etc. MouseDown3* are gone.
- RISK [by design of tmux]: if the inner app does NOT enable mouse and is on the main screen, wheel/drag enter tmux copy-mode and key input goes to copy-mode until `q`/Esc/scroll-to-bottom. omp enables mouse tracking itself (tui.ts:100) and uses alt screen (tui.ts:1860), where `WheelUpPane` is forwarded (`#{alternate_on}` or `mouse_any_flag` → `send-keys -M`) [forwarding in alt-screen w/o mouse flag UNVERIFIED].

## 8. Exp. 7/8 — focus & bracketed paste
- tmux requests `?1004h` (after first DA/XTVERSION reply, see §2). Client starts as `focused`. Outer `\e[I` while already focused → nothing delivered; `\e[O` → inner (with `?1004h`) gets `1b 5b 4f`; next `\e[I` → `1b 5b 49`; duplicates are deduplicated (client_flags `attached,focused,UTF-8` ↔ `attached,UTF-8`). Inner without 1004 receives nothing. So the app must send `\e[I`/`\e[O` on window focus change (only if tmux enabled 1004h in outer output; always true here).
- tmux requests `?2004h` unconditionally at attach. Pasting `\e[200~hi\nthere\e[201~`: inner WITHOUT 2004 gets plain `68 69 0a 74 68 65 72 65` (brackets stripped); inner WITH `?2004h` gets `\e[200~hi\nthere\e[201~` intact (also with embedded `\e[A`: `\e[200~a\e[Ab\e[201~`). ⇒ the app should always wrap pastes in `\e[200~…\e[201~` (tmux decides), regardless of the emulator's own 2004 state (tmux itself keeps outer 2004 on). Inner changing 2004/1004/1049/25 does not generate outer mode changes.

## 9. Exp. 9 — OSC 8 (feature hyperlinks)
inner `\e]8;;https://example.com\a link \e]8;;\a` → outer `\e]8;id=tmux1;https://example.com\e\\link\e]8;;\e\\` (ST-terminated, id synthesised `tmux<N>`); inner `id=a1` form also rewritten to `id=tmux1`. Without `hyperlinks` feature: dropped.
Other passthroughs: `\ePtmux;\e\e]777;notify;t;b\a\e\\` (allow-passthrough on) → outer `\e]777;notify;t;b\a`. OSC 9 / OSC 99 raw (unwrapped) → NOT forwarded. BEL → forwarded `\a`. Underline: inner `\e[4:3m\e[58:2::255:0:0m` → outer `\e[58:2:255:0:0m\e[4:3m` (note: colorspace-empty field removed; colons). Terminal must parse colon SGR (`58:2:r:g:b`, `4:3`).

## 10. Exp. 12 — color
`\e[38;2;255;0;0mX` → outer `\e[38;2;255;0;0mX\e(B\e[m` (raw truecolor; `48;2;1;2;3` likewise; `38;5;196` stays 256). Cell-attr resets are emitted as `\e(B\e[m` and `\e[39m`. Embedded terminal must accept `ESC ( B`.

## 11. Exp. 10 — sessions lifecycle (socket `-L S`)
- Server NOT running (never started): `list-sessions -F '#{session_name}'`, `has-session -t t1`, `kill-session -t t1`, `list-clients` → **rc=1, stdout empty, stderr `error connecting to /tmp/tmux-1000/<sock> (No such file or directory)`**. After the last session is killed (server exited, socket file left behind): rc=1, stderr `no server running on /tmp/tmux-1000/<sock>`. ⇒ treat rc≠0 with either stderr as "empty list".
- Running: `has-session -t t1` rc 0 (silent); `has-session -t nope` rc 1, stderr `can't find session: nope`; `-t =t1` exact match rc 0 (use `=name` to avoid prefix matching). `list-sessions -F '#{session_name}|#{session_attached}|#{session_path}'` → `t1|1|/tmp`.
- `new-session -A -s t1 -- cmd`: first run executes cmd (count file 1 line); after closing the client PTY master (client gets SIGHUP) → `list-sessions` `t1|attached=0`, pane pid unchanged, process alive; second `new-session -A -s t1 -- cmd SHOULD-NOT-RUN` → attaches, cmd NOT rerun (count still 1, same `pane_pid`), `session_attached=1`, full redraw begins with `\e[?1049h\e[22;0;0t…` (~1.3 KB). Existing session's `-c` is ignored on attach.
- `kill-session -t t1` rc 0; with `exit-empty on` (default) the server then exits (subsequent commands: "no server running").
- Window resize: TIOCSWINSZ 120x40 on the client PTY + SIGWINCH → `window_width x window_height` 120x40 within 0.6 s (alacritty `tty` resize path does exactly that).

## 12. Exp. 11 — `-f` semantics
Server started with `A.conf` (`history-limit 111`, `mouse off`); `new-session -A -s t2 -f B.conf` (history-limit 222, mouse on) on the running server → options UNCHANGED (111/off), second session created. `tmux source-file B.conf` → rc 0 applies (222/on). Hence: **`-f` only at server start**. Recommended startup sequence:
```
# 1) write conf (final conf above) to e.g. $XDG_RUNTIME_DIR/omp-deck/tmux.conf
#    if `tmux -L omp-deck list-sessions` exits 0 (server already running) → run `tmux -L omp-deck source-file <conf>`
#    (idempotent, rc 0); otherwise skip (the first new-session's -f will load it)
# 2) per tab (PTY child of alacritty tty::new):
env TERM=xterm-256color COLORTERM=truecolor TERM_PROGRAM=omp-deck \
  tmux -L omp-deck -f <conf> new-session -A -s <name> -c <cwd> -- omp --resume <path>
```
Options set via `source-file` on a running server affect already-attached clients only partly: `terminal-features`, `focus-events`, `extended-keys` are evaluated per client at attach (man tmux: "Attached clients should be detached and attached again after changing" for focus-events) → apply before attaching, which the above order guarantees.
Always `-u TMUX`/unset `TMUX` in the environment of the spawned client.

## 13. Embedded-terminal checklist derived from tmux's traffic
Reply to: `CSI c` (`\e[?6c` ok; omp's regex `^\x1b\[\?[\d;]*c$` ok), `CSI > c`, `CSI > q` (recommended: `\eP>|OmpDeck <ver>\e\\`; omp only uses the first word via `[A-Za-z][A-Za-z0-9._+-]*` and only matches known ids: kitty/ghostty/wezterm/iterm2/vscode/alacritty/warp/orca/otty/rio/tern — anything else ⇒ `trueColor` from COLORTERM, terminal-capabilities.ts:757-775), `CSI 14t`/`18t` (optional), `CSI ? 996 n` (optional, `\e[?997;1n` dark / `;2n` light), OSC 10/11 (optional). Process: 1049, 2004, 1000/1002/1003/1006, 1004, 2026, `?7727` (or always emit Esc as `ESC O [`), OSC 0/2 (title), OSC 8, OSC 52 store, SGR colon forms, `ESC ( B`. Ignore `\e[>4;2m` and `\e[22;0;0t`/`\e[23t` title-stack ops (alacritty_terminal handles/ignores).

## 14. Copy-mode exit on keypress (follow-up; verified on tmux 3.7c)
**Answer:** tmux has a wildcard key `Any` for key tables (`bind -T copy-mode Any …` is accepted; it is the fallback when no specific binding in that table matches). Replaying the triggering key: `send-keys` with NO key args, run from a key binding, "uses that key" (man tmux send-keys) — and since the pane has left copy-mode by then it is delivered through the normal input path (so extended-keys/modifyOtherKeys encoding for the inner app is identical to the non-copy-mode case). No `#{key}` format exists; none needed.
Needed pieces:
1. Remove ALL default copy-mode key bindings (otherwise `q`, `Enter`, `C-c`, `Space`, arrows, `M-x`, `Escape` … are handled by copy-mode and never reach `Any`). Pin `mode-keys emacs` so only the `copy-mode` table is used (default `mode-keys` flips to `vi` when $EDITOR/$VISUAL contains "vi"; `copy-mode-vi` is also emptied for safety).
2. Re-add only the mouse bindings (copied from the default `list-keys -T copy-mode`).
3. `Any` must ignore mouse events: unbound mouse events (button release `MouseUp1Pane`, middle/right, motion, shift/ctrl+wheel) also fall through to `Any`; without the guard they cancelled copy-mode (observed: `in_mode` 1→0 after a plain button release). Guard = `if -F '#{!=:#{mouse_x},}' '' {…}` (`mouse_x` is empty for keyboard events, non-empty for mouse events).
Final lines (appended to `/tmp/omp-deck-notes/tmux.conf`):
```
set -g mode-keys emacs
unbind -aq -T copy-mode
unbind -aq -T copy-mode-vi
bind -T copy-mode MouseDown1Pane select-pane
bind -T copy-mode MouseDrag1Pane select-pane \; send-keys -X begin-selection
bind -T copy-mode MouseDragEnd1Pane send-keys -X copy-pipe-and-cancel
bind -T copy-mode WheelDownPane select-pane \; send-keys -X -N 5 scroll-down
bind -T copy-mode WheelUpPane select-pane \; send-keys -X -N 5 scroll-up
bind -T copy-mode DoubleClick1Pane select-pane \; send-keys -X select-word \; run-shell -d 0.3 \; send-keys -X copy-pipe-and-cancel
bind -T copy-mode TripleClick1Pane select-pane \; send-keys -X select-line \; run-shell -d 0.3 \; send-keys -X copy-pipe-and-cancel
bind -T copy-mode Any if -F '#{!=:#{mouse_x},}' '' { send-keys -X cancel ; send-keys }
```
Idempotent: `new-session -d -f conf` rc 0; 2× `source-file conf` rc 0, stderr empty; `list-keys -T copy-mode` = exactly these 8 bindings, `copy-mode-vi` empty.
**Observed (harness `/tmp/ompx/cm.py <conf>`; outer enters copy-mode with SGR wheel-up `\e[<64;10;10M`, `pane_in_mode` 1 → send key → 0 in every row; inner program hex-logs stdin):**
| outer sends | inner in legacy mode | inner in modifyOtherKeys 2 (`\e[>4;2m`) |
|---|---|---|
| `a` | `61` | `61` |
| Enter `\r` | `0d` | `0d` |
| Ctrl+C `03` | `03` | `1b 5b 39 39 3b 35 75` (`\e[99;5u`) |
| Shift+Enter `\e[13;2u` | `0d` | `\e[13;2u` |
| Ctrl+Enter `\e[13;5u` | `0d` | `\e[13;5u` |
| `ESC O [` | `1b` | `1b` |
| Alt+a `\e a` | `1b 61` | `\e[97;3u` |
| Alt+x `\e x` | `1b 78` | `\e[120;3u` |
| Ctrl+Shift+A `\e[97;6u` | `01` | `\e[97;6u` |
| Up `\e[A` / PgUp `\e[5~` / Home `\e[H` | `1b 5b 41` / `1b 5b 35 7e` / `1b 5b 31 7e` | same |
| Space, `q`, Tab, Backspace, `é` | `20`, `71`, `09`, `7f`, `c3 a9` | same |
| Ctrl+V `16` | `16` | `\e[118;5u` |
All byte-identical to the non-copy-mode results in §3 — the first key is NOT lost (it cancels copy-mode and is delivered once). Bracketed paste `\e[200~hi\e[201~` in copy-mode: inner with `?2004h` receives `\e[200~hi\e[201~` intact (copy-mode exited); without 2004 receives `68 69`.
**Mouse (pane = bash printing `line 1..100`, then inner logger with 2004 on):**
- wheel-up #1 enters copy-mode (`scroll_position` 0), each further wheel-up +5 (5, 20 after 3 more), wheel-down −5 (15). Scrolling to the bottom exits (entered via root `copy-mode -e`).
- Unbound mouse events do nothing and keep copy-mode (`in_mode` stays 1, scroll unchanged), nothing reaches the inner app: button-1 release `\e[<0;10;10m`, middle press/release `\e[<1;…M/m`, right press/release `\e[<2;…M/m`, motion `\e[<35;11;10M`, Shift+wheel `\e[<68;…M`, Ctrl+wheel `\e[<80;…M` (Shift/Ctrl+wheel do NOT scroll).
- Drag selection in copy-mode (`\e[<0;3;5M`, `\e[<32;8;5M`, `\e[<32;12;5M`, release `\e[<0;12;5m`): copy-mode exits on release, buffer set (`show-buffer` = `ne 57`), outer receives **`\e]52;;bmUgNTc=\a`** (note EMPTY selector `52;;`, unlike `52;c;` for app-originated OSC 52; treat empty as clipboard/`c`). Drag starting from the live pane (root `MouseDrag1Pane` → `copy-mode -M`) → same, `\e]52;;bmUgNzk=\a`.
- Double-click selects word and copies: `\e]52;;Nzc=\a` ("77"), exits copy-mode after 0.3 s.
- Caveat: while an app has mouse mode on (mouse_any_flag, e.g. omp), root bindings forward mouse events to it and copy-mode is not entered at all — this section only applies to non-mouse panes / alt-screen-less shells.
