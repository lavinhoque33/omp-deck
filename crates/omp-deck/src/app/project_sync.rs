//! App-side glue for project sync (wave 10): per-folder `.omp` + sessions backups to Google Drive,
//! the Settings "Sync" page, the restore flow and the sidebar entries.
//!
//! It runs on its own lane (one project job at a time, independent of the config backup lane in
//! `drive.rs`). Network, packing and measuring all run on workers; the UI only reads shared
//! progress and channel results.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use deck_core::config::{SyncInterval, SyncRow};
use deck_core::gdrive::{
    Error, OmpDir, Parts, PreparedProjectRestore, Progress, ProjectBackup, ProjectBackupOutcome,
    ProjectRestoreReport, ProjectSizes, ProjectSyncState, Service, Stage, hostname, measure,
    path_id,
};
use deck_core::sessions::abbreviate_home;

use super::{App, AppEvent, human_size};
use crate::drive_view::DriveCard;
use crate::omp_worker;
use crate::sidebar::{DriveMenu, format_age};
use crate::sync_view::{
    self, BackupAnswer, BackupDialog, Cells, CloudProject, LastView, ListState, OmpCell,
    PathAnswer, PathPrompt, PathPurpose, PlanView, ProgView, ReportView, RestoreAction,
    RestoreDialog, RowView, SessionsCell, Step, SyncAction, SyncPage, TargetStatus, VersionRow,
};
use crate::transfer;
use crate::tree_layout::clock;

/// First automatic pass after startup and after signing in.
const AUTO_FIRST: Duration = Duration::from_secs(2 * 60);
/// Progress callbacks wake the UI at most this often.
const REPAINT_EVERY: Duration = Duration::from_millis(100);
/// The confirmation lists at most this many overwritten paths.
const PLAN_LIST: usize = 8;

/// What the lane is doing.
enum Kind {
    Backup { path: PathBuf },
    Listing,
    Preparing,
    Restoring,
}

/// Latest progress of the running job, written by the worker, read each frame.
struct Prog {
    label: &'static str,
    done: u64,
    total: u64,
}

struct Shared {
    prog: Mutex<Option<Prog>>,
    last_repaint: Mutex<Instant>,
}

impl Shared {
    fn report(&self, ctx: &egui::Context, p: Progress) {
        let label = match p.stage {
            Stage::Scanning => "Scanning",
            Stage::Packing => "Packing",
            Stage::Uploading => "Uploading",
            Stage::Downloading => "Downloading",
            Stage::Verifying => "Verifying",
            Stage::RestoringOmp => "Restoring .omp",
            Stage::RestoringSessions => "Restoring sessions",
        };
        *self.prog.lock().unwrap_or_else(PoisonError::into_inner) = Some(Prog {
            label,
            done: p.done,
            total: p.total,
        });
        let mut last = self
            .last_repaint
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if last.elapsed() >= REPAINT_EVERY {
            *last = Instant::now();
            ctx.request_repaint();
        }
    }

    fn view(&self) -> Option<ProgView> {
        let g = self.prog.lock().unwrap_or_else(PoisonError::into_inner);
        let p = g.as_ref()?;
        let fraction = (p.total > 0).then(|| (p.done as f64 / p.total as f64) as f32);
        Some(ProgView {
            label: match fraction {
                Some(f) => format!("{} {}%", p.label, (f.clamp(0.0, 1.0) * 100.0) as u32),
                None => format!("{}…", p.label),
            },
            fraction,
        })
    }
}

struct Job {
    kind: Kind,
    cancel: Arc<AtomicBool>,
    shared: Arc<Shared>,
}

/// What a folder pick is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pick {
    Add,
    Change(usize),
    RestoreTarget,
}

/// A finished project-sync worker.
pub(super) enum SyncEvent {
    State(ProjectSyncState),
    Sizes {
        path: PathBuf,
        result: Result<ProjectSizes, String>,
    },
    BackedUp {
        path: PathBuf,
        auto: bool,
        result: Result<Option<Box<ProjectBackupOutcome>>, Error>,
        state: ProjectSyncState,
    },
    Listed(Result<Vec<ProjectBackup>, Error>),
    Prepared(Result<Box<PreparedProjectRestore>, Error>),
    Restored(Result<ProjectRestoreReport, Error>),
    Picked {
        pick: Pick,
        path: Option<PathBuf>,
    },
    /// A typed folder was checked on disk.
    Checked {
        purpose: PathPurpose,
        path: PathBuf,
        result: Result<(), String>,
    },
    /// Does the restore target exist? `preset` = the original path offered as the default.
    Probed {
        generation: u64,
        path: PathBuf,
        exists: bool,
        preset: bool,
    },
}

struct Restore {
    dlg: RestoreDialog,
    backups: Vec<ProjectBackup>,
    prepared: Option<Box<PreparedProjectRestore>>,
    /// Sessions in the prepared plan (for the "3 of 11" wording of a cancelled restore).
    session_total: usize,
    /// Sidebar restore: only this folder's backups are listed.
    only: Option<PathBuf>,
    probe_generation: u64,
    /// The user typed or browsed a target: the preset must not overwrite it.
    target_touched: bool,
}

pub(super) struct SyncUi {
    job: Option<Job>,
    state: ProjectSyncState,
    state_requested: bool,
    /// Measured sizes by folder; absent = being measured for the first time.
    sizes: HashMap<PathBuf, Result<ProjectSizes, String>>,
    measuring: HashSet<PathBuf>,
    /// The last failed backup of each folder.
    errors: HashMap<PathBuf, String>,
    /// `.omp` entries the last backup of each folder left out.
    skipped: HashMap<PathBuf, Vec<String>>,
    next_auto: Instant,
    /// Folders left in the running automatic pass.
    pass: VecDeque<PathBuf>,
    pass_failure_shown: bool,
    was_signed_in: bool,
    /// The Sync page was drawn (this frame / last frame): sizes are measured when it appears.
    seen: bool,
    was_visible: bool,
    prompt: Option<PathPrompt>,
    picking: bool,
    backup: Option<BackupDialog>,
    restore: Option<Restore>,
}

