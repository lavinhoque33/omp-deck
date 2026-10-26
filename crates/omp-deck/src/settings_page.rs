//! The preferences page shown in the central area when the Settings tab is active: a nav rail on
//! the left (OmpDeck's own preferences, then omp's settings tabs) and the selected section on the
//! right, with a search field that filters across every omp tab.

use crate::icons::Icon;
use crate::icons::{
    ICON_BELL, ICON_BRAIN, ICON_CHEVRONS_UP_DOWN, ICON_CIRCLE_ALERT, ICON_CIRCLE_ARROW_UP,
    ICON_CLOUD, ICON_CLOUD_UPLOAD, ICON_CPU, ICON_FOLD_VERTICAL, ICON_FOLDER, ICON_KEYBOARD,
    ICON_LIST_CHECKS, ICON_LIST_TREE, ICON_MOON, ICON_NEWSPAPER, ICON_PALETTE, ICON_REFRESH_CW,
    ICON_SEARCH, ICON_SEARCH_X, ICON_SETTINGS_2, ICON_SLIDERS_HORIZONTAL, ICON_SPARKLES, ICON_SUN,
    ICON_TERMINAL, ICON_TRIANGLE_ALERT, ICON_WRENCH,
};
use deck_core::config::{Density, GatewayMode, Theme, UiStyle};
use deck_core::mux::BackendChoice;
use deck_core::omp::{Setting, SettingsCatalog};
use deck_service::gateway::protocol::{Effective, GatewayStatus, Supervision};
use egui::{
    Align2, Color32, CursorIcon, FontId, Frame, Margin, Rect, RichText, Sense, Stroke, StrokeKind,
    Ui, Vec2, pos2, vec2,
};
use serde_json::Value;

use crate::drive_view::{self, DriveAction, DriveCard};
use crate::notify_view::{self, NotifyAction, NotifyInput};
use crate::omp_rows::{
    JsonModal, ModalOutcome, RowEvent, SecretModal, note_banner, show_json_modal, show_row,
    show_secret_modal, skeleton,
};
use crate::omp_settings::{Model, TabHits, TabModel};
use crate::sync_view::{self, SyncAction, SyncPage};
use crate::theme::{self, Palette};
use crate::update::{UpdateState, status_line};
use crate::widgets::{
    card, empty_state, icon_button, icon_text, paint_icon, segmented, setting_row, toggle,
};

/// One page of the settings area: an OmpDeck subpage, project sync, or omp's catalog tab `i`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Entry {
    OmpDeck(OmpDeckPage),
    Sync,
    Omp(usize),
}

impl Default for Entry {
    fn default() -> Self {
        Self::OmpDeck(OmpDeckPage::General)
    }
}

/// The subpages of Settings → OmpDeck, in nav order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OmpDeckPage {
    General,
    Appearance,
    Notifications,
    Updates,
    Backup,
}

impl OmpDeckPage {
    pub const ALL: [Self; 5] = [
        Self::General,
        Self::Appearance,
        Self::Notifications,
        Self::Updates,
        Self::Backup,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Appearance => "Appearance",
            Self::Notifications => "Notifications",
            Self::Updates => "Updates",
            Self::Backup => "Backup",
        }
    }

    /// Stable id part of the palette command.
    pub fn slug(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Appearance => "appearance",
            Self::Notifications => "notifications",
            Self::Updates => "updates",
            Self::Backup => "backup",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            Self::General => "Where sessions run and how the window behaves",
            Self::Appearance => "Font size, theme, style and density of this app",
            Self::Notifications => "What the bell and desktop pop-ups tell you about your sessions",
            Self::Updates => "Keep OmpDeck and omp up to date",
            Self::Backup => "Back up your omp configuration to Google Drive",
        }
    }

    pub fn icon(self) -> Icon {
        match self {
            Self::General => ICON_SLIDERS_HORIZONTAL,
            Self::Appearance => ICON_PALETTE,
            Self::Notifications => ICON_BELL,
            Self::Updates => ICON_CIRCLE_ARROW_UP,
            Self::Backup => ICON_CLOUD_UPLOAD,
        }
    }
}

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
    selected: Entry,
    /// (selected page or `None` while searching, search query) last shown; used to reset scroll.
    view_key: Option<(Option<Entry>, String)>,
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

    /// Show one OmpDeck subpage.
    pub fn show_ompdeck_page(&mut self, page: OmpDeckPage) {
        self.selected = Entry::OmpDeck(page);
        self.omp.query.clear();
    }

    /// Show the project sync page.
    pub fn show_sync(&mut self) {
        self.selected = Entry::Sync;
        self.omp.query.clear();
    }

    /// `(id, label)` of omp's settings categories once the catalog is loaded.
    pub fn omp_pages(&self) -> Vec<(String, String)> {
        self.omp.model.as_ref().map_or_else(Vec::new, |m| {
            m.tabs
                .iter()
                .map(|t| (t.id.clone(), t.label.clone()))
                .collect()
        })
    }

    /// Show omp's settings category `id`; false when the catalog has none by that id.
    pub fn show_omp_page(&mut self, id: &str) -> bool {
        let Some(i) = self
            .omp
            .model
            .as_ref()
            .and_then(|m| m.tabs.iter().position(|t| t.id == id))
        else {
            return false;
        };
        self.selected = Entry::Omp(i);
        self.omp.query.clear();
        true
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
    pub backend: BackendCard,
    pub drive: DriveCard,
    pub sync: SyncPage<'a>,
    pub notify: NotifyInput<'a>,
    pub app_update: crate::app_update::SettingsCard<'a>,
    pub mobile: MobileCard<'a>,
}

