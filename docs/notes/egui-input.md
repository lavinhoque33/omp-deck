# egui 0.37 / egui-winit 0.37 / eframe 0.37 — input, clipboard, focus, window

Paths: `R=~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f`. All `file:line` below are relative to `$R`.
Snippets in §Recommended were compiled (`cargo check --offline`, throwaway crate /tmp/egui-input-check, same features/lock as workspace; deleted afterwards). Not run interactively (no display) -> runtime keyboard behaviour on a real Wayland session is UNVERIFIED where flagged.

## 0. TL;DR decisions
1. **Set `NativeOptions { clipboard_shortcuts: false, .. }`**. Then Ctrl+C/X/V (and the Copy/Cut/Paste keys) arrive as ordinary `Event::Key`. No `raw_input_hook` needed. (`eframe-0.37.0/src/epi.rs:416-423`; applied to egui-winit at `glow_integration.rs:1406` `egui_winit.set_clipboard_shortcuts(..)`.)
2. **In OmpDeck's current feature set egui-winit has NO clipboard backend** (eframe `default-features=false`, no `clipboard` feature -> egui-winit `clipboard` feature off; `cargo tree -p egui-winit -e normal --depth 1` lists only bytemuck, egui, log, profiling, raw-window-handle, serde, web-time, winit — no arboard / smithay-clipboard). Consequences, all from `egui-winit-0.37.0/src/clipboard.rs`:
   - `ctx.copy_text(..)` / `OutputCommand::CopyText` only writes an in-process `String` (`clipboard.rs:140`) -> **never reaches the system clipboard**.
   - Paste read returns `Some(self.clipboard.clone())` (`clipboard.rs:110`) = that same in-process string.
   - `Event::MiddleClickPaste` never fires (`get_primary_text` -> `None`, `lib.rs:890`), `TextSelectionSettled` is dropped.
   - Therefore: **own the system clipboard in the app with a single `arboard::Clipboard`** (clipboard + Primary via `GetExtLinux/SetExtLinux`). Do NOT enable eframe's `clipboard` feature (it would add a 2nd arboard + smithay-clipboard instance fighting ours over selection ownership; and egui-winit's `get()` prefers smithay, `clipboard.rs:~80-95`). Do not use `ctx.copy_text` for terminal copy.
3. Disable `ctx.options_mut(|o| o.zoom_with_keyboard = false)`: egui consumes Ctrl+`+`/`-`/`0` at end of every frame (`context.rs:2931`, `gui_zoom.rs:33-47`, default `true` `memory/mod.rs:336`) and rescales the UI — terminal needs those.
4. Terminal widget: `Sense::click_and_drag().union(Sense::focusable_noninteractive())`, `request_focus` on click, and every frame while focused call `set_focus_lock_filter` with all four flags `true`.

