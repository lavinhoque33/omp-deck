//! The Running dock pinned to the sidebar's bottom: every session still alive in the session
//! backends (tmux or the OmpDeck host), with or without an open tab. A grab strip resizes it,
//! the header folds it; rows with a tab come first in strip order.
use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use deck_core::mux::session_name_for;
use deck_core::sessions::SessionEntry;
use deck_service::backend::kind_label;
use deck_service::status::TabStatus;
use egui::text::{LayoutJob, TextFormat};
use egui::{Align2, CursorIcon, Id, Rect, Sense, Stroke, Ui, pos2, vec2};

use super::{
    KeyTone, STATUS_SIZE, SidebarAction, SidebarInput, SidebarOutput, dock_row_h, ledger_key,
    paint_key_at, paint_row_bg, row_radius, session_menu, tab_menu, view_action, view_icon,
    view_switch_reason,
};
use crate::badge::{self, Hue};
use crate::folder_scope;
use crate::icons::{
    ICON_APP_WINDOW, ICON_CHEVRON_DOWN, ICON_CHEVRON_UP, ICON_CIRCLE_STOP, ICON_PLAY,
    ICON_TERMINAL, ICON_X,
};
use crate::tabs;
use crate::theme::{self, Palette, skin};
use crate::widgets::{menu_item, paint_icon};
use crate::{card, tips};

/// Most rows the dock fits before it scrolls.
const MAX_ROWS: f32 = 6.0;
/// Height of the grab strip above the dock header.
const GRAB_H: f32 = 11.0;
/// Height of the dock header.
fn header_h() -> f32 {
    32.0
}

/// One live session of the section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Row {
    /// The backend session name.
    name: String,
    /// Strip index of the tab showing it.
    tab: Option<usize>,
    /// `(group, session)` in the sessions snapshot: the session file it holds open.
    loc: Option<(usize, usize)>,
    /// That file's mtime.
    modified: Option<SystemTime>,
}

/// Sessions with a tab first (strip order), then the rest newest-first by session file; sessions
/// without a listed file last, by name.
fn order(rows: &mut [Row]) {
    rows.sort_by(|a, b| match (a.tab, b.tab) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => match (a.modified, b.modified) {
            (Some(x), Some(y)) => y.cmp(&x).then_with(|| a.name.cmp(&b.name)),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => a.name.cmp(&b.name),
        },
    });
}

/// `(sessions snapshot, scope, live sessions + tabs)`.
type Key = ((u64, usize), Option<PathBuf>, u64);

/// The rows, valid for one [`Key`] (the path lookups run once per change, not per frame).
#[derive(Default)]
pub(super) struct RunningCache {
    key: Option<Key>,
    rows: Vec<Row>,
}

impl RunningCache {
    fn sync(&mut self, input: &SidebarInput<'_>) -> &[Row] {
        let key = (
            (
                input.generation,
                input.groups.map_or(0, |g| g.as_ptr() as usize),
            ),
            input.scope.map(Path::to_path_buf),
            fingerprint(input),
        );
        if self.key.as_ref() != Some(&key) {
            self.rows = build(input);
            self.key = Some(key);
        }
        &self.rows
    }
}

/// What the rows depend on besides the sessions snapshot: which sessions live and hold which
/// files (order independent), and the tab strip's session names and folders.
fn fingerprint(input: &SidebarInput<'_>) -> u64 {
    let snap = input.running;
    let live = snap.alive.iter().fold(0u64, |acc, name| {
        let mut h = DefaultHasher::new();
        name.hash(&mut h);
        snap.files_of(name).hash(&mut h);
        acc.wrapping_add(h.finish())
    });
    let mut h = DefaultHasher::new();
    (live, snap.blind, input.tabs.len()).hash(&mut h);
    for t in input.tabs {
        t.mux_name().hash(&mut h);
        t.cwd.as_path().hash(&mut h);
    }
    h.finish()
}

