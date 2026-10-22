//! Background thread behind the Agent Hub panel.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::scan::AgentsScanner;
use super::{AgentStatus, AgentsSnapshot, AgentsTarget};
use crate::status::lock;

const BUSY_INTERVAL: Duration = Duration::from_secs(1);
const IDLE_INTERVAL: Duration = Duration::from_secs(3);

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

fn run(shared: &Shared, on_update: &dyn Fn()) {
    let mut scanner: Option<AgentsScanner> = None;
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
            scanner = None;
            continue;
        };
        if scanner
            .as_ref()
            .is_none_or(|s| s.session() != target.session)
        {
            scanner = Some(AgentsScanner::new(target.session.clone()));
        }
        let Some(scanner) = scanner.as_mut() else {
            continue;
        };
        let snapshot = scanner.scan(target.owner_alive);
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
                if latest.as_deref() == Some(&snapshot) {
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
