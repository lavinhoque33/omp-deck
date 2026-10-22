# egui/eframe 0.37 — modern app chrome for OmpDeck

Paths: `E=~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/` → `egui-0.37.0/src/…`, `epaint-0.37.0/src/…`, `eframe-0.37.0/src/…`, `winit-0.30.13/src/…`.
**Everything marked "(compiled)" is in `/tmp/omp-deck-notes/egui-style-demo.rs`** (full 480-line app; deps in `egui-style-demo.Cargo.toml` = workspace features: eframe default-features=false+`default_fonts,glow,wayland,x11`, epaint `color_fonts`, + `egui_material_icons 0.9`). Screenshot (Xvfb, software GL, right-click menu + submenu open): `/tmp/omp-deck-notes/egui-style-demo.png`. Modal and toast were also exercised via XTEST clicks (both rendered correctly; screenshots not kept).

---------------------------------------------------------------------------------------------------
## 0. 0.37 API renames that will bite (all hit while compiling the demo)
| old (≤0.33) | 0.37 | evidence |
|---|---|---|
| `App::update(&mut self, ctx, frame)` | `App::ui(&mut self, ui: &mut egui::Ui, frame)` (+ optional `logic(&mut self, ctx, frame)`) | `eframe-0.37.0/src/epi.rs:170,185` (existing app already uses it: `crates/omp-deck/src/app.rs:770`) |
| `SidePanel`, `TopBottomPanel` | **removed**; `egui::Panel::{left,right,top,bottom}(id_salt)` + `.show(ui, ..)` | `containers/panel.rs:207,252-277,425`; `.show_inside` is `#[deprecated]` (`:431`) |
| `CentralPanel::default().show(ctx,..)` | `.show(ui, ..)` (takes `&mut Ui`) | `panel.rs:1377` |
| `Id::new(x)` | `Id::unique(x)` (global) / `IdSalt::new` (local); `Id::new` deprecated | `id.rs:91,101` |
| `ui.id()` | `ui.scope_id()` | compiler warning |
| `ctx.style()` | `ctx.global_style()`; `ctx.global_style_mut(f)`; `ctx.set_global_style(s)` | `context.rs:2599,2613,2624` |
| `ui.visuals_mut`/`ctx.set_visuals` | unchanged; `ctx.set_visuals(v)` = current theme only (`context.rs:2704`) | |
Other: `Button::new/MenuButton/ComboBox::new/TextEdit::hint_text/prefix/suffix/Slider::prefix/DragValue::suffix` take `impl IntoAtoms` (so `(RichText, "text")` tuples work, compiled). `Ui::scope_builder(UiBuilder, f)`; `Rect` shrink etc. unchanged. `ui.set_opacity(f32)` exists (`ui.rs:587`).

---------------------------------------------------------------------------------------------------
## 1. Style / Visuals
### 1.1 Setting them (`context.rs:2594-2710`)
```rust
ctx.set_theme(egui::ThemePreference::Dark);               // :2594 (Dark/Light/System)
ctx.style_mut_of(egui::Theme::Dark, |style| { ... });     // :2661  mutate one theme's Style
ctx.all_styles_mut(|style| { ... });                      // :2637  both dark+light (FnMut)
ctx.set_style_of(egui::Theme::Dark, style);               // :2674
ctx.set_visuals_of(egui::Theme::Dark, visuals);           // :2691  (= style_mut_of(.., |s| s.visuals = v))
ctx.set_visuals(visuals);                                 // :2704  active theme only
```
Existing app sets `ctx.set_visuals(visuals(cfg.theme))` (`app.rs:106,419`) → switch to `style_mut_of(Theme::Dark, apply_theme)` once, and keep `set_theme` as the dark/light switch. Per-Ui override: `ui.style_mut()` / `ui.visuals_mut()` / `ui.spacing_mut()`.

