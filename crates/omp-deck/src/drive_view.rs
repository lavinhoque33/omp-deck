//! The "Google Drive backup" settings card and the restore picker (pure UI: state in, actions out;
//! the logic lives in `app/drive.rs` and `deck_core::gdrive`).

use deck_core::config::SyncInterval;
use egui::{Button, Context, RichText, ScrollArea, Ui, vec2};

use crate::confirm::{Dialog, show_dialog};
use crate::icons::{ICON_ARCHIVE_RESTORE, ICON_CLOUD_UPLOAD, ICON_LOG_IN, ICON_LOG_OUT, ICON_X};
use crate::theme::Palette;
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
}

#[derive(Debug, PartialEq)]
pub enum DriveAction {
    SignIn,
    CancelSignIn,
    SetInterval(SyncInterval),
    BackupNow,
    OpenRestore,
    SignOut,
}

fn button(ui: &mut Ui, enabled: bool, icon: crate::icons::Icon, label: &str) -> bool {
    let pal = Palette::of_ui(ui);
    ui.add_enabled(
        enabled,
        Button::new((icon_text(icon, 16.0, pal.glyph), RichText::new(label)))
            .min_size(vec2(0.0, 30.0)),
    )
    .clicked()
}

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
                    if button(ui, true, ICON_LOG_IN, "Sign in with Google") {
                        actions.push(DriveAction::SignIn);
                    }
                },
            );
            note(
                ui,
                "Stored in a private app folder only OmpDeck can see. Logins and sessions stay on \
                 this computer.",
            );
        }
        DriveCard::Waiting => setting_row(
            ui,
            "Waiting for the browser…",
            "Finish signing in on the Google page that just opened.",
            |ui| {
                if button(ui, true, ICON_X, "Cancel") {
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
                    } else if button(ui, idle, ICON_LOG_OUT, "Sign out") {
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
                } else if button(ui, idle, ICON_ARCHIVE_RESTORE, "Restore…") {
                    actions.push(DriveAction::OpenRestore);
                }
                if *busy == Some(Busy::BackingUp) {
                    ui.add(Spin::new(18.0, pal.accent_b));
                } else if button(ui, idle, ICON_CLOUD_UPLOAD, "Back up now") {
                    actions.push(DriveAction::BackupNow);
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
}

/// The modal listing backups newest first; `preparing` = the chosen one is being downloaded.
pub fn restore_dialog(
    ctx: &Context,
    list: &RestoreList<'_>,
    selected: &mut Option<usize>,
    preparing: bool,
) -> Option<RestorePick> {
    let mut retry = false;
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
                            let resp = ui.add_enabled(
                                !preparing,
                                Button::selectable(*selected == Some(i), label)
                                    .min_size(vec2(ui.available_width(), 28.0)),
                            );
                            if resp.clicked() {
                                *selected = Some(i);
                            }
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
    match answer {
        Some(true) => selected.map(RestorePick::Choose),
        Some(false) => Some(RestorePick::Cancel),
        None => None,
    }
}
