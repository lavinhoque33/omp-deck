//! App-side glue for OmpDeck self-update: the startup sweep + install detection, the release
//! check (startup, every 6 h, manual), the download/verify/apply worker and the restart.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use deck_core::selfupdate::{self, Applied, InstallKind, Release, UpdateError};

use super::{App, AppEvent};
use crate::app_update::{
    self as au, AppUpdateTracker, CheckState, Phase, PillView, SettingsAction, SettingsCard,
    UpdateAction,
};
use crate::omp_worker;

/// Minimum time between two repaints requested by download progress.
const PROGRESS_REPAINT_MS: u64 = 100;

/// What a self-update worker reports.
pub(super) enum AppUpdateEvent {
    /// How this copy was installed (and the Windows leftovers were swept).
    Detected(InstallKind),
    Checked(Result<Release, String>),
    /// The download is verified; installing now.
    Applying,
    Finished(Result<Applied, UpdateError>, Option<tempfile::TempDir>),
}

/// Download progress shared with the worker (read once per frame).
#[derive(Default)]
struct Progress {
    done: AtomicU64,
    /// 0 = unknown.
    total: AtomicU64,
    last_repaint_ms: AtomicU64,
}

pub(super) struct AppUpdateUi {
    pub(super) tracker: AppUpdateTracker,
    started: bool,
    cancel: Arc<AtomicBool>,
    progress: Arc<Progress>,
    /// The window was asked to close while the worker ran; close once it reports.
    close_after: bool,
    /// The verified package a shown manual install command points to.
    payload: Option<tempfile::TempDir>,
}

impl AppUpdateUi {
    pub(super) fn new() -> AppUpdateUi {
        AppUpdateUi {
            tracker: AppUpdateTracker::default(),
            started: false,
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(Progress::default()),
            close_after: false,
            payload: None,
        }
    }
}

impl App {
    /// Per frame: startup work, download progress and the 6 h schedule.
    pub(super) fn tick_app_update(&mut self, ctx: &egui::Context) {
        if !self.app_update.started {
            self.app_update.started = true;
            omp_worker::spawn(&self.ctx, &self.events_tx, || {
                selfupdate::sweep_leftovers();
                au::sweep_stale_update_dirs(&std::env::temp_dir(), au::STALE_AFTER);
                AppEvent::AppUpdate(AppUpdateEvent::Detected(selfupdate::detect()))
            });
        }
        if let Phase::Downloading { .. } = self.app_update.tracker.phase {
            let p = &self.app_update.progress;
            let total = p.total.load(Ordering::Relaxed);
            self.app_update.tracker.phase = Phase::Downloading {
                done: p.done.load(Ordering::Relaxed),
                total: (total > 0).then_some(total),
            };
        }
        // Checks wait for the install detection, and a development build never checks.
        let may_check = matches!(&self.app_update.tracker.kind, Some(k) if *k != InstallKind::Dev);
        if may_check
            && self
                .app_update
                .tracker
                .due(Instant::now(), self.cfg.check_app_updates)
        {
            self.begin_app_check(false);
        }
        self.guard_close_during_update(ctx);
        ctx.request_repaint_after(Duration::from_secs(60));
    }

    fn begin_app_check(&mut self, manual: bool) {
        self.app_update.tracker.begin(Instant::now(), manual);
        omp_worker::spawn(&self.ctx, &self.events_tx, || {
            AppEvent::AppUpdate(AppUpdateEvent::Checked(
                selfupdate::latest().map_err(|e| e.to_string()),
            ))
        });
    }

    /// "Check for OmpDeck updates" (menu, Settings, palette). A development build never checks.
    pub(super) fn check_app_update_manually(&mut self) {
        match &self.app_update.tracker.kind {
            None => self.toasts.info(
                &self.ctx,
                "Still detecting how OmpDeck was installed; try again in a moment",
            ),
            Some(InstallKind::Dev) => {
                let why = au::cannot_update(&InstallKind::Dev).unwrap_or_default();
                self.toasts.info(&self.ctx, why);
            }
            Some(_) => {
                if !self.app_update.tracker.is_checking() {
                    self.begin_app_check(true);
                }
            }
        }
    }

