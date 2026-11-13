//! Durable child links; all filesystem, git and incremental stats work runs off the actor.
//! Failed registry loads are retried without writing; view publication compares complete cached views.
use super::*;
use crate::gateway::protocol::{OrchNode, OrchReport, OrchWorktree, OrchestrationView, Topic};
use crate::status::TabStatus;
use deck_core::status::SessionStats;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};

const INTERVAL: Duration = Duration::from_secs(5);
const MAX_AGE: u64 = 14 * 24 * 60 * 60 * 1000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::gateway) struct Link {
    handle: String,
    opener_root: PathBuf,
    child_file: PathBuf,
    title: String,
    opened_at_unix_ms: u64,
    fan_out: Option<String>,
    last_report: Option<OrchReport>,
    #[serde(skip)]
    file_seen: bool,
}
pub(in crate::gateway) struct Restored {
    link: Link,
    cwd: PathBuf,
    id: String,
    opener: Option<(PathBuf, SessionKey)>,
}
#[derive(Default)]
pub(in crate::gateway::engine) struct State {
    pub(super) pending_prunes: HashSet<String>,
    next_load: Option<Instant>,
    loaded: bool,
    io_busy: bool,
    saved: Vec<Link>,
    force_save: bool,
    next_prune: Option<Instant>,
    next_stats: Option<Instant>,
    stats_busy: bool,
    readers: HashMap<PathBuf, SessionStatsReader>,
    stats: HashMap<PathBuf, SessionStats>,
    repos: HashMap<PathBuf, Option<OrchWorktree>>,
    cwds: HashMap<PathBuf, PathBuf>,
    published: Option<OrchestrationView>,
}
impl State {
    fn loaded(&mut self, success: bool, now: Instant) {
        self.io_busy = false;
        self.loaded = success;
        self.force_save = success;
        self.next_load = (!success).then_some(now + INTERVAL);
    }

    fn ready_to_save(&self) -> bool {
        self.loaded && !self.io_busy
    }

    fn view_changed(&mut self, view: OrchestrationView) -> bool {
        if self.published.as_ref() == Some(&view) {
            return false;
        }
        self.published = Some(view);
        true
    }
}

pub(in crate::gateway) struct Removed {
    link: Link,
    file_exists: bool,
}

#[derive(PartialEq, Eq)]
pub(in crate::gateway) struct ProbeIdentity {
    key: SessionKey,
    target: Option<SessionTarget>,
    live: bool,
    attaching: bool,
    file_seen: bool,
}
pub(in crate::gateway) enum StatsUpdate {
    Loaded(Result<Vec<Restored>, String>),
    Saved {
        submitted: Vec<Link>,
        removed: Vec<Removed>,
        identities: HashMap<String, ProbeIdentity>,
        result: Result<(), String>,
    },
    Refreshed {
        readers: HashMap<PathBuf, SessionStatsReader>,
        stats: HashMap<PathBuf, SessionStats>,
        repos: HashMap<PathBuf, Option<OrchWorktree>>,
        cwds: HashMap<PathBuf, PathBuf>,
    },
}

fn storage() -> Option<PathBuf> {
    dirs::data_dir().map(|p| p.join("omp-deck/agent-children.json"))
}
fn header(file: &Path) -> Option<(PathBuf, String)> {
    let reader = BufReader::new(std::fs::File::open(file).ok()?.take(32 * 1024));
    for line in reader.lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value["type"] == "session" {
            let cwd = PathBuf::from(value["cwd"].as_str()?);
            let id = value["id"].as_str()?.to_owned();
            return (cwd.is_absolute() && !id.is_empty()).then_some((cwd, id));
        }
    }
    None
}