/// The session-backend card: the stored choice and which backends exist on this machine.
#[derive(Clone, Copy)]
pub struct BackendCard {
    pub choice: BackendChoice,
    pub tmux: bool,
    pub host: bool,
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
    /// Run omp's official installer in a tab (omp was not found).
    InstallOmp,
    /// The Google Drive backup card.
    Drive(DriveAction),
    /// Where new sessions run (Unix: tmux or the OmpDeck host).
    SessionBackend(BackendChoice),
    /// The project sync page.
    Sync(SyncAction),
    Notify(NotifyAction),
    /// The gateway lifetime mode (Settings → Mobile access).
    GatewayMode(GatewayMode),
    AppUpdate(crate::app_update::SettingsAction),
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
        theme::skin::paint_canvas(ui.painter(), ui.max_rect(), pal);
        Frame::NONE
            .inner_margin(Margin::symmetric(32, 24))
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                if input.omp_found {
                    search_field(ui, &mut state.omp.query);
                    ui.add_space(14.0);
                }
                // Scroll resets to the top whenever the category or the search query changes.
                let view_key = if searching {
                    (None, state.omp.query.trim().to_owned())
                } else {
                    (Some(state.selected), String::new())
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
                    } else {
                        match state.selected {
                            Entry::OmpDeck(page) => ompdeck_page(ui, page, input, &mut actions),
                            Entry::Sync => actions.extend(
                                sync_view::page(ui, &input.sync)
                                    .into_iter()
                                    .map(SettingsAction::Sync),
                            ),
                            Entry::Omp(_) if !input.omp_found => not_found_card(ui, &mut actions),
                            Entry::Omp(tab) => {
                                omp_view(ui, &mut state.omp, View::Tab(tab), &mut actions);
                            }
                        }
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
    ui.label(theme::section_caption("OmpDeck", pal.weak));
    ui.add_space(4.0);
    for page in OmpDeckPage::ALL {
        if nav_item(
            ui,
            page.icon(),
            page.label(),
            state.selected == Entry::OmpDeck(page) && !searching,
            None,
        )
        .clicked()
        {
            state.show_ompdeck_page(page);
        }
    }
    ui.add_space(8.0);
    if nav_item(
        ui,
        ICON_CLOUD,
        "Sync",
        state.selected == Entry::Sync && !searching,
        None,
    )
    .clicked()
    {
        state.show_sync();
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
                let selected = !searching && state.selected == Entry::Omp(i);
                if nav_item(
                    ui,
                    tab_icon(&tab.id),
                    &tab.label,
                    selected,
                    badge.as_ref().map(|(t, hot)| (t.as_str(), *hot)),
                )
                .clicked()
                {
                    state.selected = Entry::Omp(i);
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
            if nav_item(
                ui,
                icon,
                label,
                matches!(state.selected, Entry::Omp(_)) && !searching,
                None,
            )
            .clicked()
            {
                state.selected = Entry::Omp(0);
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
    let neutral = theme::flat();
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

fn not_found_card(ui: &mut Ui, actions: &mut Vec<SettingsAction>) {
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
                            format!("OmpDeck edits omp's settings through the `omp` command, but could not find it in {}. Install it with omp's official installer, or add it to PATH and restart OmpDeck.", crate::update::OMP_LOOKUP),
                        )
                        .color(pal.weak),
                    )
                    .wrap_mode(egui::TextWrapMode::Wrap),
                );
                ui.add_space(10.0);
                if crate::widgets::install_omp_button(ui) {
                    actions.push(SettingsAction::InstallOmp);
                }
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

/// One OmpDeck subpage: its title and one-line subtitle, then only its own cards.
fn ompdeck_page(
    ui: &mut Ui,
    page: OmpDeckPage,
    input: &SettingsInput<'_>,
    actions: &mut Vec<SettingsAction>,
) {
    let pal = Palette::of_ui(ui);
    ui.label(
        theme::strong(page.label())
            .size(22.0)
            .color(pal.text_strong),
    );
    ui.label(RichText::new(page.subtitle()).color(pal.weak));
    ui.add_space(16.0);
    match page {
        OmpDeckPage::General => general_cards(ui, input, actions),
        OmpDeckPage::Appearance => appearance_cards(ui, input, actions),
        OmpDeckPage::Notifications => actions.extend(
            notify_view::card_ui(ui, &input.notify)
                .into_iter()
                .map(SettingsAction::Notify),
        ),
        OmpDeckPage::Updates => updates_cards(ui, input, actions),
        OmpDeckPage::Backup => backup_card(ui, input, actions),
    }
}

fn appearance_cards(ui: &mut Ui, input: &SettingsInput<'_>, actions: &mut Vec<SettingsAction>) {
    let pal = Palette::of_ui(ui);
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
        let dark_only = input.style.dark_only();
        setting_row(
            ui,
            "Theme",
            if dark_only {
                "Chrome and terminal palette (this style is dark-only)"
            } else {
                "Chrome and terminal palette"
            },
            |ui| {
                ui.add_enabled_ui(!dark_only, |ui| {
                    let mut idx = usize::from(input.theme == Theme::Light && !dark_only);
                    if segmented(ui, &mut idx, &[(ICON_MOON, "Dark"), (ICON_SUN, "Light")]) {
                        actions.push(SettingsAction::Theme(if idx == 0 {
                            Theme::Dark
                        } else {
                            Theme::Light
                        }));
                    }
                })
                .response
                .on_disabled_hover_text(format!("{} is dark-only", input.style.label()));
            },
        );
        ui.add_space(8.0);
        style_gallery(ui, input.style, actions);
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
}

fn general_cards(ui: &mut Ui, input: &SettingsInput<'_>, actions: &mut Vec<SettingsAction>) {
    let pal = Palette::of_ui(ui);
    card(ui, "Sessions", |ui| {
        if cfg!(windows) {
            setting_row(
                ui,
                "Session backend",
                "Sessions run in the OmpDeck session host and keep running when you close their tab",
                |ui| {
                    ui.label(theme::strong("OmpDeck host").color(pal.text_strong));
                },
            );
            return;
        }
        let b = input.backend;
        let mut missing = Vec::new();
        if !b.tmux {
            missing.push("tmux");
        }
        if !b.host {
            missing.push("omp-deck-host");
        }
        let note = if missing.is_empty() {
            "Where new sessions run. Running sessions keep their backend.".to_owned()
        } else {
            format!(
                "Where new sessions run. Running sessions keep their backend. Not found: {}.",
                missing.join(", ")
            )
        };
        setting_row(ui, "Session backend", &note, |ui| {
            let mut idx = match b.choice {
                BackendChoice::Auto => 0,
                BackendChoice::Tmux => 1,
                BackendChoice::Host => 2,
            };
            if segmented(
                ui,
                &mut idx,
                &[
                    (ICON_SPARKLES, "Auto"),
                    (ICON_TERMINAL, "tmux"),
                    (ICON_CPU, "OmpDeck host"),
                ],
            ) {
                actions.push(SettingsAction::SessionBackend(match idx {
                    0 => BackendChoice::Auto,
                    1 => BackendChoice::Tmux,
                    _ => BackendChoice::Host,
                }));
            }
        });
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

    mobile_card(ui, input, actions);
}

/// What the Mobile access card shows: the gateway's own record of its lifetime mode.
pub struct MobileCard<'a> {
    /// `None` until the gateway has reported.
    pub status: Option<&'a GatewayStatus>,
    pub link_up: bool,
    /// A mode change is in flight.
    pub pending: bool,
    /// Why the last change failed.
    pub error: Option<&'a str>,
}

fn requested_text(mode: GatewayMode) -> &'static str {
    match mode {
        GatewayMode::Off => "Off",
        GatewayMode::WhileOpen => "While open",
        GatewayMode::Background => "Always on",
    }
}

/// `(headline, detail)`: the headline fits a row, the detail wraps under it.
type StatusText = (String, Option<String>);

fn effective_text(e: &Effective) -> StatusText {
    match e {
        Effective::OnDemand => ("Runs while needed".to_owned(), None),
        Effective::Background => ("Running in the background".to_owned(), None),
        Effective::Maintenance(p) => (
            "Maintenance".to_owned(),
            Some(format!("{p:?}: draining, refusing new work")),
        ),
        Effective::Stopping => (
            "Stopping".to_owned(),
            Some("No window and no work left.".to_owned()),
        ),
    }
}

/// Strips a `Failed("…")` Debug wrapper a message may have been given on the way.
fn plain_message(e: &str) -> &str {
    e.strip_prefix("Failed(\"")
        .and_then(|r| r.strip_suffix("\")"))
        .unwrap_or(e)
}

fn supervision_text(s: &Supervision) -> StatusText {
    match s {
        Supervision::None => ("Not requested".to_owned(), None),
        Supervision::Active { registration } => ("Active".to_owned(), Some(registration.clone())),
        Supervision::Registered { registration } => (
            "Registered, waiting for the supervisor".to_owned(),
            Some(registration.clone()),
        ),
        Supervision::Failed(e) => (
            "Registration failed".to_owned(),
            Some(plain_message(e).to_owned()),
        ),
    }
}

/// One status line: the label keeps its natural width, the headline sits right-aligned in what
/// remains (truncated, full text on hover) and the detail wraps in the card under the row.
fn status_row(ui: &mut Ui, pal: &Palette, title: &str, headline: &str, detail: Option<&str>) {
    ui.horizontal(|ui| {
        ui.add(egui::Label::new(RichText::new(title).color(pal.text_strong)).extend());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(egui::Label::new(RichText::new(headline).color(pal.text)).truncate())
                .on_hover_text(headline);
        });
    });
    if let Some(d) = detail {
        ui.add(egui::Label::new(RichText::new(d).small().color(pal.weak)).wrap())
            .on_hover_text(d);
    }
    ui.add_space(4.0);
}

/// The always-on gateway that mobile clients will use. Off by default; turning it on keeps the
/// gateway running in the background.
fn mobile_card(ui: &mut Ui, input: &SettingsInput<'_>, actions: &mut Vec<SettingsAction>) {
    let pal = Palette::of_ui(ui);
    let m = &input.mobile;
    card(ui, "Mobile access", |ui| {
        let mode = m.status.map_or(GatewayMode::Off, |s| s.mode);
        let editable = m.link_up && m.status.is_some() && !m.pending;
        let note = if m.pending {
            "Applying the change…"
        } else if !m.link_up {
            "The gateway is not reachable right now."
        } else {
            "Keep the OmpDeck gateway running so sessions stay reachable without this window."
        };
        setting_row(ui, "Allow mobile access", note, |ui| {
            ui.add_enabled_ui(editable, |ui| {
                let mut on = mode != GatewayMode::Off;
                if toggle(ui, &mut on).changed() {
                    actions.push(SettingsAction::GatewayMode(if on {
                        GatewayMode::Background
                    } else {
                        GatewayMode::Off
                    }));
                }
            });
        });
        if mode != GatewayMode::Off {
            setting_row(
                ui,
                "Gateway lifetime",
                "Always on survives logins and crashes; While open stops with the last window.",
                |ui| {
                    ui.add_enabled_ui(editable, |ui| {
                        let mut idx = usize::from(mode == GatewayMode::Background);
                        if segmented(
                            ui,
                            &mut idx,
                            &[(ICON_TERMINAL, "While open"), (ICON_CPU, "Always on")],
                        ) {
                            actions.push(SettingsAction::GatewayMode(if idx == 1 {
                                GatewayMode::Background
                            } else {
                                GatewayMode::WhileOpen
                            }));
                        }
                    });
                },
            );
        }
        if let Some(s) = m.status {
            let jobs = if s.jobs.is_empty() {
                ("No running jobs".to_owned(), None)
            } else {
                (format!("{} running job(s)", s.jobs.len()), None)
            };
            let lines = [
                ("Requested", (requested_text(mode).to_owned(), None)),
                ("Effective", effective_text(&s.effective)),
                ("Supervision", supervision_text(&s.supervision)),
                ("Jobs", jobs),
            ];
            for (title, (headline, detail)) in lines {
                status_row(ui, pal, title, &headline, detail.as_deref());
            }
        }
        if let Some(e) = m.error {
            ui.add(egui::Label::new(RichText::new(e).color(pal.warn)).wrap());
        }
    });
}

fn updates_cards(ui: &mut Ui, input: &SettingsInput<'_>, actions: &mut Vec<SettingsAction>) {
    let pal = Palette::of_ui(ui);
    actions.extend(
        crate::app_update::settings_card(ui, &input.app_update)
            .into_iter()
            .map(SettingsAction::AppUpdate),
    );

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

fn backup_card(ui: &mut Ui, input: &SettingsInput<'_>, actions: &mut Vec<SettingsAction>) {
    card(ui, "Google Drive backup", |ui| {
        actions.extend(
            drive_view::card(ui, &input.drive)
                .into_iter()
                .map(SettingsAction::Drive),
        );
    });
}

/// Width and height of one style tile's preview.
const TILE: Vec2 = vec2(148.0, 88.0);

/// Picker of every UI style: a miniature of its window (sidebar, canvas, a card, the accent) plus
/// its name. The active tile carries an accent ring; hover lifts the outline.
fn style_gallery(ui: &mut Ui, current: UiStyle, actions: &mut Vec<SettingsAction>) {
    let pal = Palette::of_ui(ui);
    ui.label(theme::strong("Style").color(pal.text_strong));
    ui.label(
        RichText::new("Pick the look of the whole app; every style has its own colours and shapes")
            .color(pal.weak)
            .size(12.0),
    );
    ui.add_space(8.0);
    let gap = 12.0;
    let label_h = 22.0;
    let cols = (((ui.available_width() + gap) / (TILE.x + gap)) as usize).max(1);
    for row in UiStyle::ALL.chunks(cols) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for style in row {
                let (rect, resp) =
                    ui.allocate_exact_size(vec2(TILE.x, TILE.y + label_h), Sense::click());
                let selected = *style == current;
                let preview = Rect::from_min_size(rect.min, TILE);
                paint_style_preview(ui.painter(), preview, *style);
                let ring = if selected {
                    Stroke::new(2.0, pal.accent)
                } else if resp.hovered() {
                    Stroke::new(2.0, theme::hover_stroke(pal))
                } else {
                    Stroke::new(1.0, pal.stroke)
                };
                ui.painter()
                    .rect_stroke(preview, 8, ring, egui::StrokeKind::Outside);
                let label_pos = pos2(preview.left() + 2.0, preview.bottom() + 5.0);
                ui.painter().text(
                    label_pos,
                    Align2::LEFT_TOP,
                    style.label(),
                    FontId::new(12.5, theme::ui_family()),
                    if selected { pal.text_strong } else { pal.text },
                );
                if resp.on_hover_text(style.label()).clicked() && !selected {
                    actions.push(SettingsAction::Style(*style));
                }
            }
        });
        ui.add_space(6.0);
    }
}