### 1.2 Exact field names (`style.rs`)
* `Style` (`:244`): `text_styles: BTreeMap<TextStyle, FontId>` (:289), `spacing: Spacing` (:309), `interaction`, `visuals: Visuals` (:315), `animation_time: f32` (:318), `wrap_mode`, `compact_menu_style: bool` (:341), `drag_value_text_style`, `override_font_id`, `scroll_animation`.
* `TextStyle` (`:72`): `Small, Body, Monospace, Button, Heading, Name(Arc<str>)`. Custom: `TextStyle::Name("Section".into())` + insert into `style.text_styles`; use `RichText::text_style(TextStyle::Name(..))` (compiled).
* `Spacing` (`:386`): `item_spacing: Vec2`, `window_margin: Margin`, `button_padding: Vec2`, `menu_margin: Margin`, `indent`, `interact_size: Vec2`, `slider_width`, `slider_rail_height`, `combo_width`, `text_edit_width`, `icon_width/icon_width_inner/icon_spacing`, `tooltip_width`, `menu_width`, `menu_spacing`, `combo_height`, `scroll: ScrollStyle`.
* `ScrollStyle` (`:496`): `floating: bool`, `content_margin`, `bar_width`, `handle_min_length`, `bar_inner_margin`, `bar_outer_margin`, `floating_width`, `floating_allocated_width`, `foreground_color`, `{dormant,active,interact}_{background,handle}_opacity`, `fade`, `kinetic`. Presets `ScrollStyle::solid()` `:597`, `::thin()` `:624`, `::floating()` `:648` (**default** = floating). Modern look: `floating()` + `bar_width=6; floating_width=4` (compiled).
* `Visuals` (`:1099`): `dark_mode`, `override_text_color: Option<Color32>`, `weak_text_alpha`, `weak_text_color`, `widgets: Widgets`, `selection: Selection{bg_fill, stroke}` (:1304), `hyperlink_color`, `faint_bg_color`, `extreme_bg_color` (TextEdit/slider rail bg), `text_edit_bg_color: Option`, `code_bg_color`, `warn_fg_color`, `error_fg_color`, `window_corner_radius: CornerRadius`, `window_shadow: Shadow`, `window_fill`, `window_stroke: Stroke`, `menu_corner_radius`, `panel_fill`, `popup_shadow: Shadow`, `slider_trailing_fill: bool`, `handle_shape: HandleShape` (`style::HandleShape::Circle | Rect{aspect_ratio}`), `button_frame`, `collapsing_header_frame`, `striped`, `disabled_alpha`, `text_cursor`, `interact_cursor`, `text_options` (new: colour transfer fn).
* `Widgets` (`:1360`): `noninteractive, inactive, hovered, active, open: WidgetVisuals`. `WidgetVisuals` (`:1400`): `bg_fill`, `weak_bg_fill` (buttons use this; may be transparent), `bg_stroke: Stroke`, `corner_radius: CornerRadius`, `fg_stroke: Stroke` (text/icon colour), `expansion: f32`.
* `CornerRadius { nw, ne, sw, se: u8 }`, `CornerRadius::same(u8)` (`epaint-0.37.0/src/corner_radius.rs:13,59`). `Margin { left,right,top,bottom: i8 }`; `Margin::same(i8)`, `::symmetric(i8,i8)`. `Shadow { offset: [i8;2], blur: u8, spread: u8, color: Color32 }` (`epaint/src/shadow.rs:10-27`), `Shadow::NONE`, `shadow.as_shape(rect, corner_radius) -> RectShape` (:48, real blurred shadow ⇒ use for glows, compiled). **Radii/margins/blur are integers (u8/i8)**; `Frame::corner_radius(impl Into<CornerRadius>)` accepts a plain `u8`/`i8`… (compiled with `10`, `8`, `14`).
* Presets that read these fields: `Frame::window/menu/popup(style)` all use `window_fill()`, `window_stroke()`, `menu_corner_radius`, `popup_shadow`, `spacing.menu_margin` (`frame.rs:196-224`). Tooltips use `Spacing::tooltip_width` + popup frame.
* **Gotcha (observed)**: `egui::containers::menu::menu_style` (`menu.rs:25`) is re-applied to every menu/submenu Ui: sets `button_padding=(2,0)`, `inactive.weak_bg_fill=TRANSPARENT`, no strokes. So menu items look cramped. Fix inside each menu closure: `ui.spacing_mut().button_padding = vec2(10.0, 6.0)` (compiled `menu_pad`). Also **keep `window_fill` ≠ `widgets.hovered.weak_bg_fill`** else menu hover is invisible (observed).
* Dark theme defaults for reference (`style.rs:1608+`): window_corner_radius 6, window_shadow offset [10,20] blur 15, window_fill gray(27), panel_fill gray(27), popup_shadow [6,10] blur 8, extreme_bg gray(10), hyperlink (90,170,255).

