//! Project sync UI (pure: state in, actions out; the logic lives in `app/project_sync.rs` and
//! `deck_core::gdrive`): the Settings "Sync" page, the restore dialog, the sidebar's backup dialog
//! and the typed-path prompt.

use deck_core::config::SyncInterval;
use egui::{Button, Context, Frame, Id, Margin, RichText, ScrollArea, Stroke, Ui, vec2};

use crate::confirm::{Dialog, show_dialog};
use crate::drive_view::{DriveAction, DriveCard};
use crate::icons::{
    ICON_ARCHIVE_RESTORE, ICON_CLOUD_UPLOAD, ICON_FOLDER, ICON_FOLDER_OPEN, ICON_FOLDER_PLUS,
    ICON_LOG_IN, ICON_PENCIL, ICON_TRASH_2, ICON_TRIANGLE_ALERT, ICON_X,
};
use crate::theme::{self, Palette};
use crate::widgets::{Spin, card, icon_text, setting_row, toggle};

const PLAN_LIST_MAX: usize = 8;

/// Wording shared by the page, the sidebar menus and the dialogs.
pub const NOTE: &str = "Stored in OmpDeck's private Google Drive app folder and counted toward your \
     Drive storage. A project's .omp folder may contain secrets (API tokens in config.yml, \
     mcp.json, secrets.yml).";

// ------------------------------------------------------------------ shared cells

/// What measuring a project's `.omp` found.
#[derive(Clone, Debug, PartialEq)]
pub enum OmpCell {
    Loading,
    Missing,
    /// The folder is `$HOME`: its `.omp` is omp's global directory, not a project one.
    Global,
    /// The project's `.omp` is a symlink: never followed or backed up.
    Symlink,
    Present(String),
    Failed(String),
}

/// What measuring a project's sessions found.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionsCell {
    Loading,
    /// `0` sessions = nothing to back up.
    Ready {
        count: u64,
        text: String,
    },
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cells {
    pub omp: OmpCell,
    pub sessions: SessionsCell,
}

impl Cells {
    pub fn omp_usable(&self) -> bool {
        matches!(self.omp, OmpCell::Present(_))
    }

    pub fn sessions_usable(&self) -> bool {
        matches!(self.sessions, SessionsCell::Ready { count, .. } if count > 0)
    }

    fn omp_text(&self) -> (String, Option<&'static str>) {
        match &self.omp {
            OmpCell::Loading => ("measuring…".into(), None),
            OmpCell::Missing => (
                "no .omp folder".into(),
                Some("This folder has no .omp directory"),
            ),
            OmpCell::Global => (
                "global config".into(),
                Some(
                    "This is your home folder: its .omp is omp's global directory, not a project's",
                ),
            ),
            OmpCell::Present(t) => (t.clone(), None),
            OmpCell::Failed(e) => (
                format!("unreadable: {e}"),
                Some("The size could not be read"),
            ),
            OmpCell::Symlink => (
                "symlink".into(),
                Some(
                    "This project's .omp is a symlink; OmpDeck does not follow or back up symlinks",
                ),
            ),
        }
    }

    fn sessions_text(&self) -> (String, Option<&'static str>) {
        match &self.sessions {
            SessionsCell::Loading => ("measuring…".into(), None),
            SessionsCell::Ready { count: 0, .. } => (
                "no sessions".into(),
                Some("omp has no sessions for this folder"),
            ),
            SessionsCell::Ready { text, .. } => (text.clone(), None),
            SessionsCell::Failed(e) => (
                format!("unreadable: {e}"),
                Some("The sessions could not be read"),
            ),
        }
    }
}

/// A running job's label and fraction.
#[derive(Clone, Debug, PartialEq)]
pub struct ProgView {
    pub label: String,
    pub fraction: Option<f32>,
}

fn weak_note(ui: &mut Ui, text: &str) {
    let pal = Palette::of_ui(ui);
    ui.add(
        egui::Label::new(RichText::new(text).small().color(pal.weak))
            .wrap_mode(egui::TextWrapMode::Wrap),
    );
}

fn button(ui: &mut Ui, enabled: bool, icon: crate::icons::Icon, label: &str) -> egui::Response {
    let pal = Palette::of_ui(ui);
    ui.add_enabled(
        enabled,
        Button::new((icon_text(icon, 16.0, pal.glyph), RichText::new(label)))
            .min_size(vec2(0.0, theme::dens(28.0, 30.0))),
    )
}

fn progress(ui: &mut Ui, p: &ProgView) {
    let pal = Palette::of_ui(ui);
    ui.horizontal(|ui| {
        ui.add(Spin::new(16.0, pal.accent_b));
        ui.label(RichText::new(&p.label).color(pal.weak));
    });
    if let Some(f) = p.fraction {
        ui.add(
            egui::ProgressBar::new(f.clamp(0.0, 1.0))
                .desired_height(6.0)
                .desired_width(ui.available_width()),
        );
    }
}

// ------------------------------------------------------------------ the page

/// The last backup of a row.
#[derive(Clone, Debug, PartialEq)]
pub enum LastView {
    Never,
    /// "2h ago", tooltip with the exact time and host.
    At {
        text: String,
        tip: String,
        /// ".omp entries left out of the last backup" ("<path>: <reason>").
        skipped: Vec<String>,
    },
    Failed(String),
}