fn load_registry(path: &Path, openers: &[(PathBuf, SessionKey)]) -> Result<Vec<Restored>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let links: Vec<Link> = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let openers: Vec<_> = openers
        .iter()
        .map(|(path, source)| (canonical(path), path, source))
        .collect();
    Ok(links
        .into_iter()
        .filter_map(|mut link| {
            if !link.opener_root.is_absolute()
                || !link.child_file.is_absolute()
                || link.handle.is_empty()
            {
                return None;
            }
            let (cwd, id) = header(&link.child_file)?;
            link.opener_root = canonical(&link.opener_root);
            link.child_file = canonical(&link.child_file);
            let opener = openers
                .iter()
                .find(|(path, _, _)| path == &link.opener_root)
                .map(|(_, original, source)| ((*original).clone(), (*source).clone()));
            Some(Restored {
                link,
                cwd,
                id,
                opener,
            })
        })
        .collect())
}
fn atomic_write(path: &Path, links: &[Link]) -> Result<(), String> {
    let dir = path.parent().ok_or("Missing child registry directory")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
    let temp = dir.join(format!(
        ".agent-children-{:016x}.tmp",
        u64::from_le_bytes(bytes)
    ));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp).map_err(|e| e.to_string())?;
        serde_json::to_writer(&mut file, links).map_err(|e| e.to_string())?;
        file.write_all(b"\n")
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
        std::fs::rename(&temp, path).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        std::fs::File::open(dir)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}
fn retain_link(file_exists: bool, file_seen: bool, age_ms: u64, live: bool) -> bool {
    (file_exists && (age_ms <= MAX_AGE || live)) || (!file_seen && live)
}

impl Link {
    fn probe_current(
        &self,
        child: &ChildSession,
        expected: Option<&ProbeIdentity>,
        current: Option<&ProbeIdentity>,
        now: Instant,
    ) -> bool {
        !child.invalidated
            && child.opener_root == self.opener_root
            && child.file.as_ref() == Some(&self.child_file)
            && child.opened_at_unix_ms == self.opened_at_unix_ms
            && !child.attaching
            && !child.relocation_discovering(now)
            && expected.is_some()
            && current == expected
    }
}

impl Engine {
    fn child_link_live(&self, handle: &str, now_unix_ms: u64) -> bool {
        let Some(child) = self.agent_tools.children.entries.get(handle) else {
            return false;
        };
        if self
            .sess
            .find(&child.key)
            .is_some_and(|i| !self.sess.managed[i].exited())
            || (!child.restored
                && now_unix_ms.saturating_sub(child.opened_at_unix_ms)
                    < DELIVERY.as_millis() as u64)
            || self
                .child_question_snapshot(handle)
                .is_some_and(|snapshot| {
                    snapshot.session_file.as_deref() == child.file.as_deref()
                        && snapshot.session_file.is_some()
                })
        {
            return true;
        }
        // Initial omp turns can be waiting in ask before their announced jsonl exists.
        // Closing the terminal controller does not stop that backend or its ctl link.
        let Some(running) = self.running.latest().filter(|running| !running.blind) else {
            return true;
        };
        child.attach.mux_name.as_deref().is_some_and(|name| {
            running.is_alive(name)
                && running.owner_of(name) == child.backend
                && (running.files_of(name).is_empty()
                    || child
                        .file
                        .as_ref()
                        .is_some_and(|file| running.files_of(name).iter().any(|path| path == file)))
        })
    }

    fn child_probe_identity(&self, handle: &str, now_unix_ms: u64) -> Option<ProbeIdentity> {
        let child = self.agent_tools.children.entries.get(handle)?;
        Some(ProbeIdentity {
            key: child.key.clone(),
            target: self
                .sess
                .find(&child.key)
                .map(|i| self.sess.managed[i].ctl.target().clone()),
            live: self.child_link_live(handle, now_unix_ms),
            attaching: child.attaching,
            file_seen: child.file_seen,
        })
    }

    fn child_probe_current(&self, link: &Link, identity: Option<&ProbeIdentity>) -> bool {
        self.agent_tools
            .children
            .entries
            .get(&link.handle)
            .is_some_and(|child| {
                link.probe_current(
                    child,
                    identity,
                    self.child_probe_identity(&link.handle, crate::gateway::unix_ms())
                        .as_ref(),
                    Instant::now(),
                )
            })
    }

