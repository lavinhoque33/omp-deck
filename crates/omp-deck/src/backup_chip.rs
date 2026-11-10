//! The title bar's backup activity chip: whatever job holds the gateway's backup lane (this
//! window's, another window's, the phone's or an automatic pass) as a pill with a slim progress
//! bar and a percentage, plus a hover card with the details and a Cancel button.
//!
//! [`ChipView`] is the pure view model (built once per gateway backup snapshot, never per frame);
//! [`show`] draws the pill and its interactive card and reports what the user asked for. The
//! card is an interactive egui tooltip: it stays open while the pointer travels from the pill
//! onto it, so Cancel can be pressed.
//!
//! Placement: the CSD title strip, immediately left of the window buttons; with native window
//! frames there is no strip, so the chip sits in the tab row beside the bell instead.

use std::path::Path;
use std::time::Duration;

use deck_service::gateway::protocol::{LaneJob, LaneKind, LaneOrigin, Progress};
use egui::{Color32, Rect, Sense, Ui, pos2, vec2};

use crate::app::human_size;
use crate::badge::{self, Hue};
use crate::icons::{
    ICON_CLOUD, ICON_CLOUD_UPLOAD, ICON_DOWNLOAD, ICON_HARD_DRIVE, ICON_TRASH_2, ICON_USER, ICON_X,
    Icon,
};
use crate::theme::{self, Palette};
use crate::widgets::{DraftButton, DraftKind, paint_icon};
use crate::{card, tips};

/// The pill's label is cut to this many characters (plus the ellipsis).
pub const LABEL_MAX_CHARS: usize = 22;

const ICON_SIZE: f32 = 16.0;
const PAD_LEFT: f32 = 10.0;
const PAD_RIGHT: f32 = 12.0;
const GAP: f32 = 6.0;
const BAR_GAP: f32 = 8.0;
const BAR_W: f32 = 56.0;
const BAR_H: f32 = 4.0;
const CARD_W: f32 = 320.0;
const KEY_W: f32 = 108.0;
/// One sweep of the indeterminate bar, in seconds.
const SWEEP_SECS: f64 = 1.2;
/// Share of the track the indeterminate segment covers.
const SWEEP_SEG: f32 = 0.35;

/// Where a click on the pill goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    /// Settings → Project sync.
    ProjectSync,
    /// Settings → OmpDeck → Backup.
    ConfigBackup,
}

/// What the user asked of the chip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChipAction {
    Open(Page),
    /// Stop the job holding the lane.
    Cancel,
}

/// What the job does, for the icon and the card's title.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Backup,
    Restore,
    Delete,
    Account,
}

impl Verb {
    fn icon(self) -> Icon {
        match self {
            Verb::Backup => ICON_CLOUD_UPLOAD,
            Verb::Restore => ICON_DOWNLOAD,
            Verb::Delete => ICON_TRASH_2,
            Verb::Account => ICON_USER,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Verb::Backup => "Backing up",
            Verb::Restore => "Restoring",
            Verb::Delete => "Deleting backups",
            Verb::Account => "Backup account",
        }
    }
}

/// Everything the chip shows about the lane job, formatted once per gateway snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct ChipView {
    pub verb: Verb,
    pub page: Page,
    /// Short pill label (folder name / "Settings"), already elided; "Cancelling…" while stopping.
    pub label: String,
    /// The gateway's description of the job ("Backing up ~/proj").
    pub what: String,
    /// The folder's full `~` path; `None` when not a project job or `what` already names it.
    pub folder: Option<String>,
    pub destination: String,
    pub on_server: bool,
    /// Human stage name; `None` before the first report.
    pub stage: Option<String>,
    /// 0..=1 when the total is known.
    pub fraction: Option<f32>,
    pub percent: Option<u32>,
    pub amount: Option<String>,
    /// Automatic pass: (1-based folder index, folders in the pass).
    pub pass: Option<(u32, u32)>,
    pub started_by: String,
    pub started_unix_ms: u64,
    pub cancellable: bool,
    pub cancelling: bool,
}