### 1.3 Palette + theme used in the demo (compiled, `apply_theme`)
`BG #0A0E1A, PANEL #0F1424, SURFACE #161D33, RAISED #1E2744, STROKE #263050, TEXT #D6DEF0, TEXT_WEAK #7C87A8, ACCENT #6C7CFF (only vivid colour; OK #34D399 status, DANGER #F87171)`. Key lines:
```rust
ctx.style_mut_of(Theme::Dark, |style| {
    style.text_styles = [(TextStyle::Small, FontId::new(11.5, FontFamily::Proportional)), (TextStyle::Body, FontId::new(13.5, ..)),
        (TextStyle::Button, FontId::new(13.5, ..)), (TextStyle::Heading, FontId::new(20.0, ..)),
        (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)), (TextStyle::Name("Section".into()), FontId::new(11.0, ..))].into();
    let sp = &mut style.spacing;
    sp.item_spacing = vec2(8.0, 6.0); sp.button_padding = vec2(12.0, 6.0); sp.interact_size = vec2(36.0, 30.0);
    sp.menu_margin = Margin::same(6); sp.window_margin = Margin::same(16);
    sp.slider_rail_height = 6.0; sp.slider_width = 160.0;
    sp.scroll = egui::style::ScrollStyle::floating(); sp.scroll.bar_width = 6.0; sp.scroll.floating_width = 4.0;
    let v = &mut style.visuals;
    v.panel_fill = PANEL; v.window_fill = Color32::from_rgb(19, 26, 47); v.extreme_bg_color = BG; v.faint_bg_color = SURFACE;
    v.hyperlink_color = ACCENT; v.selection.bg_fill = ACCENT.gamma_multiply(0.35); v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.window_corner_radius = CornerRadius::same(14); v.menu_corner_radius = CornerRadius::same(10);
    v.window_shadow = Shadow { offset: [0, 12], blur: 32, spread: 0, color: Color32::from_black_alpha(140) };
    v.popup_shadow  = Shadow { offset: [0, 6],  blur: 20, spread: 0, color: Color32::from_black_alpha(120) };
    v.slider_trailing_fill = true; v.handle_shape = egui::style::HandleShape::Circle;
    let w = &mut v.widgets;   // then per state: bg_fill/weak_bg_fill/bg_stroke/fg_stroke/corner_radius(8)/expansion(0)
    w.inactive.weak_bg_fill = SURFACE; w.hovered.weak_bg_fill = RAISED; w.hovered.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.6));
    w.active.weak_bg_fill = ACCENT.gamma_multiply(0.5); w.active.bg_stroke = Stroke::new(1.0, ACCENT); ...
});
```
Colour helpers (`ecolor-0.37.0/src/color32.rs`): `Color32::from_rgb`(const, :108), `gamma_multiply(f32)` (:269, alpha scale), `lerp_to_gamma(&self, other, t)` (:331), `from_white_alpha`, `from_black_alpha`, `to_normalized_gamma_f32()`. Hex at runtime: `Color32::from_hex(&str)` (`hex_color_runtime.rs:143`).

---------------------------------------------------------------------------------------------------
## 2. Building blocks (0.37 exact usage; all compiled unless noted)
**Frame** (`frame.rs:96`): pub fields `inner_margin, fill, stroke, corner_radius, outer_margin, shadow`; ctors `Frame::NONE`, `Frame::new()`, `Frame::popup(&style)`, `menu`, `window`, `group`, `central_panel`, `canvas`; builders `.inner_margin(impl Into<Margin>) .fill() .stroke(impl Into<Stroke>) .corner_radius() .outer_margin() .shadow(Shadow) .multiply_with_opacity(f32)`; `.show(ui, |ui| ..) -> InnerResponse`.

**Panels**
```rust
egui::Panel::top("titlebar").exact_size(46.0).show_separator_line(false).frame(Frame::NONE).show(ui, |ui| { .. });
egui::Panel::left("sidebar").default_size(290.0).size_range(230.0..=460.0).resizable(true).frame(Frame::NONE.fill(PANEL).inner_margin(Margin::symmetric(12,12))).show(ui, |ui| ..);
egui::CentralPanel::default().frame(Frame::NONE.fill(BG).inner_margin(Margin::same(24))).show(ui, |ui| ..);
```
Panel methods: `resizable, drag_to_open, show_separator_line, default_size, min_size, max_size, size_range(impl Into<Rangef>), exact_size, frame, show, show_collapsible(ui,&mut bool,..)` (collapsible sidebar with slide animation, `panel.rs:454`), `show_animated_inside`, `show_switched` (collapsed⇄expanded) (`panel.rs:325-663`). Panels nest inside any `Ui` (e.g. a rounded root `Frame`), declare top before left before central.

