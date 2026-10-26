//! The "Google Drive backup" settings card, the restore picker and the "Manage settings backups"
//! dialog (pure UI: state in, actions out; the logic lives in `app/drive.rs` and
//! `deck_core::gdrive`).

use std::collections::HashSet;

use deck_core::config::SyncInterval;
use egui::{Button, Context, RichText, ScrollArea, Ui, vec2};

use crate::confirm::{Dialog, show_dialog, show_dialog_labeled};
use crate::icons::{
    ICON_ARCHIVE_RESTORE, ICON_CLOUD_UPLOAD, ICON_HARD_DRIVE, ICON_LIST_CHECKS, ICON_LOG_IN,
    ICON_LOG_OUT, ICON_REFRESH_CW, ICON_TRASH_2, ICON_X,
};
use crate::theme::{self, Palette};
use crate::widgets::{Spin, icon_text, setting_row};

/// What the card shows.
#[derive(Clone, Debug, PartialEq)]
pub enum DriveCard {
    /// This build has no Google OAuth client.
    Unavailable,
    SignedOut,
    /// The consent page is open; the loopback waits for the browser.
    Waiting,
    SignedIn {
        email: Option<String>,
        /// "2026-10-22 13:52:01 UTC from host", `None` = no backup from this computer yet.
        last: Option<String>,
        interval: SyncInterval,
        busy: Option<Busy>,
    },
}

/// The running Drive job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Busy {
    BackingUp,
    Listing,
    Preparing,
    Restoring,
    SigningOut,
    /// Deleting backups chosen in the manager or the restore picker.
    Deleting,
}

#[derive(Debug, PartialEq)]
pub enum DriveAction {
    SignIn,
    CancelSignIn,
    SetInterval(SyncInterval),
    BackupNow,
    OpenRestore,
    /// "Manage backups…": the dialog listing every config backup for deletion.
    OpenManager,
    SignOut,
}

fn button(ui: &mut Ui, enabled: bool, icon: crate::icons::Icon, label: &str) -> egui::Response {
    let pal = Palette::of_ui(ui);
    ui.add_enabled(
        enabled,
        Button::new((icon_text(icon, 16.0, pal.glyph), RichText::new(label)))
            .min_size(vec2(0.0, 30.0)),
    )
}

const MANAGE_TITLE: &str = "Backups on Google Drive";
const MANAGE_SUB: &str = "Delete old versions or another PC's backups (the newest 30 are kept)";

/// A wrapped explanation under a row.
fn note(ui: &mut Ui, text: &str) {
    let pal = Palette::of_ui(ui);
    ui.add(
        egui::Label::new(RichText::new(text).small().color(pal.weak))
            .wrap_mode(egui::TextWrapMode::Wrap),
    );
}

