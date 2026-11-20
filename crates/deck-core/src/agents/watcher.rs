//! Background thread behind the Agent Hub panel.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::scan::AgentsScanner;
use super::{AgentStatus, AgentsSnapshot, AgentsTarget};
use crate::status::lock;

const BUSY_INTERVAL: Duration = Duration::from_secs(1);
const IDLE_INTERVAL: Duration = Duration::from_secs(3);
// Park folds and offsets, never watcher threads or open files. The byte budget is shared by
// all inactive sessions; a scanner too large to admit simply starts cold on its next visit.
const WARM_SESSIONS: usize = 3;
const WARM_BYTES: usize = 8 * 1024 * 1024;

struct Control {
    target: Option<AgentsTarget>,
    /// Bumped on every target change; a scan started under an older value publishes nothing.
    generation: u64,
    stop: bool,
}

struct Shared {
    control: Mutex<Control>,
    wake: Condvar,
    latest: Mutex<Option<Arc<AgentsSnapshot>>>,
}

/// One thread tracking the active tab's session.
pub struct AgentsWatcher {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

impl AgentsWatcher {
    /// Rescans `<stem>/` (recursively for nested dirs) + incrementally tails each transcript
    /// ((dev,ino) + offset + partial-line; reset on inode change/shrink); 1 s while any agent is
    /// Running, else 3 s; publishes only on change, then runs `on_update`.
    pub fn spawn(on_update: impl Fn() + Send + 'static) -> AgentsWatcher {
        let shared = Arc::new(Shared {
            control: Mutex::new(Control {
                target: None,
                generation: 0,
                stop: false,
            }),
            wake: Condvar::new(),
            latest: Mutex::new(None),
        });
        let worker_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("deck-agents".into())
            .spawn(move || run(&worker_shared, &on_update));
        let handle = match handle {
            Ok(h) => Some(h),
            Err(e) => {
                log::error!("cannot spawn agents watcher thread: {e}");
                None
            }
        };
        AgentsWatcher { shared, handle }
    }

    /// Switches the tracked session (scanned immediately). Changing only `owner_alive` keeps the
    /// current snapshot until the rescan; a different session drops it at once. Equal target:
    /// no-op.
    pub fn set_target(&self, target: Option<AgentsTarget>) {
        let mut ctl = lock(&self.shared.control);
        if ctl.target == target {
            return;
        }
        let same_session = match (&ctl.target, &target) {
            (Some(a), Some(b)) => a.session == b.session,
            _ => false,
        };
        ctl.target = target;
        ctl.generation += 1;
        if !same_session {
            *lock(&self.shared.latest) = None;
        }
        drop(ctl);
        self.shared.wake.notify_all();
    }

    /// Snapshot for the current target; `None` until its first scan finished.
    pub fn latest(&self) -> Option<Arc<AgentsSnapshot>> {
        lock(&self.shared.latest).clone()
    }
}

impl Drop for AgentsWatcher {
    fn drop(&mut self) {
        lock(&self.shared.control).stop = true;
        self.shared.wake.notify_all();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[derive(Default)]
struct Scanners {
    active: Option<AgentsScanner>,
    warm: VecDeque<(AgentsScanner, usize)>, // newest first
}

impl Scanners {
    fn follow(&mut self, session: &Path) -> &mut AgentsScanner {
        if self.active.as_ref().is_some_and(|s| s.session() != session) {
            let next = self
                .warm
                .iter()
                .position(|(s, _)| s.session() == session)
                .and_then(|i| self.warm.remove(i));
            if let Some(previous) = self.active.take() {
                let bytes = previous.cache_weight();
                if bytes <= WARM_BYTES {
                    let mut total = bytes + self.warm.iter().map(|(_, size)| size).sum::<usize>();
                    while self.warm.len() >= WARM_SESSIONS || total > WARM_BYTES {
                        if let Some((_, evicted)) = self.warm.pop_back() {
                            total -= evicted;
                        }
                    }
                    self.warm.push_front((previous, bytes));
                }
            }
            self.active = next.map(|(scanner, _)| scanner);
        }
        self.active
            .get_or_insert_with(|| AgentsScanner::new(session.to_owned()))
    }
}

fn run(shared: &Shared, on_update: &dyn Fn()) {
    let mut scanners = Scanners::default();
    let mut seen_generation = u64::MAX;
    let mut next = Instant::now();
    loop {
        let (target, generation) = {
            let mut ctl = lock(&shared.control);
            loop {
                if ctl.stop {
                    return;
                }
                if ctl.generation != seen_generation {
                    break;
                }
                let now = Instant::now();
                if ctl.target.is_some() && now >= next {
                    break;
                }
                let wait = if ctl.target.is_some() {
                    next.saturating_duration_since(now)
                } else {
                    Duration::from_secs(3600)
                };
                ctl = shared
                    .wake
                    .wait_timeout(ctl, wait)
                    .map_or_else(|e| e.into_inner().0, |(g, _)| g);
            }
            (ctl.target.clone(), ctl.generation)
        };
        seen_generation = generation;
        let Some(target) = target else {
            scanners = Scanners::default();
            continue;
        };
        let snapshot = scanners.follow(&target.session).scan(target.owner_alive);
        let busy = snapshot
            .agents
            .iter()
            .any(|a| a.status == AgentStatus::Running);
        next = Instant::now() + if busy { BUSY_INTERVAL } else { IDLE_INTERVAL };

        let changed = {
            let ctl = lock(&shared.control);
            if ctl.generation != generation || ctl.stop {
                false
            } else {
                let mut latest = lock(&shared.latest);
                if latest
                    .as_ref()
                    .is_some_and(|old| Arc::ptr_eq(&old.agents, &snapshot.agents))
                {
                    false
                } else {
                    *latest = Some(Arc::new(snapshot));
                    true
                }
            }
        };
        if changed {
            on_update();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    #[test]
    fn oversized_partial_transcript_is_not_kept_after_switching_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("one.jsonl");
        fs::create_dir(session.with_extension("")).unwrap();
        let mut file = fs::File::create(session.with_extension("").join("worker.jsonl")).unwrap();
        file.write_all(b"{\"type\":\"session_init\",\"task\":\"example\"}\n")
            .unwrap();
        // Simulate a large tool result whose terminating newline hasn't arrived.
        file.set_len((WARM_BYTES + 1024) as u64).unwrap();
        drop(file);
        let mut scanners = Scanners::default();
        let current = scanners.follow(&session).scan(true);
        let old_row = Arc::downgrade(&current.agents[0]);
        drop(current);
        scanners.follow(&dir.path().join("two.jsonl"));
        assert!(
            old_row.upgrade().is_none(),
            "inactive scanner exceeded the warm budget"
        );
        assert_eq!(scanners.follow(&session).scan(true).agents[0].id, "worker");
    }
}
