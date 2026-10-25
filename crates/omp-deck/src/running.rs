//! Background poller: which sessions are alive in the available backends (tmux and/or the
//! OmpDeck host) and which omp session files they hold.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use deck_core::mux::session_name_for;
use deck_core::mux::{Mux, MuxKind};

use deck_service::statusfmt::Activity;

/// Outcome of [`RunningSnapshot::resolve`]; both variants are the name `attach_or_create` gets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionTarget {
    Attach(String),
    Create(String),
}

impl SessionTarget {
    pub fn into_name(self) -> String {
        match self {
            SessionTarget::Attach(n) | SessionTarget::Create(n) => n,
        }
    }
}

/// One poll result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunningSnapshot {
    pub alive: HashSet<String>,
    pub files: HashMap<String, Vec<PathBuf>>,
    /// Backend of each live session (the first one listing it); empty when unknown.
    owners: HashMap<String, MuxKind>,
    held: HashMap<PathBuf, String>,
    /// What omp reports in the terminal title of each live session (`Activity::Unknown` is not
    /// stored). Only state changes alter the snapshot, not spinner frames.
    activities: HashMap<String, Activity>,
}

impl RunningSnapshot {
    pub fn new(alive: HashSet<String>, files: HashMap<String, Vec<PathBuf>>) -> Self {
        let mut held = HashMap::new();
        // Deterministic owner when several sessions claim one file: smallest name.
        let mut names: Vec<&String> = files.keys().collect();
        names.sort();
        for name in names {
            for path in &files[name] {
                held.entry(path.clone()).or_insert_with(|| name.clone());
            }
        }
        RunningSnapshot {
            alive,
            files,
            owners: HashMap::new(),
            held,
            activities: HashMap::new(),
        }
    }

    /// Records the activity each live session's terminal title shows.
    pub fn with_activities(mut self, activities: HashMap<String, Activity>) -> Self {
        self.activities = activities;
        self
    }

    /// What live session `name` is doing, from its terminal title; `Unknown` without one.
    pub fn activity_of(&self, name: &str) -> Activity {
        self.activities
            .get(name)
            .copied()
            .unwrap_or(Activity::Unknown)
    }

    /// Records which backend each live session belongs to.
    pub fn with_owners(mut self, owners: HashMap<String, MuxKind>) -> Self {
        self.owners = owners;
        self
    }

    /// The backend holding live session `name`.
    pub fn owner_of(&self, name: &str) -> Option<MuxKind> {
        self.owners.get(name).copied()
    }

    /// Running = a live session holds the file, or the canonical tmux name for this id is
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

    /// Which session a tab for `path` must use: the one that holds the file; else `preferred`
    /// when it is not alive (create it) or alive without a known file (attach: a fresh omp);
    /// else — `preferred` runs another file now — a new name derived from it (`<preferred>-<ms>`).
    pub fn resolve(&self, preferred: &str, path: Option<&Path>, unix_ms: u128) -> SessionTarget {
        if let Some(holder) = path.and_then(|p| self.holder(p)) {
            return SessionTarget::Attach(holder.to_owned());
        }
        if !self.is_alive(preferred) {
            return SessionTarget::Create(preferred.to_owned());
        }
        match path {
            Some(_) if !self.files_of(preferred).is_empty() => {
                SessionTarget::Create(format!("{preferred}-{unix_ms}"))
            }
            _ => SessionTarget::Attach(preferred.to_owned()),
        }
    }

    /// Name for a tab when no trustworthy snapshot exists (the poller could not start): never
    /// attach to a possibly foreign session, always create `<preferred>-<ms>`.
    pub fn resolve_blind(preferred: &str, unix_ms: u128) -> SessionTarget {
        SessionTarget::Create(format!("{preferred}-{unix_ms}"))
    }

    /// [`resolve`](Self::resolve), or [`resolve_blind`](Self::resolve_blind) when `blind`.
    pub fn resolve_for(
        &self,
        blind: bool,
        preferred: &str,
        path: Option<&Path>,
        unix_ms: u128,
    ) -> SessionTarget {
        if blind {
            Self::resolve_blind(preferred, unix_ms)
        } else {
            self.resolve(preferred, path, unix_ms)
        }
    }