/// The card body (inside `widgets::card`).
pub fn card(ui: &mut Ui, state: &DriveCard) -> Vec<DriveAction> {
    let pal = Palette::of_ui(ui);
    let mut actions = Vec::new();
    match state {
        DriveCard::Unavailable => {
            setting_row(
                ui,
                "Not available in this build",
                "This OmpDeck was built without a Google OAuth client",
                |_| {},
            );
            note(
                ui,
                "Set OMP_DECK_GOOGLE_CLIENT_ID and OMP_DECK_GOOGLE_CLIENT_SECRET to enable it.",
            );
        }
        DriveCard::SignedOut => {
            setting_row(
                ui,
                "Back up your omp config to Google Drive",
                "Settings, models, custom agents, skills, … and prompt history",
                |ui| {
                    if button(ui, true, ICON_LOG_IN, "Sign in with Google").clicked() {
                        actions.push(DriveAction::SignIn);
                    }
                },
            );
            note(
                ui,
                "Stored in a private app folder only OmpDeck can see. Logins and sessions stay on \
                 this computer.",
            );
            ui.add_space(4.0);
            setting_row(ui, MANAGE_TITLE, MANAGE_SUB, |ui| {
                button(ui, false, ICON_LIST_CHECKS, "Manage backups…")
                    .on_disabled_hover_text("Sign in with Google first");
            });
        }
        DriveCard::Waiting => setting_row(
            ui,
            "Waiting for the browser…",
            "Finish signing in on the Google page that just opened.",
            |ui| {
                if button(ui, true, ICON_X, "Cancel").clicked() {
                    actions.push(DriveAction::CancelSignIn);
                }
                ui.add(Spin::new(18.0, pal.accent_b));
            },
        ),
        DriveCard::SignedIn {
            email,
            last,
            interval,
            busy,
        } => {
            let idle = busy.is_none();
            let who = email
                .as_deref()
                .map_or_else(|| "Signed in".to_owned(), |e| format!("Signed in as {e}"));
            setting_row(
                ui,
                &who,
                "Backups live in OmpDeck's private app folder on your Drive",
                |ui| {
                    if *busy == Some(Busy::SigningOut) {
                        ui.add(Spin::new(18.0, pal.accent_b));
                    } else if button(ui, idle, ICON_LOG_OUT, "Sign out").clicked() {
                        actions.push(DriveAction::SignOut);
                    }
                },
            );
            ui.add_space(4.0);
            setting_row(
                ui,
                "Back up automatically",
                "Checks on this schedule and uploads when the backed-up files changed",
                |ui| {
                    egui::ComboBox::from_id_salt("drive-backup-interval")
                        .selected_text(interval.label())
                        .show_ui(ui, |ui| {
                            for v in SyncInterval::ALL {
                                if ui.selectable_label(*interval == v, v.label()).clicked()
                                    && *interval != v
                                {
                                    actions.push(DriveAction::SetInterval(v));
                                }
                            }
                        });
                },
            );
            ui.add_space(4.0);
            let sub = last
                .as_deref()
                .unwrap_or("No backup from this computer yet");
            setting_row(ui, "Last backup", sub, |ui| {
                let restoring = matches!(
                    busy,
                    Some(Busy::Listing | Busy::Preparing | Busy::Restoring)
                );
                if restoring {
                    ui.add(Spin::new(18.0, pal.accent_b));
                } else if button(ui, idle, ICON_ARCHIVE_RESTORE, "Restore…").clicked() {
                    actions.push(DriveAction::OpenRestore);
                }
                if *busy == Some(Busy::BackingUp) {
                    ui.add(Spin::new(18.0, pal.accent_b));
                } else if button(ui, idle, ICON_CLOUD_UPLOAD, "Back up now").clicked() {
                    actions.push(DriveAction::BackupNow);
                }
            });
            ui.add_space(4.0);
            setting_row(ui, MANAGE_TITLE, MANAGE_SUB, |ui| {
                if *busy == Some(Busy::Deleting) {
                    ui.add(Spin::new(18.0, pal.accent_b));
                } else if button(ui, idle, ICON_LIST_CHECKS, "Manage backups…")
                    .on_disabled_hover_text("Wait for the running Google Drive job to finish")
                    .clicked()
                {
                    actions.push(DriveAction::OpenManager);
                }
            });
        }
    }
    actions
}

/// One backup in the picker, pre-formatted.
pub struct RestoreRow {
    pub when: String,
    pub host: String,
    pub size: String,
}

