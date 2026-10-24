//! OmpDeck self-update: the check tracker (independent of omp), the install phase, the title-bar
//! pill + popover and the Settings card. Pure state and painting; the workers live in
//! `app/app_update.rs`.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use deck_core::selfupdate::{self, InstallKind, Release};
use egui::{ProgressBar, RichText, Ui};

use crate::icons::{
    ICON_CIRCLE_ARROW_UP, ICON_COPY, ICON_DOWNLOAD, ICON_EXTERNAL_LINK, ICON_REFRESH_CW, ICON_X,
};
use crate::theme::{self, Palette};
use crate::widgets::{Spin, card, icon_text, menu_item, setting_row, toggle};

/// Background re-check cadence (plus one check at startup).
pub const RECHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// The running OmpDeck's version.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Download folders left by a killed run are removed once this old.
pub const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// Best effort: removes `omp-deck-update-*` folders of `tmp` not modified for `max_age` (a
/// running download keeps its folder fresh).
pub fn sweep_stale_update_dirs(tmp: &std::path::Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("omp-deck-update-") {
            continue;
        }
        let stale = entry
            .metadata()
            .ok()
            .filter(|m| m.is_dir())
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= max_age);
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

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
    /// Could not install automatically: run this command (pkexec missing or declined).
    NeedsManual {
        command: String,
    },
    Failed(String),
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

#[derive(Debug, Default)]
pub struct AppUpdateTracker {
    pub check: CheckState,
    /// Last finished check: the pill stays while a re-check runs.
    settled: CheckState,
    last_started: Option<Instant>,
    /// The running check was asked for by the user (report its outcome with a toast).
    pub manual: bool,
    pub phase: Phase,
    /// How this copy was installed; `None` until detection finished.
    pub kind: Option<InstallKind>,
}

impl AppUpdateTracker {
    pub fn is_checking(&self) -> bool {
        self.check == CheckState::Checking
    }

    /// A scheduled check is due: automatic checks are on, none runs, and none ran in the last 6 h.
    pub fn due(&self, now: Instant, enabled: bool) -> bool {
        enabled
            && !self.is_checking()
            && self
                .last_started
                .is_none_or(|t| now.saturating_duration_since(t) >= RECHECK_EVERY)
    }

    pub fn begin(&mut self, now: Instant, manual: bool) {
        self.check = CheckState::Checking;
        self.last_started = Some(now);
        self.manual = manual;
        // An earlier install failure no longer describes the state of things.
        if matches!(self.phase, Phase::Failed(_)) {
            self.phase = Phase::Idle;
        }
    }

