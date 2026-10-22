//! Sidebar scope: the chip at the top that says whether the sidebar shows every project or one
//! folder, its menu (all projects, recent folders, open folder…, current-folder actions) and the
//! folder's "New session" button.

use std::path::{Path, PathBuf};

use crate::icons::{
    ICON_CHECK, ICON_CHEVRONS_UP_DOWN, ICON_COPY, ICON_FOLDER, ICON_FOLDER_OPEN, ICON_FOLDER_PLUS,
    ICON_GLOBE, ICON_PLUS, ICON_X,
};
use egui::{
    Align2, CursorIcon, Frame, Margin, Rect, RichText, Sense, Stroke, StrokeKind, TextEdit, Ui,
    pos2, vec2,
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

fn chip_h() -> f32 {
    theme::dens(32.0, 40.0)
}
const MENU_W: f32 = 300.0;

/// The scope chip; its menu opens on click. In folder mode a ✕ on the chip returns to all projects.
pub fn chip(ui: &mut Ui, input: &ScopeInput<'_>, state: &mut ScopeState) -> Vec<ScopeAction> {
    let pal = Palette::of_ui(ui);
    let mut actions = Vec::new();
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), chip_h()), Sense::click());
    let clear_rect =
        Rect::from_center_size(pos2(rect.right() - 20.0, rect.center().y), vec2(26.0, 26.0));
    let clear = input
        .scope
        .is_some()
        .then(|| ui.interact(clear_rect, resp.id.with("clear"), Sense::click()));
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
    let hov = ui.ctx().animate_bool_with_time(
        resp.id.with("hov"),
        resp.hovered() || open || clear.as_ref().is_some_and(|c| c.hovered()),
        0.1,
    );

    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let neutral = theme::neutral();
        let (fill, stroke) = match input.scope {
            Some(_) if neutral => (
                theme::mix(pal.surface, pal.raised, hov),
                pal.accent.gamma_multiply(0.45 + 0.35 * hov),
            ),
            Some(_) => (
                theme::mix(pal.surface, pal.accent, 0.14 + 0.06 * hov),
                pal.accent.gamma_multiply(0.45 + 0.35 * hov),
            ),
            None => (
                theme::mix(pal.surface, pal.raised, hov),
                theme::mix(pal.stroke, pal.accent, 0.5 * hov),
            ),
        };
        p.rect(
            rect,
            if neutral { 8 } else { 11 },
            fill,
            Stroke::new(1.0, stroke),
            StrokeKind::Inside,
        );
        let (icon, color) = match input.scope {
            Some(_) => (ICON_FOLDER_OPEN, pal.accent),
            None => (ICON_GLOBE, pal.weak),
        };
        paint_icon(
            p,
            pos2(
                rect.left() + if neutral { 16.0 } else { 20.0 },
                rect.center().y,
            ),
            Align2::CENTER_CENTER,
            icon,
            if neutral { 16.0 } else { 20.0 },
            color,
        );
        let right = if input.scope.is_some() {
            clear_rect.left() - 2.0
        } else {
            rect.right() - 10.0
        };
        let chevron = pos2(right - 9.0, rect.center().y);
        paint_icon(
            p,
            chevron,
            Align2::CENTER_CENTER,
            ICON_CHEVRONS_UP_DOWN,
            if neutral { 14.0 } else { 17.0 },
            theme::mix(pal.weak, pal.text_strong, hov),
        );
        let text_x = rect.left() + if neutral { 32.0 } else { 38.0 };
        let max_w = (chevron.x - 14.0 - text_x).max(10.0);
        match input.scope {
            Some(dir) => {
                let name = p.layout(
                    folder_name(dir),
                    crate::theme::font(if neutral { 13.0 } else { 14.5 }),
                    pal.text_strong,
                    max_w,
                );
                let parent = parent_display(dir);
                let sub = p.layout(parent, crate::theme::font(11.0), pal.weak, max_w);
                let total = name.size().y + sub.size().y + 1.0;
                let top = rect.center().y - total / 2.0;
                p.galley(pos2(text_x, top), name.clone(), pal.text_strong);
                p.galley(pos2(text_x, top + name.size().y + 1.0), sub, pal.weak);
            }
            None => {
                let g = p.layout(
                    "All projects".to_owned(),
                    crate::theme::font(if neutral { 13.0 } else { 14.0 }),
                    pal.text,
                    max_w,
                );
                p.galley(
                    pos2(text_x, rect.center().y - g.size().y / 2.0),
                    g,
                    pal.text,
                );
            }
        }
        if let Some(clear) = &clear {
            let ch = ui
                .ctx()
                .animate_bool_with_time(clear.id.with("hov"), clear.hovered(), 0.1);
            let p = ui.painter();
            p.circle_filled(
                clear_rect.center(),
                11.0,
                theme::mix(pal.raised, pal.accent, 0.45).gamma_multiply(ch),
            );
            paint_icon(
                p,
                clear_rect.center(),
                Align2::CENTER_CENTER,
                ICON_X,
                16.0,
                theme::mix(pal.weak, pal.text_strong, ch),
            );
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
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.1);
    let p = ui.painter();
    p.rect(
        rect,
        10,
        pal.accent.gamma_multiply(0.14 + 0.16 * hov),
        Stroke::new(1.0, pal.accent.gamma_multiply(0.5 + 0.4 * hov)),
        StrokeKind::Inside,
    );
    paint_icon(
        p,
        pos2(rect.left() + 18.0, rect.center().y),
        Align2::CENTER_CENTER,
        ICON_PLUS,
        19.0,
        pal.accent,
    );
    let g = p.layout(
        format!("New session in {}", folder_name(folder)),
        crate::theme::font(13.5),
        pal.text_strong,
        rect.width() - 46.0,
    );
    p.galley(
        pos2(rect.left() + 34.0, rect.center().y - g.size().y / 2.0),
        g,
        pal.text_strong,
    );
    resp.on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(format!("Start omp in {}", folder.display()))
        .clicked()
}
