//! OmpDeck self-update: the window's picture of the gateway's update state, the install phase,
//! the title-bar pill + popover and the Settings card. Pure state and painting; the gateway
//! checks, downloads and installs (`app/app_update.rs` sends the requests).

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;

use deck_core::selfupdate::{self, Applied, InstallKind, Release};
use deck_service::gateway::protocol::{AppUpdateView, UpdatePhase};
use egui::{Color32, ProgressBar, RichText, Ui};

use crate::icons::{
    ICON_CIRCLE_ARROW_UP, ICON_COPY, ICON_DOWNLOAD, ICON_EXTERNAL_LINK, ICON_REFRESH_CW, ICON_X,
};
use crate::theme::{self, Palette};
use crate::widgets::{DraftButton, DraftKind, Spin, card, menu_item, setting_row, toggle};

/// The running OmpDeck's version.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CheckState {
    #[default]
    Unknown,
    Checking,
    UpToDate,
    Available(Release),
    Error(String),
}

/// Where an install stands.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Idle,
    Downloading {
        done: u64,
        total: Option<u64>,
    },
    /// Downloaded and verified; unpacking / replacing / waiting for the package manager.
    Applying,
    Failed(String),
    /// The gateway could not install automatically: the user runs `command` (the download is kept).
    NeedsManual {
        command: String,
    },
    /// Installed; the app is closing and the new version starts.
    Restarting,
}

impl Phase {
    /// A download or install is in flight (a second one must not start).
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            Phase::Downloading { .. } | Phase::Applying | Phase::Restarting
        )
    }
}

/// A check this window asked for, finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Checked {
    Available(Release),
    UpToDate,
    Error(String),
}

/// What taking the gateway's view produced for the window to act on.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum Adopted {
    #[default]
    Nothing,
    /// The check this window asked for finished.
    Checked(Checked),
    /// The install this window started is in place: restart into it.
    Applied(Applied),
}

#[derive(Debug, Default)]
pub struct AppUpdateTracker {
    pub check: CheckState,
    /// Last finished check: the pill stays while a re-check runs or fails.
    settled: CheckState,
    /// This window asked for a check and waits for the gateway to finish one newer than this
    /// `checked_unix_ms`.
    awaiting: Option<Option<u64>>,
    pub phase: Phase,
    /// This window runs the install (download, then apply): the gateway's download/apply phases
    /// belong to it.
    pub installing: bool,
    /// How this copy was installed; `None` until the gateway reported.
    pub kind: Option<InstallKind>,
    /// The gateway's last check time, for [`AppUpdateTracker::ask`].
    last_checked: Option<u64>,
}

impl AppUpdateTracker {
    pub fn is_checking(&self) -> bool {
        self.check == CheckState::Checking || self.awaiting.is_some()
    }

    /// This window asks for a check (the caller sends the request); the outcome is reported by
    /// [`AppUpdateTracker::adopt`].
    pub fn ask(&mut self) {
        self.awaiting = Some(self.last_checked);
        // An earlier install failure no longer describes the state of things.
        if matches!(self.phase, Phase::Failed(_)) {
            self.phase = Phase::Idle;
        }
    }

    /// The gateway refused the check: nothing is awaited.
    pub fn abandon_ask(&mut self) {
        self.awaiting = None;
    }

    /// Takes the gateway's view.
    pub fn adopt(&mut self, v: &AppUpdateView) -> Adopted {
        self.kind = Some(v.install.clone());
        match &v.phase {
            UpdatePhase::Checking => {
                self.check = CheckState::Checking;
                Adopted::Nothing
            }
            UpdatePhase::Downloading { done, total } => {
                if self.installing {
                    self.phase = Phase::Downloading {
                        done: *done,
                        total: *total,
                    };
                }
                Adopted::Nothing
            }
            UpdatePhase::Applied(applied) => {
                self.settle(v);
                if self.installing {
                    self.installing = false;
                    self.phase = Phase::Restarting;
                    Adopted::Applied(applied.clone())
                } else {
                    Adopted::Nothing
                }
            }
            UpdatePhase::NeedsManual { command } => {
                if self.installing {
                    self.installing = false;
                    self.phase = Phase::NeedsManual {
                        command: command.clone(),
                    };
                }
                Adopted::Nothing
            }
            UpdatePhase::Failed(e) if self.installing => {
                self.installing = false;
                self.phase = Phase::Failed(e.clone());
                Adopted::Nothing
            }
            UpdatePhase::Failed(e) => {
                // A failed check says nothing about the release already known.
                self.check = CheckState::Error(e.clone());
                match self.awaiting.take() {
                    Some(_) => Adopted::Checked(Checked::Error(e.clone())),
                    None => Adopted::Nothing,
                }
            }
            UpdatePhase::Idle => {
                self.settle(v);
                let done = self
                    .awaiting
                    .is_some_and(|since| v.checked_unix_ms != since);
                if !done {
                    return Adopted::Nothing;
                }
                self.awaiting = None;
                Adopted::Checked(match &self.settled {
                    CheckState::Available(r) => Checked::Available(r.clone()),
                    _ => Checked::UpToDate,
                })
            }
        }
    }

