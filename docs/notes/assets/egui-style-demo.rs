use egui::{
    Align2, Button, Color32, CornerRadius, CursorIcon, FontData, FontDefinitions, FontFamily,
    FontId, Frame, Id, Margin, Order, PointerButton, Rect, RichText, Sense, Shadow, Stroke,
    StrokeKind, TextStyle, Theme, ThemePreference, Ui, UiBuilder, Vec2, ViewportCommand, vec2, pos2,
};
use egui::collapsing_header::CollapsingState;
use egui::epaint::{Mesh, text::LayoutJob};
use egui_material_icons::icons::*;
use std::sync::Arc;

// ───────────────────────── palette ─────────────────────────
const BG: Color32 = Color32::from_rgb(10, 14, 26); // window / deepest
const PANEL: Color32 = Color32::from_rgb(15, 20, 36); // sidebar
const SURFACE: Color32 = Color32::from_rgb(22, 29, 51); // cards, inputs
const RAISED: Color32 = Color32::from_rgb(30, 39, 68); // hover / popups
const STROKE: Color32 = Color32::from_rgb(38, 48, 80);
const TEXT: Color32 = Color32::from_rgb(214, 222, 240);
const TEXT_WEAK: Color32 = Color32::from_rgb(124, 135, 168);
const ACCENT: Color32 = Color32::from_rgb(108, 124, 255);
const OK: Color32 = Color32::from_rgb(52, 211, 153);
const DANGER: Color32 = Color32::from_rgb(248, 113, 113);