**Hover/selection animation** (`context.rs:3719-3790`): `ctx.animate_bool_with_time(id, bool, secs) -> f32 (0..1)`, `animate_bool_responsive(id,bool)`, `animate_value_with_time(id, f32, secs)`, `animate_bool_with_time_and_easing`. Custom row (compiled `session_row`): `let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 42.0), Sense::click());` then `hov = ctx.animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.12)`; paint `rect_filled(pill, 10, RAISED.gamma_multiply(0.7*hov*(1.0-sel)))`; selected = `painter.rect(pill, 10, ACCENT.gamma_multiply(0.16*sel), Stroke::new(1.0, ACCENT.gamma_multiply(0.45*sel)), StrokeKind::Inside)` + 3px accent bar. Painter: `rect_filled(rect, cr, color)`, `rect(rect, cr, fill, stroke, StrokeKind)`, `circle_filled`, `text(pos, Align2, str, FontId, color)`, `layout_job(LayoutJob)`+`galley(pos, galley, fallback)`. One-line truncation: `LayoutJob::simple_singleline(text, font, color)`; `job.wrap.max_width=..; max_rows=1; break_anywhere=true`. Keep animating UIs awake: `ctx.request_repaint_after(Duration)`.

**Gradients** (compiled `gradient_rect`, `gradient_rounded_h`): `let mut m = epaint::Mesh::default(); m.colored_vertex(pos, color) ×4; m.add_triangle(0,1,2); m.add_triangle(1,3,2); painter.add(egui::Shape::mesh(m));` (`epaint/src/mesh.rs:169,179`). `Mesh::colored_vertex` uses the white-pixel UV so per-vertex colours interpolate linearly (gamma space). Rounded gradient = triangle fan from centre + arc points with colour computed from x (no AA feather on the edge; fine ≥ 1px opaque neighbours/small chips).

**Glow dot**: `Shadow{offset:[0,0],blur:10,spread:2,color: OK.gamma_multiply(0.5)}.as_shape(Rect::from_center_size(c, Vec2::splat(5.0)), CornerRadius::same(3))` pushed via `painter.add(..)` then `circle_filled(c, 3.5, OK)`; pulse via `ctx.input(|i| i.time)`.

**Context menu** (`response.rs:1112`, `menu.rs`):
```rust
resp.context_menu(|ui| {                       // resp must have Sense::click(); right-click opens at pointer
    ui.set_min_width(210.0); menu_pad(ui);
    if ui.add(Button::new((ICON_PLAY_ARROW.rich_text().size(16.0), "Open in tab")).shortcut_text("Enter")).clicked() { ..; ui.close(); }
    ui.separator();
    ui.menu_button((ICON_CONTENT_COPY.rich_text().size(16.0), "Copy to project"), |ui| {   // inside a menu ⇒ SubMenuButton automatically (ui.rs:2922)
        menu_pad(ui); for p in &projects { if ui.button(p).clicked() { ..; ui.close(); } }
    });
});
```
`ui.close()` (`ui.rs:1165`) closes the whole menu. `response.context_menu_opened()` (:1119). Config types: `MenuConfig::new().close_behavior(PopupCloseBehavior).style(impl Into<StyleModifier>).submenu_gap(f32)` (`menu.rs:80-141`), `MenuButton::new(atoms).config(..).ui(ui, f)`, `SubMenuButton::new(atoms)`, `MenuBar::new().ui(ui, f)`; lower level `egui::Popup::context_menu(&resp).style(..).frame(Frame).show(f)` (`popup.rs:255,395,456,547`).

**Modal** (`modal.rs`, compiled):
```rust
let m = egui::Modal::new(Id::unique("delete-modal"))        // Modal::new(id: Id)
    .backdrop_color(Color32::from_black_alpha(150))          // default alpha 100
    .frame(Frame::popup(&ctx.global_style()).fill(RAISED).corner_radius(16).inner_margin(Margin::same(22)))  // default Frame::popup
    .show(ctx, |ui| { ui.set_width(380.0); ...; egui::Sides::new().show(ui, |_| {}, |ui| { /* right_to_left: first = rightmost */ }); });
if m.should_close() { /* backdrop click, ui.close(), or Esc when topmost */ }
```
`ModalResponse { response, backdrop_response, inner, is_top_modal, any_popup_open }`, `.should_close()` (`modal.rs:~115`). Anchored centre, `Order::Foreground`; also `.area(Area)`/`Modal::default_area(id)`.

**Sides** (`sides.rs:45-145`): `egui::Sides::new().height(f32).spacing(f32).shrink_left()/shrink_right()/extend()/truncate()/wrap().show(ui, |ui| left, |ui| right)`; right side is `right_to_left`. Used for title bar, setting rows, dialog footers.