    /// Stores the check outcome the view carries (a release that is not newer than this build
    /// counts as up to date).
    fn settle(&mut self, v: &AppUpdateView) {
        self.last_checked = v.checked_unix_ms;
        let state = match &v.latest {
            Some(r) if selfupdate::is_newer(&r.version, CURRENT) => {
                CheckState::Available(r.clone())
            }
            _ if v.checked_unix_ms.is_some() => CheckState::UpToDate,
            _ => CheckState::Unknown,
        };
        self.settled = state.clone();
        self.check = state;
    }

    /// The newer release, if one is known (skipped or not).
    pub fn available(&self) -> Option<&Release> {
        match &self.settled {
            CheckState::Available(r) => Some(r),
            _ => None,
        }
    }

    /// The release to announce: known, and not the one the user skipped.
    pub fn offered(&self, skipped: Option<&str>) -> Option<&Release> {
        self.available()
            .filter(|r| skipped != Some(r.version.as_str()))
    }
}

/// One-line status for Settings.
pub fn status_line(state: &CheckState) -> String {
    match state {
        CheckState::Unknown => "Not checked yet".to_owned(),
        CheckState::Checking => "Checking for updates…".to_owned(),
        CheckState::UpToDate => "Up to date".to_owned(),
        CheckState::Available(r) => format!("Version {} is available", r.version),
        CheckState::Error(e) => format!("Check failed: {e}"),
    }
}

/// Percent done of a download, when the size is known.
pub fn percent(done: u64, total: Option<u64>) -> Option<u32> {
    let total = total.filter(|&t| t > 0)?;
    Some(u32::try_from((done.min(total) * 100) / total).unwrap_or(100))
}

// ------------------------------------------------------------------ restart

/// What to start after this process exited (set when an update was installed).
static RESTART: Mutex<Option<(PathBuf, Vec<OsString>)>> = Mutex::new(None);

/// Remembers the program to start once the app has closed.
pub fn set_restart(plan: (PathBuf, Vec<OsString>)) {
    *RESTART
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(plan);
}

/// Starts the program set by [`set_restart`], detached from this process. Called by `main` after
/// the state is flushed and the terminals are detached, so the new instance finds both on disk.
pub fn spawn_successor() {
    let plan = RESTART
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let Some((exe, args)) = plan else {
        return;
    };
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    if let Err(e) = cmd.spawn() {
        log::error!("could not restart {}: {e}", exe.display());
        eprintln!("omp-deck: could not restart {}: {e}", exe.display());
    }
}

// ------------------------------------------------------------------ title-bar pill

/// What the pill and its popover need.
pub struct PillView<'a> {
    pub release: &'a Release,
    pub phase: &'a Phase,
    /// `None` while the install method is still being detected.
    pub kind: Option<&'a InstallKind>,
}

/// What the popover asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateAction {
    Install,
    Cancel,
    OpenPage(String),
    Skip(String),
    Check,
    CopyCommand(String),
}

impl PillView<'_> {
    pub fn label(&self) -> String {
        match self.phase {
            Phase::Downloading { done, total } => match percent(*done, *total) {
                Some(p) => format!("Downloading {p}%"),
                None => "Downloading…".to_owned(),
            },
            Phase::Applying => "Installing…".to_owned(),
            Phase::Restarting => "Restarting…".to_owned(),
            Phase::Failed(_) => "Update failed".to_owned(),
            Phase::NeedsManual { .. } => "Finish the update manually".to_owned(),
            Phase::Idle => format!("OmpDeck {}", self.release.version),
        }
    }
}

