//! App-side glue for the Google Drive backup: sign-in, manual backups, restore, deleting backups.
//! Everything runs in the gateway (OAuth flows and prepared restores stay there as handles; the
//! automatic backups follow its schedule). This window only opens the consent page, shows the
//! account and the dialogs, and asks.

use std::collections::VecDeque;
use std::sync::Arc;

use deck_core::config::SyncInterval;
use deck_core::gdrive::{BackupFile, HISTORY_DB, hostname};
use deck_core::sessions::abbreviate_home;
use deck_core::usage::parse_iso_ms;
use deck_service::gateway::protocol::{
    Answer, Done, DriveView, GatewayError, Mutation, Query, RestorePlanView,
};

use super::{App, PendingConfirm, human_size};
use crate::confirm::{self, Confirm};
use crate::drive_view::{
    self, BackupManager, Busy, DriveAction, DriveCard, HostGroup, ManagedBackup, ManagerAction,
    ManagerList, ManagerStep, RestoreList, RestorePick, RestoreRow,
};
use crate::tree_layout::{age, clock};

/// Restore-confirmation lists show at most this many paths per line.
const LIST_MAX: usize = 6;
const JOB_BUSY: &str = "Another Google Drive job is running; try again in a moment.";

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

pub(super) struct DriveUi {
    phase: Phase,
    /// The gateway's last Drive error that was already shown; a new one is shown once.
    seen_error: Option<String>,
    picker: Option<RestorePicker>,
    manager: Option<Manager>,
    deletion: Option<Deletion>,
}

impl DriveUi {
    pub(super) fn new() -> DriveUi {
        DriveUi {
            phase: Phase::Idle,
            seen_error: None,
            picker: None,
            manager: None,
            deletion: None,
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

    /// The restore picker or the backup manager is open.
    pub(super) fn modal_open(&self) -> bool {
        self.picker.is_some() || self.manager.is_some()
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
                    pending_delete: None,
                });
                self.start_listing();
            }
            DriveAction::OpenManager => self.open_drive_manager(),
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

    /// Lists the backups for whichever of the restore picker and the manager is open.
    fn start_listing(&mut self) {
        if let Some(p) = &mut self.drive.picker {
            p.files = None;
            p.selected = None;
        }
        if let Some(m) = &mut self.drive.manager {
            m.dlg.set_list(ManagerList::Loading);
        }
        if !self.drive_begin(Busy::Listing) {
            self.on_drive_listed(Err(JOB_BUSY.to_owned()));
            return;
        }
        self.gw.query(Query::DriveList, |app, r| {
            app.drive.phase = Phase::Idle;
            let result = match r {
                Ok(Answer::DriveList(files)) => Ok(files),
                Ok(_) => Err("unexpected reply".to_owned()),
                Err(e) => Err(e),
            };
            app.on_drive_listed(result);
        });
    }

    fn on_drive_listed(&mut self, result: Result<Vec<BackupFile>, String>) {
        if let Some(m) = &mut self.drive.manager {
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
        if let Some(p) = &mut self.drive.picker {
            p.rows = result.as_ref().map_or_else(|_| Vec::new(), |f| rows(f));
            p.files = Some(result);
        }
    }

    /// "Manage backups…" on the Google Drive card or in the palette.
    fn open_drive_manager(&mut self) {
        if !self.drive_signed_in() {
            self.toasts.info(
                &self.ctx,
                "Sign in to Google Drive in Settings → Backup first.",
            );
            return;
        }
        if !self.drive.idle() {
            self.toasts.info(&self.ctx, JOB_BUSY);
            return;
        }
        self.drive.manager = Some(Manager {
            dlg: BackupManager::new(),
            files: Vec::new(),
        });
        self.start_listing();
    }

    /// The backup manager, while open.
    fn show_drive_manager(&mut self, ctx: &egui::Context) {
        let progress = self.drive.deletion.as_ref().map(|d| (d.done, d.total));
        let busy = !self.drive.idle() || self.drive_view().is_some_and(|v| v.busy.is_some());
        let Some(m) = &mut self.drive.manager else {
            return;
        };
        let Some(action) = drive_view::manager_dialog(ctx, &m.dlg, progress, busy) else {
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
                    if let Some(d) = &mut self.drive.deletion {
                        d.cancelled = true;
                    }
                } else {
                    self.drive.manager = None;
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
                if self.start_drive_delete(files) {
                    if let Some(m) = &mut self.drive.manager {
                        m.dlg.step = ManagerStep::Deleting;
                        m.dlg.cancelling = false;
                        m.dlg.failures.clear();
                    }
                } else {
                    if let Some(m) = &mut self.drive.manager {
                        m.dlg.step = ManagerStep::Browse;
                    }
                    self.toasts.info(&self.ctx, JOB_BUSY);
                }
            }
        }
    }