    pub(in crate::gateway::engine) fn step_child_persistence(&mut self, now: Instant) {
        let state = &mut self.agent_tools.orchestration;
        if !state.loaded {
            if state.io_busy {
                return;
            }
            if let Some(next) = state.next_load
                && now < next
            {
                self.wake_within(next.saturating_duration_since(now));
                return;
            }
            state.io_busy = true;
            let openers: Vec<_> = self
                .sess
                .managed
                .iter()
                .filter_map(|m| {
                    m.ctl
                        .session_path()
                        .map(|path| (path.to_owned(), m.ctl.target().key.clone()))
                })
                .collect();
            self.spawn_worker("child-registry-load", move || {
                let result = storage().map_or_else(
                    || Err("No per-user data directory".to_owned()),
                    |path| load_registry(&path, &openers),
                );
                Fin::OrchestrationStats(StatsUpdate::Loaded(result))
            });
            return;
        }
        if !state.ready_to_save() {
            return;
        }
        let mut links: Vec<_> = self
            .agent_tools
            .children
            .entries
            .iter()
            .filter_map(|(handle, child)| {
                if child.invalidated {
                    return None;
                }
                Some(Link {
                    handle: handle.clone(),
                    opener_root: child.opener_root.clone(),
                    child_file: child.file.clone()?,
                    title: child.title.clone(),
                    opened_at_unix_ms: child.opened_at_unix_ms,
                    fan_out: child.fan_out.clone(),
                    last_report: child.last_report.clone(),
                    file_seen: child.file_seen && !child.relocation_discovering(now),
                })
            })
            .collect();
        links.sort_by(|a, b| a.handle.cmp(&b.handle));
        if !self.agent_tools.orchestration.force_save
            && links == self.agent_tools.orchestration.saved
            && self
                .agent_tools
                .orchestration
                .next_prune
                .is_some_and(|next| now < next)
        {
            if let Some(next) = self.agent_tools.orchestration.next_prune {
                self.wake_within(next.saturating_duration_since(now));
            }
            return;
        }
        let live: HashSet<_> = self
            .sess
            .managed
            .iter()
            .filter(|m| !m.exited())
            .filter_map(|m| m.ctl.session_path().map(ToOwned::to_owned))
            .chain(
                self.running
                    .latest()
                    .into_iter()
                    .flat_map(|r| r.files.values().flatten().cloned().collect::<Vec<_>>()),
            )
            .collect();
        let mut live = live;
        let now_unix_ms = crate::gateway::unix_ms();
        for (handle, child) in &self.agent_tools.children.entries {
            if self.child_link_live(handle, now_unix_ms)
                && let Some(file) = &child.file
            {
                live.insert(file.clone());
            }
        }
        let identities = links
            .iter()
            .filter_map(|link| {
                self.child_probe_identity(&link.handle, now_unix_ms)
                    .map(|identity| (link.handle.clone(), identity))
            })
            .collect();
        let saved = self.agent_tools.orchestration.saved.clone();
        let force_save = self.agent_tools.orchestration.force_save;
        self.agent_tools.orchestration.io_busy = true;
        self.agent_tools.orchestration.next_prune = Some(now + INTERVAL);
        self.spawn_worker("child-registry-save", move || {
            let mut removed = Vec::new();
            let retained: Vec<_> = links
                .into_iter()
                .filter_map(|mut link| {
                    let exists = link.child_file.is_file();
                    link.file_seen |= exists;
                    let keep = retain_link(
                        exists,
                        link.file_seen,
                        crate::gateway::unix_ms().saturating_sub(link.opened_at_unix_ms),
                        live.contains(&link.child_file),
                    );
                    if keep {
                        Some(link)
                    } else {
                        removed.push(Removed {
                            link,
                            file_exists: exists,
                        });
                        None
                    }
                })
                .collect();
            let result = if !force_save && retained == saved {
                Ok(())
            } else {
                storage().map_or_else(
                    || Err("No per-user data directory".to_owned()),
                    |path| atomic_write(&path, &retained),
                )
            };
            Fin::OrchestrationStats(StatsUpdate::Saved {
                submitted: retained,
                removed,
                identities,
                result,
            })
        });
        self.wake_within(INTERVAL);
    }

