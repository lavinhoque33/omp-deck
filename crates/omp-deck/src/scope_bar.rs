//! Sidebar scope: the breadcrumb plate at the top that says whether the sidebar shows every
//! project or one folder, its menu (all projects, recent folders, open folder…, current-folder
//! actions) and the folder's "New session" button.

use std::path::{Path, PathBuf};

use crate::icons::{
    ICON_CHECK, ICON_CHEVRON_RIGHT, ICON_CHEVRONS_UP_DOWN, ICON_COPY, ICON_FOLDER,
    ICON_FOLDER_OPEN, ICON_FOLDER_PLUS, ICON_GLOBE, ICON_PLUS, ICON_X, Icon,
};
use egui::{
    Align2, Color32, CursorIcon, Frame, Margin, Rect, RichText, Sense, Stroke, StrokeKind,
    TextEdit, Ui, pos2, vec2,
};

use crate::folder_scope::{folder_name, parent_display};
use crate::theme::{self, Palette};
use crate::widgets::{menu_item, paint_icon};

pub struct ScopeInput<'a> {
    /// `None` = every project.
    pub scope: Option<&'a Path>,
    /// Most recent first.
    pub recent: &'a [PathBuf],
    /// kdialog or zenity exists.
    pub picker_available: bool,
    /// The native folder dialog is open.
    pub picking: bool,
    /// Sessions shown under the current scope (the total pill).
    pub total: usize,
}

/// `text` on one line, cut with an ellipsis at `width` (the plate never grows a second line).
fn one_line(
    p: &egui::Painter,
    text: String,
    font_id: egui::FontId,
    color: Color32,
    width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text, font_id, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(8.0));
    p.layout_job(job)
}

#[derive(Debug, PartialEq, Eq)]
pub enum ScopeAction {
    /// Switch the sidebar scope (`None` = all projects).
    Set(Option<PathBuf>),
    /// Run the native folder picker.
    Browse,
    /// A path typed into the menu.
    Typed(String),
    NewSession(PathBuf),
    OpenInFileManager(PathBuf),
    CopyPath(PathBuf),
}

#[derive(Default)]
pub struct ScopeState {
    typed: String,
    /// Why the last typed path was refused (shown in the menu).
    pub error: Option<String>,
}

impl ScopeState {
    /// The typed path was accepted: empty the field. A refused one stays for editing.
    pub fn clear_typed(&mut self) {
        self.typed.clear();
        self.error = None;
    }
}

fn chip_h(scoped: bool) -> f32 {
    if scoped {
        theme::dens(48.0, 54.0)
    } else {
        theme::dens(36.0, 42.0)
    }
}
const MENU_W: f32 = 300.0;

/// Depth shadow of a bordered key: 1 px dark at the bottom (lighter on light themes).
fn key_shadow(pal: &Palette) -> Color32 {
    if pal.bg.r() < 128 {
        Color32::from_black_alpha(160)
    } else {
        Color32::from_black_alpha(48)
    }
}

/// Paints a bordered 22 px key at `rect` (the scope bar's chevron / clear keys).
fn paint_bar_key(p: &egui::Painter, pal: &Palette, rect: Rect, icon: Icon, alpha: f32) {
    let radius = if theme::flat() { theme::ui_rad(3) } else { 6 };
    p.rect_filled(
        Rect::from_min_max(
            pos2(rect.left() + 1.0, rect.bottom() - 1.0),
            pos2(rect.right() - 1.0, rect.bottom()),
        ),
        0,
        key_shadow(pal).gamma_multiply(alpha),
    );
    p.rect_filled(rect, radius, pal.panel.gamma_multiply(alpha));
    p.rect_stroke(
        rect,
        radius,
        Stroke::new(1.0, pal.stroke.gamma_multiply(alpha)),
        StrokeKind::Inside,
    );
    paint_icon(
        p,
        rect.center(),
        Align2::CENTER_CENTER,
        icon,
        13.0,
        pal.glyph.gamma_multiply(alpha),
    );
}