fn build(input: &SidebarInput<'_>) -> Vec<Row> {
    let snap = input.running;
    let mut tab_of: HashMap<&str, usize> = HashMap::new();
    for (i, t) in input.tabs.iter().enumerate() {
        if let Some(name) = t.mux_name() {
            tab_of.entry(name).or_insert(i);
        }
    }
    let mut rows: Vec<Row> = snap
        .alive
        .iter()
        .map(|name| Row {
            name: name.clone(),
            tab: tab_of.get(name.as_str()).copied(),
            loc: None,
            modified: None,
        })
        .collect();
    if let Some(groups) = input.groups {
        // A session holds the files it has open; one that holds none (a fresh omp) is the
        // session of the id its canonical name is made from.
        let mut by_path: HashMap<&Path, usize> = HashMap::new();
        let mut by_name: HashMap<&str, usize> = HashMap::new();
        for (k, row) in rows.iter().enumerate() {
            let files = snap.files_of(&row.name);
            if files.is_empty() {
                by_name.insert(row.name.as_str(), k);
            }
            for f in files {
                by_path.entry(f.as_path()).or_insert(k);
            }
        }
        let mut found: Vec<Option<((usize, usize), SystemTime)>> = vec![None; rows.len()];
        for (gi, group) in groups.iter().enumerate() {
            for (si, e) in group.sessions.iter().enumerate() {
                let k = by_path.get(e.path.as_path()).copied().or_else(|| {
                    (!by_name.is_empty())
                        .then(|| by_name.get(session_name_for(&e.id).as_str()).copied())
                        .flatten()
                });
                if let Some(k) = k
                    && found[k].is_none_or(|(_, m)| e.modified > m)
                {
                    found[k] = Some(((gi, si), e.modified));
                }
            }
        }
        for (row, f) in rows.iter_mut().zip(found) {
            if let Some((loc, modified)) = f {
                row.loc = Some(loc);
                row.modified = Some(modified);
            }
        }
    }
    if let Some(scope) = input.scope {
        rows.retain(|r| {
            let cwd = match (r.loc, input.groups) {
                (Some((g, s)), Some(groups)) => groups
                    .get(g)
                    .and_then(|g| g.sessions.get(s))
                    .map(|e| e.cwd.as_path()),
                _ => r
                    .tab
                    .and_then(|i| input.tabs.get(i))
                    .map(|t| t.cwd.as_path()),
            };
            cwd.is_some_and(|c| folder_scope::contains(scope, c))
        });
    }
    order(&mut rows);
    rows
}

/// The live state of a row: its tab's, else what the session's terminal title says.
fn status_of(input: &SidebarInput<'_>, row: &Row) -> TabStatus {
    match row.tab.and_then(|i| input.tabs.get(i).map(|t| (i, t))) {
        Some((i, tab)) => tab.status(i == input.active_tab),
        None => TabStatus::of(None, input.running.activity_of(&row.name), false),
    }
}
/// Whether the dock shows at all: hidden when blind, nothing alive, or nothing in scope.
fn rows_for<'c>(cache: &'c mut RunningCache, input: &SidebarInput<'_>) -> &'c [Row] {
    if input.running.blind || input.running.alive.is_empty() {
        return &[];
    }
    let rows = cache.sync(input);
    if rows.is_empty() { &[] } else { rows }
}

/// The dock's chrome height: grab strip + header + top border.
fn chrome_h() -> f32 {
    GRAB_H + header_h() + 1.0
}

/// Clamps the dock body to [one row, 55 % of the sidebar] given the sidebar height.
fn clamp_body(want: f32, sidebar_h: f32) -> f32 {
    let max = (sidebar_h * 0.55 - chrome_h()).max(dock_row_h() + 8.0);
    want.clamp(dock_row_h() + 8.0, max)
}