    pub(in crate::gateway::engine) fn on_orchestration_stats(&mut self, update: StatsUpdate) {
        match update {
            StatsUpdate::Loaded(result) => {
                self.agent_tools
                    .orchestration
                    .loaded(result.is_ok(), Instant::now());
                match result {
                    Err(e) => {
                        log::warn!("Cannot restore agent children: {e}");
                        self.wake_within(INTERVAL);
                    }
                    Ok(links) => {
                        for Restored {
                            link,
                            cwd,
                            id,
                            opener,
                        } in links
                        {
                            if self.agent_tools.children.entries.contains_key(&link.handle)
                                || self.agent_tools.children.entries.values().any(|c| {
                                    c.file.as_ref() == Some(&link.child_file)
                                        && c.opener_root == link.opener_root
                                })
                            {
                                continue;
                            }
                            let name = deck_core::mux::session_name_for(&id);
                            let backend = self.backends.for_launch(None, self.cfg.session_view);
                            let key = SessionKey::direct();
                            let opener = opener
                                .and_then(|(path, key)| {
                                    self.sess
                                        .managed
                                        .iter()
                                        .find(|m| {
                                            m.ctl.target().key == key
                                                && m.ctl.session_path() == Some(path.as_path())
                                        })
                                        .map(OpenerSource::from_managed)
                                })
                                .unwrap_or(OpenerSource {
                                    key: SessionKey::direct(),
                                    mux_name: None,
                                    backend: None,
                                });
                            self.agent_tools.children.entries.insert(
                                link.handle,
                                ChildSession {
                                    opener_root: link.opener_root,
                                    opener,
                                    key,
                                    file: Some(link.child_file.clone()),
                                    last: None,
                                    background: None,
                                    background_call: None,
                                    forward_questions: false,
                                    questions: QuestionWatch::new(None),
                                    attach: LaunchSpec {
                                        agent_child: true,
                                        mux_name: Some(name),
                                        cwd,
                                        session_path: Some(link.child_file),
                                        label: link.title.clone(),
                                        argv: Vec::new(),
                                        role: SessionRole::Session,
                                        copy_of: None,
                                        view: self.cfg.session_view,
                                    },
                                    backend: backend.map(deck_core::mux::Mux::kind),
                                    attaching: false,
                                    invalidated: false,
                                    reports: ReportRate::new(Instant::now()),
                                    last_report: link.last_report,
                                    title: link.title,
                                    opened_at_unix_ms: link.opened_at_unix_ms,
                                    file_seen: true,
                                    fan_out: link.fan_out,
                                    restored: true,
                                    relocation: None,
                                },
                            );
                        }
                    }
                }
            }
            StatsUpdate::Saved {
                submitted,
                removed,
                identities,
                result,
            } => {
                self.agent_tools.orchestration.io_busy = false;
                for link in &submitted {
                    if link.file_seen
                        && self.child_probe_current(link, identities.get(&link.handle))
                        && let Some(child) = self.agent_tools.children.entries.get_mut(&link.handle)
                    {
                        child.file_seen = true;
                    }
                }
                if let Err(e) = result {
                    log::warn!("Cannot persist agent children: {e}");
                } else {
                    self.agent_tools.orchestration.saved = submitted;
                    self.agent_tools.orchestration.force_save = false;
                }
                for Removed { link, file_exists } in removed {
                    // Only worker observations of the same conversation/liveness may prune.
                    // Resume, attach and relocation discovery require a fresh probe.
                    let unchanged = self.child_probe_current(&link, identities.get(&link.handle));
                    let live = self.child_link_live(&link.handle, crate::gateway::unix_ms());
                    if unchanged && (!live || (link.file_seen && !file_exists)) {
                        if self.agent_tools.child_registry_referenced(&link.handle) {
                            let child = self
                                .agent_tools
                                .children
                                .entries
                                .get_mut(&link.handle)
                                .expect("unchanged child");
                            child.invalidated = true;
                            child.questions.stop();
                            self.agent_tools
                                .orchestration
                                .pending_prunes
                                .insert(link.handle);
                        } else {
                            self.agent_tools.children.entries.remove(&link.handle);
                        }
                    }
                }
            }
            StatsUpdate::Refreshed {
                readers,
                stats,
                repos,
                cwds,
            } => {
                let state = &mut self.agent_tools.orchestration;
                state.stats_busy = false;
                state.readers = readers;
                state.stats = stats;
                state.repos = repos;
                state.cwds = cwds;
            }
        }
        self.waker.now();
    }

