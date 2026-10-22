//! The preferences page shown in the central area when the Settings tab is active: a nav rail on
//! the left (OmpDeck's own preferences, then omp's settings tabs) and the selected section on the
//! right, with a search field that filters across every omp tab.

use crate::icons::Icon;
use crate::icons::{
    ICON_BRAIN, ICON_CHEVRONS_UP_DOWN, ICON_CIRCLE_ALERT, ICON_CLOUD, ICON_CPU, ICON_FOLD_VERTICAL,
    ICON_FOLDER, ICON_KEYBOARD, ICON_LIST_CHECKS, ICON_LIST_TREE, ICON_MOON, ICON_NEWSPAPER,
    ICON_PALETTE, ICON_REFRESH_CW, ICON_SEARCH, ICON_SEARCH_X, ICON_SETTINGS_2,
    ICON_SLIDERS_HORIZONTAL, ICON_SPARKLES, ICON_SQUARE, ICON_SUN, ICON_TERMINAL,
    ICON_TRIANGLE_ALERT, ICON_WRENCH,
};
use deck_core::config::{Density, Theme, UiStyle};
use deck_core::omp::{Setting, SettingsCatalog};
use egui::{
    Align2, Color32, CursorIcon, Frame, Margin, RichText, Sense, Stroke, StrokeKind, Ui, pos2, vec2,
};
use serde_json::Value;

use crate::omp_rows::{
    JsonModal, ModalOutcome, RowEvent, SecretModal, note_banner, show_json_modal, show_row,
    show_secret_modal, skeleton,
};
use crate::omp_settings::{Model, TabHits, TabModel};
use crate::theme::{self, Palette};
use crate::update::{UpdateState, status_line};
use crate::widgets::{
    card, empty_state, icon_button, icon_text, paint_icon, segmented, setting_row, toggle,
};

/// Nav entry for OmpDeck's own preferences.
const OMPDECK_ENTRY: usize = 0;
const CONTENT_MAX_W: f32 = 760.0;

enum Modal {
    Json(JsonModal),
    Secret(SecretModal),
}

/// State of the omp half of the page.
#[derive(Default)]
struct OmpPane {
    model: Option<Model>,
    loading: bool,
    load_error: Option<String>,
    query: String,
    /// Search results for the query they were computed for.
    hits: Option<(String, Vec<TabHits>)>,
    modal: Option<Modal>,
}

#[derive(Default)]
pub struct SettingsState {
    /// 0 = OmpDeck, `1 + i` = omp tab `i`.
    selected: usize,
    /// (selected category or search marker, search query) last shown; used to reset scroll.
    view_key: Option<(usize, String)>,
    omp: OmpPane,
}

impl SettingsState {
    /// A catalog load finished (success or failure).
    pub fn catalog_loaded(&mut self, result: Result<SettingsCatalog, String>, config_dir: String) {
        let pane = &mut self.omp;
        pane.loading = false;
        pane.hits = None;
        match result {
            Ok(catalog) => {
                pane.model = Some(Model::from_catalog(catalog, config_dir));
                pane.load_error = None;
            }
            Err(e) => pane.load_error = Some(e),
        }
    }

    /// A write finished; returns the row's label for the toast (the key if the row is gone).
    pub fn write_done(&mut self, key: &str, outcome: Result<Option<Value>, String>) -> String {
        match self.omp.model.as_mut().and_then(|m| m.find_mut(key)) {
            Some(row) => {
                row.finish(outcome);
                row.label.clone()
            }
            None => key.to_owned(),
        }
    }
}

pub struct SettingsInput<'a> {
    pub font_size: f32,
    pub font_range: (f32, f32),
    pub font_default: f32,
    pub theme: Theme,
    pub style: UiStyle,
    pub density: Density,
    pub native_decorations: bool,
    pub omp_found: bool,
    pub omp_version: Option<&'a str>,
    pub check_updates: bool,
    pub update: &'a UpdateState,
}

#[derive(Debug, PartialEq)]
pub enum SettingsAction {
    FontSize(f32),
    Theme(Theme),
    Style(UiStyle),
    Density(Density),
    NativeDecorations(bool),
    SetCheckUpdates(bool),
    CheckUpdates,
    /// Load (or reload) omp's settings catalog on a worker.
    LoadCatalog,
    WriteSetting {
        setting: Box<Setting>,
        value: Value,
    },
    ResetSetting {
        key: String,
    },
    CopyKey(String),
    /// Copy an omp error message to the clipboard.
    CopyDetails(String),
}

