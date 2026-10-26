//! The **Running** view of the session manager hub: a sortable table of every live session with
//! its memory, uptime and badges, per-row and bulk "Kill…", and the footer with the totals and
//! the tmux server line. Data and rules live in [`crate::running_model`].

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use egui::epaint::text::Galley;
use egui::{
    Align, Align2, Color32, CursorIcon, Id, Layout, RichText, Sense, Sides, Stroke, StrokeKind, Ui,
    pos2, vec2,
};

use crate::app::human_size;
use crate::badge::{self, Hue};
use crate::col_alloc::{self, CELL_PAD};
use crate::icons::{
    ICON_ACTIVITY, ICON_ARROW_DOWN, ICON_ARROW_UP, ICON_CIRCLE_STOP, ICON_CPU, ICON_TERMINAL,
};
use crate::paint_cache;
use crate::running_model::{RunCol, RunModel, RunRow};
use crate::sessions_model::{Check, plural, split_project_label};
use crate::sessions_view::{
    CHECK_W, HEAD_H, ROW_H, SessionsAction, ViewInput, action_button, galley_cell, lay,
    paint_checkbox, paint_row_bg, pill, project_cell,
};
use crate::theme::{self, Palette};
use crate::widgets::{Spin, empty_state, paint_icon};
use crate::{card, tips};
use deck_service::backend::kind_label;

const TITLE_MIN: f32 = 190.0;
const KILL_W: f32 = 84.0;
/// The Status column stops growing here; its pills clip beyond it.
const STATUS_MAX: f32 = 190.0;
/// Width a badge pill adds around its label (6 on each side) plus the gap after it.
const PILL_PAD: f32 = 16.0;

struct Col {
    col: Option<RunCol>,
    label: &'static str,
    tip: &'static str,
    /// Minimum width: the whole width of a column whose cells are not measured.
    w: f32,
    right: bool,
    /// Place in the order columns are dropped when the table is too narrow (`0` = never).
    drop: u8,
}

/// The columns that fit the table's width, with their measured widths.
struct Laid {
    cols: Vec<Col>,
    widths: Vec<f32>,
    total_w: f32,
}

fn columns() -> [Col; 7] {
    let c = |col, label, tip, w, right, drop| Col {
        col,
        label,
        tip,
        w,
        right,
        drop,
    };
    [
        c(
            Some(RunCol::Title),
            "Session",
            "Session title, else the terminal title, else the session name",
            0.0,
            false,
            0,
        ),
        c(
            Some(RunCol::Project),
            "Project",
            "Folder the session runs in",
            150.0,
            false,
            2,
        ),
        c(
            Some(RunCol::Backend),
            "Backend",
            "What keeps the session alive",
            0.0,
            false,
            1,
        ),
        c(
            Some(RunCol::Ram),
            "RAM",
            "Memory of the whole process tree (shared pages are split between their users)",
            0.0,
            true,
            0,
        ),
        c(
            Some(RunCol::Uptime),
            "Uptime",
            "Time since the session's process started",
            0.0,
            true,
            4,
        ),
        c(Some(RunCol::Status), "Status", "Badges", 0.0, false, 3),
        c(None, "", "", KILL_W + 8.0, true, 0),
    ]
}

/// Column widths for the rows in view, measured like the Sessions table's: recomputed only when
/// the rows' cells, the sort, the style or the width change.
fn layout(ui: &Ui, run: &RunModel, pal: &Palette, avail: f32) -> Arc<Laid> {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (
        paint_cache::style_key(ui.ctx(), pal),
        run.sort.col as u8,
        avail.to_bits(),
        run.any_killing(),
    )
        .hash(&mut h);
    for &i in run.order() {
        let r = &run.rows()[i];
        (
            &r.ram,
            &r.uptime,
            r.raw.backend as u8,
            r.flags.in_tab,
            r.flags.busy,
            r.flags.attention,
        )
            .hash(&mut h);
    }
    let key = h.finish();
    let id = Id::unique("running-col-layout");
    if let Some((k, laid)) = ui.data(|d| d.get_temp::<(u64, Arc<Laid>)>(id))
        && k == key
    {
        return laid;
    }
    let laid = Arc::new(measure_layout(ui.painter(), run, pal, avail));
    ui.data_mut(|d| d.insert_temp(id, (key, Arc::clone(&laid))));
    laid
}