pub enum RestoreList<'a> {
    Loading,
    Failed(&'a str),
    Rows(&'a [RestoreRow]),
}

pub enum RestorePick {
    Cancel,
    Retry,
    Choose(usize),
    /// The trash button of a row: ask before deleting it from Drive.
    Delete(usize),
}

/// The modal listing backups newest first; `preparing` = the chosen one is being downloaded,
/// `busy` = some Google Drive job runs (no deleting meanwhile).
pub fn restore_dialog(
    ctx: &Context,
    list: &RestoreList<'_>,
    selected: &mut Option<usize>,
    preparing: bool,
    busy: bool,
) -> Option<RestorePick> {
    let mut retry = false;
    let mut delete = None;
    let answer = show_dialog(
        ctx,
        &Dialog {
            id: "drive-restore",
            title: "Restore from Google Drive",
            width: 520.0,
            confirm_label: "Restore…",
            confirm_enabled: selected.is_some() && !preparing,
            danger: false,
            enter_confirms: false,
        },
        |ui| {
            let pal = Palette::of_ui(ui);
            match list {
                RestoreList::Loading => {
                    ui.horizontal(|ui| {
                        ui.add(Spin::new(18.0, pal.accent_b));
                        ui.label(RichText::new("Loading backups…").color(pal.weak));
                    });
                }
                RestoreList::Failed(err) => {
                    ui.add(
                        egui::Label::new(RichText::new(*err).color(pal.danger))
                            .wrap_mode(egui::TextWrapMode::Wrap),
                    );
                    ui.add_space(8.0);
                    retry = ui.button("Retry").clicked();
                }
                RestoreList::Rows([]) => {
                    ui.label(RichText::new("No backups on Google Drive yet.").color(pal.weak));
                }
                RestoreList::Rows(rows) => {
                    ui.label(
                        RichText::new("Newest first. Pick the backup to restore.").color(pal.weak),
                    );
                    ui.add_space(6.0);
                    ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for (i, r) in rows.iter().enumerate() {
                            let text = format!("{}    {}    {}", r.when, r.host, r.size);
                            let label = RichText::new(text).color(pal.text);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                if ui
                                    .add_enabled(
                                        !busy,
                                        Button::new(icon_text(ICON_TRASH_2, 14.0, pal.danger))
                                            .min_size(vec2(28.0, 28.0)),
                                    )
                                    .on_hover_text("Delete this backup from Google Drive")
                                    .on_disabled_hover_text("A Google Drive job is running")
                                    .clicked()
                                {
                                    delete = Some(i);
                                }
                                let resp = ui.add_enabled(
                                    !preparing,
                                    Button::selectable(*selected == Some(i), label)
                                        .min_size(vec2(ui.available_width(), 28.0)),
                                );
                                if resp.clicked() {
                                    *selected = Some(i);
                                }
                            });
                        }
                    });
                    if preparing {
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.add(Spin::new(18.0, pal.accent_b));
                            ui.label(
                                RichText::new("Downloading and checking the backup…")
                                    .color(pal.weak),
                            );
                        });
                    }
                }
            }
        },
    );
    if retry {
        return Some(RestorePick::Retry);
    }
    if let Some(i) = delete {
        return Some(RestorePick::Delete(i));
    }
    match answer {
        Some(true) => selected.map(RestorePick::Choose),
        Some(false) => Some(RestorePick::Cancel),
        None => None,
    }
}

// ------------------------------------------------------------------ backup manager

/// One config backup in the manager, pre-formatted.
#[derive(Clone, Debug, PartialEq)]
pub struct ManagedBackup {
    /// Index into the app's backup list.
    pub index: usize,
    /// `2026-10-22 13:52:01 UTC`.
    pub when: String,
    /// `3h ago`.
    pub age: String,
    pub bytes: u64,
    /// Human size, empty when Drive reported none.
    pub size: String,
    /// The newest backup on Drive.
    pub latest: bool,
    /// `• <when> · <host>` for the confirmation.
    pub confirm_line: String,
}