fn is_package(kind: &InstallKind) -> bool {
    matches!(kind, InstallKind::Deb | InstallKind::Rpm)
}

/// Why this copy cannot update itself, when it cannot.
pub fn cannot_update(kind: &InstallKind) -> Option<String> {
    match kind {
        InstallKind::Dev => Some(
            "This is a development build running from a cargo target folder. Update it with git and cargo."
                .to_owned(),
        ),
        InstallKind::Unsupported { reason } => Some(format!(
            "OmpDeck cannot replace itself here: {reason}. Download the new version from the release page."
        )),
        _ => None,
    }
}

/// The popover under the OmpDeck pill.
pub fn popover(ui: &mut Ui, v: &PillView<'_>) -> Option<UpdateAction> {
    let pal = Palette::of_ui(ui);
    let mut action = None;
    ui.set_min_width(300.0);
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        ui.spacing_mut().item_spacing.x = 10.0;
        crate::card::icon_tile(ui, 30.0, ICON_CIRCLE_ARROW_UP, Color32::TRANSPARENT, true);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.label(
                theme::strong(format!("OmpDeck {} is available", v.release.version))
                    .size(14.5)
                    .color(pal.text_strong),
            );
            ui.label(
                RichText::new(format!("Installed: {CURRENT}"))
                    .small()
                    .color(pal.weak),
            );
        });
    });
    ui.add_space(6.0);
    let left = ui.min_rect().left();
    crate::card::header_rule(
        ui,
        left,
        left + ui.min_rect().width().max(300.0),
        ui.cursor().top(),
        Color32::TRANSPARENT,
    );
    ui.add_space(4.0);
    match v.phase {
        Phase::Downloading { done, total } => {
            ui.add_space(4.0);
            let frac = percent(*done, *total).map_or(0.0, |p| p as f32 / 100.0);
            ui.add(ProgressBar::new(frac).desired_width(280.0).text(format!(
                "{} / {}",
                mib(*done),
                total.map_or_else(|| "?".to_owned(), mib)
            )));
            ui.add_space(4.0);
            if DraftButton::new("Cancel download")
                .icon(ICON_X)
                .show(ui)
                .clicked()
            {
                action = Some(UpdateAction::Cancel);
            }
        }
        Phase::Applying => {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.add(Spin::new(16.0, pal.accent_b));
                let what = if v.kind.is_some_and(is_package) {
                    "Waiting for the installer (it asks for your password)…"
                } else {
                    "Installing…"
                };
                ui.label(RichText::new(what).color(pal.weak));
            });
        }
        Phase::Restarting => {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.add(Spin::new(16.0, pal.accent_b));
                ui.label(RichText::new("Restarting…").color(pal.weak));
            });
        }
        Phase::Idle | Phase::Failed(_) | Phase::NeedsManual { .. } => {
            if let Phase::NeedsManual { command } = v.phase {
                ui.add_space(4.0);
                ui.add(
                    egui::Label::new(
                        RichText::new("Run this to finish the update:")
                            .small()
                            .color(pal.weak),
                    )
                    .wrap(),
                );
                ui.add(
                    egui::Label::new(RichText::new(command).monospace().small())
                        .selectable(true)
                        .wrap(),
                );
                if DraftButton::new("Copy command")
                    .icon(ICON_COPY)
                    .show(ui)
                    .clicked()
                {
                    action = Some(UpdateAction::CopyCommand(command.clone()));
                }
            }
            if let Phase::Failed(e) = v.phase {
                ui.add_space(4.0);
                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                    ui.colored_label(pal.danger, RichText::new(e).small());
                });
            }
            match v.kind {
                None => {
                    ui.label(
                        RichText::new("Checking how OmpDeck was installed…")
                            .small()
                            .color(pal.weak),
                    );
                }
                Some(kind) => match cannot_update(kind) {
                    Some(why) => {
                        ui.add_space(4.0);
                        ui.add(egui::Label::new(RichText::new(why).small().color(pal.weak)).wrap());
                    }
                    None => {
                        let (icon, label) = if is_package(kind) {
                            (ICON_DOWNLOAD, "Install (asks for your password)")
                        } else {
                            (ICON_CIRCLE_ARROW_UP, "Update and restart")
                        };
                        if DraftButton::new(label)
                            .kind(DraftKind::Primary)
                            .icon(icon)
                            .show(ui)
                            .clicked()
                        {
                            action = Some(UpdateAction::Install);
                        }
                    }
                },
            }
            page_item(ui, v, &mut action);
            ui.separator();
            if menu_item(ui, ICON_X, "Skip this version", None, true, false).clicked() {
                action = Some(UpdateAction::Skip(v.release.version.clone()));
                ui.close();
            }
        }
    }
    action
}

