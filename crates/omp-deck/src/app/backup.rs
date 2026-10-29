//! App-side glue for the backups (Google Drive or the user's FTP/FTPS/SFTP server): sign-in,
//! server setup, manual backups, restore, deleting backups. Everything runs in the gateway
//! (OAuth flows and prepared restores stay there as handles; the automatic backups follow its
//! schedule). This window only opens the consent page, shows the destination and the dialogs,
//! and asks.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use deck_core::backup::server::ServerConfig;
use deck_core::backup::{BackupFile, Destination, HISTORY_DB, hostname};
use deck_core::config::SyncInterval;
use deck_core::sessions::abbreviate_home;
use deck_core::usage::parse_iso_ms;
use deck_service::gateway::protocol::{
    Answer, BackupView, Done, GatewayError, Mutation, Query, RestorePlanView, ServerConnectOutcome,
};

use super::{App, PendingConfirm, human_size};
use crate::backup_view::{
    self, BackupAction, BackupCard, BackupManager, Busy, DriveAccount, HostGroup, ManagedBackup,
    ManagerAction, ManagerList, ManagerStep, RestoreList, RestorePick, RestoreRow, ServerForm,
};
use crate::confirm::{self, Confirm};
use crate::transfer;
use crate::tree_layout::{age, clock};

/// Restore-confirmation lists show at most this many paths per line.
const LIST_MAX: usize = 6;
const JOB_BUSY: &str = "Another backup job is running; try again in a moment.";

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
    /// A row's trash button was pressed: (index into `files`, its confirmation).
    pending_delete: Option<(usize, Confirm)>,
}

/// The "Manage settings backups" dialog and the backups behind its checkboxes.
struct Manager {
    dlg: BackupManager,
    files: Vec<BackupFile>,
}

/// Backups being deleted one request after the other.
struct Deletion {
    queue: VecDeque<BackupFile>,
    total: usize,
    /// Requests answered so far.
    done: usize,
    deleted: usize,
    /// `<backup>: <error>`.
    failed: Vec<String>,
    cancelled: bool,
}

pub(super) struct BackupUi {
    phase: Phase,
    /// The gateway's last automatic-backup error that was already shown; a new one is shown once.
    seen_error: Option<String>,
    picker: Option<RestorePicker>,
    manager: Option<Manager>,
    deletion: Option<Deletion>,
}

impl BackupUi {
    pub(super) fn new() -> BackupUi {
        BackupUi {
            phase: Phase::Idle,
            seen_error: None,
            picker: None,
            manager: None,
            deletion: None,
        }
    }

    /// The settings card's state. `view` is the gateway's backup record (`None` before the first
    /// one arrived: Drive then reads as unavailable rather than guessing).
    pub(super) fn card(
        &self,
        view: Option<&BackupView>,
        interval: SyncInterval,
        destination: Destination,
        picker: bool,
    ) -> BackupCard {
        let drive = match view {
            None => DriveAccount::Unavailable,
            Some(v) if !v.drive_available => DriveAccount::Unavailable,
            Some(_) if matches!(self.phase, Phase::Starting { .. } | Phase::Waiting { .. }) => {
                DriveAccount::Waiting
            }
            Some(v) if !v.drive_signed_in => DriveAccount::SignedOut,
            Some(v) => DriveAccount::SignedIn {
                email: v.drive_email.clone(),
            },
        };
        // The setting changes here first; the gateway's record follows a moment later.
        let current = view.filter(|v| v.destination == destination);
        BackupCard {
            destination,
            drive,
            server: view.and_then(|v| v.server.clone()),
            place: place_of(destination, view),
            last: current
                .and_then(|v| v.last_backup.as_ref())
                .map(|l| format!("{} from {}", clock(l.unix), l.host)),
            interval,
            busy: match self.phase {
                Phase::Busy(b) => Some(b),
                // A backup the gateway runs on its own schedule.
                _ => view.and_then(|v| v.busy.as_ref()).map(|_| Busy::BackingUp),
            },
            picker,
        }
    }

    /// The restore picker or the backup manager is open.
    pub(super) fn modal_open(&self) -> bool {
        self.picker.is_some() || self.manager.is_some()
    }