fn measure_layout(p: &egui::Painter, run: &RunModel, pal: &Palette, avail: f32) -> Laid {
    let cols = columns();
    let sorted = run.sort.col;
    let mut nat: Vec<f32> = cols
        .iter()
        .map(|c| {
            let arrow = if c.col == Some(sorted) { 16.0 } else { 0.0 };
            (lay(p, c.label, 12.5, pal.weak).size().x + arrow + CELL_PAD).max(c.w)
        })
        .collect();
    let mut status_w = 0.0_f32;
    let killing = if run.any_killing() {
        lay(p, "Killing…", 11.0, pal.danger).size().x + PILL_PAD
    } else {
        0.0
    };
    for &i in run.order() {
        let row = &run.rows()[i];
        for (c, n) in cols.iter().zip(&mut nat) {
            let (text, size) = match c.col {
                Some(RunCol::Backend) => (kind_label(row.raw.backend), 12.5),
                Some(RunCol::Ram) => (row.ram.as_str(), 13.0),
                Some(RunCol::Uptime) => (row.uptime.as_str(), 12.5),
                _ => continue,
            };
            *n = n.max(lay(p, text, size, pal.text).size().x + CELL_PAD);
        }
        let pills: f32 = badge_items(row, pal)
            .map(|(text, _)| lay(p, text, 11.0, pal.text).size().x + PILL_PAD)
            .sum();
        status_w = status_w.max(pills + killing);
    }
    let specs: Vec<col_alloc::Spec> = cols
        .iter()
        .zip(&mut nat)
        .map(|(c, n)| {
            if c.col == Some(RunCol::Status) {
                *n = n.max(status_w.min(STATUS_MAX) + CELL_PAD);
            }
            col_alloc::Spec {
                natural: *n,
                drop: c.drop,
            }
        })
        .collect();
    let alloc = col_alloc::allocate(&specs, 0, CHECK_W, avail, TITLE_MIN);
    let mut out = Laid {
        cols: Vec::with_capacity(cols.len()),
        widths: Vec::with_capacity(cols.len()),
        total_w: alloc.total,
    };
    for (c, w) in cols.into_iter().zip(alloc.widths) {
        if w > 0.0 {
            out.cols.push(c);
            out.widths.push(w);
        }
    }
    out
}

pub fn table(
    ui: &mut Ui,
    run: &mut RunModel,
    input: &ViewInput<'_>,
    out: &mut Vec<SessionsAction>,
) {
    let pal = Palette::of_ui(ui);
    if !run.loaded() {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() / 2.0 - 30.0);
            ui.add(Spin::new(22.0, pal.accent));
            ui.add_space(8.0);
            ui.label(RichText::new("Looking for running sessions…").color(pal.weak));
        });
        return;
    }
    if run.rows().is_empty() {
        empty_state(
            ui,
            ICON_ACTIVITY,
            pal.accent,
            "No running sessions",
            "omp sessions that OmpDeck keeps alive show up here with the memory they use.",
        );
        return;
    }
    let laid = layout(ui, run, pal, ui.available_width());
    let (cols, widths, total_w) = (&laid.cols, &laid.widths, laid.total_w);

    egui::ScrollArea::horizontal()
        .id_salt("running-table-h")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_min_width(total_w);
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            header(ui, run, cols, widths, total_w);
            ui.add_space(4.0);
            rows(ui, run, cols, widths, total_w, input, out);
        });
}

