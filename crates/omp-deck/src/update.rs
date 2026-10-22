//! omp update tracking: the check state machine, the 6 h schedule and channel inference.
//! Pure data; the actual `omp update --check` runs on a worker (see `omp_worker`).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use deck_core::omp::{UpdateKind, UpdateStatus};

/// Background re-check cadence.
pub const RECHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

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

#[derive(Debug, Default)]
pub struct UpdateTracker {
    /// What the UI shows (includes `Checking`).
    pub state: UpdateState,
    /// Last finished state; the badge keeps showing "available" while a re-check runs.
    settled: UpdateState,
    /// Installed omp version (from `--version` or a check).
    pub version: Option<String>,
    last_started: Option<Instant>,
    /// Announce this finished update with the result of the check now running (set when that
    /// check starts, never earlier, so an older in-flight check cannot claim it).
    pub announce: Option<UpdateKind>,
    /// Re-checks owed after finished update tabs (`Some(kind)` = announce its outcome), run one
    /// at a time once no check is in flight.
    pub recheck: VecDeque<Option<UpdateKind>>,
}

impl UpdateTracker {
    pub fn is_checking(&self) -> bool {
        self.state == UpdateState::Checking
    }

    /// A scheduled check is due (never ran, or the last one started ≥ 6 h ago).
    pub fn due(&self, now: Instant) -> bool {
        !self.is_checking()
            && self
                .last_started
                .is_none_or(|t| now.saturating_duration_since(t) >= RECHECK_EVERY)
    }

    pub fn begin(&mut self, now: Instant) {
        self.state = UpdateState::Checking;
        self.last_started = Some(now);
    }

    /// Starts the next owed post-update check when none is running; it owns the announcement.
    pub fn start_queued(&mut self, now: Instant) -> bool {
        if self.is_checking() {
            return false;
        }
        let Some(announce) = self.recheck.pop_front() else {
            return false;
        };
        self.announce = announce;
        self.begin(now);
        true
    }

    pub fn finish(&mut self, result: Result<UpdateStatus, String>) {
        if let Ok(s) = &result {
            self.version = Some(s.current.clone());
        }
        self.settled = UpdateState::from_result(result);
        self.state = self.settled.clone();
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
    fn schedule_is_due_at_start_then_after_six_hours() {
        let t0 = Instant::now();
        let mut tr = UpdateTracker::default();
        assert!(tr.due(t0));
        tr.begin(t0);
        assert!(!tr.due(t0 + RECHECK_EVERY * 2), "never while a check runs");
        tr.finish(Ok(status(None)));
        assert!(!tr.due(t0 + RECHECK_EVERY - Duration::from_secs(1)));
        assert!(tr.due(t0 + RECHECK_EVERY));
    }

    #[test]
    fn badge_survives_a_recheck() {
        let t0 = Instant::now();
        let mut tr = UpdateTracker::default();
        tr.finish(Ok(status(Some("18.12.5"))));
        assert_eq!(tr.available(), Some(("18.12.3", "18.12.5")));
        tr.begin(t0);
        assert!(tr.is_checking());
        assert_eq!(tr.available(), Some(("18.12.3", "18.12.5")));
        tr.finish(Ok(status(None)));
        assert_eq!(tr.available(), None);
    }

    #[test]
    fn queued_recheck_waits_for_the_running_check_and_owns_the_announcement() {
        let t0 = Instant::now();
        let mut tr = UpdateTracker::default();
        tr.begin(t0); // an older check is in flight
        tr.recheck.push_back(Some(UpdateKind::Omp));
        tr.recheck.push_back(None);
        assert!(!tr.start_queued(t0), "never while a check runs");
        assert_eq!(tr.announce, None, "the older check must not claim it");

        tr.finish(Ok(status(None)));
        assert!(tr.start_queued(t0));
        assert!(tr.is_checking());
        assert_eq!(tr.announce, Some(UpdateKind::Omp));

        tr.finish(Ok(status(None)));
        tr.announce = None;
        assert!(tr.start_queued(t0), "the second owed check runs next");
        assert_eq!(tr.announce, None);
        tr.finish(Ok(status(None)));
        assert!(!tr.start_queued(t0), "queue drained");
    }

    #[test]
    fn failed_check_keeps_last_version() {
        let mut tr = UpdateTracker::default();
        tr.finish(Ok(status(None)));
        tr.finish(Err("offline".into()));
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
