//! Work that keeps running without a GUI: usage fetches, app and omp update checks, the backup
//! accounts and the automatic config and project backups to the active destination (on the
//! one-at-a-time backup lane).

use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use deck_core::backup::{self, Destination, LastBackup, MemoryBank, OmpDir, Parts, ProjectSizes};
use deck_core::omp::UpdateStatus;
use deck_core::selfupdate::{self, InstallKind, Release};
use deck_core::usage::UsageSnapshot;

use super::{Engine, Sched};
use crate::gateway::GATEWAY_VERSION;
use crate::gateway::protocol::{
    AppUpdateView, BackupView, GatewayError, OmpUpdateView, Progress, ServerView, UpdatePhase,
    UsageView,
};

/// First usage fetch after start (or after omp turned up).
const USAGE_FIRST: Duration = Duration::from_secs(3);
const USAGE_EVERY: Duration = Duration::from_secs(300);
/// First app/omp update check after start.
const CHECK_FIRST: Duration = Duration::from_secs(10);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
/// First automatic config backup after the destination was set up (or turning it on).
const BACKUP_FIRST: Duration = Duration::from_secs(45);
/// First automatic project pass after the destination was set up (or turning it on).
const PROJECT_FIRST: Duration = Duration::from_secs(2 * 60);
/// The backup lane label of automatic work.
const AUTO_LANE: &str = "automatic backup";

/// What the destinations' local records say (read on a worker).
pub(crate) struct Accounts {
    /// Signed in to Google: the email and the last backup from this machine.
    drive: Option<(Option<String>, Option<LastBackup>)>,
    /// The saved server and the last backup from this machine.
    server: Option<(ServerView, Option<LastBackup>)>,
}

pub(super) struct Schedules {
    // usage
    usage: Option<UsageSnapshot>,
    usage_at: Option<u64>,
    usage_error: Option<String>,
    usage_fetching: bool,
    next_usage: Option<Instant>,
    // app update
    install: Option<InstallKind>,
    latest: Option<Release>,
    asset: Option<String>,
    app_checked: Option<u64>,
    next_app: Instant,
    phase: UpdatePhase,
    /// The verified download: its private directory and the file.
    downloaded: Option<(PathBuf, PathBuf)>,
    downloading: bool,
    applying: bool,
    // omp
    omp_version: Option<String>,
    omp_status: Option<Result<UpdateStatus, String>>,
    omp_checked: Option<u64>,
    omp_checking: bool,
    next_omp: Instant,
    // backup destinations
    accounts: Accounts,
    backup_error: Option<String>,
    last_auto: Option<u64>,
    account_busy: bool,
    account_again: bool,
    /// The destination the timers are armed for, and whether it was set up then.
    armed: (Destination, bool),
    next_backup: Option<Instant>,
    backup_period: Option<Duration>,
    // project sync
    next_project: Option<Instant>,
    project_period: Option<Duration>,
    pass: VecDeque<PathBuf>,
}

impl Schedules {
    pub(super) fn new(now: Instant, omp_found: bool) -> Schedules {
        Schedules {
            usage: None,
            usage_at: None,
            usage_error: None,
            usage_fetching: false,
            next_usage: omp_found.then_some(now + USAGE_FIRST),
            install: None,
            latest: None,
            asset: None,
            app_checked: None,
            next_app: now + CHECK_FIRST,
            phase: UpdatePhase::Idle,
            downloaded: None,
            downloading: false,
            applying: false,
            omp_version: None,
            omp_status: None,
            omp_checked: None,
            omp_checking: false,
            next_omp: now + CHECK_FIRST,
            accounts: Accounts {
                drive: None,
                server: None,
            },
            backup_error: None,
            last_auto: None,
            account_busy: false,
            account_again: false,
            armed: (Destination::Drive, false),
            next_backup: None,
            backup_period: None,
            next_project: None,
            project_period: None,
            pass: VecDeque::new(),
        }
    }