/// The dock's height for this frame (0 = hidden). `sidebar_h` is the full sidebar height the
/// 55 % clamp is computed against.
pub(super) fn dock_height_for(input: &SidebarInput<'_>, rows: usize, sidebar_h: f32) -> f32 {
    if rows == 0 {
        return 0.0;
    }
    if input.dock_collapsed {
        return chrome_h();
    }
    let fit = (dock_row_h() + 2.0) * (rows as f32).min(MAX_ROWS) + 8.0;
    let body = if input.dock_height > 0.0 {
        clamp_body(input.dock_height - chrome_h(), sidebar_h)
    } else {
        clamp_body(fit, sidebar_h)
    };
    chrome_h() + body
}

/// Measures the dock without painting or interacting: syncs the cache so the count is current
/// and returns the height [`show_dock`] will take (0 = hidden). `sidebar_h` is the full
/// sidebar height the 55 % clamp is computed against.
pub(super) fn measure(
    _ui: &mut Ui,
    input: &SidebarInput<'_>,
    cache: &mut RunningCache,
    sidebar_h: f32,
) -> f32 {
    let rows = rows_for(cache, input);
    dock_height_for(input, rows.len(), sidebar_h)
}

/// The Running dock: grab strip, header (title, count pill, attention dot, collapse key) and
/// the rows. `height` is the [`measure`]d height.
pub(super) fn show_dock(
    ui: &mut Ui,
    input: &SidebarInput<'_>,
    cache: &mut RunningCache,
    height: f32,
    sidebar_h: f32,
    out: &mut SidebarOutput,
) {
    let rows: Vec<Row> = rows_for(cache, input).to_vec();
    if rows.is_empty() || height <= 0.0 {
        return;
    }
    let total = ui.available_width();
    let top = ui.cursor().top();
    let rect = Rect::from_min_size(pos2(ui.cursor().left(), top), vec2(total, height));
    // Claim the whole dock rect up front so the groups scroll area never overlaps it.
    ui.advance_cursor_after_rect(rect);
    if !ui.is_rect_visible(rect) {
        return;
    }
    let pal = Palette::of_ui(ui);
    let p = ui.painter();
    // Top edge: 1 px `stroke` border.
    p.hline(
        rect.x_range(),
        rect.top() + 0.5,
        Stroke::new(1.0, pal.stroke),
    );
    let strip = Rect::from_min_size(
        pos2(rect.left(), rect.top() + 1.0),
        vec2(rect.width(), GRAB_H),
    );
    // Grab strip: `surface→panel` gradient with four centred dots.
    theme::gradient_rect(p, strip, pal.surface, pal.panel, false);
    for k in 0..4 {
        let c = pos2(strip.center().x + (k as f32 - 1.5) * 7.0, strip.center().y);
        p.circle_filled(c, 1.5, pal.weak);
    }
    let grab_id = Id::unique("running-dock-grab");
    let grab = ui.interact(strip, grab_id, Sense::click_and_drag());
    let grab = grab.on_hover_cursor(egui::CursorIcon::ResizeVertical);
    if grab.dragged() {
        // Dragging up from the strip unfolds a collapsed dock.
        if input.dock_collapsed {
            out.dock_collapsed = Some(false);
        } else if let Some(dy) =
            ui.input(|i| (i.pointer.delta().y != 0.0).then_some(i.pointer.delta().y))
        {
            let next = chrome_h() + clamp_body(height - chrome_h() - dy, sidebar_h);
            if (next - input.dock_height).abs() > 1.0 {
                out.dock_height = Some(next);
            }
        }
    }
    let head = Rect::from_min_size(
        pos2(rect.left(), strip.bottom()),
        vec2(rect.width(), header_h()),
    );
    p.rect_filled(
        Rect::from_min_max(head.min, pos2(head.right(), head.bottom() + 1.0)),
        0,
        pal.surface,
    );
    p.hline(
        head.x_range(),
        head.bottom() + 0.5,
        Stroke::new(1.0, pal.stroke),
    );
    let waiting = rows
        .iter()
        .filter(|r| status_of(input, r) == TabStatus::Attention)
        .count();
    // Header contents in a child row: title, count pill, attention dot, spacer, collapse key.
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(head.shrink2(vec2(6.0, 0.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.spacing_mut().item_spacing.x = 6.0;
    child.label(
        egui::RichText::new("Running")
            .font(theme::font(12.5))
            .color(pal.text_strong),
    );
    count_pill(&mut child, pal, rows.len());
    if waiting > 0 {
        // The warn glow dot stays visible while collapsed.
        let (dot_rect, _) = child.allocate_exact_size(vec2(11.0, 11.0), Sense::hover());
        theme::glow_dot(
            child.painter(),
            dot_rect.center(),
            3.5,
            pal.warn,
            theme::GLOW_REST,
        );
    }
    child.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let key = ledger_key(
            ui,
            if input.dock_collapsed {
                ICON_CHEVRON_UP
            } else {
                ICON_CHEVRON_DOWN
            },
            if input.dock_collapsed {
                "Expand the Running dock"
            } else {
                "Collapse the Running dock"
            },
            22.0,
            KeyTone::Plain,
        );
        if key.clicked() {
            out.dock_collapsed = Some(!input.dock_collapsed);
            out.interacted = true;
        }
    });
    if input.dock_collapsed {
        return;
    }
    let body_rect = Rect::from_min_max(
        pos2(rect.left(), head.bottom() + 1.0),
        pos2(rect.right(), rect.bottom()),
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(body_rect)
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
    );
    egui::ScrollArea::vertical()
        .id_salt("running-dock-list")
        .auto_shrink([false, false])
        .show(&mut child, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.add_space(3.0);
            for row in &rows {
                running_row(ui, input, row, out);
            }
            ui.add_space(3.0);
        });
}