    /// Name of a live session holding `path` open.
    pub fn holder(&self, path: &Path) -> Option<&str> {
        self.held.get(path).map(String::as_str)
    }

    pub fn files_of(&self, mux_name: &str) -> &[PathBuf] {
        self.files.get(mux_name).map_or(&[], Vec::as_slice)
    }

    pub fn is_alive(&self, mux_name: &str) -> bool {
        self.alive.contains(mux_name)
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
    /// Polls every backend in `muxes` (live sessions are the union; control dirs under
    /// `ctl_ns` are pruned against it). Without a backend there is nothing to poll: the
    /// snapshot is permanently empty.
    pub fn spawn(
        muxes: Vec<Mux>,
        ctl_ns: String,
        sessions_root: PathBuf,
        interval: Duration,
        on_update: impl Fn() + Send + 'static,
    ) -> RunningMonitor {
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner::default()),
            cv: Condvar::new(),
        });
        if muxes.is_empty() {
            lock(&shared).latest = Some(Arc::new(RunningSnapshot::default()));
            return RunningMonitor {
                shared,
                thread: None,
                blind: false,
            };
        }
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
                    let snap = poll(&muxes, &sessions_root);
                    // An empty answer may be a failed query: never prune on it, a running
                    // omp's socket would be lost.
                    if !snap.alive.is_empty() {
                        let mut names = snap.alive.clone();
                        names.extend(lock(&worker).ctl_keep.iter().cloned());
                        deck_core::ctl::prune_ctl_dirs(&ctl_ns, &names);
                    }
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
            // unique names instead of trusting it.
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

    /// True when backends exist but their poller could not start: snapshots carry no information.
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

/// One backend's answer to a poll.
struct Polled {
    kind: MuxKind,
    alive: HashSet<String>,
    files: HashMap<String, Vec<PathBuf>>,
    /// Session name → current terminal title.
    titles: HashMap<String, String>,
}

/// Asks every backend (blocking) and merges the answers.
fn poll(muxes: &[Mux], sessions_root: &Path) -> RunningSnapshot {
    merge(muxes.iter().map(|mux| {
        let alive = mux.alive_sessions();
        let (files, titles) = if alive.is_empty() {
            (HashMap::new(), HashMap::new())
        } else {
            (mux.session_files(sessions_root), mux.pane_titles())
        };
        Polled {
            kind: mux.kind(),
            alive,
            files,
            titles,
        }
    }))
}