fn apply_theme(ctx: &egui::Context) {
    ctx.set_theme(ThemePreference::Dark);
    ctx.style_mut_of(Theme::Dark, |style| {
        // ---- text styles
        style.text_styles = [
            (TextStyle::Small, FontId::new(11.5, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(13.5, FontFamily::Proportional)),
            (TextStyle::Heading, FontId::new(20.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
            (TextStyle::Name("Section".into()), FontId::new(11.0, FontFamily::Proportional)),
        ]
        .into();
        // ---- spacing
        let sp = &mut style.spacing;
        sp.item_spacing = vec2(8.0, 6.0);
        sp.button_padding = vec2(12.0, 6.0);
        sp.interact_size = vec2(36.0, 30.0);
        sp.menu_margin = Margin::same(6);
        sp.window_margin = Margin::same(16);
        sp.menu_width = 200.0;
        sp.slider_rail_height = 6.0;
        sp.slider_width = 160.0;
        sp.scroll = egui::style::ScrollStyle::floating();
        sp.scroll.bar_width = 6.0;
        sp.scroll.floating_width = 4.0;
        // ---- visuals
        let v = &mut style.visuals;
        v.dark_mode = true;
        v.panel_fill = PANEL;
        v.window_fill = Color32::from_rgb(19, 26, 47); // popups/menus/modals; hover (RAISED) must differ from it
        v.extreme_bg_color = BG; // TextEdit / slider rail background
        v.faint_bg_color = SURFACE;
        v.code_bg_color = SURFACE;
        v.hyperlink_color = ACCENT;
        v.warn_fg_color = Color32::from_rgb(251, 191, 36);
        v.error_fg_color = DANGER;
        v.selection.bg_fill = ACCENT.gamma_multiply(0.35);
        v.selection.stroke = Stroke::new(1.0, ACCENT);
        v.slider_trailing_fill = true;
        v.handle_shape = egui::style::HandleShape::Circle;
        v.window_corner_radius = CornerRadius::same(14);
        v.menu_corner_radius = CornerRadius::same(10);
        v.window_stroke = Stroke::new(1.0, STROKE);
        v.window_shadow = Shadow { offset: [0, 12], blur: 32, spread: 0, color: Color32::from_black_alpha(140) };
        v.popup_shadow = Shadow { offset: [0, 6], blur: 20, spread: 0, color: Color32::from_black_alpha(120) };
        let r = CornerRadius::same(8);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = PANEL;
        w.noninteractive.weak_bg_fill = PANEL;
        w.noninteractive.bg_stroke = Stroke::new(1.0, STROKE);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        w.noninteractive.corner_radius = r;
        w.inactive.bg_fill = SURFACE;
        w.inactive.weak_bg_fill = SURFACE;
        w.inactive.bg_stroke = Stroke::new(1.0, STROKE);
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        w.inactive.corner_radius = r;
        w.hovered.bg_fill = RAISED;
        w.hovered.weak_bg_fill = RAISED;
        w.hovered.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.6));
        w.hovered.fg_stroke = Stroke::new(1.5, Color32::WHITE);
        w.hovered.corner_radius = r;
        w.hovered.expansion = 0.0;
        w.active.bg_fill = ACCENT.gamma_multiply(0.5);
        w.active.weak_bg_fill = ACCENT.gamma_multiply(0.5);
        w.active.bg_stroke = Stroke::new(1.0, ACCENT);
        w.active.fg_stroke = Stroke::new(1.5, Color32::WHITE);
        w.active.corner_radius = r;
        w.active.expansion = 0.0;
        w.open.bg_fill = RAISED;
        w.open.weak_bg_fill = RAISED;
        w.open.bg_stroke = Stroke::new(1.0, STROKE);
        w.open.fg_stroke = Stroke::new(1.0, TEXT);
        w.open.corner_radius = r;
    });
}

fn install_fonts(ctx: &egui::Context) {
    let mut fd = FontDefinitions::default(); // keeps Hack/Ubuntu/NotoEmoji/emoji-icon-font fallbacks
    let p = "/usr/share/fonts/adwaita-sans-fonts/AdwaitaSans-Regular.ttf";
    if let Ok(bytes) = std::fs::read(p) {
        let data = FontData::from_owned(bytes);
        eprintln!("AdwaitaSans axes: {:?}", data.variation_axes());
        fd.font_data.insert("ui-sans".into(), Arc::new(data));
        fd.families.get_mut(&FontFamily::Proportional).unwrap().insert(0, "ui-sans".into());
    }
    ctx.set_fonts(fd);
    egui_material_icons::initialize(ctx); // AFTER set_fonts (uses add_font)
}

// ───────────────────────── drawing helpers ─────────────────────────
/// Axis-aligned 2-colour gradient (horizontal if `horizontal`).
fn gradient_rect(painter: &egui::Painter, rect: Rect, a: Color32, b: Color32, horizontal: bool) {
    let mut m = Mesh::default();
    let (c_tl, c_tr, c_bl, c_br) = if horizontal { (a, b, a, b) } else { (a, a, b, b) };
    m.colored_vertex(rect.left_top(), c_tl);
    m.colored_vertex(rect.right_top(), c_tr);
    m.colored_vertex(rect.left_bottom(), c_bl);
    m.colored_vertex(rect.right_bottom(), c_br);
    m.add_triangle(0, 1, 2);
    m.add_triangle(1, 3, 2);
    painter.add(egui::Shape::mesh(m));
}

/// Rounded-rect with horizontal gradient: triangle fan around the centre, per-vertex colour by x.
fn gradient_rounded_h(painter: &egui::Painter, rect: Rect, radius: f32, a: Color32, b: Color32) {
    let radius = radius.min(rect.height() / 2.0).min(rect.width() / 2.0);
    let col = |x: f32| a.lerp_to_gamma(b, ((x - rect.min.x) / rect.width()).clamp(0.0, 1.0));
    let mut m = Mesh::default();
    m.colored_vertex(rect.center(), col(rect.center().x));
    let corners = [
        (rect.right_top() + vec2(-radius, radius), -90.0f32),
        (rect.right_bottom() + vec2(-radius, -radius), 0.0),
        (rect.left_bottom() + vec2(radius, -radius), 90.0),
        (rect.left_top() + vec2(radius, radius), 180.0),
    ];
    const STEPS: usize = 8;
    for (c, start) in corners {
        for i in 0..=STEPS {
            let ang = (start + 90.0 * i as f32 / STEPS as f32).to_radians();
            let p = c + radius * vec2(ang.cos(), ang.sin());
            m.colored_vertex(p, col(p.x));
        }
    }
    let n = (m.vertices.len() - 1) as u32;
    for i in 1..=n {
        m.add_triangle(0, i, if i == n { 1 } else { i + 1 });
    }
    painter.add(egui::Shape::mesh(m));
}

fn glow_dot(painter: &egui::Painter, center: egui::Pos2, color: Color32, pulse: f32) {
    let glow = Shadow {
        offset: [0, 0],
        blur: (8.0 + 6.0 * pulse) as u8,
        spread: 2,
        color: color.gamma_multiply(0.35 + 0.25 * pulse),
    };
    let r = Rect::from_center_size(center, Vec2::splat(5.0));
    painter.add(glow.as_shape(r, CornerRadius::same(3)));
    painter.circle_filled(center, 3.5, color);
}

fn toggle(ui: &mut Ui, on: &mut bool) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(40.0, 22.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_with_time(resp.id, *on, 0.15);
    let hov = ui.ctx().animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.12);
    let track = SURFACE.lerp_to_gamma(RAISED, hov).lerp_to_gamma(ACCENT, t);
    let p = ui.painter();
    p.rect(rect, CornerRadius::same(11), track, Stroke::new(1.0, STROKE.lerp_to_gamma(ACCENT, t)), StrokeKind::Inside);
    let cx = egui::lerp((rect.left() + 11.0)..=(rect.right() - 11.0), t);
    p.circle_filled(pos2(cx, rect.center().y + 1.0), 8.0, Color32::from_black_alpha(60));
    p.circle_filled(pos2(cx, rect.center().y), 8.0, Color32::WHITE.lerp_to_gamma(Color32::WHITE, t));
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

fn section_label(ui: &mut Ui, text: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(text.to_uppercase()).text_style(TextStyle::Name("Section".into())).color(TEXT_WEAK).variation("wght", 700.0));
}