fn header(ui: &mut Ui, run: &mut RunModel, cols: &[Col], widths: &[f32], total_w: f32) {
    let pal = Palette::of_ui(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(total_w, HEAD_H), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, theme::rad(6, 9), pal.raised);
    p.rect_stroke(
        rect,
        theme::rad(6, 9),
        Stroke::new(1.0, pal.stroke),
        StrokeKind::Inside,
    );
    let check_rect = egui::Rect::from_min_size(rect.min, vec2(CHECK_W, HEAD_H));
    let resp = ui.interact(check_rect, Id::unique("running-check-all"), Sense::click());
    paint_checkbox(
        ui.painter(),
        check_rect.center(),
        run.header_check(),
        resp.hovered(),
        pal,
    );
    if resp
        .on_hover_text("Select / clear every running session")
        .on_hover_cursor(CursorIcon::PointingHand)
        .clicked()
    {
        run.toggle_all();
    }
    let mut x = rect.left() + CHECK_W;
    for (c, &w) in cols.iter().zip(widths) {
        let cell = egui::Rect::from_min_size(pos2(x, rect.top()), vec2(w, HEAD_H));
        x += w;
        let Some(col) = c.col else {
            continue;
        };
        let resp = ui.interact(cell, Id::unique(("running-head", c.label)), Sense::click());
        let active = run.sort.col == col;
        let hov = ui
            .ctx()
            .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.08);
        let p = ui.painter();
        if hov > 0.0 {
            p.rect_stroke(
                cell.shrink(1.0),
                theme::rad(5, 8),
                Stroke::new(1.5, theme::hover_stroke(pal).gamma_multiply(hov)),
                StrokeKind::Inside,
            );
        }
        let ink = if active { pal.text_strong } else { pal.weak };
        let arrow_w = if active { 16.0 } else { 0.0 };
        let inner = cell.shrink2(vec2(8.0, 0.0));
        let tp = p.with_clip_rect(inner);
        let g = tp.layout_no_wrap(c.label.to_owned(), theme::font(12.5), ink);
        let text_w = g.size().x;
        let start_x = if c.right {
            inner.right() - text_w - arrow_w
        } else {
            inner.left()
        };
        tp.galley(pos2(start_x, cell.center().y - g.size().y / 2.0), g, ink);
        if active {
            paint_icon(
                p,
                pos2(start_x + text_w + 9.0, cell.center().y),
                Align2::CENTER_CENTER,
                if run.sort.desc {
                    ICON_ARROW_DOWN
                } else {
                    ICON_ARROW_UP
                },
                13.0,
                pal.accent,
            );
        }
        if resp
            .on_hover_text(format!("{} — click to sort", c.tip))
            .on_hover_cursor(CursorIcon::PointingHand)
            .clicked()
        {
            run.sort.click(col);
            run.mark_order_dirty();
        }
    }
}