    fn idle(&self) -> bool {
        matches!(self.phase, Phase::Idle)
    }
}

/// The destination's name in titles and toasts: "Google Drive", `sftp://host`, "server".
fn place_of(destination: Destination, view: Option<&BackupView>) -> String {
    match (destination, view.and_then(|v| v.server.as_ref())) {
        (Destination::Drive, _) => "Google Drive".to_owned(),
        (Destination::Server, Some(s)) => s.label.clone(),
        (Destination::Server, None) => "your server".to_owned(),
    }
}

impl App {
    /// The gateway's backup record.
    pub(super) fn backup_view(&self) -> Option<Arc<BackupView>> {
        self.gw.backup()
    }

    /// The Backup card's (and the Sync page's destination strip's) state.
    pub(super) fn backup_card(&self) -> BackupCard {
        self.backup.card(
            self.backup_view().as_deref(),
            self.cfg.drive_backup_interval,
            self.cfg.backup_destination,
            self.picker_available,
        )
    }

    /// The active destination's name ("Google Drive", `sftp://host`).
    pub(super) fn backup_place(&self) -> String {
        place_of(self.cfg.backup_destination, self.backup_view().as_deref())
    }

    /// The active destination is set up: signed in to Drive / a server saved.
    pub(super) fn backup_linked(&self) -> bool {
        let destination = self.cfg.backup_destination;
        self.backup_view().is_some_and(|v| match destination {
            Destination::Drive => v.drive_available && v.drive_signed_in,
            Destination::Server => v.server.is_some(),
        })
    }