fn page_item(ui: &mut Ui, v: &PillView<'_>, action: &mut Option<UpdateAction>) {
    if menu_item(ui, ICON_EXTERNAL_LINK, "Release page", None, true, false).clicked() {
        *action = Some(UpdateAction::OpenPage(v.release.page_url.clone()));
    }
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

// ------------------------------------------------------------------ settings

pub struct SettingsCard<'a> {
    pub check: bool,
    pub state: &'a CheckState,
    /// The newest known release, which a later failed check does not erase.
    pub available: Option<&'a Release>,
    pub phase: &'a Phase,
    pub kind: Option<&'a InstallKind>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SettingsAction {
    SetCheck(bool),
    CheckNow,
    Install,
    Cancel,
}

/// The status text under the version in Settings.
fn card_line(c: &SettingsCard<'_>) -> String {
    match c.phase {
        Phase::Downloading { done, total } => match percent(*done, *total) {
            Some(p) => format!("Downloading… {p}%"),
            None => "Downloading…".to_owned(),
        },
        Phase::Applying => "Installing…".to_owned(),
        Phase::Restarting => "Restarting…".to_owned(),
        _ => {
            // A development build never checks; an unsupported install says why until a check
            // has said something else.
            let why = c.kind.and_then(cannot_update);
            match (c.kind, why) {
                (Some(InstallKind::Dev), Some(why)) => why,
                (_, Some(why)) if matches!(c.state, CheckState::Unknown) => why,
                _ => status_line(c.state),
            }
        }
    }
}