/// The header's count pill: accent border and text.
fn count_pill(ui: &mut Ui, pal: &Palette, n: usize) {
    let text = n.to_string();
    let g = ui
        .painter()
        .layout_no_wrap(text, theme::font(9.0), pal.accent);
    let (rect, _) =
        ui.allocate_exact_size(vec2((g.size().x + 10.0).max(18.0), 15.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 7, pal.panel);
    p.rect_stroke(
        rect,
        7,
        Stroke::new(1.0, skin::alpha(pal.accent, 153)),
        egui::StrokeKind::Inside,
    );
    p.galley(rect.center() - g.size() / 2.0, g, pal.accent);
}

/// The tab-less row's dashed "attach" pill at rest (play icon in `ok`): returns its width.
fn attach_pill(p: &egui::Painter, pal: &Palette, pill: Rect) -> f32 {
    let text = p.layout_no_wrap("attach".to_owned(), theme::font(9.0), pal.weak);
    let w = 9.0 + 4.0 + text.size().x + 10.0;
    let rect = Rect::from_min_size(
        pos2(pill.right() - 8.0 - w, pill.center().y - 8.0),
        vec2(w, 16.0),
    );
    p.rect_filled(rect, 2, pal.panel);
    let dash = egui::Shape::dashed_line(
        &[
            rect.left_top(),
            rect.right_top(),
            rect.right_bottom(),
            rect.left_bottom(),
        ],
        Stroke::new(1.0, pal.stroke),
        3.0,
        2.0,
    );
    // `dashed_line` connects the points in order; close the loop by hand.
    p.add(dash);
    p.add(egui::Shape::dashed_line(
        &[rect.left_bottom(), rect.left_top()],
        Stroke::new(1.0, pal.stroke),
        3.0,
        2.0,
    ));
    paint_icon(
        p,
        pos2(rect.left() + 5.0 + 4.5, rect.center().y),
        Align2::CENTER_CENTER,
        ICON_PLAY,
        9.0,
        pal.ok,
    );
    p.galley(
        pos2(
            rect.left() + 9.0 + 4.0 + 4.0,
            rect.center().y - text.size().y / 2.0,
        ),
        text,
        pal.weak,
    );
    w + 8.0
}

fn running_row(ui: &mut Ui, input: &SidebarInput<'_>, row: &Row, out: &mut SidebarOutput) {
    let pal = Palette::of_ui(ui);
    let entry: Option<&SessionEntry> = row.loc.and_then(|(g, s)| {
        input
            .groups
            .and_then(|gs| gs.get(g))
            .and_then(|g| g.sessions.get(s))
    });
    let tab = row.tab.and_then(|i| input.tabs.get(i));
    let status = status_of(input, row);
    let label = match (entry, tab) {
        (Some(e), _) => e.label(),
        (None, Some(t)) => t.label(input.labels),
        (None, None) => row.name.as_str(),
    };
    let cwd = entry
        .map(|e| e.cwd.as_path())
        .or_else(|| tab.map(|t| t.cwd.as_path()));
    let folder = cwd.and_then(Path::file_name).and_then(|n| n.to_str());
    let active = row.tab == Some(input.active_tab);
    let attention = status == TabStatus::Attention;
    let progress = tab
        .and_then(|t| t.info())
        .and_then(|i| i.progress.as_ref())
        .filter(|p| !p.is_empty())
        .or_else(|| input.progress.of_name(&row.name));

    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), dock_row_h()), Sense::click());
    let pill = rect.shrink2(vec2(8.0, 1.0));
    let (view, view_label) = view_action(input.default_view, tab.map(tabs::Tab::view));
    let view_why = tab.and_then(|t| view_switch_reason(t, view, input.host_available));
    // Tabbed rows: view-switch + close-tab keys on hover (the old Open-tabs row's keys).
    let close_rect =
        Rect::from_center_size(pos2(pill.right() - 10.0, pill.center().y), vec2(20.0, 20.0));
    let view_rect = close_rect.translate(vec2(-23.0, 0.0));
    let close_btn = tab
        .is_some()
        .then(|| ui.interact(close_rect, resp.id.with("close"), Sense::click()));
    let view_btn = tab.is_some().then(|| {
        ui.interact(view_rect, resp.id.with("session-view"), Sense::click())
            .on_hover_text(view_why.unwrap_or(view_label))
    });
    // Tab-less rows: the attach key on hover.
    let attach_btn = tab.is_none().then(|| {
        ui.interact(close_rect, resp.id.with("attach"), Sense::click())
            .on_hover_text(match entry {
                Some(_) => "Open this session",
                None => "Attach to this live session",
            })
    });
    let over_key = close_btn.as_ref().is_some_and(egui::Response::hovered)
        || view_btn.as_ref().is_some_and(egui::Response::hovered)
        || attach_btn.as_ref().is_some_and(egui::Response::hovered);
    let ctx = ui.ctx().clone();
    let hov = ctx.animate_bool_with_time(
        resp.id.with("hov"),
        resp.hovered() || over_key || resp.context_menu_opened(),
        0.12,
    );
    let sel = ctx.animate_bool_with_time(resp.id.with("sel"), active, 0.18);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        paint_row_bg(p, pal, pill, (hov, sel));
        if attention {
            // Warn 9 % wash + warn border (the tab strip's attention wash is heavier: 13 %).
            p.rect(
                pill,
                row_radius(),
                pal.warn.gamma_multiply(0.09),
                Stroke::new(1.0, pal.warn.gamma_multiply(0.5)),
                egui::StrokeKind::Inside,
            );
        }
        let ic = pos2(pill.left() + 13.0, pill.center().y);
        if !tabs::paint_status(ui, ic, STATUS_SIZE, status) {
            theme::glow_dot(p, ic, 3.0, pal.ok, theme::GLOW_REST);
        }
        // Right side. Tabbed rows: accent window marker at rest, keys on hover. Tab-less
        // rows: dashed attach pill at rest, the Attach key on hover.
        let mut text_right;
        if tab.is_some() {
            if hov > 0.01 {
                paint_key_at(
                    p,
                    pal,
                    view_rect,
                    view_icon(view),
                    if view_why.is_none() {
                        KeyTone::Accent
                    } else {
                        KeyTone::Plain
                    },
                    hov,
                );
                paint_key_at(p, pal, close_rect, ICON_X, KeyTone::Plain, hov);
                text_right = view_rect.left() - 4.0;
            } else {
                paint_icon(
                    p,
                    pos2(pill.right() - 14.0, pill.center().y),
                    Align2::CENTER_CENTER,
                    ICON_APP_WINDOW,
                    13.0,
                    theme::mix(pal.weak, pal.accent, 0.5 + 0.5 * sel),
                );
                text_right = pill.right() - 26.0;
            }
        } else if hov > 0.01 {
            paint_key_at(p, pal, close_rect, ICON_PLAY, KeyTone::Ok, hov);
            text_right = close_rect.left() - 4.0;
        } else {
            let w = attach_pill(p, pal, pill);
            text_right = pill.right() - 8.0 - w;
        }
        // Live todos: a compact `3/7` badge before the right-hand marker (keys replace it).
        if hov <= 0.01
            && let Some(b) = progress.and_then(|pr| crate::progress_view::row_badge(p, pal, pr))
        {
            let left = text_right - b.width();
            b.paint_at(p, left, pill.center().y);
            text_right = left - 6.0;
        }
        let color = theme::mix(pal.text, pal.text_strong, sel.max(hov * 0.6));
        let mut job = LayoutJob::default();
        job.append(
            label,
            0.0,
            TextFormat {
                font_id: theme::font(12.0),
                color,
                ..Default::default()
            },
        );
        let mut folder_w = 0.0;
        if let Some(folder) = folder
            && hov <= 0.01
        {
            // Faint folder, max ~64 px; the keys take its place on hover.
            let mut name = folder.to_owned();
            let mut g = p.layout_no_wrap(name.clone(), theme::font(10.0), pal.weak);
            while g.size().x > 64.0 && name.len() > 4 {
                name.pop();
                g = p.layout_no_wrap(format!("{name}…"), theme::font(10.0), pal.weak);
            }
            folder_w = g.size().x + 8.0;
            job.append(
                &format!("  {name}"),
                0.0,
                TextFormat {
                    font_id: theme::font(10.0),
                    color: pal.weak,
                    ..Default::default()
                },
            );
        }
        let text_x = pill.left() + 26.0;
        job.wrap.max_width = (text_right - text_x - folder_w.min(20.0)).max(10.0);
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        let galley = p.layout_job(job);
        p.galley(
            pos2(text_x, pill.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
    }
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    let close_clicked = close_btn.as_ref().is_some_and(egui::Response::clicked);
    let view_clicked = view_btn.as_ref().is_some_and(egui::Response::clicked);
    let attach_clicked = attach_btn.as_ref().is_some_and(egui::Response::clicked);
    if close_clicked || (resp.middle_clicked() && tab.is_some()) {
        if let Some(i) = row.tab {
            out.actions.push(SidebarAction::CloseTab(i));
            out.interacted = true;
        }
    } else if view_clicked {
        if view_why.is_none() {
            if let Some(t) = tab {
                out.actions
                    .push(SidebarAction::SwitchView { uid: t.uid, view });
            }
            out.interacted = true;
        }
    } else if attach_clicked || resp.clicked() {
        out.actions.push(match (row.tab, entry) {
            (Some(i), _) => SidebarAction::ActivateTab(i),
            (None, Some(e)) => SidebarAction::Open(e.clone()),
            (None, None) => SidebarAction::AttachRunning {
                name: row.name.clone(),
                title: label.to_owned(),
            },
        });
        out.interacted = true;
    }
    let env = input.menu_env();
    theme::context_menu(&resp, 230.0, |ui| match (row.tab.zip(tab), entry) {
        (Some((i, tab)), _) => tab_menu(ui, i, tab, &env, out),
        (None, Some(e)) => session_menu(ui, e, true, true, &env, out),
        (None, None) => {
            if menu_item(ui, ICON_PLAY, "Open", None, true, false).clicked() {
                out.actions.push(SidebarAction::AttachRunning {
                    name: row.name.clone(),
                    title: label.to_owned(),
                });
                out.interacted = true;
                ui.close();
            }
            ui.separator();
            if menu_item(
                ui,
                ICON_CIRCLE_STOP,
                "Stop session process",
                None,
                true,
                true,
            )
            .clicked()
            {
                out.actions.push(SidebarAction::StopRunning {
                    name: row.name.clone(),
                    label: label.to_owned(),
                });
                ui.close();
            }
        }
    });
    hover_card(&resp, pal, row, label, status, entry, cwd, input, progress);
}