/// Every backup one PC uploaded, newest first.
#[derive(Clone, Debug, PartialEq)]
pub struct HostGroup {
    pub host: String,
    pub this_pc: bool,
    pub versions: Vec<ManagedBackup>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ManagerList {
    Loading,
    Failed(String),
    Ready(Vec<HostGroup>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagerStep {
    Browse,
    /// "Delete permanently?" with the totals.
    Confirm,
    Deleting,
}

/// State of the "Manage settings backups" dialog.
pub struct BackupManager {
    pub step: ManagerStep,
    list: ManagerList,
    /// Checked backups, by index into the app's backup list.
    checked: HashSet<usize>,
    /// (count, bytes) of `checked`, and its footer text; recomputed on change, not per frame.
    totals: (usize, u64),
    totals_label: String,
    /// The user pressed Cancel while deleting.
    pub cancelling: bool,
    /// `<backup>: <error>` of the last deletion run that failed for some.
    pub failures: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub enum ManagerAction {
    /// Close (browse), stop deleting (deleting).
    Cancel,
    Refresh,
    Toggle(usize, bool),
    ToggleGroup(usize, bool),
    /// Go to the confirmation.
    AskDelete,
    /// Back from the confirmation to the list.
    Back,
    Delete,
}

impl BackupManager {
    pub fn new() -> BackupManager {
        let mut m = BackupManager {
            step: ManagerStep::Browse,
            list: ManagerList::Loading,
            checked: HashSet::new(),
            totals: (0, 0),
            totals_label: String::new(),
            cancelling: false,
            failures: Vec::new(),
        };
        m.recount();
        m
    }

    /// (checked backups, their total size).
    pub fn totals(&self) -> (usize, u64) {
        self.totals
    }

    /// The checked indices into the app's backup list, ascending (= newest first).
    pub fn checked(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self.checked.iter().copied().collect();
        v.sort_unstable();
        v
    }

    pub fn set_version(&mut self, index: usize, on: bool) {
        if on {
            self.checked.insert(index);
        } else {
            self.checked.remove(&index);
        }
        self.recount();
    }

    pub fn set_group(&mut self, group: usize, on: bool) {
        let ManagerList::Ready(groups) = &self.list else {
            return;
        };
        let Some(g) = groups.get(group) else {
            return;
        };
        for v in &g.versions {
            if on {
                self.checked.insert(v.index);
            } else {
                self.checked.remove(&v.index);
            }
        }
        self.recount();
    }

    /// Replaces the list. Selections are indices into the app's list, which a refresh renumbers,
    /// so they are dropped.
    pub fn set_list(&mut self, list: ManagerList) {
        self.checked.clear();
        self.list = list;
        self.recount();
    }

    fn recount(&mut self) {
        self.totals = match &self.list {
            ManagerList::Ready(groups) => groups
                .iter()
                .flat_map(|g| &g.versions)
                .filter(|v| self.checked.contains(&v.index))
                .fold((0, 0), |(n, b), v| (n + 1, b + v.bytes)),
            _ => (0, 0),
        };
        self.totals_label = format!(
            "{} selected · {}",
            self.totals.0,
            crate::app::human_size(self.totals.1)
        );
    }
}

impl Default for BackupManager {
    fn default() -> Self {
        Self::new()
    }
}

/// "Manage settings backups": every config backup on Drive, PC → versions, with checkboxes.
/// `progress` = (deleted so far, total) while deleting; `busy` = another Google Drive job runs.
pub fn manager_dialog(
    ctx: &Context,
    d: &BackupManager,
    progress: Option<(usize, usize)>,
    busy: bool,
) -> Option<ManagerAction> {
    let pal = Palette::of_ctx(ctx);
    let mut act = None;
    let (selected, bytes) = d.totals;
    let (label, enabled, danger) = match d.step {
        ManagerStep::Browse => (
            "Delete selected…",
            selected > 0 && !busy && matches!(d.list, ManagerList::Ready(_)),
            false,
        ),
        ManagerStep::Confirm => ("Delete permanently", true, true),
        ManagerStep::Deleting => ("Delete permanently", false, true),
    };
    let cancel_label = match (&d.step, &d.list) {
        (ManagerStep::Browse, ManagerList::Failed(_)) => "Close",
        (ManagerStep::Browse, ManagerList::Ready(_)) if selected == 0 => "Close",
        (ManagerStep::Confirm, _) => "Back",
        _ => "Cancel",
    };
    let answer = show_dialog_labeled(
        ctx,
        &Dialog {
            id: "drive-manager",
            title: "Manage settings backups",
            width: 560.0,
            confirm_label: label,
            confirm_enabled: enabled,
            danger,
            enter_confirms: false,
        },
        cancel_label,
        |ui| match d.step {
            ManagerStep::Browse => manager_body(ui, d, busy, &mut act),
            ManagerStep::Confirm => confirm_body(ui, d, selected, bytes),
            ManagerStep::Deleting => {
                let text = if d.cancelling {
                    "Stopping after the current backup…".to_owned()
                } else if let Some((done, total)) = progress {
                    format!("Deleting {} of {total}", (done + 1).min(total))
                } else {
                    "Deleting…".to_owned()
                };
                ui.horizontal(|ui| {
                    ui.add(Spin::new(16.0, pal.accent_b));
                    ui.label(RichText::new(text).color(pal.weak));
                });
                if let Some((done, total)) = progress {
                    ui.add(
                        egui::ProgressBar::new(done as f32 / total.max(1) as f32)
                            .desired_height(6.0)
                            .desired_width(ui.available_width()),
                    );
                }
            }
        },
    );
    if act.is_some() {
        return act;
    }
    match (answer, d.step) {
        (Some(true), ManagerStep::Browse) => Some(ManagerAction::AskDelete),
        (Some(true), ManagerStep::Confirm) => Some(ManagerAction::Delete),
        (Some(false), ManagerStep::Confirm) => Some(ManagerAction::Back),
        (Some(false), _) => Some(ManagerAction::Cancel),
        _ => None,
    }
}

fn wrapped(ui: &mut Ui, text: RichText) {
    ui.add(egui::Label::new(text).wrap_mode(egui::TextWrapMode::Wrap));
}

fn confirm_body(ui: &mut Ui, d: &BackupManager, selected: usize, bytes: u64) {
    let pal = Palette::of_ui(ui);
    wrapped(
        ui,
        RichText::new(format!(
            "Permanently delete {selected} settings backup{} ({}) from Google Drive?",
            if selected == 1 { "" } else { "s" },
            crate::app::human_size(bytes)
        ))
        .color(pal.text_strong),
    );
    ui.add_space(4.0);
    wrapped(
        ui,
        RichText::new("This can't be undone. The files do not go to the trash.").color(pal.danger),
    );
    let ManagerList::Ready(groups) = &d.list else {
        return;
    };
    ui.add_space(8.0);
    ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
        for v in groups
            .iter()
            .flat_map(|g| &g.versions)
            .filter(|v| d.checked.contains(&v.index))
        {
            ui.add(
                egui::Label::new(RichText::new(&v.confirm_line).small().color(pal.text)).truncate(),
            );
        }
    });
}

fn manager_body(ui: &mut Ui, d: &BackupManager, busy: bool, act: &mut Option<ManagerAction>) {
    let pal = Palette::of_ui(ui);
    let groups = match &d.list {
        ManagerList::Loading => {
            ui.horizontal(|ui| {
                ui.add(Spin::new(18.0, pal.accent_b));
                ui.label(RichText::new("Loading backups…").color(pal.weak));
            });
            return;
        }
        ManagerList::Failed(e) => {
            wrapped(ui, RichText::new(e).color(pal.danger));
            ui.add_space(8.0);
            if ui.button("Retry").clicked() {
                *act = Some(ManagerAction::Refresh);
            }
            return;
        }
        ManagerList::Ready(g) => g,
    };
    if !d.failures.is_empty() {
        let n = d.failures.len();
        wrapped(
            ui,
            RichText::new(format!(
                "{n} backup{} could not be deleted:",
                if n == 1 { "" } else { "s" }
            ))
            .color(pal.danger),
        );
        for f in &d.failures {
            wrapped(ui, RichText::new(f).small().color(pal.danger));
        }
        ui.add_space(6.0);
    }
    if groups.is_empty() {
        ui.label(RichText::new("No settings backups on Google Drive.").color(pal.weak));
        ui.add_space(8.0);
        if ui.add_enabled(!busy, Button::new("Refresh")).clicked() {
            *act = Some(ManagerAction::Refresh);
        }
        return;
    }
    wrapped(
        ui,
        RichText::new("Grouped by the PC that uploaded them, newest first.").color(pal.weak),
    );
    ui.add_space(6.0);
    ScrollArea::vertical().max_height(340.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        for (gi, g) in groups.iter().enumerate() {
            if gi > 0 {
                ui.add_space(6.0);
            }
            let mut on = g.versions.iter().all(|v| d.checked.contains(&v.index));
            ui.horizontal(|ui| {
                if ui
                    .checkbox(&mut on, "")
                    .on_hover_text("Select every backup from this PC")
                    .changed()
                {
                    *act = Some(ManagerAction::ToggleGroup(gi, on));
                }
                ui.label(icon_text(ICON_HARD_DRIVE, 16.0, pal.glyph));
                ui.label(theme::strong(&g.host).color(pal.text_strong));
                if g.this_pc {
                    ui.label(RichText::new("this PC").small().color(pal.accent_b));
                }
            });
            ui.indent(("drive-mgr-versions", gi), |ui| {
                for v in &g.versions {
                    let mut on = d.checked.contains(&v.index);
                    ui.horizontal(|ui| {
                        if ui
                            .checkbox(&mut on, RichText::new(&v.when).small().color(pal.text))
                            .changed()
                        {
                            *act = Some(ManagerAction::Toggle(v.index, on));
                        }
                        ui.label(RichText::new(&v.age).small().color(pal.weak));
                        if !v.size.is_empty() {
                            ui.label(RichText::new(&v.size).small().color(pal.text));
                        }
                        if v.latest {
                            ui.label(RichText::new("latest").small().color(pal.accent_b))
                                .on_hover_text("The newest settings backup on Google Drive");
                        }
                    });
                }
            });
        }
    });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                !busy,
                Button::new((
                    icon_text(ICON_REFRESH_CW, 14.0, pal.glyph),
                    RichText::new("Refresh"),
                )),
            )
            .clicked()
        {
            *act = Some(ManagerAction::Refresh);
        }
        ui.label(RichText::new(&d.totals_label).color(pal.text));
    });
    if busy {
        wrapped(
            ui,
            RichText::new("Another Google Drive job is running; wait for it to finish.")
                .small()
                .color(pal.weak),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(index: usize, bytes: u64) -> ManagedBackup {
        ManagedBackup {
            index,
            when: format!("t{index}"),
            age: String::new(),
            bytes,
            size: String::new(),
            latest: index == 0,
            confirm_line: String::new(),
        }
    }

    #[test]
    fn manager_selection_totals_groups_and_resets_on_refresh() {
        let mut d = BackupManager::new();
        d.set_list(ManagerList::Ready(vec![
            HostGroup {
                host: "me".into(),
                this_pc: true,
                versions: vec![version(0, 10), version(2, 20)],
            },
            HostGroup {
                host: "other".into(),
                this_pc: false,
                versions: vec![version(1, 5)],
            },
        ]));
        assert_eq!(d.totals(), (0, 0));
        d.set_group(0, true);
        assert_eq!(d.totals(), (2, 30));
        d.set_version(1, true);
        d.set_version(2, false);
        assert_eq!(d.totals(), (2, 15));
        assert_eq!(d.checked(), vec![0, 1]);
        assert_eq!(d.totals_label, "2 selected · 15 B");
        d.set_group(0, false);
        assert_eq!(d.checked(), vec![1]);
        d.set_list(ManagerList::Loading);
        assert_eq!(d.totals(), (0, 0));
        assert!(d.checked().is_empty());
    }
}