    pub(in crate::gateway::engine) fn step_orchestration(&mut self, wanted: bool, now: Instant) {
        if !wanted {
            self.agent_tools.orchestration.next_stats = None;
            return;
        }
        let view = self.orchestration_value();
        if self.agent_tools.orchestration.view_changed(view) {
            self.changed(Topic::Orchestration);
        }
        let state = &mut self.agent_tools.orchestration;
        if state.stats_busy {
            return;
        }
        if let Some(next) = state.next_stats
            && now < next
        {
            self.wake_within(next.saturating_duration_since(now));
            return;
        }
        state.stats_busy = true;
        state.next_stats = Some(now + INTERVAL);
        let mut readers = std::mem::take(&mut state.readers);
        let mut files = HashMap::new();
        for child in self
            .agent_tools
            .children
            .entries
            .values()
            .filter(|c| !c.invalidated)
        {
            files.insert(child.opener_root.clone(), None);
            if let Some(file) = &child.file {
                files.insert(file.clone(), Some(child.attach.cwd.clone()));
            }
        }
        if let Some(groups) = self.sessions_watch.latest() {
            for session in groups.iter().flat_map(|g| &g.sessions) {
                if let Some(cwd) = files.get_mut(&session.path) {
                    *cwd = Some(session.cwd.clone());
                }
            }
        }
        for m in &self.sess.managed {
            if let Some(file) = m.ctl.session_path()
                && let Some(cwd) = files.get_mut(file)
            {
                *cwd = Some(m.current_cwd().to_owned());
            }
        }
        let bases = self.cfg.worktree_bases.clone();
        self.spawn_worker("orchestration-stats", move || {
            readers.retain(|path, _| files.contains_key(path));
            let mut stats = HashMap::new();
            let mut repos = HashMap::new();
            let mut cwds = HashMap::new();
            for (path, cwd) in files {
                let reader = readers
                    .entry(path.clone())
                    .or_insert_with(|| SessionStatsReader::new(path.clone()));
                if reader.refresh().is_ok() {
                    stats.insert(path.clone(), reader.stats().clone());
                }
                let cwd = cwd.or_else(|| header(&path).map(|h| h.0));
                if let Some(cwd) = cwd {
                    cwds.insert(path.clone(), cwd.clone());
                    let repo = deck_core::worktree::repo_info(&cwd)
                        .ok()
                        .filter(|r| r.root != r.primary)
                        .map(|r| OrchWorktree {
                            base: bases.get(&r.root).cloned(),
                            path: r.root,
                            branch: r.current_branch,
                        });
                    repos.insert(path, repo);
                }
            }
            Fin::OrchestrationStats(StatsUpdate::Refreshed {
                readers,
                stats,
                repos,
                cwds,
            })
        });
        self.wake_within(INTERVAL);
    }

