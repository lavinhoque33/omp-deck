//! The **Checkpoints** section of the Changes panel: the repo snapshots OmpDeck's extension took
//! before each agent turn's first edit (newest first), a diff-stat preview of what rolling back
//! would change, and "Roll back to here". The list comes from the gateway's
//! `Topic::Checkpoints(session file)`; the app glue lives in `app/changes.rs`.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use deck_core::checkpoints::{Checkpoint, CheckpointKind, FileDelta};
use deck_service::gateway::protocol::CheckpointsView;
use egui::{Align2, CursorIcon, Rect, RichText, ScrollArea, Sense, Stroke, StrokeKind, Ui, vec2};

use crate::badge::{Badge, Hue};
use crate::changes_view::ChangesAction;
use crate::icons::{ICON_EYE, ICON_FILE_DIFF, ICON_HISTORY, ICON_SHIELD_CHECK, ICON_UNDO_2};
use crate::sidebar::format_age;
use crate::theme::{self, Palette};
use crate::widgets::{Spin, empty_state, menu_item, paint_icon};

/// Height of one checkpoint row.
const ROW_H: f32 = 44.0;
/// Height of one preview row.
const DELTA_H: f32 = 22.0;
/// Characters of a prompt shown in a row before the ellipsis.
const PROMPT_CHARS: usize = 90;

/// What rolling back to a checkpoint would change, as last asked of the gateway.
#[derive(Debug, Default)]
pub enum Preview {
    #[default]
    Idle,
    Loading(u32),
    /// The first rows (the gateway caps them) and the full changed-file count.
    Ready(u32, Vec<FileDelta>, u32),
    Failed(u32, String),
}

impl Preview {
    pub fn seq(&self) -> Option<u32> {
        match self {
            Preview::Idle => None,
            Preview::Loading(s) | Preview::Ready(s, ..) | Preview::Failed(s, _) => Some(*s),
        }
    }
}

/// The section's state, kept in the Changes panel's view state.
#[derive(Default)]
pub struct State {
    /// The session file whose checkpoints are listed (`None`: the active tab has none).
    pub session: Option<PathBuf>,
    /// The gateway's list (`None` until it arrived).
    pub view: Option<CheckpointsView>,
    pub preview: Preview,
    /// Why "Roll back" is unavailable right now (a turn runs, the session is not live …).
    pub blocked: Option<String>,
    /// A restore to this checkpoint is in flight.
    pub restoring: Option<u32>,
    /// Checkpoints are switched off in Settings (new sessions take none).
    pub disabled: bool,
}

impl State {
    /// Points the section at `session`; a different file drops the list and the preview.
    pub fn retarget(&mut self, session: Option<PathBuf>) -> bool {
        if self.session == session {
            return false;
        }
        self.session = session;
        self.view = None;
        self.preview = Preview::Idle;
        self.restoring = None;
        true
    }
}

/// The row's text: "Before turn: <prompt>" (one line, cut).
pub fn row_title(c: &Checkpoint) -> String {
    let prompt = c.prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    let what = match c.kind {
        CheckpointKind::Turn => "Before turn",
        CheckpointKind::Rollback => "Before rollback",
    };
    if prompt.is_empty() {
        return what.to_owned();
    }
    let cut: String = prompt.chars().take(PROMPT_CHARS).collect();
    if cut.len() < prompt.len() {
        format!("{what}: {cut}…")
    } else {
        format!("{what}: {cut}")
    }
}

/// The confirmation text of a rollback that changes `n` files.
pub fn confirm_body(c: &Checkpoint, n: usize) -> String {
    let files = match n {
        0 => "No files differ from this checkpoint".to_owned(),
        1 => "1 file will change".to_owned(),
        n => format!("{n} files will change"),
    };
    format!(
        "{files}: the working tree is reset to how it was {}.\n\nA safety checkpoint of the current state is taken first, so this rollback can be undone from the Checkpoints list. Staged changes in git's index are kept.",
        match c.kind {
            CheckpointKind::Turn => "before that turn",
            CheckpointKind::Rollback => "before that rollback",
        }
    )
}

fn kind_badge(p: &egui::Painter, pal: &Palette, kind: CheckpointKind) -> Badge {
    match kind {
        CheckpointKind::Turn => Badge::single(p, None, "Turn", Hue::Blue.color(pal), 10.5),
        CheckpointKind::Rollback => Badge::single(
            p,
            Some(ICON_SHIELD_CHECK),
            "Safety copy",
            Hue::Amber.color(pal),
            10.5,
        ),
    }
}