    /// Takes a finished check; a release that is not newer than `current` counts as up to date.
    pub fn finish(&mut self, result: Result<Release, String>, current: &str) {
        let state = match result {
            Ok(r) if selfupdate::is_newer(&r.version, current) => CheckState::Available(r),
            Ok(_) => CheckState::UpToDate,
            Err(e) => CheckState::Error(e),
        };
        // A failed check says nothing about the release already known: keep offering it.
        if !matches!(state, CheckState::Error(_)) {
            self.settled = state.clone();
        }
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
    CopyCommand(String),
    Skip(String),
    Check,
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
            Phase::Idle | Phase::NeedsManual { .. } => format!("OmpDeck {}", self.release.version),
        }
    }

    pub fn tip(&self) -> String {
        format!(
            "OmpDeck {} is available (you have {CURRENT})",
            self.release.version
        )
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
        ui.add_space(10.0);
        ui.vertical(|ui| {
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
    ui.add_space(2.0);
    ui.separator();
    match v.phase {
        Phase::Downloading { done, total } => {
            ui.add_space(4.0);
            let frac = percent(*done, *total).map_or(0.0, |p| p as f32 / 100.0);
            ui.add(ProgressBar::new(frac).desired_width(280.0).text(format!(
                "{} / {}",
                mib(*done),
                total.map_or_else(|| "?".to_owned(), mib)
            )));
            if menu_item(ui, ICON_X, "Cancel download", None, true, false).clicked() {
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
        Phase::NeedsManual { command } => {
            ui.add_space(4.0);
            ui.label(
                RichText::new("OmpDeck could not install itself. Run this in a terminal:")
                    .small()
                    .color(pal.weak),
            );
            ui.add(
                egui::Label::new(RichText::new(command).monospace().color(pal.text))
                    .wrap()
                    .selectable(true),
            );
            if menu_item(ui, ICON_COPY, "Copy command", None, true, false).clicked() {
                action = Some(UpdateAction::CopyCommand(command.clone()));
            }
            page_item(ui, v, &mut action);
        }
        Phase::Idle | Phase::Failed(_) => {
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
                        if menu_item(ui, icon, label, None, true, false).clicked() {
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
                if ui
                    .add(
                        egui::Button::new((
                            icon_text(ICON_X, 16.0, pal.glyph),
                            RichText::new("Cancel"),
                        ))
                        .min_size(egui::vec2(0.0, 30.0)),
                    )
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
                && ui
                    .add(
                        egui::Button::new((
                            icon_text(ICON_CIRCLE_ARROW_UP, 16.0, pal.glyph),
                            RichText::new(format!("Update to {}", r.version)),
                        ))
                        .min_size(egui::vec2(0.0, 30.0)),
                    )
                    .clicked()
            {
                out.push(SettingsAction::Install);
            }
            if ui
                .add(
                    egui::Button::new((
                        icon_text(ICON_REFRESH_CW, 16.0, pal.glyph),
                        RichText::new("Check now"),
                    ))
                    .min_size(egui::vec2(0.0, 30.0)),
                )
                .clicked()
            {
                out.push(SettingsAction::CheckNow);
            }
        });
        if let Phase::Failed(e) = c.phase {
            ui.colored_label(pal.danger, RichText::new(e).small());
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

    #[test]
    fn a_check_is_due_at_startup_then_every_six_hours_and_only_when_enabled() {
        let t0 = Instant::now();
        let mut t = AppUpdateTracker::default();
        assert!(t.due(t0, true));
        assert!(!t.due(t0, false), "automatic checks off");
        t.begin(t0, false);
        assert!(
            !t.due(t0 + RECHECK_EVERY * 2, true),
            "one is already running"
        );
        t.finish(Ok(release("0.0.1")), "0.0.1");
        assert!(!t.due(t0 + RECHECK_EVERY - Duration::from_secs(1), true));
        assert!(t.due(t0 + RECHECK_EVERY, true));
    }

    #[test]
    fn only_a_newer_release_is_available() {
        let mut t = AppUpdateTracker::default();
        t.finish(Ok(release("0.9.0")), "0.8.0");
        assert_eq!(t.available().map(|r| r.version.as_str()), Some("0.9.0"));
        t.finish(Ok(release("0.8.0")), "0.8.0");
        assert!(t.available().is_none());
        assert_eq!(t.check, CheckState::UpToDate);
        t.finish(Ok(release("0.7.0")), "0.8.0");
        assert!(t.available().is_none(), "a downgrade is never offered");
        t.finish(Ok(release("0.8.0-rc.1")), "0.8.0");
        assert!(
            t.available().is_none(),
            "a release candidate is not newer than its release"
        );
    }

    #[test]
    fn skipping_hides_that_version_but_not_a_newer_one() {
        let mut t = AppUpdateTracker::default();
        t.finish(Ok(release("0.9.0")), "0.8.0");
        assert!(t.offered(None).is_some());
        assert!(t.offered(Some("0.9.0")).is_none());
        assert!(t.offered(Some("0.8.5")).is_some());
        t.finish(Ok(release("0.9.1")), "0.8.0");
        assert!(t.offered(Some("0.9.0")).is_some());
        // The skipped release is still known (Settings and manual checks can show it).
        t.finish(Ok(release("0.9.0")), "0.8.0");
        assert_eq!(t.available().map(|r| r.version.as_str()), Some("0.9.0"));
    }

    #[test]
    fn a_known_release_survives_a_recheck_and_a_failed_check() {
        let mut t = AppUpdateTracker::default();
        t.finish(Ok(release("0.9.0")), "0.8.0");
        t.begin(Instant::now(), true);
        assert!(t.is_checking());
        assert!(t.available().is_some(), "the pill stays while re-checking");
        t.finish(Err("offline".into()), "0.8.0");
        assert_eq!(
            t.available().map(|r| r.version.as_str()),
            Some("0.9.0"),
            "an offline re-check does not un-know the release"
        );
        assert_eq!(status_line(&t.check), "Check failed: offline");
        t.finish(Ok(release("0.8.0")), "0.8.0");
        assert!(t.available().is_none(), "a fresh answer replaces it");
    }

    #[test]
    fn starting_a_check_clears_a_previous_install_failure() {
        let mut t = AppUpdateTracker {
            phase: Phase::Failed("no asset".into()),
            ..AppUpdateTracker::default()
        };
        t.begin(Instant::now(), true);
        assert_eq!(t.phase, Phase::Idle);
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
    fn stale_update_dirs_are_swept_but_fresh_and_foreign_ones_stay() {
        let tmp = tempfile::tempdir().unwrap();
        let mk = |name: &str, age_days: u64| {
            let p = tmp.path().join(name);
            std::fs::create_dir(&p).unwrap();
            std::fs::write(p.join("x.part"), b"1").unwrap();
            let when = std::time::SystemTime::now() - Duration::from_secs(age_days * 86_400);
            std::fs::File::open(&p).unwrap().set_modified(when).unwrap();
            p
        };
        let old = mk("omp-deck-update-OLD", 3);
        let fresh = mk("omp-deck-update-NEW", 0);
        let foreign = mk("other-old", 3);
        sweep_stale_update_dirs(tmp.path(), STALE_AFTER);
        assert!(!old.exists());
        assert!(fresh.exists() && foreign.exists());
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
            !Phase::NeedsManual {
                command: "x".into()
            }
            .is_busy()
        );
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
