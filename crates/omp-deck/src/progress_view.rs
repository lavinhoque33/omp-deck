//! Live todo / goal progress of a session (`ControllerInfo::progress`, from omp's live-control
//! snapshot): the compact `done/total` text, the task and goal status looks, the hover rows of
//! the session rows, the status bar's Progress card and the tab's progress line. Shared by the
//! status bar, the tab strip, the sidebar's session rows and Running dock, and the agents
//! dashboard.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use deck_core::ctl::{GoalInfo, GoalStatus, Progress, TodoStatus, TodoTask};
use deck_service::gateway::protocol::{BackendRef, ControllerInfo};
use egui::{Color32, Label, Painter, Pos2, Rect, RichText, Shape, Stroke, Ui, pos2, vec2};

use crate::badge::{self, Badge, Hue};
use crate::card;
use crate::icons::{
    ICON_BAN, ICON_CIRCLE, ICON_CIRCLE_CHECK, ICON_CIRCLE_DOT, ICON_LIST_CHECKS, ICON_TARGET,
    ICON_TRIANGLE_ALERT, Icon,
};
use crate::theme::{self, Palette};
use crate::tips;

/// Most task lines the Progress card lists before `+N more`.
const MAX_CARD_TASKS: usize = 24;
/// Characters of the current task in the one-line summaries.
const NOW_CHARS: usize = 80;

/// `(completed, total)` when the session has tasks (abandoned ones do not count).
pub fn task_counts(p: &Progress) -> Option<(usize, usize)> {
    let (done, total) = p.counts();
    (total > 0).then_some((done, total))
}

/// `3/7`; `None` without tasks.
pub fn counts_text(p: &Progress) -> Option<String> {
    task_counts(p).map(|(d, t)| format!("{d}/{t}"))
}

/// Completed share of the tasks, `0..=1`; `None` without tasks.
pub fn ratio(p: &Progress) -> Option<f32> {
    task_counts(p).map(|(d, t)| (d as f32 / t as f32).clamp(0.0, 1.0))
}

/// `3/7 · now: <task>` (the task cut to [`NOW_CHARS`]); `None` without tasks.
pub fn todos_line(p: &Progress) -> Option<String> {
    let counts = counts_text(p)?;
    Some(match p.current() {
        Some(t) => format!("{counts} · now: {}", cut(&t.content, NOW_CHARS)),
        None => counts,
    })
}

/// `s` cut to `max` characters with `…`.
pub fn cut(s: &str, max: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", s[..i].trim_end()),
        None => s.to_owned(),
    }
}

pub fn goal_label(s: &GoalStatus) -> &'static str {
    match s {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::BudgetLimited => "budget limited",
        GoalStatus::Complete => "complete",
        GoalStatus::Dropped => "dropped",
        GoalStatus::Unknown => "unknown",
    }
}

pub fn goal_hue(s: &GoalStatus) -> Hue {
    match s {
        GoalStatus::Active => Hue::Blue,
        GoalStatus::Paused | GoalStatus::BudgetLimited => Hue::Amber,
        GoalStatus::Complete => Hue::Green,
        GoalStatus::Dropped | GoalStatus::Unknown => Hue::Rose,
    }
}

/// Icon and colour of a task: in progress in the accent, completed muted with a check,
/// abandoned weak (its text struck), blocked in the warning tone.
pub fn task_look(s: &TodoStatus, pal: &Palette) -> (Icon, Color32) {
    match s {
        TodoStatus::InProgress => (ICON_CIRCLE_DOT, pal.accent),
        TodoStatus::Completed => (ICON_CIRCLE_CHECK, pal.weak),
        TodoStatus::Abandoned => (ICON_BAN, pal.weak.gamma_multiply(0.7)),
        TodoStatus::Blocked => (ICON_TRIANGLE_ALERT, pal.warn),
        TodoStatus::Pending | TodoStatus::Unknown => (ICON_CIRCLE, pal.glyph),
    }
}