impl ChipView {
    /// `destination` is the name of where backups go ("Google Drive", `sftp://host`).
    pub fn new(job: &LaneJob, destination: &str, on_server: bool) -> ChipView {
        let (verb, page) = classify(&job.kind);
        let fraction = job.progress.as_ref().and_then(fraction);
        let label = if job.cancelling {
            "Cancelling…".to_owned()
        } else {
            elide(&short_label(&job.kind), LABEL_MAX_CHARS)
        };
        let folder = project_path(&job.kind)
            .map(deck_core::sessions::abbreviate_home)
            .filter(|p| !job.label.contains(p.as_str()));
        ChipView {
            verb,
            page,
            label,
            what: job.label.clone(),
            folder,
            destination: destination.to_owned(),
            on_server,
            stage: job
                .progress
                .as_ref()
                .map(|p| stage_label(&p.stage).to_owned()),
            fraction,
            percent: fraction.map(percent),
            amount: job.progress.as_ref().map(amount),
            pass: job.pass,
            started_by: match &job.origin {
                LaneOrigin::Automatic => "Automatic".to_owned(),
                LaneOrigin::Desktop => "This desktop".to_owned(),
                LaneOrigin::Device(name) => name.clone(),
            },
            started_unix_ms: job.started_unix_ms,
            cancellable: job.cancellable,
            cancelling: job.cancelling,
        }
    }

    /// Cancel is offered: the job can be stopped and a cancel was not asked yet.
    pub fn can_cancel(&self) -> bool {
        self.cancellable && !self.cancelling
    }
}

fn classify(kind: &LaneKind) -> (Verb, Page) {
    match kind {
        LaneKind::ConfigBackup => (Verb::Backup, Page::ConfigBackup),
        LaneKind::ConfigRestore => (Verb::Restore, Page::ConfigBackup),
        LaneKind::ConfigDelete { .. } => (Verb::Delete, Page::ConfigBackup),
        LaneKind::ProjectBackup { .. } => (Verb::Backup, Page::ProjectSync),
        LaneKind::ProjectRestore { .. } => (Verb::Restore, Page::ProjectSync),
        LaneKind::ProjectDelete { .. } => (Verb::Delete, Page::ProjectSync),
        LaneKind::Account => (Verb::Account, Page::ConfigBackup),
    }
}

fn project_path(kind: &LaneKind) -> Option<&Path> {
    match kind {
        LaneKind::ProjectBackup { project } => Some(project),
        LaneKind::ProjectRestore { target } => Some(target),
        _ => None,
    }
}

/// "Settings" for the config jobs, the folder's name for project jobs.
fn short_label(kind: &LaneKind) -> String {
    match kind {
        LaneKind::ConfigBackup | LaneKind::ConfigRestore | LaneKind::ConfigDelete { .. } => {
            "Settings".to_owned()
        }
        LaneKind::ProjectBackup { project: p } | LaneKind::ProjectRestore { target: p } => {
            p.file_name().map_or_else(
                || deck_core::sessions::abbreviate_home(p),
                |n| n.to_string_lossy().into_owned(),
            )
        }
        LaneKind::ProjectDelete { .. } => "Project backups".to_owned(),
        LaneKind::Account => "Account".to_owned(),
    }
}

