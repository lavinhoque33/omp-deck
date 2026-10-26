//! App-side glue for the Google Drive backup: sign-in, manual backups, restore. Everything runs in
//! the gateway (OAuth flows and prepared restores stay there as handles; the automatic backups
//! follow its schedule). This window only opens the consent page, shows the account and the
//! dialogs, and asks.

use std::sync::Arc;

use deck_core::config::SyncInterval;
use deck_core::gdrive::{BackupFile, HISTORY_DB};
use deck_core::sessions::abbreviate_home;
use deck_core::usage::parse_iso_ms;
use deck_service::gateway::protocol::{
    Answer, Done, DriveView, GatewayError, Mutation, Query, RestorePlanView,
};

use super::{App, PendingConfirm, human_size};
use crate::confirm::Confirm;
use crate::drive_view::{self, Busy, DriveAction, DriveCard, RestoreList, RestorePick, RestoreRow};
use crate::tree_layout::clock;

/// Restore-confirmation lists show at most this many paths per line.
const LIST_MAX: usize = 6;

/// A restore the gateway prepared and holds under `handle`, waiting for the user's answer.
pub(super) struct PendingRestore {
    handle: u64,
}

enum Phase {
    Idle,
    /// The sign-in was requested; the gateway has not returned its consent page yet.
    Starting {
        cancel: bool,
    },
    /// The consent page is open; the gateway's loopback waits for the browser.
    Waiting {
        handle: u64,
    },
    Busy(Busy),
}

struct RestorePicker {
    /// `None` = still loading.
    files: Option<Result<Vec<BackupFile>, String>>,
    rows: Vec<RestoreRow>,
    selected: Option<usize>,
}

pub(super) struct DriveUi {
    phase: Phase,
    /// The gateway's last Drive error that was already shown; a new one is shown once.
    seen_error: Option<String>,
    picker: Option<RestorePicker>,
}

impl DriveUi {
    pub(super) fn new() -> DriveUi {
        DriveUi {
            phase: Phase::Idle,
            seen_error: None,
            picker: None,
        }
    }

    /// The settings card's state. `view` is the gateway's Drive record (`None` before the first
    /// one arrived: the card then reads as unavailable rather than guessing).
    pub(super) fn card(&self, view: Option<&DriveView>, interval: SyncInterval) -> DriveCard {
        let Some(view) = view else {
            return DriveCard::Unavailable;
        };
        if !view.available {
            return DriveCard::Unavailable;
        }
        if matches!(self.phase, Phase::Starting { .. } | Phase::Waiting { .. }) {
            return DriveCard::Waiting;
        }
        if !view.signed_in {
            return DriveCard::SignedOut;
        }
        DriveCard::SignedIn {
            email: view.email.clone(),
            last: view
                .last_backup
                .as_ref()
                .map(|l| format!("{} from {}", clock(l.unix), l.host)),
            interval,
            busy: match self.phase {
                Phase::Busy(b) => Some(b),
                // A backup the gateway runs on its own schedule.
                _ => view.busy.as_ref().map(|_| Busy::BackingUp),
            },
        }
    }

    /// The restore picker is open.
    pub(super) fn modal_open(&self) -> bool {
        self.picker.is_some()
    }

    fn idle(&self) -> bool {
        matches!(self.phase, Phase::Idle)
    }
}

impl App {
    /// The gateway's Drive record.
    pub(super) fn drive_view(&self) -> Option<Arc<DriveView>> {
        self.gw.drive()
    }

    /// Google Drive is configured in this build.
    pub(super) fn drive_available(&self) -> bool {
        self.drive_view().is_some_and(|v| v.available)
    }

    /// Signed in with a usable account.
    pub(super) fn drive_signed_in(&self) -> bool {
        self.drive_view()
            .is_some_and(|v| v.available && v.signed_in)
    }

    pub(super) fn apply_drive_action(&mut self, action: DriveAction) {
        match action {
            DriveAction::SignIn => self.start_sign_in(),
            DriveAction::CancelSignIn => match &mut self.drive.phase {
                Phase::Starting { cancel } => *cancel = true,
                Phase::Waiting { handle } => {
                    let handle = *handle;
                    self.gw
                        .mutate(Mutation::DriveLoginCancel(handle), |_, _| {});
                }
                _ => {}
            },
            DriveAction::SetInterval(v) => {
                self.cfg.drive_backup_interval = v;
                self.dirty = true;
            }
            DriveAction::BackupNow => self.start_backup(),
            DriveAction::OpenRestore => {
                self.drive.picker = Some(RestorePicker {
                    files: None,
                    rows: Vec::new(),
                    selected: None,
                });
                self.start_listing();
            }
            DriveAction::SignOut => self.start_sign_out(),
        }
    }

    /// Marks a local Drive job as running; false when another one is.
    fn drive_begin(&mut self, busy: Busy) -> bool {
        if !self.drive.idle() || !self.gw.is_up() {
            return false;
        }
        self.drive.phase = Phase::Busy(busy);
        true
    }