    pub(super) fn usage_view(&self) -> UsageView {
        UsageView {
            snapshot: self.usage.clone(),
            fetched_unix_ms: self.usage_at,
            error: self.usage_error.clone(),
            fetching: self.usage_fetching,
        }
    }

    pub(super) fn app_view(&self) -> AppUpdateView {
        AppUpdateView {
            install: self
                .install
                .clone()
                .unwrap_or_else(|| InstallKind::Unsupported {
                    reason: "still detecting how OmpDeck was installed".to_owned(),
                }),
            current: GATEWAY_VERSION.to_owned(),
            latest: self.latest.clone(),
            asset: self.asset.clone(),
            checked_unix_ms: self.app_checked,
            phase: self.phase.clone(),
        }
    }

    pub(super) fn omp_view(&self) -> OmpUpdateView {
        OmpUpdateView {
            version: self.omp_version.clone(),
            status: self.omp_status.clone(),
            checked_unix_ms: self.omp_checked,
            checking: self.omp_checking,
        }
    }

    pub(super) fn usage_fetching(&self) -> bool {
        self.usage_fetching
    }

    pub(super) fn begin_usage_fetch(&mut self, now: Instant) {
        self.usage_fetching = true;
        self.next_usage = Some(now + USAGE_EVERY);
    }

    pub(super) fn begin_app_check(&mut self) {
        if !self.downloading && !self.applying {
            self.phase = UpdatePhase::Checking;
        }
    }

    pub(super) fn begin_omp_check(&mut self) {
        self.omp_checking = true;
    }

    fn updatable(&self) -> Result<&InstallKind, String> {
        match &self.install {
            None => Err("still detecting how OmpDeck was installed".to_owned()),
            Some(InstallKind::Dev) => Err("a development build is never updated".to_owned()),
            Some(InstallKind::Unsupported { reason }) => Err(reason.clone()),
            Some(k) => Ok(k),
        }
    }

    pub(super) fn download_refusal(&self) -> Option<GatewayError> {
        let refused = |s: String| Some(GatewayError::Refused(s));
        if let Err(e) = self.updatable() {
            return refused(e);
        }
        if self.downloading || self.applying {
            return refused("an update is already being installed".to_owned());
        }
        if self.latest.is_none() {
            return refused("no newer release was found".to_owned());
        }
        if self.asset.is_none() {
            return refused("the release has nothing for this installation".to_owned());
        }
        None
    }

    pub(super) fn apply_refusal(&self) -> Option<GatewayError> {
        let refused = |s: &str| Some(GatewayError::Refused(s.to_owned()));
        if let Err(e) = self.updatable() {
            return Some(GatewayError::Refused(e));
        }
        if self.downloading || self.applying {
            return refused("an update is already being installed");
        }
        if self.downloaded.is_none() {
            return refused("download the update first");
        }
        None
    }

    pub(super) fn download_plan(&self) -> Option<(Release, String)> {
        self.download_refusal()
            .is_none()
            .then(|| self.latest.clone().zip(self.asset.clone()))
            .flatten()
    }

    pub(super) fn begin_download(&mut self) {
        self.downloading = true;
        self.phase = UpdatePhase::Downloading {
            done: 0,
            total: None,
        };
    }

    pub(super) fn download_progress(&mut self, p: &Progress) {
        if self.downloading {
            self.phase = UpdatePhase::Downloading {
                done: p.done,
                total: p.total,
            };
        }
    }

    /// The install kind, the downloaded file and its directory; marks the update applying.
    pub(super) fn apply_plan(&mut self) -> Option<(InstallKind, PathBuf, PathBuf)> {
        if self.apply_refusal().is_some() {
            return None;
        }
        let kind = self.install.clone()?;
        let (dir, file) = self.downloaded.clone()?;
        self.applying = true;
        Some((kind, file, dir))
    }
}

