//! Launching terminal tabs. A session tab first has its control directory prepared (and the
//! session probed) on a worker; the terminal itself is then created on the UI thread (a host
//! session connects on the terminal's own reader thread).
//! Launches are processed one at a time in request order, so restored tabs keep their order.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use deck_core::ctl::{ctl_dir, ctl_env, prepare_ctl_dir, with_ctl};
use deck_core::mux::{Launch, Mux};
use deck_term::{TermOptions, Terminal};

use super::{App, AppEvent, CopyPending, Injector, Tab, TabRole, TermTab, term_theme};
use crate::host_remote::HostRemote;
use crate::omp_worker;
use crate::tabs::Anchor;

/// What to do with the tab once it exists.
pub(super) enum Post {
    None,
    /// `omp --fork`: the tab waits for the copy's session file.
    Copy {
        src_id: String,
        label: String,
        dest: PathBuf,
        started_at: SystemTime,
    },
    /// A restored tab; `active` = it was the focused one.
    Restore {
        active: bool,
    },
    /// The tab takes the strip position next to its former neighbours (a restart, a moved
    /// session).
    Anchor(Anchor),
}

/// Whether the finished tab becomes the active one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Focus {
    /// Always (restored tabs, restarts — they handle focus themselves or replace the active tab).
    Always,
    /// Only while the user has not activated or closed another tab since the launch was
    /// requested (the latest request wins).
    Claim,
    /// Never; the tab opens in the background.
    Never,
}

pub(super) struct SpawnReq {
    pub mux_name: Option<String>,
    pub cwd: PathBuf,
    pub session_path: Option<PathBuf>,
    pub title: String,
    pub argv: Vec<String>,
    pub role: TabRole,
    pub post: Post,
    pub focus: Focus,
}

pub(super) enum Job {
    Spawn {
        req: Box<SpawnReq>,
        /// The activation counter when the request was made (`Focus::Claim` only): the launch
        /// may take focus only while the counter still equals it. Per request, so a newer launch
        /// cannot revalidate an older one the user already overtook.
        claim: Option<u64>,
    },
    /// All restored tabs are queued before this marker: focus the one that was active.
    RestoreDone,
}

/// Session file and tmux name of a launch.
type Slot = (Option<PathBuf>, Option<String>);

/// The launch queue and its one in-flight launch.
#[derive(Default)]
pub(super) struct Pump {
    queue: VecDeque<Job>,
    busy: bool,
    /// Control key of the launch being prepared.
    key: Option<String>,
    /// The launch being prepared (it is in neither the queue nor the tab strip yet).
    inflight: Option<Slot>,
    /// Sessions whose tab was closed for a relaunch that has not been queued yet (a restart
    /// waiting for its stop worker): they must not be opened again meanwhile.
    reserved: Vec<Slot>,
}

impl Pump {
    /// The next job, unless a launch is being prepared.
    fn next(&mut self) -> Option<Job> {
        if self.busy {
            return None;
        }
        self.queue.pop_front()
    }

    /// Mark a launch in flight and run `launch` (which hands it to a worker). If `launch`
    /// fails nothing will ever report back, so the in-flight state is cleared again and the
    /// next job may run.
    fn start(
        &mut self,
        key: Option<String>,
        slot: Slot,
        launch: impl FnOnce() -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        self.busy = true;
        self.key = key;
        self.inflight = Some(slot);
        let result = launch();
        if result.is_err() {
            self.finish();
        }
        result
    }

    /// The in-flight launch is done.
    fn finish(&mut self) {
        self.busy = false;
        self.key = None;
        self.inflight = None;
    }

    pub(super) fn reserve(&mut self, slot: Slot) {
        self.reserved.push(slot);
    }

    pub(super) fn release(&mut self, slot: &Slot) {
        if let Some(i) = self.reserved.iter().position(|s| s == slot) {
            self.reserved.swap_remove(i);
        }
    }

    fn queued_for(&self, path: Option<&std::path::Path>, name: Option<&str>) -> bool {
        let matches = |(p, n): &Slot| {
            (path.is_some() && p.as_deref() == path) || (name.is_some() && n.as_deref() == name)
        };
        self.inflight.iter().any(matches)
            || self.reserved.iter().any(matches)
            || self.queue.iter().any(|j| match j {
                Job::Spawn { req, .. } => {
                    matches(&(req.session_path.clone(), req.mux_name.clone()))
                }
                Job::RestoreDone => false,
            })
    }