    /// Why "Update and restart" cannot start right now (`None` = [`App::start_app_update`] works).
    pub(super) fn app_update_blocked(&self) -> Option<&'static str> {
        let t = &self.app_update.tracker;
        if t.phase.is_busy() {
            return Some("An OmpDeck update is already running");
        }
        if t.available().is_none() {
            return Some("No newer OmpDeck version is known; check for updates first");
        }
        match &t.kind {
            None => Some("Still detecting how OmpDeck was installed"),
            Some(k) if au::cannot_update(k).is_some() => {
                Some("This copy of OmpDeck cannot update itself")
            }
            Some(_) => None,
        }
    }

    /// "Update and restart": download, verify, install, then restart into the new version.
    pub(super) fn start_app_update(&mut self) {
        if self.app_update_blocked().is_some() {
            return;
        }
        let t = &self.app_update.tracker;
        let (Some(release), Some(kind)) = (t.available().cloned(), t.kind.clone()) else {
            return;
        };
        if au::cannot_update(&kind).is_some() {
            return;
        }
        self.app_update.cancel.store(false, Ordering::Relaxed);
        self.app_update.progress.done.store(0, Ordering::Relaxed);
        self.app_update.progress.total.store(0, Ordering::Relaxed);
        self.app_update.tracker.phase = Phase::Downloading {
            done: 0,
            total: None,
        };
        let (cancel, progress) = (
            self.app_update.cancel.clone(),
            self.app_update.progress.clone(),
        );
        let (ctx, tx) = (self.ctx.clone(), self.events_tx.clone());
        self.app_update.payload = None;
        omp_worker::spawn(&self.ctx, &self.events_tx, move || {
            let (result, keep) = install(&kind, &release, &cancel, &progress, &ctx, &tx);
            AppEvent::AppUpdate(AppUpdateEvent::Finished(result, keep))
        });
    }

    pub(super) fn on_app_update_event(&mut self, ev: AppUpdateEvent) {
        match ev {
            AppUpdateEvent::Detected(kind) => {
                log::info!("OmpDeck install kind: {kind:?}");
                self.app_update.tracker.kind = Some(kind);
            }
            AppUpdateEvent::Checked(result) => self.on_app_checked(result),
            AppUpdateEvent::Applying => self.app_update.tracker.phase = Phase::Applying,
            AppUpdateEvent::Finished(Ok(applied), _) => {
                // A close requested while it ran just closes: no successor, the new version
                // starts next time.
                if !std::mem::take(&mut self.app_update.close_after) {
                    au::set_restart(applied.restart);
                }
                self.app_update.tracker.phase = Phase::Restarting;
                // The normal close path: `on_exit` flushes the state, `main` starts the successor
                // once the terminals are detached.
                self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            AppUpdateEvent::Finished(Err(UpdateError::Cancelled), _) => {
                self.app_update.tracker.phase = Phase::Idle;
                self.finish_deferred_close();
            }
            AppUpdateEvent::Finished(Err(UpdateError::NeedsManual { command }), keep) => {
                self.app_update.payload = keep;
                self.app_update.tracker.phase = Phase::NeedsManual { command };
                self.finish_deferred_close();
            }
            AppUpdateEvent::Finished(Err(e), _) => {
                let text = e.to_string();
                log::warn!("OmpDeck update failed: {text}");
                self.toasts
                    .error(&self.ctx, format!("OmpDeck update failed: {text}"));
                self.app_update.tracker.phase = Phase::Failed(text);
                self.finish_deferred_close();
            }
        }
    }

    /// Per frame: a window close while the update worker runs is held back until it reports, so
    /// the thread is never killed halfway through replacing the installed files. A download is
    /// cancelled; an install is awaited.
    fn guard_close_during_update(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        match self.app_update.tracker.phase {
            Phase::Downloading { .. } => {
                self.app_update.cancel.store(true, Ordering::Relaxed);
            }
            Phase::Applying => {
                if !self.app_update.close_after {
                    self.toasts.info(ctx, "Finishing the OmpDeck update…");
                }
            }
            _ => return,
        }
        self.app_update.close_after = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
    }

    /// The worker reported without restarting: close now when the user asked to while it ran.
    fn finish_deferred_close(&mut self) {
        if std::mem::take(&mut self.app_update.close_after) {
            self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn on_app_checked(&mut self, result: Result<Release, String>) {
        let manual = std::mem::take(&mut self.app_update.tracker.manual);
        self.app_update.tracker.finish(result, au::CURRENT);
        let state = self.app_update.tracker.check.clone();
        match &state {
            CheckState::Available(r) => {
                if manual && self.cfg.skipped_app_version.as_deref() == Some(r.version.as_str()) {
                    // Asking explicitly brings a skipped version back.
                    self.cfg.skipped_app_version = None;
                    self.dirty = true;
                }
                if manual {
                    self.toasts
                        .info(&self.ctx, format!("OmpDeck {} is available", r.version));
                }
            }
            CheckState::UpToDate if manual => {
                self.toasts
                    .success(&self.ctx, format!("OmpDeck {} is up to date", au::CURRENT));
            }
            CheckState::Error(e) => {
                log::warn!("OmpDeck update check failed: {e}");
                if manual {
                    self.toasts
                        .error(&self.ctx, format!("OmpDeck update check failed: {e}"));
                }
            }
            _ => {}
        }
    }

    pub(super) fn apply_app_update_action(&mut self, action: UpdateAction) {
        match action {
            UpdateAction::Install => self.start_app_update(),
            UpdateAction::Cancel => self.app_update.cancel.store(true, Ordering::Relaxed),
            UpdateAction::OpenPage(url) => self.open_external(url),
            UpdateAction::CopyCommand(c) => self.copy_text(&c, "command"),
            UpdateAction::Skip(version) => {
                self.cfg.skipped_app_version = Some(version);
                self.dirty = true;
            }
            UpdateAction::Check => self.check_app_update_manually(),
        }
    }

    pub(super) fn apply_app_update_setting(&mut self, action: SettingsAction) {
        match action {
            SettingsAction::SetCheck(on) => {
                self.cfg.check_app_updates = on;
                self.dirty = true;
            }
            SettingsAction::CheckNow => self.check_app_update_manually(),
            SettingsAction::Install => self.start_app_update(),
            SettingsAction::Cancel => self.app_update.cancel.store(true, Ordering::Relaxed),
        }
    }

    /// What the title-bar pill shows: a known, not skipped, newer release.
    pub(super) fn app_update_bar(&self) -> Option<PillView<'_>> {
        let t = &self.app_update.tracker;
        let release = t.offered(self.cfg.skipped_app_version.as_deref())?;
        Some(PillView {
            release,
            phase: &t.phase,
            kind: t.kind.as_ref(),
        })
    }
}

impl AppUpdateUi {
    /// The Settings card input, borrowing only this state (the page also borrows other `App` fields).
    pub(super) fn card(&self, check: bool) -> SettingsCard<'_> {
        let t = &self.tracker;
        SettingsCard {
            check,
            state: &t.check,
            available: t.available(),
            phase: &t.phase,
            kind: t.kind.as_ref(),
        }
    }
}

