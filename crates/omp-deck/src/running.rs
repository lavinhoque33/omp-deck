//! Background tmux poller: which tmux sessions are alive and which omp session files they hold.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use deck_core::tmux::{Tmux, session_name_for};

/// Outcome of [`RunningSnapshot::resolve`]; both variants are the name `attach_or_create` gets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TmuxTarget {
    Attach(String),
    Create(String),
}

impl TmuxTarget {
    pub fn into_name(self) -> String {
        match self {
            TmuxTarget::Attach(n) | TmuxTarget::Create(n) => n,
        }
    }
}

/// One poll result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunningSnapshot {
    pub alive: HashSet<String>,
    pub files: HashMap<String, Vec<PathBuf>>,
    held: HashMap<PathBuf, String>,
}

impl RunningSnapshot {
    pub fn new(alive: HashSet<String>, files: HashMap<String, Vec<PathBuf>>) -> Self {
        let mut held = HashMap::new();
        // Deterministic owner when several tmux sessions claim one file: smallest name.
        let mut names: Vec<&String> = files.keys().collect();
        names.sort();
        for name in names {
            for path in &files[name] {
                held.entry(path.clone()).or_insert_with(|| name.clone());
            }
        }
        RunningSnapshot { alive, files, held }
    }

    /// Running = a live tmux session holds the file, or the canonical tmux name for this id is
    /// alive without a known file (a fresh omp that has not opened its jsonl yet). A canonical
    /// session that holds a *different* file (after `/new`, `/fork`, `/resume`) does not make this
    /// one running.
    pub fn is_running(&self, id: &str, path: &Path) -> bool {
        if self.held.contains_key(path) {
            return true;
        }
        let name = session_name_for(id);
        self.alive.contains(&name) && self.files_of(&name).is_empty()
    }

    /// Which tmux session a tab for `path` must use: the one that holds the file; else `preferred`
    /// when it is not alive (create it) or alive without a known file (attach: a fresh omp);
    /// else — `preferred` runs another file now — a new name derived from it (`<preferred>-<ms>`).
    pub fn resolve(&self, preferred: &str, path: Option<&Path>, unix_ms: u128) -> TmuxTarget {
        if let Some(holder) = path.and_then(|p| self.holder(p)) {
            return TmuxTarget::Attach(holder.to_owned());
        }
        if !self.is_alive(preferred) {
            return TmuxTarget::Create(preferred.to_owned());
        }
        match path {
            Some(_) if !self.files_of(preferred).is_empty() => {
                TmuxTarget::Create(format!("{preferred}-{unix_ms}"))
            }
            _ => TmuxTarget::Attach(preferred.to_owned()),
        }
    }

    /// Name for a tab when no trustworthy snapshot exists (the poller could not start): never
    /// attach to a possibly foreign session, always create `<preferred>-<ms>`.
    pub fn resolve_blind(preferred: &str, unix_ms: u128) -> TmuxTarget {
        TmuxTarget::Create(format!("{preferred}-{unix_ms}"))
    }

    /// [`resolve`](Self::resolve), or [`resolve_blind`](Self::resolve_blind) when `blind`.
    pub fn resolve_for(
        &self,
        blind: bool,
        preferred: &str,
        path: Option<&Path>,
        unix_ms: u128,
    ) -> TmuxTarget {
        if blind {
            Self::resolve_blind(preferred, unix_ms)
        } else {
            self.resolve(preferred, path, unix_ms)
        }
    }

    /// Name of a live tmux session holding `path` open.
    pub fn holder(&self, path: &Path) -> Option<&str> {
        self.held.get(path).map(String::as_str)
    }

    pub fn files_of(&self, tmux_name: &str) -> &[PathBuf] {
        self.files.get(tmux_name).map_or(&[], Vec::as_slice)
    }

    pub fn is_alive(&self, tmux_name: &str) -> bool {
        self.alive.contains(tmux_name)
    }
}

#[derive(Default)]
struct Inner {
    latest: Option<Arc<RunningSnapshot>>,
    stop: bool,
    poke: bool,
    /// Control keys that must survive pruning (open tabs, launches in flight).
    ctl_keep: HashSet<String>,
}

struct Shared {
    inner: Mutex<Inner>,
    cv: Condvar,
}

fn lock(shared: &Shared) -> MutexGuard<'_, Inner> {
    shared.inner.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct RunningMonitor {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    blind: bool,
}