    /// Deletes `files` permanently, one request after the other; false when another Drive job
    /// runs.
    fn start_drive_delete(&mut self, files: Vec<BackupFile>) -> bool {
        if files.is_empty() || !self.drive_begin(Busy::Deleting) {
            return false;
        }
        self.drive.deletion = Some(Deletion {
            total: files.len(),
            queue: files.into(),
            done: 0,
            deleted: 0,
            failed: Vec::new(),
            cancelled: false,
        });
        self.drive_delete_next();
        true
    }

    /// Sends the next deletion, or reports when the queue is done (or cancelled).
    fn drive_delete_next(&mut self) {
        let Some(d) = &mut self.drive.deletion else {
            return;
        };
        let next = if d.cancelled {
            None
        } else {
            d.queue.pop_front()
        };
        let Some(file) = next else {
            let Some(d) = self.drive.deletion.take() else {
                return;
            };
            self.drive.phase = Phase::Idle;
            self.on_drive_deleted(d);
            return;
        };
        let label = backup_label(&file);
        self.gw
            .mutate(Mutation::DriveDeleteBackup(file), move |app, r| {
                if let Some(d) = &mut app.drive.deletion {
                    d.done += 1;
                    match r {
                        Ok(_) => d.deleted += 1,
                        Err(e) => d.failed.push(format!("{label}: {e}")),
                    }
                }
                app.drive_delete_next();
            });
    }

    fn on_drive_deleted(&mut self, d: Deletion) {
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
        if let Some(m) = &mut self.drive.manager {
            m.dlg.step = ManagerStep::Browse;
            m.dlg.cancelling = false;
            m.dlg.failures = d.failed;
        }
        if self.drive.picker.is_some() || self.drive.manager.is_some() {
            self.start_listing();
        }
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

    /// The Drive dialogs, while open: the backup manager and the restore picker.
    pub(super) fn show_drive_picker(&mut self, ctx: &egui::Context) {
        self.show_drive_manager(ctx);
        let preparing = matches!(self.drive.phase, Phase::Busy(Busy::Preparing));
        let busy = !self.drive.idle() || self.drive_view().is_some_and(|v| v.busy.is_some());
        let Some(p) = &mut self.drive.picker else {
            return;
        };
        let list = match &p.files {
            None => RestoreList::Loading,
            Some(Err(e)) => RestoreList::Failed(e),
            Some(Ok(_)) => RestoreList::Rows(&p.rows),
        };
        let pick = drive_view::restore_dialog(ctx, &list, &mut p.selected, preparing, busy);
        if let Some((i, question)) = &p.pending_delete {
            let i = *i;
            match confirm::show(ctx, "drive-delete-backup", question) {
                None => {}
                Some(false) => p.pending_delete = None,
                Some(true) => {
                    p.pending_delete = None;
                    let file = p
                        .files
                        .as_ref()
                        .and_then(|f| f.as_ref().ok()?.get(i).cloned());
                    if let Some(file) = file {
                        if self.start_drive_delete(vec![file]) {
                            if let Some(p) = &mut self.drive.picker {
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
        let Some(p) = &mut self.drive.picker else {
            return;
        };
        match pick {
            None => {}
            Some(RestorePick::Delete(i)) => {
                let file = p.files.as_ref().and_then(|f| f.as_ref().ok()?.get(i));
                if let Some(file) = file {
                    p.pending_delete = Some((i, delete_question(file)));
                }
            }
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
fn delete_question(f: &BackupFile) -> Confirm {
    let host = match f.host() {
        Some(h) if h == hostname() => "this PC".to_owned(),
        Some(h) => h.to_owned(),
        None => "an unknown host".to_owned(),
    };
    Confirm {
        title: "Delete this backup permanently?".to_owned(),
        body: format!(
            "Settings backup from {host}, {} ({}).\n\nThis can't be undone: the file is removed \
             from Google Drive, not moved to the trash.",
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
