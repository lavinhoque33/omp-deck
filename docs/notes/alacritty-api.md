# alacritty_terminal 0.26.0 embedding API (verified against registry source + compiled spike)

Root: `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/alacritty_terminal-0.26.0/src` (abbrev `AT/`), vte at `.../vte-0.15.0/src/ansi.rs` (abbrev `VTE:`).
Re-exports: `alacritty_terminal::vte` (lib.rs:19-20) => use `alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb, CursorShape, CursorStyle, Hyperlink, KeyboardModes, ModifyOtherKeys}`.
Modules: event, event_loop, grid, index, selection, sync, term, thread, tty, vi_mode (lib.rs:9-18). `term::search` also exists (regex search; usable for URL detection).
Cargo: `alacritty_terminal = "0.26"`; no features needed. Spike built with `=0.26.0`, edition 2024, OK.

## 1. tty (AT/tty/mod.rs, AT/tty/unix.rs)
```rust
// tty/mod.rs:21-39
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Options {
    pub shell: Option<Shell>,                 // None => $SHELL/passwd shell
    pub working_directory: Option<PathBuf>,
    pub drain_on_exit: bool,
    pub env: HashMap<String, String>,         // extra env; applied AFTER defaults => overrides them
    // #[cfg(windows)] pub escape_args: bool  (not on Linux)
}
// tty/mod.rs:42-58
pub struct Shell { /* pub(crate) program: String, args: Vec<String> */ }
impl Shell { pub fn new(program: String, args: Vec<String>) -> Self }   // args do NOT include argv[0]
// tty/unix.rs:195
pub fn new(config: &Options, window_size: WindowSize, window_id: u64) -> std::io::Result<Pty>
// tty/unix.rs:202 pub fn from_fd(config, window_id, master: OwnedFd, slave: OwnedFd) -> Result<Pty>
impl Pty { pub fn child(&self) -> &std::process::Child; pub fn file(&self) -> &std::fs::File }  // unix.rs:109-117
```
Env behavior (unix.rs:228-241): child inherits OUR whole process env, then:
- sets `ALACRITTY_WINDOW_ID=<window_id>`, `WINDOWID=<window_id>` (unix.rs:230,234 — OVERRIDES whatever we had), `USER`, `HOME` (from env or passwd);
- then applies `config.env`;
- removes `XDG_ACTIVATION_TOKEN`, `DESKTOP_STARTUP_ID`.
- `tty::new` does NOT set TERM/COLORTERM. Only `tty::setup_env()` (tty/mod.rs:104-112) does, and it mutates OUR process env (`set_var TERM=alacritty|xterm-256color, COLORTERM=truecolor`) — don't call it; pass `("TERM","xterm-256color")`, `("COLORTERM","truecolor")` in `Options.env`.
- OBSERVED: spawned `sh -c 'echo $TERM $COLORTERM $WINDOWID $ALACRITTY_WINDOW_ID'` => `TERM=dumb`(inherited from my shell) `CT=truecolor`(inherited) `WID=0 AWID=0` (our window_id=0).
- Cannot unset WINDOWID/ALACRITTY_WINDOW_ID via Options (env map only sets). Workaround, VERIFIED: use `Shell::new("env", ["-u","WINDOWID","-u","ALACRITTY_WINDOW_ID","tmux","-L","omp-deck",...])` — tmux window saw `WINDOWID` empty. (Or `Options.env` can set them to "" but then they exist as empty.) Alternatively `std::env::remove_var` is irrelevant because tty sets them on the Command.
- Child setup (unix.rs:248-276): `setsid`, chdir, TIOCSCTTY, resets SIGCHLD/HUP/INT/QUIT/TERM/ALRM to default. IUTF8 set on pty (unix.rs:206-211).
- NOTE: `tty::new` installs a process-global SIGCHLD pipe handler per Pty (signal_hook, unix.rs:279-286); `die!` calls `process::exit(1)` if TIOCSWINSZ fails (unix.rs:416).
- Drop for Pty (unix.rs:309-321): `kill(child_pid, SIGHUP)`, unregister signal, then blocking `child.wait()`. => dropping a Pty SIGHUPs the **tmux client process only**. VERIFIED (see §9-tmux): tmux server+session survived, `list-clients` empty afterwards, join+drop took ~2ms.