pub struct RowView {
    pub display: String,
    pub full: String,
    pub omp_on: bool,
    pub sessions_on: bool,
    pub cells: Cells,
    pub last: LastView,
    /// This row's job is running.
    pub busy: Option<ProgView>,
}

pub struct SyncPage<'a> {
    pub account: &'a DriveCard,
    pub rows: &'a [RowView],
    pub interval: SyncInterval,
    /// A project job (any row, a restore) is running: starting another is disabled.
    pub lane_busy: bool,
    /// The native dialog is open.
    pub picking: bool,
}

#[derive(Debug, PartialEq)]
pub enum SyncAction {
    /// The page was drawn this frame (the app measures sizes when it newly appears).
    Visible,
    Drive(DriveAction),
    Add,
    Change(usize),
    Remove(usize),
    SetOmp(usize, bool),
    SetSessions(usize, bool),
    BackUp(usize),
    SetInterval(SyncInterval),
    OpenRestore,
}

fn account_strip(ui: &mut Ui, account: &DriveCard, actions: &mut Vec<SyncAction>) {
    let pal = Palette::of_ui(ui);
    match account {
        DriveCard::Unavailable => setting_row(
            ui,
            "Not available in this build",
            "This OmpDeck was built without a Google OAuth client",
            |_| {},
        ),
        DriveCard::SignedOut => setting_row(
            ui,
            "Sign in to back up projects",
            "Uses the same Google account as the config backup",
            |ui| {
                if button(ui, true, ICON_LOG_IN, "Sign in with Google").clicked() {
                    actions.push(SyncAction::Drive(DriveAction::SignIn));
                }
            },
        ),
        DriveCard::Waiting => setting_row(
            ui,
            "Waiting for the browser…",
            "Finish signing in on the Google page that just opened.",
            |ui| {
                if button(ui, true, ICON_X, "Cancel").clicked() {
                    actions.push(SyncAction::Drive(DriveAction::CancelSignIn));
                }
                ui.add(Spin::new(18.0, pal.accent_b));
            },
        ),
        DriveCard::SignedIn { email, .. } => {
            let who = email
                .as_deref()
                .map_or_else(|| "Signed in".to_owned(), |e| format!("Signed in as {e}"));
            setting_row(
                ui,
                &who,
                "Project backups live in OmpDeck's private app folder on your Drive",
                |_| {},
            );
        }
    }
}

/// The Sync page body (inside the settings scroll area).
pub fn page(ui: &mut Ui, page: &SyncPage<'_>) -> Vec<SyncAction> {
    let pal = Palette::of_ui(ui);
    let mut actions = vec![SyncAction::Visible];
    ui.label(theme::strong("Sync").size(22.0).color(pal.text_strong));
    ui.label(
        RichText::new("Back up a project's .omp folder and its omp sessions to Google Drive")
            .color(pal.weak),
    );
    ui.add_space(16.0);

    let signed_in = matches!(page.account, DriveCard::SignedIn { .. });
    card(ui, "Google account", |ui| {
        account_strip(ui, page.account, &mut actions);
    });

    let idle = signed_in && !page.lane_busy;
    let why_not = if !signed_in {
        "Sign in with Google first"
    } else {
        "Another project backup or restore is running"
    };
    card(ui, "Projects", |ui| {
        if page.rows.is_empty() {
            ui.add_space(4.0);
            ui.label(
                RichText::new("No project folders yet. Add one to back it up.").color(pal.weak),
            );
            ui.add_space(8.0);
        }
        for (i, row) in page.rows.iter().enumerate() {
            project_row(ui, i, row, idle, why_not, &mut actions);
            ui.add_space(8.0);
        }
        ui.horizontal(|ui| {
            let label = if page.picking {
                "Waiting…"
            } else {
                "Add folder…"
            };
            if button(ui, !page.picking, ICON_FOLDER_PLUS, label).clicked() {
                actions.push(SyncAction::Add);
            }
        });
    });

    card(ui, "Automatic backup", |ui| {
        setting_row(
            ui,
            "Back up listed projects automatically",
            "Only projects whose files changed since their last backup are uploaded",
            |ui| {
                egui::ComboBox::from_id_salt("sync-interval")
                    .selected_text(page.interval.label())
                    .show_ui(ui, |ui| {
                        for v in SyncInterval::ALL {
                            if ui.selectable_label(page.interval == v, v.label()).clicked()
                                && page.interval != v
                            {
                                actions.push(SyncAction::SetInterval(v));
                            }
                        }
                    });
            },
        );
    });

    card(ui, "Restore", |ui| {
        setting_row(
            ui,
            "Restore a project from Google Drive",
            "Pick a backup, a folder on this computer and what to bring back",
            |ui| {
                let resp = button(ui, idle, ICON_ARCHIVE_RESTORE, "Restore…");
                if resp.clicked() {
                    actions.push(SyncAction::OpenRestore);
                }
                resp.on_disabled_hover_text(why_not);
            },
        );
        weak_note(ui, NOTE);
    });
    actions
}