    fn start_sign_out(&mut self) {
        if !self.drive_begin(Busy::SigningOut) {
            return;
        }
        self.gw.mutate(Mutation::DriveSignOut, |app, r| {
            app.drive.phase = Phase::Idle;
            match r {
                Ok(_) => app.toasts.info(&app.ctx, "Signed out of Google Drive"),
                Err(e) => app.toasts.error(
                    &app.ctx,
                    format!("Signed out locally, but revoking access at Google failed: {e}"),
                ),
            }
        });
    }

    fn start_sign_in(&mut self) {
        if !self.drive.idle() || !self.drive_available() || !self.gw.is_up() {
            return;
        }
        self.drive.phase = Phase::Starting { cancel: false };
        self.gw.mutate(Mutation::DriveLoginBegin, |app, r| match r {
            Ok(Done::DriveLogin { handle, url }) => app.on_login_begun(handle, url),
            Ok(_) => {
                app.drive.phase = Phase::Idle;
                app.toasts
                    .error(&app.ctx, "Google sign-in failed: unexpected reply");
            }
            Err(e) => {
                app.drive.phase = Phase::Idle;
                app.toasts.error(&app.ctx, e);
            }
        });
    }

    /// The gateway holds the OAuth flow: open the consent page here, then wait for the browser.
    fn on_login_begun(&mut self, handle: u64, url: String) {
        let cancelled = matches!(self.drive.phase, Phase::Starting { cancel: true });
        if cancelled {
            self.drive.phase = Phase::Idle;
            self.gw
                .mutate(Mutation::DriveLoginCancel(handle), |_, _| {});
            return;
        }
        self.drive.phase = Phase::Waiting { handle };
        self.open_external(url);
        self.gw
            .mutate(Mutation::DriveLoginFinish(handle), |app, r| {
                app.drive.phase = Phase::Idle;
                match r {
                    Ok(Done::DriveAccount { email }) => {
                        app.drive.seen_error = None;
                        let who = email.unwrap_or_else(|| "your account".into());
                        app.toasts
                            .success(&app.ctx, format!("Google Drive backup: signed in as {who}"));
                    }
                    Ok(_) => {}
                    Err(e) if e == GatewayError::Cancelled.to_string() => {}
                    Err(e) => app.toasts.error(&app.ctx, e),
                }
            });
    }