    /// Control keys that must survive pruning: the in-flight launch, queued and reserved tmux
    /// names (their sessions may not exist yet).
    fn pending_keys(&self) -> impl Iterator<Item = String> + '_ {
        self.key
            .iter()
            .cloned()
            .chain(self.queue.iter().filter_map(|j| match j {
                Job::Spawn { req, .. } => req.mux_name.clone(),
                Job::RestoreDone => None,
            }))
            .chain(self.reserved.iter().filter_map(|(_, n)| n.clone()))
    }

    /// A launch took focus by bumping the activation counter from `before` to `after`: queued
    /// claims that were still valid follow it; revoked ones stay revoked.
    fn rebase_claims(&mut self, before: u64, after: u64) {
        for j in &mut self.queue {
            if let Job::Spawn { claim, .. } = j
                && *claim == Some(before)
            {
                *claim = Some(after);
            }
        }
    }
}

impl Focus {
    /// Whether a launch may take focus, given the latest claim and the activation counter.
    fn grants(self, claim: Option<u64>, activation_seq: u64) -> bool {
        match self {
            Focus::Always => true,
            Focus::Claim => claim == Some(activation_seq),
            Focus::Never => false,
        }
    }
}

/// The terminal for a resolved launch. A tmux client or direct child runs in a local PTY the GUI
/// owns (Unix); a host session is connected lazily by [`HostRemote`], so nothing here blocks.
fn open_terminal(
    launch: Launch,
    mux: Option<&Mux>,
    opts: TermOptions,
    ctx: &egui::Context,
) -> Result<Terminal, String> {
    match launch {
        Launch::Host(spec) => {
            let Some(Mux::Host(host)) = mux else {
                return Err("the OmpDeck session host is not available".to_owned());
            };
            let remote = HostRemote::new(host.namespace().clone(), spec);
            Ok(Terminal::connect(Box::new(remote), opts, ctx.clone()))
        }
        #[cfg(unix)]
        Launch::Pty(spec) => {
            let spec = deck_term::SpawnSpec {
                program: spec.program,
                args: spec.args,
                cwd: spec.cwd,
                env: spec.env,
                tmux_client: matches!(mux, Some(Mux::Tmux(_))),
            };
            Terminal::spawn(spec, opts, ctx.clone()).map_err(|e| e.to_string())
        }
        #[cfg(not(unix))]
        Launch::Pty(_) => Err("local terminals are not available on this platform".to_owned()),
    }
}

/// Launch without any backend (nothing outlives the app): the command itself in a local PTY.
#[cfg(unix)]
fn direct(
    cwd: &std::path::Path,
    argv: &[String],
    env: Vec<(String, String)>,
) -> Result<Launch, String> {
    let mut spec = deck_core::tmux::direct_launch(cwd, argv);
    spec.env.extend(env);
    Ok(Launch::Pty(spec))
}

#[cfg(not(unix))]
fn direct(
    _cwd: &std::path::Path,
    _argv: &[String],
    _env: Vec<(String, String)>,
) -> Result<Launch, String> {
    Err("the OmpDeck session host (omp-deck-host.exe) was not found".to_owned())
}

/// A request whose control directory was prepared and whose launch command was resolved.
pub(super) struct Prepared {
    req: Box<SpawnReq>,
    /// The request's own focus claim (see [`Job::Spawn`]).
    claim: Option<u64>,
    /// Control key; `None` = launch without control (not wanted, or the directory is unsafe).
    key: Option<String>,
    /// The launch creates the session (an attach ignores the argv).
    created: bool,
    /// Resolved on the worker: the working directory is stat'ed there, not on the UI thread.
    launch: Result<Launch, String>,
    /// The backend that runs the session (`None`: a direct launch).
    mux: Option<Mux>,
}

impl App {
    pub(super) fn spawn_tab_post(&mut self, req: SpawnReq) {
        // The claim belongs to this request: only the user's later tab switches revoke it.
        let claim = (req.focus == Focus::Claim).then_some(self.activation_seq);
        self.spawn.queue.push_back(Job::Spawn {
            req: Box::new(req),
            claim,
        });
        self.sync_ctl_keep();
        self.pump_spawns();
    }

    pub(super) fn queue_restore_done(&mut self) {
        self.spawn.queue.push_back(Job::RestoreDone);
        self.pump_spawns();
    }