fn project_row(
    ui: &mut Ui,
    i: usize,
    row: &RowView,
    idle: bool,
    why_not: &str,
    actions: &mut Vec<SyncAction>,
) {
    let pal = Palette::of_ui(ui);
    Frame::NONE
        .fill(
            pal.surface
                .gamma_multiply(if theme::neutral() { 0.6 } else { 0.9 }),
        )
        .stroke(Stroke::new(1.0, pal.stroke))
        .corner_radius(theme::rad(6, 12))
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::Sides::new().shrink_left().show(
                ui,
                |ui| {
                    ui.add(icon_label(ICON_FOLDER, pal.glyph));
                    ui.add(
                        egui::Label::new(RichText::new(&row.display).color(pal.text_strong))
                            .truncate(),
                    )
                    .on_hover_text(&row.full);
                },
                |ui| {
                    let rm = ui.add_enabled(
                        row.busy.is_none(),
                        Button::new(icon_text(ICON_TRASH_2, 16.0, pal.danger))
                            .min_size(vec2(0.0, theme::dens(28.0, 30.0))),
                    );
                    if rm
                        .on_hover_text("Remove from Sync (Drive backups stay)")
                        .clicked()
                    {
                        actions.push(SyncAction::Remove(i));
                    }
                    match &row.busy {
                        Some(p) => {
                            ui.label(RichText::new(&p.label).small().color(pal.weak));
                            ui.add(Spin::new(16.0, pal.accent_b));
                        }
                        None => {
                            let resp = button(ui, idle, ICON_CLOUD_UPLOAD, "Back up now");
                            if resp.clicked() {
                                actions.push(SyncAction::BackUp(i));
                            }
                            resp.on_disabled_hover_text(why_not);
                        }
                    }
                    if button(ui, row.busy.is_none(), ICON_PENCIL, "Change…").clicked() {
                        actions.push(SyncAction::Change(i));
                    }
                },
            );
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let (omp_text, omp_why) = row.cells.omp_text();
                part_switch(
                    ui,
                    ".omp folder",
                    row.omp_on,
                    row.cells.omp_usable(),
                    &omp_text,
                    omp_why,
                    |on| {
                        actions.push(SyncAction::SetOmp(i, on));
                    },
                );
                ui.add_space(14.0);
                let (s_text, s_why) = row.cells.sessions_text();
                part_switch(
                    ui,
                    "Sessions",
                    row.sessions_on,
                    row.cells.sessions_usable(),
                    &s_text,
                    s_why,
                    |on| {
                        actions.push(SyncAction::SetSessions(i, on));
                    },
                );
            });
            ui.add_space(2.0);
            match &row.last {
                LastView::Never => {
                    ui.label(RichText::new("Last backup: never").small().color(pal.weak));
                }
                LastView::At { text, tip, skipped } => {
                    let mut line = format!("Last backup: {text}");
                    let mut hover = tip.clone();
                    if !skipped.is_empty() {
                        line += &format!(
                            " · {} file{} skipped",
                            skipped.len(),
                            if skipped.len() == 1 { "" } else { "s" }
                        );
                        hover += &format!("\nSkipped:\n{}", skipped.join("\n"));
                    }
                    let color = if skipped.is_empty() {
                        pal.weak
                    } else {
                        pal.warn
                    };
                    ui.label(RichText::new(line).small().color(color))
                        .on_hover_text(hover);
                }
                LastView::Failed(e) => {
                    ui.horizontal(|ui| {
                        ui.label(icon_text(ICON_TRIANGLE_ALERT, 14.0, pal.danger));
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!("Last backup failed: {e}"))
                                    .small()
                                    .color(pal.danger),
                            )
                            .truncate(),
                        )
                        .on_hover_text(e);
                    });
                }
            }
            if let Some(f) = row.busy.as_ref().and_then(|p| p.fraction) {
                ui.add_space(4.0);
                ui.add(
                    egui::ProgressBar::new(f.clamp(0.0, 1.0))
                        .desired_height(5.0)
                        .desired_width(ui.available_width()),
                );
            }
        });
}

fn icon_label(icon: crate::icons::Icon, color: egui::Color32) -> egui::Label {
    egui::Label::new(icon_text(icon, 16.0, color))
}

/// `[switch] label  size` where the switch is disabled (and shown off) when `usable` is false.
fn part_switch(
    ui: &mut Ui,
    label: &str,
    on: bool,
    usable: bool,
    size: &str,
    why: Option<&str>,
    mut changed: impl FnMut(bool),
) {
    let pal = Palette::of_ui(ui);
    let mut shown = on && usable;
    let resp = ui.add_enabled_ui(usable, |ui| toggle(ui, &mut shown)).inner;
    if resp.changed() {
        changed(shown);
    }
    ui.label(RichText::new(label).color(pal.text));
    let sz = ui.label(RichText::new(size).small().color(pal.weak));
    if let Some(why) = why {
        resp.on_disabled_hover_text(why);
        sz.on_hover_text(why);
    }
}

// ------------------------------------------------------------------ typed-path prompt

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathPurpose {
    Add,
    Change(usize),
}