## 1. Clipboard key conversion (egui-winit)
`egui-winit-0.37.0/src/lib.rs:1120-1160` (`on_keyboard_input`), predicates at `lib.rs:1538-1556`:
```rust
fn is_cut_command(m, k)  -> k==Key::Cut   || (m.command_only() && k==Key::X) || (cfg!(windows) && m.shift && k==Key::Delete)
fn is_copy_command(m, k) -> k==Key::Copy  || (m.command_only() && k==Key::C) || (cfg!(windows) && m.ctrl  && k==Key::Insert)
fn is_paste_command(m,k) -> k==Key::Paste || (m.command_only() && k==Key::V) || (cfg!(windows) && m.shift && k==Key::Insert)
```
- `command_only()` = `!alt && !shift && command` (`egui-0.37.0/src/data/input/modifiers.rs:172`); on Linux `command == ctrl` (`lib.rs:564`). So on Linux **only plain Ctrl+X / Ctrl+C / Ctrl+V** (no Shift, no Alt) plus the dedicated `Key::Cut/Copy/Paste` (XF86 keys) convert.
- **Shift+Insert, Ctrl+Insert, Shift+Delete are Windows-only (`cfg!(target_os="windows")`) -> on Linux they are plain `Event::Key`** (key=Insert/Delete, modifiers shift / ctrl). Ctrl+Shift+C / Ctrl+Shift+V are plain `Event::Key` (not command_only).
- Only evaluated when `pressed && self.clipboard_shortcuts` (`lib.rs:1129`). **On match the function `return`s (`lib.rs:1132,1135,1151`): NO `Event::Key` (neither press) is emitted, and no `Event::Text`.** Key *release* of C/X/V is NOT converted (pressed==false) -> a lone `Event::Key{pressed:false}` still arrives.
- Ctrl+V details (`lib.rs:1136-1151`): `clipboard.get()` text -> `\r\n`→`\n`, if non-empty `Event::Paste(String)`; else if `get_image()` -> `Event::PasteImage(Arc<ColorImage>)`; **if neither (empty/no text, no image) nothing at all is emitted** (event swallowed, still returns). Image-only clipboard -> `PasteImage`, not `Paste`. Note with the current build (no backend) `get()` returns the internal string, `get_image()` is never reached.
- `Event::Copy`, `Event::Cut` are unit variants; `Event::Paste(String)` carries **no modifiers** (`egui-0.37.0/src/data/input/event.rs:18-40`).
- Configurable: yes. `eframe::NativeOptions::clipboard_shortcuts: bool` (default `true`, `epi.rs:423,502`) / `egui_winit::State::set_clipboard_shortcuts(bool)` (`lib.rs:290`). Doc: "so your app can bind them … passed on as ordinary Event::Key" (`lib.rs:275-287`).
- With `false`, Ctrl+C -> `Event::Key{key: C, modifiers{ctrl,command}, pressed:true}`. Text is NOT emitted for any key while ctrl is held (`lib.rs:1170-1182`: `pressed && !is_cmd`, is_cmd = ctrl||command||mac_cmd).