fn rows(
    ui: &mut Ui,
    run: &mut RunModel,
    cols: &[Col],
    widths: &[f32],
    total_w: f32,
    input: &ViewInput<'_>,
    out: &mut Vec<SessionsAction>,
) {
    let pal = Palette::of_ui(ui);
    let shift = ui.input(|i| i.modifiers.shift);
    let n = run.order().len();
    let can_kill = input.can_stop && !run.any_killing();
    let style = paint_cache::style_key(ui.ctx(), pal);
    let mut click: Option<usize> = None;
    egui::ScrollArea::vertical()
        .id_salt("running-table-v")
        .auto_shrink([false, false])
        .show_rows(ui, ROW_H, n, |ui, range| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            ui.set_min_width(total_w);
            for pos in range {
                let Some(&idx) = run.order().get(pos) else {
                    continue;
                };
                let row = &run.rows()[idx];
                let selected = run.is_selected(row.name());
                let (rect, resp) = ui.allocate_exact_size(vec2(total_w, ROW_H), Sense::click());
                let inner = rect.shrink2(vec2(0.0, 1.0));
                let check_rect =
                    egui::Rect::from_min_size(inner.min, vec2(CHECK_W, inner.height()));
                let check = ui.interact(check_rect, resp.id.with("check"), Sense::click());
                let kill_rect = egui::Rect::from_min_size(
                    pos2(inner.right() - KILL_W - 8.0, inner.center().y - 12.0),
                    vec2(KILL_W, 24.0),
                );
                let kill = ui.interact(
                    kill_rect,
                    resp.id.with("kill"),
                    if can_kill {
                        Sense::click()
                    } else {
                        Sense::hover()
                    },
                );
                let hov = ui.ctx().animate_bool_with_time(
                    resp.id.with("hov"),
                    resp.hovered() || check.hovered() || kill.hovered(),
                    0.08,
                );
                if ui.is_rect_visible(rect) {
                    let p = ui.painter();
                    let r = theme::rad(5, 8);
                    if selected {
                        p.rect_filled(inner, r, theme::accent_fill(pal, 0.10));
                    }
                    paint_row_bg(p, inner, r, pal, selected, hov);
                    if !selected && hov == 0.0 {
                        p.hline(
                            inner.x_range().shrink(6.0),
                            rect.bottom(),
                            Stroke::new(1.0, theme::mix(pal.surface, pal.stroke, 0.5)),
                        );
                    }
                    let state = if selected { Check::All } else { Check::None };
                    paint_checkbox(p, check_rect.center(), state, check.hovered(), pal);
                    paint_row(
                        ui,
                        row,
                        inner,
                        cols,
                        widths,
                        run.is_killing(row.name()),
                        style,
                        pal,
                    );
                    paint_kill_button(ui, kill_rect, &kill, can_kill, run.is_killing(row.name()));
                }
                if kill.clicked() {
                    out.push(SessionsAction::KillRunning(vec![row.name().to_owned()]));
                } else if check.clicked() || resp.clicked() {
                    click = Some(pos);
                }
                let pointer = ui.input(|i| i.pointer.hover_pos());
                if let Some(at) = pointer
                    && inner.contains(at)
                    && !kill_rect.contains(at)
                {
                    let col = column_at(inner.left() + CHECK_W, cols, widths, at.x);
                    tip(&resp, row, col);
                }
            }
        });
    if let Some(pos) = click {
        run.click_row(pos, shift);
    }
}

fn column_at(mut x: f32, cols: &[Col], widths: &[f32], at: f32) -> Option<RunCol> {
    for (c, &w) in cols.iter().zip(widths) {
        if at >= x && at < x + w {
            return c.col;
        }
        x += w;
    }
    None
}

/// Badges of a row, in paint order: (label, colour).
fn badge_items(row: &RunRow, pal: &Palette) -> impl Iterator<Item = (&'static str, Color32)> {
    [
        (row.flags.in_tab, "Open in tab", pal.accent),
        (row.flags.busy, "Busy", pal.warn),
        (row.flags.attention, "Attention", pal.warn),
    ]
    .into_iter()
    .filter(|(on, _, _)| *on)
    .map(|(_, text, color)| (text, color))
}

/// Cell text laid out once per content change: title, project, backend, RAM, uptime, badges.
fn row_galleys(p: &egui::Painter, row: &RunRow, pal: &Palette) -> Vec<Arc<Galley>> {
    let ram_ink = if row.raw.memory.is_some() {
        pal.text_strong
    } else {
        pal.weak
    };
    let mut v = vec![
        lay(p, &row.title, 13.5, pal.text_strong),
        lay(p, split_project_label(&row.project).0, 12.5, pal.text),
        lay(p, kind_label(row.raw.backend), 12.5, pal.weak),
        lay(p, &row.ram, 13.0, ram_ink),
        lay(p, &row.uptime, 12.5, pal.text),
    ];
    v.extend(badge_items(row, pal).map(|(text, color)| lay(p, text, 11.0, color)));
    v
}

fn row_key(style: u64, row: &RunRow) -> u64 {
    paint_cache::with(
        style,
        (
            &row.title,
            &row.project,
            &row.ram,
            &row.uptime,
            row.raw.backend as u8,
            row.flags.in_tab,
            row.flags.busy,
            row.flags.attention,
            row.raw.memory.is_some(),
        ),
    )
}