pub struct PathPrompt {
    pub purpose: PathPurpose,
    pub text: String,
    pub error: Option<String>,
    pub checking: bool,
    focus_first: bool,
}

impl PathPrompt {
    pub fn new(purpose: PathPurpose) -> PathPrompt {
        PathPrompt {
            purpose,
            text: String::new(),
            error: None,
            checking: false,
            focus_first: true,
        }
    }
}

pub enum PathAnswer {
    Open,
    Cancel,
    Confirm(String),
}

/// Folder prompt for machines without kdialog / zenity.
pub fn path_dialog(ctx: &Context, p: &mut PathPrompt) -> PathAnswer {
    let pal = Palette::of_ctx(ctx);
    let title = match p.purpose {
        PathPurpose::Add => "Add project folder",
        PathPurpose::Change(_) => "Change project folder",
    };
    let ok = !p.text.trim().is_empty() && !p.checking;
    let answer = show_dialog(
        ctx,
        &Dialog {
            id: "sync-path",
            title,
            width: 460.0,
            confirm_label: "Add",
            confirm_enabled: ok,
            danger: false,
            enter_confirms: true,
        },
        |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new("Type the project folder (absolute, or starting with ~/).")
                        .color(pal.weak),
                )
                .wrap_mode(egui::TextWrapMode::Wrap),
            );
            ui.add_space(10.0);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut p.text)
                    .id(Id::unique("omp-deck-sync-path"))
                    .hint_text("~/projects/app")
                    .font(egui::TextStyle::Monospace)
                    .desired_width(ui.available_width())
                    .margin(Margin::symmetric(10, 8)),
            );
            if p.focus_first {
                resp.request_focus();
                p.focus_first = false;
            }
            if resp.changed() {
                p.error = None;
            }
            if p.checking {
                ui.add_space(6.0);
                ui.label(RichText::new("Checking folder…").small().color(pal.weak));
            } else if let Some(e) = &p.error {
                ui.add_space(6.0);
                ui.label(RichText::new(e).small().color(pal.danger));
            }
        },
    );
    match answer {
        Some(true) => PathAnswer::Confirm(p.text.clone()),
        Some(false) => PathAnswer::Cancel,
        None => PathAnswer::Open,
    }
}

// ------------------------------------------------------------------ sidebar backup dialog

pub struct BackupDialog {
    pub path: std::path::PathBuf,
    pub display: String,
    pub omp: bool,
    pub sessions: bool,
    pub keep: bool,
}

pub enum BackupAnswer {
    Open,
    Cancel,
    Run,
}

/// "Back up to Google Drive": both parts with their sizes, "Keep this folder in Sync", Back up now.
pub fn backup_dialog(
    ctx: &Context,
    d: &mut BackupDialog,
    cells: &Cells,
    busy: bool,
) -> BackupAnswer {
    let pal = Palette::of_ctx(ctx);
    let any = (d.omp && cells.omp_usable()) || (d.sessions && cells.sessions_usable());
    let answer = show_dialog(
        ctx,
        &Dialog {
            id: "sync-backup",
            title: "Back up to Google Drive",
            width: 480.0,
            confirm_label: "Back up now",
            confirm_enabled: any && !busy,
            danger: false,
            enter_confirms: false,
        },
        |ui| {
            ui.horizontal(|ui| {
                ui.add(icon_label(ICON_FOLDER, pal.glyph));
                ui.add(
                    egui::Label::new(RichText::new(&d.display).color(pal.text_strong)).truncate(),
                );
            });
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                let (t, why) = cells.omp_text();
                part_switch(
                    ui,
                    ".omp folder",
                    d.omp,
                    cells.omp_usable(),
                    &t,
                    why,
                    |on| d.omp = on,
                );
            });
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                let (t, why) = cells.sessions_text();
                part_switch(
                    ui,
                    "Sessions",
                    d.sessions,
                    cells.sessions_usable(),
                    &t,
                    why,
                    |on| {
                        d.sessions = on;
                    },
                );
            });
            ui.add_space(10.0);
            ui.checkbox(&mut d.keep, "Keep this folder in Sync")
                .on_hover_text("Adds the folder to the Sync page so it is backed up automatically");
            ui.add_space(6.0);
            weak_note(ui, NOTE);
            if busy {
                ui.add_space(6.0);
                weak_note(
                    ui,
                    "Another project backup or restore is running; wait for it to finish.",
                );
            }
        },
    );
    match answer {
        Some(true) => BackupAnswer::Run,
        Some(false) => BackupAnswer::Cancel,
        None => BackupAnswer::Open,
    }
}

// ------------------------------------------------------------------ restore dialog

/// One uploaded version of a project.
pub struct VersionRow {
    /// Index into the app's backup list.
    pub backup: usize,
    pub when: String,
    /// ".omp 24 KiB · 3 sessions 1.2 MB".
    pub summary: String,
    pub has_omp: bool,
    pub has_sessions: bool,
}

/// A project on Drive: every version of one (host, path).
pub struct CloudProject {
    pub name: String,
    pub path: String,
    pub host: String,
    pub this_pc: bool,
    /// Newest first.
    pub versions: Vec<VersionRow>,
}

