//! App-side glue for OmpDeck self-update. The gateway detects the install kind, checks for
//! releases on its schedule, downloads and installs; this window asks for checks and installs,
//! shows the state the gateway publishes and restarts into the new version once it is in place.

use deck_core::selfupdate::{Applied, InstallKind};
use deck_service::controller::RequestKey;
use deck_service::gateway::protocol::{Mutation, Topic};

use super::App;
use crate::app_update::{
    self as au, Adopted, AppUpdateTracker, Checked, Phase, PillView, SettingsAction, SettingsCard,
    UpdateAction,
};

pub(super) struct AppUpdateUi {
    pub(super) tracker: AppUpdateTracker,
    /// The gateway revision of the update topic last adopted.
    revision: u64,
    /// The download this window started (cancelled with it).
    download: Option<RequestKey>,
    /// The window was asked to close while the install ran; close once it reports.
    close_after: bool,
}

impl AppUpdateUi {
    pub(super) fn new() -> AppUpdateUi {
        AppUpdateUi {
            tracker: AppUpdateTracker::default(),
            revision: 0,
            download: None,
            close_after: false,
        }
    }
}

impl App {
    /// Per frame: adopt the gateway's update state and guard a close during an install.
    pub(super) fn tick_app_update(&mut self, ctx: &egui::Context) {
        let revision = self.gw.revision(&Topic::AppUpdate);
        if revision != self.app_update.revision {
            self.app_update.revision = revision;
            if let Some(view) = self.gw.app_update() {
                let adopted = self.app_update.tracker.adopt(&view);
                self.on_app_adopted(adopted);
            }
        }
        self.guard_close_during_update(ctx);
    }

    fn on_app_adopted(&mut self, adopted: Adopted) {
        match adopted {
            Adopted::Nothing => {}
            Adopted::Checked(checked) => self.on_app_checked(checked),
            Adopted::Applied(applied) => self.restart_into(&applied),
        }
    }

    /// The new version is in place: close through the normal path (`on_exit` flushes the state
    /// and `main` starts the successor once the terminals are detached). A close the user
    /// requested while the install ran just closes: the new version starts next time.
    fn restart_into(&mut self, applied: &Applied) {
        if !std::mem::take(&mut self.app_update.close_after) {
            au::set_restart((
                applied.executable.clone(),
                std::env::args_os().skip(1).collect(),
            ));
        }
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// "Check for OmpDeck updates" (menu, Settings, palette). A development build never checks.
    pub(super) fn check_app_update_manually(&mut self) {
        match &self.app_update.tracker.kind {
            None => self.toasts.info(
                &self.ctx,
                "Waiting for the gateway to report how OmpDeck was installed; try again in a moment",
            ),
            Some(InstallKind::Dev) => {
                let why = au::cannot_update(&InstallKind::Dev).unwrap_or_default();
                self.toasts.info(&self.ctx, why);
            }
            Some(_) => {
                if self.app_update.tracker.is_checking() {
                    return;
                }
                self.app_update.tracker.ask();
                self.gw.mutate(Mutation::AppUpdateCheck, |app, result| {
                    if let Err(e) = result {
                        app.app_update.tracker.abandon_ask();
                        app.toasts
                            .error(&app.ctx, format!("OmpDeck update check failed: {e}"));
                    }
                });
            }
        }
    }

    /// Why "Update and restart" cannot start right now (`None` = [`App::start_app_update`] works).
    pub(super) fn app_update_blocked(&self) -> Option<&'static str> {
        let t = &self.app_update.tracker;
        if t.phase.is_busy() {
            return Some("An OmpDeck update is already running");
        }
        if !self.gw.is_up() {
            return Some("The gateway is not connected");
        }
        if t.available().is_none() {
            return Some("No newer OmpDeck version is known; check for updates first");
        }
        match &t.kind {
            None => Some("Waiting for the gateway to report how OmpDeck was installed"),
            Some(k) if au::cannot_update(k).is_some() => {
                Some("This copy of OmpDeck cannot update itself")
            }
            Some(_) => None,
        }
    }

