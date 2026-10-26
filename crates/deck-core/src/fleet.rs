//! Cross-session view for the agents dashboard: model, context, cost, last reply, subagents and
//! git counts of many sessions at once, read-only from omp's files.
//!
//! [`FleetWatcher`] runs two threads. The session thread keeps one incremental
//! [`SessionStatsReader`] and one [`AgentsScanner`] per target, handles one session per step
//! (most overdue first) and publishes after each step that changed something. The git thread runs
//! `git status` once per distinct working directory, so a slow repository can never delay a due
//! session update; its runs are cancelled when the targets change or the watcher is dropped.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::agents::{AgentStatus, AgentSummary, AgentsScanner};
use crate::status::{GitInfo, SessionStatsReader, git_info_cancellable, lock};

/// Stats + agents cadence of a session with a running subagent.
const BUSY_INTERVAL: Duration = Duration::from_secs(1);
/// Stats + agents cadence otherwise.
const IDLE_INTERVAL: Duration = Duration::from_secs(3);
/// `git status` cadence per working directory.
const GIT_INTERVAL: Duration = Duration::from_secs(5);
/// Cap of [`FleetSession::last_reply`] in characters.
pub const LAST_REPLY_CHARS: usize = 400;

/// One session to follow.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FleetTarget {
    /// The session `.jsonl`.
    pub session: PathBuf,
    /// Working directory the git counts are taken in.
    pub cwd: PathBuf,
    /// The session's omp process is running (decides how unfinished subagents are reported).
    pub owner_alive: bool,
}

/// What is known about one session.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FleetSession {
    pub session: PathBuf,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub context_tokens: Option<u64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost: f64,
    /// Start of the last assistant message, whitespace collapsed, at most
    /// [`LAST_REPLY_CHARS`] characters.
    pub last_reply: Option<String>,
    pub last_turn_failed: bool,
    /// Shared with the watcher: publishing never deep-copies unchanged agents.
    pub agents: Arc<Vec<AgentSummary>>,
    /// `None` until the first `git status` of its cwd finished, or when it is no repository.
    pub git: Option<GitInfo>,
}

/// Target order. A target appears once its first read finished (a missing or unreadable file
/// counts as read and shows empty stats); targets still waiting for their first read are absent.
/// Sessions that did not change since the previous snapshot are the same `Arc`.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FleetSnapshot {
    pub sessions: Vec<Arc<FleetSession>>,
}

struct Control {
    targets: Vec<FleetTarget>,
    /// Bumped on every target change; the workers re-sync when it moves.
    generation: u64,
    stop: bool,
}

struct Shared {
    control: Mutex<Control>,
    /// Wakes the session thread.
    wake: Condvar,
    /// Wakes the git thread (same mutex as `wake`).
    git_wake: Condvar,
    latest: Mutex<Option<Arc<FleetSnapshot>>>,
    /// Newest `git status` per cwd (`None` = not a repository / failed).
    git: Mutex<HashMap<PathBuf, Option<GitInfo>>>,
    /// The git thread stored a new result the session thread has not folded in yet.
    git_dirty: AtomicBool,
    /// Cancels a running `git status`; set on retarget and drop, cleared by the git thread
    /// (under the control lock) before each run.
    cancel: AtomicBool,
}

/// Two threads following many sessions.
pub struct FleetWatcher {
    shared: Arc<Shared>,
    handles: Vec<JoinHandle<()>>,
}