## 2. Raw passthrough design
### (A) RECOMMENDED: `clipboard_shortcuts: false`
All combos are `Event::Key` with accurate per-event `modifiers` (copy of `State.modifiers`, updated by `WindowEvent::ModifiersChanged`, `lib.rs:551-568`; reset on focus loss `lib.rs:495`). Terminal widget code (compiled):
```rust
use eframe::egui::{self, Event, EventFilter, Key, Modifiers, MouseWheelUnit, CursorIcon};

fn ctrl_byte(key: Key) -> Option<u8> {            // Ctrl+letter etc. -> C0
    Some(match key {
        Key::A=>1, Key::B=>2, Key::C=>3, Key::D=>4, Key::E=>5, Key::F=>6, Key::G=>7, Key::H=>8, Key::I=>9,
        Key::J=>10, Key::K=>11, Key::L=>12, Key::M=>13, Key::N=>14, Key::O=>15, Key::P=>16, Key::Q=>17,
        Key::R=>18, Key::S=>19, Key::T=>20, Key::U=>21, Key::V=>22, Key::W=>23, Key::X=>24, Key::Y=>25, Key::Z=>26,
        Key::OpenBracket=>27, Key::Backslash=>28, Key::CloseBracket=>29, Key::Space | Key::Num2 => 0, Key::Minus=>31,
        _ => return None })
}
// per frame, only if resp.has_focus():
let events = ctx.input(|i| i.events.clone());
for ev in &events {
    match ev {
        Event::Key { key, pressed: true, modifiers: m, .. } => {          // `repeat` is filled by egui for held keys
            if m.ctrl && m.shift && !m.alt && *key == Key::C { /* local copy */ continue }
            if m.ctrl && m.shift && !m.alt && *key == Key::V { /* local paste (own arboard) */ continue }
            if m.shift && !m.ctrl && !m.alt && *key == Key::Insert { /* local paste */ continue }
            if m.ctrl && !m.shift && !m.alt { if let Some(b) = ctrl_byte(*key) { /* write [b] */ } }
            /* else: encode Enter/Tab/Esc/arrows/F-keys/Home/End/... as xterm/kitty sequences */
        }
        Event::Text(t) => { /* write t.as_bytes() */ }
        _ => {}
    }
}
```
Whole-app wiring (compiled):
```rust
let opts = eframe::NativeOptions {
    viewport: egui::ViewportBuilder::default().with_app_id("omp-deck").with_title("OmpDeck")
        .with_inner_size([1200.0, 800.0]).with_min_inner_size([480.0, 320.0]),
    clipboard_shortcuts: false,
    persist_window: true,
    ..Default::default()
};
eframe::run_native("OmpDeck", opts, Box::new(|cc| {
    cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
    Ok(Box::new(App::new(cc)))      // AppCreator: Box<dyn FnOnce(&CreationContext)->Result<Box<dyn App>, DynError>>
}))
```
### (B) Alternative: keep conversion, undo in `raw_input_hook`
- Exists: `eframe::App::raw_input_hook(&mut self, _ctx: &egui::Context, _raw_input: &mut egui::RawInput)` default no-op (`eframe-0.37.0/src/epi.rs:282`). Called in `prepare_raw_input` right after merging pending + new winit input, before the egui pass (`native/epi_integration.rs:417-426`; also before `logic()` pass at ~395). Runs on every pass.
- **`egui::RawInput` has NO `modifiers` field in 0.37** (fields: viewport_id, viewports, safe_area_insets, screen_rect, max_texture_side, time, predicted_dt, events, hovered_files, dropped_files, focused, system_theme — `egui-0.37.0/src/data/input/raw_input.rs:18-`). Modifiers must be tracked yourself from `Event::ModifiersChanged(m)` (state persists across frames; reset on `Event::WindowFocused(false)`). `Copy/Cut/Paste` carry no modifiers; but since conversion only happens for command_only (no shift/alt), a converted `Paste` is always plain Ctrl+V on Linux, so shift/alt are known to be absent. Plain Ctrl+Shift+V / Shift+Insert already arrive as Key events and need no rewrite.
- Compiled rewrite:
```rust
#[derive(Default)] struct HookState { mods: Modifiers }
fn undo_clipboard_events(st: &mut HookState, raw: &mut egui::RawInput) {
    let mut out = Vec::with_capacity(raw.events.len());
    for ev in raw.events.drain(..) {
        let key = match &ev {
            Event::ModifiersChanged(m) => { st.mods = *m; None }
            Event::WindowFocused(false) => { st.mods = Modifiers::NONE; None }
            Event::Copy => Some(Key::C), Event::Cut => Some(Key::X), Event::Paste(_) => Some(Key::V),
            _ => None };
        match key { Some(k) => out.push(Event::Key{ key:k, physical_key:Some(k), pressed:true, repeat:false, modifiers: st.mods }),
                    None => out.push(ev) }
    }
    raw.events = out;
}
```
 Downsides vs (A): lost key *release* is fine, but Ctrl+V with empty clipboard emits nothing (event swallowed in egui-winit) so Ctrl+V 0x16 would be silently dropped -> **(A) is the only fully reliable option**. Also needs the clipboard backend for conversion to even fire the Paste. Use (A).