    fn start_backup(&mut self) {
        if !self.drive_begin(Busy::BackingUp) {
            return;
        }
        self.gw
            .mutate(Mutation::DriveBackup { force: true }, |app, r| {
                app.drive.phase = Phase::Idle;
                match r {
                    Ok(Done::DriveBackup(outcome)) => {
                        app.drive.seen_error = None;
                        if let Some(o) = outcome {
                            log::info!("uploaded {} ({} pruned)", o.file.name, o.pruned);
                            app.toasts.success(
                                &app.ctx,
                                format!("Backed up to Google Drive ({})", human_size(o.file.size)),
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(e) => app.toasts.error(&app.ctx, format!("Backup failed: {e}")),
                }
            });
    }

    fn start_listing(&mut self) {
        if let Some(p) = &mut self.drive.picker {
            p.files = None;
            p.selected = None;
        }
        if !self.drive_begin(Busy::Listing) {
            if let Some(p) = &mut self.drive.picker {
                p.files = Some(Err(
                    "Another Google Drive job is running; try again.".to_owned()
                ));
            }
            return;
        }
        self.gw.query(Query::DriveList, |app, r| {
            app.drive.phase = Phase::Idle;
            let result = match r {
                Ok(Answer::DriveList(files)) => Ok(files),
                Ok(_) => Err("unexpected reply".to_owned()),
                Err(e) => Err(e),
            };
            if let Some(p) = &mut app.drive.picker {
                p.rows = result.as_ref().map_or_else(|_| Vec::new(), |f| rows(f));
                p.files = Some(result);
            }
        });
    }

    /// Per frame: the gateway runs the automatic backups; a failure of one is reported once.
    pub(super) fn drive_tick(&mut self, _ctx: &egui::Context) {
        let Some(view) = self.drive_view() else {
            return;
        };
        match &view.last_error {
            None => self.drive.seen_error = None,
            Some(e) if self.drive.seen_error.as_ref() != Some(e) => {
                self.drive.seen_error = Some(e.clone());
                // A failure of this window's own job was shown by that job.
                if self.drive.idle() {
                    self.toasts.error(
                        &self.ctx,
                        format!("Automatic Google Drive backup failed: {e}"),
                    );
                }
            }
            Some(_) => {}
        }
    }

    /// The restore picker, while open.
    pub(super) fn show_drive_picker(&mut self, ctx: &egui::Context) {
        let preparing = matches!(self.drive.phase, Phase::Busy(Busy::Preparing));
        let Some(p) = &mut self.drive.picker else {
            return;
        };
        let list = match &p.files {
            None => RestoreList::Loading,
            Some(Err(e)) => RestoreList::Failed(e),
            Some(Ok(_)) => RestoreList::Rows(&p.rows),
        };
        let pick = drive_view::restore_dialog(ctx, &list, &mut p.selected, preparing);
        match pick {
            None => {}
            Some(RestorePick::Cancel) => {
                // Closing while a download runs just drops its result.
                if !preparing {
                    self.drive.picker = None;
                }
            }
            Some(RestorePick::Retry) => self.start_listing(),
            Some(RestorePick::Choose(i)) => {
                let Some(file) = p
                    .files
                    .as_ref()
                    .and_then(|f| f.as_ref().ok()?.get(i).cloned())
                else {
                    return;
                };
                if !self.drive_begin(Busy::Preparing) {
                    return;
                }
                self.gw
                    .mutate(Mutation::DrivePrepareRestore(file), |app, r| {
                        app.drive.phase = Phase::Idle;
                        match r {
                            Ok(Done::DrivePrepared { handle, plan }) => {
                                app.drive.picker = None;
                                app.confirm = Some((
                                    restore_question(&plan),
                                    PendingConfirm::RestoreBackup(PendingRestore { handle }),
                                ));
                            }
                            Ok(_) => {}
                            Err(e) => app
                                .toasts
                                .error(&app.ctx, format!("Cannot restore that backup: {e}")),
                        }
                    });
            }
        }
    }

    /// The user answered the restore confirmation.
    pub(super) fn answer_restore(&mut self, p: PendingRestore, yes: bool) {
        if !yes {
            self.gw
                .mutate(Mutation::DriveDiscardRestore(p.handle), |_, _| {});
            return;
        }
        if !self.drive_begin(Busy::Restoring) {
            self.toasts.info(
                &self.ctx,
                "Another Google Drive job is running; restore again in a moment.",
            );
            self.gw
                .mutate(Mutation::DriveDiscardRestore(p.handle), |_, _| {});
            return;
        }
        self.gw
            .mutate(Mutation::DriveApplyRestore(p.handle), |app, r| {
                app.drive.phase = Phase::Idle;
                match r {
                    Ok(Done::DriveRestored(report)) => {
                        app.toasts.success(
                            &app.ctx,
                            format!(
                                "Restored from Google Drive. Restart running omp sessions to pick up the \
                                 restored config. The previous files are in {}",
                                abbreviate_home(&report.safety_dir)
                            ),
                        );
                    }
                    Ok(_) => {}
                    Err(e) => app.toasts.error(&app.ctx, format!("Restore failed: {e}")),
                }
            });
    }
}

fn rows(files: &[BackupFile]) -> Vec<RestoreRow> {
    files
        .iter()
        .map(|f| RestoreRow {
            when: f.created_unix().map_or_else(|| f.created.clone(), clock),
            host: f.host().unwrap_or("unknown host").to_owned(),
            size: human_size(f.size),
        })
        .collect()
}

/// `a, b, c and 3 more`.
fn list(items: &[String]) -> String {
    let shown = items[..items.len().min(LIST_MAX)].join(", ");
    match items.len().saturating_sub(LIST_MAX) {
        0 => shown,
        more => format!("{shown} and {more} more"),
    }
}

/// The confirmation spelling out what the restore replaces.
fn restore_question(p: &RestorePlanView) -> Confirm {
    let when =
        parse_iso_ms(&p.created).map_or_else(|| p.created.clone(), |ms| clock(ms.div_euclid(1000)));
    let mut body = format!("Backup from {when} on {}.\n", p.host);
    let label = |rel: &String| {
        if rel == HISTORY_DB {
            "history.db (prompt history)".to_owned()
        } else {
            rel.clone()
        }
    };
    if !p.replaced.is_empty() {
        let names: Vec<String> = p.replaced.iter().map(label).collect();
        body += &format!("\nReplaced: {}", list(&names));
    }
    if !p.added.is_empty() {
        let names: Vec<String> = p.added.iter().map(label).collect();
        body += &format!("\nAdded: {}", list(&names));
    }
    if !p.dirs.is_empty() {
        let dirs: Vec<String> = p.dirs.iter().map(|d| format!("{d}/")).collect();
        body += &format!("\nMade identical to the backup: {}", list(&dirs));
        if !p.removed.is_empty() {
            body += &format!(
                " (removes {} local file{} not in the backup: {})",
                p.removed.len(),
                if p.removed.len() == 1 { "" } else { "s" },
                list(&p.removed)
            );
        }
    }
    body += &format!(
        "\n\nEverything replaced or removed is first copied to {}.\nRunning omp sessions keep \
         their old config: restart them to pick up the restored files.",
        abbreviate_home(&p.safety_dir)
    );
    Confirm {
        title: "Restore this backup?".to_owned(),
        body,
        confirm_label: "Restore".to_owned(),
        danger: true,
    }
}