// ───────────────────────── app ─────────────────────────
struct Sess { title: &'static str, age: &'static str, running: bool }
struct Project { name: &'static str, path: &'static str, sessions: Vec<Sess> }

enum Modal { Delete { title: String }, None }

struct Toast { text: String, born: f64, id: u64 }

struct Demo {
    projects: Vec<Project>,
    selected: (usize, usize),
    search: String,
    modal: Modal,
    toasts: Vec<Toast>,
    next_toast: u64,
    auto_update: bool,
    notify: bool,
    font_size: f32,
    theme_sel: usize,
    transparent: bool,
}

impl Demo {
    fn new(cc: &eframe::CreationContext<'_>, transparent: bool) -> Self {
        install_fonts(&cc.egui_ctx);
        apply_theme(&cc.egui_ctx);
        let s = |title, age, running| Sess { title, age, running };
        Self {
            projects: vec![
                Project { name: "OmpDeck", path: "~/PersProject/OmpDeck", sessions: vec![
                    s("Redesign sidebar chrome", "2m", true), s("tmux key forwarding", "1h", true), s("Fix OSC passthrough", "yesterday", false)] },
                Project { name: "oh-my-pi", path: "~/PersProject/oh-my-pi", sessions: vec![
                    s("Read settings schema", "3h", false), s("Update flow investigation", "2d", false)] },
                Project { name: "dotfiles", path: "~/.config", sessions: vec![s("Kitty config tweak", "5d", false)] },
            ],
            selected: (0, 0), search: String::new(), modal: Modal::None, toasts: vec![], next_toast: 0,
            auto_update: true, notify: false, font_size: 14.0, theme_sel: 0, transparent,
        }
    }
    fn toast(&mut self, ctx: &egui::Context, text: impl Into<String>) {
        self.next_toast += 1;
        self.toasts.push(Toast { text: text.into(), born: ctx.input(|i| i.time), id: self.next_toast });
    }
}