    /// A launch for this session file or tmux name is queued, being prepared, or reserved.
    pub(super) fn spawn_queued_for(
        &self,
        path: Option<&std::path::Path>,
        name: Option<&str>,
    ) -> bool {
        self.spawn.queued_for(path, name)
    }

    /// Tell the tmux poller which control dirs must survive pruning: those of open tabs and of
    /// queued launches (their tmux session may not exist yet).
    pub(super) fn sync_ctl_keep(&self) {
        let keep = self
            .tabs
            .iter()
            .filter_map(Tab::term)
            .filter_map(|t| t.ctl_key.clone())
            .chain(self.spawn.pending_keys())
            .collect();
        self.running.set_ctl_keep(keep);
    }

    /// The backend a launch for `name` goes to: the one that holds a live session of that name
    /// (so a session of the other backend is re-attached, never respawned elsewhere), else the
    /// backend new sessions use.
    pub(super) fn mux_for(&self, name: &str) -> Option<Mux> {
        let kind = self.running.latest().and_then(|s| s.owner_of(name));
        let owner: Option<&Mux> = kind.and_then(|k| self.backends.of_kind(k));
        owner.or_else(|| self.backends.primary()).cloned()
    }

    fn pump_spawns(&mut self) {
        while let Some(job) = self.spawn.next() {
            let (mut req, claim) = match job {
                Job::RestoreDone => {
                    self.finish_restore();
                    continue;
                }
                Job::Spawn { req, claim } => (req, claim),
            };
            if req.mux_name.is_none() && !cfg!(unix) {
                // Without a direct PTY every tab, update and installer tabs included, runs in a
                // host session of its own.
                req.mux_name = Some(format!("omp-tool-{}", super::unix_ms()));
            }
            let mux = req.mux_name.as_deref().and_then(|n| self.mux_for(n));
            if mux.is_none() {
                req.mux_name = None;
            }
            let wants_ctl = self.ctl_ext.is_some() && req.role == TabRole::Session;
            let key = wants_ctl.then(|| {
                req.mux_name.clone().unwrap_or_else(|| {
                    self.direct_seq += 1;
                    format!("direct-{}-{}", std::process::id(), self.direct_seq)
                })
            });
            let ext = self.ctl_ext.clone();
            let ns = self.ctl_ns.clone();
            let slot = (req.session_path.clone(), req.mux_name.clone());
            let title = req.title.clone();
            let (ctx, tx) = (self.ctx.clone(), self.events_tx.clone());
            let key_for_state = key.clone();
            let started = self.spawn.start(key_for_state, slot, || {
                omp_worker::try_spawn(&ctx, &tx, move || {
                    let key = key.and_then(|key| match prepare_ctl_dir(&ctl_dir(&ns, &key)) {
                        Ok(()) => Some(key),
                        Err(e) => {
                            log::warn!("live control unavailable for {key}: {e}");
                            None
                        }
                    });
                    let created = match (&mux, &req.mux_name) {
                        (Some(m), Some(name)) => !m.has_session(name),
                        _ => true,
                    };
                    // The environment is part of "control available": without it omp must not
                    // load the extension, and the tab offers the restart-with-control path.
                    let (key, env) = match key {
                        Some(key) => match ctl_env(&ctl_dir(&ns, &key)) {
                            Ok(vars) => {
                                let env: Vec<(String, String)> = vars
                                    .into_iter()
                                    .map(|(k, v)| (k.to_owned(), v.to_string_lossy().into_owned()))
                                    .collect();
                                (Some(key), env)
                            }
                            Err(e) => {
                                log::warn!("live control unavailable for {key}: {e}");
                                (None, Vec::new())
                            }
                        },
                        None => (None, Vec::new()),
                    };
                    let argv = match (&key, &ext) {
                        (Some(_), Some(ext)) => with_ctl(&req.argv, ext),
                        _ => req.argv.clone(),
                    };
                    let launch = match (&mux, &req.mux_name) {
                        (Some(m), Some(name)) => Ok(m.launch_with_env(name, &req.cwd, &argv, &env)),
                        _ => direct(&req.cwd, &argv, env),
                    };
                    AppEvent::SpawnPrepared(Prepared {
                        req,
                        claim,
                        key,
                        created,
                        launch,
                        mux,
                    })
                })
            });
            if let Err(e) = started {
                // The launch never started: nothing will report back, so unwind and go on.
                log::error!("cannot prepare launch of {title:?}: {e}");
                self.toasts
                    .error(&self.ctx, format!("Could not open “{title}”: {e}"));
            }
            self.sync_ctl_keep();
        }
    }