fn tab_icon(id: &str) -> Icon {
    match id {
        "appearance" => ICON_PALETTE,
        "model" => ICON_BRAIN,
        "interaction" => ICON_KEYBOARD,
        "context" => ICON_NEWSPAPER,
        "memory" => ICON_CPU,
        "files" => ICON_FOLDER,
        "shell" => ICON_TERMINAL,
        "tools" => ICON_WRENCH,
        "tasks" => ICON_LIST_CHECKS,
        "providers" => ICON_CLOUD,
        "internal" => ICON_SETTINGS_2,
        _ => ICON_LIST_TREE,
    }
}

pub fn show(
    ui: &mut Ui,
    state: &mut SettingsState,
    input: &SettingsInput<'_>,
) -> Vec<SettingsAction> {
    let pal = Palette::of_ui(ui);
    let mut actions = Vec::new();

    // Lazy first load.
    let pane = &mut state.omp;
    if input.omp_found && pane.model.is_none() && !pane.loading && pane.load_error.is_none() {
        pane.loading = true;
        actions.push(SettingsAction::LoadCatalog);
    }
    let searching = input.omp_found && !pane.query.trim().is_empty();
    if searching {
        ensure_hits(pane);
    }

    egui::Panel::left("settings-nav")
        .exact_size(230.0)
        .resizable(false)
        .frame(
            Frame::NONE
                .fill(pal.panel)
                .inner_margin(Margin::symmetric(12, 18)),
        )
        .show(ui, |ui| {
            ui.label(theme::strong("Settings").size(20.0).color(pal.text_strong));
            ui.add_space(12.0);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| nav(ui, state, input, searching));
        });

    egui::CentralPanel::no_frame().show(ui, |ui| {
        Frame::NONE
            .fill(pal.bg)
            .inner_margin(Margin::symmetric(32, 24))
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                if input.omp_found {
                    search_field(ui, &mut state.omp.query);
                    ui.add_space(14.0);
                }
                // Scroll resets to the top whenever the category or the search query changes.
                let view_key = if searching {
                    (usize::MAX, state.omp.query.trim().to_owned())
                } else {
                    (state.selected, String::new())
                };
                let mut scroll = egui::ScrollArea::vertical()
                    .id_salt("settings-content")
                    .auto_shrink([false, false]);
                if state.view_key.as_ref() != Some(&view_key) {
                    state.view_key = Some(view_key);
                    scroll = scroll.vertical_scroll_offset(0.0);
                }
                scroll.show(ui, |ui| {
                    ui.set_max_width(ui.available_width().min(CONTENT_MAX_W));
                    if searching {
                        omp_view(ui, &mut state.omp, View::Search, &mut actions);
                    } else if state.selected == OMPDECK_ENTRY {
                        ompdeck_section(ui, input, &mut actions);
                    } else if !input.omp_found {
                        not_found_card(ui);
                    } else {
                        let tab = state.selected - 1;
                        omp_view(ui, &mut state.omp, View::Tab(tab), &mut actions);
                    }
                });
            });
    });
    show_modals(ui.ctx(), &mut state.omp, &mut actions);
    actions
}

fn ensure_hits(pane: &mut OmpPane) {
    let q = pane.query.trim();
    if pane.hits.as_ref().is_some_and(|(k, _)| k == q) {
        return;
    }
    pane.hits = pane.model.as_ref().map(|m| (q.to_owned(), m.search(q)));
}

// ------------------------------------------------------------------ nav

