//! App-side glue for the Google Drive backup: sign-in, manual and automatic backups, restore.
//! All network and file work runs on workers (`deck_core::gdrive::Service`); one job at a time.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use deck_core::config::SyncInterval;
use deck_core::gdrive::{
    Account, BackupFile, BackupOutcome, Credentials, Error, HISTORY_DB, LOGIN_TIMEOUT,
    PreparedRestore, RestoreReport, Service,
};
use deck_core::sessions::abbreviate_home;
use deck_core::usage::parse_iso_ms;

use super::{App, AppEvent, PendingConfirm, human_size};
use crate::confirm::Confirm;
use crate::drive_view::{self, Busy, DriveAction, DriveCard, RestoreList, RestorePick, RestoreRow};
use crate::omp_worker;
use crate::tree_layout::clock;

/// First automatic check after startup, and after signing in / changing the interval.
const AUTO_FIRST: Duration = Duration::from_secs(45);
/// Restore-confirmation lists show at most this many paths per line.
const LIST_MAX: usize = 6;

/// A finished Drive job.
pub(super) enum DriveEvent {
    SignedIn(Result<Account, Error>),
    BackedUp {
        manual: bool,
        result: Result<Option<BackupOutcome>, Error>,
        account: Option<Account>,
    },
    Listed(Result<Vec<BackupFile>, Error>),
    Prepared(Result<Box<PreparedRestore>, Error>),
    Restored(Result<RestoreReport, Error>),
    LoggedOut(Result<(), Error>),
}

enum Phase {
    Idle,
    /// The loopback waits for the browser; setting the flag cancels it.
    Waiting(Arc<AtomicBool>),
    Busy(Busy),
}

struct RestorePicker {
    /// `None` = still loading.
    files: Option<Result<Vec<BackupFile>, String>>,
    rows: Vec<RestoreRow>,
    selected: Option<usize>,
}

pub(super) struct DriveUi {
    /// `None` = no OAuth client in this build: the feature is unavailable.
    service: Option<Service>,
    account: Option<Account>,
    phase: Phase,
    next_auto: Instant,
    /// An automatic backup failure was shown; later ones stay quiet until a backup succeeds.
    auto_failure_shown: bool,
    picker: Option<RestorePicker>,
}

impl DriveUi {
    pub(super) fn new(agent_dir: PathBuf) -> DriveUi {
        let service = Credentials::detect().map(|c| Service::new(c, agent_dir));
        // A small local file: read on the UI thread once at startup.
        let account = service.as_ref().and_then(|s| {
            s.store
                .load()
                .inspect_err(|e| log::warn!("google drive account: {e}"))
                .ok()
                .flatten()
        });
        DriveUi {
            service,
            account,
            phase: Phase::Idle,
            next_auto: Instant::now() + AUTO_FIRST,
            auto_failure_shown: false,
            picker: None,
        }
    }

    /// The settings card's state.
    pub(super) fn card(&self, interval: SyncInterval) -> DriveCard {
        if self.service.is_none() {
            return DriveCard::Unavailable;
        }
        if matches!(self.phase, Phase::Waiting(_)) {
            return DriveCard::Waiting;
        }
        let Some(account) = &self.account else {
            return DriveCard::SignedOut;
        };
        DriveCard::SignedIn {
            email: account.email.clone(),
            last: account
                .last_backup
                .as_ref()
                .map(|l| format!("{} from {}", clock(l.unix), l.host)),
            interval,
            busy: match self.phase {
                Phase::Busy(b) => Some(b),
                _ => None,
            },
        }
    }

    /// The restore picker is open.
    pub(super) fn modal_open(&self) -> bool {
        self.picker.is_some()
    }

    /// The Drive service (`None` = no OAuth client in this build); shared with project sync.
    pub(super) fn service(&self) -> Option<&Service> {
        self.service.as_ref()
    }

    /// Signed in with a usable account.
    pub(super) fn signed_in(&self) -> bool {
        self.service.is_some() && self.account.is_some()
    }
}