#[allow(clippy::too_many_arguments)]
fn paint_row(
    ui: &Ui,
    row: &RunRow,
    rect: egui::Rect,
    cols: &[Col],
    widths: &[f32],
    killing: bool,
    style: u64,
    pal: &Palette,
) {
    let p = ui.painter();
    let mut cache = row.paint.borrow_mut();
    let g = cache.get(row_key(style, row), || row_galleys(p, row, pal));
    let mut x = rect.left() + CHECK_W;
    for (c, &w) in cols.iter().zip(widths) {
        let cell = egui::Rect::from_min_size(pos2(x, rect.top()), vec2(w, rect.height()));
        x += w;
        let inner = cell.shrink2(vec2(8.0, 0.0));
        match c.col {
            Some(RunCol::Title) => galley_cell(p, inner, &g[0], false),
            Some(RunCol::Project) => project_cell(p, inner, &g[1], &row.project, pal),
            Some(RunCol::Backend) => galley_cell(p, inner, &g[2], false),
            Some(RunCol::Ram) => galley_cell(p, inner, &g[3], true),
            Some(RunCol::Uptime) => galley_cell(p, inner, &g[4], true),
            Some(RunCol::Status) => badges(p, inner, row, &g[5..], killing, pal),
            None => {}
        }
    }
}

fn badges(
    p: &egui::Painter,
    rect: egui::Rect,
    row: &RunRow,
    galleys: &[Arc<Galley>],
    killing: bool,
    pal: &Palette,
) {
    let tp = p.with_clip_rect(rect);
    let mut x = rect.left();
    if killing {
        let g = lay(&tp, "Killing…", 11.0, pal.danger);
        x = pill(&tp, rect, pal, x, &g, pal.danger);
    }
    for ((_, color), g) in badge_items(row, pal).zip(galleys) {
        if x > rect.right() {
            break;
        }
        x = pill(&tp, rect, pal, x, g, color);
    }
}

fn paint_kill_button(
    ui: &Ui,
    rect: egui::Rect,
    resp: &egui::Response,
    enabled: bool,
    killing: bool,
) {
    let pal = Palette::of_ui(ui);
    let hov =
        ui.ctx()
            .animate_bool_with_time(resp.id.with("khov"), resp.hovered() && enabled, 0.08);
    let alpha = if enabled { 1.0 } else { 0.4 };
    let p = ui.painter();
    let r = theme::rad(6, 9);
    p.rect_filled(rect, r, pal.raised.gamma_multiply(alpha));
    p.rect_stroke(
        rect,
        r,
        Stroke::new(
            1.0 + 0.5 * hov,
            theme::mix(pal.danger.gamma_multiply(0.55), pal.danger, hov).gamma_multiply(alpha),
        ),
        StrokeKind::Inside,
    );
    let label = if killing { "Killing…" } else { "Kill…" };
    let g = p.layout_no_wrap(
        label.to_owned(),
        theme::font(12.5),
        pal.danger.gamma_multiply(alpha),
    );
    let at = pos2(
        rect.center().x - g.size().x / 2.0,
        rect.center().y - g.size().y / 2.0,
    );
    p.galley(at, g, pal.danger);
    if enabled {
        resp.clone()
            .on_hover_text("Kill this session's process (the session file stays)")
            .on_hover_cursor(CursorIcon::PointingHand);
    }
}