## 2. event::WindowSize, term::Config, Dimensions
```rust
// event.rs:89-95
#[derive(Copy, Clone, Debug)]
pub struct WindowSize { pub num_lines: u16, pub num_cols: u16, pub cell_width: u16, pub cell_height: u16 }
// pty winsize xpixel = cols*cell_width, ypixel = rows*cell_height (unix.rs:428-436)

// term/mod.rs:333-352  (Clone, Debug, PartialEq, Eq)
pub struct Config {
    pub scrolling_history: usize,                 // default 10000
    pub default_cursor_style: CursorStyle,        // vte::ansi::CursorStyle{shape,blinking}; default Block, blinking:false
    pub vi_mode_cursor_style: Option<CursorStyle>,// default None
    pub semantic_escape_chars: String,            // default term::SEMANTIC_ESCAPE_CHARS = ",│`|:\"' ()[]{}<>\t"
    pub kitty_keyboard: bool,                     // DEFAULT FALSE
    pub osc52: Osc52,                             // DEFAULT OnlyCopy
}
// term/mod.rs:371-384: pub enum Osc52 { Disabled, OnlyCopy (default), OnlyPaste, CopyPaste }
impl Config: Default (mod.rs:356). Use `Config { kitty_keyboard: true, scrolling_history: N, ..Config::default() }`.
Term::set_options(&mut self, Config) (mod.rs:499) re-applies; toggling kitty_keyboard clears kitty stacks.