impl SyncUi {
    pub(super) fn new() -> SyncUi {
        SyncUi {
            job: None,
            state: ProjectSyncState::default(),
            state_requested: false,
            sizes: HashMap::new(),
            measuring: HashSet::new(),
            errors: HashMap::new(),
            skipped: HashMap::new(),
            next_auto: Instant::now() + AUTO_FIRST,
            pass: VecDeque::new(),
            pass_failure_shown: false,
            was_signed_in: false,
            seen: false,
            was_visible: false,
            prompt: None,
            picking: false,
            backup: None,
            restore: None,
        }
    }

    /// A dialog of this feature is open.
    pub(super) fn modal_open(&self) -> bool {
        self.prompt.is_some() || self.backup.is_some() || self.restore.is_some()
    }
}

/// Sizes for the UI: `Loading` until the first measurement lands.
fn cells_of(sizes: Option<&Result<ProjectSizes, String>>) -> Cells {
    match sizes {
        None => Cells {
            omp: OmpCell::Loading,
            sessions: SessionsCell::Loading,
        },
        Some(Err(e)) => Cells {
            omp: OmpCell::Failed(e.clone()),
            sessions: SessionsCell::Failed(e.clone()),
        },
        Some(Ok(s)) => Cells {
            omp: match &s.omp {
                OmpDir::Missing => OmpCell::Missing,
                OmpDir::Global => OmpCell::Global,
                OmpDir::Symlink => OmpCell::Symlink,
                OmpDir::Present { files, bytes } => OmpCell::Present(format!(
                    "{} · {files} file{}",
                    human_size(*bytes),
                    if *files == 1 { "" } else { "s" }
                )),
            },
            sessions: SessionsCell::Ready {
                count: s.sessions,
                text: format!(
                    "{} session{} · {}",
                    s.sessions,
                    if s.sessions == 1 { "" } else { "s" },
                    human_size(s.session_bytes)
                ),
            },
        },
    }
}

/// What a backup of `path` can include right now, given the user's switches.
fn usable_parts(want: Parts, sizes: &ProjectSizes) -> Parts {
    Parts {
        omp: want.omp && matches!(sizes.omp, OmpDir::Present { .. }),
        sessions: want.sessions && sizes.sessions > 0,
    }
}

impl App {
    // ---------------------------------------------------------------- gating

    fn sync_signed_in(&self) -> bool {
        self.drive.signed_in()
    }

    /// The sidebar's "Google Drive" entries.
    pub(super) fn drive_menu(&self) -> DriveMenu {
        if self.drive.service().is_none() {
            DriveMenu::Hidden
        } else if !self.sync_signed_in() {
            DriveMenu::Off("Sign in to Google Drive in Settings → Sync first")
        } else if self.sync.job.is_some() {
            DriveMenu::Off("Another project backup or restore is running")
        } else {
            DriveMenu::On
        }
    }

    // ---------------------------------------------------------------- jobs

    /// Runs `job` on a worker unless the lane is busy or nobody is signed in.
    fn lane_job<F>(&mut self, kind: Kind, job: F) -> bool
    where
        F: FnOnce(&Service, &AtomicBool, &(dyn Fn(Progress) + Sync)) -> SyncEvent + Send + 'static,
    {
        if self.sync.job.is_some() || !self.sync_signed_in() {
            return false;
        }
        let Some(service) = self.drive.service().cloned() else {
            return false;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared {
            prog: Mutex::new(None),
            last_repaint: Mutex::new(Instant::now()),
        });
        let (c, s, ctx) = (cancel.clone(), shared.clone(), self.ctx.clone());
        let started = omp_worker::try_spawn(&self.ctx, &self.events_tx, move || {
            let report = |p: Progress| s.report(&ctx, p);
            AppEvent::Sync(job(&service, &c, &report))
        });
        match started {
            Ok(()) => {
                self.sync.job = Some(Job {
                    kind,
                    cancel,
                    shared,
                });
                true
            }
            Err(e) => {
                self.toasts
                    .error(&self.ctx, format!("Cannot start the Drive job: {e}"));
                false
            }
        }
    }

    fn row_parts(&self, path: &Path) -> Parts {
        self.cfg
            .project_sync
            .iter()
            .find(|r| r.path == path)
            .map_or(Parts::default(), |r| Parts {
                omp: r.omp,
                sessions: r.sessions,
            })
    }

    /// Measures, then backs up `path` with whichever of `want` exists.
    fn start_project_backup(&mut self, path: PathBuf, want: Parts, auto: bool) -> bool {
        let root = self.root.clone();
        let kind = Kind::Backup { path: path.clone() };
        self.lane_job(kind, move |service, cancel, report| {
            let result = measure(&root, &path)
                .map_err(|e| Error::Io(format!("reading {}: {e}", path.display())))
                .and_then(|sizes| {
                    service
                        .backup_project(
                            &root,
                            &path,
                            usable_parts(want, &sizes),
                            !auto,
                            cancel,
                            report,
                        )
                        .map(|o| o.map(Box::new))
                });
            SyncEvent::BackedUp {
                path,
                auto,
                result,
                state: service.project_state(),
            }
        })
    }

    /// Measure `path` on a worker (never per frame).
    fn measure_path(&mut self, path: &Path) {
        if !self.sync.measuring.insert(path.to_path_buf()) {
            return;
        }
        let (root, path) = (self.root.clone(), path.to_path_buf());
        omp_worker::spawn(&self.ctx, &self.events_tx, move || {
            let result = measure(&root, &path).map_err(|e| e.to_string());
            AppEvent::Sync(SyncEvent::Sizes { path, result })
        });
    }