/// Union of the backends' sessions. A name listed by several backends belongs to the first.
fn merge(polled: impl IntoIterator<Item = Polled>) -> RunningSnapshot {
    let mut alive = HashSet::new();
    let mut files: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut activities = HashMap::new();
    let mut owners = HashMap::new();
    for p in polled {
        for name in p.alive {
            if alive.insert(name.clone()) {
                owners.insert(name, p.kind);
            }
        }
        for (name, list) in p.files {
            if owners.get(&name) == Some(&p.kind) {
                files.insert(name, list);
            }
        }
        for (name, title) in p.titles {
            let activity = Activity::from_title(Some(&title));
            if activity != Activity::Unknown && owners.get(&name) == Some(&p.kind) {
                activities.insert(name, activity);
            }
        }
    }
    RunningSnapshot::new(alive, files)
        .with_owners(owners)
        .with_activities(activities)
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
            SessionTarget::Create("omp-a-42".into())
        );
        assert_eq!(
            s.resolve("omp-b", Some(Path::new("/s/b.jsonl")), 42),
            SessionTarget::Attach("omp-a".into()),
            "attach to whichever tmux session holds the file"
        );
    }

    #[test]
    fn resolve_creates_or_attaches_the_canonical_name() {
        let s = snap(&[], &["omp-fresh"]);
        // Not alive: create under the canonical name.
        assert_eq!(
            s.resolve("omp-a", Some(Path::new("/s/a.jsonl")), 1),
            SessionTarget::Create("omp-a".into())
        );
        // Alive, file unknown (fresh omp): attach.
        assert_eq!(
            s.resolve("omp-fresh", Some(Path::new("/s/x.jsonl")), 1),
            SessionTarget::Attach("omp-fresh".into())
        );
        // No file to compare (new tab): the preferred name as is.
        assert_eq!(
            snap(&[("omp-n", &["/s/z.jsonl"])], &["omp-n"]).resolve("omp-n", None, 1),
            SessionTarget::Attach("omp-n".into())
        );
    }

    #[test]
    fn no_backend_gives_empty_snapshot() {
        let m = RunningMonitor::spawn(
            Vec::new(),
            "direct".into(),
            PathBuf::from("/x"),
            Duration::from_secs(1),
            || {},
        );
        assert_eq!(m.latest().as_deref(), Some(&RunningSnapshot::default()));
    }

    fn polled(kind: MuxKind, files: &[(&str, &[&str])], alive: &[&str]) -> Polled {
        Polled {
            kind,
            alive: alive.iter().map(|a| (*a).to_owned()).collect(),
            files: files
                .iter()
                .map(|(n, f)| ((*n).to_owned(), f.iter().map(PathBuf::from).collect()))
                .collect(),
            titles: HashMap::new(),
        }
    }

    #[test]
    fn sessions_of_both_backends_are_running_and_routed_to_their_owner() {
        let s = merge([
            polled(MuxKind::Tmux, &[("omp-a", &["/s/a.jsonl"])], &["omp-a"]),
            polled(
                MuxKind::Host,
                &[("omp-b", &["/s/b.jsonl"]), ("omp-a", &["/s/stale.jsonl"])],
                &["omp-b", "omp-a"],
            ),
        ]);
        assert!(s.is_running("a", Path::new("/s/a.jsonl")));
        assert!(s.is_running("b", Path::new("/s/b.jsonl")));
        assert_eq!(
            s.owner_of("omp-a"),
            Some(MuxKind::Tmux),
            "first backend wins"
        );
        assert_eq!(s.owner_of("omp-b"), Some(MuxKind::Host));
        assert_eq!(s.owner_of("omp-zzz"), None);
        assert_eq!(s.files_of("omp-a"), [PathBuf::from("/s/a.jsonl")]);
    }

    #[test]
    fn titles_become_activity_of_the_owning_backend_only() {
        let mut tmux = polled(MuxKind::Tmux, &[], &["omp-a", "omp-b", "omp-c"]);
        tmux.titles = [
            ("omp-a".to_owned(), "π > proj".to_owned()),
            ("omp-b".to_owned(), "π ⠦ proj".to_owned()),
            ("omp-c".to_owned(), "bash".to_owned()),
        ]
        .into();
        let mut host = polled(MuxKind::Host, &[], &["omp-a"]);
        host.titles = [("omp-a".to_owned(), "π ! stale".to_owned())].into();
        let s = merge([tmux, host]);
        assert_eq!(s.activity_of("omp-a"), Activity::Idle, "first backend wins");
        assert_eq!(s.activity_of("omp-b"), Activity::Busy);
        assert_eq!(
            s.activity_of("omp-c"),
            Activity::Unknown,
            "not an omp title"
        );
        assert_eq!(s.activity_of("omp-zzz"), Activity::Unknown);
    }

    #[test]
    fn blind_resolution_always_creates_unique_name() {
        let mut alive = HashSet::new();
        alive.insert("omp-a".to_owned());
        let snap = RunningSnapshot::new(alive, HashMap::new());
        let t = snap.resolve_for(true, "omp-a", Some(Path::new("/x.jsonl")), 42);
        assert_eq!(t, SessionTarget::Create("omp-a-42".into()));
        let t = snap.resolve_for(false, "omp-a", None, 42);
        assert_eq!(t, SessionTarget::Attach("omp-a".into()));
    }
}