pub enum ListState {
    Loading,
    Failed(String),
    Ready(Vec<CloudProject>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetStatus {
    Unknown,
    Exists,
    Missing,
}

/// What the confirmation step shows.
pub struct PlanView {
    pub target: String,
    pub create_target: bool,
    pub omp_exists: bool,
    pub omp_overwritten_total: usize,
    /// The first few overwritten paths.
    pub omp_overwritten: Vec<String>,
    pub omp_added: usize,
    /// ".omp files that will NOT be written" ("<path>: <reason>").
    pub omp_skipped: Vec<String>,
    /// Sessions that cannot be restored ("<title>: <reason>").
    pub session_problems: Vec<String>,
    /// `Some((sessions, with a new id))` when sessions are restored.
    pub sessions: Option<(usize, usize)>,
    pub safety: String,
}

impl PlanView {
    pub fn danger(&self) -> bool {
        self.omp_exists && self.omp_overwritten_total > 0
    }
}

pub struct ReportView {
    pub summary: String,
    pub failures: Vec<(String, String)>,
    /// ".omp files that were not written" ("<path>: <reason>").
    pub notes: Vec<String>,
}

pub enum Step {
    Pick,
    /// Downloading and checking.
    Preparing,
    Confirm(PlanView),
    Applying,
    Report(ReportView),
}

pub struct RestoreDialog {
    pub step: Step,
    pub list: ListState,
    /// (project, version) within `list`.
    pub sel: Option<(usize, usize)>,
    pub target: String,
    /// Restoring from the sidebar: the target folder is the project's and cannot change.
    pub target_fixed: bool,
    pub target_status: TargetStatus,
    pub omp: bool,
    pub sessions: bool,
    /// omp was found (sessions are restored through it).
    pub omp_available: bool,
    pub picker: bool,
    pub picking: bool,
    /// The user pressed Cancel during the apply step.
    pub cancelling: bool,
    /// Why the last prepare failed (shown in the dialog before any confirmation).
    pub error: Option<String>,
    focus_target: bool,
}

impl RestoreDialog {
    pub fn new(
        target_fixed: bool,
        target: String,
        omp_available: bool,
        picker: bool,
    ) -> RestoreDialog {
        RestoreDialog {
            step: Step::Pick,
            list: ListState::Loading,
            sel: None,
            target,
            target_fixed,
            target_status: TargetStatus::Unknown,
            omp: false,
            sessions: false,
            omp_available,
            picker,
            picking: false,
            cancelling: false,
            error: None,
            focus_target: true,
        }
    }

    pub fn version(&self) -> Option<&VersionRow> {
        let ListState::Ready(projects) = &self.list else {
            return None;
        };
        let (p, v) = self.sel?;
        projects.get(p)?.versions.get(v)
    }

    pub fn can_continue(&self, lane_busy: bool) -> bool {
        !lane_busy
            && self.version().is_some()
            && (self.omp || self.sessions)
            && !self.target.trim().is_empty()
    }
}

#[derive(Debug, PartialEq)]
pub enum RestoreAction {
    Cancel,
    Retry,
    Browse,
    /// The target text was edited.
    TargetEdited,
    Select(usize, usize),
    Continue,
    Confirm,
    Close,
}

/// The restore dialog; `progress` is the running prepare/apply job, `lane_busy` another project job.
pub fn restore_dialog(
    ctx: &Context,
    d: &mut RestoreDialog,
    progress: Option<&ProgView>,
    lane_busy: bool,
) -> Option<RestoreAction> {
    let pal = Palette::of_ctx(ctx);
    let mut act = None;
    let (label, enabled, danger) = match &d.step {
        Step::Pick => ("Continue", d.can_continue(lane_busy), false),
        Step::Preparing => ("Continue", false, false),
        Step::Confirm(p) => ("Restore", true, p.danger()),
        Step::Applying => ("Restore", false, false),
        Step::Report(_) => ("Close", true, false),
    };
    let answer = show_dialog(
        ctx,
        &Dialog {
            id: "sync-restore",
            title: "Restore a project from Google Drive",
            width: 560.0,
            confirm_label: label,
            confirm_enabled: enabled,
            danger,
            enter_confirms: false,
        },
        |ui| match &d.step {
            Step::Pick => pick_body(ui, d, &mut act),
            Step::Preparing => {
                progress_or_wait(ui, progress, "Downloading and checking the backup…");
            }
            Step::Confirm(plan) => confirm_body(ui, plan),
            Step::Applying => {
                progress_or_wait(
                    ui,
                    progress,
                    if d.cancelling {
                        "Stopping after the current item…"
                    } else {
                        "Restoring…"
                    },
                );
            }
            Step::Report(r) => {
                ui.label(RichText::new(&r.summary).color(pal.text));
                if !r.failures.is_empty() {
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("These sessions could not be restored:").color(pal.danger),
                    );
                    ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                        for (name, why) in &r.failures {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(format!("• {name}: {why}"))
                                        .small()
                                        .color(pal.text),
                                )
                                .wrap_mode(egui::TextWrapMode::Wrap),
                            );
                        }
                    });
                }
                if !r.notes.is_empty() {
                    ui.add_space(8.0);
                    ui.label(RichText::new("These .omp files were not written:").color(pal.warn));
                    ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
                        for n in &r.notes {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(format!("• {n}")).small().color(pal.text),
                                )
                                .wrap_mode(egui::TextWrapMode::Wrap),
                            );
                        }
                    });
                }
            }
        },
    );
    if act.is_some() {
        return act;
    }
    match (answer, &d.step) {
        (Some(true), Step::Pick) => Some(RestoreAction::Continue),
        (Some(true), Step::Confirm(_)) => Some(RestoreAction::Confirm),
        (Some(true), Step::Report(_)) | (Some(false), Step::Report(_)) => {
            Some(RestoreAction::Close)
        }
        (Some(false), _) => Some(RestoreAction::Cancel),
        _ => None,
    }
}