impl eframe::App for Demo {
    fn clear_color(&self, _v: &egui::Visuals) -> [f32; 4] {
        if self.transparent { Color32::TRANSPARENT.to_normalized_gamma_f32() } else { BG.to_normalized_gamma_f32() }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        let rounded = self.transparent && !maximized;
        let r: u8 = if rounded { 14 } else { 0 };

        // Root frame: rounded window background + 1px border (CSD).
        Frame::NONE
            .fill(BG)
            .corner_radius(r)
            .stroke(if rounded { Stroke::new(1.0, STROKE) } else { Stroke::NONE })
            .show(ui, |ui| {
                self.title_bar(ui, r, maximized);
                self.sidebar(ui, r);
                egui::CentralPanel::default()
                    .frame(Frame::NONE.fill(BG).inner_margin(Margin::same(24)).corner_radius(CornerRadius { nw: 0, ne: 0, sw: 0, se: r }))
                    .show(ui, |ui| self.content(ui));
            });

        self.modal_ui(&ctx);
        show_toasts(&ctx, &mut self.toasts);
        if !maximized && !self.transparent_off() { resize_handles(&ctx); }
    }
}

impl Demo {
    fn transparent_off(&self) -> bool { false }

    fn title_bar(&mut self, ui: &mut Ui, r: u8, maximized: bool) {
        let ctx = ui.ctx().clone();
        egui::Panel::top("titlebar").exact_size(46.0).show_separator_line(false).frame(Frame::NONE).show(ui, |ui| {
            let rect = ui.max_rect();
            // gradient background, clipped to rounded top corners
            if r > 0 {
                gradient_rounded_h(ui.painter(), rect.expand2(vec2(0.0, 0.0)), r as f32, Color32::from_rgb(24, 30, 66), Color32::from_rgb(14, 19, 38));
                ui.painter().rect_filled(Rect::from_min_max(pos2(rect.min.x, rect.center().y), rect.max), 0.0, Color32::from_rgb(19, 25, 52));
            } else {
                gradient_rect(ui.painter(), rect, Color32::from_rgb(26, 32, 72), Color32::from_rgb(13, 18, 36), true);
            }
            ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, STROKE));
            // drag area: whole bar (widgets added later win hit-testing)
            let drag = ui.interact(rect, ui.scope_id().with("drag"), Sense::click_and_drag());
            if drag.drag_started_by(PointerButton::Primary) { ctx.send_viewport_cmd(ViewportCommand::StartDrag); }
            if drag.double_clicked() { ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized)); }

            ui.scope_builder(UiBuilder::new().max_rect(rect.shrink2(vec2(14.0, 0.0))), |ui| {
                egui::Sides::new().height(rect.height()).show(ui,
                    |ui| {
                        // logo chip
                        let (lr, _) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
                        gradient_rounded_h(ui.painter(), lr, 7.0, ACCENT, Color32::from_rgb(168, 85, 247));
                        ui.painter().text(lr.center(), Align2::CENTER_CENTER, ICON_TERMINAL.codepoint, FontId::new(15.0, ICON_TERMINAL.font_family()), Color32::WHITE);
                        ui.label(RichText::new("OmpDeck").size(15.0).variation("wght", 650.0).color(Color32::WHITE));
                        ui.label(RichText::new("·  OmpDeck / Redesign sidebar chrome").color(TEXT_WEAK));
                    },
                    |ui| {
                        // right-to-left: first added is rightmost
                        if win_btn(ui, ICON_CLOSE.codepoint, true).clicked() { ctx.send_viewport_cmd(ViewportCommand::Close); }
                        let icon = if maximized { ICON_FILTER_NONE } else { ICON_CROP_SQUARE };
                        if win_btn(ui, icon.codepoint, false).clicked() { ctx.send_viewport_cmd(ViewportCommand::Maximized(!maximized)); }
                        if win_btn(ui, ICON_MINIMIZE.codepoint, false).clicked() { ctx.send_viewport_cmd(ViewportCommand::Minimized(true)); }
                        ui.add_space(8.0);
                        // primary action chip
                        if ui.add(Button::new((ICON_SYSTEM_UPDATE_ALT.rich_text().size(16.0), "Update")).fill(ACCENT.gamma_multiply(0.25)).stroke(Stroke::new(1.0, ACCENT.gamma_multiply(0.7))).corner_radius(8)).clicked() {
                            self.toast(&ctx, "omp 18.12.3 is up to date");
                        }
                    });
            });
        });
    }

    fn sidebar(&mut self, ui: &mut Ui, r: u8) {
        let ctx = ui.ctx().clone();
        egui::Panel::left("sidebar").default_size(290.0).size_range(230.0..=460.0).resizable(true)
            .frame(Frame::NONE.fill(PANEL).inner_margin(Margin::symmetric(12, 12)).corner_radius(CornerRadius { nw: 0, ne: 0, sw: r, se: 0 }))
            .show(ui, |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.search)
                    .prefix(ICON_SEARCH.rich_text().size(16.0).color(TEXT_WEAK))
                    .hint_text("Search sessions…")
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(10, 8))
                    .frame(Frame::new().fill(SURFACE).corner_radius(10).stroke(Stroke::new(1.0, STROKE))));
                ui.add_space(6.0);
                let names: Vec<&str> = self.projects.iter().map(|p| p.name).collect();
                let mut action: Option<(String, usize, usize)> = None;
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for pi in 0..self.projects.len() {
                        let pid = ui.make_persistent_id(("proj", pi));
                        let mut state = CollapsingState::load_with_default_open(&ctx, pid, true);
                        let open_t = state.openness(&ctx);
                        // header
                        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
                        let hov = ctx.animate_bool_with_time(pid.with("hov"), resp.hovered(), 0.1);
                        ui.painter().rect_filled(rect, 8, RAISED.gamma_multiply(0.6 * hov));
                        let c = pos2(rect.left() + 12.0, rect.center().y);
                        let a = open_t * std::f32::consts::FRAC_PI_2; // 0 = pointing right, 90° = down
                        let rot = |x: f32, y: f32| c + vec2(x * a.cos() - y * a.sin(), x * a.sin() + y * a.cos());
                        ui.painter().add(egui::Shape::line(vec![rot(-2.5, -4.0), rot(2.5, 0.0), rot(-2.5, 4.0)], Stroke::new(1.6, TEXT_WEAK.lerp_to_gamma(TEXT, hov))));
                        let p = &self.projects[pi];
                        ui.painter().text(pos2(rect.left() + 26.0, rect.center().y), Align2::LEFT_CENTER, ICON_FOLDER.codepoint, FontId::new(16.0, ICON_FOLDER.font_family()), ACCENT);
                        ui.painter().text(pos2(rect.left() + 48.0, rect.center().y), Align2::LEFT_CENTER, p.name, FontId::proportional(13.5), TEXT);
                        let n = p.sessions.len().to_string();
                        ui.painter().text(pos2(rect.right() - 10.0, rect.center().y), Align2::RIGHT_CENTER, n, FontId::proportional(11.5), TEXT_WEAK);
                        if resp.clicked() { state.toggle(ui); }
                        resp.on_hover_text(p.path);
                        state.show_body_unindented(ui, |ui| {
                            for si in 0..self.projects[pi].sessions.len() {
                                let sel = self.selected == (pi, si);
                                let resp = session_row(ui, &self.projects[pi].sessions[si], sel);
                                if resp.clicked() { self.selected = (pi, si); }
                                resp.context_menu(|ui| {
                                    ui.set_min_width(210.0);
                                    menu_pad(ui);
                                    if ui.add(Button::new((ICON_PLAY_ARROW.rich_text().size(16.0), "Open in tab")).shortcut_text("Enter")).clicked() { action = Some(("open".into(), pi, si)); ui.close(); }
                                    if ui.add(Button::new((ICON_EDIT.rich_text().size(16.0), "Rename…")).shortcut_text("F2")).clicked() { action = Some(("rename".into(), pi, si)); ui.close(); }
                                    ui.separator();
                                    ui.menu_button((ICON_CONTENT_COPY.rich_text().size(16.0), "Copy to project"), |ui| {
                                        menu_pad(ui);
                                        for (ti, name) in names.iter().enumerate() {
                                            if ui.button(*name).clicked() { action = Some((format!("copy:{ti}"), pi, si)); ui.close(); }
                                        }
                                        ui.separator();
                                        if ui.button("Choose folder…").clicked() { action = Some(("copy:dir".into(), pi, si)); ui.close(); }
                                    });
                                    ui.menu_button((ICON_DRIVE_FILE_MOVE.rich_text().size(16.0), "Move to project"), |ui| {
                                        menu_pad(ui);
                                        for (ti, name) in names.iter().enumerate().filter(|(ti, _)| *ti != pi) {
                                            if ui.button(*name).clicked() { action = Some((format!("move:{ti}"), pi, si)); ui.close(); }
                                        }
                                    });
                                    ui.separator();
                                    if ui.add(Button::new((ICON_DELETE.rich_text().size(16.0).color(DANGER), RichText::new("Delete").color(DANGER)))).clicked() { action = Some(("delete".into(), pi, si)); ui.close(); }
                                });
                            }
                        });
                        ui.add_space(4.0);
                    }
                });
                if let Some((a, pi, si)) = action {
                    let title = self.projects[pi].sessions[si].title;
                    if a == "delete" { self.modal = Modal::Delete { title: title.to_string() }; }
                    else { self.toast(&ctx, format!("{a}: “{title}”")); }
                }
            });
    }

    fn content(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        ui.label(RichText::new("Settings").heading().color(Color32::WHITE).variation("wght", 700.0));
        ui.label(RichText::new("Appearance, updates and behaviour of OmpDeck").color(TEXT_WEAK));
        ui.add_space(14.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            card(ui, "Updates", |ui| {
                row(ui, "Check for updates automatically", "Runs `omp update --check` on startup", |ui| { toggle(ui, &mut self.auto_update); });
                row(ui, "Notifications", "Toast when a session finishes", |ui| { toggle(ui, &mut self.notify); });
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(16.0).color(ACCENT));
                    ui.label(RichText::new("Checking npm registry…").color(TEXT_WEAK));
                    ui.add(egui::ProgressBar::new(0.62).animate(true).desired_width(180.0).corner_radius(6).fill(ACCENT));
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.add(Button::new("Check now").fill(ACCENT).min_size(vec2(110.0, 32.0)).corner_radius(10)).clicked() { self.toast(&ctx, "Update check started"); }
                    if ui.add(Button::new("Install update").min_size(vec2(130.0, 32.0)).corner_radius(10)).clicked() { self.toast(&ctx, "Nothing to install"); }
                });
            });
            card(ui, "Interface", |ui| {
                row(ui, "Font size", "Terminal grid font", |ui| { ui.add(egui::Slider::new(&mut self.font_size, 9.0..=24.0).suffix(" pt").step_by(0.5)); });
                row(ui, "Tab width", "Drag to change", |ui| { let mut v = 180.0f32; ui.add(egui::DragValue::new(&mut v).suffix(" px").range(120.0..=320.0)); });
                row(ui, "Theme", "Dark only for now", |ui| {
                    egui::ComboBox::from_id_salt("theme").selected_text(["Midnight", "Graphite", "Light"][self.theme_sel]).width(140.0).show_ui(ui, |ui| {
                        for (i, n) in ["Midnight", "Graphite", "Light"].iter().enumerate() { ui.selectable_value(&mut self.theme_sel, i, *n); }
                    });
                });
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Show toast").clicked() { self.toast(&ctx, "Saved settings"); }
                if ui.add(Button::new(RichText::new("Delete session…").color(DANGER))).clicked() { self.modal = Modal::Delete { title: "Redesign sidebar chrome".into() }; }
            });
        });
    }

    fn modal_ui(&mut self, ctx: &egui::Context) {
        let Modal::Delete { title } = &self.modal else { return };
        let title = title.clone();
        let mut close = false;
        let mut confirmed = false;
        let m = egui::Modal::new(Id::unique("delete-modal"))
            .backdrop_color(Color32::from_black_alpha(150))
            .frame(Frame::popup(&ctx.global_style()).fill(RAISED).corner_radius(16).inner_margin(Margin::same(22)).shadow(Shadow { offset: [0, 20], blur: 48, spread: 0, color: Color32::from_black_alpha(180) }))
            .show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(RichText::new("Delete session?").heading().color(Color32::WHITE));
                ui.add_space(6.0);
                ui.label(RichText::new(format!("“{title}” and its tmux window will be removed. This cannot be undone.")).color(TEXT_WEAK));
                ui.add_space(18.0);
                egui::Sides::new().show(ui, |_| {}, |ui| {
                    if ui.add(Button::new(RichText::new("Delete").color(Color32::WHITE)).fill(DANGER.gamma_multiply(0.85)).min_size(vec2(90.0, 32.0)).corner_radius(10)).clicked() { confirmed = true; close = true; }
                    if ui.add(Button::new("Cancel").min_size(vec2(80.0, 32.0)).corner_radius(10)).clicked() { close = true; }
                });
            });
        if m.should_close() || close { self.modal = Modal::None; }
        if confirmed { self.toast(ctx, format!("Deleted “{title}”")); }
    }
}