/// Sessions of `sizes` that are not opted out.
fn included_sessions(sizes: &ProjectSizes, excluded: &BTreeSet<String>) -> u64 {
    let gone = sizes
        .session_list
        .iter()
        .filter(|s| excluded.contains(&s.id))
        .count();
    sizes
        .sessions
        .saturating_sub(u64::try_from(gone).unwrap_or(u64::MAX))
}

/// What a backup can include right now, given the row's switches and exclusions.
fn usable_parts(want: Parts, sizes: &ProjectSizes, excluded: &BTreeSet<String>) -> Parts {
    Parts {
        omp: want.omp && matches!(sizes.omp, OmpDir::Present { .. }),
        sessions: want.sessions && included_sessions(sizes, excluded) > 0,
        memory: want.memory && matches!(sizes.memory, MemoryBank::Present { .. }),
    }
}

fn parts_of(row: &deck_core::config::SyncRow) -> Parts {
    Parts {
        omp: row.omp,
        sessions: row.sessions,
        memory: row.memory,
    }
}

impl Engine {
    pub(super) fn backup_view(&self) -> BackupView {
        let s = &self.sched;
        let destination = self.cfg.backup_destination;
        let drive = s.accounts.drive.as_ref();
        let server = s.accounts.server.as_ref();
        BackupView {
            destination,
            drive_available: self.service.creds.is_some(),
            drive_signed_in: drive.is_some(),
            drive_email: drive.and_then(|(e, _)| e.clone()),
            server: server.map(|(v, _)| v.clone()),
            last_backup: match destination {
                Destination::Drive => drive.and_then(|(_, l)| l.clone()),
                Destination::Server => server.and_then(|(_, l)| l.clone()),
            },
            busy: self.backup_lane.clone(),
            last_error: s.backup_error.clone(),
            last_auto_unix_ms: s.last_auto,
        }
    }

    /// The active destination is set up (signed in to Google / a server saved).
    fn backup_linked(&self) -> bool {
        match self.cfg.backup_destination {
            Destination::Drive => {
                self.service.creds.is_some() && self.sched.accounts.drive.is_some()
            }
            Destination::Server => self.sched.accounts.server.is_some(),
        }
    }

    pub(super) fn start_schedules(&mut self) {
        self.spawn_worker("app-detect", || {
            selfupdate::sweep_leftovers();
            Sched::AppDetected(selfupdate::detect()).into()
        });
        self.refresh_omp_version();
        self.load_accounts();
    }

    fn refresh_omp_version(&mut self) {
        let Some(omp) = self.omp.clone() else {
            self.sched.omp_version = None;
            self.dirty.omp = true;
            return;
        };
        self.spawn_worker("omp-version", move || {
            Sched::OmpVersion(omp.version().map_err(|e| e.to_string())).into()
        });
    }

    /// Re-reads both destinations' local records (sign-in, saved server, last backups). A request
    /// while a read runs is served by one more read after it.
    pub(super) fn load_accounts(&mut self) {
        if self.sched.account_busy {
            self.sched.account_again = true;
            return;
        }
        self.sched.account_busy = true;
        let service = self.service.clone();
        self.spawn_worker("backup-accounts", move || {
            let read = || -> backup::Result<Accounts> {
                let drive = match service.creds {
                    Some(_) => service.store.load()?.map(|a| (a.email, a.last_backup)),
                    None => None,
                };
                let server = service
                    .server_account()?
                    .map(|a| (ServerView::of(&a), a.last_backup));
                Ok(Accounts { drive, server })
            };
            Sched::Accounts(read().map_err(|e| e.to_string())).into()
        });
    }

    /// omp may have been installed, updated or moved (a tool run ended).
    pub(super) fn redetect_omp(&mut self) {
        let recorded = self.context.omp.clone();
        self.spawn_worker("omp-detect", move || {
            Sched::OmpFound(crate::gateway::resolve_omp(recorded.as_deref())).into()
        });
    }