fn progress_or_wait(ui: &mut Ui, p: Option<&ProgView>, fallback: &str) {
    match p {
        Some(p) => progress(ui, p),
        None => progress(
            ui,
            &ProgView {
                label: fallback.to_owned(),
                fraction: None,
            },
        ),
    }
}

fn pick_body(ui: &mut Ui, d: &mut RestoreDialog, act: &mut Option<RestoreAction>) {
    let pal = Palette::of_ui(ui);
    if let Some(e) = &d.error {
        ui.horizontal(|ui| {
            ui.label(icon_text(ICON_TRIANGLE_ALERT, 16.0, pal.danger));
            ui.add(
                egui::Label::new(RichText::new(e).small().color(pal.danger))
                    .wrap_mode(egui::TextWrapMode::Wrap),
            );
        });
        ui.add_space(6.0);
    }
    match &d.list {
        ListState::Loading => {
            ui.horizontal(|ui| {
                ui.add(Spin::new(18.0, pal.accent_b));
                ui.label(RichText::new("Loading project backups…").color(pal.weak));
            });
            return;
        }
        ListState::Failed(e) => {
            ui.add(
                egui::Label::new(RichText::new(e).color(pal.danger))
                    .wrap_mode(egui::TextWrapMode::Wrap),
            );
            ui.add_space(8.0);
            if ui.button("Retry").clicked() {
                *act = Some(RestoreAction::Retry);
            }
            return;
        }
        ListState::Ready(p) if p.is_empty() => {
            let msg = if d.target_fixed {
                "No backups of this folder on Google Drive yet."
            } else {
                "No project backups on Google Drive yet."
            };
            ui.label(RichText::new(msg).color(pal.weak));
            return;
        }
        ListState::Ready(_) => {}
    }
    let ListState::Ready(projects) = &d.list else {
        return;
    };
    ui.label(RichText::new("Pick a project backup.").color(pal.weak));
    ui.add_space(6.0);
    ScrollArea::vertical().max_height(230.0).show(ui, |ui| {
        for (pi, p) in projects.iter().enumerate() {
            let open = d.sel.is_some_and(|(s, _)| s == pi);
            let host = if p.this_pc {
                "this PC".to_owned()
            } else {
                p.host.clone()
            };
            let newest = p.versions.first().map_or("", |v| v.when.as_str());
            let text = RichText::new(format!("{newest}    {host}    {}", p.name)).color(pal.text);
            let resp = ui.add(
                Button::selectable(open, text)
                    .truncate()
                    .min_size(vec2(ui.available_width(), 28.0)),
            );
            resp.clone()
                .on_hover_text(format!("{}\n{}", p.name, p.path));
            if resp.clicked() && !open {
                *act = Some(RestoreAction::Select(pi, 0));
            }
            if open {
                ui.indent(("sync-versions", pi), |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(&p.path).small().color(pal.weak)).truncate(),
                    );
                    for (vi, v) in p.versions.iter().enumerate() {
                        let sel = d.sel == Some((pi, vi));
                        let text = RichText::new(format!("{}   {}", v.when, v.summary))
                            .small()
                            .color(pal.text);
                        if ui
                            .add(
                                Button::selectable(sel, text)
                                    .truncate()
                                    .min_size(vec2(ui.available_width(), 24.0)),
                            )
                            .clicked()
                            && !sel
                        {
                            *act = Some(RestoreAction::Select(pi, vi));
                        }
                    }
                });
            }
        }
    });
    ui.add_space(12.0);
    ui.label(theme::section_caption("Restore into", pal.weak));
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let browse_w = if d.picker && !d.target_fixed {
            108.0
        } else {
            0.0
        };
        let resp = ui.add_enabled(
            !d.target_fixed,
            egui::TextEdit::singleline(&mut d.target)
                .id(Id::unique("omp-deck-sync-restore-target"))
                .hint_text("~/projects/app")
                .font(egui::TextStyle::Monospace)
                .desired_width(ui.available_width() - browse_w - 8.0)
                .margin(Margin::symmetric(10, 8)),
        );
        if d.focus_target && !d.target_fixed {
            resp.request_focus();
            d.focus_target = false;
        }
        if resp.changed() {
            *act = Some(RestoreAction::TargetEdited);
        }
        if browse_w > 0.0 {
            let label = if d.picking { "Waiting…" } else { "Browse…" };
            let btn = Button::new((
                icon_text(ICON_FOLDER_OPEN, 17.0, pal.glyph),
                RichText::new(label),
            ))
            .min_size(vec2(browse_w, 34.0));
            if ui.add_enabled(!d.picking, btn).clicked() {
                *act = Some(RestoreAction::Browse);
            }
        }
    });
    if d.target_status == TargetStatus::Missing && !d.target.trim().is_empty() {
        ui.add_space(4.0);
        ui.label(
            RichText::new("This folder does not exist yet: it will be created.")
                .small()
                .color(pal.warn),
        );
    }
    ui.add_space(10.0);
    let (has_omp, has_sessions) = d
        .version()
        .map_or((false, false), |v| (v.has_omp, v.has_sessions));
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 16.0;
        ui.add_enabled(has_omp, egui::Checkbox::new(&mut d.omp, ".omp folder"));
        let sess = ui.add_enabled(
            has_sessions && d.omp_available,
            egui::Checkbox::new(&mut d.sessions, "Sessions"),
        );
        if has_sessions && !d.omp_available {
            sess.on_disabled_hover_text("omp was not found: sessions are restored through it");
        }
    });
    if has_sessions && !d.omp_available {
        weak_note(
            ui,
            "Sessions need omp, which was not found on this computer.",
        );
    }
}