/// `egui::containers::menu::menu_style` resets button_padding to (2,0) for every menu/submenu Ui; re-apply ours.
fn menu_pad(ui: &mut Ui) {
    ui.spacing_mut().button_padding = vec2(10.0, 6.0);
    ui.spacing_mut().item_spacing.y = 2.0;
}

fn win_btn(ui: &mut Ui, glyph: &str, danger: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(36.0, 28.0), Sense::click());
    let t = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.1);
    let hover_col = if danger { DANGER } else { Color32::from_white_alpha(28) };
    ui.painter().rect_filled(rect, 8, hover_col.gamma_multiply(t));
    let fg = TEXT_WEAK.lerp_to_gamma(Color32::WHITE, t);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, glyph, FontId::new(16.0, FontFamily::Name(egui_material_icons::FONT_FAMILY.into())), fg);
    resp
}

fn session_row(ui: &mut Ui, s: &Sess, selected: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 42.0), Sense::click());
    let ctx = ui.ctx();
    let hov = ctx.animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.12);
    let sel = ctx.animate_bool_with_time(resp.id.with("sel"), selected, 0.18);
    let pill = rect.shrink2(vec2(0.0, 1.0));
    let p = ui.painter();
    p.rect_filled(pill, 10, RAISED.gamma_multiply(0.7 * hov * (1.0 - sel)));
    if sel > 0.01 {
        p.rect(pill, 10, ACCENT.gamma_multiply(0.16 * sel), Stroke::new(1.0, ACCENT.gamma_multiply(0.45 * sel)), StrokeKind::Inside);
        let bar = Rect::from_center_size(pos2(pill.left() + 2.0, pill.center().y), vec2(3.0, 18.0 * sel));
        p.rect_filled(bar, 2, ACCENT);
    }
    // status dot
    let dot = pos2(pill.left() + 20.0, pill.center().y);
    if s.running {
        let pulse = (ctx.input(|i| i.time) as f32 * 2.0).sin() * 0.5 + 0.5;
        glow_dot(p, dot, OK, pulse);
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    } else {
        p.circle_filled(dot, 3.0, TEXT_WEAK.gamma_multiply(0.5));
    }
    // title (single line, truncated with …)
    let mut job = LayoutJob::simple_singleline(s.title.to_owned(), FontId::proportional(13.5), TEXT.lerp_to_gamma(Color32::WHITE, sel));
    job.wrap.max_width = pill.width() - 36.0 - 56.0;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let g = p.layout_job(job);
    p.galley(pos2(pill.left() + 34.0, pill.center().y - g.size().y / 2.0), g, TEXT);
    p.text(pos2(pill.right() - 10.0, pill.center().y), Align2::RIGHT_CENTER, s.age, FontId::proportional(11.5), TEXT_WEAK);
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