/// `text` cut to at most `max` characters, the last one an ellipsis when something was dropped.
pub fn elide(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// The stage's name for people; unknown stages pass through.
pub fn stage_label(stage: &str) -> &str {
    match stage {
        "RestoringOmp" => "Restoring .omp",
        "RestoringSessions" => "Restoring sessions",
        "RestoringMemory" => "Restoring memory",
        other => other,
    }
}

/// Done share of the stage, 0..=1; `None` while the total is unknown (or zero).
pub fn fraction(p: &Progress) -> Option<f32> {
    p.total
        .filter(|t| *t > 0)
        .map(|t| (p.done as f64 / t as f64).clamp(0.0, 1.0) as f32)
}

/// Whole percent (rounded down) of a 0..=1 fraction.
pub fn percent(fraction: f32) -> u32 {
    (fraction.clamp(0.0, 1.0) * 100.0) as u32
}

/// What has been done so far: items for the scanning/restoring stages, human sizes for the
/// byte stages (`1.5 MB of 3.0 GB`).
pub fn amount(p: &Progress) -> String {
    let items = |one: &str, many: &str| match p.total {
        Some(t) => format!("{} of {t} {}", p.done, if t == 1 { one } else { many }),
        None => format!("{} {}", p.done, if p.done == 1 { one } else { many }),
    };
    match p.stage.as_str() {
        "Scanning" | "RestoringSessions" => items("session", "sessions"),
        "RestoringOmp" | "RestoringMemory" => items("file", "files"),
        "Packing" | "Uploading" | "Downloading" | "Verifying" => match p.total {
            Some(t) => format!("{} of {}", human_size(p.done), human_size(t)),
            None => human_size(p.done),
        },
        _ => match p.total {
            Some(t) => format!("{} of {t}", p.done),
            None => p.done.to_string(),
        },
    }
}

/// `42s`, `3m 05s`, `1h 02m`; a start in the future (clock skew) reads `0s`.
pub fn elapsed_text(started_unix_ms: u64, now_unix_ms: u64) -> String {
    let secs = now_unix_ms.saturating_sub(started_unix_ms) / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// The indeterminate bar's moving segment at `time` (seconds), as `(start, end)` shares of the
/// track in 0..=1. It enters from the left edge and leaves at the right one.
pub fn sweep(time: f64) -> (f32, f32) {
    let phase = (time / SWEEP_SECS).fract() as f32;
    let start = -SWEEP_SEG + phase * (1.0 + SWEEP_SEG);
    (start.clamp(0.0, 1.0), (start + SWEEP_SEG).clamp(0.0, 1.0))
}

// ---------------------------------------------------------------------------- drawing

struct Galleys {
    label: std::sync::Arc<egui::Galley>,
    percent: Option<std::sync::Arc<egui::Galley>>,
    /// Width reserved for the percentage (`100%` in the pill's font).
    percent_w: f32,
}

fn galleys(ui: &Ui, v: &ChipView) -> Galleys {
    let p = ui.painter();
    let layout =
        |text: String, size: f32| p.layout_no_wrap(text, theme::font(size), Color32::PLACEHOLDER);
    let percent = v.percent.map(|n| layout(format!("{n}%"), 12.0));
    let percent_w = percent
        .as_ref()
        .map_or(0.0, |_| layout("100%".to_owned(), 12.0).size().x);
    Galleys {
        label: layout(v.label.clone(), 13.0),
        percent,
        percent_w,
    }
}

fn pill_width(g: &Galleys) -> f32 {
    let percent = if g.percent.is_some() {
        BAR_GAP + g.percent_w
    } else {
        0.0
    };
    PAD_LEFT + ICON_SIZE + GAP + g.label.size().x + BAR_GAP + BAR_W + percent + PAD_RIGHT
}

/// Width of the pill (the title bar reserves it before laying the right-hand side out).
pub fn width(ui: &Ui, v: &ChipView) -> f32 {
    pill_width(&galleys(ui, v))
}

/// Draws the pill and its hover card; returns what the user asked for this frame.
pub fn show(ui: &mut Ui, v: &ChipView) -> Option<ChipAction> {
    let pal = Palette::of_ui(ui);
    let g = galleys(ui, v);
    let (rect, resp) = ui.allocate_exact_size(
        vec2(pill_width(&g), theme::dens(24.0, 28.0)),
        Sense::click(),
    );
    let ctx = ui.ctx().clone();
    let hov = ctx.animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.12);
    let tone = v.cancelling.then_some(pal.warn);
    let fg = crate::titlebar::pill_chrome(ui, rect, hov, tone);
    let p = ui.painter();
    let cy = rect.center().y;
    paint_icon(
        p,
        pos2(rect.left() + PAD_LEFT + ICON_SIZE / 2.0, cy),
        egui::Align2::CENTER_CENTER,
        v.verb.icon(),
        ICON_SIZE,
        fg,
    );
    let label_x = rect.left() + PAD_LEFT + ICON_SIZE + GAP;
    p.galley(
        pos2(label_x, cy - g.label.size().y / 2.0),
        g.label.clone(),
        fg,
    );
    let bar_x = label_x + g.label.size().x + BAR_GAP;
    let track = Rect::from_min_size(pos2(bar_x, cy - BAR_H / 2.0), vec2(BAR_W, BAR_H));
    p.rect_filled(track, 2, fg.gamma_multiply(0.28));
    // Cancelling has no meaningful fraction: the bar sweeps until the job ends.
    match v.fraction.filter(|_| !v.cancelling) {
        Some(f) => {
            let f = ctx.animate_value_with_time(resp.id.with("fraction"), f, 0.25);
            let fill = Rect::from_min_size(track.min, vec2(BAR_W * f.clamp(0.0, 1.0), BAR_H));
            p.rect_filled(fill, 2, fg);
        }
        None => {
            let (a, b) = sweep(ctx.input(|i| i.time));
            if b > a {
                let seg = Rect::from_min_max(
                    pos2(track.left() + BAR_W * a, track.top()),
                    pos2(track.left() + BAR_W * b, track.bottom()),
                );
                p.rect_filled(seg, 2, fg);
            }
            // Only while the pill is shown: nothing repaints once the lane frees.
            ctx.request_repaint_after(Duration::from_millis(32));
        }
    }
    if let Some(pg) = &g.percent {
        let right = rect.right() - PAD_RIGHT;
        p.galley(
            pos2(right - pg.size().x, cy - pg.size().y / 2.0),
            pg.clone(),
            fg,
        );
    }
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let mut action = resp.clicked().then_some(ChipAction::Open(v.page));

    let tone = tone.unwrap_or(Color32::TRANSPARENT);
    let head = card::Header::new(v.verb.icon(), v.verb.title()).tone(tone);
    let card_action = card::hover(
        &resp,
        CARD_W,
        false,
        &head,
        |ui| header_badge(ui, v),
        |ui| card_body(ui, v, now_unix_ms()),
    )
    .flatten();
    if card_action.is_some() {
        action = card_action;
    }
    action
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// The card header's right slot: the percentage, or that a cancel is under way.
fn header_badge(ui: &mut Ui, v: &ChipView) {
    if v.cancelling {
        badge::show(ui, None, "cancelling", Hue::Amber, tips::VALUE);
    } else if let Some(n) = v.percent {
        badge::show(ui, None, &format!("{n}%"), Hue::Blue, tips::VALUE);
    }
}

fn card_body(ui: &mut Ui, v: &ChipView, now_ms: u64) -> Option<ChipAction> {
    // The elapsed row ticks while the card is open.
    ui.ctx().request_repaint_after(Duration::from_millis(500));
    card::kv(ui, KEY_W, "What", |ui| tips::strong(ui, &v.what));
    if let Some(folder) = &v.folder {
        card::kv(ui, KEY_W, "Path", |ui| tips::mono(ui, folder));
    }
    card::kv(ui, KEY_W, "Destination", |ui| {
        let icon = if v.on_server {
            ICON_HARD_DRIVE
        } else {
            ICON_CLOUD
        };
        badge::show(ui, Some(icon), &v.destination, Hue::Blue, tips::VALUE);
    });
    card::kv(ui, KEY_W, "Stage", |ui| match &v.stage {
        Some(stage) => {
            badge::show(ui, None, stage, Hue::Teal, tips::VALUE);
            if let Some(n) = v.percent {
                badge::show(ui, None, &format!("{n}%"), Hue::Green, tips::VALUE);
            }
        }
        None => {
            badge::show(ui, None, "Starting…", Hue::Amber, tips::VALUE);
        }
    });
    if let Some(amount) = &v.amount {
        card::kv(ui, KEY_W, "Amount", |ui| tips::strong(ui, amount));
    }
    if let Some((n, of)) = v.pass {
        card::kv(ui, KEY_W, "Folder", |ui| {
            badge::show(ui, None, &format!("{n} of {of}"), Hue::Violet, tips::VALUE);
        });
    }
    card::kv(ui, KEY_W, "Started by", |ui| {
        badge::show(ui, None, &v.started_by, Hue::Orange, tips::VALUE);
    });
    card::kv(ui, KEY_W, "Elapsed", |ui| {
        tips::strong(ui, &elapsed_text(v.started_unix_ms, now_ms));
    });
    let action = cancel_control(ui, v);
    card::footer_hints(ui, &[("click", "open settings")]);
    action
}

/// The danger Cancel button, a note that a cancel is under way, or a muted "can't be cancelled".
fn cancel_control(ui: &mut Ui, v: &ChipView) -> Option<ChipAction> {
    if v.cancelling {
        tips::note(ui, "Cancelling… waiting for the job to stop.");
        None
    } else if v.cancellable {
        DraftButton::new("Cancel job")
            .kind(DraftKind::Danger)
            .icon(ICON_X)
            .show(ui)
            .clicked()
            .then_some(ChipAction::Cancel)
    } else {
        tips::note(ui, "Can't be cancelled");
        None
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn job(kind: LaneKind) -> LaneJob {
        LaneJob {
            label: "Backing up /srv/work/proj".to_owned(),
            kind,
            origin: LaneOrigin::Desktop,
            started_unix_ms: 1_000,
            progress: None,
            pass: None,
            cancellable: true,
            cancelling: false,
        }
    }

    fn progress(stage: &str, done: u64, total: Option<u64>) -> Progress {
        Progress {
            stage: stage.to_owned(),
            done,
            total,
        }
    }

    fn project_job() -> LaneJob {
        job(LaneKind::ProjectBackup {
            project: PathBuf::from("/srv/work/proj"),
        })
    }

    #[test]
    fn percent_rounds_down_and_stays_in_range() {
        assert_eq!(fraction(&progress("Packing", 0, Some(200))), Some(0.0));
        assert_eq!(
            fraction(&progress("Packing", 1, Some(3))).map(percent),
            Some(33)
        );
        assert_eq!(
            fraction(&progress("Packing", 200, Some(200))).map(percent),
            Some(100)
        );
        // A report that overshoots its total never shows more than 100 %.
        assert_eq!(
            fraction(&progress("Packing", 9, Some(3))).map(percent),
            Some(100)
        );
        // Unknown or zero totals are indeterminate, not 0 % / NaN.
        assert_eq!(fraction(&progress("Packing", 5, None)), None);
        assert_eq!(fraction(&progress("Packing", 0, Some(0))), None);
    }

    #[test]
    fn amount_reads_per_stage() {
        assert_eq!(
            amount(&progress("Scanning", 3, Some(11))),
            "3 of 11 sessions"
        );
        assert_eq!(amount(&progress("Scanning", 0, Some(1))), "0 of 1 session");
        assert_eq!(amount(&progress("RestoringSessions", 1, None)), "1 session");
        assert_eq!(
            amount(&progress("RestoringOmp", 4, Some(9))),
            "4 of 9 files"
        );
        assert_eq!(
            amount(&progress("RestoringMemory", 2, Some(2))),
            "2 of 2 files"
        );
        assert_eq!(
            amount(&progress("Uploading", 1536, Some(3 * 1024 * 1024))),
            "1.5 KB of 3.0 MB"
        );
        assert_eq!(amount(&progress("Packing", 10, None)), "10 B");
        assert_eq!(amount(&progress("Mystery", 2, Some(5))), "2 of 5");
    }

    #[test]
    fn stage_names_are_for_people() {
        assert_eq!(stage_label("Uploading"), "Uploading");
        assert_eq!(stage_label("RestoringOmp"), "Restoring .omp");
        assert_eq!(stage_label("RestoringSessions"), "Restoring sessions");
        assert_eq!(stage_label("Whatever"), "Whatever");
    }

    #[test]
    fn elapsed_steps_through_units() {
        assert_eq!(elapsed_text(1_000, 1_000), "0s");
        assert_eq!(elapsed_text(1_000, 60_999), "59s");
        assert_eq!(elapsed_text(1_000, 61_000), "1m 00s");
        assert_eq!(elapsed_text(0, 3_599_000), "59m 59s");
        assert_eq!(elapsed_text(0, 3_600_000), "1h 00m");
        assert_eq!(elapsed_text(0, 7_385_000), "2h 03m");
        // A start after "now" (clock skew between gateway and window) does not underflow.
        assert_eq!(elapsed_text(9_000, 1_000), "0s");
    }

    #[test]
    fn labels_elide_at_the_boundary() {
        assert_eq!(elide("abcdef", 6), "abcdef");
        assert_eq!(elide("abcdefg", 6), "abcde…");
        assert_eq!(elide("abc", 0), "");
        assert_eq!(elide("x", 1), "x");
        assert_eq!(elide("xy", 1), "…");
        // Multi-byte characters are counted, never split.
        assert_eq!(elide("ééééé", 4), "ééé…");
    }

    #[test]
    fn sweep_stays_inside_the_track_and_moves_right() {
        let mut last = (0.0f32, 0.0f32);
        for i in 0..100 {
            let (a, b) = sweep(f64::from(i) * SWEEP_SECS / 100.0);
            assert!((0.0..=1.0).contains(&a) && (0.0..=1.0).contains(&b) && a <= b);
            assert!(
                b >= last.1,
                "the segment's head only moves right within a sweep"
            );
            last = (a, b);
        }
        // It starts off-track (nothing drawn) and a full period later is back there.
        assert_eq!(sweep(0.0), (0.0, 0.0));
        assert_eq!(sweep(SWEEP_SECS), (0.0, 0.0));
    }

    #[test]
    fn project_job_is_labelled_by_its_folder() {
        let mut j = project_job();
        j.progress = Some(progress("Uploading", 50, Some(200)));
        let v = ChipView::new(&j, "Google Drive", false);
        assert_eq!((v.verb, v.page), (Verb::Backup, Page::ProjectSync));
        assert_eq!(v.label, "proj");
        assert_eq!(v.stage.as_deref(), Some("Uploading"));
        assert_eq!((v.fraction, v.percent), (Some(0.25), Some(25)));
        assert_eq!(v.started_by, "This desktop");
        // The job's own label already names the folder: no duplicate row.
        assert_eq!(v.folder, None);
        let mut j = project_job();
        j.label = "Restoring a backup".to_owned();
        assert_eq!(
            ChipView::new(&j, "x", false).folder.as_deref(),
            Some("/srv/work/proj")
        );
    }

    #[test]
    fn long_folder_names_are_elided() {
        let j = job(LaneKind::ProjectRestore {
            target: PathBuf::from("/srv/a-really-long-project-folder-name"),
        });
        let v = ChipView::new(&j, "x", false);
        assert_eq!(v.label.chars().count(), LABEL_MAX_CHARS);
        assert!(v.label.ends_with('…'));
        assert_eq!((v.verb, v.page), (Verb::Restore, Page::ProjectSync));
    }

    #[test]
    fn config_and_account_jobs_open_the_backup_page() {
        for (kind, verb) in [
            (LaneKind::ConfigBackup, Verb::Backup),
            (LaneKind::ConfigRestore, Verb::Restore),
            (LaneKind::ConfigDelete { count: 2 }, Verb::Delete),
            (LaneKind::Account, Verb::Account),
        ] {
            let v = ChipView::new(&job(kind), "x", false);
            assert_eq!((v.verb, v.page), (verb, Page::ConfigBackup));
        }
        let v = ChipView::new(&job(LaneKind::ConfigBackup), "x", false);
        assert_eq!(v.label, "Settings");
        let v = ChipView::new(&job(LaneKind::ProjectDelete { count: 3 }), "x", false);
        assert_eq!((v.verb, v.page), (Verb::Delete, Page::ProjectSync));
    }

    #[test]
    fn job_without_a_report_is_indeterminate_and_starting() {
        let v = ChipView::new(&project_job(), "x", false);
        assert_eq!(
            (v.stage, v.fraction, v.percent, v.amount),
            (None, None, None, None)
        );
    }

    #[test]
    fn origin_and_pass_come_through() {
        let mut j = project_job();
        j.origin = LaneOrigin::Device("Pixel 8".to_owned());
        j.pass = Some((2, 5));
        let v = ChipView::new(&j, "x", true);
        assert_eq!(v.started_by, "Pixel 8");
        assert_eq!(v.pass, Some((2, 5)));
        assert!(v.on_server);
        j.origin = LaneOrigin::Automatic;
        assert_eq!(ChipView::new(&j, "x", false).started_by, "Automatic");
    }

    #[test]
    fn cancel_is_offered_only_for_a_cancellable_job_not_already_stopping() {
        let mut j = project_job();
        assert!(ChipView::new(&j, "x", false).can_cancel());
        j.cancellable = false;
        assert!(!ChipView::new(&j, "x", false).can_cancel());
        j.cancellable = true;
        j.cancelling = true;
        j.progress = Some(progress("Packing", 1, Some(2)));
        let v = ChipView::new(&j, "x", false);
        assert!(!v.can_cancel());
        assert_eq!(v.label, "Cancelling…");
    }

    /// Runs `cancel_control` for `v` in a headless egui context, pressing the primary button on
    /// the control's centre once it has been laid out; returns what it reported.
    fn press_cancel(v: &ChipView) -> (Vec<Option<ChipAction>>, Rect) {
        let ctx = egui::Context::default();
        crate::ui_fonts::install(&ctx, crate::ui_fonts::UiFonts::none());
        ctx.run_ui(egui::RawInput::default(), |_| {})
            .textures_delta
            .clear();
        crate::ui_fonts::finalize(&ctx);
        theme::apply(
            &ctx,
            deck_core::config::UiStyle::ALL[0],
            deck_core::config::Density::Comfortable,
            deck_core::config::UiFont::Auto,
            true,
        );
        let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 300.0));
        let frame = |events: Vec<egui::Event>| egui::RawInput {
            screen_rect: Some(screen),
            events,
            ..Default::default()
        };
        let mut rect = Rect::NOTHING;
        let mut seen = Vec::new();
        let mut run = |input: egui::RawInput, rect: &mut Rect| {
            let mut out = ctx.run_ui(input, |ui| {
                let mut child = ui.new_child(
                    egui::UiBuilder::new()
                        .id_salt("cancel-test")
                        .max_rect(Rect::from_min_size(pos2(20.0, 20.0), vec2(300.0, 200.0))),
                );
                seen.push(cancel_control(&mut child, v));
                *rect = child.min_rect();
            });
            out.textures_delta.clear();
        };
        run(frame(vec![]), &mut rect);
        run(frame(vec![]), &mut rect);
        let at = rect.center();
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        run(frame(vec![egui::Event::PointerMoved(at)]), &mut rect);
        run(frame(vec![button(true)]), &mut rect);
        run(frame(vec![button(false)]), &mut rect);
        (seen, rect)
    }

    #[test]
    fn pressing_cancel_asks_to_cancel() {
        let (seen, _) = press_cancel(&ChipView::new(&project_job(), "x", false));
        assert_eq!(
            seen.iter()
                .filter(|a| **a == Some(ChipAction::Cancel))
                .count(),
            1,
            "exactly one click: {seen:?}"
        );
    }

    #[test]
    fn a_job_that_cannot_be_cancelled_has_no_button() {
        let mut j = project_job();
        j.cancellable = false;
        let (seen, _) = press_cancel(&ChipView::new(&j, "x", false));
        assert!(seen.iter().all(Option::is_none));
        // Already stopping: same.
        let mut j = project_job();
        j.cancelling = true;
        let (seen, _) = press_cancel(&ChipView::new(&j, "x", false));
        assert!(seen.iter().all(Option::is_none));
    }
}