fn confirm_body(ui: &mut Ui, plan: &PlanView) {
    let pal = Palette::of_ui(ui);
    let wrap = |ui: &mut Ui, text: RichText| {
        ui.add(egui::Label::new(text).wrap_mode(egui::TextWrapMode::Wrap));
    };
    wrap(
        ui,
        RichText::new(format!("Restore into {}", plan.target)).color(pal.text_strong),
    );
    if plan.create_target {
        wrap(
            ui,
            RichText::new("The folder does not exist and will be created.")
                .small()
                .color(pal.weak),
        );
    }
    if plan.danger() {
        ui.add_space(8.0);
        Frame::NONE
            .fill(pal.danger.gamma_multiply(0.12))
            .stroke(Stroke::new(1.0, pal.danger.gamma_multiply(0.6)))
            .corner_radius(theme::rad(6, 10))
            .inner_margin(Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(icon_text(ICON_TRIANGLE_ALERT, 16.0, pal.danger));
                    ui.label(
                        theme::strong(format!(
                            "{} file{} in the existing .omp will be overwritten",
                            plan.omp_overwritten_total,
                            if plan.omp_overwritten_total == 1 {
                                ""
                            } else {
                                "s"
                            }
                        ))
                        .color(pal.danger),
                    );
                });
                for rel in &plan.omp_overwritten {
                    ui.label(RichText::new(format!("• {rel}")).small().color(pal.text));
                }
                let more = plan
                    .omp_overwritten_total
                    .saturating_sub(plan.omp_overwritten.len());
                if more > 0 {
                    ui.label(
                        RichText::new(format!("… and {more} more"))
                            .small()
                            .color(pal.weak),
                    );
                }
                ui.label(
                    RichText::new(format!(
                        "Other files are kept. Overwritten files are first copied to {}.",
                        plan.safety
                    ))
                    .small()
                    .color(pal.weak),
                );
            });
    } else if plan.omp_added > 0 {
        ui.add_space(6.0);
        wrap(
            ui,
            RichText::new(format!(
                ".omp: {} file{} will be added.",
                plan.omp_added,
                if plan.omp_added == 1 { "" } else { "s" }
            ))
            .color(pal.text),
        );
    }
    if plan.danger() && plan.omp_added > 0 {
        ui.add_space(4.0);
        wrap(
            ui,
            RichText::new(format!(
                "{} more .omp file(s) will be added.",
                plan.omp_added
            ))
            .small()
            .color(pal.text),
        );
    }
    if let Some((n, new_ids)) = plan.sessions {
        ui.add_space(8.0);
        let mut t = format!(
            "{n} session{} will be added alongside the existing ones",
            if n == 1 { "" } else { "s" }
        );
        if new_ids > 0 {
            t +=
                &format!(" ({new_ids} get a new id because a session with that id already exists)");
        }
        t.push('.');
        wrap(ui, RichText::new(t).color(pal.text));
    }
    warn_list(
        ui,
        ".omp files that will NOT be written:",
        &plan.omp_skipped,
    );
    warn_list(
        ui,
        "Sessions that cannot be restored:",
        &plan.session_problems,
    );
}