/// The section body (below the panel's header).
pub fn show(ui: &mut Ui, st: &State, out: &mut Vec<ChangesAction>) {
    let pal = Palette::of_ui(ui);
    if st.session.is_none() {
        empty_state(
            ui,
            ICON_HISTORY,
            pal.weak,
            "No session file",
            "Checkpoints belong to an omp session: open a session tab to see its checkpoints.",
        );
        return;
    }
    let Some(view) = &st.view else {
        ui.horizontal(|ui| {
            ui.add(Spin::new(16.0, pal.accent_b));
            ui.label(RichText::new("Reading checkpoints…").color(pal.weak));
        });
        return;
    };
    if let Some(e) = &view.error {
        ui.colored_label(pal.danger, "Could not read the checkpoints");
        ui.label(RichText::new(e).small().color(pal.weak));
        return;
    }
    if view.repo.is_none() {
        empty_state(
            ui,
            ICON_HISTORY,
            pal.weak,
            "Not a git repository",
            "Checkpoints are git snapshots: this session's folder is not inside a repository.",
        );
        return;
    }
    if view.items.is_empty() {
        let sub = if st.disabled {
            "Checkpoints are off (Settings → General → Agent tools). Sessions started after turning them on snapshot the repo before each turn's first edit."
        } else {
            "Before the agent's first edit of each turn, OmpDeck snapshots the repo here; roll back to any of them."
        };
        empty_state(ui, ICON_HISTORY, pal.accent, "No checkpoints yet", sub);
        return;
    }
    if let Some(why) = &st.blocked {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(why).small().color(pal.warn));
        });
    }
    let now = SystemTime::now();
    let list_h = (ui.available_height() * 0.5).max(ROW_H * 2.0);
    let rows_h = (view.items.len() as f32 * (ROW_H + 4.0)).min(list_h);
    ScrollArea::vertical()
        .id_salt("checkpoints-list")
        .max_height(rows_h)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            for c in &view.items {
                row(ui, st, c, now, out);
            }
        });
    // Ages move with the clock.
    ui.ctx().request_repaint_after(Duration::from_secs(30));
    ui.add_space(8.0);
    preview(ui, st, view);
}