/// `45s`, `12m 05s`, `2h 03m`.
pub fn elapsed(secs: u64) -> String {
    crate::agent_model::fmt_duration(Duration::from_secs(secs))
}

/// `12.3k / 50.0k` or `12.3k` without a budget.
pub fn goal_tokens(g: &GoalInfo) -> String {
    let used = crate::sessions_model::compact(g.tokens_used);
    match g.token_budget {
        Some(b) => format!("{used} / {}", crate::sessions_model::compact(b)),
        None => used,
    }
}

/// The compact label of a session's progress: `(icon, text)`; `3/7` with the checklist icon, or
/// the goal's status with the target icon when there are no tasks.
pub fn compact(p: &Progress) -> Option<(Icon, String)> {
    if let Some(t) = counts_text(p) {
        return Some((ICON_LIST_CHECKS, t));
    }
    p.goal
        .as_ref()
        .map(|g| (ICON_TARGET, goal_label(&g.status).to_owned()))
}

/// The progress of every managed session, looked up by session file or backend name. Cached on
/// the app: [`ProgressIndex::refresh`] rebuilds the maps (cleared and refilled, storage kept)
/// only when the gateway replaced its controller list, so rendering never allocates for it.
#[derive(Default)]
pub struct ProgressIndex {
    src: Option<Arc<Vec<ControllerInfo>>>,
    /// Session file / backend name → index into `src` (only controllers with progress).
    by_path: HashMap<PathBuf, usize>,
    by_name: HashMap<String, usize>,
}

impl ProgressIndex {
    /// Follows the gateway's controller list; a no-op while it is the same snapshot.
    pub fn refresh(&mut self, controllers: Option<Arc<Vec<ControllerInfo>>>) {
        let same = match (&self.src, &controllers) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.by_path.clear();
        self.by_name.clear();
        for (i, c) in controllers.iter().flat_map(|c| c.iter()).enumerate() {
            if c.progress.as_ref().is_none_or(Progress::is_empty) {
                continue;
            }
            if let Some(path) = &c.session_path {
                self.by_path.insert(path.clone(), i);
            }
            if let BackendRef::Mux { name, .. } = &c.backend {
                self.by_name.insert(name.clone(), i);
            }
        }
        self.src = controllers;
    }

    fn at(&self, i: usize) -> Option<&Progress> {
        self.src.as_ref()?.get(i)?.progress.as_ref()
    }

    pub fn of_path(&self, path: &Path) -> Option<&Progress> {
        self.at(*self.by_path.get(path)?)
    }

    pub fn of_name(&self, name: &str) -> Option<&Progress> {
        self.at(*self.by_name.get(name)?)
    }
}

/// The Todos and Goal rows of a session's hover card.
pub fn kv_rows(ui: &mut Ui, key_w: f32, p: &Progress) {
    if let Some(line) = todos_line(p) {
        card::kv(ui, key_w, "Todos", |ui| {
            let ratio = ratio(p).unwrap_or(0.0);
            let hue = if ratio >= 1.0 { Hue::Green } else { Hue::Blue };
            badge::show(ui, Some(ICON_LIST_CHECKS), &line, hue, tips::VALUE);
        });
    }
    if let Some(g) = &p.goal {
        card::kv(ui, key_w, "Goal", |ui| {
            badge::show(
                ui,
                Some(ICON_TARGET),
                goal_label(&g.status),
                goal_hue(&g.status),
                tips::VALUE,
            );
            tips::strong(ui, &cut(&g.objective, NOW_CHARS));
        });
    }
}

/// A compact `3/7` (or goal) badge, as painted on the session rows; its width.
pub fn row_badge(p: &Painter, pal: &Palette, progress: &Progress) -> Option<Badge> {
    let (icon, text) = compact(progress)?;
    let done = ratio(progress).is_some_and(|r| r >= 1.0);
    let tint = if done {
        Hue::Green.color(pal)
    } else {
        theme::mix(pal.weak, pal.accent, 0.55)
    };
    Some(Badge::single(p, Some(icon), &text, tint, 10.0))
}