impl App {
    pub(super) fn apply_drive_action(&mut self, action: DriveAction) {
        match action {
            DriveAction::SignIn => self.start_sign_in(),
            DriveAction::CancelSignIn => {
                if let Phase::Waiting(cancel) = &self.drive.phase {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
            DriveAction::SetInterval(v) => {
                let was_off = self.cfg.drive_backup_interval == SyncInterval::Off;
                self.cfg.drive_backup_interval = v;
                self.dirty = true;
                if let Some(period) = v.period() {
                    self.drive.next_auto =
                        Instant::now() + if was_off { AUTO_FIRST } else { period };
                }
            }
            DriveAction::BackupNow => self.start_backup(true),
            DriveAction::OpenRestore => {
                self.drive.picker = Some(RestorePicker {
                    files: None,
                    rows: Vec::new(),
                    selected: None,
                });
                self.start_listing();
            }
            DriveAction::SignOut => {
                self.drive_job(Busy::SigningOut, |s| DriveEvent::LoggedOut(s.sign_out()));
            }
        }
    }

    /// Runs `job` on a worker unless another Drive job is running.
    fn drive_job(
        &mut self,
        busy: Busy,
        job: impl FnOnce(&Service) -> DriveEvent + Send + 'static,
    ) -> bool {
        let Some(service) = self.drive.service.clone() else {
            return false;
        };
        if !matches!(self.drive.phase, Phase::Idle) {
            return false;
        }
        let started = omp_worker::try_spawn(&self.ctx, &self.events_tx, move || {
            AppEvent::Drive(job(&service))
        });
        match started {
            Ok(()) => {
                self.drive.phase = Phase::Busy(busy);
                true
            }
            Err(e) => {
                self.toasts
                    .error(&self.ctx, format!("Cannot start the Drive job: {e}"));
                false
            }
        }
    }

    fn start_sign_in(&mut self) {
        let Some(service) = self.drive.service.clone() else {
            return;
        };
        if !matches!(self.drive.phase, Phase::Idle) {
            return;
        }
        let flow = match service.begin_login() {
            Ok(f) => f,
            Err(e) => {
                self.toasts.error(&self.ctx, format!("Google {e}"));
                return;
            }
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let url = flow.url().to_owned();
        let flag = cancel.clone();
        let started = omp_worker::try_spawn(&self.ctx, &self.events_tx, move || {
            AppEvent::Drive(DriveEvent::SignedIn(service.finish_login(
                flow,
                &flag,
                LOGIN_TIMEOUT,
            )))
        });
        match started {
            Ok(()) => {
                self.drive.phase = Phase::Waiting(cancel);
                self.open_external(url);
            }
            Err(e) => self
                .toasts
                .error(&self.ctx, format!("Cannot start the sign-in: {e}")),
        }
    }

    fn start_backup(&mut self, manual: bool) {
        self.drive_job(Busy::BackingUp, move |s| {
            let result = s.backup(manual);
            DriveEvent::BackedUp {
                manual,
                result,
                account: s.store.load().ok().flatten(),
            }
        });
    }

    fn start_listing(&mut self) {
        if let Some(p) = &mut self.drive.picker {
            p.files = None;
            p.selected = None;
        }
        if !self.drive_job(Busy::Listing, |s| DriveEvent::Listed(s.list()))
            && let Some(p) = &mut self.drive.picker
        {
            p.files = Some(Err(
                "Another Google Drive job is running; try again.".to_owned()
            ));
        }
    }

    /// Per frame: start the automatic backup when it is due.
    pub(super) fn drive_tick(&mut self, ctx: &egui::Context) {
        let d = &self.drive;
        let Some(period) = self.cfg.drive_backup_interval.period() else {
            return;
        };
        // Not while a restore is being picked or confirmed either.
        if d.account.is_none()
            || d.picker.is_some()
            || matches!(self.confirm, Some((_, PendingConfirm::RestoreBackup(_))))
            || !matches!(d.phase, Phase::Idle)
        {
            return;
        }
        let now = Instant::now();
        if now >= d.next_auto {
            self.drive.next_auto = now + period;
            self.start_backup(false);
        } else {
            ctx.request_repaint_after(d.next_auto - now);
        }
    }

    pub(super) fn on_drive_event(&mut self, ev: DriveEvent) {
        self.drive.phase = Phase::Idle;
        match ev {
            DriveEvent::SignedIn(Ok(account)) => {
                let who = account
                    .email
                    .clone()
                    .unwrap_or_else(|| "your account".into());
                self.drive.account = Some(account);
                self.drive.next_auto = Instant::now() + AUTO_FIRST;
                self.drive.auto_failure_shown = false;
                self.toasts.success(
                    &self.ctx,
                    format!("Google Drive backup: signed in as {who}"),
                );
            }
            DriveEvent::SignedIn(Err(Error::Cancelled)) => {}
            DriveEvent::SignedIn(Err(e @ (Error::Login(_) | Error::TimedOut))) => {
                self.toasts.error(&self.ctx, format!("Google {e}"));
            }
            DriveEvent::SignedIn(Err(e)) => {
                self.toasts
                    .error(&self.ctx, format!("Google sign-in failed: {e}"));
            }
            DriveEvent::BackedUp {
                manual,
                result,
                account,
            } => {
                if self.drive.account.is_some() {
                    self.drive.account = account;
                }
                self.on_backed_up(manual, result);
            }
            DriveEvent::Listed(result) => {
                let result = result.map_err(|e| self.drive_error(e));
                if let Some(p) = &mut self.drive.picker {
                    p.rows = result.as_ref().map_or_else(|_| Vec::new(), |f| rows(f));
                    p.files = Some(result);
                }
            }
            DriveEvent::Prepared(Ok(prepared)) => {
                self.drive.picker = None;
                self.confirm = Some((
                    restore_question(&prepared),
                    PendingConfirm::RestoreBackup(prepared),
                ));
            }
            DriveEvent::Prepared(Err(e)) => {
                let msg = self.drive_error(e);
                self.toasts
                    .error(&self.ctx, format!("Cannot restore that backup: {msg}"));
            }
            DriveEvent::Restored(Ok(report)) => {
                self.toasts.success(
                    &self.ctx,
                    format!(
                        "Restored from Google Drive. Restart running omp sessions to pick up the \
                         restored config. The previous files are in {}",
                        abbreviate_home(&report.safety_dir)
                    ),
                );
            }
            DriveEvent::Restored(Err(e)) => {
                let msg = self.drive_error(e);
                self.toasts
                    .error(&self.ctx, format!("Restore failed: {msg}"));
            }
            DriveEvent::LoggedOut(result) => {
                self.drive.account = None;
                match result {
                    Ok(()) => self.toasts.info(&self.ctx, "Signed out of Google Drive"),
                    Err(e) => self.toasts.error(
                        &self.ctx,
                        format!("Signed out locally, but revoking access at Google failed: {e}"),
                    ),
                }
            }
        }
    }

    fn on_backed_up(&mut self, manual: bool, result: Result<Option<BackupOutcome>, Error>) {
        match result {
            Ok(outcome) => {
                self.drive.auto_failure_shown = false;
                if let Some(o) = outcome {
                    log::info!("uploaded {} ({} pruned)", o.file.name, o.pruned);
                    if manual {
                        self.toasts.success(
                            &self.ctx,
                            format!("Backed up to Google Drive ({})", human_size(o.file.size)),
                        );
                    }
                }
            }
            Err(e) => {
                let signed_out = matches!(e, Error::SignedOut);
                let msg = self.drive_error(e);
                if manual || signed_out {
                    self.toasts
                        .error(&self.ctx, format!("Backup failed: {msg}"));
                } else if !self.drive.auto_failure_shown {
                    self.drive.auto_failure_shown = true;
                    self.toasts.error(
                        &self.ctx,
                        format!("Automatic Google Drive backup failed: {msg}"),
                    );
                } else {
                    log::warn!("automatic backup failed: {msg}");
                }
            }
        }
    }

    /// The message for `e`; a revoked sign-in also switches the card to signed out.
    pub(super) fn drive_error(&mut self, e: Error) -> String {
        if matches!(e, Error::SignedOut | Error::NotSignedIn) {
            self.drive.account = None;
        }
        e.to_string()
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
                self.drive_job(Busy::Preparing, move |s| {
                    DriveEvent::Prepared(s.prepare_restore(&file).map(Box::new))
                });
            }
        }
    }

    /// The user answered the restore confirmation.
    pub(super) fn answer_restore(&mut self, prepared: Box<PreparedRestore>, yes: bool) {
        if !yes {
            return;
        }
        if !self.drive_job(Busy::Restoring, move |s| {
            DriveEvent::Restored(s.apply_restore(&prepared))
        }) {
            self.toasts.info(
                &self.ctx,
                "Another Google Drive job is running; restore again in a moment.",
            );
        }
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
fn restore_question(p: &PreparedRestore) -> Confirm {
    let plan = &p.plan;
    let when = parse_iso_ms(&p.manifest.created).map_or_else(
        || p.manifest.created.clone(),
        |ms| clock(ms.div_euclid(1000)),
    );
    let mut body = format!("Backup from {when} on {}.\n", p.manifest.host);
    let label = |rel: &String| {
        if rel == HISTORY_DB {
            "history.db (prompt history)".to_owned()
        } else {
            rel.clone()
        }
    };
    if !plan.replaced.is_empty() {
        let names: Vec<String> = plan.replaced.iter().map(label).collect();
        body += &format!("\nReplaced: {}", list(&names));
    }
    if !plan.added.is_empty() {
        let names: Vec<String> = plan.added.iter().map(label).collect();
        body += &format!("\nAdded: {}", list(&names));
    }
    if !plan.dirs.is_empty() {
        let dirs: Vec<String> = plan.dirs.iter().map(|d| format!("{d}/")).collect();
        body += &format!("\nMade identical to the backup: {}", list(&dirs));
        if !plan.removed.is_empty() {
            body += &format!(
                " (removes {} local file{} not in the backup: {})",
                plan.removed.len(),
                if plan.removed.len() == 1 { "" } else { "s" },
                list(&plan.removed)
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