fn row(ui: &mut Ui, st: &State, c: &Checkpoint, now: SystemTime, out: &mut Vec<ChangesAction>) {
    let pal = Palette::of_ui(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    let selected = st.preview.seq() == Some(c.seq);
    let restoring = st.restoring == Some(c.seq);
    let can_roll = st.blocked.is_none() && st.restoring.is_none();
    let roll_tip = st.blocked.as_deref().unwrap_or(if st.restoring.is_some() {
        "A rollback is running"
    } else {
        "Roll the working tree back to this checkpoint (asks first; a safety copy is taken)"
    });
    let hover =
        ui.ctx()
            .animate_bool_with_time(resp.id.with("hov"), resp.hovered() || selected, 0.1);
    let btn_w = 24.0;
    let roll_rect = Rect::from_center_size(
        egui::pos2(rect.right() - 6.0 - btn_w / 2.0, rect.center().y),
        vec2(btn_w, btn_w),
    );
    let eye_rect = roll_rect.translate(vec2(-btn_w - 2.0, 0.0));
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let r = theme::ui_rad(6);
        p.rect_filled(rect, r, theme::hover_fill(pal).gamma_multiply(0.7 * hover));
        p.rect_stroke(
            rect,
            r,
            Stroke::new(
                1.0,
                theme::mix(
                    pal.stroke,
                    pal.accent,
                    if selected { 1.0 } else { hover * 0.8 },
                ),
            ),
            StrokeKind::Inside,
        );
        let icon = match c.kind {
            CheckpointKind::Turn => ICON_HISTORY,
            CheckpointKind::Rollback => ICON_SHIELD_CHECK,
        };
        paint_icon(
            p,
            egui::pos2(rect.left() + 16.0, rect.center().y),
            Align2::CENTER_CENTER,
            icon,
            15.0,
            pal.glyph,
        );
        let text_x = rect.left() + 32.0;
        let text_right = if resp.hovered() || selected {
            eye_rect.left() - 6.0
        } else {
            rect.right() - 8.0
        };
        let title = p.layout_no_wrap(row_title(c), theme::font(12.5), pal.text_strong);
        let clip = Rect::from_min_max(
            egui::pos2(text_x, rect.top()),
            egui::pos2(text_right, rect.bottom()),
        );
        p.with_clip_rect(clip)
            .galley(egui::pos2(text_x, rect.top() + 6.0), title, pal.text_strong);
        let at = SystemTime::UNIX_EPOCH + Duration::from_millis(c.at_unix_ms);
        let age = format_age(now, at);
        let age = if age == "now" {
            "just now".to_owned()
        } else {
            format!("{age} ago")
        };
        let cy = rect.bottom() - 11.0;
        let b = kind_badge(p, pal, c.kind);
        b.paint_at(p, text_x, cy);
        let age_g = p.layout_no_wrap(age, theme::label_font(11.0), pal.weak);
        let age_y = cy - age_g.size().y / 2.0;
        p.galley(egui::pos2(text_x + b.width() + 6.0, age_y), age_g, pal.weak);
        if restoring {
            Spin::new(14.0, pal.accent).paint_at(ui, roll_rect.center());
        }
    }
    // Hover buttons: Preview, Roll back (the same actions as the context menu).
    let mut clicked_btn = false;
    if resp.hovered() || selected || ui.rect_contains_pointer(rect) {
        if hover_button(
            ui,
            eye_rect,
            resp.id.with("eye"),
            ICON_EYE,
            "Preview what changes",
            true,
        ) {
            out.push(ChangesAction::PreviewCheckpoint(c.seq));
            clicked_btn = true;
        }
        if !restoring
            && hover_button(
                ui,
                roll_rect,
                resp.id.with("roll"),
                ICON_UNDO_2,
                roll_tip,
                can_roll,
            )
        {
            out.push(ChangesAction::RollBack(c.seq));
            clicked_btn = true;
        }
    }
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    if resp.clicked() && !clicked_btn {
        out.push(ChangesAction::PreviewCheckpoint(c.seq));
    }
    theme::context_menu(&resp, 220.0, |ui| {
        if menu_item(ui, ICON_EYE, "Preview", None, true, false).clicked() {
            out.push(ChangesAction::PreviewCheckpoint(c.seq));
            ui.close();
        }
        let roll = menu_item(ui, ICON_UNDO_2, "Roll back to here…", None, can_roll, true)
            .on_disabled_hover_text(roll_tip);
        if roll.clicked() {
            out.push(ChangesAction::RollBack(c.seq));
            ui.close();
        }
    });
}

/// A row's icon button: hover pops it, disabled ones still say why.
fn hover_button(
    ui: &mut Ui,
    rect: Rect,
    id: egui::Id,
    icon: crate::icons::Icon,
    tip: &str,
    enabled: bool,
) -> bool {
    let pal = Palette::of_ui(ui);
    let resp = ui.interact(rect, id, Sense::click());
    let hov = resp.hovered() && enabled;
    let p = ui.painter();
    p.rect(
        rect,
        theme::ui_rad(5),
        if hov {
            theme::hover_fill(pal).gamma_multiply(1.6)
        } else {
            pal.surface
        },
        Stroke::new(1.0, if hov { pal.accent } else { pal.stroke }),
        StrokeKind::Inside,
    );
    paint_icon(
        p,
        rect.center(),
        Align2::CENTER_CENTER,
        icon,
        14.0,
        if enabled {
            pal.glyph
        } else {
            pal.weak.gamma_multiply(0.5)
        },
    );
    let resp = resp.on_hover_text(tip);
    if enabled {
        resp.on_hover_cursor(CursorIcon::PointingHand).clicked()
    } else {
        false
    }
}