fn card(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui)) {
    Frame::NONE.fill(SURFACE.gamma_multiply(0.7)).stroke(Stroke::new(1.0, STROKE)).corner_radius(14)
        .inner_margin(Margin::symmetric(18, 14)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            section_label(ui, title);
            ui.add_space(4.0);
            add(ui);
        });
    ui.add_space(12.0);
}

fn row(ui: &mut Ui, title: &str, sub: &str, right: impl FnOnce(&mut Ui)) {
    egui::Sides::new().height(40.0).show(ui,
        |ui| { ui.vertical(|ui| { ui.spacing_mut().item_spacing.y = 1.0; ui.label(RichText::new(title).color(TEXT)); ui.label(RichText::new(sub).small().color(TEXT_WEAK)); }); },
        right);
}

fn show_toasts(ctx: &egui::Context, toasts: &mut Vec<Toast>) {
    let now = ctx.input(|i| i.time);
    toasts.retain(|t| now - t.born < 3.5);
    for (i, t) in toasts.iter().rev().enumerate() {
        let age = (now - t.born) as f32;
        let a = (age / 0.2).min(1.0).min(((3.5 - age) / 0.4).clamp(0.0, 1.0)); // fade in/out
        egui::Area::new(Id::unique(("toast", t.id))).order(Order::Foreground).interactable(false)
            .anchor(Align2::RIGHT_BOTTOM, vec2(-20.0, -20.0 - i as f32 * 54.0 + (1.0 - a) * 10.0))
            .show(ctx, |ui| {
                ui.set_opacity(a);
                Frame::popup(ui.style()).fill(RAISED).corner_radius(12).inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(ICON_CHECK_CIRCLE.rich_text().size(18.0).color(OK));
                        ui.label(RichText::new(&t.text).color(TEXT));
                    });
                });
            });
    }
    if !toasts.is_empty() { ctx.request_repaint_after(std::time::Duration::from_millis(16)); }
}