impl FleetWatcher {
    /// Starts idle; `on_update` runs after every published change.
    pub fn spawn(on_update: impl Fn() + Send + 'static) -> FleetWatcher {
        let shared = Arc::new(Shared {
            control: Mutex::new(Control {
                targets: Vec::new(),
                generation: 0,
                stop: false,
            }),
            wake: Condvar::new(),
            git_wake: Condvar::new(),
            latest: Mutex::new(None),
            git: Mutex::new(HashMap::new()),
            git_dirty: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
        });
        let mut handles = Vec::new();
        let worker = Arc::clone(&shared);
        match std::thread::Builder::new()
            .name("fleet-sessions".into())
            .spawn(move || run_sessions(&worker, &on_update))
        {
            Ok(h) => handles.push(h),
            Err(e) => log::error!("cannot start the fleet watcher: {e}"),
        }
        let worker = Arc::clone(&shared);
        match std::thread::Builder::new()
            .name("fleet-git".into())
            .spawn(move || run_git(&worker))
        {
            Ok(h) => handles.push(h),
            Err(e) => log::error!("cannot start the fleet git thread: {e}"),
        }
        FleetWatcher { shared, handles }
    }

    /// Replaces the followed sessions. Readers of sessions that stay are kept (incremental);
    /// new ones are read at once, round-robin; removed ones are dropped. An empty list drops
    /// everything, clears [`latest`](Self::latest) and idles both threads. Equal list: no-op.
    pub fn set_targets(&self, targets: Vec<FleetTarget>) {
        {
            let mut ctl = lock(&self.shared.control);
            if ctl.targets == targets {
                return;
            }
            if targets.is_empty() {
                // Under the control lock: a publish of the old generation cannot slip in after.
                *lock(&self.shared.latest) = None;
            }
            ctl.targets = targets;
            ctl.generation += 1;
            self.shared.cancel.store(true, Ordering::Release);
        }
        self.shared.wake.notify_all();
        self.shared.git_wake.notify_all();
    }

    /// The newest snapshot; `None` before the first target was read and after `set_targets(vec![])`.
    pub fn latest(&self) -> Option<Arc<FleetSnapshot>> {
        lock(&self.shared.latest).clone()
    }
}