#[expect(
    clippy::too_many_arguments,
    reason = "one card, its facts are separate"
)]
fn hover_card(
    resp: &egui::Response,
    pal: &Palette,
    row: &Row,
    label: &str,
    status: TabStatus,
    entry: Option<&SessionEntry>,
    cwd: Option<&Path>,
    input: &SidebarInput<'_>,
    progress: Option<&deck_core::ctl::Progress>,
) {
    const KEY_W: f32 = 76.0;
    let mut head = card::Header::new(ICON_TERMINAL, label).tone(tips::status_tone(pal, status));
    if let Some(hint) = status.hint() {
        head = head.subtitle(hint);
    }
    let backend = input.running.owner_of(&row.name).map(kind_label);
    let open_tab = row.tab.is_some();
    card::hover(
        resp,
        390.0,
        false,
        &head,
        |ui| tips::status_badge(ui, status, None),
        |ui| {
            if let Some(dir) = cwd {
                card::kv(ui, KEY_W, "Folder", |ui| tips::path(ui, dir));
            }
            if let Some(e) = entry {
                card::kv(ui, KEY_W, "File", |ui| tips::path(ui, &e.path));
            }
            card::kv(ui, KEY_W, "Session", |ui| tips::mono(ui, &row.name));
            if let Some(b) = backend {
                card::kv(ui, KEY_W, "Backend", |ui| {
                    badge::show(ui, None, b, Hue::Orange, tips::VALUE);
                });
            }
            if open_tab {
                card::kv(ui, KEY_W, "Tab", |ui| {
                    badge::show(ui, Some(ICON_APP_WINDOW), "open", Hue::Blue, tips::VALUE);
                });
            }
            if let Some(p) = progress {
                crate::progress_view::kv_rows(ui, KEY_W, p);
            }
            card::footer_hints(
                ui,
                &[
                    ("click", if open_tab { "focus" } else { "open" }),
                    ("right-click", "actions"),
                ],
            );
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn row(name: &str, tab: Option<usize>, age: Option<u64>) -> Row {
        Row {
            name: name.to_owned(),
            tab,
            loc: age.map(|_| (0, 0)),
            modified: age.map(|s| SystemTime::UNIX_EPOCH + Duration::from_secs(s)),
        }
    }

    #[test]
    fn tabs_first_in_strip_order_then_newest_file_then_unmapped_by_name() {
        let mut rows = vec![
            row("omp-z", None, None),
            row("omp-old", None, Some(10)),
            row("omp-b", Some(2), Some(1)),
            row("omp-new", None, Some(99)),
            row("omp-a", Some(0), None),
            row("omp-c", None, None),
        ];
        order(&mut rows);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            ["omp-a", "omp-b", "omp-new", "omp-old", "omp-c", "omp-z"]
        );
    }

    #[test]
    fn equal_mtimes_fall_back_to_the_name() {
        let mut rows = vec![row("omp-b", None, Some(5)), row("omp-a", None, Some(5))];
        order(&mut rows);
        assert_eq!(rows[0].name, "omp-a");
    }
}