fn nav(ui: &mut Ui, state: &mut SettingsState, input: &SettingsInput<'_>, searching: bool) {
    let pal = Palette::of_ui(ui);
    if nav_item(
        ui,
        ICON_SLIDERS_HORIZONTAL,
        "OmpDeck",
        state.selected == OMPDECK_ENTRY && !searching,
        None,
    )
    .clicked()
    {
        state.selected = OMPDECK_ENTRY;
        state.omp.query.clear();
    }
    ui.add_space(12.0);
    ui.label(theme::section_caption("omp", pal.weak));
    ui.add_space(4.0);
    match (&state.omp.model, input.omp_found) {
        (Some(model), true) => {
            let hits = state.omp.hits.as_ref().filter(|_| searching);
            for (i, tab) in model.tabs.iter().enumerate() {
                let badge = match hits {
                    Some((_, hits)) => {
                        let n = hits.iter().find(|h| h.tab == i).map_or(0, |h| h.rows.len());
                        (n > 0).then(|| (n.to_string(), true))
                    }
                    None => Some((tab.rows.len().to_string(), false)),
                };
                let selected = !searching && state.selected == i + 1;
                if nav_item(
                    ui,
                    tab_icon(&tab.id),
                    &tab.label,
                    selected,
                    badge.as_ref().map(|(t, hot)| (t.as_str(), *hot)),
                )
                .clicked()
                {
                    state.selected = i + 1;
                    state.omp.query.clear();
                }
            }
        }
        _ => {
            let label = if input.omp_found {
                "omp settings"
            } else {
                "omp not found"
            };
            let icon = if input.omp_found {
                ICON_SETTINGS_2
            } else {
                ICON_TRIANGLE_ALERT
            };
            if nav_item(ui, icon, label, state.selected >= 1 && !searching, None).clicked() {
                state.selected = 1;
                state.omp.query.clear();
            }
        }
    }
}