/// The miniature of `style`: window, sidebar, canvas, one card and the accent.
fn paint_style_preview(painter: &egui::Painter, rect: Rect, style: UiStyle) {
    use crate::theme::{CardShadow, Region};
    let p = theme::palette(style, true);
    let look = theme::look_of(style);
    painter.rect_filled(rect, 8, p.window);
    let inner = rect.shrink(4.0);
    let side_w = inner.width() * 0.26;
    let (side, canvas, r) = match look.region {
        Region::Islands => {
            let side = Rect::from_min_size(inner.min, vec2(side_w - 2.0, inner.height()));
            let canvas = Rect::from_min_max(pos2(side.right() + 4.0, inner.top()), inner.max);
            (side, canvas, 6)
        }
        _ => {
            let side = Rect::from_min_size(inner.min, vec2(side_w, inner.height()));
            let canvas = Rect::from_min_max(pos2(side.right(), inner.top()), inner.max);
            (side, canvas, 3)
        }
    };
    let rounded = |rect: Rect, radius: u8| {
        painter.rect_filled(rect, radius, p.panel);
    };
    match look.region {
        Region::Islands => {
            rounded(side, r);
            painter.rect_stroke(
                side,
                r,
                Stroke::new(1.0, p.separator),
                egui::StrokeKind::Inside,
            );
            painter.rect_filled(canvas, r, p.bg);
            painter.rect_stroke(
                canvas,
                r,
                Stroke::new(1.0, p.separator),
                egui::StrokeKind::Inside,
            );
        }
        Region::Frame => {
            painter.rect_filled(side, 0, p.window);
            painter.rect_filled(
                canvas,
                egui::CornerRadius {
                    nw: 6,
                    ne: 0,
                    sw: 0,
                    se: 3,
                },
                p.bg,
            );
        }
        _ => {
            painter.rect_filled(
                side,
                egui::CornerRadius {
                    nw: 3,
                    ne: 0,
                    sw: 3,
                    se: 0,
                },
                p.panel,
            );
            painter.rect_filled(
                canvas,
                egui::CornerRadius {
                    nw: 0,
                    ne: 3,
                    sw: 0,
                    se: 3,
                },
                p.bg,
            );
            painter.vline(side.right(), side.y_range(), Stroke::new(1.0, p.separator));
        }
    }
    // Sidebar rows; the first is the selected one.
    for i in 0..3 {
        let row = Rect::from_min_size(
            pos2(side.left() + 5.0, side.top() + 8.0 + i as f32 * 11.0),
            vec2(side.width() - 10.0, 7.0),
        );
        if i == 0 {
            painter.rect_filled(row, 2, p.raised);
            painter.rect_filled(
                Rect::from_min_size(row.min, vec2(2.0, row.height())),
                1,
                p.accent,
            );
        } else {
            painter.rect_filled(row, 2, theme::mix(p.panel, p.text, 0.12));
        }
    }
    // A card with a status line and the accent chip.
    let card = Rect::from_min_size(
        pos2(canvas.left() + 8.0, canvas.top() + 9.0),
        vec2(canvas.width() - 16.0, 36.0),
    );
    let cr = look.card.radius.min(8);
    if look.card.shadow == CardShadow::Hard {
        painter.rect_filled(card.translate(vec2(3.0, 3.0)), cr, egui::Color32::BLACK);
    }
    painter.rect_filled(card, cr, p.surface);
    painter.rect_stroke(
        card,
        cr,
        Stroke::new(look.card.border_w.clamp(1.0, 2.0), p.stroke),
        egui::StrokeKind::Inside,
    );
    painter.rect_filled(
        Rect::from_min_size(card.min + vec2(6.0, 7.0), vec2(card.width() * 0.5, 4.0)),
        2,
        p.text_strong,
    );
    painter.rect_filled(
        Rect::from_min_size(card.min + vec2(6.0, 15.0), vec2(card.width() * 0.7, 3.0)),
        1,
        p.weak,
    );
    painter.rect_filled(
        Rect::from_min_size(card.min + vec2(6.0, 24.0), vec2(card.width() * 0.3, 5.0)),
        2,
        p.accent,
    );
    painter.circle_filled(card.right_top() + vec2(-8.0, 8.0), 2.5, p.ok);
    // Second, dimmer card.
    let card2 = Rect::from_min_size(
        card.left_bottom() + vec2(0.0, 6.0),
        vec2(card.width(), 14.0),
    );
    if card2.bottom() < canvas.bottom() - 2.0 {
        painter.rect_filled(card2, cr.min(4), p.surface);
        painter.rect_stroke(
            card2,
            cr.min(4),
            Stroke::new(1.0, p.stroke),
            egui::StrokeKind::Inside,
        );
    }
}