    pub(super) fn on_spawn_prepared(&mut self, p: Prepared) {
        self.spawn.finish();
        self.finish_spawn(p);
        self.pump_spawns();
    }

    fn finish_spawn(&mut self, p: Prepared) {
        let Prepared {
            req,
            claim,
            key,
            created,
            launch,
            mux,
        } = p;
        let SpawnReq {
            mux_name,
            cwd,
            session_path,
            title,
            argv: _,
            role,
            post,
            focus,
        } = *req;
        let opts = TermOptions {
            scrollback: 10_000,
            theme: term_theme(self.cfg.theme, self.cfg.style),
            clipboard: self.clipboard.clone(),
            // ConPTY does not implement the kitty keyboard protocol; the host (not this side)
            // answers `CSI ? u`, so this only selects the key encoder.
            kitty_keyboard: !cfg!(windows),
        };
        let opened = launch.and_then(|l| open_terminal(l, mux.as_ref(), opts, &self.ctx));
        let term = match opened {
            Ok(term) => term,
            Err(e) => {
                log::error!("failed to open terminal: {e}");
                self.toasts
                    .error(&self.ctx, format!("Failed to start terminal: {e}"));
                self.sync_ctl_keep();
                return;
            }
        };
        self.next_tab_uid += 1;
        let uid = self.next_tab_uid;
        self.tabs.push(Tab::Terminal(Box::new(TermTab {
            term,
            mux_name,
            mux,
            cwd,
            session_path,
            fallback_title: title,
            bell: false,
            role,
            exit_handled: false,
            copy: None,
            uid,
            injector: Injector::default(),
            pending_switch: None,
            assumed_switch: None,
            // Attaching to an existing session ignores the argv: only a launch that created the
            // session can be expected to open the socket.
            ctl_expect: key.is_some() && created,
            ctl_key: key,
            ctl_seen: false,
            spawned: Instant::now(),
        })));
        let mut at = self.tabs.len() - 1;
        if let Post::Anchor(anchor) = &post {
            // The tab was appended; put it next to the neighbour it sat beside.
            let uids = self.tab_uids();
            let want = anchor.index(&uids[..at]);
            if want != at {
                let t = self.tabs.remove(at);
                self.tabs.insert(want, t);
                at = want;
            }
        }
        if focus.grants(claim, self.activation_seq) {
            let before = self.activation_seq;
            self.activate(at);
            // Our own activation must not void a later request still waiting to focus.
            self.spawn.rebase_claims(before, self.activation_seq);
        } else if at <= self.active && self.tabs.len() > 1 {
            // Inserted in front of the active tab: it keeps its identity, not its index.
            self.active += 1;
        }
        self.dirty = true;
        self.running.poke();
        self.sync_ctl_keep();
        match post {
            Post::None => {}
            Post::Restore { active } => {
                if active {
                    self.restore_focus = Some(uid);
                }
            }
            Post::Copy {
                src_id,
                label,
                dest,
                started_at,
            } => {
                self.next_copy_id += 1;
                let copy = CopyPending {
                    id: self.next_copy_id,
                    src_id,
                    started_at,
                    started: Instant::now(),
                    next_poll: Instant::now() + super::COPY_POLL,
                    polling: false,
                };
                if let Some(t) = self.tabs.last_mut().and_then(Tab::term_mut) {
                    t.copy = Some(copy);
                }
                self.toasts.info(
                    &self.ctx,
                    format!(
                        "Copying “{label}” to {}…",
                        deck_core::sessions::abbreviate_home(&dest)
                    ),
                );
            }
            Post::Anchor(_) => {}
        }
    }

    fn finish_restore(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        let at = self
            .restore_focus
            .take()
            .and_then(|uid| {
                self.tabs
                    .iter()
                    .position(|t| matches!(t, Tab::Terminal(x) if x.uid == uid))
            })
            .unwrap_or(self.tabs.len() - 1);
        self.activate(at);
    }
}

#[cfg(test)]
mod tests {
    use super::{Focus, Job, PathBuf, Post, Pump, SpawnReq, TabRole};