fn nav_item(
    ui: &mut Ui,
    icon: Icon,
    label: &str,
    selected: bool,
    badge: Option<(&str, bool)>,
) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let neutral = theme::neutral();
    let (rect, resp) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::dens(30.0, 36.0)),
        Sense::click(),
    );
    let ctx = ui.ctx();
    let hov = ctx.animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.12);
    let sel = ctx.animate_bool_with_time(resp.id.with("sel"), selected, 0.18);
    let p = ui.painter();
    p.rect_filled(
        rect,
        if neutral { 6 } else { 10 },
        theme::hover_fill(pal).gamma_multiply(0.7 * hov * (1.0 - sel)),
    );
    if sel > 0.01 && neutral {
        p.rect_filled(rect, 6, pal.surface.gamma_multiply(sel));
        let bar = egui::Rect::from_center_size(
            pos2(rect.left() + 1.0, rect.center().y),
            vec2(2.0, 14.0 * sel),
        );
        p.rect_filled(bar, 1, pal.accent);
    } else if sel > 0.01 {
        p.rect(
            rect,
            10,
            theme::mix(pal.surface, pal.accent, 0.20).gamma_multiply(sel),
            Stroke::new(1.0, pal.accent.gamma_multiply(0.5 * sel)),
            StrokeKind::Inside,
        );
        let bar = egui::Rect::from_center_size(
            pos2(rect.left() + 2.0, rect.center().y),
            vec2(3.0, 18.0 * sel),
        );
        theme::gradient_rect(p, bar, pal.accent_a, pal.accent_b, false);
    }
    paint_icon(
        p,
        pos2(
            rect.left() + if neutral { 20.0 } else { 22.0 },
            rect.center().y,
        ),
        Align2::CENTER_CENTER,
        icon,
        if neutral { 16.0 } else { 19.0 },
        if neutral {
            theme::mix(pal.glyph, pal.text_strong, sel.max(hov * 0.6))
        } else {
            theme::mix(pal.weak, pal.accent, sel.max(hov * 0.6))
        },
    );
    p.text(
        pos2(
            rect.left() + if neutral { 36.0 } else { 40.0 },
            rect.center().y,
        ),
        Align2::LEFT_CENTER,
        label,
        crate::theme::font(if neutral { 13.0 } else { 14.0 }),
        theme::mix(pal.text, pal.text_strong, sel),
    );
    if let Some((text, hot)) = badge {
        let color = if hot { pal.accent } else { pal.weak };
        let g = p.layout_no_wrap(text.to_owned(), crate::theme::font(11.5), color);
        let chip = egui::Rect::from_center_size(
            pos2(
                rect.right() - 10.0 - (g.size().x + 12.0) / 2.0,
                rect.center().y,
            ),
            vec2(g.size().x + 12.0, 18.0),
        );
        let fill = if hot {
            pal.accent.gamma_multiply(0.18)
        } else if neutral {
            Color32::TRANSPARENT
        } else {
            pal.surface.gamma_multiply(0.8)
        };
        p.rect_filled(chip, 9, fill);
        p.galley(chip.center() - g.size() / 2.0, g, color);
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

// ------------------------------------------------------------------ search + omp views

fn search_field(ui: &mut Ui, query: &mut String) {
    let pal = Palette::of_ui(ui);
    let id = egui::Id::unique("omp-settings-search");
    let focused = ui.memory(|m| m.has_focus(id));
    ui.scope(|ui| {
        ui.set_max_width(ui.available_width().min(CONTENT_MAX_W));
        ui.add(
            egui::TextEdit::singleline(query)
                .id(id)
                .prefix(icon_text(ICON_SEARCH, 17.0, pal.weak))
                .hint_text("Search all omp settings…")
                .align(Align2::LEFT_CENTER)
                .min_size(vec2(0.0, 38.0))
                .desired_width(ui.available_width())
                .margin(Margin::symmetric(12, 9))
                .frame(
                    Frame::new()
                        .fill(pal.surface)
                        .corner_radius(12)
                        .stroke(Stroke::new(
                            1.0,
                            if focused { pal.accent } else { pal.stroke },
                        )),
                ),
        );
    });
}

#[derive(Clone, Copy)]
enum View {
    Tab(usize),
    Search,
}

fn omp_view(ui: &mut Ui, pane: &mut OmpPane, view: View, actions: &mut Vec<SettingsAction>) {
    let pal = Palette::of_ui(ui);
    let OmpPane {
        model,
        loading,
        load_error,
        query,
        hits,
        modal,
    } = pane;
    let Some(model) = model.as_mut() else {
        match (load_error.as_deref(), *loading) {
            (Some(err), false) => load_error_card(ui, err, actions, loading),
            _ => {
                ui.label(
                    theme::strong("omp settings")
                        .size(22.0)
                        .color(pal.text_strong),
                );
                ui.add_space(16.0);
                skeleton(ui);
            }
        }
        return;
    };

    let (title, sub) = match view {
        View::Tab(t) => match model.tabs.get(t) {
            Some(tab) => (tab.label.clone(), format!("{} settings", tab.rows.len())),
            None => return,
        },
        View::Search => {
            let n: usize = hits
                .as_ref()
                .map_or(0, |(_, h)| h.iter().map(|g| g.rows.len()).sum());
            (
                format!("{n} result{}", if n == 1 { "" } else { "s" }),
                format!("for “{}”", query.trim()),
            )
        }
    };
    egui::Sides::new().height(40.0).show(
        ui,
        |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::strong(title).size(22.0).color(pal.text_strong));
                ui.label(RichText::new(sub).color(pal.weak));
            });
        },
        |ui| {
            if *loading {
                ui.add_space(8.0);
                ui.add(crate::widgets::Spin::new(18.0, pal.accent_b));
            } else if icon_button(ui, ICON_REFRESH_CW, "Reload settings from omp", false).clicked()
            {
                *loading = true;
                actions.push(SettingsAction::LoadCatalog);
            }
        },
    );
    ui.add_space(8.0);
    let banner = format!(
        "Changes are written to {}/config.yml via `omp config`; running omp sessions reload them automatically.",
        model.config_dir
    );
    note_banner(ui, &banner);

    let mut events: Vec<(usize, usize, RowEvent)> = Vec::new();
    match view {
        View::Tab(t) => {
            let TabModel { rows, sections, .. } = &mut model.tabs[t];
            for sec in sections.iter() {
                card(ui, &sec.title, |ui| {
                    for (n, &ri) in sec.rows.iter().enumerate() {
                        if let Some(ev) = show_row(ui, &mut rows[ri], n == 0) {
                            events.push((t, ri, ev));
                        }
                    }
                });
            }
        }
        View::Search => {
            let groups: &[TabHits] = hits.as_ref().map_or(&[], |(_, h)| h.as_slice());
            if groups.is_empty() {
                empty_state(
                    ui,
                    ICON_SEARCH_X,
                    pal.weak,
                    "No matching settings",
                    "Try other words — every word must match the key, name or description.",
                );
            }
            for g in groups {
                let tab = &mut model.tabs[g.tab];
                let label = tab.label.clone();
                card(ui, &label, |ui| {
                    for (n, &ri) in g.rows.iter().enumerate() {
                        if let Some(ev) = show_row(ui, &mut tab.rows[ri], n == 0) {
                            events.push((g.tab, ri, ev));
                        }
                    }
                });
            }
        }
    }

    for (t, r, ev) in events {
        let row = &mut model.tabs[t].rows[r];
        match ev {
            RowEvent::Set(value) => {
                row.begin(Some(value.clone()));
                actions.push(SettingsAction::WriteSetting {
                    setting: Box::new(row.setting.clone()),
                    value,
                });
            }
            RowEvent::Reset => {
                row.begin(None);
                actions.push(SettingsAction::ResetSetting {
                    key: row.setting.key.clone(),
                });
            }
            RowEvent::CopyKey => actions.push(SettingsAction::CopyKey(row.setting.key.clone())),
            RowEvent::CopyDetails(text) => actions.push(SettingsAction::CopyDetails(text)),
            RowEvent::EditJson => *modal = Some(Modal::Json(JsonModal::new(row))),
            RowEvent::EditSecret => *modal = Some(Modal::Secret(SecretModal::new(row))),
        }
    }
}