/// Worker body: download + verify into a temporary folder, then install. The folder is removed
/// when this returns, except when the install needs a manual step: the verified package stays
/// there for the command that is shown, and the guard travels back with the result.
fn install(
    kind: &InstallKind,
    release: &Release,
    cancel: &AtomicBool,
    progress: &Progress,
    ctx: &egui::Context,
    tx: &std::sync::mpsc::Sender<AppEvent>,
) -> (Result<Applied, UpdateError>, Option<tempfile::TempDir>) {
    let Some(asset) = selfupdate::asset_for(kind, &release.version) else {
        return (
            Err(UpdateError::NoAsset(format!(
                "{} has no download for this system",
                release.tag
            ))),
            None,
        );
    };
    let dir = match tempfile::Builder::new()
        .prefix("omp-deck-update-")
        .tempdir()
    {
        Ok(d) => d,
        Err(e) => return (Err(UpdateError::Io(e)), None),
    };
    let began = Instant::now();
    let file = match selfupdate::download(
        release,
        &asset,
        dir.path(),
        &|done, total| {
            progress.done.store(done, Ordering::Relaxed);
            progress.total.store(total.unwrap_or(0), Ordering::Relaxed);
            let now = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
            let last = progress.last_repaint_ms.load(Ordering::Relaxed);
            if now.saturating_sub(last) >= PROGRESS_REPAINT_MS {
                progress.last_repaint_ms.store(now, Ordering::Relaxed);
                ctx.request_repaint();
            }
        },
        cancel,
    ) {
        Ok(f) => f,
        Err(e) => return (Err(e), None),
    };
    let _ = tx.send(AppEvent::AppUpdate(AppUpdateEvent::Applying));
    ctx.request_repaint();
    let result = selfupdate::apply(kind, &file);
    let keep = matches!(result, Err(UpdateError::NeedsManual { .. })).then_some(dir);
    (result, keep)
}