/// The diff-stat preview of the selected checkpoint.
fn preview(ui: &mut Ui, st: &State, view: &CheckpointsView) {
    let pal = Palette::of_ui(ui);
    let Some(seq) = st.preview.seq() else {
        ui.label(
            RichText::new("Click a checkpoint to preview what rolling back would change.")
                .small()
                .color(pal.weak),
        );
        return;
    };
    let title = view
        .items
        .iter()
        .find(|c| c.seq == seq)
        .map_or_else(|| format!("Checkpoint {seq}"), row_title);
    ui.horizontal(|ui| {
        ui.label(crate::widgets::icon_text(ICON_FILE_DIFF, 14.0, pal.glyph));
        ui.add(
            egui::Label::new(
                theme::strong("Roll back would change")
                    .size(12.5)
                    .color(pal.text_strong),
            )
            .truncate(),
        );
    });
    ui.add(egui::Label::new(RichText::new(title).small().color(pal.weak)).truncate());
    match &st.preview {
        Preview::Idle => {}
        Preview::Loading(_) => {
            ui.horizontal(|ui| {
                ui.add(Spin::new(14.0, pal.accent_b));
                ui.label(RichText::new("Comparing…").small().color(pal.weak));
            });
        }
        Preview::Failed(_, e) => {
            ui.colored_label(pal.danger, RichText::new(e).small());
        }
        Preview::Ready(_, deltas, _) if deltas.is_empty() => {
            ui.label(
                RichText::new("Nothing differs: the working tree matches this checkpoint.")
                    .small()
                    .color(pal.weak),
            );
        }
        Preview::Ready(_, deltas, total) => {
            let more = (*total as usize).saturating_sub(deltas.len());
            if more > 0 {
                ui.label(
                    RichText::new(format!(
                        "{total} files differ; the first {} are listed",
                        deltas.len()
                    ))
                    .small()
                    .color(pal.weak),
                );
            }
            ScrollArea::vertical()
                .id_salt("checkpoint-preview")
                .auto_shrink([false, false])
                .show_rows(ui, DELTA_H, deltas.len(), |ui, range| {
                    for d in &deltas[range] {
                        delta_row(ui, d);
                    }
                });
        }
    }
}

fn delta_row(ui: &mut Ui, d: &FileDelta) {
    let pal = Palette::of_ui(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), DELTA_H), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let p = ui.painter();
    let tone = match d.status {
        'A' => pal.ok,
        'D' => pal.danger,
        'R' | 'C' => Hue::Blue.color(pal),
        _ => pal.warn,
    };
    let cy = rect.center().y;
    p.text(
        egui::pos2(rect.left() + 8.0, cy),
        Align2::CENTER_CENTER,
        d.status.to_string(),
        theme::label_font(11.5),
        tone,
    );
    let mut right = rect.right() - 4.0;
    let stat = |n: Option<u32>, sign: char| n.filter(|&n| n > 0).map(|n| format!("{sign}{n}"));
    for (text, color) in [
        (stat(d.removed, '−'), pal.danger),
        (stat(d.added, '+'), pal.ok),
    ] {
        if let Some(t) = text {
            let g = p.layout_no_wrap(t, theme::label_font(11.0), color);
            right -= g.size().x;
            p.galley(egui::pos2(right, cy - g.size().y / 2.0), g, color);
            right -= 6.0;
        }
    }
    let path = match &d.old_path {
        Some(old) if old != &d.path => format!("{old} → {}", d.path),
        _ => d.path.clone(),
    };
    let g = p.layout_no_wrap(path, theme::label_font(11.5), pal.text);
    let clip = Rect::from_min_max(
        egui::pos2(rect.left() + 20.0, rect.top()),
        egui::pos2(right, rect.bottom()),
    );
    p.with_clip_rect(clip).galley(
        egui::pos2(rect.left() + 20.0, cy - g.size().y / 2.0),
        g,
        pal.text,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cp(prompt: &str, kind: CheckpointKind) -> Checkpoint {
        Checkpoint {
            seq: 1,
            commit: "abc".into(),
            at_unix_ms: 0,
            prompt: prompt.into(),
            entry: None,
            kind,
        }
    }

    #[test]
    fn row_titles_are_one_cut_line() {
        assert_eq!(
            row_title(&cp("fix\n  the  bug", CheckpointKind::Turn)),
            "Before turn: fix the bug"
        );
        assert_eq!(row_title(&cp("", CheckpointKind::Turn)), "Before turn");
        let long = "é".repeat(200);
        let t = row_title(&cp(&long, CheckpointKind::Turn));
        assert!(t.ends_with('…'));
        assert_eq!(t.chars().count(), "Before turn: ".len() + PROMPT_CHARS + 1);
    }

    #[test]
    fn retargeting_to_another_session_drops_list_and_preview() {
        let mut st = State {
            session: Some("/a".into()),
            view: Some(CheckpointsView::default()),
            preview: Preview::Loading(3),
            ..State::default()
        };
        assert!(!st.retarget(Some("/a".into())));
        assert!(st.view.is_some());
        assert!(st.retarget(Some("/b".into())));
        assert!(st.view.is_none());
        assert_eq!(st.preview.seq(), None);
    }
}