    /// "Update and restart": the gateway downloads, verifies and installs; this window restarts
    /// into the new version once it is in place.
    pub(super) fn start_app_update(&mut self) {
        if self.app_update_blocked().is_some() {
            return;
        }
        let t = &mut self.app_update.tracker;
        t.installing = true;
        t.phase = Phase::Downloading {
            done: 0,
            total: None,
        };
        let key = self
            .gw
            .mutate(Mutation::AppUpdateDownload, |app, result| match result {
                Ok(_) => {
                    app.app_update.download = None;
                    app.app_update.tracker.phase = Phase::Applying;
                    app.gw.mutate(Mutation::AppUpdateApply, |app, result| {
                        if let Err(e) = result {
                            app.on_app_update_failed(e);
                        }
                    });
                }
                Err(e) => app_download_failed(app, e),
            });
        self.app_update.download = Some(key);
    }

    fn on_app_update_failed(&mut self, text: String) {
        log::warn!("OmpDeck update failed: {text}");
        self.toasts
            .error(&self.ctx, format!("OmpDeck update failed: {text}"));
        let t = &mut self.app_update.tracker;
        t.installing = false;
        t.phase = Phase::Failed(text);
        self.finish_deferred_close();
    }

    /// Per frame: a window close while the install runs is held back until it reports, so the
    /// installer is never killed halfway through replacing the installed files. A download is
    /// cancelled; an install is awaited.
    fn guard_close_during_update(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        match self.app_update.tracker.phase {
            Phase::Downloading { .. } => {
                self.cancel_app_download();
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

    /// The install reported without restarting: close now when the user asked to while it ran.
    fn finish_deferred_close(&mut self) {
        if std::mem::take(&mut self.app_update.close_after) {
            self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn on_app_checked(&mut self, checked: Checked) {
        match checked {
            Checked::Available(r) => {
                if self.cfg.skipped_app_version.as_deref() == Some(r.version.as_str()) {
                    // Asking explicitly brings a skipped version back.
                    self.cfg.skipped_app_version = None;
                    self.dirty = true;
                }
                self.toasts
                    .info(&self.ctx, format!("OmpDeck {} is available", r.version));
            }
            Checked::UpToDate => {
                self.toasts
                    .success(&self.ctx, format!("OmpDeck {} is up to date", au::CURRENT));
            }
            Checked::Error(e) => {
                log::warn!("OmpDeck update check failed: {e}");
                self.toasts
                    .error(&self.ctx, format!("OmpDeck update check failed: {e}"));
            }
        }
    }

    fn cancel_app_download(&mut self) {
        if let Some(key) = self.app_update.download.clone() {
            self.gw.cancel(key, |_, r| {
                if let Err(e) = r {
                    log::debug!("the update download was not cancelled: {e}");
                }
            });
        }
    }

    pub(super) fn apply_app_update_action(&mut self, action: UpdateAction) {
        match action {
            UpdateAction::Install => self.start_app_update(),
            UpdateAction::Cancel => self.cancel_app_download(),
            UpdateAction::OpenPage(url) => self.open_external(url),
            UpdateAction::Skip(version) => {
                self.cfg.skipped_app_version = Some(version);
                self.dirty = true;
            }
            UpdateAction::Check => self.check_app_update_manually(),
            UpdateAction::CopyCommand(text) => self.copy_text(&text, "update command"),
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
            SettingsAction::Cancel => self.cancel_app_download(),
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

/// A download that did not finish: cancelled by the user, or failed.
fn app_download_failed(app: &mut App, e: String) {
    app.app_update.download = None;
    if e == "cancelled" {
        let t = &mut app.app_update.tracker;
        t.installing = false;
        t.phase = Phase::Idle;
        app.finish_deferred_close();
    } else {
        app.on_app_update_failed(e);
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