fn warn_list(ui: &mut Ui, title: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    let pal = Palette::of_ui(ui);
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label(icon_text(ICON_TRIANGLE_ALERT, 16.0, pal.warn));
        ui.label(theme::strong(title).color(pal.warn));
    });
    for it in items.iter().take(PLAN_LIST_MAX) {
        ui.add(
            egui::Label::new(RichText::new(format!("• {it}")).small().color(pal.text))
                .wrap_mode(egui::TextWrapMode::Wrap),
        );
    }
    if items.len() > PLAN_LIST_MAX {
        ui.label(
            RichText::new(format!("… and {} more", items.len() - PLAN_LIST_MAX))
                .small()
                .color(pal.weak),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn plan_for_render() -> PlanView {
        plan(true, 1)
    }

    fn plan(exists: bool, overwritten: usize) -> PlanView {
        PlanView {
            target: "~/x".into(),
            create_target: false,
            omp_exists: exists,
            omp_overwritten_total: overwritten,
            omp_overwritten: Vec::new(),
            omp_added: 0,
            omp_skipped: Vec::new(),
            session_problems: Vec::new(),
            sessions: None,
            safety: String::new(),
        }
    }

    #[test]
    fn overwriting_an_existing_omp_is_dangerous_only_with_overlap() {
        assert!(plan(true, 2).danger());
        assert!(!plan(true, 0).danger());
        assert!(!plan(false, 0).danger());
    }

    #[test]
    fn continue_needs_a_version_a_part_and_a_target() {
        let mut d = RestoreDialog::new(false, "/tmp/x".into(), true, false);
        assert!(!d.can_continue(false));
        d.list = ListState::Ready(vec![CloudProject {
            name: "app".into(),
            path: "/w/app".into(),
            host: "h".into(),
            this_pc: true,
            versions: vec![VersionRow {
                backup: 0,
                when: "now".into(),
                summary: String::new(),
                has_omp: true,
                has_sessions: false,
            }],
        }]);
        d.sel = Some((0, 0));
        assert!(!d.can_continue(false), "no part chosen yet");
        d.omp = true;
        assert!(d.can_continue(false));
        assert!(!d.can_continue(true), "the lane is busy");
        d.target = "  ".into();
        assert!(!d.can_continue(false));
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;

    /// Runs `f` for a few frames in a headless egui context.
    fn frames(mut f: impl FnMut(&mut Ui, &Context)) {
        let ctx = Context::default();
        theme::apply(
            &ctx,
            deck_core::config::UiStyle::Neutral,
            deck_core::config::Density::Compact,
            true,
        );
        for _ in 0..3 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                let c = ui.ctx().clone();
                f(ui, &c);
            });
            out.textures_delta.clear();
        }
    }

    fn cells() -> Cells {
        Cells {
            omp: OmpCell::Present("1 KB · 1 file".into()),
            sessions: SessionsCell::Ready {
                count: 2,
                text: "2 sessions · 3 KB".into(),
            },
        }
    }

    #[test]
    fn page_reports_visibility_every_frame_and_stays_idle() {
        let rows = [RowView {
            display: "~/app".into(),
            full: "/home/u/app".into(),
            omp_on: true,
            sessions_on: true,
            cells: cells(),
            last: LastView::Failed("offline".into()),
            busy: Some(ProgView {
                label: "Uploading 40%".into(),
                fraction: Some(0.4),
            }),
        }];
        let account = DriveCard::SignedIn {
            email: Some("a@b.c".into()),
            last: None,
            auto: false,
            busy: None,
        };
        let page_state = SyncPage {
            account: &account,
            rows: &rows,
            interval: SyncInterval::Daily,
            lane_busy: true,
            picking: false,
        };
        frames(|ui, _| {
            let actions = page(ui, &page_state);
            assert_eq!(actions, [SyncAction::Visible]);
        });
    }

    #[test]
    fn every_restore_step_renders() {
        let mut d = RestoreDialog::new(false, "/tmp/x".into(), true, true);
        d.list = ListState::Ready(vec![CloudProject {
            name: "app".into(),
            path: "/w/app".into(),
            host: "h".into(),
            this_pc: false,
            versions: vec![VersionRow {
                backup: 0,
                when: "2026-10-22".into(),
                summary: ".omp 1 KB".into(),
                has_omp: true,
                has_sessions: true,
            }],
        }]);
        d.sel = Some((0, 0));
        d.target_status = TargetStatus::Missing;
        let progress = ProgView {
            label: "Downloading 10%".into(),
            fraction: Some(0.1),
        };
        let steps = [
            Step::Pick,
            Step::Preparing,
            Step::Confirm(PlanView {
                omp_overwritten: vec!["a.md".into()],
                omp_overwritten_total: 5,
                sessions: Some((2, 1)),
                ..super::tests::plan_for_render()
            }),
            Step::Applying,
            Step::Report(ReportView {
                summary: "done".into(),
                failures: vec![("t".into(), "why".into())],
                notes: vec!["a.md: symlink".into()],
            }),
        ];
        for step in steps {
            d.step = step;
            frames(|_, ctx| {
                restore_dialog(ctx, &mut d, Some(&progress), false);
            });
        }
        d.list = ListState::Failed("offline".into());
        d.step = Step::Pick;
        frames(|_, ctx| {
            restore_dialog(ctx, &mut d, None, false);
        });
    }

    #[test]
    fn backup_and_path_dialogs_render_and_stay_open() {
        let mut b = BackupDialog {
            path: "/w/app".into(),
            display: "~/app".into(),
            omp: true,
            sessions: true,
            keep: true,
        };
        let mut p = PathPrompt::new(PathPurpose::Add);
        frames(|_, ctx| {
            assert!(matches!(
                backup_dialog(ctx, &mut b, &cells(), false),
                BackupAnswer::Open
            ));
            assert!(matches!(path_dialog(ctx, &mut p), PathAnswer::Open));
        });
    }
}