    fn measure_rows(&mut self) {
        let paths: Vec<PathBuf> = self
            .cfg
            .project_sync
            .iter()
            .map(|r| r.path.clone())
            .collect();
        for p in paths {
            self.measure_path(&p);
        }
    }

    // ---------------------------------------------------------------- per frame

    /// Per frame: load the local state once, start the automatic pass when it is due.
    pub(super) fn sync_tick(&mut self, ctx: &egui::Context) {
        self.sync.was_visible = std::mem::take(&mut self.sync.seen);
        let signed_in = self.sync_signed_in();
        if signed_in && !self.sync.was_signed_in {
            self.sync.next_auto = Instant::now() + AUTO_FIRST;
            self.sync.pass.clear();
            self.sync.pass_failure_shown = false;
        }
        self.sync.was_signed_in = signed_in;
        if let Some(service) = self.drive.service().cloned()
            && !self.sync.state_requested
        {
            self.sync.state_requested = true;
            omp_worker::spawn(&self.ctx, &self.events_tx, move || {
                AppEvent::Sync(SyncEvent::State(service.project_state()))
            });
        }
        if !signed_in {
            self.sync.pass.clear();
            return;
        }
        if self.sync.job.is_some() || self.sync.restore.is_some() || self.sync.backup.is_some() {
            return;
        }
        if let Some(path) = self.sync.pass.pop_front() {
            if self.cfg.project_sync.iter().any(|r| r.path == path) {
                let want = self.row_parts(&path);
                self.start_project_backup(path, want, true);
            }
            ctx.request_repaint();
            return;
        }
        let Some(period) = self.cfg.project_sync_interval.period() else {
            return;
        };
        if self.cfg.project_sync.is_empty() {
            return;
        }
        let now = Instant::now();
        if now >= self.sync.next_auto {
            self.sync.next_auto = now + period;
            self.sync.pass = self
                .cfg
                .project_sync
                .iter()
                .map(|r| r.path.clone())
                .collect();
            self.sync.pass_failure_shown = false;
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(self.sync.next_auto - now);
        }
    }

    // ---------------------------------------------------------------- the Sync page

    /// The page's view data.
    pub(super) fn sync_rows(&self) -> Vec<RowView> {
        let now = SystemTime::now();
        self.cfg
            .project_sync
            .iter()
            .map(|r| {
                let busy = self.sync.job.as_ref().and_then(|j| match &j.kind {
                    Kind::Backup { path, .. } if *path == r.path => {
                        Some(j.shared.view().unwrap_or(ProgView {
                            label: "Starting…".into(),
                            fraction: None,
                        }))
                    }
                    _ => None,
                });
                let last = match (
                    self.sync.errors.get(&r.path),
                    self.sync.state.projects.get(&r.path),
                ) {
                    (Some(e), _) => LastView::Failed(e.clone()),
                    (None, Some(s)) => {
                        let at = SystemTime::UNIX_EPOCH
                            + Duration::from_secs(u64::try_from(s.last.unix).unwrap_or(0));
                        let age = format_age(now, at);
                        LastView::At {
                            text: if age == "now" {
                                "just now".to_owned()
                            } else {
                                format!("{age} ago")
                            },
                            tip: format!("{} from {}", clock(s.last.unix), s.last.host),
                            skipped: self.sync.skipped.get(&r.path).cloned().unwrap_or_default(),
                        }
                    }
                    (None, None) => LastView::Never,
                };
                RowView {
                    display: abbreviate_home(&r.path),
                    full: r.path.display().to_string(),
                    omp_on: r.omp,
                    sessions_on: r.sessions,
                    cells: cells_of(self.sync.sizes.get(&r.path)),
                    last,
                    busy,
                }
            })
            .collect()
    }