/// The scope bar: full-width breadcrumb plate (`globe › All projects` or the folder's two
/// lines), total sessions pill, bordered chevron key (plus a clear ✕ key when scoped). Its
/// menu opens on click.
pub fn chip(ui: &mut Ui, input: &ScopeInput<'_>, state: &mut ScopeState) -> Vec<ScopeAction> {
    let pal = Palette::of_ui(ui);
    let mut actions = Vec::new();
    let scoped = input.scope.is_some();
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), chip_h(scoped)), Sense::click());
    let key_rect =
        Rect::from_center_size(pos2(rect.right() - 19.0, rect.center().y), vec2(22.0, 22.0));
    let _key = ui.interact(key_rect, resp.id.with("menu-key"), Sense::click());
    let clear_rect = key_rect.translate(vec2(-26.0, 0.0));
    let clear = scoped.then(|| ui.interact(clear_rect, resp.id.with("clear"), Sense::click()));
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
    let hov = ui.ctx().animate_bool_with_time(
        resp.id.with("hov"),
        resp.hovered() || open || clear.as_ref().is_some_and(|c| c.hovered()),
        0.1,
    );

    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let neutral = theme::flat();
        let radius = if neutral { theme::ui_rad(6) } else { 9 };
        // Fill `surface` with an `accent_a` 6 % wash at its left, 1 px `stroke` border.
        theme::skin::gradient_corners(
            p,
            resp.id.with("wash"),
            rect,
            [radius as f32; 4],
            (theme::mix(pal.surface, pal.accent_a, 0.06), pal.surface),
            theme::skin::Dir::Right,
        );
        p.rect_stroke(
            rect,
            radius,
            Stroke::new(1.0, pal.stroke),
            StrokeKind::Inside,
        );
        // Accent underline fading to transparent by 55 % of the width.
        let line_y = rect.bottom() - 0.5;
        let w = rect.width() * 0.55;
        let mut m = egui::epaint::Mesh::default();
        m.colored_vertex(pos2(rect.left(), line_y), pal.accent.gamma_multiply(0.7));
        m.colored_vertex(pos2(rect.left() + w, line_y), Color32::TRANSPARENT);
        m.colored_vertex(
            pos2(rect.left(), line_y + 1.0),
            pal.accent.gamma_multiply(0.7),
        );
        m.colored_vertex(pos2(rect.left() + w, line_y + 1.0), Color32::TRANSPARENT);
        m.add_triangle(0, 1, 2);
        m.add_triangle(1, 3, 2);
        p.add(egui::Shape::mesh(m));
        if hov > 0.01 {
            p.rect_stroke(
                rect,
                radius,
                Stroke::new(1.0, theme::hover_stroke(pal).gamma_multiply(hov)),
                StrokeKind::Inside,
            );
        }
        // Breadcrumb: globe › name.
        let mut x = rect.left() + 11.0;
        let cy = rect.center().y;
        paint_icon(
            p,
            pos2(x + 7.5, cy),
            Align2::CENTER_CENTER,
            ICON_GLOBE,
            15.0,
            pal.glyph,
        );
        x += 19.0;
        paint_icon(
            p,
            pos2(x + 6.0, cy),
            Align2::CENTER_CENTER,
            ICON_CHEVRON_RIGHT,
            12.0,
            pal.weak,
        );
        x += 16.0;
        let right = if scoped {
            clear_rect.left() - 6.0
        } else {
            key_rect.left() - 6.0
        };
        // Total pill before the keys.
        let tot = p.layout_no_wrap(input.total.to_string(), theme::font(9.0), pal.weak);
        let pill = Rect::from_center_size(
            pos2(right - (tot.size().x + 12.0) / 2.0 - 2.0, cy),
            vec2(tot.size().x + 12.0, 15.0),
        );
        let text_right = if right - pill.width() - 14.0 > x + 30.0 {
            p.rect_filled(pill, 7, pal.panel);
            p.rect_stroke(pill, 7, Stroke::new(1.0, pal.stroke), StrokeKind::Inside);
            p.galley(pill.center() - tot.size() / 2.0, tot, pal.weak);
            pill.left() - 6.0
        } else {
            right
        };
        let max_w = (text_right - x).max(10.0);
        match input.scope {
            Some(dir) => {
                paint_icon(
                    p,
                    pos2(x + 7.5, cy),
                    Align2::CENTER_CENTER,
                    ICON_FOLDER_OPEN,
                    15.0,
                    pal.accent,
                );
                let name = one_line(
                    p,
                    folder_name(dir),
                    theme::font(13.0),
                    pal.text_strong,
                    max_w - 20.0,
                );
                let sub = one_line(
                    p,
                    parent_display(dir),
                    egui::FontId::new(10.0, egui::FontFamily::Monospace),
                    pal.weak,
                    max_w - 20.0,
                );
                let total = name.size().y + sub.size().y + 1.0;
                let top = cy - total / 2.0;
                p.galley(pos2(x + 20.0, top), name.clone(), pal.text_strong);
                p.galley(pos2(x + 20.0, top + name.size().y + 1.0), sub, pal.weak);
            }
            None => {
                let g = one_line(
                    p,
                    "All projects".to_owned(),
                    theme::font(13.0),
                    pal.text_strong,
                    max_w,
                );
                p.galley(pos2(x, cy - g.size().y / 2.0), g, pal.text_strong);
            }
        }
        paint_bar_key(p, pal, key_rect, ICON_CHEVRONS_UP_DOWN, 1.0);
        if scoped {
            paint_bar_key(p, pal, clear_rect, ICON_X, 1.0);
        }
    }

    if let Some(clear) = clear {
        if clear.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        let clicked = clear.clicked();
        let _ = clear.on_hover_text("Back to all projects");
        if clicked {
            actions.push(ScopeAction::Set(None));
            return actions;
        }
    }
    let resp = resp
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(match input.scope {
            Some(dir) => format!(
                "{}\nClick to switch folder or view all projects",
                dir.display()
            ),
            None => "Showing sessions of every project.\nClick to focus a single folder".to_owned(),
        });
    theme::dropdown_start_menu(&resp, MENU_W, |ui| menu(ui, input, state, &mut actions));
    actions
}