    pub(super) fn step_schedules(&mut self, now: Instant) {
        self.track_intervals(now);
        // An on-demand owner about to exit and a draining one start nothing new.
        if !self.background_wanted() || self.barrier.is_some() || self.stopping {
            return;
        }
        let mut next: Vec<Instant> = Vec::new();
        // Usage.
        if self.omp.is_some()
            && !self.sched.usage_fetching
            && let Some(at) = self.sched.next_usage
        {
            if now >= at {
                self.sched.begin_usage_fetch(now);
                self.dirty.usage = true;
                let ctx = self.job_ctx();
                self.spawn_worker("usage", move || {
                    Sched::Usage(super::mutate::fetch_usage(&ctx)).into()
                });
            } else {
                next.push(at);
            }
        }
        // App update check.
        if self.cfg.check_app_updates
            && self.sched.updatable().is_ok()
            && !matches!(self.sched.phase, UpdatePhase::Checking)
            && !self.sched.downloading
            && !self.sched.applying
        {
            if now >= self.sched.next_app {
                self.sched.next_app = now + CHECK_EVERY;
                self.sched.begin_app_check();
                self.dirty.app = true;
                self.spawn_worker("app-check", || {
                    Sched::AppChecked(selfupdate::latest().map_err(|e| e.to_string())).into()
                });
            } else {
                next.push(self.sched.next_app);
            }
        }
        // omp update check.
        if self.cfg.check_updates
            && !self.sched.omp_checking
            && let Some(omp) = self.omp.clone()
        {
            if now >= self.sched.next_omp {
                self.sched.next_omp = now + CHECK_EVERY;
                self.sched.begin_omp_check();
                self.dirty.omp = true;
                self.spawn_worker("omp-check", move || {
                    Sched::OmpChecked(omp.check_update().map_err(|e| e.to_string())).into()
                });
            } else {
                next.push(self.sched.next_omp);
            }
        }
        // Automatic config backup, then project passes, to the active destination (one lane
        // job at a time).
        let linked = self.sched.armed.1;
        if let Some(period) = self.sched.backup_period
            && linked
            && let Some(at) = self.sched.next_backup
        {
            if now < at {
                next.push(at);
            } else if self.backup_lane.is_none() {
                self.sched.next_backup = Some(now + period);
                self.backup_lane = Some(AUTO_LANE.to_owned());
                self.dirty.backup = true;
                let destination = self.cfg.backup_destination;
                let service = self.service.with_destination(destination);
                self.spawn_worker("backup-auto", move || {
                    let result = service.backup(false).map(drop).map_err(|e| e.to_string());
                    Sched::BackupAuto {
                        destination,
                        result,
                    }
                    .into()
                });
            }
        }
        if let Some(period) = self.sched.project_period
            && linked
            && let Some(at) = self.sched.next_project
        {
            if now >= at {
                self.sched.next_project = Some(now + period);
                if self.sched.pass.is_empty() {
                    self.sched.pass = self
                        .cfg
                        .project_sync
                        .iter()
                        .filter(|r| parts_of(r).any())
                        .map(|r| r.path.clone())
                        .collect();
                }
            } else {
                next.push(at);
            }
        }
        if self.backup_lane.is_none() {
            self.next_project_backup();
        }
        if let Some(at) = next.into_iter().min() {
            self.wake_within(at.saturating_duration_since(now));
        }
    }

