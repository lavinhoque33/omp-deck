//! The one-at-a-time backup lane: the job holding it (a request job or automatic work), its
//! latest progress and its cancellation ([`crate::gateway::protocol::Mutation::CancelBackupJob`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::controller::RequestKey;
use crate::gateway::protocol::{LaneJob, Progress};

/// Refusal of a cancel while the lane is free.
pub(super) const NOTHING_RUNNING: &str = "no backup job is running";
/// Refusal of a cancel of a job that cannot stop midway.
pub(super) const NOT_CANCELLABLE: &str = "this job cannot be cancelled";

struct Held {
    job: LaneJob,
    /// Stops the job (honoured only when `job.cancellable`).
    cancel: Arc<AtomicBool>,
    /// The request job's key (`None`: automatic work).
    key: Option<RequestKey>,
}

/// What a cancel stopped.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Cancelled {
    /// A request job: its outcome reports the cancellation.
    Request,
    /// An automatic folder: the rest of its pass is dropped.
    Automatic,
}

#[derive(Default)]
pub(super) struct BackupLane(Option<Held>);

impl BackupLane {
    /// The job holding the lane.
    pub(super) fn job(&self) -> Option<&LaneJob> {
        self.0.as_ref().map(|h| &h.job)
    }

    pub(super) fn is_free(&self) -> bool {
        self.0.is_none()
    }

    /// Puts `job` on the lane (`key`: the request job's; `None`: automatic work). Lane jobs are
    /// only started while it is free.
    pub(super) fn claim(&mut self, job: LaneJob, cancel: Arc<AtomicBool>, key: Option<RequestKey>) {
        self.0 = Some(Held { job, cancel, key });
    }

    /// `key` (`None`: automatic work) holds the lane.
    fn holds(&self, key: Option<&RequestKey>) -> bool {
        self.0.as_ref().is_some_and(|h| h.key.as_ref() == key)
    }

    /// Frees the lane when `key` holds it; whether it did.
    pub(super) fn release(&mut self, key: Option<&RequestKey>) -> bool {
        let held = self.holds(key);
        if held {
            self.0 = None;
        }
        held
    }

    /// Records the progress of `key`'s job when it holds the lane; whether it did.
    pub(super) fn progress(&mut self, key: Option<&RequestKey>, progress: Progress) -> bool {
        match &mut self.0 {
            Some(h) if h.key.as_ref() == key => {
                h.job.progress = Some(progress);
                true
            }
            _ => false,
        }
    }

    /// Why [`BackupLane::cancel`] would stop nothing.
    pub(super) fn cancel_refusal(&self) -> Option<&'static str> {
        match self.job() {
            None => Some(NOTHING_RUNNING),
            Some(j) if !j.cancellable => Some(NOT_CANCELLABLE),
            Some(_) => None,
        }
    }

    /// Asks the job holding the lane to stop (`None`: nothing cancellable holds it).
    pub(super) fn cancel(&mut self) -> Option<Cancelled> {
        let h = self.0.as_mut().filter(|h| h.job.cancellable)?;
        h.cancel.store(true, Ordering::Relaxed);
        h.job.cancelling = true;
        Some(match h.key {
            Some(_) => Cancelled::Request,
            None => Cancelled::Automatic,
        })
    }

    /// Request job `key` was cancelled by its own key: shown as cancelling when it holds the
    /// lane; whether it does.
    pub(super) fn mark_cancelling(&mut self, key: &RequestKey) -> bool {
        match &mut self.0 {
            Some(h) if h.key.as_ref() == Some(key) => {
                h.job.cancelling = true;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::protocol::{LaneKind, LaneOrigin};
    use std::path::PathBuf;

    fn job(kind: LaneKind, origin: LaneOrigin, cancellable: bool) -> LaneJob {
        LaneJob {
            label: "Backing up ~/proj".to_owned(),
            kind,
            origin,
            started_unix_ms: 1,
            progress: None,
            pass: None,
            cancellable,
            cancelling: false,
        }
    }

    fn key(seq: u64) -> RequestKey {
        RequestKey {
            client: "gui-1".to_owned(),
            seq,
        }
    }

    fn packing(done: u64) -> Progress {
        Progress {
            stage: "Packing".to_owned(),
            done,
            total: Some(100),
        }
    }

    #[test]
    fn a_request_job_shows_its_kind_origin_and_progress_until_it_ends() {
        let mut lane = BackupLane::default();
        let kind = LaneKind::ProjectBackup {
            project: PathBuf::from("/home/u/proj"),
        };
        let origin = LaneOrigin::Device("Pixel".to_owned());
        lane.claim(
            job(kind.clone(), origin.clone(), true),
            Arc::new(AtomicBool::new(false)),
            Some(key(1)),
        );
        let shown = lane.job().expect("held");
        assert_eq!(
            (
                &shown.kind,
                &shown.origin,
                shown.label.as_str(),
                &shown.progress
            ),
            (&kind, &origin, "Backing up ~/proj", &None)
        );

        // Another job's (or automatic work's) report is not this job's.
        assert!(!lane.progress(Some(&key(2)), packing(10)));
        assert!(!lane.progress(None, packing(10)));
        assert!(lane.progress(Some(&key(1)), packing(40)));
        assert_eq!(
            lane.job().and_then(|j| j.progress.clone()),
            Some(packing(40))
        );

        // Only its own end frees the lane.
        assert!(!lane.release(None));
        assert!(!lane.release(Some(&key(2))));
        assert!(lane.release(Some(&key(1))));
        assert!(lane.is_free());
    }

    #[test]
    fn cancelling_with_nothing_running_is_refused() {
        let mut lane = BackupLane::default();
        assert_eq!(lane.cancel_refusal(), Some("no backup job is running"));
        assert_eq!(lane.cancel(), None);
    }

    #[test]
    fn a_job_that_cannot_stop_refuses_cancel_and_keeps_running() {
        let mut lane = BackupLane::default();
        let cancel = Arc::new(AtomicBool::new(false));
        lane.claim(
            job(LaneKind::ConfigBackup, LaneOrigin::Automatic, false),
            cancel.clone(),
            None,
        );
        assert_eq!(lane.cancel_refusal(), Some("this job cannot be cancelled"));
        assert_eq!(lane.cancel(), None);
        assert!(!cancel.load(Ordering::Relaxed));
        assert!(lane.job().is_some_and(|j| !j.cancelling));
    }

    #[test]
    fn cancelling_a_request_job_flags_it_for_its_worker() {
        let mut lane = BackupLane::default();
        let cancel = Arc::new(AtomicBool::new(false));
        lane.claim(
            job(
                LaneKind::ProjectRestore {
                    target: PathBuf::from("/home/u/proj"),
                },
                LaneOrigin::Desktop,
                true,
            ),
            cancel.clone(),
            Some(key(3)),
        );
        assert_eq!(lane.cancel_refusal(), None);
        assert_eq!(lane.cancel(), Some(Cancelled::Request));
        assert!(cancel.load(Ordering::Relaxed));
        assert!(lane.job().is_some_and(|j| j.cancelling));
        // Still held until the job reports its end.
        assert!(!lane.is_free());
    }
}