    #[test]
    fn a_queued_launch_focuses_only_while_the_active_tab_is_unchanged() {
        assert!(Focus::Claim.grants(Some(4), 4));
        assert!(!Focus::Claim.grants(Some(4), 5), "the user switched tabs");
        assert!(!Focus::Claim.grants(None, 0));
    }

    #[test]
    fn restarts_always_focus_and_background_opens_never_do() {
        assert!(Focus::Always.grants(None, 9));
        assert!(!Focus::Never.grants(Some(1), 1));
    }

    fn job(path: &str, claim: Option<u64>) -> Job {
        Job::Spawn {
            req: Box::new(SpawnReq {
                mux_name: None,
                cwd: PathBuf::from("/"),
                session_path: Some(PathBuf::from(path)),
                title: String::new(),
                argv: Vec::new(),
                role: TabRole::Session,
                post: Post::None,
                focus: Focus::Claim,
            }),
            claim,
        }
    }

    fn claim_of(pump: &Pump, i: usize) -> Option<u64> {
        match &pump.queue[i] {
            Job::Spawn { claim, .. } => *claim,
            Job::RestoreDone => None,
        }
    }

    /// The pump loop as `App::pump_spawns` runs it, with `launch` standing in for the worker.
    fn run(pump: &mut Pump, mut launch: impl FnMut(&str) -> std::io::Result<()>) -> Vec<String> {
        let mut started = Vec::new();
        while let Some(job) = pump.next() {
            let Job::Spawn { req, .. } = job else {
                continue;
            };
            let path = req.session_path.clone().unwrap_or_default();
            let name = path.to_string_lossy().into_owned();
            if pump
                .start(None, (Some(path), None), || launch(&name))
                .is_ok()
            {
                started.push(name);
            }
        }
        started
    }

    #[test]
    fn a_failed_worker_start_does_not_stall_the_queue() {
        let mut pump = Pump::default();
        for p in ["/a", "/b", "/c"] {
            pump.queue.push_back(job(p, None));
        }
        let started = run(&mut pump, |p| {
            if p == "/a" {
                Err(std::io::Error::other("no threads"))
            } else {
                Ok(())
            }
        });
        // /a failed and was unwound; /b started and now holds the pump, /c waits behind it.
        assert_eq!(started, ["/b"]);
        assert!(pump.busy);
        assert!(pump.queued_for(Some(std::path::Path::new("/b")), None));
        assert!(!pump.queued_for(Some(std::path::Path::new("/a")), None));
        assert_eq!(pump.queue.len(), 1);
        pump.finish();
        assert_eq!(run(&mut pump, |_| Ok(())), ["/c"]);
    }

    #[test]
    fn a_failed_start_leaves_no_in_flight_state() {
        let mut pump = Pump::default();
        let r = pump.start(Some("k".into()), (None, Some("n".into())), || {
            Err(std::io::Error::other("no threads"))
        });
        assert!(r.is_err());
        assert!(!pump.busy && pump.key.is_none() && pump.inflight.is_none());
        assert!(pump.next().is_none());
    }

    #[test]
    fn focus_claims_are_per_request() {
        let mut pump = Pump::default();
        // Older request claimed at 3 (the user has since moved on to 4); a newer one at 4.
        pump.queue.push_back(job("/old", Some(3)));
        pump.queue.push_back(job("/new", Some(4)));
        let seq = 4;
        assert!(
            !Focus::Claim.grants(claim_of(&pump, 0), seq),
            "the older claim stays void"
        );
        assert!(Focus::Claim.grants(claim_of(&pump, 1), seq));
        // The newer launch activates (4 -> 5): the valid claim follows, the void one does not.
        pump.rebase_claims(4, 5);
        assert_eq!(claim_of(&pump, 0), Some(3));
        assert_eq!(claim_of(&pump, 1), Some(5));
    }

    #[test]
    fn reserved_restarts_count_as_queued_until_released() {
        let mut pump = Pump::default();
        let slot = (Some(PathBuf::from("/s")), Some("omp-s".to_owned()));
        pump.reserve(slot.clone());
        assert!(pump.queued_for(Some(std::path::Path::new("/s")), None));
        assert!(pump.queued_for(None, Some("omp-s")));
        assert_eq!(pump.pending_keys().collect::<Vec<_>>(), ["omp-s"]);
        pump.release(&slot);
        assert!(!pump.queued_for(Some(std::path::Path::new("/s")), None));
    }
}