/// Body of the status bar's Progress card: the goal block, then the phases with their tasks.
pub fn card_body(ui: &mut Ui, p: &Progress) {
    const KEY_W: f32 = 84.0;
    let pal = Palette::of_ui(ui);
    if let Some(g) = &p.goal {
        card::section(ui, "Goal");
        ui.add(
            Label::new(
                RichText::new(&g.objective)
                    .size(tips::VALUE + 1.0)
                    .color(pal.text_strong),
            )
            .wrap(),
        );
        card::kv(ui, KEY_W, "Status", |ui| {
            badge::show(
                ui,
                Some(ICON_TARGET),
                goal_label(&g.status),
                goal_hue(&g.status),
                tips::VALUE,
            );
        });
        card::kv(ui, KEY_W, "Tokens", |ui| {
            badge::show(ui, None, &goal_tokens(g), Hue::Teal, tips::VALUE);
        });
        card::kv(ui, KEY_W, "Elapsed", |ui| {
            badge::show(
                ui,
                None,
                &elapsed(g.time_used_seconds),
                Hue::Violet,
                tips::VALUE,
            );
        });
    }
    let mut listed = 0usize;
    let mut hidden = 0usize;
    for phase in &p.phases {
        if phase.tasks.is_empty() {
            continue;
        }
        if listed >= MAX_CARD_TASKS {
            hidden += phase.tasks.len();
            continue;
        }
        ui.add_space(4.0);
        let done = phase
            .tasks
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count();
        let live = phase
            .tasks
            .iter()
            .filter(|t| t.status != TodoStatus::Abandoned)
            .count();
        card::section(ui, &format!("{}  {done}/{live}", phase.name));
        for t in &phase.tasks {
            if listed >= MAX_CARD_TASKS {
                hidden += 1;
                continue;
            }
            task_line(ui, pal, t);
            listed += 1;
        }
    }
    if hidden > 0 || p.truncated {
        let more = if hidden > 0 {
            format!("+{hidden} more")
        } else {
            "+ more (cut by omp's snapshot)".to_owned()
        };
        tips::note(ui, &more);
    }
}

fn task_line(ui: &mut Ui, pal: &Palette, t: &TodoTask) {
    let (icon, color) = task_look(&t.status, pal);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(icon.rich_text().size(12.0).color(color));
        let mut text = RichText::new(&t.content).size(tips::VALUE);
        text = match t.status {
            TodoStatus::InProgress => text.color(pal.text_strong),
            TodoStatus::Completed => text.color(pal.weak),
            TodoStatus::Abandoned => text.color(pal.weak).strikethrough(),
            TodoStatus::Blocked => text.color(pal.warn),
            TodoStatus::Pending | TodoStatus::Unknown => text.color(pal.text),
        };
        ui.add(Label::new(text).wrap());
    });
}

/// A thin track with the completed share filled in the accent (dashboard cards).
pub fn paint_mini_bar(p: &Painter, pal: &Palette, rect: Rect, ratio: f32) {
    let r = rect.height() / 2.0;
    p.rect_filled(rect, r, theme::mix(pal.surface, pal.stroke, 0.7));
    if ratio > 0.0 {
        let fill =
            Rect::from_min_size(rect.min, vec2(rect.width() * ratio.min(1.0), rect.height()));
        let color = if ratio >= 1.0 { pal.ok } else { pal.accent };
        p.rect_filled(fill, r, color);
    }
}