/// "OmpDeck" card of the Settings page: version, automatic checks, check now.
pub fn settings_card(ui: &mut Ui, c: &SettingsCard<'_>) -> Vec<SettingsAction> {
    let pal = Palette::of_ui(ui);
    let mut out = Vec::new();
    card(ui, "OmpDeck updates", |ui| {
        setting_row(
            ui,
            "Check for OmpDeck updates automatically",
            "Looks for a new release at startup and every 6 hours. Nothing is installed until you choose to.",
            |ui| {
                let mut on = c.check;
                if toggle(ui, &mut on).changed() {
                    out.push(SettingsAction::SetCheck(on));
                }
            },
        );
        ui.add_space(4.0);
        let line = card_line(c);
        setting_row(ui, &format!("OmpDeck {CURRENT}"), &line, |ui| {
            if matches!(c.phase, Phase::Downloading { .. }) {
                ui.add(Spin::new(18.0, pal.accent_b));
                if DraftButton::new("Cancel")
                    .icon(ICON_X)
                    .min_width(88.0)
                    .show(ui)
                    .clicked()
                {
                    out.push(SettingsAction::Cancel);
                }
                return;
            }
            if matches!(c.state, CheckState::Checking) || c.phase.is_busy() {
                ui.add(Spin::new(18.0, pal.accent_b));
                return;
            }
            if let Some(r) = c.available
                && c.kind.is_some_and(|k| cannot_update(k).is_none())
                && DraftButton::new(&format!("Update to {}", r.version))
                    .kind(DraftKind::Primary)
                    .icon(ICON_CIRCLE_ARROW_UP)
                    .show(ui)
                    .clicked()
            {
                out.push(SettingsAction::Install);
            }
            if DraftButton::new("Check now")
                .icon(ICON_REFRESH_CW)
                .min_width(104.0)
                .show(ui)
                .clicked()
            {
                out.push(SettingsAction::CheckNow);
            }
        });
        if let Phase::Failed(e) = c.phase {
            ui.colored_label(pal.danger, RichText::new(e).small());
        }
        if let Phase::NeedsManual { command } = c.phase {
            ui.label(RichText::new("Run this to finish the update:").small());
            ui.add(
                egui::Label::new(RichText::new(command).monospace().small())
                    .selectable(true)
                    .wrap(),
            );
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(v: &str) -> Release {
        Release {
            version: v.to_owned(),
            tag: format!("v{v}"),
            page_url: format!("https://example.test/releases/tag/v{v}"),
        }
    }

    fn view(latest: Option<Release>, checked: Option<u64>, phase: UpdatePhase) -> AppUpdateView {
        AppUpdateView {
            install: InstallKind::Deb,
            current: CURRENT.to_owned(),
            latest,
            asset: None,
            checked_unix_ms: checked,
            phase,
        }
    }

    fn idle(latest: Option<Release>, checked: Option<u64>) -> AppUpdateView {
        view(latest, checked, UpdatePhase::Idle)
    }

    fn newer() -> Release {
        release("999.0.0")
    }

    #[test]
    fn only_a_newer_release_is_available() {
        let mut t = AppUpdateTracker::default();
        t.adopt(&idle(Some(newer()), Some(1)));
        assert_eq!(t.available().map(|r| r.version.as_str()), Some("999.0.0"));
        t.adopt(&idle(Some(release(CURRENT)), Some(2)));
        assert!(t.available().is_none());
        assert_eq!(t.check, CheckState::UpToDate);
        t.adopt(&idle(Some(release("0.0.1")), Some(3)));
        assert!(t.available().is_none(), "a downgrade is never offered");
        t.adopt(&idle(None, None));
        assert_eq!(t.check, CheckState::Unknown);
    }

    #[test]
    fn skipping_hides_that_version_but_not_a_newer_one() {
        let mut t = AppUpdateTracker::default();
        t.adopt(&idle(Some(newer()), Some(1)));
        assert!(t.offered(None).is_some());
        assert!(t.offered(Some("999.0.0")).is_none());
        assert!(t.offered(Some("998.0.0")).is_some());
        t.adopt(&idle(Some(release("999.0.1")), Some(2)));
        assert!(t.offered(Some("999.0.0")).is_some());
    }

    #[test]
    fn a_known_release_survives_a_recheck_and_a_failed_check() {
        let mut t = AppUpdateTracker::default();
        t.adopt(&idle(Some(newer()), Some(1)));
        t.ask();
        assert!(t.is_checking());
        t.adopt(&view(Some(newer()), Some(1), UpdatePhase::Checking));
        assert!(t.available().is_some(), "the pill stays while re-checking");
        let done = t.adopt(&view(
            Some(newer()),
            Some(1),
            UpdatePhase::Failed("offline".into()),
        ));
        assert_eq!(done, Adopted::Checked(Checked::Error("offline".into())));
        assert!(
            t.available().is_some(),
            "an offline re-check keeps the release"
        );
        assert_eq!(status_line(&t.check), "Check failed: offline");
        t.adopt(&idle(None, Some(5)));
        assert!(t.available().is_none(), "a fresh answer replaces it");
    }

    #[test]
    fn only_the_check_this_window_asked_for_is_reported() {
        let mut t = AppUpdateTracker::default();
        assert_eq!(t.adopt(&idle(Some(newer()), Some(1))), Adopted::Nothing);
        t.ask();
        assert_eq!(
            t.adopt(&idle(Some(newer()), Some(1))),
            Adopted::Nothing,
            "the old result is not the answer"
        );
        assert_eq!(
            t.adopt(&idle(None, Some(2))),
            Adopted::Checked(Checked::UpToDate)
        );
        assert_eq!(t.adopt(&idle(None, Some(3))), Adopted::Nothing);
    }

    #[test]
    fn asking_clears_a_previous_install_failure() {
        let mut t = AppUpdateTracker {
            phase: Phase::Failed("no asset".into()),
            ..AppUpdateTracker::default()
        };
        t.ask();
        assert_eq!(t.phase, Phase::Idle);
    }

    #[test]
    fn the_install_this_window_started_follows_the_gateway() {
        let mut t = AppUpdateTracker {
            installing: true,
            ..AppUpdateTracker::default()
        };
        t.adopt(&view(
            Some(newer()),
            Some(1),
            UpdatePhase::Downloading {
                done: 1,
                total: Some(4),
            },
        ));
        assert_eq!(
            t.phase,
            Phase::Downloading {
                done: 1,
                total: Some(4)
            }
        );
        let applied = Applied {
            executable: PathBuf::from("/opt/omp-deck"),
        };
        let out = t.adopt(&view(
            Some(newer()),
            Some(1),
            UpdatePhase::Applied(applied.clone()),
        ));
        assert_eq!(out, Adopted::Applied(applied));
        assert_eq!(t.phase, Phase::Restarting);
        assert!(!t.installing);
    }

    #[test]
    fn another_clients_install_is_not_this_windows_to_restart() {
        let mut t = AppUpdateTracker::default();
        let applied = Applied {
            executable: PathBuf::from("/opt/omp-deck"),
        };
        let out = t.adopt(&view(None, Some(1), UpdatePhase::Applied(applied)));
        assert_eq!(out, Adopted::Nothing);
        assert_eq!(t.phase, Phase::Idle);
    }

    #[test]
    fn a_failed_install_this_window_started_is_an_install_failure() {
        let mut t = AppUpdateTracker {
            installing: true,
            ..AppUpdateTracker::default()
        };
        t.adopt(&view(None, Some(1), UpdatePhase::Failed("no asset".into())));
        assert_eq!(t.phase, Phase::Failed("no asset".into()));
        assert!(!t.installing);
    }

    #[test]
    fn settings_explains_dev_and_unsupported_instead_of_not_checked_yet() {
        let card = |kind: &InstallKind, state: &CheckState| {
            card_line(&SettingsCard {
                check: true,
                state,
                available: None,
                phase: &Phase::Idle,
                kind: Some(kind),
            })
        };
        let dev = card(&InstallKind::Dev, &CheckState::Unknown);
        assert!(dev.contains("development build"), "{dev}");
        let un = InstallKind::Unsupported {
            reason: "read-only folder".into(),
        };
        assert!(card(&un, &CheckState::Unknown).contains("read-only folder"));
        assert_eq!(card(&un, &CheckState::UpToDate), "Up to date");
        assert_eq!(
            card(&InstallKind::Deb, &CheckState::Unknown),
            "Not checked yet"
        );
    }

    #[test]
    fn download_percent_is_clamped_and_needs_a_size() {
        assert_eq!(percent(50, Some(200)), Some(25));
        assert_eq!(percent(300, Some(200)), Some(100));
        assert_eq!(percent(5, None), None);
        assert_eq!(percent(5, Some(0)), None);
    }

    #[test]
    fn busy_phases_block_a_second_install() {
        assert!(!Phase::Idle.is_busy());
        assert!(!Phase::Failed("x".into()).is_busy());
        assert!(
            Phase::Downloading {
                done: 0,
                total: None
            }
            .is_busy()
        );
        assert!(Phase::Applying.is_busy());
        assert!(Phase::Restarting.is_busy());
    }

    #[test]
    fn dev_and_unsupported_installs_explain_instead_of_updating() {
        assert!(cannot_update(&InstallKind::Dev).is_some());
        let why = cannot_update(&InstallKind::Unsupported {
            reason: "read-only folder".into(),
        })
        .unwrap_or_default();
        assert!(why.contains("read-only folder"));
        assert!(cannot_update(&InstallKind::Deb).is_none());
        assert!(cannot_update(&InstallKind::Rpm).is_none());
    }

    #[test]
    fn the_pill_label_follows_the_install_phase() {
        let r = release("0.9.0");
        fn view<'a>(release: &'a Release, phase: &'a Phase) -> PillView<'a> {
            PillView {
                release,
                phase,
                kind: None,
            }
        }
        assert_eq!(view(&r, &Phase::Idle).label(), "OmpDeck 0.9.0");
        let downloading = Phase::Downloading {
            done: 1,
            total: Some(4),
        };
        assert_eq!(view(&r, &downloading).label(), "Downloading 25%");
        assert_eq!(view(&r, &Phase::Applying).label(), "Installing…");
        assert_eq!(
            view(&r, &Phase::Failed("x".into())).label(),
            "Update failed"
        );
    }
}