## 3. Event::Key / Key / Text / IME
`egui-0.37.0/src/data/input/event.rs:18-` (`Event` is `#[derive(Clone, Debug, PartialEq)]`), variants: `Copy, Cut, Paste(String), PasteImage(Arc<ColorImage>), MiddleClickPaste{pos,text}, Text(String), Key{..}, ModifiersChanged(Modifiers), PointerMoved(Pos2), MouseMoved(Vec2), PointerButton{pos,button,pressed,modifiers}, PointerGone, Zoom(f32), Rotate(f32), Ime(ImeEvent), Touch{..}, MouseWheel{..}, WindowFocused(bool), AccessKitActionRequest(..)`.
```rust
Event::Key { key: Key, physical_key: Option<Key>, pressed: bool, repeat: bool, modifiers: Modifiers }
pub struct Modifiers { pub alt: bool, pub ctrl: bool, pub shift: bool, pub mac_cmd: bool, pub command: bool }  // modifiers.rs:19-37. NO super/logo field: Super/Meta is invisible to egui on Linux.
```
- `repeat` is overwritten by egui (`input_state/mod.rs:404-412`, `!keys_down.insert(key)`); winit's repeat flag ignored (`lib.rs:1112 repeat: _`).
- `key` = logical key via `key_from_winit_key(logical_key)` (Character -> `Key::from_name`, Named -> `key_from_named_key`), falling back to physical (`logical_key.or(physical_key)`, `lib.rs:1127`). If both None, **no Event::Key at all** (Text may still be emitted). Lone modifier presses (Shift/Ctrl/Alt/Super) never produce Event::Key (only `ModifiersChanged`).
- `Key` variants (`data/key.rs:7-`): ArrowDown ArrowLeft ArrowRight ArrowUp Escape Tab Backspace Enter Space Insert Delete Home End PageUp PageDown Copy Cut Paste; punctuation: Colon Comma Backslash Slash Pipe Questionmark Exclamationmark OpenBracket CloseBracket OpenCurlyBracket CloseCurlyBracket Backtick Minus Period Plus Equals Semicolon Quote; Num0..Num9; A..Z; **F1..F35**; BrowserBack/Forward/Refresh/Search/Home/Favorites/Stop, MediaPlayPause/TrackNext/TrackPrevious/Stop, AudioVolumeMute/Down/Up, LaunchMail/App1/App2, CapsLock NumLock ScrollLock PrintScreen Pause Menu Fn Eject Help Power Sleep Clear (list at key.rs:334 `Key::ALL`; `Key::from_name` key.rs:503, `Key::name()` 704).
  - **Missing keys**: no `<`, `>`, `@`, `#`, `~`, `"`, `$`, `%`, `^`, `&`, `*`, `(`, `)`, `_`. `Quote` is `'` only. For shifted chars whose logical char has no Key (e.g. Shift+' = `"`), `key` falls back to the *physical* key (Quote) — so `Key` alone is NOT enough to know the produced character; use `Event::Text`.
  - Numpad: digits/operators map onto Num*/Minus/Plus/Slash/Period..., NumpadEnter -> Enter; numpad keys are indistinguishable from main keys (location dropped, `lib.rs:1102 location: _`). Keypad-application mode sequences cannot be produced.
  - Letter case: logical "A" and "a" both -> `Key::A`; Shift is in `modifiers`.
- **Event::Text** (`lib.rs:1163-1183`): emitted AFTER the Key event for the same keypress, same `on_keyboard_input` call (order: `Key` then `Text`), from `KeyEvent.text` (or logical_key.to_text()), iff `pressed && !(ctrl||command||mac_cmd)` and text non-empty and every char is printable (`!is_ascii_control`, not private-use). Hence:
  - **Alt+printable: Text IS emitted** (alt isn't in is_cmd): Key{X, alt} then Text("x"). (UNVERIFIED on real Wayland whether xkb text under Alt differs — it doesn't change keysym; verify with logging.) Terminal must treat Alt+char as ESC-prefix itself: take `Text` for the char and `modifiers.alt` from the preceding Key.
  - **Ctrl+anything: no Text** (use Key + `ctrl_byte`). Ctrl+Alt: no Text either.
  - **Shift+Enter / Enter / Tab / Esc / Backspace: no Text** (control chars filtered; docs on `Event::Text`: Enter sends only Key::Enter). Shift+Enter is just Key{Enter, shift}. Same Shift+Tab.
  - **AltGr**: no special handling in egui-winit. winit/xkb on Linux reports AltGr (ISO_Level3_Shift, Mod5) as neither `alt` nor `ctrl` in `ModifiersChanged` (UNVERIFIED — verify by pressing AltGr+Q on a de layout and logging events), so the produced char arrives as `Event::Text("@")` with a Key whose logical key may be unmapped -> falls back to physical. Treat `Text` as authoritative for printable chars.
  - Key release with text: Text only when `pressed` (`lib.rs:1179`).
  - Repeat: each auto-repeat produces another Key{repeat:true} and another Text.
- **Unmodified/shifted character for kitty/modifyOtherKeys**: Event gives no "base layout key" field. Available: `key` (logical, layout-aware, shift-applied only if it maps to a `Key`: Shift+1 -> logical "!" -> `Exclamationmark`; Shift+; -> `Colon`), `physical_key` (layout-independent; US-position key via `key_from_key_code`, `lib.rs:1677-`; None for unmapped codes), `modifiers`, and the following `Event::Text` (shifted result, only without ctrl). For CSI-u codepoints derive from `key` (map Key->unshifted char) + `physical_key` for the "base layout key" field; for Ctrl-held keys Text is absent so rely on Key table. No access to the raw winit `KeyEvent` through eframe (UNVERIFIED that no hook exists; none found in `epi.rs`).
- **IME**: `Event::Ime(ImeEvent)`; `ImeEvent` = `Preedit{text: String, active_range_chars: Option<Range<usize>>}`, `Commit(String)`, `DeleteSurrounding{before_chars, after_chars}`, and deprecated `Enabled`/`Disabled` (`ime_event.rs:6-`). egui-winit only forwards Preedit and Commit (`lib.rs:787-828`). **IME is only enabled if the app sets `PlatformOutput::ime = Some(IMEOutput{..})` that frame** (`lib.rs:1257-1262`: `window.set_ime_allowed(ime.is_some())`):
```rust
ui.output_mut(|o| o.ime = Some(egui::output::IMEOutput {
    purpose: egui::IMEPurpose::Terminal,           // Normal | Password | Terminal (viewport.rs:1033)
    rect: widget_rect, cursor_rect: cursor_cell_rect,   // cursor rect positions the candidate window (lib.rs:1280-)
    should_interrupt_composition: false }));
```
  Only when `ctx.memory(|m| m.owns_ime_events(id))` (= widget has focus, `memory/mod.rs:1069`). Without this, no Ime events and dead keys/compose may still work through xkb Text.

## 4. Focus lock / EventFilter
```rust
pub struct EventFilter { pub tab: bool, pub horizontal_arrows: bool, pub vertical_arrows: bool, pub escape: bool }  // data/input/event_filter.rs:11; Default = all false
Memory::set_focus_lock_filter(&mut self, id: Id, event_filter: EventFilter)                                       // memory/mod.rs:908
```
- Semantics: `true` = widget *wants* the key; egui then does NOT do focus navigation/drop for it (`memory/mod.rs:575-606`: nav only if `!event_filter.matches(event)`). Nav keys only act with no modifiers (Tab: none or shift-only; arrows: none; Escape: none). Events are NOT removed from `ctx.input(|i| i.events)` — the filter only gates egui's own focus behaviour, you always still see the Key events.
- Must be called every frame while focused, only effective if the widget has focus *and* had focus last frame (`memory/mod.rs:908-916`: `had_focus_last_frame(id) && has_focus(id)`) — so first frame after `request_focus` it is a no-op; one frame of Tab leakage is possible only if Tab pressed that exact frame. The filter is reset to default when focus is reassigned (`FocusWidget::new`).
- Setting all four true => Tab, Shift+Tab, arrows, **Escape** do not drop focus. Without `escape: true`, Escape (no modifiers) sets `focused_widget = None` (`memory/mod.rs:595-598`).
- Request/hold focus: `Response::request_focus()` (`response.rs:433`), `Memory::request_focus(id)` (memory:921+). Widget must be focusable: `Sense::focusable_noninteractive()` (sense.rs:52) or `Sense::click_and_drag()` (interactive senses are focusable). Clicking outside surrenders focus per `InputOptions::surrender_focus_on` (`input_state/mod.rs:98`, applied `context.rs:1844-1852`) — a click on a different widget/background removes focus; re-request on click of the terminal. `Response::has_focus()` also requires window focus (`response.rs:405`). Focus lost if the widget isn't rendered for one frame (dead-man switch, `memory/mod.rs:~625`).
- Compiled:
```rust
let (rect, resp) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag().union(egui::Sense::focusable_noninteractive()));
if resp.clicked() || resp.drag_started() { resp.request_focus(); }
if resp.has_focus() {
    ui.memory_mut(|m| m.set_focus_lock_filter(resp.id, EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true }));
}
```
- App-level shortcuts (Ctrl+Tab etc.) still get Key events: read them via `ctx.input` before/regardless of focus; egui doesn't consume them (except zoom, §0.3).

## 5. Window focus, mouse
- `Event::WindowFocused(bool)` pushed on `WindowEvent::Focused` (`lib.rs:483-502`); also `InputState::focused` and `ctx.input(|i| i.viewport().focused)`. On focus loss egui-winit zeroes its modifiers (`lib.rs:495`) and egui clears `keys_down`+modifiers (`input_state/mod.rs:443-449`). No synthetic key-up events -> on regain, rely on fresh events.
- `PointerButton`: `enum PointerButton { Primary=0, Secondary=1, Middle=2, Extra1=3, Extra2=4 }` (`pointer_button.rs:4`); winit Left/Right/Middle/Back/Forward map via `translate_mouse_button` (`lib.rs:1560-`). `Event::PointerButton{pos: Pos2, button, pressed, modifiers}` (event.rs, points). Ignored if no pointer pos known yet (`lib.rs:~872` `if let Some(pos)=pointer_pos_in_points`).
- Move: `Event::PointerMoved(Pos2)` (points), `Event::MouseMoved(Vec2)` (raw), `Event::PointerGone` on leave.
- Wheel: `Event::MouseWheel { unit: MouseWheelUnit, delta: Vec2, phase: TouchPhase, source: MouseWheelSource, modifiers: Modifiers }`. `MouseWheelUnit { Point, Line, Page }` (`mouse_wheel_unit.rs:4`). On Linux/winit: `LineDelta(x,y)` -> `Line` with delta=(x,y) lines (mouse wheel, one notch = 1.0 ... verify per-device); `PixelDelta` -> `Point` converted to egui points (physical px / pixels_per_point, `lib.rs:1027-1040`). `Page` is never produced by egui-winit. Positive `delta.y` = scroll up (content moves down, same as winit). Read raw from `ctx.input(|i| i.events.clone())`; `InputState::smooth_scroll_delta()` (input_state/mod.rs:553) is already converted to points (line*`line_scroll_speed`=40.0 on native, `input_state/mod.rs:105-110`) and smoothed — for a terminal use the raw Event and convert yourself: lines = `delta.y` for Line; `delta.y / cell_height_points` for Point (accumulate fractional remainder). `modifiers` on the event = state at that time; note egui also turns Ctrl+wheel into zoom inside `InputState` (`zoom_modifier`) but raw events are unaffected.
- Middle-click paste: with no backend `MiddleClickPaste` never fires; handle `PointerButton{button: Middle, pressed:true}` yourself and read PRIMARY via arboard.

## 6. Clipboard output
- `Context::copy_text(&self, text: String)` = `send_cmd(OutputCommand::CopyText(text))` (`context.rs:1995`); `OutputCommand` variants: `CopyText(String)`, `TextSelectionSettled(String)` (-> PRIMARY), `CopyImage(ColorImage)`, `OpenUrl(OpenUrl)` (`data/output.rs:121-`). Executed in `handle_platform_output_inner` (`lib.rs:1241-1252`) -> `Clipboard::set_text` -> smithay (if feature + Wayland display) else arboard else internal string (`clipboard.rs:~118-140`).
- Backend gating: smithay-clipboard + arboard are optional deps behind egui-winit `clipboard = ["arboard","bytemuck","smithay-clipboard"]` (egui-winit Cargo.toml:62; eframe `clipboard = ["egui-winit/clipboard"]`). OmpDeck's tree has neither => `copy_text` is a no-op for other apps (§0.2). If you did enable it: smithay used on Wayland (wl-seat based, needs focus/serial), arboard as fallback; reading: `smithay.load()` first, then arboard (`clipboard.rs:~80-107`). It would coexist with our arboard (separate instance), but two owners of the selection -> avoid.
- Our own arboard (compiled against 3.7.0, `arboard-3.7.0/src/platform/linux/mod.rs:105-210,308`):
```rust
use arboard::{Clipboard, GetExtLinux, SetExtLinux, LinuxClipboardKind};
let mut cb = Clipboard::new()?;                                    // create once, keep in App; Send? create on UI thread
let text = cb.get().clipboard(LinuxClipboardKind::Clipboard).text();       // Result<String, arboard::Error>; Err(ContentNotAvailable) if image-only/empty
cb.set().clipboard(LinuxClipboardKind::Clipboard).text(s)?;
cb.set().clipboard(LinuxClipboardKind::Primary).text(selection)?;           // on select; Wayland Primary needs compositor support (else Err)
let p = cb.get().clipboard(LinuxClipboardKind::Primary).text();
```
  Wayland data-control availability under KWin and X11 ownership lifetime: UNVERIFIED here (belongs to the arboard note); verify by running a set then `wl-paste`.
- `Context` is `#[derive(Clone)] pub struct Context(Arc<RwLock<ContextImpl>>)` (`context.rs:779-780`); compile-checked that a cloned `Context` can be moved into `std::thread::spawn` and `request_repaint()` called there. Doc (`context.rs:2120-2127`): from outside the UI thread it wakes the UI thread (eframe sets the callback). `request_repaint_after(Duration)` also exists.

## 7. eframe 0.37 API
- **Required trait method is `ui`, NOT `update`**: `fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame);` (`epi.rs:185`). `update` does not exist. Optional: `fn logic(&mut self, ctx:&egui::Context, frame:&mut Frame)` (called before each `ui`, also when hidden/repaint requested; no painting; `epi.rs:170`), `fn save(&mut self, storage:&mut dyn Storage)` (`:225`; only with `persistence` feature; auto every `auto_save_interval()` = 30 s, and on exit), `fn on_exit(&mut self, gl: Option<&glow::Context>)` (glow feature; `:236`), `fn persist_egui_memory(&self)->bool` (default true, `:262`), `fn raw_input_hook(..)` (`:282`), `clear_color`. The `ui` gets a `Ui` with no margin/background — wrap in `egui::CentralPanel::default().show_inside(ui, ..)` / Frame. Get `ctx` via `ui.ctx()`.
- `eframe::run_native(app_name: &str, native_options: NativeOptions, app_creator: AppCreator<'_>) -> eframe::Result` (`lib.rs:288`); `AppCreator<'app> = Box<dyn 'app + FnOnce(&CreationContext<'_>) -> Result<Box<dyn 'app + App>, DynError>>` (`epi.rs:49`). Also `run_native_ext(app_name, opts, Option<egui::Context>, creator)` (`lib.rs:306`).
- `CreationContext<'s>` pub fields: `egui_ctx: egui::Context`, `integration_info: IntegrationInfo`, `storage: Option<&'s dyn Storage>`, `gl: Option<Arc<glow::Context>>` (glow), `get_proc_address: Option<..>` (glow); `winit_window()` accessor (`epi.rs:53-95,143`). Clone `cc.egui_ctx` to hand to background threads.
- `NativeOptions` pub fields (`epi.rs:299-433`): `viewport: egui::ViewportBuilder, multisampling: u16, depth_buffer: u8, stencil_buffer: u8, renderer, run_and_return: bool, event_loop_builder, window_builder, centered: bool, glow_options, wgpu_options, persist_window: bool (default true `:494`), system_font_fallback, persistence_path: Option<PathBuf>, dithering: bool, clipboard_shortcuts: bool (default true), android_app`. Use `..Default::default()`.
- `ViewportBuilder` (`egui-0.37.0/src/viewport.rs`): `.with_app_id(impl Into<String>)` (:651; becomes the **Wayland app_id** via `WindowAttributesExtWayland::with_name(app_id, "")` `egui-winit lib.rs:2378-2381` — must match the .desktop file name; also the storage dir name), `.with_title(..)` (:361), `.with_inner_size(impl Into<Vec2>)` (:537; `[f32;2]` ok), `.with_min_inner_size` (:550), `.with_max_inner_size` (:563), `.with_position`, `.with_icon(Arc<IconData>)`, `.with_decorations`, `.with_resizable`, `.with_transparent`, `.with_maximized`, `.with_visible`, `.with_active`, `.with_window_level`. If `app_id` unset, `run_native`'s `app_name` is used for storage (`glow_integration.rs:315-322`) — but NOT for the Wayland app_id, so set `with_app_id`.
- Persistence: `persist_window: true` saves/restores size+pos+maximized under key `"window"` (`epi_integration.rs:478-488,516`) — restored geometry overrides `with_inner_size` after first run. egui memory under its own key if `persist_egui_memory()`. Storage: `eframe::Storage` trait (`get_string/set_string/remove_string/flush`, `epi.rs:1086`); `eframe::get_value<T: DeserializeOwned>(&dyn Storage, &str) -> Option<T>` and `eframe::set_value<T: Serialize>(&mut dyn Storage, &str, &T)` (`epi.rs:1102,1117`; RON; need `persistence` feature -> enabled). Location (Linux): `$XDG_DATA_HOME` (absolute) else `~/.local/share`, joined with `app_id` lowercased with whitespace stripped, file `app.ron` -> `~/.local/share/omp-deck/app.ron` for app_id `omp-deck` (`file_storage.rs:17-28,131-145`). `eframe::storage_dir(app_id)` is public. Override with `NativeOptions::persistence_path`.
```rust
fn save(&mut self, s: &mut dyn eframe::Storage) { eframe::set_value(s, "state", &self.state); }
// in creator: let st: Option<State> = cc.storage.and_then(|s| eframe::get_value(s, "state"));
```
- Title at runtime: `ctx.send_viewport_cmd(egui::ViewportCommand::Title(String))` (`viewport.rs:1096`; `Context::send_viewport_cmd(&self, ViewportCommand)` context.rs:4549). Compiled.

## 8. Cursor icons
`ctx.set_cursor_icon(egui::CursorIcon)` (`context.rs:1955`, = `output_mut(|o| o.cursor_icon = ..)`); applied at end of frame in `handle_platform_output` -> `window.set_cursor` (`lib.rs:1396-1410`, de-duplicated). Resets to `Default` every frame, so call each frame while hovering. Variants (`data/output.rs:356`): Default None ContextMenu Help PointingHand Progress Wait Cell Crosshair **Text** VerticalText Alias Copy Move NoDrop NotAllowed Grab Grabbing AllScroll ResizeHorizontal ResizeNeSw ResizeNwSe ResizeVertical ResizeEast ResizeSouthEast ResizeSouth ResizeSouthWest ResizeWest ResizeNorthWest ResizeNorth ResizeNorthEast ResizeColumn ResizeRow ZoomIn ZoomOut. `CursorIcon::None` hides the cursor (`set_cursor_visible(false)`, lib.rs:1408). Alternatively `resp.on_hover_cursor(CursorIcon::Text)`. Custom bitmap: `ctx.set_cursor_image(Option<CustomCursorImage>)` (context.rs:1968).

## 9. Gotchas summary
- `Event::Text` never carries ctrl combos; ordering is Key then Text within one keypress.
- egui consumes (not removes from `events`) Ctrl+/-/0 for zoom; disable `zoom_with_keyboard`.
- Super/Meta never visible. No numpad/location info. Keys without a `Key` variant (`<>"@#~$%^&*()_`) fall back to physical key; use Text.
- Dead keys: logical `Dead(_)` -> None -> falls back to physical key + no text until composed (UNVERIFIED on Wayland; text comes via `KeyEvent.text` or IME Commit).
- After focus loss egui drops modifiers/keys; terminal should release kitty-protocol key state on `WindowFocused(false)`.