    /// Why the active destination takes no backups yet (`None` = it does).
    pub(super) fn backup_unlinked_reason(&self) -> Option<&'static str> {
        let unavailable = !self.backup_view().is_some_and(|v| v.drive_available);
        (!self.backup_linked())
            .then(|| backup_view::unlinked_reason(self.cfg.backup_destination, unavailable))
    }

    pub(super) fn apply_backup_action(&mut self, action: BackupAction) {
        match action {
            BackupAction::SetDestination(d) => {
                self.cfg.backup_destination = d;
                self.dirty = true;
            }
            BackupAction::SignIn => self.start_sign_in(),
            BackupAction::CancelSignIn => match &mut self.backup.phase {
                Phase::Starting { cancel } => *cancel = true,
                Phase::Waiting { handle } => {
                    let handle = *handle;
                    self.gw
                        .mutate(Mutation::DriveLoginCancel(handle), |_, _| {});
                }
                _ => {}
            },
            BackupAction::SetInterval(v) => {
                self.cfg.drive_backup_interval = v;
                self.dirty = true;
            }
            BackupAction::BackupNow => self.start_backup(),
            BackupAction::OpenRestore => {
                self.backup.picker = Some(RestorePicker {
                    files: None,
                    rows: Vec::new(),
                    selected: None,
                    pending_delete: None,
                });
                self.start_listing();
            }
            BackupAction::OpenManager => self.open_backup_manager(),
            BackupAction::SignOut => self.start_sign_out(),
            BackupAction::Connect {
                config,
                keep_secret,
            } => self.start_server_connect(*config, keep_secret),
            BackupAction::BrowseKey => self.browse_key(),
            BackupAction::RemoveServer => {
                let label = self
                    .backup_view()
                    .and_then(|v| v.server.as_ref().map(|s| s.label.clone()))
                    .unwrap_or_else(|| "the server".to_owned());
                self.confirm = Some((
                    Confirm {
                        title: "Remove the backup server?".into(),
                        body: format!(
                            "OmpDeck forgets {label}, its login and its trusted key here. The \
                             backups on the server stay; add the server again to reach them."
                        ),
                        confirm_label: "Remove server".into(),
                        danger: true,
                    },
                    PendingConfirm::RemoveServer,
                ));
            }
            BackupAction::OpenBackupSettings => {
                self.open_settings_page(crate::settings_page::OmpDeckPage::Backup);
            }
        }
    }

    /// Marks a local backup job as running; false when another one is.
    fn backup_begin(&mut self, busy: Busy) -> bool {
        if !self.backup.idle() || !self.gw.is_up() {
            return false;
        }
        self.backup.phase = Phase::Busy(busy);
        true
    }

    fn start_sign_out(&mut self) {
        if !self.backup_begin(Busy::SigningOut) {
            return;
        }
        self.gw.mutate(Mutation::DriveSignOut, |app, r| {
            app.backup.phase = Phase::Idle;
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
        let available = self.backup_view().is_some_and(|v| v.drive_available);
        if !self.backup.idle() || !available || !self.gw.is_up() {
            return;
        }
        self.backup.phase = Phase::Starting { cancel: false };
        self.gw.mutate(Mutation::DriveLoginBegin, |app, r| match r {
            Ok(Done::DriveLogin { handle, url }) => app.on_login_begun(handle, url),
            Ok(_) => {
                app.backup.phase = Phase::Idle;
                app.toasts
                    .error(&app.ctx, "Google sign-in failed: unexpected reply");
            }
            Err(e) => {
                app.backup.phase = Phase::Idle;
                app.toasts.error(&app.ctx, e);
            }
        });
    }

    /// The gateway holds the OAuth flow: open the consent page here, then wait for the browser.
    fn on_login_begun(&mut self, handle: u64, url: String) {
        let cancelled = matches!(self.backup.phase, Phase::Starting { cancel: true });
        if cancelled {
            self.backup.phase = Phase::Idle;
            self.gw
                .mutate(Mutation::DriveLoginCancel(handle), |_, _| {});
            return;
        }
        self.backup.phase = Phase::Waiting { handle };
        self.open_external(url);
        self.gw
            .mutate(Mutation::DriveLoginFinish(handle), |app, r| {
                app.backup.phase = Phase::Idle;
                match r {
                    Ok(Done::DriveAccount { email }) => {
                        app.backup.seen_error = None;
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

    /// Checks and saves the server (the gateway logs in, tests writing, then stores it). An
    /// unknown key or certificate comes back as a fingerprint to trust.
    fn start_server_connect(&mut self, config: ServerConfig, keep_secret: bool) {
        if !self.backup_begin(Busy::Connecting) {
            self.settings.server_form.error = Some(JOB_BUSY.to_owned());
            return;
        }
        self.settings.server_form.error = None;
        self.gw.mutate(
            Mutation::ServerConnect {
                config,
                keep_secret,
            },
            |app, r| {
                app.backup.phase = Phase::Idle;
                let form = &mut app.settings.server_form;
                match r {
                    Ok(Done::ServerConnect(ServerConnectOutcome::Connected(view))) => {
                        *form = ServerForm::default();
                        app.backup.seen_error = None;
                        app.toasts
                            .success(&app.ctx, format!("Backups go to {} now", view.label));
                    }
                    Ok(Done::ServerConnect(ServerConnectOutcome::Untrusted { fingerprint })) => {
                        form.trust = Some(fingerprint);
                    }
                    Ok(Done::ServerConnect(ServerConnectOutcome::Changed {
                        pinned,
                        presented,
                    })) => {
                        form.error = Some(format!(
                            "The server's identity changed: OmpDeck trusts {pinned}, the server \
                             now presents {presented}. If the server's owner changed its key or \
                             certificate, remove the server and add it again."
                        ));
                    }
                    Ok(_) => form.error = Some("Unexpected reply from the gateway".to_owned()),
                    Err(e) => form.error = Some(e),
                }
            },
        );
    }

    /// The user confirmed removing the saved server.
    pub(super) fn remove_server(&mut self) {
        if !self.backup_begin(Busy::RemovingServer) {
            self.toasts.info(&self.ctx, JOB_BUSY);
            return;
        }
        self.gw.mutate(Mutation::ServerForget, |app, r| {
            app.backup.phase = Phase::Idle;
            match r {
                Ok(_) => {
                    app.settings.server_form = ServerForm::default();
                    app.toasts.info(&app.ctx, "Backup server removed");
                }
                Err(e) => app
                    .toasts
                    .error(&app.ctx, format!("Cannot remove the server: {e}")),
            }
        });
    }

    /// The native file dialog for the SFTP key, starting in `~/.ssh` (or the typed file's folder).
    fn browse_key(&mut self) {
        let typed = PathBuf::from(self.settings.server_form.key_path.trim());
        let start = typed
            .parent()
            .filter(|p| p.is_dir())
            .map(PathBuf::from)
            .or_else(|| {
                dirs::home_dir()
                    .map(|h| h.join(".ssh"))
                    .filter(|p| p.is_dir())
            })
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        self.gw.native(
            move || transfer::pick_file(&start),
            |app, path| {
                if let Some(path) = path {
                    app.settings.server_form.key_path = path.to_string_lossy().into_owned();
                }
            },
        );
    }

    fn start_backup(&mut self) {
        if !self.backup_begin(Busy::BackingUp) {
            return;
        }
        let place = self.backup_place();
        self.gw
            .mutate(Mutation::Backup { force: true }, move |app, r| {
                app.backup.phase = Phase::Idle;
                match r {
                    Ok(Done::Backup(outcome)) => {
                        app.backup.seen_error = None;
                        if let Some(o) = outcome {
                            log::info!("uploaded {} ({} pruned)", o.file.name, o.pruned);
                            app.toasts.success(
                                &app.ctx,
                                format!("Backed up to {place} ({})", human_size(o.file.size)),
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(e) => app.toasts.error(&app.ctx, format!("Backup failed: {e}")),
                }
            });
    }

    /// Lists the backups for whichever of the restore picker and the manager is open.
    fn start_listing(&mut self) {
        if let Some(p) = &mut self.backup.picker {
            p.files = None;
            p.selected = None;
        }
        if let Some(m) = &mut self.backup.manager {
            m.dlg.set_list(ManagerList::Loading);
        }
        if !self.backup_begin(Busy::Listing) {
            self.on_backups_listed(Err(JOB_BUSY.to_owned()));
            return;
        }
        self.gw.query(Query::BackupList, |app, r| {
            app.backup.phase = Phase::Idle;
            let result = match r {
                Ok(Answer::BackupList(files)) => Ok(files),
                Ok(_) => Err("unexpected reply".to_owned()),
                Err(e) => Err(e),
            };
            app.on_backups_listed(result);
        });
    }

    fn on_backups_listed(&mut self, result: Result<Vec<BackupFile>, String>) {
        if let Some(m) = &mut self.backup.manager {
            match &result {
                Ok(files) => {
                    m.dlg.set_list(ManagerList::Ready(host_groups(
                        files,
                        &hostname(),
                        unix_now(),
                    )));
                    m.files = files.clone();
                }
                Err(e) => {
                    m.dlg.set_list(ManagerList::Failed(e.clone()));
                    m.files.clear();
                }
            }
        }
        if let Some(p) = &mut self.backup.picker {
            p.rows = result.as_ref().map_or_else(|_| Vec::new(), |f| rows(f));
            p.files = Some(result);
        }
    }

    /// "Manage backups…" on the Backup card or in the palette.
    fn open_backup_manager(&mut self) {
        if let Some(why) = self.backup_unlinked_reason() {
            self.toasts.info(&self.ctx, why);
            return;
        }
        if !self.backup.idle() {
            self.toasts.info(&self.ctx, JOB_BUSY);
            return;
        }
        self.backup.manager = Some(Manager {
            dlg: BackupManager::new(),
            files: Vec::new(),
        });
        self.start_listing();
    }

    /// The backup manager, while open.
    fn show_backup_manager(&mut self, ctx: &egui::Context) {
        let progress = self.backup.deletion.as_ref().map(|d| (d.done, d.total));
        let busy = !self.backup.idle() || self.backup_view().is_some_and(|v| v.busy.is_some());
        let place = self.backup_place();
        let Some(m) = &mut self.backup.manager else {
            return;
        };
        let Some(action) = backup_view::manager_dialog(ctx, &m.dlg, progress, busy, &place) else {
            return;
        };
        match action {
            ManagerAction::Refresh => self.start_listing(),
            ManagerAction::Toggle(i, on) => m.dlg.set_version(i, on),
            ManagerAction::ToggleGroup(g, on) => m.dlg.set_group(g, on),
            ManagerAction::AskDelete => {
                if m.dlg.totals().0 > 0 {
                    m.dlg.step = ManagerStep::Confirm;
                }
            }
            ManagerAction::Back => m.dlg.step = ManagerStep::Browse,
            ManagerAction::Cancel => {
                if m.dlg.step == ManagerStep::Deleting {
                    m.dlg.cancelling = true;
                    // The deletion in flight finishes; none is sent after it.
                    if let Some(d) = &mut self.backup.deletion {
                        d.cancelled = true;
                    }
                } else {
                    self.backup.manager = None;
                    self.focus_pending = true;
                }
            }
            ManagerAction::Delete => {
                let files: Vec<BackupFile> = m
                    .dlg
                    .checked()
                    .into_iter()
                    .filter_map(|i| m.files.get(i).cloned())
                    .collect();
                if files.is_empty() {
                    m.dlg.step = ManagerStep::Browse;
                    return;
                }
                if self.start_backup_delete(files) {
                    if let Some(m) = &mut self.backup.manager {
                        m.dlg.step = ManagerStep::Deleting;
                        m.dlg.cancelling = false;
                        m.dlg.failures.clear();
                    }
                } else {
                    if let Some(m) = &mut self.backup.manager {
                        m.dlg.step = ManagerStep::Browse;
                    }
                    self.toasts.info(&self.ctx, JOB_BUSY);
                }
            }
        }
    }

    /// Deletes `files` permanently, one request after the other; false when another backup job
    /// runs.
    fn start_backup_delete(&mut self, files: Vec<BackupFile>) -> bool {
        if files.is_empty() || !self.backup_begin(Busy::Deleting) {
            return false;
        }
        self.backup.deletion = Some(Deletion {
            total: files.len(),
            queue: files.into(),
            done: 0,
            deleted: 0,
            failed: Vec::new(),
            cancelled: false,
        });
        self.backup_delete_next();
        true
    }

    /// Sends the next deletion, or reports when the queue is done (or cancelled).
    fn backup_delete_next(&mut self) {
        let Some(d) = &mut self.backup.deletion else {
            return;
        };
        let next = if d.cancelled {
            None
        } else {
            d.queue.pop_front()
        };
        let Some(file) = next else {
            let Some(d) = self.backup.deletion.take() else {
                return;
            };
            self.backup.phase = Phase::Idle;
            self.on_backups_deleted(d);
            return;
        };
        let label = backup_label(&file);
        self.gw.mutate(Mutation::DeleteBackup(file), move |app, r| {
            if let Some(d) = &mut app.backup.deletion {
                d.done += 1;
                match r {
                    Ok(_) => d.deleted += 1,
                    Err(e) => d.failed.push(format!("{label}: {e}")),
                }
            }
            app.backup_delete_next();
        });
    }

    fn on_backups_deleted(&mut self, d: Deletion) {
        let s = |n: usize| if n == 1 { "" } else { "s" };
        let stopped = if d.cancelled { " (stopped early)" } else { "" };
        if let Some(first) = d.failed.first() {
            for f in &d.failed {
                log::warn!("deleting a settings backup failed: {f}");
            }
            self.toasts.error(
                &self.ctx,
                format!(
                    "Deleted {} backup{}, {} failed{stopped}: {first}",
                    d.deleted,
                    s(d.deleted),
                    d.failed.len()
                ),
            );
        } else if d.deleted > 0 {
            self.toasts.success(
                &self.ctx,
                format!(
                    "Permanently deleted {} settings backup{}{stopped}",
                    d.deleted,
                    s(d.deleted)
                ),
            );
        } else if d.cancelled {
            self.toasts.info(&self.ctx, "Deletion cancelled");
        }
        if let Some(m) = &mut self.backup.manager {
            m.dlg.step = ManagerStep::Browse;
            m.dlg.cancelling = false;
            m.dlg.failures = d.failed;
        }
        if self.backup.picker.is_some() || self.backup.manager.is_some() {
            self.start_listing();
        }
    }

    /// Per frame: the gateway runs the automatic backups; a failure of one is reported once.
    pub(super) fn backup_tick(&mut self, _ctx: &egui::Context) {
        let Some(view) = self.backup_view() else {
            return;
        };
        match &view.last_error {
            None => self.backup.seen_error = None,
            Some(e) if self.backup.seen_error.as_ref() != Some(e) => {
                self.backup.seen_error = Some(e.clone());
                // A failure of this window's own job was shown by that job.
                if self.backup.idle() {
                    self.toasts.error(
                        &self.ctx,
                        format!("Automatic backup to {} failed: {e}", self.backup_place()),
                    );
                }
            }
            Some(_) => {}
        }
    }

    /// The backup dialogs, while open: the backup manager and the restore picker.
    pub(super) fn show_backup_dialogs(&mut self, ctx: &egui::Context) {
        self.show_backup_manager(ctx);
        let preparing = matches!(self.backup.phase, Phase::Busy(Busy::Preparing));
        let busy = !self.backup.idle() || self.backup_view().is_some_and(|v| v.busy.is_some());
        let place = self.backup_place();
        let Some(p) = &mut self.backup.picker else {
            return;
        };
        let list = match &p.files {
            None => RestoreList::Loading,
            Some(Err(e)) => RestoreList::Failed(e),
            Some(Ok(_)) => RestoreList::Rows(&p.rows),
        };
        let pick =
            backup_view::restore_dialog(ctx, &list, &mut p.selected, preparing, busy, &place);
        if let Some((i, question)) = &p.pending_delete {
            let i = *i;
            match confirm::show(ctx, "backup-delete-backup", question) {
                None => {}
                Some(false) => p.pending_delete = None,
                Some(true) => {
                    p.pending_delete = None;
                    let file = p
                        .files
                        .as_ref()
                        .and_then(|f| f.as_ref().ok()?.get(i).cloned());
                    if let Some(file) = file {
                        if self.start_backup_delete(vec![file]) {
                            if let Some(p) = &mut self.backup.picker {
                                p.files = None;
                                p.selected = None;
                            }
                        } else {
                            self.toasts.info(&self.ctx, JOB_BUSY);
                        }
                    }
                    return;
                }
            }
        }
        let Some(p) = &mut self.backup.picker else {
            return;
        };
        match pick {
            None => {}
            Some(RestorePick::Delete(i)) => {
                let file = p.files.as_ref().and_then(|f| f.as_ref().ok()?.get(i));
                if let Some(file) = file {
                    p.pending_delete = Some((i, delete_question(file, &place)));
                }
            }
            Some(RestorePick::Cancel) => {
                // Closing while a download runs just drops its result.
                if !preparing {
                    self.backup.picker = None;
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
                if !self.backup_begin(Busy::Preparing) {
                    return;
                }
                self.gw.mutate(Mutation::PrepareRestore(file), |app, r| {
                    app.backup.phase = Phase::Idle;
                    match r {
                        Ok(Done::Prepared { handle, plan }) => {
                            app.backup.picker = None;
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
                .mutate(Mutation::DiscardRestore(p.handle), |_, _| {});
            return;
        }
        if !self.backup_begin(Busy::Restoring) {
            self.toasts.info(
                &self.ctx,
                "Another backup job is running; restore again in a moment.",
            );
            self.gw
                .mutate(Mutation::DiscardRestore(p.handle), |_, _| {});
            return;
        }
        let place = self.backup_place();
        self.gw
            .mutate(Mutation::ApplyRestore(p.handle), move |app, r| {
                app.backup.phase = Phase::Idle;
                match r {
                    Ok(Done::Restored(report)) => {
                        app.toasts.success(
                            &app.ctx,
                            format!(
                                "Restored from {place}. Restart running omp sessions to pick up the \
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

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn when(f: &BackupFile) -> String {
    f.created_unix().map_or_else(|| f.created.clone(), clock)
}

/// `2026-10-22 13:52:01 UTC from host` for toasts and errors.
fn backup_label(f: &BackupFile) -> String {
    format!("{} from {}", when(f), f.host().unwrap_or("unknown host"))
}

/// The restore picker's "delete this backup" confirmation.
fn delete_question(f: &BackupFile, place: &str) -> Confirm {
    let host = match f.host() {
        Some(h) if h == hostname() => "this PC".to_owned(),
        Some(h) => h.to_owned(),
        None => "an unknown host".to_owned(),
    };
    Confirm {
        title: "Delete this backup permanently?".to_owned(),
        body: format!(
            "Settings backup from {host}, {} ({}).\n\nThis can't be undone: the file is removed \
             from {place}, not moved to the trash.",
            when(f),
            human_size(f.size)
        ),
        confirm_label: "Delete permanently".to_owned(),
        danger: true,
    }
}

/// `files` (newest first) grouped by the PC that uploaded them: this PC first, then the others by
/// name; versions keep the newest-first order and point back into `files`.
fn host_groups(files: &[BackupFile], me: &str, now: i64) -> Vec<HostGroup> {
    let mut groups: Vec<HostGroup> = Vec::new();
    for (index, f) in files.iter().enumerate() {
        let host = f.host().unwrap_or("unknown host");
        let when = when(f);
        let age = f
            .created_unix()
            .map_or_else(String::new, |t| match age(now, t) {
                a if a == "now" => "just now".to_owned(),
                a => format!("{a} ago"),
            });
        let this_pc = host == me;
        let version = ManagedBackup {
            index,
            confirm_line: if this_pc {
                format!("• {when} · {host} (this PC)")
            } else {
                format!("• {when} · {host}")
            },
            when,
            age,
            bytes: f.size,
            size: if f.size > 0 {
                human_size(f.size)
            } else {
                String::new()
            },
            latest: index == 0,
        };
        match groups.iter_mut().find(|g| g.host == host) {
            Some(g) => g.versions.push(version),
            None => groups.push(HostGroup {
                host: host.to_owned(),
                this_pc,
                versions: vec![version],
            }),
        }
    }
    groups.sort_by(|a, b| b.this_pc.cmp(&a.this_pc).then_with(|| a.host.cmp(&b.host)));
    groups
}

fn rows(files: &[BackupFile]) -> Vec<RestoreRow> {
    files
        .iter()
        .map(|f| RestoreRow {
            when: when(f),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: &str, created: &str, host: &str, size: u64) -> BackupFile {
        BackupFile {
            id: id.into(),
            name: format!("omp-backup-{id}.tar.gz"),
            size,
            created: created.into(),
            props: [("host".to_owned(), host.to_owned())].into(),
        }
    }

    #[test]
    fn backups_group_by_pc_with_this_pc_first_and_keep_newest_first() {
        // Newest first, as the gateway lists them.
        let files = [
            file("c", "2026-10-22T12:00:00.000Z", "zeta", 30),
            file("b", "2026-10-22T11:00:00.000Z", "me", 0),
            file("a", "2026-10-21T12:00:00.000Z", "alpha", 10),
            file("d", "2026-10-20T12:00:00.000Z", "me", 20),
        ];
        let now = files[0].created_unix().unwrap() + 3 * 3600;
        let groups = host_groups(&files, "me", now);
        let hosts: Vec<&str> = groups.iter().map(|g| g.host.as_str()).collect();
        assert_eq!(hosts, ["me", "alpha", "zeta"]);
        assert!(groups[0].this_pc && !groups[1].this_pc);
        let mine: Vec<usize> = groups[0].versions.iter().map(|v| v.index).collect();
        assert_eq!(mine, [1, 3], "newest first, indices into the list");
        assert_eq!(groups[0].versions[0].age, "4h ago");
        assert_eq!(groups[0].versions[0].size, "", "no size reported");
        assert!(
            groups[0].versions[0]
                .confirm_line
                .ends_with("· me (this PC)")
        );
        assert_eq!(groups[2].versions[0].age, "3h ago");
        assert!(groups[2].versions[0].latest);
        assert_eq!(
            groups
                .iter()
                .flat_map(|g| &g.versions)
                .filter(|v| v.latest)
                .count(),
            1
        );
    }
}