/// Edge/corner resize for undecorated windows (no widget needed: polls pointer).
fn resize_handles(ctx: &egui::Context) {
    use egui::viewport::ResizeDirection as D;
    let screen = ctx.content_rect();
    let Some(p) = ctx.input(|i| i.pointer.hover_pos()) else { return };
    let b = 5.0;
    let (l, rr, t, bt) = (p.x < screen.left() + b, p.x > screen.right() - b, p.y < screen.top() + b, p.y > screen.bottom() - b);
    let (dir, cur) = match (l, rr, t, bt) {
        (true, _, true, _) => (D::NorthWest, CursorIcon::ResizeNorthWest),
        (_, true, true, _) => (D::NorthEast, CursorIcon::ResizeNorthEast),
        (true, _, _, true) => (D::SouthWest, CursorIcon::ResizeSouthWest),
        (_, true, _, true) => (D::SouthEast, CursorIcon::ResizeSouthEast),
        (true, ..) => (D::West, CursorIcon::ResizeWest),
        (_, true, ..) => (D::East, CursorIcon::ResizeEast),
        (_, _, true, _) => (D::North, CursorIcon::ResizeNorth),
        (.., true) => (D::South, CursorIcon::ResizeSouth),
        _ => return,
    };
    ctx.set_cursor_icon(cur);
    if ctx.input(|i| i.pointer.primary_pressed()) { ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir)); }
}

fn main() -> eframe::Result {
    let transparent = std::env::var_os("DEMO_TRANSPARENT").is_some();
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([760.0, 480.0])
            .with_decorations(false)
            .with_transparent(transparent)
            .with_app_id("omp-deck-demo"),
        ..Default::default()
    };
    eframe::run_native("omp-deck-demo", opts, Box::new(move |cc| Ok(Box::new(Demo::new(cc, transparent)))))
}