    /// Applies interval, destination and sign-in changes to the backup and project timers: a
    /// destination that just became active and set up gets its first passes soon (its own sync
    /// state decides what is uploaded).
    fn track_intervals(&mut self, now: Instant) {
        let armed = (self.cfg.backup_destination, self.backup_linked());
        if armed != self.sched.armed {
            let s = &mut self.sched;
            if armed.0 != s.armed.0 {
                s.backup_error = None;
            }
            s.armed = armed;
            let (backup, project) = if armed.1 {
                (
                    s.backup_period.map(|_| now + BACKUP_FIRST),
                    s.project_period.map(|_| now + PROJECT_FIRST),
                )
            } else {
                (None, None)
            };
            s.next_backup = backup;
            s.next_project = project;
            s.pass.clear();
            self.dirty.backup = true;
        }
        let backup = self.cfg.drive_backup_interval.period();
        if backup != self.sched.backup_period {
            let was = std::mem::replace(&mut self.sched.backup_period, backup);
            self.sched.next_backup = match (was, backup) {
                (_, None) => None,
                (None, Some(_)) => Some(now + BACKUP_FIRST),
                (Some(_), Some(p)) => Some(now + p),
            };
        }
        let project = self.cfg.project_sync_interval.period();
        if project != self.sched.project_period {
            let was = std::mem::replace(&mut self.sched.project_period, project);
            self.sched.next_project = match (was, project) {
                (_, None) => None,
                (None, Some(_)) => Some(now + PROJECT_FIRST),
                (Some(_), Some(p)) => Some(now + p),
            };
            if project.is_none() {
                self.sched.pass.clear();
            }
        }
    }

    /// Starts the next folder of the running automatic pass.
    fn next_project_backup(&mut self) {
        if !self.sched.armed.1 {
            self.sched.pass.clear();
            return;
        }
        if self.sched.pass.is_empty() {
            return;
        }
        let service = self.service.with_destination(self.cfg.backup_destination);
        while let Some(path) = self.sched.pass.pop_front() {
            // The row may have been removed or turned off since the pass started.
            let Some(row) = self
                .cfg
                .project_sync
                .iter()
                .find(|r| r.path == path && parts_of(r).any())
            else {
                continue;
            };
            let want = parts_of(row);
            let excluded = row.excluded_sessions.clone();
            let root = self.root.clone();
            self.backup_lane = Some(AUTO_LANE.to_owned());
            self.dirty.backup = true;
            self.spawn_worker("project-auto", move || {
                let result = backup::measure(&root, &path)
                    .map_err(|e| e.to_string())
                    .and_then(|sizes| {
                        let parts = usable_parts(want, &sizes, &excluded);
                        if !parts.any() {
                            // Nothing selected is usable (every session opted out): not a
                            // failure.
                            log::info!(
                                "automatic project backup of {}: nothing to back up",
                                path.display()
                            );
                            return Ok(());
                        }
                        let cancel = AtomicBool::new(false);
                        service
                            .backup_project(&root, &path, parts, &excluded, false, &cancel, &|_| {})
                            .map(drop)
                            .map_err(|e| e.to_string())
                    });
                Sched::ProjectAuto { path, result }.into()
            });
            return;
        }
    }