// term/mod.rs:410
impl<T> Term<T> { pub fn new<D: Dimensions>(config: Config, dimensions: &D, event_proxy: T) -> Term<T> }
// grid/mod.rs:486 trait Dimensions { fn total_lines(&self)->usize; fn screen_lines(&self)->usize; fn columns(&self)->usize; /*provided: last_column, topmost_line, bottommost_line, history_size*/ }
// Ready-made: alacritty_terminal::term::test::TermSize { pub columns: usize, pub screen_lines: usize } ; TermSize::new(columns, screen_lines)  (term/mod.rs:2425-2450; its total_lines()==screen_lines)
// Term<T> itself implements Dimensions (mod.rs:1042).
```
Consts: `term::MIN_COLUMNS=2`, `MIN_SCREEN_LINES=1` (mod.rs:36-39).

## 3. EventLoop (AT/event_loop.rs)
```rust
pub enum Msg { Input(Cow<'static,[u8]>), Shutdown, Resize(WindowSize) }                // :31-40
impl<T: tty::EventedPty + event::OnResize + Send + 'static, U: EventListener + Send + 'static> EventLoop<T,U> {
    pub fn new(terminal: Arc<FairMutex<Term<U>>>, event_proxy: U, pty: T, drain_on_exit: bool, ref_test: bool) -> io::Result<EventLoop<T,U>>  // :63
    pub fn channel(&self) -> EventLoopSender                                          // :84
    pub fn spawn(self) -> JoinHandle<(Self, State)>                                   // :205, thread named "PTY reader"
}
#[derive(Clone)] pub struct EventLoopSender;  impl { pub fn send(&self, msg: Msg) -> Result<(), EventLoopSendError> }  // :383-394 (also pokes the poller)
pub struct Notifier(pub EventLoopSender);  impl event::Notify  { fn notify<B: Into<Cow<'static,[u8]>>>(&self, b: B) } // :333-348 ; empty bytes ignored
impl event::OnResize for Notifier { fn on_resize(&mut self, WindowSize) } // sends Msg::Resize   (:350)
```
- `Notify` trait: event.rs:82. `OnResize`: event.rs:98.
- The EventLoop owns an internal `tx` clone, so the channel never disconnects while the loop lives; you MUST send `Msg::Shutdown` to stop it (dropping EventLoopSender/JoinHandle alone leaks the thread + child).
- Shutdown (event_loop.rs:252-254): loop `break`s; thread returns `(EventLoop, State)`; the `Pty` lives in that tuple. Pty::drop (SIGHUP + wait) runs when the tuple is dropped: on `join()` result drop, or automatically at thread end if the JoinHandle was dropped (detached). Don't `join()` on the UI thread unnecessarily (wait() blocks until child dies; ~ms for tmux client).
- Resize: Term is NOT resized by the loop. Do both, in this order (what alacritty does):
  `let mut t = term.lock(); t.resize(TermSize::new(cols, rows)); sender.send(Msg::Resize(WindowSize{...}))` => pty TIOCSWINSZ => child gets SIGWINCH. `Term::resize` is a no-op if cols/lines unchanged (mod.rs:662), but you still need Msg::Resize when only cell pixel size changed. Resizing clears selection if columns changed (mod.rs:683).
- Child exit (event_loop.rs:258-271): on SIGCHLD, if `status` is Some => `Event::ChildExit(std::process::ExitStatus)` is sent FIRST, then (if `drain_on_exit`) the remaining pty output is read, then `term.exit()` => `Event::Exit`, then `Event::Wakeup`, loop ends. OBSERVED event order for `printf title+red; exit 3`: `ChildExit(unix_wait_status(768)) , Title(hi), Exit, Wakeup`. Use `status.code() == Some(3)` (768 is the raw wait status, 3<<8). ChildExit therefore can arrive BEFORE the final output events; treat `Exit` as "grid final". Set `drain_on_exit: true` so final output is not lost.
- Locking: `FairMutex` (AT/sync.rs): `lock()` (fair; UI should use this), `lock_unfair()`, `try_lock_unfair()`, `lease()`. Loop uses lease+try_lock_unfair. UI: `let t = term.lock();` keep hold-time short (copy cells to a frame struct).
- Parser sync updates (CSI ? 2026 h/l) are handled inside the loop; Wakeup is not emitted for fully-synchronized bytes (event_loop.rs:166).

## 4. Events (AT/event.rs:14-59) — exact
```rust
#[derive(Clone)]
pub enum Event {
    MouseCursorDirty,
    Title(String),
    ResetTitle,
    ClipboardStore(term::ClipboardType, String),                                   // OSC 52 set; only if Config.osc52 in {OnlyCopy,CopyPaste}
    ClipboardLoad(term::ClipboardType, Arc<dyn Fn(&str) -> String + Sync + Send>), // OSC 52 query; only if osc52 in {OnlyPaste,CopyPaste}; reply = f(clipboard_text) -> write to pty
    ColorRequest(usize, Arc<dyn Fn(Rgb) -> String + Sync + Send>),                 // reply = f(rgb) -> write to pty
    PtyWrite(String),
    TextAreaSizeRequest(Arc<dyn Fn(WindowSize) -> String + Sync + Send>),          // CSI 14 t ; reply = f(current WindowSize)
    CursorBlinkingChange,
    Wakeup,
    Bell,
    Exit,
    ChildExit(std::process::ExitStatus),
}
pub trait EventListener { fn send_event(&self, _event: Event) {} }   // event.rs:103 ; VoidListener exists
```
`ClipboardType { Clipboard, Selection }` (term/mod.rs:2313). Listener is called FROM the PTY thread (and from `Term::scroll_display` etc. on caller's thread while term is locked) => send_event must not lock the Term (deadlock) and should be cheap: forward to an mpsc + `egui::Context::request_repaint()`.
Embedder MUST answer:
- `PtyWrite(s)` => write `s.into_bytes()` to pty (`EventLoopSender.send(Msg::Input(Cow::Owned(..)))`). Term generates these itself for: DA1 `ESC[?6c` (mod.rs:1260), DA2 `ESC[>0;<ver>;1c` (mod.rs:1265), DSR 5 `ESC[0n`, DSR 6 cursor pos `ESC[r;cR` (mod.rs:1332-1345), `CSI 18 t` `ESC[8;rows;colst` (mod.rs:2268), DECRQM mode reports, kitty `CSI ? u` => `ESC[?<flags>u` (mod.rs:1275; ONLY when Config.kitty_keyboard=true, else silently ignored). OBSERVED replies read by a child: `\x1b[?6c \x1b[1;1R \x1b[?0u \x1b[8;5;80t ... \x1b[?1u`.
- `ColorRequest(index, fmt)` for OSC 4;n;? (index = n, 0..=255), OSC 10;? (index 256 = NamedColor::Foreground), OSC 11;? (257 = Background), OSC 12;? (258 = Cursor) (VTE:1373-1385, 1425-1440). Compute the color: `term.colors()[index]` is `Option<Rgb>` (Some only if the app set it via OSC 4/10/11/12) else your theme's palette entry; reply `sender.send(Msg::Input(fmt(rgb).into_bytes().into()))`. fmt output is `ESC ] <prefix> ; rgb:rrrr/gggg/bbbb <terminator>` mirroring the query terminator (BEL vs ST preserved; OBSERVED `\x1b]10;rgb:cccc/cccc/cccc\x1b\\`, `\x1b]11;rgb:1010/1010/1010\x07`, `\x1b]4;1;rgb:0101/0202/0303\x07`). tmux queries OSC 10/11 on attach (observed), so answer them or tmux assumes defaults.
- `TextAreaSizeRequest(f)`: reply `f(WindowSize{...current...})` => `ESC[4;<h_px>;<w_px>t`. tmux sends `CSI 14 t` and `CSI 18 t` at start (observed). Needs shared current WindowSize in the listener (Arc<Mutex<WindowSize>>/atomics).
- `ClipboardStore` => put in system clipboard (arboard). `ClipboardLoad` => read clipboard, reply `f(&text)`. (Only if osc52 policy allows.)
- `Title/ResetTitle` => tab title; there is NO public `term.title` accessor (field private, mod.rs ~line 300), so store the last Title from events. `Bell` => visual bell/notify. `Wakeup` => request_repaint. `CursorBlinkingChange` / `MouseCursorDirty` => repaint. `Exit` => session ended. `ChildExit(st)` => `st.code()`.
- NOT emitted by Term (so embedder-generated): focus reports (`ESC[I` / `ESC[O` when mode FOCUS_IN_OUT), mouse reports, key encoding, bracketed paste wrapping.
- NOT handled by alacritty_terminal at all (silently ignored): `CSI > q` XTVERSION (tmux sends it, observed), `CSI ? 996 n`, `CSI ? 2031 h`, `CSI ? 7727 h` (tmux), `CSI > 4 ; n m` modifyOtherKeys (see §5).

## 5. TermMode (term/mod.rs:53-86) — `bitflags u32`, `Term::mode() -> &TermMode`, also `RenderableContent.mode`
SHOW_CURSOR=1, APP_CURSOR=1<<1 (DECCKM ?1), APP_KEYPAD=1<<2, MOUSE_REPORT_CLICK=1<<3 (?1000), BRACKETED_PASTE=1<<4 (?2004), SGR_MOUSE=1<<5 (?1006), MOUSE_MOTION=1<<6 (?1003), LINE_WRAP=1<<7, LINE_FEED_NEW_LINE=1<<8, ORIGIN=1<<9, INSERT=1<<10, FOCUS_IN_OUT=1<<11 (?1004), ALT_SCREEN=1<<12 (?1049/47/1047), MOUSE_DRAG=1<<13 (?1002), UTF8_MOUSE=1<<14 (?1005), ALTERNATE_SCROLL=1<<15 (?1007, default ON), VI=1<<16, URGENCY_HINTS=1<<17,
DISAMBIGUATE_ESC_CODES=1<<18, REPORT_EVENT_TYPES=1<<19, REPORT_ALTERNATE_KEYS=1<<20, REPORT_ALL_KEYS_AS_ESC=1<<21, REPORT_ASSOCIATED_TEXT=1<<22,
composites: `MOUSE_MODE = MOUSE_REPORT_CLICK|MOUSE_MOTION|MOUSE_DRAG` (mutually exclusive: setting one clears the others, mod.rs:1948-1964), `KITTY_KEYBOARD_PROTOCOL` = OR of the 5 kitty flags. `TermMode::default()` = SHOW_CURSOR|LINE_WRAP|ALTERNATE_SCROLL|URGENCY_HINTS (mod.rs:113). SGR_MOUSE and UTF8_MOUSE are mutually exclusive (mod.rs:1966-1974).
Kitty: `CSI = flags;mode u`, `CSI > flags u` (push), `CSI < n u` (pop), `CSI ? u` (query) are processed only if `Config.kitty_keyboard == true` (mod.rs:1275-1330). Stack is per-screen (inactive_keyboard_mode_stack swap on alt screen). Flags map: bit0 DISAMBIGUATE, bit1 REPORT_EVENT_TYPES, bit2 REPORT_ALTERNATE_KEYS, bit3 REPORT_ALL_KEYS_AS_ESC, bit4 REPORT_ASSOCIATED_TEXT (vte `KeyboardModes`).
OBSERVED: child sent `ESC[>1u` => `mode` gained DISAMBIGUATE_ESC_CODES; `ESC[?1000h ESC[?1006h ESC[?2004h` => MOUSE_REPORT_CLICK|SGR_MOUSE|BRACKETED_PASTE.
**modifyOtherKeys**: vte parses `CSI > 4 ; n m` into `Handler::set_modify_other_keys(ModifyOtherKeys::{Reset,EnableExceptWellDefined,EnableAll})` and `CSI ? 4 m` into `report_modify_other_keys` (VTE:1685-1700, trait defaults no-op at VTE:722-728). `Term`'s `impl Handler` does NOT override them (no occurrence of "modify_other" in AT/term/mod.rs) => alacritty_terminal does NOT track modifyOtherKeys, no TermMode flag, no reply. Cannot hook it: the vte `Processor` lives inside `event_loop::State` and `Term` is the only handler.
Practical consequence (VERIFIED experiment, tmux 3.7c, TERM=xterm-256color, conf `set -g extended-keys on` + `set -ga terminal-features ',xterm-256color:extkeys'`): 
 * tmux itself emits `ESC[>4;2m` (modifyOtherKeys 2) and `ESC[?7727h` to the outer terminal on startup (observed; without the terminal-features line tmux emitted neither and did not emit `>1u`). tmux never emitted `CSI > Nu` kitty push / `CSI ? u` query to the outer terminal in 3.7c.
 * Outer-terminal input is accepted by tmux in BOTH forms regardless of what the inner app asked for. Driver sent to the tmux client, inner app got:
   inner asked `ESC[>4;2m`: sent `ESC[13;2u` => inner `ESC[27;2;13~`; sent `ESC[27;2;13~` => `ESC[27;2;13~`; sent `ESC[97;5u` => `ESC[27;5;97~`; sent `ESC[27;5;97~` => `ESC[27;5;97~`.
   inner asked nothing: `ESC[13;2u`=>`\r`, `ESC[27;2;13~`=>`\r`, `ESC[97;5u`=>`\x01`, `ESC[27;5;97~`=>`\x01` (tmux downgrades to legacy for the app).
 => Embedder can ALWAYS send CSI-u (`ESC[<codepoint>;<mod+1>u`) or `ESC[27;<mod+1>;<cp>~` for ambiguous keys (Shift+Enter, Ctrl+Enter, Ctrl+Shift+x, ...) to the tmux client; tmux translates per what the inner app (omp) negotiated. Tradeoff: if you do this when NOT running under tmux (plain shell), apps that did not enable the mode would see garbage; irrelevant for our tmux-only setup. Needs `extended-keys on` (+ the extkeys terminal-feature) in the omp-deck tmux conf (we pass `-f`). Whether tmux honours the CSI-u input without `extended-keys on` is UNVERIFIED (extended-keys on/always both tested identical).
 * omp (packages/tui/src/terminal.ts:1443-1503, 909-910) probes `ESC[?u`, falls back to `ESC[>4;2m`; that all happens against tmux, not against our Term.
 * To be safe set `kitty_keyboard: true` or not? With tmux in front our Term never receives kitty requests (tmux doesn't forward them in the observed run), so the flag is moot; the `TermMode::*KITTY*` bits stay 0. Setting true is harmless and makes a plain-shell tab work.

## 6. Rendering
```rust
// term/mod.rs:637 (requires T: EventListener)
pub fn renderable_content(&self) -> RenderableContent<'_>
// term/mod.rs:2393
pub struct RenderableContent<'a> {
    pub display_iter: GridIterator<'a, Cell>,   // Iterator<Item = Indexed<&'a Cell>>, row-major, exactly screen_lines*columns items, includes scrollback offset
    pub selection: Option<SelectionRange>,      // {start: Point, end: Point, is_block} grid coords (selection.rs:33)
    pub cursor: RenderableCursor,               // { pub shape: CursorShape, pub point: Point }  (mod.rs:2365) ; shape==Hidden when !SHOW_CURSOR; point already fixed up for WIDE_CHAR_SPACER; point is grid coords (line 0 = first screen row, ignores display_offset)
    pub display_offset: usize,
    pub colors: &'a color::Colors,
    pub mode: TermMode,
}
// grid/mod.rs:554  pub struct Indexed<T> { pub point: Point, pub cell: T }  (Deref<Target=T>)
```
Viewport row of a cell: `row = (indexed.point.line.0 + display_offset as i32) as usize` (or `term::point_to_viewport(display_offset, point) -> Option<Point<usize>>`, mod.rs:124); `col = indexed.point.column.0`. Inverse: `term::viewport_to_point(display_offset, Point<usize>) -> Point` (mod.rs:131). OBSERVED dump with display_offset=3 produced correct rows.
Cursor to draw on screen: `point_to_viewport(display_offset, cursor.point)` (None if scrolled off). Draw HollowBlock yourself when unfocused (Term doesn't: `Term.is_focused` pub field exists but RenderableCursor ignores it; set it for completeness).
```rust
// term/cell.rs
pub struct Cell { pub c: char, pub fg: Color, pub bg: Color, pub flags: Flags, pub extra: Option<Arc<CellExtra>> }
impl Cell {
  pub fn zerowidth(&self) -> Option<&[char]>        // combining chars to append after c
  pub fn underline_color(&self) -> Option<Color>    // SGR 58
  pub fn hyperlink(&self) -> Option<term::cell::Hyperlink>   // OSC 8 (clones Arc)
}
impl term::cell::Hyperlink { pub fn id(&self)->&str; pub fn uri(&self)->&str }   // cell.rs:34-60 ; id auto "<n>_alacritty" when absent
// Flags: u16 bitflags (cell.rs:12-35)
INVERSE=1 BOLD=2 ITALIC=4 BOLD_ITALIC=6 UNDERLINE=8 WRAPLINE=0x10 WIDE_CHAR=0x20 WIDE_CHAR_SPACER=0x40 DIM=0x80 DIM_BOLD=0x82 HIDDEN=0x100 STRIKEOUT=0x200 LEADING_WIDE_CHAR_SPACER=0x400 DOUBLE_UNDERLINE=0x800 UNDERCURL=0x1000 DOTTED_UNDERLINE=0x2000 DASHED_UNDERLINE=0x4000 ALL_UNDERLINES=(UNDERLINE|DOUBLE|UNDERCURL|DOTTED|DASHED)
```
Rules: skip drawing glyph for `WIDE_CHAR_SPACER` and `LEADING_WIDE_CHAR_SPACER`; `WIDE_CHAR` cell spans 2 columns; test bold with `flags.intersects(Flags::BOLD)` (DIM_BOLD/BOLD_ITALIC are composites — use `contains(Flags::BOLD)` and `contains(Flags::DIM)`, careful: `contains(DIM_BOLD)` needs both).
```rust
// vte::ansi (VTE:1128, 1013, 60, 1126)
pub enum Color { Named(NamedColor), Spec(Rgb), Indexed(u8) }
pub struct Rgb { pub r: u8, pub g: u8, pub b: u8 }
#[repr(usize-ish)] pub enum NamedColor { Black=0, Red, Green, Yellow, Blue, Magenta, Cyan, White,
  BrightBlack, BrightRed, BrightGreen, BrightYellow, BrightBlue, BrightMagenta, BrightCyan, BrightWhite,   // 8..=15
  Foreground=256, Background, Cursor, DimBlack, DimRed, DimGreen, DimYellow, DimBlue, DimMagenta, DimCyan, DimWhite, BrightForeground, DimForeground }  // 256..=268
NamedColor::to_bright(self) / to_dim(self) exist (VTE:1074+)
// term/color.rs: pub const COUNT: usize = 269; Colors: Index<usize>/Index<NamedColor> -> Option<Rgb>  (Some only if overridden by OSC 4/10/11/12 from the app)
```
Resolve color: `Named(n)` => `colors[n]` override else theme[n] (map Dim* to dimmed theme/ fg*2/3); `Indexed(i)` => `colors[i as usize]` override else 0..15 theme, 16..231 6x6x6 cube, 232..255 grayscale (`i<16`/cube formula yours); `Spec(rgb)`. Default cells are `Named(Foreground)`/`Named(Background)` (cell.rs Default). INVERSE => swap fg/bg. Bold-as-bright: alacritty frontend maps `Named(Black..White)` + BOLD => Bright* — your choice (Konsole default: bold = bright off? keep faithful to theme). `HIDDEN` => draw nothing.
Cursor style/blink: `Term::cursor_style(&self) -> vte::ansi::CursorStyle { shape: CursorShape::{Block,Underline,Beam,HollowBlock,Hidden}, blinking: bool }` (mod.rs:942; honours vi-mode). Blink STATE (on/off phase) is NOT tracked by Term — only the `blinking` flag (set by DECSCUSR `CSI Ps SP q` and `?12 h/l`, emits `Event::CursorBlinkingChange`). The UI must run its own blink timer (e.g. 500ms toggle, reset on input) when `cursor_style().blinking`.
No hyperlink-at-point helper: `term.grid()[point].hyperlink()` where `Grid<T>: Index<Point>` (grid/mod.rs:469) and `Index<Line>` for rows. URL regex detection: `term::search::RegexSearch` (not exercised).

## 7. Scrolling + selection
```rust
use alacritty_terminal::grid::Scroll;   // grid/mod.rs:73  enum Scroll { Delta(i32), PageUp, PageDown, Top, Bottom }  (Delta>0 = scroll UP into history)
term.scroll_display(Scroll::Delta(3));  // mod.rs:389 ; emits MouseCursorDirty; clamps 0..=history_size
term.grid().display_offset() -> usize   // grid/mod.rs:432 ;  term.history_size()/screen_lines()/columns()/total_lines() via Dimensions trait (import `alacritty_terminal::grid::Dimensions`)
// selection.rs
pub enum SelectionType { Simple, Block, Semantic, Lines }
Selection::new(ty: SelectionType, location: Point, side: Side) -> Selection   // :125
selection.update(point: Point, side: Side)                                       // :133
pub selection: Option<Selection>  (pub field on Term)                              // set/clear directly: term.selection = Some(sel) / None
term.selection_to_string() -> Option<String>                                     // mod.rs:529 ; Lines type appends "\n"
term.bounds_to_string(start: Point, end: Point) -> String                        // mod.rs:558
Selection::is_empty(), to_range(&Term)->Option<SelectionRange>, Selection::include_all
// index.rs
pub type Side = Direction;  pub enum Direction { Left, Right }
pub struct Line(pub i32);  pub struct Column(pub usize);  pub struct Point<L=Line,C=Column>{ pub line: L, pub column: C }  Point::new(line, column)
```
Point.line is grid-absolute: viewport row r (0=top) with scroll offset d => `Line(r as i32 - d as i32)`; history lines are negative. Use `viewport_to_point(display_offset, Point::new(row_usize, Column(col)))`. Side = which half of the cell the mouse is in (Left if x frac <0.5). Semantic selection uses `Config.semantic_escape_chars`. Resize with column change auto-clears selection; selection is auto-cleared when overwritten by output.
OBSERVED: `seq 1 100` in 40x10 term, resize to 30x6 (`total=101 hist=95`), `Scroll::Delta(3)` => `display_offset 3`, rows `93..98`; selection viewport (1,0)->(2,3) => `selection_to_string() == Some("94\n95")`.
Mouse wheel when app has no mouse mode: if `mode.contains(ALT_SCREEN | ALTERNATE_SCROLL)` and not MOUSE_MODE, alacritty converts wheel to arrow keys (APP_CURSOR => `ESC O A/B`, else `ESC [ A/B`) — this logic is in the alacritty frontend, not in this crate; you implement it.

## 8. Misc
- `Term::mode() -> &TermMode` (mod.rs:709); `Term::resize<S: Dimensions>(&mut self, size: S)` (mod.rs:655); `Term::grid() -> &Grid<Cell>`, `grid_mut()`; `Term::colors() -> &Colors` (:952); `Term::cursor_style()` (:942); `Term::semantic_escape_chars()`; `pub is_focused: bool`; `pub selection`; `pub vi_mode_cursor`; `toggle_vi_mode`, `vi_motion`, `scroll_to_point`, `expand_wide(point, Direction)` (:901).
- Damage tracking: `term.damage() -> TermDamage<'_>`, `term.reset_damage()` (mod.rs:458,489) — optional; egui repaints whole frame so can ignore.
- Focus: Term does not emit focus reports; when `mode.contains(FOCUS_IN_OUT)` you write `ESC[I` / `ESC[O` via `Msg::Input` on focus change (tmux sets ?1004 itself).
- Bracketed paste: if `mode.contains(BRACKETED_PASTE)` write `ESC[200~` + text + `ESC[201~` (strip embedded `ESC[201~`); else write text with `\n`->`\r`.
- Mouse report encoding is yours: SGR (`ESC[<b;x;y M/m`) when SGR_MOUSE, UTF8_MOUSE, else legacy `ESC[M` + (32+b,32+x,32+y) limited to 223.
- `Term::reset_state` etc. not needed. `vi_mode` / `search` modules exist.

## 9. Working spike (compiled & run; Cargo `alacritty_terminal="=0.26.0"`, edition 2024)
Observed run output (stdout of spike):
```
EVENT ChildExit(ExitStatus(unix_wait_status(768)))
EVENT Title(hi)
EVENT Exit
EVENT Wakeup
GRID:
red
first cells fg: ["'r'/Named(Red)", "'e'/Named(Red)", "'d'/Named(Red)"]
cursor Point { line: Line(0), column: Column(3) } CursorStyle { shape: Block, blinking: false }
mode TermMode(SHOW_CURSOR | LINE_WRAP | ALTERNATE_SCROLL | URGENCY_HINTS)
ENV: TERM=dumb CT=truecolor WID=0 AWID=0 FOO=bar LOG=
EVENT3 ColorRequest(256) / ColorRequest(257) / ColorRequest(1) / TextAreaSizeRequest / MouseCursorDirty / Wakeup
REPLIES: b'\x1b[?6c\x1b[1;1R\x1b[?0u\x1b[8;5;80t\x1b[?1u\x1b]10;rgb:cccc/cccc/cccc\x1b\\\x1b]11;rgb:1010/1010/1010\x07\x1b]4;1;rgb:0101/0202/0303\x07'
after resize cols=30 lines=6 total=101 hist=95 ; display_offset 3 ; viewport 93..98 ; SEL Some("94\n95")
shutdown+join took 581.753µs
tmux: spawn `env -u WINDOWID -u ALACRITTY_WINDOW_ID tmux -L omp-deck-test-N -f /dev/null new-session -A -s t ...` via EventLoop with TERM=xterm-256color;
      grid showed tmux status bar "[t] 0:tmux*"; Shutdown+join+drop took 1.98ms; afterwards `tmux -L omp-deck-test-N ls` => `t: 1 windows` (server+session alive), `list-clients` empty, tmux killed with `kill-server` on the test socket.
```
(Note: observed `TERM=tmux-256color` inside the tmux pane; irrelevant.) Also note `CSI > 4;? m` produced no reply (untracked).

Code (exact, compiled):
```rust
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg, Notifier};
use alacritty_terminal::event::Notify;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::tty::{self, Options, Shell};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

#[derive(Clone)]
struct Listener {
    tx: Sender<Event>,
    pty: Arc<OnceLock<EventLoopSender>>,
}
impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(s) => {
                if let Some(p) = self.pty.get() {
                    let _ = p.send(Msg::Input(Cow::Owned(s.into_bytes())));
                }
            }
            e => {
                let _ = self.tx.send(e);
            }
        }
    }
}

struct Run {
    term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
    rx: mpsc::Receiver<Event>,
    jh: std::thread::JoinHandle<(EventLoop<tty::Pty, Listener>, alacritty_terminal::event_loop::State)>,
}

fn start(prog: &str, args: &[&str], env: &[(&str, &str)], cols: u16, rows: u16) -> Run {
    let ws = WindowSize { num_lines: rows, num_cols: cols, cell_width: 9, cell_height: 18 };
    let (tx, rx) = mpsc::channel();
    let pty_cell = Arc::new(OnceLock::new());
    let listener = Listener { tx, pty: pty_cell.clone() };
    let cfg = Config { kitty_keyboard: true, ..Config::default() };
    let term = Arc::new(FairMutex::new(Term::new(cfg, &TermSize::new(cols as usize, rows as usize), listener.clone())));
    let opts = Options {
        shell: Some(Shell::new(prog.into(), args.iter().map(|s| s.to_string()).collect())),
        working_directory: None,
        drain_on_exit: true,
        env: env.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<HashMap<_, _>>(),
    };
    let pty = tty::new(&opts, ws, 0).expect("pty");
    let el = EventLoop::new(term.clone(), listener, pty, opts.drain_on_exit, false).expect("el");
    let sender = el.channel();
    pty_cell.set(sender.clone()).ok();
    let jh = el.spawn();
    Run { term, sender, rx, jh }
}

fn dump(term: &Term<Listener>) -> String {
    let rc = term.renderable_content();
    let mut rows: Vec<String> = vec![String::new(); term.screen_lines()];
    for c in rc.display_iter {
        let row = (c.point.line.0 + rc.display_offset as i32) as usize;
        let mut s = String::new();
        s.push(c.cell.c);
        rows[row].push(c.cell.c);
    }
    rows.into_iter().map(|r| r.trim_end().to_string()).filter(|r| !r.is_empty()).collect::<Vec<_>>().join("\n")
}

```
Resize + scroll + select (compiled, from spike):
```rust
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::viewport_to_point;
let mut t = term.lock();
t.resize(TermSize::new(30, 6));
sender.send(Msg::Resize(WindowSize { num_lines: 6, num_cols: 30, cell_width: 9, cell_height: 18 })).unwrap();
t.scroll_display(Scroll::Delta(3));
let off = t.grid().display_offset();
let a = viewport_to_point(off, Point::new(1usize, Column(0)));
let b = viewport_to_point(off, Point::new(2usize, Column(3)));
let mut sel = Selection::new(SelectionType::Simple, a, Side::Left);
sel.update(b, Side::Right);
t.selection = Some(sel);
let text: Option<String> = t.selection_to_string();
// teardown: drop lock, then
let _ = sender.send(Msg::Shutdown);   // thread exits, Pty dropped (SIGHUP to child) when JoinHandle result dropped
```
Teardown tmux-safe: `sender.send(Msg::Shutdown)`; optionally `let (el, _state) = jh.join().unwrap(); drop(el);` — SIGHUPs only the tmux *client* child; server/session unaffected (verified).