impl Drop for FleetWatcher {
    fn drop(&mut self) {
        {
            lock(&self.shared.control).stop = true;
            self.shared.cancel.store(true, Ordering::Release);
        }
        self.shared.wake.notify_all();
        self.shared.git_wake.notify_all();
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

struct Entry {
    target: FleetTarget,
    stats: SessionStatsReader,
    scanner: AgentsScanner,
    agents: Arc<Vec<AgentSummary>>,
    /// `None` until the first read finished.
    view: Option<Arc<FleetSession>>,
    due: Instant,
}

impl Entry {
    /// Reads what changed; true when the published view changed.
    fn poll(&mut self, git: &HashMap<PathBuf, Option<GitInfo>>) -> bool {
        let stats_changed = match self.stats.refresh() {
            Ok(changed) => changed,
            Err(e) => {
                log::debug!("fleet: {}: {e}", self.target.session.display());
                false
            }
        };
        let scanned = self.scanner.scan(self.target.owner_alive).agents;
        let busy = scanned.iter().any(|a| a.status == AgentStatus::Running);
        self.due = Instant::now() + if busy { BUSY_INTERVAL } else { IDLE_INTERVAL };
        let agents_changed = scanned != *self.agents;
        if agents_changed {
            self.agents = Arc::new(scanned);
        }
        if self.view.is_some() && !stats_changed && !agents_changed {
            return false;
        }
        let view = self.build_view(git);
        if self.view.as_deref() == Some(&view) {
            return false;
        }
        self.view = Some(Arc::new(view));
        true
    }

    fn build_view(&self, git: &HashMap<PathBuf, Option<GitInfo>>) -> FleetSession {
        let s = self.stats.stats();
        FleetSession {
            session: self.target.session.clone(),
            model: s.model.clone(),
            thinking: s.thinking.clone(),
            context_tokens: s.context_tokens,
            input_tokens: s.input_tokens,
            output_tokens: s.output_tokens,
            cost: s.cost,
            last_reply: s
                .last_assistant_text
                .as_deref()
                .and_then(|t| collapse_whitespace(t, LAST_REPLY_CHARS)),
            last_turn_failed: s.last_turn_failed,
            agents: Arc::clone(&self.agents),
            git: git.get(&self.target.cwd).cloned().flatten(),
        }
    }

    /// Folds a new git result into the view; true when it changed.
    fn apply_git(&mut self, git: &HashMap<PathBuf, Option<GitInfo>>) -> bool {
        let Some(view) = &self.view else {
            return false;
        };
        let now = git.get(&self.target.cwd).cloned().flatten();
        if view.git == now {
            return false;
        }
        let mut next = (**view).clone();
        next.git = now;
        self.view = Some(Arc::new(next));
        true
    }
}

/// Matches the entries to `targets`: kept sessions keep their readers, new ones are due at
/// once, gone ones are dropped. Returns the new list and whether the published set changed.
fn reconcile(entries: Vec<Entry>, targets: &[FleetTarget], now: Instant) -> (Vec<Entry>, bool) {
    let before: Vec<PathBuf> = entries.iter().map(|e| e.target.session.clone()).collect();
    let mut old: HashMap<PathBuf, Entry> = entries
        .into_iter()
        .map(|e| (e.target.session.clone(), e))
        .collect();
    let mut out: Vec<Entry> = Vec::with_capacity(targets.len());
    for target in targets {
        if out.iter().any(|e| e.target.session == target.session) {
            continue; // A duplicate session path keeps its first position only.
        }
        let entry = match old.remove(&target.session) {
            Some(mut e) => {
                if e.target.owner_alive != target.owner_alive {
                    e.due = now;
                }
                e.target = target.clone();
                e
            }
            None => Entry {
                target: target.clone(),
                stats: SessionStatsReader::new(target.session.clone()),
                scanner: AgentsScanner::new(target.session.clone()),
                agents: Arc::new(Vec::new()),
                view: None,
                due: now,
            },
        };
        out.push(entry);
    }
    let same =
        before.len() == out.len() && before.iter().zip(&out).all(|(b, e)| *b == e.target.session);
    (out, !same)
}

fn run_sessions(shared: &Shared, on_update: &dyn Fn()) {
    let mut entries: Vec<Entry> = Vec::new();
    let mut seen_generation = 0;
    loop {
        let (retargeted, generation) = {
            let mut ctl = lock(&shared.control);
            loop {
                if ctl.stop {
                    return;
                }
                if ctl.generation != seen_generation || shared.git_dirty.load(Ordering::Acquire) {
                    break;
                }
                let wait = match entries.iter().map(|e| e.due).min() {
                    Some(due) => {
                        let wait = due.saturating_duration_since(Instant::now());
                        if wait.is_zero() {
                            break;
                        }
                        wait
                    }
                    // Idle: nothing to do until the targets change.
                    None => Duration::from_secs(3600),
                };
                ctl = shared
                    .wake
                    .wait_timeout(ctl, wait)
                    .map_or_else(|e| e.into_inner().0, |(g, _)| g);
            }
            let retargeted = (ctl.generation != seen_generation).then(|| ctl.targets.clone());
            (retargeted, ctl.generation)
        };
        seen_generation = generation;
        let mut dirty = false;
        if let Some(targets) = retargeted {
            let (next, changed) = reconcile(std::mem::take(&mut entries), &targets, Instant::now());
            entries = next;
            dirty |= changed;
            if targets.is_empty() {
                continue;
            }
        }
        if shared.git_dirty.swap(false, Ordering::AcqRel) {
            let git = lock(&shared.git);
            for e in &mut entries {
                dirty |= e.apply_git(&git);
            }
        }
        // One session per step, the most overdue first: the others get their turn right after.
        let now = Instant::now();
        if let Some(entry) = entries
            .iter_mut()
            .filter(|e| e.due <= now)
            .min_by_key(|e| e.due)
        {
            let git = lock(&shared.git).clone();
            dirty |= entry.poll(&git);
        }
        if !dirty {
            continue;
        }
        let snapshot = FleetSnapshot {
            sessions: entries.iter().filter_map(|e| e.view.clone()).collect(),
        };
        let published = {
            let ctl = lock(&shared.control);
            if ctl.generation != generation || ctl.stop {
                false
            } else {
                *lock(&shared.latest) = Some(Arc::new(snapshot));
                true
            }
        };
        if published {
            on_update();
        }
    }
}

fn run_git(shared: &Shared) {
    // cwd → next run.
    let mut due: HashMap<PathBuf, Instant> = HashMap::new();
    let mut seen_generation = u64::MAX;
    loop {
        let cwd = {
            let mut ctl = lock(&shared.control);
            loop {
                if ctl.stop {
                    return;
                }
                if ctl.generation != seen_generation {
                    seen_generation = ctl.generation;
                    let now = Instant::now();
                    let mut keep: HashMap<PathBuf, Instant> = HashMap::new();
                    for t in &ctl.targets {
                        keep.entry(t.cwd.clone())
                            .or_insert_with(|| due.get(&t.cwd).copied().unwrap_or(now));
                    }
                    due = keep;
                    lock(&shared.git).retain(|cwd, _| due.contains_key(cwd));
                }
                let next = due.iter().min_by_key(|(_, d)| **d);
                let wait = match next {
                    Some((cwd, d)) => {
                        let wait = d.saturating_duration_since(Instant::now());
                        if wait.is_zero() {
                            shared.cancel.store(false, Ordering::Release);
                            break cwd.clone();
                        }
                        wait
                    }
                    None => Duration::from_secs(3600),
                };
                ctl = shared
                    .git_wake
                    .wait_timeout(ctl, wait)
                    .map_or_else(|e| e.into_inner().0, |(g, _)| g);
            }
        };
        let info = git_info_cancellable(&cwd, Some(&shared.cancel));
        if shared.cancel.load(Ordering::Acquire) {
            // Cancelled mid-run (retarget or drop): the result is meaningless; retry at once.
            continue;
        }
        due.insert(cwd.clone(), Instant::now() + GIT_INTERVAL);
        let changed = {
            let mut results = lock(&shared.git);
            if results.get(&cwd) == Some(&info) {
                false
            } else {
                results.insert(cwd, info);
                true
            }
        };
        if changed {
            shared.git_dirty.store(true, Ordering::Release);
            let _ctl = lock(&shared.control);
            shared.wake.notify_all();
        }
    }
}

/// Words of `text` joined by single spaces, at most `max` characters (cut on a character
/// boundary, trailing space dropped); `None` when `text` has no words.
fn collapse_whitespace(text: &str, max: usize) -> Option<String> {
    let mut out = String::new();
    let mut chars = 0;
    for word in text.split_whitespace() {
        if chars > 0 {
            out.push(' ');
            chars += 1;
        }
        for c in word.chars() {
            if chars == max {
                out.truncate(out.trim_end().len());
                return Some(out);
            }
            out.push(c);
            chars += 1;
        }
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_joins_words_and_caps_on_char_boundaries() {
        assert_eq!(
            collapse_whitespace("  a \n\t b  ", 10).as_deref(),
            Some("a b")
        );
        assert_eq!(collapse_whitespace(" \n ", 10), None);
        assert_eq!(collapse_whitespace("", 10), None);
        // Exactly at the cap: kept whole; one more character: cut.
        assert_eq!(collapse_whitespace("abcd", 4).as_deref(), Some("abcd"));
        assert_eq!(collapse_whitespace("abcde", 4).as_deref(), Some("abcd"));
        // Multi-byte characters count once and never split.
        assert_eq!(collapse_whitespace("ééé ééé", 5).as_deref(), Some("ééé é"));
        // A cut right after a separator leaves no trailing space.
        assert_eq!(collapse_whitespace("ab cd", 3).as_deref(), Some("ab"));
        let long = "word ".repeat(1000);
        let out = collapse_whitespace(&long, LAST_REPLY_CHARS).unwrap();
        assert!(out.chars().count() <= LAST_REPLY_CHARS && !out.ends_with(' '));
        assert!(out.chars().count() >= LAST_REPLY_CHARS - 1);
    }
}