fn show_modals(ctx: &egui::Context, pane: &mut OmpPane, actions: &mut Vec<SettingsAction>) {
    let outcome = match &mut pane.modal {
        None => return,
        Some(Modal::Json(m)) => (m.key.clone(), show_json_modal(ctx, m)),
        Some(Modal::Secret(m)) => (m.key.clone(), show_secret_modal(ctx, m)),
    };
    match outcome {
        (_, ModalOutcome::Open) => {}
        (_, ModalOutcome::Cancel) => pane.modal = None,
        (key, ModalOutcome::Save(value)) => {
            pane.modal = None;
            if let Some(row) = pane.model.as_mut().and_then(|m| m.find_mut(&key)) {
                row.begin(Some(value.clone()));
                actions.push(SettingsAction::WriteSetting {
                    setting: Box::new(row.setting.clone()),
                    value,
                });
            }
        }
    }
}

fn not_found_card(ui: &mut Ui) {
    let pal = Palette::of_ui(ui);
    ui.label(
        theme::strong("omp settings")
            .size(22.0)
            .color(pal.text_strong),
    );
    ui.add_space(16.0);
    card(ui, "omp not found", |ui| {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            ui.label(icon_text(ICON_TRIANGLE_ALERT, 26.0, pal.warn));
            ui.vertical(|ui| {
                ui.label(
                    theme::strong("omp not found on PATH")
                        .size(15.0)
                        .color(pal.text_strong),
                );
                ui.add_space(2.0);
                ui.add(
                    egui::Label::new(
                        RichText::new(
                            "OmpDeck edits omp's settings through the `omp` command, but could not find it on your PATH or in ~/.local/bin. Install omp or add it to PATH, then restart OmpDeck.",
                        )
                        .color(pal.weak),
                    )
                    .wrap_mode(egui::TextWrapMode::Wrap),
                );
            });
        });
    });
}

fn load_error_card(ui: &mut Ui, err: &str, actions: &mut Vec<SettingsAction>, loading: &mut bool) {
    let pal = Palette::of_ui(ui);
    ui.label(
        theme::strong("omp settings")
            .size(22.0)
            .color(pal.text_strong),
    );
    ui.add_space(16.0);
    card(ui, "Could not load settings", |ui| {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            ui.label(icon_text(ICON_CIRCLE_ALERT, 26.0, pal.danger));
            ui.vertical(|ui| {
                ui.add(
                    egui::Label::new(RichText::new(err).monospace().small().color(pal.text))
                        .wrap_mode(egui::TextWrapMode::Wrap),
                );
                ui.add_space(10.0);
                let retry = egui::Button::new((
                    icon_text(ICON_REFRESH_CW, 17.0, theme::on_accent(pal)),
                    RichText::new("Retry").color(theme::on_accent(pal)),
                ))
                .fill(pal.accent)
                .min_size(vec2(96.0, 32.0))
                .corner_radius(theme::rad(8, 10));
                if ui.add(retry).clicked() {
                    *loading = true;
                    actions.push(SettingsAction::LoadCatalog);
                }
            });
        });
    });
}

// ------------------------------------------------------------------ OmpDeck's own preferences