fn menu(ui: &mut Ui, input: &ScopeInput<'_>, state: &mut ScopeState, out: &mut Vec<ScopeAction>) {
    let pal = Palette::of_ui(ui);
    ui.set_min_width(MENU_W - 16.0);
    ui.label(theme::section_caption("Show sessions for", pal.weak));
    ui.add_space(2.0);
    let all_icon = if input.scope.is_none() {
        ICON_CHECK
    } else {
        ICON_GLOBE
    };
    if menu_item(ui, all_icon, "All projects", None, true, false).clicked() {
        out.push(ScopeAction::Set(None));
        ui.close();
    }
    if !input.recent.is_empty() {
        ui.add_space(4.0);
        ui.label(theme::section_caption("Recent folders", pal.weak));
        ui.add_space(2.0);
        for dir in input.recent {
            let current = input.scope == Some(dir.as_path());
            let icon = if current { ICON_CHECK } else { ICON_FOLDER };
            let label = format!("{}   {}", folder_name(dir), parent_display(dir));
            let r = menu_item(ui, icon, &label, None, true, false)
                .on_hover_text(dir.display().to_string());
            if r.clicked() {
                out.push(ScopeAction::Set(Some(dir.clone())));
                ui.close();
            }
        }
    }
    ui.separator();
    if input.picker_available {
        let label = if input.picking {
            "Choosing a folder…"
        } else {
            "Open folder…"
        };
        if menu_item(ui, ICON_FOLDER_PLUS, label, None, !input.picking, false).clicked() {
            out.push(ScopeAction::Browse);
            ui.close();
        }
    }
    let edit = TextEdit::singleline(&mut state.typed)
        .hint_text("…or type a path, then Enter")
        .desired_width(f32::INFINITY)
        .frame(
            Frame::new()
                .inner_margin(Margin::symmetric(10, 7))
                .fill(pal.bg)
                .corner_radius(8)
                .stroke(Stroke::new(1.0, pal.stroke)),
        );
    let r = ui.add(edit);
    if r.changed() {
        state.error = None;
    }
    if r.lost_focus()
        && ui.input(|i| i.key_pressed(egui::Key::Enter))
        && !state.typed.trim().is_empty()
    {
        out.push(ScopeAction::Typed(state.typed.clone()));
        r.request_focus();
    }
    if let Some(e) = &state.error {
        ui.add(egui::Label::new(RichText::new(e).small().color(pal.danger)).truncate())
            .on_hover_text(e);
    }
    if let Some(dir) = input.scope {
        ui.separator();
        if menu_item(ui, ICON_PLUS, "New session here", None, true, false).clicked() {
            out.push(ScopeAction::NewSession(dir.to_path_buf()));
            ui.close();
        }
        if menu_item(
            ui,
            ICON_FOLDER_OPEN,
            "Open in file manager",
            None,
            true,
            false,
        )
        .clicked()
        {
            out.push(ScopeAction::OpenInFileManager(dir.to_path_buf()));
            ui.close();
        }
        if menu_item(ui, ICON_COPY, "Copy path", None, true, false).clicked() {
            out.push(ScopeAction::CopyPath(dir.to_path_buf()));
            ui.close();
        }
    }
}

/// Full-width "New session in <folder>" button; true when clicked.
pub fn new_session_button(ui: &mut Ui, folder: &Path) -> bool {
    let pal = Palette::of_ui(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.1);
    let p = ui.painter();
    let radius = if theme::flat() { theme::ui_rad(3) } else { 6 };
    // Accent border + wash + depth (a dark bottom edge, lighter on light themes).
    p.rect_filled(
        Rect::from_min_max(
            pos2(rect.left() + 2.0, rect.bottom() - 2.0),
            pos2(rect.right() - 2.0, rect.bottom()),
        ),
        0,
        key_shadow(pal),
    );
    p.rect(
        rect,
        radius,
        theme::mix(pal.panel, pal.accent, 0.10 + 0.06 * hov),
        Stroke::new(1.0, pal.accent.gamma_multiply(0.6 + 0.3 * hov)),
        StrokeKind::Inside,
    );
    // Inner accent highlight along the top.
    p.hline(
        (rect.left() + radius as f32)..=(rect.right() - radius as f32),
        rect.top() + 1.0,
        Stroke::new(1.0, pal.accent.gamma_multiply(0.25)),
    );
    let label = format!("New session in {}", folder_name(folder));
    let g = p.layout(
        label,
        crate::theme::font(11.0),
        pal.text_strong,
        rect.width() - 24.0,
    );
    // Centred icon + label.
    let total = 13.0 + 7.0 + g.size().x;
    let mut x = rect.center().x - total / 2.0;
    paint_icon(
        p,
        pos2(x + 6.5, rect.center().y),
        Align2::CENTER_CENTER,
        ICON_PLUS,
        13.0,
        pal.accent,
    );
    x += 20.0;
    p.galley(
        pos2(x, rect.center().y - g.size().y / 2.0),
        g,
        pal.text_strong,
    );
    resp.on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(format!("Start omp in {}", folder.display()))
        .clicked()
}