**Atoms / Button** (`atomics/*`, `widgets/button.rs:60-322`): `Button::new(impl IntoAtoms)`, `Button::selectable(bool, atoms)`, builders `.fill(Color32) .stroke() .corner_radius() .min_size(Vec2) .frame(bool) .frame_when_inactive(bool) .small() .truncate() .wrap() .selected(bool) .gap(f32) .shortcut_text(atoms) .right_text(atoms) .left_text(atoms) .sense()`. Atom: `Atom::grow()`, `Atom::custom(id,size)`, `Atom::paint(size, |ui, AtomPaintArgs{rect,..}| ..)` (inline painted dot etc.), `AtomKind::{Empty,Text,Image,Closure,Paint,Widget,Container}`, ext `.atom_size/.atom_grow/.atom_max_size/.atom_align/.atom_id` (`AtomExt`). Tuples of `Into<Atom>` work: `Button::new((icon_richtext, "Label"))`. `Ui::menu_button`, `ComboBox::new(id_salt, label)`, `Checkbox::new(&mut bool, atoms)` also take atoms.

**TextEdit** (`text_edit/builder.rs`): `TextEdit::singleline(&mut s).hint_text(atoms).prefix(atoms).suffix(atoms).margin(Margin).frame(Frame).background_color(c).desired_width(f32::INFINITY).font(..).text_color(..).min_size(..)`; `.frame(Frame::new().fill(SURFACE).corner_radius(10).stroke(..))` works (compiled). Passing a *tuple with an icon* to `hint_text` rendered the hint oddly right-aligned (observed) — use `.prefix(icon)` + `.hint_text("…")` instead.

**Other widgets** (all compiled): `egui::Slider::new(&mut f32, 9.0..=24.0).suffix(" pt").step_by(0.5)` (+`.text .logarithmic .handle_shape .trailing_fill .show_value`), `egui::DragValue::new(&mut v).suffix(" px").range(120.0..=320.0).speed()`, `egui::ComboBox::from_id_salt("x").selected_text(..).width(140.0).show_ui(ui, |ui| ui.selectable_value(&mut sel, i, "name"))` (+`.popup_style(StyleModifier)`, `.icon(fn)`, `.close_behavior`), `egui::ProgressBar::new(0.6).animate(true).desired_width().corner_radius().fill(c).show_percentage()`, `egui::Spinner::new().size(16.0).color(c)`.

**Toggle switch** (compiled `toggle`): `let (rect, mut resp) = ui.allocate_exact_size(vec2(40.0,22.0), Sense::click()); if resp.clicked() { *on = !*on; resp.mark_changed(); } let t = ctx.animate_bool_with_time(resp.id, *on, 0.15);` track `rect(rect, 11, SURFACE.lerp_to_gamma(ACCENT, t), Stroke, StrokeKind::Inside)`, knob `circle_filled(pos2(lerp(left+11..=right-11, t), cy), 8.0, WHITE)`; `resp.on_hover_cursor(CursorIcon::PointingHand)`. For accessibility add `resp.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, ui.is_enabled(), *on, ""))`.

**CollapsingState group header with animated chevron** (compiled in `sidebar`):
```rust
let pid = ui.make_persistent_id(("proj", pi));
let mut state = CollapsingState::load_with_default_open(&ctx, pid, true);   // egui::collapsing_header::CollapsingState
let open_t = state.openness(&ctx);                                         // 0..1 animated  (collapsing_header.rs:74)
let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
// paint chevron polyline rotated by open_t*90°, folder icon, name, count …
if resp.clicked() { state.toggle(ui); }
state.show_body_unindented(ui, |ui| { /* rows */ });                       // animated reveal + stores state (:175)
```
Built-in alternatives: `state.show_header(ui, |ui| ..).body(|ui| ..)` (:129), `CollapsingHeader::new(text).default_open(true).show_background(false).icon(|ui, openness, &resp| ..)` (:392-516).

**Tooltips**: `resp.on_hover_text(..)`, `.on_hover_ui(|ui| ..)`; timing in `style.interaction.{tooltip_delay=0.5, tooltip_grace_time, show_tooltips_only_when_still}`; frame follows `Frame::popup`. `egui::Tooltip::for_widget(&resp).show(f)` (`tooltip.rs:40`).

---------------------------------------------------------------------------------------------------
## 3. Icons — crates.io sparse-index results (`curl -s https://index.crates.io/<path>`; latest non-yanked line)
| crate | latest | egui dep | verdict |
|---|---|---|---|
| **egui_material_icons** | **0.9.0** | `egui ^0.37.0` (verified in `egui_material_icons-0.9.0/Cargo.toml`) | ✅ use this |
| egui-phosphor | 0.14.0 | `egui ^0.36` | ✗ (0.37 not yet) |
| egui-remixicon | 0.34.0 | `egui ^0.34` | ✗ |
| egui-lucide | 0.1.0 | `egui ^0.36`, `egui_extras ^0.36` | ✗ |
| egui_phosphor / lucide-egui / egui_fontawesome / egui_remixicon / egui-material-icons | not found in index | | – |
| catppuccin-egui | 5.7.0 | egui 0.26–0.33 | ✗ (not needed) |