    pub(in crate::gateway::engine) fn orchestration_value(&self) -> OrchestrationView {
        let children = &self.agent_tools.children.entries;
        let child_files: HashSet<_> = children
            .values()
            .filter(|c| !c.invalidated)
            .filter_map(|c| c.file.clone())
            .collect();
        let mut roots: Vec<_> = children
            .values()
            .filter(|c| !c.invalidated && !child_files.contains(&c.opener_root))
            .map(|c| c.opener_root.clone())
            .collect();
        roots.sort();
        roots.dedup();
        OrchestrationView {
            roots: roots
                .into_iter()
                .map(|path| self.orchestration_node(&path, None, &mut HashSet::new()))
                .collect(),
        }
    }
    fn orchestration_node(
        &self,
        path: &Path,
        handle: Option<&str>,
        seen: &mut HashSet<PathBuf>,
    ) -> OrchNode {
        let child = handle.and_then(|h| self.agent_tools.children.entries.get(h));
        let managed = self
            .sess
            .managed
            .iter()
            .find(|m| m.ctl.session_path() == Some(path));
        let entry = self.sessions_watch.latest().and_then(|groups| {
            groups
                .iter()
                .flat_map(|g| &g.sessions)
                .find(|s| s.path == path)
                .cloned()
        });
        let stats = self.agent_tools.orchestration.stats.get(path);
        let running = self.running.latest();
        let live = managed.is_some_and(|m| !m.exited())
            || running
                .as_ref()
                .is_some_and(|r| entry.as_ref().is_some_and(|s| r.is_running(&s.id, path)));
        let snapshot = managed
            .and_then(|m| m.live.as_ref())
            .and_then(|l| l.client.latest());
        let activity = managed
            .map(|m| m.ctl.activity())
            .or_else(|| {
                running
                    .as_ref()
                    .and_then(|r| r.holder(path).map(|n| r.activity_of(n)))
            })
            .unwrap_or(Activity::Unknown);
        let mut node = OrchNode {
            handle: handle.map(str::to_owned),
            target: managed.map(|m| m.ctl.target().clone()),
            session_path: path.to_owned(),
            title: managed
                .map(|m| m.display_label())
                .or_else(|| child.map(|c| c.title.clone()))
                .or_else(|| entry.as_ref().map(|e| e.label().to_owned()))
                .or_else(|| stats.and_then(|s| s.title.clone()))
                .unwrap_or_else(|| "(untitled)".to_owned()),
            cwd: managed
                .map(|m| m.current_cwd().to_owned())
                .or_else(|| entry.as_ref().map(|s| s.cwd.clone()))
                .or_else(|| self.agent_tools.orchestration.cwds.get(path).cloned())
                .or_else(|| child.map(|c| c.attach.cwd.clone()))
                .unwrap_or_default(),
            status: managed.map_or_else(
                || TabStatus::of((!live).then_some(0), activity, false),
                |m| m.ctl.status(false),
            ),
            busy: activity == Activity::Busy || managed.is_some_and(|m| m.ctl.is_busy()),
            live,
            model: managed
                .and_then(|m| m.ctl.current_model())
                .or_else(|| stats.and_then(|s| s.model.clone())),
            thinking: snapshot
                .as_ref()
                .and_then(|s| s.model.as_ref())
                .map(|m| m.thinking.clone())
                .or_else(|| stats.and_then(|s| s.thinking.clone())),
            progress: snapshot.and_then(|s| s.progress.clone()),
            last_report: child.and_then(|c| c.last_report.clone()),
            worktree: self
                .agent_tools
                .orchestration
                .repos
                .get(path)
                .cloned()
                .flatten(),
            finish: handle.and_then(|h| self.child_finish_view(h)),
            fan_out: child.and_then(|c| c.fan_out.clone()),
            cost: stats.map(|s| s.cost),
            opened_at_unix_ms: child.map_or(0, |c| c.opened_at_unix_ms),
            children: Vec::new(),
        };
        if seen.insert(path.to_owned()) {
            let mut nested: Vec<_> = self
                .agent_tools
                .children
                .entries
                .iter()
                .filter(|(_, c)| !c.invalidated && c.opener_root == path)
                .filter_map(|(h, c)| {
                    c.file
                        .as_ref()
                        .filter(|p| !seen.contains(*p))
                        .map(|p| (h.as_str(), p.clone(), c.opened_at_unix_ms))
                })
                .collect();
            nested.sort_by(|a, b| a.2.cmp(&b.2).then(a.0.cmp(b.0)));
            for (h, p, _) in nested {
                node.children
                    .push(self.orchestration_node(&p, Some(h), seen));
            }
            seen.remove(path);
        }
        node
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(path: &str) -> OrchNode {
        OrchNode {
            handle: None,
            target: None,
            session_path: path.into(),
            title: path.into(),
            cwd: PathBuf::new(),
            status: TabStatus::Idle,
            busy: false,
            live: false,
            model: None,
            thinking: None,
            progress: None,
            last_report: None,
            worktree: None,
            finish: None,
            fan_out: None,
            cost: None,
            opened_at_unix_ms: 0,
            children: Vec::new(),
        }
    }

    #[test]
    fn publication_tracks_view_changes_without_republishing_unchanged_views() {
        let mut state = State::default();
        let mut view = OrchestrationView::default();
        assert!(state.view_changed(view.clone()));
        assert!(!state.view_changed(view.clone()));
        view.roots.push(node("opener.jsonl"));
        assert!(state.view_changed(view.clone()));
        assert!(!state.view_changed(view.clone()));
        view.roots[0].cost = Some(1.25);
        assert!(state.view_changed(view.clone()));
        assert!(!state.view_changed(view.clone()));
        view.roots[0].children.push(node("child.jsonl"));
        assert!(state.view_changed(view.clone()));
        assert!(!state.view_changed(view.clone()));
        view.roots[0].children[0].last_report = Some(OrchReport {
            kind: "progress".into(),
            text: "Completed the first phase".into(),
            at_unix_ms: 123,
        });
        assert!(state.view_changed(view.clone()));
        assert!(!state.view_changed(view.clone()));
        view.roots[0].children[0].model = Some("provider/model".into());
        assert!(state.view_changed(view.clone()));
        view.roots[0].children[0].thinking = Some("high".into());
        assert!(state.view_changed(view.clone()));
        view.roots[0].children[0].progress = Some(deck_core::ctl::Progress::default());
        assert!(state.view_changed(view.clone()));
        assert!(!state.view_changed(view));
    }

    #[test]
    fn failed_registry_loads_preserve_storage_and_can_recover() {
        let dir = tempfile::tempdir().expect("temporary data directory");
        let path = dir.path().join("agent-children.json");
        let now = Instant::now();
        let mut state = State::default();
        assert!(!state.ready_to_save());

        // A directory fails read on every supported platform, without relying on uid/ACLs.
        std::fs::create_dir(&path).expect("unreadable registry");
        let marker = path.join("keep");
        std::fs::write(&marker, b"original").expect("marker");
        let result = load_registry(&path, &[]);
        assert!(result.is_err());
        state.loaded(result.is_ok(), now);
        if state.ready_to_save() {
            atomic_write(&path, &[]).expect("must not attempt a save");
        }
        assert!(!state.loaded);
        assert!(!state.force_save);
        assert_eq!(state.next_load, Some(now + INTERVAL));
        assert_eq!(
            std::fs::read(&marker).expect("untouched marker"),
            b"original"
        );
        assert_eq!(std::fs::read_dir(dir.path()).expect("directory").count(), 1);

        std::fs::remove_file(marker).expect("remove test marker");
        std::fs::remove_dir(&path).expect("replace test directory");
        let malformed = b"{not valid registry JSON";
        std::fs::write(&path, malformed).expect("malformed registry");
        let result = load_registry(&path, &[]);
        assert!(result.is_err());
        state.loaded(result.is_ok(), now + INTERVAL);
        if state.ready_to_save() {
            atomic_write(&path, &[]).expect("must not attempt a save");
        }
        assert!(!state.ready_to_save());
        assert_eq!(state.next_load, Some(now + INTERVAL + INTERVAL));
        assert_eq!(std::fs::read(&path).expect("untouched registry"), malformed);
        assert_eq!(std::fs::read_dir(dir.path()).expect("directory").count(), 1);

        let child_file = dir.path().join("child.jsonl");
        std::fs::write(
            &child_file,
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "session", "id": "child", "cwd": dir.path()
                })
            ),
        )
        .expect("session header");
        let link = Link {
            handle: "s-recovered".into(),
            opener_root: dir.path().join("opener.jsonl"),
            child_file,
            title: "recovered child".into(),
            opened_at_unix_ms: 123,
            fan_out: None,
            last_report: None,
            file_seen: false,
        };
        std::fs::write(&path, serde_json::to_vec(&[link]).expect("valid registry"))
            .expect("repair registry");
        let result = load_registry(&path, &[]);
        assert!(result.is_ok());
        state.loaded(result.is_ok(), now + INTERVAL + INTERVAL);
        assert!(state.ready_to_save());
        assert!(state.force_save);
        assert_eq!(state.next_load, None);
        let restored = result.expect("recovered registry");
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].link.handle, "s-recovered");
        atomic_write(&path, &[restored.into_iter().next().expect("child").link])
            .expect("save after successful load");
        assert_eq!(load_registry(&path, &[]).expect("saved registry").len(), 1);
    }

    #[test]
    fn empty_and_missing_registries_are_successful_loads() {
        let dir = tempfile::tempdir().expect("temporary data directory");
        let path = dir.path().join("agent-children.json");
        for exists in [false, true] {
            if exists {
                std::fs::write(&path, b"[]").expect("empty registry");
            }
            let result = load_registry(&path, &[]);
            assert!(result.as_ref().is_ok_and(Vec::is_empty));
            let mut state = State::default();
            state.loaded(result.is_ok(), Instant::now());
            assert!(state.ready_to_save());
        }
    }

    #[test]
    fn stale_probes_cannot_prune_replaced_moved_or_resumed_children() {
        let mut child = super::super::tests::child();
        child.file_seen = true;
        let link = Link {
            handle: "s-child".into(),
            opener_root: child.opener_root.clone(),
            child_file: child.file.clone().expect("file"),
            title: child.title.clone(),
            opened_at_unix_ms: child.opened_at_unix_ms,
            fan_out: None,
            last_report: None,
            file_seen: true,
        };
        let original = ProbeIdentity {
            key: child.key.clone(),
            target: None,
            live: false,
            attaching: false,
            file_seen: true,
        };
        let mut current = ProbeIdentity {
            key: child.key.clone(),
            target: None,
            live: false,
            attaching: false,
            file_seen: true,
        };
        let now = Instant::now();
        assert!(link.probe_current(&child, Some(&original), Some(&current), now));
        current.live = true;
        assert!(!link.probe_current(&child, Some(&original), Some(&current), now));
        current.live = false;
        current.key = SessionKey::host("test", "resumed");
        assert!(!link.probe_current(&child, Some(&original), Some(&current), now));
        current.key = child.key.clone();
        child.attaching = true;
        assert!(!link.probe_current(&child, Some(&original), Some(&current), now));
        child.attaching = false;
        child.opened_at_unix_ms += 1;
        assert!(!link.probe_current(&child, Some(&original), Some(&current), now));
        child.opened_at_unix_ms = link.opened_at_unix_ms;
        child.file = Some(PathBuf::from("moved.jsonl"));
        assert!(!link.probe_current(&child, Some(&original), Some(&current), now));
    }

    #[test]
    fn atomic_registry_replacement_keeps_structured_metadata() {
        let dir = tempfile::tempdir().expect("temporary data directory");
        let path = dir.path().join("agent-children.json");
        let link = Link {
            handle: "s-12345678".into(),
            opener_root: dir.path().join("opener.jsonl"),
            child_file: dir.path().join("child.jsonl"),
            title: "child".into(),
            opened_at_unix_ms: 123,
            file_seen: false,
            fan_out: Some("f-123".into()),
            last_report: Some(OrchReport {
                kind: "blocked".into(),
                text: "Need a decision".into(),
                at_unix_ms: 456,
            }),
        };
        atomic_write(&path, std::slice::from_ref(&link)).expect("initial atomic save");
        let restored: Vec<Link> =
            serde_json::from_slice(&std::fs::read(&path).expect("registry")).expect("links");
        assert_eq!(restored, vec![link]);
        atomic_write(&path, &[]).expect("atomic replacement");
        assert_eq!(std::fs::read_dir(dir.path()).expect("directory").count(), 1);
        assert_eq!(std::fs::read_to_string(path).expect("replacement"), "[]\n");
    }

    #[test]
    fn restore_requires_a_real_absolute_session_header() {
        let dir = tempfile::tempdir().expect("temporary session directory");
        let file = dir.path().join("child.jsonl");
        assert!(header(&file).is_none());
        std::fs::write(
            &file,
            "{\"type\":\"session\",\"id\":\"x\",\"cwd\":\"relative\"}\n",
        )
        .expect("invalid header");
        assert!(header(&file).is_none());
        let value = serde_json::json!({"type":"session","id":"child-id","cwd":dir.path()});
        std::fs::write(
            &file,
            format!("{{\"type\":\"title\",\"title\":\"Child\"}}\n{value}\n"),
        )
        .expect("valid header");
        assert_eq!(
            header(&file),
            Some((dir.path().to_owned(), "child-id".into()))
        );
    }
}

#[cfg(test)]
mod pruning_tests {
    use super::*;

    #[test]
    fn an_announced_file_need_not_exist_while_its_child_is_live() {
        assert!(retain_link(false, false, 0, true));
        assert!(retain_link(false, false, MAX_AGE + 1, true));
        assert!(retain_link(true, true, MAX_AGE + 1, true));
    }

    #[test]
    fn materialized_missing_files_are_pruned_even_while_the_child_is_live() {
        assert!(!retain_link(false, true, 0, true));
        assert!(!retain_link(false, true, 0, false));
        assert!(!retain_link(false, false, 0, false));
        assert!(retain_link(true, true, MAX_AGE, false));
        assert!(!retain_link(true, true, MAX_AGE + 1, false));
    }
}
