//! omp update tracking: the check state machine as the window shows it, and channel inference.
//! Pure data; the gateway runs the `omp update --check` and its 6 h schedule (`Topic::OmpUpdate`).

use std::collections::VecDeque;

use deck_core::omp::{UpdateKind, UpdateStatus};
use deck_service::gateway::protocol::OmpUpdateView;

/// Where OmpDeck looks for omp, for messages ("not found on …").
pub const OMP_LOOKUP: &str = if cfg!(windows) {
    "PATH or %LOCALAPPDATA%\\omp"
} else {
    "PATH or ~/.local/bin"
};

/// Where omp's official installer puts the binary.
pub const OMP_INSTALL_DIR: &str = if cfg!(windows) {
    "%LOCALAPPDATA%\\omp"
} else {
    "~/.local/bin"
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum UpdateState {
    #[default]
    Unknown,
    Checking,
    UpToDate {
        current: String,
    },
    Available {
        current: String,
        latest: String,
    },
    Error(String),
}

impl UpdateState {
    pub fn from_result(result: Result<UpdateStatus, String>) -> UpdateState {
        match result {
            Ok(UpdateStatus {
                current,
                latest: Some(latest),
            }) => UpdateState::Available { current, latest },
            Ok(UpdateStatus {
                current,
                latest: None,
            }) => UpdateState::UpToDate { current },
            Err(e) => UpdateState::Error(e),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Canary,
}

impl Channel {
    /// omp prints no channel in a structured way; canary builds carry "canary" in their version.
    pub fn of_version(version: &str) -> Channel {
        if version.to_ascii_lowercase().contains("canary") {
            Channel::Canary
        } else {
            Channel::Stable
        }
    }

    /// The channel switch offered from this channel.
    pub fn switch_kind(self) -> UpdateKind {
        match self {
            Channel::Stable => UpdateKind::Canary,
            Channel::Canary => UpdateKind::Stable,
        }
    }
}

/// Tab title for an update process.
pub fn tab_title(kind: UpdateKind) -> &'static str {
    match kind {
        UpdateKind::Omp => "omp update",
        UpdateKind::Plugins => "omp update plugins",
        UpdateKind::Canary => "omp → canary",
        UpdateKind::Stable => "omp → stable",
    }
}

/// One-line status for settings / menus.
pub fn status_line(state: &UpdateState) -> String {
    match state {
        UpdateState::Unknown => "Not checked yet".to_owned(),
        UpdateState::Checking => "Checking for updates…".to_owned(),
        UpdateState::UpToDate { .. } => "Up to date".to_owned(),
        UpdateState::Available { current, latest } => {
            format!("Update available: {current} → {latest}")
        }
        UpdateState::Error(e) => format!("Check failed: {e}"),
    }
}

/// A finished check, for the toast that reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct Finished {
    pub result: Result<UpdateStatus, String>,
    /// The update this check follows (its outcome is announced).
    pub announce: Option<UpdateKind>,
    /// The user asked for this check.
    pub manual: bool,
}

/// The window's picture of omp's version and updates. The gateway runs the checks and the
/// 6 h schedule; this adopts its view and remembers why this window asked for a check.
#[derive(Debug, Default)]
pub struct UpdateTracker {
    /// What the UI shows (includes `Checking`).
    pub state: UpdateState,
    /// Last finished state; the badge keeps showing "available" while a re-check runs.
    settled: UpdateState,
    /// Installed omp version (from `--version` or a check).
    pub version: Option<String>,
    /// This window asked for the check now running; its outcome is reported.
    waiting: Option<Asked>,
    /// Re-checks owed after finished update tabs (`Some(kind)` = announce its outcome), run one
    /// at a time once no check is in flight.
    pub recheck: VecDeque<Option<UpdateKind>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Asked {
    announce: Option<UpdateKind>,
    manual: bool,
}

impl UpdateTracker {
    pub fn is_checking(&self) -> bool {
        self.state == UpdateState::Checking
    }

    /// A check this window requested is running (the caller sends the request).
    pub fn begin(&mut self, announce: Option<UpdateKind>) {
        self.state = UpdateState::Checking;
        self.waiting = Some(Asked {
            announce,
            manual: announce.is_none(),
        });
    }

    /// The request for the check was refused or lost: back to the last settled state.
    pub fn abandon(&mut self) {
        self.state = self.settled.clone();
        self.waiting = None;
    }

    /// The next owed post-update check, when none is running: it owns the announcement.
    pub fn next_queued(&mut self) -> Option<Option<UpdateKind>> {
        if self.is_checking() {
            return None;
        }
        let announce = self.recheck.pop_front()?;
        self.state = UpdateState::Checking;
        self.waiting = Some(Asked {
            announce,
            manual: false,
        });
        Some(announce)
    }

    /// Takes the gateway's view. Returns the check this window asked for when it just finished.
    pub fn adopt(&mut self, view: &OmpUpdateView) -> Option<Finished> {
        if let Some(v) = &view.version {
            self.version = Some(v.clone());
        }
        if view.checking {
            self.state = UpdateState::Checking;
            return None;
        }
        let result = view.status.clone()?;
        if let Ok(s) = &result {
            self.version = Some(s.current.clone());
        }
        self.settled = UpdateState::from_result(result.clone());
        self.state = self.settled.clone();
        let asked = self.waiting.take()?;
        Some(Finished {
            result,
            announce: asked.announce,
            manual: asked.manual,
        })
    }

    /// `(current, latest)` when an update is known.
    pub fn available(&self) -> Option<(&str, &str)> {
        match &self.settled {
            UpdateState::Available { current, latest } => Some((current, latest)),
            _ => None,
        }
    }

    pub fn channel(&self) -> Channel {
        self.version
            .as_deref()
            .map_or(Channel::Stable, Channel::of_version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(latest: Option<&str>) -> UpdateStatus {
        UpdateStatus {
            current: "18.12.3".into(),
            latest: latest.map(Into::into),
        }
    }

    fn view(status: Option<Result<UpdateStatus, String>>, checking: bool) -> OmpUpdateView {
        OmpUpdateView {
            version: None,
            status,
            checked_unix_ms: None,
            checking,
        }
    }

    #[test]
    fn states_from_results() {
        assert_eq!(
            UpdateState::from_result(Ok(status(Some("18.12.5")))),
            UpdateState::Available {
                current: "18.12.3".into(),
                latest: "18.12.5".into()
            }
        );
        assert_eq!(
            UpdateState::from_result(Ok(status(None))),
            UpdateState::UpToDate {
                current: "18.12.3".into()
            }
        );
        assert_eq!(
            UpdateState::from_result(Err("offline".into())),
            UpdateState::Error("offline".into())
        );
    }

    #[test]
    fn badge_survives_a_recheck() {
        let mut tr = UpdateTracker::default();
        tr.adopt(&view(Some(Ok(status(Some("18.12.5")))), false));
        assert_eq!(tr.available(), Some(("18.12.3", "18.12.5")));
        tr.adopt(&view(Some(Ok(status(Some("18.12.5")))), true));
        assert!(tr.is_checking());
        assert_eq!(tr.available(), Some(("18.12.3", "18.12.5")));
        tr.adopt(&view(Some(Ok(status(None))), false));
        assert_eq!(tr.available(), None);
    }

    #[test]
    fn a_finished_check_reports_once_with_the_owed_announcement() {
        let mut tr = UpdateTracker::default();
        tr.begin(Some(UpdateKind::Omp));
        assert!(tr.is_checking());
        assert!(tr.adopt(&view(None, true)).is_none(), "still running");
        let done = tr.adopt(&view(Some(Ok(status(None))), false));
        let done = done.expect("the check finished");
        assert_eq!(done.announce, Some(UpdateKind::Omp));
        assert!(!done.manual);
        assert!(tr.adopt(&view(Some(Ok(status(None))), false)).is_none());
    }

    #[test]
    fn queued_rechecks_run_one_at_a_time() {
        let mut tr = UpdateTracker::default();
        tr.recheck.push_back(Some(UpdateKind::Omp));
        tr.recheck.push_back(None);
        assert_eq!(tr.next_queued(), Some(Some(UpdateKind::Omp)));
        assert!(tr.is_checking());
        assert_eq!(tr.next_queued(), None, "never while a check runs");
        tr.adopt(&view(Some(Ok(status(None))), false));
        assert_eq!(tr.next_queued(), Some(None));
        tr.adopt(&view(Some(Ok(status(None))), false));
        assert_eq!(tr.next_queued(), None, "queue drained");
    }

    #[test]
    fn failed_check_keeps_last_version() {
        let mut tr = UpdateTracker::default();
        tr.adopt(&view(Some(Ok(status(None))), false));
        tr.adopt(&view(Some(Err("offline".into())), false));
        assert_eq!(tr.version.as_deref(), Some("18.12.3"));
        assert_eq!(tr.state, UpdateState::Error("offline".into()));
    }

    #[test]
    fn channel_from_version() {
        assert_eq!(Channel::of_version("18.12.3"), Channel::Stable);
        assert_eq!(Channel::of_version("18.12.4-canary.2"), Channel::Canary);
        assert_eq!(Channel::Stable.switch_kind(), UpdateKind::Canary);
        assert_eq!(Channel::Canary.switch_kind(), UpdateKind::Stable);
    }
}