**Recommendation: `egui_material_icons = "0.9"`** (MIT crate; bundles *Material Symbols Rounded* TTF: filled 1.83 MB, outline 1.22 MB; font licence = Apache-2.0 upstream **[UNVERIFIED – check the crate repo before shipping]**). Features: default `filled`; `outline`; `compressed` (include-flate). Prefer `default-features = false, features = ["outline"]` for a lighter modern line look (`.filled()` unavailable then) or `["filled","outline"]` for both.
```rust
// AFTER deck_term::install_fonts (which calls ctx.set_fonts and would wipe icons otherwise):
egui_material_icons::initialize(&cc.egui_ctx);          // lib.rs: ctx.add_font(font_insert()) — registers family Name("material-icons") + Lowest-priority in Proportional
use egui_material_icons::icons::*;                      // ICON_FOLDER, ICON_CLOSE, ICON_SETTINGS, ICON_SEARCH, ICON_ADD, ICON_CONTENT_COPY, ICON_DRIVE_FILE_MOVE, ICON_DELETE, ICON_EDIT, ICON_SYSTEM_UPDATE_ALT, ICON_CHECK_CIRCLE, ICON_PLAY_ARROW, ICON_TERMINAL, ICON_CHEVRON_RIGHT, ICON_EXPAND_MORE, ICON_MINIMIZE, ICON_CROP_SQUARE, ICON_FILTER_NONE, ICON_OPEN_IN_NEW, ICON_REFRESH, ICON_CHECK (all verified present in icons.rs)
ui.button(ICON_ADD);                                    // MaterialIcon: Into<WidgetText>/Into<Atom>
ui.add(Button::new((ICON_FOLDER.rich_text().size(16.0), "Open")));   // sized/coloured: .rich_text() -> RichText
painter.text(pos, Align2::CENTER_CENTER, ICON_CLOSE.codepoint, FontId::new(16.0, ICON_CLOSE.font_family()), color);  // codepoint: &'static str
```
`MaterialIcon { codepoint: &'static str, style }`, `.outlined()/.filled()`, `.font_family()`, `.rich_text()` (`lib.rs:~60-120`). Fallback if the crate is rejected: bundle the Phosphor/Lucide TTF yourself (`FontData::from_static(include_bytes!(..))` + `ctx.add_font(FontInsert::new(name, data, vec![InsertFontFamily{family: FontFamily::Name("icons".into()), priority: FontPriority::Highest}]))`). **No icon font is installed on this box** (`fc-list | grep -i phosphor|material|lucide|awesome|nerd` → nothing; only Noto Sans Symbols/Symbols2, Symbola, DejaVu).