fn ompdeck_section(ui: &mut Ui, input: &SettingsInput<'_>, actions: &mut Vec<SettingsAction>) {
    let pal = Palette::of_ui(ui);
    ui.label(theme::strong("OmpDeck").size(22.0).color(pal.text_strong));
    ui.label(RichText::new("Appearance, window behaviour and updates of this app").color(pal.weak));
    ui.add_space(16.0);

    card(ui, "Appearance", |ui| {
        setting_row(
            ui,
            "Terminal font size",
            "Ctrl+Shift+= / - / 0 also work",
            |ui| {
                if ui
                    .add(
                        egui::Button::new(icon_text(crate::icons::ICON_ROTATE_CCW, 16.0, pal.text))
                            .min_size(vec2(30.0, 28.0)),
                    )
                    .on_hover_text("Reset to default")
                    .clicked()
                {
                    actions.push(SettingsAction::FontSize(input.font_default));
                }
                let mut size = input.font_size;
                ui.label(
                    theme::strong(format!("{:.0} pt", input.font_size)).color(pal.text_strong),
                );
                let slider = egui::Slider::new(&mut size, input.font_range.0..=input.font_range.1)
                    .step_by(1.0)
                    .show_value(false);
                if ui.add(slider).changed() {
                    actions.push(SettingsAction::FontSize(size));
                }
            },
        );
        ui.add_space(4.0);
        setting_row(ui, "Theme", "Chrome and terminal palette", |ui| {
            let mut idx = usize::from(input.theme == Theme::Light);
            if segmented(ui, &mut idx, &[(ICON_MOON, "Dark"), (ICON_SUN, "Light")]) {
                actions.push(SettingsAction::Theme(if idx == 0 {
                    Theme::Dark
                } else {
                    Theme::Light
                }));
            }
        });
        ui.add_space(4.0);
        setting_row(
            ui,
            "Style",
            "Neutral is flat and quiet; Midnight neon keeps the violet glow",
            |ui| {
                let mut idx = usize::from(input.style == UiStyle::Neon);
                if segmented(
                    ui,
                    &mut idx,
                    &[(ICON_SQUARE, "Neutral"), (ICON_SPARKLES, "Midnight neon")],
                ) {
                    actions.push(SettingsAction::Style(if idx == 0 {
                        UiStyle::Neutral
                    } else {
                        UiStyle::Neon
                    }));
                }
            },
        );
        ui.add_space(4.0);
        setting_row(
            ui,
            "Density",
            "Spacing of the sidebar, tabs, menus and lists",
            |ui| {
                let mut idx = usize::from(input.density == Density::Comfortable);
                if segmented(
                    ui,
                    &mut idx,
                    &[
                        (ICON_FOLD_VERTICAL, "Compact"),
                        (ICON_CHEVRONS_UP_DOWN, "Comfortable"),
                    ],
                ) {
                    actions.push(SettingsAction::Density(if idx == 0 {
                        Density::Compact
                    } else {
                        Density::Comfortable
                    }));
                }
            },
        );
    });

    card(ui, "Window", |ui| {
        setting_row(
            ui,
            "Native title bar",
            "Use the window manager's frame instead of OmpDeck's own. Applies after restarting OmpDeck.",
            |ui| {
                let mut on = input.native_decorations;
                if toggle(ui, &mut on).changed() {
                    actions.push(SettingsAction::NativeDecorations(on));
                }
            },
        );
    });

    card(ui, "Updates", |ui| {
        setting_row(
            ui,
            "Check for omp updates automatically",
            "Runs `omp update --check` at startup and every 6 hours.",
            |ui| {
                let mut on = input.check_updates;
                if toggle(ui, &mut on).changed() {
                    actions.push(SettingsAction::SetCheckUpdates(on));
                }
            },
        );
        ui.add_space(4.0);
        let version = match (input.omp_found, input.omp_version) {
            (false, _) => "omp not found on PATH".to_owned(),
            (true, Some(v)) => format!("omp {v}"),
            (true, None) => "omp".to_owned(),
        };
        setting_row(ui, &version, &status_line(input.update), |ui| {
            let checking = *input.update == UpdateState::Checking;
            if checking {
                ui.add(crate::widgets::Spin::new(18.0, pal.accent_b));
            } else if ui
                .add_enabled(
                    input.omp_found,
                    egui::Button::new((
                        icon_text(ICON_REFRESH_CW, 16.0, pal.glyph),
                        RichText::new("Check now"),
                    ))
                    .min_size(vec2(0.0, 30.0)),
                )
                .clicked()
            {
                actions.push(SettingsAction::CheckUpdates);
            }
        });
    });
}