/// Hover card of a row (at the pointer): the memory breakdown over the RAM column, else the
/// session's backend name, uptime, folder and files.
fn tip(resp: &egui::Response, row: &RunRow, col: Option<RunCol>) {
    const KEY_W: f32 = 84.0;
    if col == Some(RunCol::Ram) {
        card::hover(
            resp,
            300.0,
            true,
            &card::Header::new(ICON_CPU, "Memory").subtitle(&row.title),
            |ui| {
                if let Some(m) = row.raw.memory {
                    badge::show(ui, None, &human_size(m.bytes), Hue::Teal, tips::VALUE);
                }
            },
            |ui| match row.raw.memory {
                Some(m) => {
                    card::kv(ui, KEY_W, "Processes", |ui| {
                        let n = m.processes.to_string();
                        badge::show(ui, None, &n, Hue::Amber, tips::VALUE);
                    });
                    if let Some(pid) = row.raw.pid {
                        card::kv(ui, KEY_W, "Pid", |ui| {
                            badge::show(ui, None, &pid.to_string(), Hue::Blue, tips::VALUE);
                        });
                    }
                    tips::note(ui, "Shared pages are split between their users");
                }
                None => tips::note(ui, "Memory could not be read"),
            },
        );
        return;
    }
    card::hover(
        resp,
        380.0,
        true,
        &card::Header::new(ICON_TERMINAL, &row.title),
        |ui| {
            if row.flags.attention {
                badge::show(ui, None, "needs input", Hue::Amber, tips::VALUE);
            } else if row.flags.busy {
                badge::show(ui, None, "working", Hue::Blue, tips::VALUE);
            } else {
                badge::show(ui, None, "running", Hue::Green, tips::VALUE);
            }
        },
        |ui| {
            card::kv(ui, KEY_W, "Session", |ui| tips::mono(ui, row.name()));
            card::kv(ui, KEY_W, "Uptime", |ui| {
                badge::show(ui, None, &row.uptime, Hue::Amber, tips::VALUE);
            });
            if row.flags.in_tab {
                card::kv(ui, KEY_W, "Shown", |ui| {
                    badge::show(ui, None, "open in a tab", Hue::Violet, tips::VALUE);
                });
            }
            if let Some(cwd) = &row.raw.cwd {
                card::kv(ui, KEY_W, "Folder", |ui| tips::path(ui, cwd));
            }
            for f in &row.raw.files {
                card::kv(ui, KEY_W, "File", |ui| tips::path(ui, f));
            }
            card::footer_hints(ui, &[("click", "select"), ("shift+click", "range")]);
        },
    );
}

/// Totals / selection on the left, the bulk kill button on the right.
pub fn footer(
    ui: &mut Ui,
    run: &mut RunModel,
    input: &ViewInput<'_>,
    out: &mut Vec<SessionsAction>,
    foot_h: f32,
) {
    let pal = Palette::of_ui(ui);
    let plan = run.selection_plan();
    let names = run.selected_names();
    let killing = run.any_killing();
    let mut clear = false;
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), foot_h),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            Sides::new().show(
                ui,
                |ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    if killing {
                        ui.add(Spin::new(17.0, pal.accent));
                        ui.label(theme::strong("Killing…").color(pal.text_strong));
                    } else if plan.count > 0 {
                        ui.label(
                            theme::strong(format!(
                                "{} selected · frees {}",
                                plan.count,
                                human_size(plan.bytes)
                            ))
                            .color(pal.text_strong),
                        );
                        if plan.busy > 0 {
                            ui.label(
                                RichText::new(format!("{} working", plan.busy))
                                    .size(12.5)
                                    .color(pal.warn),
                            );
                        }
                        if plan.in_tab > 0 {
                            ui.label(
                                RichText::new(format!("{} open in tabs", plan.in_tab))
                                    .size(12.5)
                                    .color(pal.warn),
                            );
                        }
                        if ui
                            .add(
                                egui::Button::new(RichText::new("Clear").color(pal.weak))
                                    .frame(false),
                            )
                            .on_hover_cursor(CursorIcon::PointingHand)
                            .clicked()
                        {
                            clear = true;
                        }
                    } else {
                        ui.label(
                            theme::strong(format!(
                                "{} · {} in total",
                                plural(run.rows().len(), "running session", "running sessions"),
                                human_size(run.total_bytes())
                            ))
                            .color(pal.text_strong),
                        );
                    }
                    if run.tmux() {
                        let server = run.tmux_server().map_or_else(
                            || "tmux server: not running".to_owned(),
                            |m| format!("tmux server: {}", human_size(m.bytes)),
                        );
                        ui.label(RichText::new(server).size(12.5).color(pal.weak))
                            .on_hover_text(
                                "One process shared by every tmux session; killing sessions \
                                 does not free it",
                            );
                    }
                },
                |ui| {
                    let on = input.can_stop && !killing && plan.count > 0;
                    if action_button(ui, ICON_CIRCLE_STOP, "Kill…", on, true)
                        .on_hover_text("Kill the selected sessions' processes")
                        .clicked()
                    {
                        out.push(SessionsAction::KillRunning(names.clone()));
                    }
                },
            );
        },
    );
    if clear {
        run.clear_selection();
    }
}