---------------------------------------------------------------------------------------------------
## 4. UI fonts on this box (`fc-list : family file`, observed)
* **Not installed**: Inter, Geist, Roboto, JetBrains Sans. 
* **Adwaita Sans** `/usr/share/fonts/adwaita-sans-fonts/AdwaitaSans-Regular.ttf` (+`AdwaitaSans-Italic.ttf`) — **variable**: axes (printed by `FontData::variation_axes()` in the demo) `opsz 14–32 (def 14)`, `wght 100–900 (def 400)`. Inter-derived design (GNOME's Inter fork) **[from memory, UNVERIFIED]**. ← **recommended Proportional face**; bold/semibold via `RichText::variation("wght", 650.0)` (`widget_text.rs:200`; compiled and visibly heavier in the screenshot).
* Cantarell: `/usr/share/fonts/abattis-cantarell-fonts/Cantarell-{Thin,Light,Regular,Bold,ExtraBold}.otf`, variable `/usr/share/fonts/abattis-cantarell-vf-fonts/Cantarell-VF.otf`.
* Noto Sans (the `fc-match sans-serif` default): `/usr/share/fonts/google-noto/NotoSans-{Thin..Black}.ttf` (+Condensed variants); also Open Sans `/usr/share/fonts/open-sans/OpenSans-*.ttf`, Red Hat Display `/usr/share/fonts/redhat/RedHatDisplay-*.otf`, Liberation Sans, DejaVu Sans, Droid Sans, Carlito.
* **Wiring while keeping egui's fallbacks (emoji, Ubuntu-Light, Hack, NotoEmoji)** — `deck_term::install_fonts` already starts from `FontDefinitions::default()` and appends fallbacks to the UI families (`crates/deck-term/src/fonts.rs:126`), so just *prepend* the UI font without another `set_fonts`:
```rust
use egui::{FontData, FontInsert, InsertFontFamily, FontPriority, FontFamily};
if let Ok(bytes) = std::fs::read("/usr/share/fonts/adwaita-sans-fonts/AdwaitaSans-Regular.ttf") {   // or from_blob(memmap) as fonts.rs does
    ctx.add_font(FontInsert::new("ui-sans", FontData::from_owned(bytes),
        vec![InsertFontFamily { family: FontFamily::Proportional, priority: FontPriority::Highest }]));
}
```
  (demo used the equivalent `fd.families.get_mut(&Proportional).unwrap().insert(0, "ui-sans")` on `FontDefinitions::default()`; both put it first, emoji/other fallbacks stay behind.) Resolve the path at runtime (`fc-match 'Adwaita Sans'` → fall back to `Noto Sans`, then egui default). `add_font` ignores an already-existing name (`context.rs:2532`). Terminal grid keeps its own `Name("term")` families — untouched.

---------------------------------------------------------------------------------------------------
## 5. Toast / spinner (compiled `show_toasts`)
```rust
struct Toast { text: String, born: f64, id: u64 }
fn show_toasts(ctx: &egui::Context, toasts: &mut Vec<Toast>) {
    let now = ctx.input(|i| i.time);
    toasts.retain(|t| now - t.born < 3.5);
    for (i, t) in toasts.iter().rev().enumerate() {                       // newest at the bottom
        let age = (now - t.born) as f32;
        let a = (age / 0.2).min(1.0).min(((3.5 - age) / 0.4).clamp(0.0, 1.0));   // fade in/out
        egui::Area::new(Id::unique(("toast", t.id))).order(Order::Foreground).interactable(false)
            .anchor(Align2::RIGHT_BOTTOM, vec2(-20.0, -20.0 - i as f32 * 54.0 + (1.0 - a) * 10.0))
            .show(ctx, |ui| { ui.set_opacity(a);
                Frame::popup(ui.style()).fill(RAISED).corner_radius(12).inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
                    ui.horizontal(|ui| { ui.label(ICON_CHECK_CIRCLE.rich_text().size(18.0).color(OK)); ui.label(RichText::new(&t.text).color(TEXT)); }); }); });
    }
    if !toasts.is_empty() { ctx.request_repaint_after(Duration::from_millis(16)); }
}
```
Call at the end of `App::ui` with `ui.ctx()`. `Area::{anchor, order, interactable, fixed_pos, fade_in, constrain}` (`area.rs:251-384`), `Area::show(self, ctx, f)` (`:441`). Spinner/progress: `egui::Spinner::new().size(16.0).color(ACCENT)` (`spinner.rs:18-38`; animates from `ctx.input(|i| i.time)` — [UNVERIFIED] whether it requests its own repaint; call `ctx.request_repaint()` while visible), `ProgressBar::new(f).animate(true)`.
**Observed**: first toast click right after closing a modal did not show a toast in one run (my click sequence); a second click showed it correctly — not investigated, probably the modal-closing click being consumed by backdrop.

---------------------------------------------------------------------------------------------------
## 6. Window chrome (client-side decorations)
Works at the API level; **Wayland/KWin behaviour UNVERIFIED** (no compositor on Xvfb; the real session is `wayland`/KDE, I did not open windows on it).
* `ViewportBuilder::default().with_decorations(false).with_transparent(bool).with_resizable(true).with_min_inner_size(..)` (`viewport.rs:372,406,430,550`). Also `with_title_shown`, `with_fullsize_content_view` (macOS only).
* Commands via `ctx.send_viewport_cmd(ViewportCommand::X)`: `StartDrag` (`viewport.rs:1108`), `BeginResize(ResizeDirection::{North,South,East,West,NorthEast,NorthWest,SouthEast,SouthWest})` (:1129,1060), `Minimized(bool)` (:1140), `Maximized(bool)` (:1143), `Fullscreen(bool)`, `Close` (:1090). State: `ctx.input(|i| i.viewport().maximized.unwrap_or(false))` (compiled).
* egui-winit mapping: `StartDrag` → `window.drag_window()` only if `window.has_focus()` (`egui-winit-0.37.0/src/lib.rs:1961`); `BeginResize` → `window.drag_resize_window(dir)` (:1995). winit-wayland: `drag_window` = `xdg_toplevel._move(seat, serial)` using the latest button serial (`winit-0.30.13/src/platform_impl/linux/wayland/window/state.rs:446-455`) and `set_decorate(false)` switches to `DecorationMode::Client` and hides the sctk frame (`:983-1000`) ⇒ on KWin you get *no* titlebar/border/shadow/resize edge at all; you must draw them. `xdg_toplevel.move/resize` are standard protocol so KWin should honour them [UNVERIFIED]. The serial must come from a real button press ⇒ trigger `StartDrag` on `drag_started_by(Primary)` (compiled) not on hover.
* Compiled pattern (see demo `title_bar`, `win_btn`, `resize_handles`): title bar = `Panel::top(..).frame(Frame::NONE)`; `let drag = ui.interact(rect, ui.scope_id().with("drag"), Sense::click_and_drag()); if drag.drag_started_by(PointerButton::Primary) { ctx.send_viewport_cmd(StartDrag) } if drag.double_clicked() { Maximized(!maximized) }`; buttons added *after* `interact` win hit-testing. Window buttons = 36×28 custom-painted `allocate_exact_size(Sense::click())` with animated hover fill (red for close). Right group via `Sides::new().height(h).show(ui, left, right)`; in the right closure items are added right→left (close first).
* Resize: egui has no native edge handles when undecorated. Demo polls pointer within a 5 px inner border of `ctx.content_rect()`: `ctx.set_cursor_icon(CursorIcon::ResizeNorthWest…)` and on `i.pointer.primary_pressed()` sends `BeginResize(dir)` (compiled, not exercised on a real WM). Caveat: widgets at the very edge also receive the click; keep a ≥5 px margin or use the transparent-frame trick below.
* **Transparent + rounded corners**: `with_transparent(true)` + `App::clear_color(&self, _v) -> [f32;4]` returning `Color32::TRANSPARENT.to_normalized_gamma_f32()` (`epi.rs:251`, default is (12,12,12,180)) and a root `Frame::NONE.fill(BG).corner_radius(14).stroke(..)` with each child panel rounding only its outer corner (`CornerRadius{nw,ne,sw,se}`) — compiled; demo flag `DEMO_TRANSPARENT=1`. Observed on Xvfb: window creates with no `Cannot create transparent window` error (that error path is `glow_integration.rs:206-210`) and the rounded corner geometry renders correctly (zoomed pixel crop); the corner's actual alpha can't be judged without a compositor [UNVERIFIED on KWin; KWin has a compositor so it should composite]. Square when maximized: demo sets radius 0 if `maximized`. Drop shadow around a CSD window would need an extra transparent margin (≈20 px) inside the root frame + `Shadow::as_shape` and then input-region handling; not implemented — a plain 1 px stroke is used.
* **Recommendation**: keep KWin server-side decorations (simplest, native snapping/tiling/shadows) for now; CSD is feasible but costs: no native shadow, no window-edge resize outside content, no KWin title-bar gestures/tiling hints. If CSD is wanted, ship it behind a config flag and keep `with_decorations(true)` default.

---------------------------------------------------------------------------------------------------
## 7. Demo — how it was built/run (for re-verification)
* Source `/tmp/omp-deck-notes/egui-style-demo.rs`; Cargo.toml `/tmp/omp-deck-notes/egui-style-demo.Cargo.toml`.
* **/tmp is a quota-limited tmpfs (6121M; was full)**: build failed with `Disk quota exceeded`. Use `CARGO_TARGET_DIR=$HOME/.cache/omp-deck-demo-target CARGO_PROFILE_DEV_DEBUG=0 cargo build` (compile ~2 min cold). 
* Run: `setsid Xvfb :89 -screen 0 1400x900x24 -nolisten tcp` then `DISPLAY=:89 WAYLAND_DISPLAY= LIBGL_ALWAYS_SOFTWARE=1 setsid ./omp-deck-demo` (use plain `cmd &`, not `(cmd &)` — the subshell form died silently in this tool), `import -window root -display :89 shot.png`. Input was driven with a ctypes XTEST script (libX11/libXtst `XTestFakeMotionEvent/ButtonEvent`; no xdotool installed): right-click row → menu → hover "Copy to project" → submenu (screenshot), click "Delete session…" → modal, click "Show toast" → toast.
* Observed working (screenshot + clicks): custom dark theme, gradient title bar, collapsible project groups with animated chevron, hover/selected pill rows, pulsing glow dot, right-click menu with icons/shortcut text/separators/submenus, modal with backdrop, toggle switches, slider (circle handle + trailing fill), DragValue, ComboBox, ProgressBar, Spinner, toasts with fade, Adwaita Sans UI font + variable-weight bold.
* Known visual nits left in the demo: progress-bar track is darker than card (it uses `extreme_bg_color`); title-bar rounded gradient uses a hack second rect for the lower half; `Slider`'s DragValue box sits left of the rail (`Slider::show_value(false)` if unwanted).