    /// The page, as the settings view needs it.
    pub(super) fn sync_page<'a>(
        &self,
        account: &'a DriveCard,
        rows: &'a [RowView],
    ) -> SyncPage<'a> {
        SyncPage {
            account,
            rows,
            interval: self.cfg.project_sync_interval,
            lane_busy: self.sync.job.is_some(),
            picking: self.sync.picking,
        }
    }

    pub(super) fn apply_sync_action(&mut self, action: SyncAction) {
        match action {
            SyncAction::Visible => {
                if !self.sync.was_visible {
                    self.measure_rows();
                }
                self.sync.seen = true;
            }
            SyncAction::Drive(a) => self.apply_drive_action(a),
            SyncAction::Add => self.pick_folder(Pick::Add),
            SyncAction::Change(i) => self.pick_folder(Pick::Change(i)),
            SyncAction::Remove(i) => {
                if i < self.cfg.project_sync.len() {
                    let row = self.cfg.project_sync.remove(i);
                    self.sync.sizes.remove(&row.path);
                    self.sync.errors.remove(&row.path);
                    self.dirty = true;
                }
            }
            SyncAction::SetOmp(i, on) => {
                if let Some(r) = self.cfg.project_sync.get_mut(i) {
                    r.omp = on;
                    self.dirty = true;
                }
            }
            SyncAction::SetSessions(i, on) => {
                if let Some(r) = self.cfg.project_sync.get_mut(i) {
                    r.sessions = on;
                    self.dirty = true;
                }
            }
            SyncAction::BackUp(i) => {
                if let Some(r) = self.cfg.project_sync.get(i) {
                    let (path, want) = (
                        r.path.clone(),
                        Parts {
                            omp: r.omp,
                            sessions: r.sessions,
                        },
                    );
                    if !self.start_project_backup(path, want, false) {
                        self.toasts.info(
                            &self.ctx,
                            "Another project job is running; try again in a moment.",
                        );
                    }
                }
            }
            SyncAction::SetInterval(v) => {
                self.cfg.project_sync_interval = v;
                self.dirty = true;
                if v != SyncInterval::Off {
                    self.sync.next_auto = Instant::now() + AUTO_FIRST;
                }
            }
            SyncAction::OpenRestore => self.open_restore(None),
        }
    }

    // ---------------------------------------------------------------- folder picking

    fn pick_folder(&mut self, pick: Pick) {
        if self.picker_available {
            if self.sync.picking {
                return;
            }
            self.sync.picking = true;
            if let Some(r) = &mut self.sync.restore {
                r.dlg.picking = pick == Pick::RestoreTarget;
            }
            let start = self.home.clone().unwrap_or_else(|| PathBuf::from("/"));
            omp_worker::spawn(&self.ctx, &self.events_tx, move || {
                let path = transfer::picker_command(&start, transfer::on_path)
                    .and_then(|p| transfer::run_picker(&p));
                AppEvent::Sync(SyncEvent::Picked { pick, path })
            });
        } else {
            let purpose = match pick {
                Pick::Add => PathPurpose::Add,
                Pick::Change(i) => PathPurpose::Change(i),
                Pick::RestoreTarget => return,
            };
            self.sync.prompt = Some(PathPrompt::new(purpose));
        }
    }

    /// Add or change a row to `path` (already known to be a folder).
    fn apply_row_path(&mut self, purpose: PathPurpose, path: PathBuf) -> Result<(), String> {
        if self
            .cfg
            .project_sync
            .iter()
            .enumerate()
            .any(|(i, r)| r.path == path && purpose != PathPurpose::Change(i))
        {
            return Err("That folder is already in the list".to_owned());
        }
        match purpose {
            PathPurpose::Add => self.cfg.project_sync.push(SyncRow {
                path: path.clone(),
                omp: true,
                sessions: true,
            }),
            PathPurpose::Change(i) => {
                let row = self
                    .cfg
                    .project_sync
                    .get_mut(i)
                    .ok_or_else(|| "That row is gone".to_owned())?;
                let old = std::mem::replace(&mut row.path, path.clone());
                self.sync.sizes.remove(&old);
                self.sync.errors.remove(&old);
            }
        }
        self.dirty = true;
        self.measure_path(&path);
        Ok(())
    }

    // ---------------------------------------------------------------- events

    pub(super) fn on_sync_event(&mut self, ev: SyncEvent) {
        match ev {
            SyncEvent::State(s) => self.sync.state = s,
            SyncEvent::Sizes { path, result } => {
                self.sync.measuring.remove(&path);
                self.sync.sizes.insert(path, result);
            }
            SyncEvent::BackedUp {
                path,
                auto,
                result,
                state,
            } => {
                self.sync.job = None;
                self.sync.state = state;
                self.on_project_backed_up(path, auto, result);
            }
            SyncEvent::Listed(result) => {
                self.sync.job = None;
                self.on_listed(result);
            }
            SyncEvent::Prepared(result) => {
                self.sync.job = None;
                self.on_prepared(result);
            }
            SyncEvent::Restored(result) => {
                self.sync.job = None;
                self.on_restored(result);
            }
            SyncEvent::Picked { pick, path } => {
                self.sync.picking = false;
                if let Some(r) = &mut self.sync.restore {
                    r.dlg.picking = false;
                }
                let Some(path) = path else {
                    return;
                };
                match pick {
                    Pick::Add => self.picked_row(PathPurpose::Add, path),
                    Pick::Change(i) => self.picked_row(PathPurpose::Change(i), path),
                    Pick::RestoreTarget => {
                        if let Some(r) = &mut self.sync.restore {
                            r.dlg.target = path.to_string_lossy().into_owned();
                            r.target_touched = true;
                        }
                        self.probe_target();
                    }
                }
            }
            SyncEvent::Checked {
                purpose,
                path,
                result,
            } => self.on_checked(purpose, path, result),
            SyncEvent::Probed {
                generation,
                path,
                exists,
                preset,
            } => {
                let Some(r) = &mut self.sync.restore else {
                    return;
                };
                if generation != r.probe_generation {
                    return;
                }
                if preset {
                    if exists && !r.target_touched {
                        r.dlg.target = path.to_string_lossy().into_owned();
                        r.dlg.target_status = TargetStatus::Exists;
                    }
                } else {
                    r.dlg.target_status = if exists {
                        TargetStatus::Exists
                    } else {
                        TargetStatus::Missing
                    };
                }
            }
        }
    }

    fn picked_row(&mut self, purpose: PathPurpose, path: PathBuf) {
        if let Err(e) = self.apply_row_path(purpose, path) {
            self.toasts.error(&self.ctx, e);
        }
    }

    fn on_checked(&mut self, purpose: PathPurpose, path: PathBuf, result: Result<(), String>) {
        let Some(prompt) = &mut self.sync.prompt else {
            return;
        };
        if prompt.purpose != purpose {
            return;
        }
        prompt.checking = false;
        let outcome = result.and_then(|()| self.apply_row_path(purpose, path));
        match outcome {
            Ok(()) => self.sync.prompt = None,
            Err(e) => {
                if let Some(p) = &mut self.sync.prompt {
                    p.error = Some(e);
                }
            }
        }
    }

    fn on_project_backed_up(
        &mut self,
        path: PathBuf,
        auto: bool,
        result: Result<Option<Box<ProjectBackupOutcome>>, Error>,
    ) {
        // The files changed or the sizes drifted: measure again.
        self.measure_path(&path);
        let name = abbreviate_home(&path);
        match result {
            Ok(outcome) => {
                self.sync.errors.remove(&path);
                if let Some(o) = outcome {
                    log::info!(
                        "project sync: uploaded {} ({} pruned, {} skipped)",
                        o.backup.file.name,
                        o.pruned,
                        o.skipped.len()
                    );
                    let skipped = o.skipped.len();
                    if o.skipped.is_empty() {
                        self.sync.skipped.remove(&path);
                    } else {
                        log::warn!("project sync: skipped {}", o.skipped.join("; "));
                        self.sync.skipped.insert(path.clone(), o.skipped);
                    }
                    if !auto {
                        let mut text = format!(
                            "Backed up {name} to Google Drive ({})",
                            human_size(o.backup.file.size)
                        );
                        if skipped > 0 {
                            text += &format!(
                                "; {skipped} file{} skipped (details on the Sync page)",
                                if skipped == 1 { "" } else { "s" }
                            );
                        }
                        self.toasts.success(&self.ctx, text);
                    }
                } else if !auto {
                    self.toasts.info(
                        &self.ctx,
                        format!("{name} is unchanged since its last backup"),
                    );
                }
            }
            Err(e) => {
                let signed_out = matches!(e, Error::SignedOut | Error::NotSignedIn);
                let nothing = matches!(e, Error::NothingToBackUp(_));
                let msg = self.drive_error(e);
                if signed_out {
                    self.sync.pass.clear();
                }
                self.sync.errors.insert(path, msg.clone());
                if !auto {
                    self.toasts
                        .error(&self.ctx, format!("Backup of {name} failed: {msg}"));
                } else if !nothing && !self.sync.pass_failure_shown {
                    self.sync.pass_failure_shown = true;
                    self.toasts.error(
                        &self.ctx,
                        format!("Automatic backup of {name} failed: {msg}"),
                    );
                } else {
                    log::warn!("automatic project backup of {name} failed: {msg}");
                }
            }
        }
    }

    // ---------------------------------------------------------------- sidebar backup dialog

    pub(super) fn open_backup_dialog(&mut self, path: PathBuf) {
        if !matches!(self.drive_menu(), DriveMenu::On) {
            return;
        }
        let row = self.cfg.project_sync.iter().find(|r| r.path == path);
        self.sync.backup = Some(BackupDialog {
            display: abbreviate_home(&path),
            omp: row.is_none_or(|r| r.omp),
            sessions: row.is_none_or(|r| r.sessions),
            keep: true,
            path: path.clone(),
        });
        self.measure_path(&path);
    }

    fn show_backup_dialog(&mut self, ctx: &egui::Context) {
        let busy = self.sync.job.is_some();
        let Some(d) = &mut self.sync.backup else {
            return;
        };
        let cells = cells_of(self.sync.sizes.get(&d.path));
        match sync_view::backup_dialog(ctx, d, &cells, busy) {
            BackupAnswer::Open => {}
            BackupAnswer::Cancel => {
                self.sync.backup = None;
                self.focus_pending = true;
            }
            BackupAnswer::Run => {
                let Some(d) = self.sync.backup.take() else {
                    return;
                };
                self.focus_pending = true;
                let want = Parts {
                    omp: d.omp,
                    sessions: d.sessions,
                };
                if d.keep {
                    match self.cfg.project_sync.iter_mut().find(|r| r.path == d.path) {
                        Some(r) => {
                            r.omp = d.omp;
                            r.sessions = d.sessions;
                        }
                        None => self.cfg.project_sync.push(SyncRow {
                            path: d.path.clone(),
                            omp: d.omp,
                            sessions: d.sessions,
                        }),
                    }
                    self.dirty = true;
                }
                if !self.start_project_backup(d.path, want, false) {
                    self.toasts.info(
                        &self.ctx,
                        "Another project job is running; try again in a moment.",
                    );
                }
            }
        }
    }

    // ---------------------------------------------------------------- restore

    /// Open the restore dialog; `only` fixes the target to that project folder.
    pub(super) fn open_restore(&mut self, only: Option<PathBuf>) {
        if self.sync.job.is_some() || !self.sync_signed_in() {
            self.toasts.info(
                &self.ctx,
                "Sign in and wait for the running project job first.",
            );
            return;
        }
        let target = only
            .as_deref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.sync.restore = Some(Restore {
            dlg: RestoreDialog::new(
                only.is_some(),
                target,
                self.omp.is_some(),
                self.picker_available,
            ),
            backups: Vec::new(),
            prepared: None,
            session_total: 0,
            only,
            probe_generation: 0,
            target_touched: false,
        });
        self.probe_target();
        self.start_project_listing();
    }

    fn start_project_listing(&mut self) {
        if let Some(r) = &mut self.sync.restore {
            r.dlg.list = ListState::Loading;
            r.dlg.sel = None;
        }
        let started = self.lane_job(Kind::Listing, |service, _, _| {
            SyncEvent::Listed(service.list_projects())
        });
        if !started && let Some(r) = &mut self.sync.restore {
            r.dlg.list = ListState::Failed("Another project job is running; try again.".to_owned());
        }
    }

    fn on_listed(&mut self, result: Result<Vec<ProjectBackup>, Error>) {
        let result = result.map_err(|e| self.drive_error(e));
        let Some(r) = &mut self.sync.restore else {
            return;
        };
        match result {
            Err(msg) => r.dlg.list = ListState::Failed(msg),
            Ok(backups) => {
                let only_id = r.only.as_deref().map(path_id);
                let groups = group_backups(&backups, &hostname(), only_id.as_deref());
                r.backups = backups;
                let first = (!groups.is_empty()).then_some((0, 0));
                r.dlg.list = ListState::Ready(groups);
                if let Some((p, v)) = first
                    && r.only.is_some()
                {
                    self.select_version(p, v);
                }
            }
        }
    }

    fn select_version(&mut self, p: usize, v: usize) {
        let Some(r) = &mut self.sync.restore else {
            return;
        };
        r.dlg.sel = Some((p, v));
        r.dlg.error = None;
        let omp_ok = r.dlg.omp_available;
        let (has_omp, has_sessions) = r
            .dlg
            .version()
            .map_or((false, false), |v| (v.has_omp, v.has_sessions));
        r.dlg.omp = has_omp;
        r.dlg.sessions = has_sessions && omp_ok;
        // Offer the original folder when it exists here.
        if !r.dlg.target_fixed && !r.target_touched {
            r.dlg.target.clear();
            r.dlg.target_status = TargetStatus::Unknown;
            let original = match &r.dlg.list {
                ListState::Ready(g) => g.get(p).map(|g| g.path.clone()),
                _ => None,
            };
            if let Some(path) = original.filter(|p| p.starts_with('/')) {
                r.probe_generation += 1;
                let generation = r.probe_generation;
                let path = PathBuf::from(path);
                omp_worker::spawn(&self.ctx, &self.events_tx, move || {
                    let exists = path.is_dir();
                    AppEvent::Sync(SyncEvent::Probed {
                        generation,
                        path,
                        exists,
                        preset: true,
                    })
                });
            }
        }
    }

    /// Does the typed restore target exist? Answered by a worker.
    fn probe_target(&mut self) {
        let home = self.home.clone();
        let Some(r) = &mut self.sync.restore else {
            return;
        };
        r.probe_generation += 1;
        r.dlg.target_status = TargetStatus::Unknown;
        let Ok(path) = transfer::lexical_dest(&r.dlg.target, home.as_deref()) else {
            return;
        };
        let generation = r.probe_generation;
        omp_worker::spawn(&self.ctx, &self.events_tx, move || {
            let exists = path.is_dir();
            AppEvent::Sync(SyncEvent::Probed {
                generation,
                path,
                exists,
                preset: false,
            })
        });
    }

    fn restore_progress(&self) -> Option<ProgView> {
        self.sync.job.as_ref().and_then(|j| j.shared.view())
    }

    fn show_restore_dialog(&mut self, ctx: &egui::Context) {
        let progress = self.restore_progress();
        let busy = self.sync.job.is_some();
        let Some(r) = &mut self.sync.restore else {
            return;
        };
        let Some(action) = sync_view::restore_dialog(ctx, &mut r.dlg, progress.as_ref(), busy)
        else {
            return;
        };
        match action {
            RestoreAction::Retry => self.start_project_listing(),
            RestoreAction::Browse => self.pick_folder(Pick::RestoreTarget),
            RestoreAction::TargetEdited => {
                r.target_touched = true;
                self.probe_target();
            }
            RestoreAction::Select(p, v) => self.select_version(p, v),
            RestoreAction::Continue => self.restore_prepare(),
            RestoreAction::Confirm => self.restore_apply(),
            RestoreAction::Cancel => match r.dlg.step {
                Step::Applying => {
                    r.dlg.cancelling = true;
                    if let Some(j) = &self.sync.job {
                        j.cancel.store(true, Ordering::Relaxed);
                    }
                }
                _ => {
                    // A running list/prepare download is told to stop; its result is dropped.
                    if let Some(j) = &self.sync.job {
                        j.cancel.store(true, Ordering::Relaxed);
                    }
                    self.sync.restore = None;
                    self.focus_pending = true;
                }
            },
            RestoreAction::Close => {
                self.sync.restore = None;
                self.focus_pending = true;
            }
        }
    }

    fn restore_prepare(&mut self) {
        let home = self.home.clone();
        let Some(r) = &mut self.sync.restore else {
            return;
        };
        r.dlg.error = None;
        let Some(index) = r.dlg.version().map(|v| v.backup) else {
            return;
        };
        let Some(backup) = r.backups.get(index).cloned() else {
            return;
        };
        let target = match transfer::lexical_dest(&r.dlg.target, home.as_deref()) {
            Ok(t) => t,
            Err(e) => {
                self.toasts.error(&self.ctx, e);
                return;
            }
        };
        let parts = Parts {
            omp: r.dlg.omp,
            sessions: r.dlg.sessions,
        };
        let root = self.root.clone();
        let started = self.lane_job(Kind::Preparing, move |service, cancel, report| {
            SyncEvent::Prepared(
                service
                    .prepare_project_restore(&backup, &target, parts, &root, cancel, report)
                    .map(Box::new),
            )
        });
        if started && let Some(r) = &mut self.sync.restore {
            r.dlg.step = Step::Preparing;
        } else if !started {
            self.toasts.info(
                &self.ctx,
                "Another project job is running; try again in a moment.",
            );
        }
    }

    fn on_prepared(&mut self, result: Result<Box<PreparedProjectRestore>, Error>) {
        let result = result.map_err(|e| (matches!(e, Error::Cancelled), self.drive_error(e)));
        let Some(r) = &mut self.sync.restore else {
            return;
        };
        match result {
            Err((cancelled, msg)) => {
                r.dlg.step = Step::Pick;
                if !cancelled {
                    r.dlg.error = Some(format!("Cannot restore that backup: {msg}"));
                }
            }
            Ok(prepared) => {
                let plan = &prepared.plan;
                let sessions = (!plan.sessions.is_empty()).then(|| {
                    (
                        plan.sessions.len(),
                        plan.sessions.iter().filter(|s| s.new_id).count(),
                    )
                });
                r.dlg.step = Step::Confirm(PlanView {
                    target: abbreviate_home(&plan.target),
                    create_target: plan.create_target,
                    omp_exists: plan.omp_exists,
                    omp_overwritten_total: plan.omp_overwritten.len(),
                    omp_overwritten: plan
                        .omp_overwritten
                        .iter()
                        .take(PLAN_LIST)
                        .cloned()
                        .collect(),
                    omp_added: plan.omp_added.len(),
                    omp_skipped: plan.omp_skipped.clone(),
                    session_problems: plan
                        .sessions
                        .iter()
                        .filter_map(|s| Some(format!("{}: {}", s.title, s.problem.as_ref()?)))
                        .collect(),
                    sessions,
                    safety: "OmpDeck's restore-safety folder".to_owned(),
                });
                r.session_total = prepared.plan.sessions.len();
                r.prepared = Some(prepared);
            }
        }
    }

    fn restore_apply(&mut self) {
        let omp = self.omp.clone();
        let Some(r) = &mut self.sync.restore else {
            return;
        };
        let Some(prepared) = r.prepared.take() else {
            return;
        };
        r.dlg.cancelling = false;
        let started = self.lane_job(Kind::Restoring, move |service, cancel, report| {
            SyncEvent::Restored(service.apply_project_restore(
                *prepared,
                omp.as_ref(),
                cancel,
                report,
            ))
        });
        if started && let Some(r) = &mut self.sync.restore {
            r.dlg.step = Step::Applying;
        } else if !started {
            self.toasts.info(
                &self.ctx,
                "Another project job is running; try again in a moment.",
            );
        }
    }

    fn on_restored(&mut self, result: Result<ProjectRestoreReport, Error>) {
        match result {
            Ok(report) => {
                let total = self.sync.restore.as_ref().map_or(0, |r| r.session_total);
                let mut parts = Vec::new();
                if report.omp_written > 0 {
                    parts.push(format!(
                        ".omp: {} file{} written ({} overwritten)",
                        report.omp_written,
                        if report.omp_written == 1 { "" } else { "s" },
                        report.omp_overwritten
                    ));
                }
                let added = report.sessions_added.len();
                if report.cancelled && total > 0 {
                    parts.push(format!("{added} of {total} sessions added"));
                } else if added > 0 || !report.sessions_failed.is_empty() {
                    parts.push(format!(
                        "{added} session{} added",
                        if added == 1 { "" } else { "s" }
                    ));
                }
                let mut summary = match (parts.is_empty(), report.cancelled) {
                    (true, true) => "Restore cancelled before anything was restored.".to_owned(),
                    (true, false) => "Nothing was restored.".to_owned(),
                    (false, true) => {
                        format!("Restore cancelled. Already restored: {}.", parts.join(", "))
                    }
                    (false, false) => format!("Restored from Google Drive: {}.", parts.join(", ")),
                };
                if let Some(dir) = &report.safety_dir {
                    summary += &format!(
                        " Overwritten files were copied to {}.",
                        abbreviate_home(dir)
                    );
                }
                self.refresh_sessions();
                let details = !report.sessions_failed.is_empty() || !report.omp_skipped.is_empty();
                if !details {
                    if report.cancelled {
                        self.toasts.info(&self.ctx, summary);
                    } else {
                        self.toasts.success(&self.ctx, summary);
                    }
                    self.sync.restore = None;
                    self.focus_pending = true;
                } else if let Some(r) = &mut self.sync.restore {
                    r.dlg.step = Step::Report(ReportView {
                        summary,
                        failures: report.sessions_failed,
                        notes: report.omp_skipped,
                    });
                } else {
                    self.toasts.error(
                        &self.ctx,
                        format!(
                            "{summary} {} session(s) could not be restored, {} file(s) not written.",
                            report.sessions_failed.len(),
                            report.omp_skipped.len()
                        ),
                    );
                }
            }
            Err(e) => {
                let cancelled = matches!(e, Error::Cancelled);
                let msg = self.drive_error(e);
                if cancelled {
                    self.toasts.info(&self.ctx, "Restore cancelled");
                } else {
                    self.toasts
                        .error(&self.ctx, format!("Restore failed: {msg}"));
                }
                self.refresh_sessions();
                self.sync.restore = None;
                self.focus_pending = true;
            }
        }
    }

    // ---------------------------------------------------------------- dialogs

    pub(super) fn show_sync_dialogs(&mut self, ctx: &egui::Context) {
        self.show_backup_dialog(ctx);
        self.show_restore_dialog(ctx);
        self.show_path_prompt(ctx);
    }

    fn show_path_prompt(&mut self, ctx: &egui::Context) {
        let Some(p) = &mut self.sync.prompt else {
            return;
        };
        match sync_view::path_dialog(ctx, p) {
            PathAnswer::Open => {}
            PathAnswer::Cancel => self.sync.prompt = None,
            PathAnswer::Confirm(text) => {
                let purpose = p.purpose;
                match transfer::lexical_dest(&text, self.home.as_deref()) {
                    Err(e) => p.error = Some(e),
                    Ok(path) => {
                        p.checking = true;
                        p.error = None;
                        omp_worker::spawn(&self.ctx, &self.events_tx, move || {
                            let result = if path.is_dir() {
                                Ok(())
                            } else {
                                Err("That folder does not exist".to_owned())
                            };
                            AppEvent::Sync(SyncEvent::Checked {
                                purpose,
                                path,
                                result,
                            })
                        });
                    }
                }
            }
        }
    }
}