impl RunningMonitor {
    /// Without tmux there is nothing to poll: the snapshot is permanently empty.
    pub fn spawn(
        tmux: Option<Tmux>,
        sessions_root: PathBuf,
        interval: Duration,
        on_update: impl Fn() + Send + 'static,
    ) -> RunningMonitor {
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner::default()),
            cv: Condvar::new(),
        });
        let Some(tmux) = tmux else {
            lock(&shared).latest = Some(Arc::new(RunningSnapshot::default()));
            return RunningMonitor {
                shared,
                thread: None,
                blind: false,
            };
        };
        let worker = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("running-monitor".into())
            .spawn(move || {
                loop {
                    {
                        // Clear before scanning: pokes arriving mid-scan stay set and
                        // make the wait below return immediately.
                        let mut guard = lock(&worker);
                        if guard.stop {
                            return;
                        }
                        guard.poke = false;
                    }
                    let alive = tmux.alive_sessions();
                    // An empty answer may be a failed query: never prune on it, a running
                    // omp's socket would be lost.
                    if !alive.is_empty() {
                        let mut names = alive.clone();
                        names.extend(lock(&worker).ctl_keep.iter().cloned());
                        deck_core::ctl::prune_ctl_dirs(tmux.socket(), &names);
                    }
                    let files = if alive.is_empty() {
                        HashMap::new()
                    } else {
                        tmux.session_files(&sessions_root)
                    };
                    let snap = RunningSnapshot::new(alive, files);
                    let mut guard = lock(&worker);
                    if guard.stop {
                        return;
                    }
                    let changed = guard.latest.as_deref() != Some(&snap);
                    if changed {
                        guard.latest = Some(Arc::new(snap));
                        drop(guard);
                        on_update();
                        guard = lock(&worker);
                    }
                    let (guard, _) = worker
                        .cv
                        .wait_timeout_while(guard, interval, |s| !s.stop && !s.poke)
                        .unwrap_or_else(PoisonError::into_inner);
                    if guard.stop {
                        return;
                    }
                }
            })
            .map_err(|e| log::error!("cannot start running monitor: {e}"))
            .ok();
        let failed = thread.is_none();
        if failed {
            // Resolve waiting requests against an empty snapshot; `is_blind` makes them pick
            // unique tmux names instead of trusting it.
            lock(&shared).latest = Some(Arc::new(RunningSnapshot::default()));
        }
        RunningMonitor {
            shared,
            thread,
            blind: failed,
        }
    }

    pub fn latest(&self) -> Option<Arc<RunningSnapshot>> {
        lock(&self.shared).latest.clone()
    }

    /// Control keys of tabs and launches that the next prune must keep.
    pub fn set_ctl_keep(&self, keep: HashSet<String>) {
        lock(&self.shared).ctl_keep = keep;
    }

    /// Re-poll immediately (after opening/killing a session).
    pub fn poke(&self) {
        lock(&self.shared).poke = true;
        self.shared.cv.notify_all();
    }

    /// True when tmux exists but its poller could not start: snapshots carry no information.
    pub fn is_blind(&self) -> bool {
        self.blind
    }
}

impl Drop for RunningMonitor {
    fn drop(&mut self) {
        lock(&self.shared).stop = true;
        self.shared.cv.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_by_file_or_name() {
        let mut files = HashMap::new();
        files.insert("omp-new-1".to_string(), vec![PathBuf::from("/s/a.jsonl")]);
        let alive: HashSet<String> = ["omp-new-1".to_string(), "omp-b".to_string()].into();
        let s = RunningSnapshot::new(alive, files);
        assert!(s.is_running("zzz", Path::new("/s/a.jsonl")));
        // `omp-b` is alive and its file is unknown: assume it is this session.
        assert!(s.is_running("b", Path::new("/s/other.jsonl")));
        assert!(!s.is_running("c", Path::new("/s/other.jsonl")));
        assert_eq!(s.holder(Path::new("/s/a.jsonl")), Some("omp-new-1"));
        assert_eq!(s.files_of("omp-new-1").len(), 1);
        assert!(s.files_of("nope").is_empty());
    }

    fn snap(files: &[(&str, &[&str])], alive: &[&str]) -> RunningSnapshot {
        RunningSnapshot::new(
            alive.iter().map(|a| (*a).to_owned()).collect(),
            files
                .iter()
                .map(|(n, f)| ((*n).to_owned(), f.iter().map(PathBuf::from).collect()))
                .collect(),
        )
    }

    #[test]
    fn canonical_session_running_another_file_is_not_this_session() {
        // `/new` in the tab opened for id `a`: omp-a now holds b.jsonl.
        let s = snap(&[("omp-a", &["/s/b.jsonl"])], &["omp-a"]);
        assert!(!s.is_running("a", Path::new("/s/a.jsonl")), "old file");
        assert!(
            s.is_running("b", Path::new("/s/b.jsonl")),
            "new file, by holder"
        );
        assert_eq!(
            s.resolve("omp-a", Some(Path::new("/s/a.jsonl")), 42),
            TmuxTarget::Create("omp-a-42".into())
        );
        assert_eq!(
            s.resolve("omp-b", Some(Path::new("/s/b.jsonl")), 42),
            TmuxTarget::Attach("omp-a".into()),
            "attach to whichever tmux session holds the file"
        );
    }

    #[test]
    fn resolve_creates_or_attaches_the_canonical_name() {
        let s = snap(&[], &["omp-fresh"]);
        // Not alive: create under the canonical name.
        assert_eq!(
            s.resolve("omp-a", Some(Path::new("/s/a.jsonl")), 1),
            TmuxTarget::Create("omp-a".into())
        );
        // Alive, file unknown (fresh omp): attach.
        assert_eq!(
            s.resolve("omp-fresh", Some(Path::new("/s/x.jsonl")), 1),
            TmuxTarget::Attach("omp-fresh".into())
        );
        // No file to compare (new tab): the preferred name as is.
        assert_eq!(
            snap(&[("omp-n", &["/s/z.jsonl"])], &["omp-n"]).resolve("omp-n", None, 1),
            TmuxTarget::Attach("omp-n".into())
        );
    }

    #[test]
    fn no_tmux_gives_empty_snapshot() {
        let m = RunningMonitor::spawn(None, PathBuf::from("/x"), Duration::from_secs(1), || {});
        assert_eq!(m.latest().as_deref(), Some(&RunningSnapshot::default()));
    }

    #[test]
    fn blind_resolution_always_creates_unique_name() {
        let mut alive = HashSet::new();
        alive.insert("omp-a".to_owned());
        let snap = RunningSnapshot::new(alive, HashMap::new());
        let t = snap.resolve_for(true, "omp-a", Some(Path::new("/x.jsonl")), 42);
        assert_eq!(t, TmuxTarget::Create("omp-a-42".into()));
        let t = snap.resolve_for(false, "omp-a", None, 42);
        assert_eq!(t, TmuxTarget::Attach("omp-a".into()));
    }
}