    /// The schedules' own worker results.
    pub(super) fn on_schedule_fin(&mut self, fin: Sched) {
        let now = Instant::now();
        match fin {
            Sched::Usage(r) => {
                self.sched.usage_fetching = false;
                self.sched.next_usage = Some(now + USAGE_EVERY);
                match r {
                    Ok(s) => {
                        self.sched.usage = Some(s);
                        self.sched.usage_at = Some(crate::gateway::unix_ms());
                        self.sched.usage_error = None;
                    }
                    Err(e) => self.sched.usage_error = Some(e),
                }
                self.dirty.usage = true;
            }
            Sched::AppDetected(kind) => {
                self.sched.install = Some(kind);
                self.dirty.app = true;
            }
            Sched::AppChecked(r) => {
                self.sched.app_checked = Some(crate::gateway::unix_ms());
                let s = &mut self.sched;
                match r {
                    Ok(release) => {
                        if selfupdate::is_newer(&release.version, GATEWAY_VERSION) {
                            s.asset = s
                                .install
                                .as_ref()
                                .and_then(|k| selfupdate::asset_for(k, &release.version));
                            s.latest = Some(release);
                        } else {
                            s.latest = None;
                            s.asset = None;
                        }
                        if matches!(s.phase, UpdatePhase::Checking | UpdatePhase::Failed(_)) {
                            s.phase = UpdatePhase::Idle;
                        }
                    }
                    Err(e) => {
                        if matches!(s.phase, UpdatePhase::Checking) {
                            s.phase = UpdatePhase::Failed(e);
                        }
                    }
                }
                self.dirty.app = true;
            }
            Sched::AppDownloaded(r) => {
                let s = &mut self.sched;
                s.downloading = false;
                match r {
                    Ok(d) => {
                        if let Some((old, _)) = s.downloaded.replace(d) {
                            let _ = std::fs::remove_dir_all(old);
                        }
                        s.phase = UpdatePhase::Idle;
                    }
                    Err(e) if e == selfupdate::UpdateError::Cancelled.to_string() => {
                        s.phase = UpdatePhase::Idle;
                    }
                    Err(e) => s.phase = UpdatePhase::Failed(e),
                }
                self.dirty.app = true;
            }
            Sched::AppApplied(r) => {
                let s = &mut self.sched;
                s.applying = false;
                match r {
                    Ok(applied) => {
                        // The worker removed the installed download.
                        s.downloaded = None;
                        s.latest = None;
                        s.asset = None;
                        s.phase = UpdatePhase::Applied(applied);
                    }
                    Err(phase) => s.phase = phase,
                }
                self.dirty.app = true;
            }
            Sched::OmpFound(omp) => {
                let found = omp.is_some();
                let changed = self.omp.as_ref().map(|o| o.program().to_path_buf())
                    != omp.as_ref().map(|o| o.program().to_path_buf());
                self.omp = omp;
                if found && (changed || self.sched.next_usage.is_none()) {
                    self.sched.next_usage = Some(now + USAGE_FIRST);
                }
                if !found {
                    self.sched.next_usage = None;
                    self.sched.omp_status = None;
                }
                // A tool run may have updated it: re-read the version and the update state.
                self.sched.next_omp = now;
                self.refresh_omp_version();
                self.dirty.omp = true;
                self.dirty.gateway = true;
            }
            Sched::OmpVersion(r) => {
                match r {
                    Ok(v) => self.sched.omp_version = Some(v),
                    Err(e) => {
                        log::warn!("reading the omp version failed: {e}");
                        self.sched.omp_version = None;
                    }
                }
                self.dirty.omp = true;
            }
            Sched::OmpChecked(r) => {
                self.sched.omp_checking = false;
                self.sched.omp_checked = Some(crate::gateway::unix_ms());
                self.sched.omp_status = Some(r);
                self.dirty.omp = true;
            }
            Sched::Accounts(r) => {
                let s = &mut self.sched;
                s.account_busy = false;
                match r {
                    Ok(accounts) => s.accounts = accounts,
                    Err(e) => s.backup_error = Some(e),
                }
                self.dirty.backup = true;
                if std::mem::take(&mut self.sched.account_again) {
                    self.load_accounts();
                }
                // Re-arm the timers for what is set up now.
                self.waker.now();
            }
            Sched::BackupAuto {
                destination,
                result,
            } => {
                self.release_auto_lane();
                self.sched.last_auto = Some(crate::gateway::unix_ms());
                match result {
                    Err(e) => {
                        log::warn!("automatic backup to {destination:?} failed: {e}");
                        // A destination switched away from meanwhile is not the one shown.
                        if destination == self.cfg.backup_destination {
                            self.sched.backup_error = Some(e);
                        }
                    }
                    Ok(()) => {
                        if destination == self.cfg.backup_destination {
                            self.sched.backup_error = None;
                        }
                    }
                }
                self.load_accounts();
            }
            Sched::ProjectAuto { path, result } => {
                self.release_auto_lane();
                if let Err(e) = result {
                    log::warn!("automatic project backup of {} failed: {e}", path.display());
                }
                self.waker.now();
            }
        }
    }

    fn release_auto_lane(&mut self) {
        if self.backup_lane.as_deref() == Some(AUTO_LANE) {
            self.backup_lane = None;
        }
        self.dirty.backup = true;
    }
}