/// Groups `backups` (newest first) by (host, path id); groups appear in order of their newest
/// version. `only_id` keeps just the backups of that folder.
fn group_backups(backups: &[ProjectBackup], me: &str, only_id: Option<&str>) -> Vec<CloudProject> {
    let mut groups: Vec<CloudProject> = Vec::new();
    let mut keys: Vec<(&str, &str)> = Vec::new();
    for (i, b) in backups.iter().enumerate() {
        if only_id.is_some_and(|id| id != b.path_id) {
            continue;
        }
        let key = (b.host.as_str(), b.path_id.as_str());
        let at = match keys.iter().position(|k| *k == key) {
            Some(at) => at,
            None => {
                keys.push(key);
                groups.push(CloudProject {
                    name: b.name.clone(),
                    path: b.path.clone(),
                    this_pc: b.host == me,
                    host: b.host.clone(),
                    versions: Vec::new(),
                });
                groups.len() - 1
            }
        };
        if let Some(g) = groups.get_mut(at) {
            g.versions.push(version_row(i, b));
        }
    }
    groups
}

fn version_row(index: usize, b: &ProjectBackup) -> VersionRow {
    let mut parts = Vec::new();
    if let Some(bytes) = b.omp_bytes {
        parts.push(format!(".omp {}", human_size(bytes)));
    }
    if let Some(n) = b.sessions {
        parts.push(format!(
            "{n} session{} {}",
            if n == 1 { "" } else { "s" },
            human_size(b.session_bytes.unwrap_or(0))
        ));
    }
    VersionRow {
        backup: index,
        when: b
            .file
            .created_unix()
            .map_or_else(|| b.file.created.clone(), clock),
        summary: parts.join(" · "),
        has_omp: b.omp_bytes.is_some(),
        has_sessions: b.sessions.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use deck_core::gdrive::BackupFile;

    use super::*;

    fn backup(
        host: &str,
        pid: &str,
        created: &str,
        omp: Option<u64>,
        sess: Option<u64>,
    ) -> ProjectBackup {
        ProjectBackup {
            file: BackupFile {
                id: format!("{host}{pid}{created}"),
                name: "n".into(),
                size: 10,
                created: created.into(),
                props: BTreeMap::new(),
            },
            host: host.into(),
            path_id: pid.into(),
            path: format!("/work/{pid}"),
            name: pid.into(),
            omp_bytes: omp,
            sessions: sess,
            session_bytes: sess.map(|_| 2048),
        }
    }

    fn sizes(omp: OmpDir, sessions: u64) -> ProjectSizes {
        ProjectSizes {
            omp,
            sessions,
            session_bytes: 4096,
        }
    }

    #[test]
    fn backup_groups_by_host_and_folder_newest_first() {
        let list = [
            backup("a", "p1", "2026-10-22T10:00:00Z", Some(1), None),
            backup("b", "p1", "2026-10-21T10:00:00Z", Some(1), Some(2)),
            backup("a", "p1", "2026-10-20T10:00:00Z", Some(1), Some(1)),
            backup("a", "p2", "2026-10-19T10:00:00Z", None, Some(1)),
        ];
        let g = group_backups(&list, "a", None);
        assert_eq!(g.len(), 3);
        assert!(g[0].this_pc && !g[1].this_pc);
        // Versions keep their order and point back into the list.
        let idx: Vec<usize> = g[0].versions.iter().map(|v| v.backup).collect();
        assert_eq!(idx, [0, 2]);
        assert!(g[0].versions[0].has_omp && !g[0].versions[0].has_sessions);
        assert!(g[0].versions[1].has_sessions);
    }

    #[test]
    fn restore_from_a_folder_lists_only_its_backups() {
        let list = [
            backup("a", "p1", "2026-10-22T10:00:00Z", Some(1), None),
            backup("a", "p2", "2026-10-21T10:00:00Z", Some(1), None),
        ];
        let g = group_backups(&list, "a", Some("p2"));
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].versions[0].backup, 1);
    }

    #[test]
    fn version_summary_names_the_parts_present() {
        let b = backup("a", "p", "2026-10-22T10:00:00Z", Some(2048), Some(1));
        let v = version_row(0, &b);
        assert_eq!(v.summary, ".omp 2.0 KB · 1 session 2.0 KB");
        let only = version_row(0, &backup("a", "p", "2026-10-22T10:00:00Z", None, Some(3)));
        assert!(only.summary.starts_with("3 sessions"));
        assert!(!only.has_omp && only.has_sessions);
    }

    #[test]
    fn backups_skip_parts_that_do_not_exist() {
        let want = Parts {
            omp: true,
            sessions: true,
        };
        let both = usable_parts(want, &sizes(OmpDir::Present { files: 3, bytes: 9 }, 2));
        assert_eq!(both, want);
        let no_omp = usable_parts(want, &sizes(OmpDir::Missing, 2));
        assert_eq!(
            no_omp,
            Parts {
                omp: false,
                sessions: true
            }
        );
        let global = usable_parts(want, &sizes(OmpDir::Global, 0));
        assert!(!global.any());
        let switched_off = usable_parts(
            Parts {
                omp: false,
                sessions: true,
            },
            &sizes(OmpDir::Present { files: 1, bytes: 1 }, 1),
        );
        assert_eq!(
            switched_off,
            Parts {
                omp: false,
                sessions: true
            }
        );
    }

    #[test]
    fn size_cells_describe_what_was_measured() {
        assert_eq!(cells_of(None).omp, OmpCell::Loading);
        let ok = Ok(sizes(
            OmpDir::Present {
                files: 1,
                bytes: 1024,
            },
            1,
        ));
        let c = cells_of(Some(&ok));
        assert_eq!(c.omp, OmpCell::Present("1.0 KB · 1 file".into()));
        assert_eq!(
            c.sessions,
            SessionsCell::Ready {
                count: 1,
                text: "1 session · 4.0 KB".into()
            }
        );
        assert!(c.omp_usable() && c.sessions_usable());
        let none = Ok(sizes(OmpDir::Global, 0));
        let c = cells_of(Some(&none));
        assert!(!c.omp_usable() && !c.sessions_usable());
        let failed: Result<ProjectSizes, String> = Err("denied".into());
        assert_eq!(
            cells_of(Some(&failed)).omp,
            OmpCell::Failed("denied".into())
        );
    }

    #[test]
    fn symlinked_omp_is_never_offered() {
        let r = Ok(sizes(OmpDir::Symlink, 1));
        let c = cells_of(Some(&r));
        assert_eq!(c.omp, OmpCell::Symlink);
        assert!(!c.omp_usable());
        let want = Parts {
            omp: true,
            sessions: true,
        };
        assert_eq!(
            usable_parts(want, &sizes(OmpDir::Symlink, 1)),
            Parts {
                omp: false,
                sessions: true
            }
        );
    }
}