/// The tab's progress line: a 2 px strip just inside the bottom edge of the signal-edge tab in
/// `pill` (whose bottom edge runs from `pill.left()` to `pill.right() - 2 * slant`), slanted
/// like the tab's sides; the completed share is filled in the accent over a faint track.
pub fn paint_tab_line(p: &Painter, pal: &Palette, pill: Rect, slant: f32, radius: f32, ratio: f32) {
    const H: f32 = 2.0;
    // The sides lean `2 * slant` over the pill's height.
    let lean = 2.0 * slant / pill.height().max(1.0) * H;
    let bottom = pill.bottom() - 1.0;
    let left = pill.left() + radius + 1.0;
    let right = pill.right() - 2.0 * slant - radius - 1.0;
    if right <= left {
        return;
    }
    let strip = |x0: f32, x1: f32| -> Vec<Pos2> {
        vec![
            pos2(x0 + lean, bottom - H),
            pos2(x1 + lean, bottom - H),
            pos2(x1, bottom),
            pos2(x0, bottom),
        ]
    };
    p.add(Shape::convex_polygon(
        strip(left, right),
        pal.stroke.gamma_multiply(0.6),
        Stroke::NONE,
    ));
    if ratio > 0.0 {
        let x1 = left + (right - left) * ratio.min(1.0);
        let color = if ratio >= 1.0 { pal.ok } else { pal.accent };
        p.add(Shape::convex_polygon(strip(left, x1), color, Stroke::NONE));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deck_core::ctl::TodoPhase;

    fn task(content: &str, status: TodoStatus) -> TodoTask {
        TodoTask {
            content: content.to_owned(),
            status,
        }
    }

    fn sample() -> Progress {
        Progress {
            phases: vec![
                TodoPhase {
                    name: "Build".into(),
                    tasks: vec![
                        task("a", TodoStatus::Completed),
                        task("b", TodoStatus::Completed),
                        task("c", TodoStatus::Abandoned),
                        task("d", TodoStatus::InProgress),
                    ],
                },
                TodoPhase {
                    name: "Ship".into(),
                    tasks: vec![
                        task("e", TodoStatus::Pending),
                        task("f", TodoStatus::Blocked),
                    ],
                },
            ],
            goal: None,
            truncated: false,
        }
    }

    #[test]
    fn counts_skip_abandoned_tasks_and_name_the_current_one() {
        let p = sample();
        assert_eq!(counts_text(&p).as_deref(), Some("2/5"));
        assert_eq!(ratio(&p), Some(0.4));
        assert_eq!(todos_line(&p).as_deref(), Some("2/5 · now: d"));
        assert_eq!(compact(&p).map(|(_, t)| t).as_deref(), Some("2/5"));
    }

    #[test]
    fn without_tasks_the_goal_speaks_and_nothing_divides_by_zero() {
        let p = Progress {
            goal: Some(GoalInfo {
                objective: "Ship it".into(),
                status: GoalStatus::BudgetLimited,
                tokens_used: 12_300,
                token_budget: Some(50_000),
                time_used_seconds: 125,
            }),
            ..Progress::default()
        };
        assert_eq!(counts_text(&p), None);
        assert_eq!(ratio(&p), None);
        assert_eq!(todos_line(&p), None);
        assert_eq!(
            compact(&p).map(|(_, t)| t).as_deref(),
            Some("budget limited")
        );
        assert_eq!(goal_tokens(p.goal.as_ref().unwrap()), "12.3k / 50.0k");
        assert_eq!(elapsed(125), "2m 05s");
        assert_eq!(compact(&Progress::default()), None);
    }

    #[test]
    fn a_long_current_task_is_cut_on_a_character_boundary() {
        let long = "é".repeat(200);
        let line = todos_line(&Progress {
            phases: vec![TodoPhase {
                name: "p".into(),
                tasks: vec![task(&long, TodoStatus::InProgress)],
            }],
            ..Progress::default()
        })
        .unwrap();
        assert_eq!(
            line.chars().count(),
            "0/1 · now: ".chars().count() + NOW_CHARS + 1
        );
        assert!(line.ends_with('…'));
    }
}
