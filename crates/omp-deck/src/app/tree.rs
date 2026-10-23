//! App-side glue for the session tree window: the watcher that reads the session file, the
//! gates for "Go here" / "Fork", and the two operations — through the control socket when the
//! active tab has live control, headless `omp --mode rpc` otherwise.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use deck_core::ctl::{CtlClient, CtlCommand, CtlReply, ctl_dir};
use deck_core::mux::{new_session_name, session_name_for};
use deck_core::tree::TreeWatcher;
use egui::{Key, Modifiers};

use super::ctl::CtlLink;
use super::spawn::Focus;
use super::{App, AppEvent};
use crate::agent_ctl::Link;
use crate::statusfmt::Activity;
use crate::tabs::{ClosedTab, Tab, TabRole};
use crate::titlebar;
use crate::toast::ToastKind;
use crate::tree_view::{self, TreeAction, TreeInput, TreeState};

/// The open tree window.
pub(super) struct TreeUi {
    state: TreeState,
    watcher: TreeWatcher,
    /// The session file shown.
    session: PathBuf,
    /// Working directory a forked tab starts in.
    cwd: PathBuf,
    /// The session id (running checks against tmux names).
    id: Option<String>,
    title: String,
}

/// What the open tabs say about the session being forked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TabFact {
    /// No live tab shows the file.
    None,
    /// The active tab shows it with a live control link.
    ActiveLive,
    /// A tab with a control socket shows it, but it is not the active one.
    Background,
    /// A tab shows it without usable live control.
    NoControl,
}

/// What the tmux snapshot says about the session.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RunFact {
    /// No snapshot yet.
    Unknown,
    /// tmux cannot be polled; nothing can be known.
    Blind,
    /// A tmux session runs it; `control` = its control socket exists.
    Running {
        control: bool,
    },
    Stopped,
}

struct ForkFacts {
    busy: bool,
    tab: TabFact,
    omp: bool,
    run: RunFact,
}

const RUNNING: &str = "Session is running in the background without live control — open it and restart with live control to fork";

/// How long a background fork waits for the control socket before giving up.
const BG_CONNECT: Duration = Duration::from_secs(3);

/// How a fork must run: over the live link of its tab, or headless when nothing runs the file
/// (two omp processes on one file are unsafe).
fn fork_route(f: &ForkFacts) -> Result<ForkRoute, &'static str> {
    if f.busy {
        return Err("Another tree action is still running");
    }
    match f.tab {
        TabFact::ActiveLive => return Ok(ForkRoute::Live),
        TabFact::Background => return Err("Switch to this session's tab to fork it"),
        TabFact::NoControl => return Err(RUNNING),
        TabFact::None => {}
    }
    if !f.omp {
        return Err("omp was not found");
    }
    match f.run {
        RunFact::Running { control: true } => Ok(ForkRoute::Background),
        RunFact::Running { control: false } => Err(RUNNING),
        RunFact::Stopped | RunFact::Blind => Ok(ForkRoute::Headless),
        RunFact::Unknown => Err("Still checking which sessions are running"),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ForkRoute {
    Live,
    /// Over a short-lived client of the (closed-tab) tmux session's control socket.
    Background,
    Headless,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TreeOp {
    Fork,
    Navigate { user: bool },
}

/// The one operation in flight (a second one is refused until it settles).
pub(super) struct TreeBusy {
    /// Control request id; `None` = a headless worker runs it.
    rid: Option<u64>,
    op: TreeOp,
    cwd: PathBuf,
    title: String,
    what: String,
    /// The control client the request went over, when its tab stopped being the active one:
    /// the extension still performs the request, so the reply is awaited here.
    keep: Option<CtlLink>,
    /// Background fork waiting for its client to connect: the entry to fork and since when.
    bg: Option<(String, PathBuf, Instant)>,
    /// `activation_seq` when requested: the result may take focus only if still unchanged.
    seq: u64,
}

impl TreeBusy {
    /// Waiting for a control reply (not a headless worker).
    pub(super) fn over_ctl(&self) -> bool {
        self.rid.is_some()
    }
}

impl App {
    /// Open the tree of `session` (file-based; works for any session).
    pub(super) fn open_tree(
        &mut self,
        session: PathBuf,
        cwd: PathBuf,
        title: String,
        id: Option<String>,
    ) {
        let ctx = self.ctx.clone();
        let watcher = TreeWatcher::spawn(session.clone(), move || ctx.request_repaint());
        self.tree = Some(TreeUi {
            state: TreeState::new(title.clone()),
            watcher,
            session,
            cwd,
            id,
            title,
        });
        self.ctx.request_repaint();
    }

    pub(super) fn close_tree(&mut self) {
        self.tree = None;
        self.focus_pending = true;
    }

    /// The tree of the active session tab.
    pub(super) fn open_tree_active(&mut self) {
        let Some(Tab::Terminal(t)) = self.tabs.get(self.active) else {
            return;
        };
        if t.role != TabRole::Session {
            return;
        }
        let Some(session) = t.session_path.clone() else {
            self.toasts.info(
                &self.ctx,
                "The session has no file yet — send a prompt first",
            );
            return;
        };
        let cwd = t.cwd.clone();
        let title = titlebar::strip_omp_prefix(t.title(&self.labels)).to_owned();
        let id = self.watcher.latest().and_then(|groups| {
            groups
                .iter()
                .flat_map(|g| &g.sessions)
                .find(|e| e.path == session)
                .map(|e| e.id.clone())
        });
        self.open_tree(session, cwd, title, id);
    }

    /// Ctrl+Shift+B: same gating as the agent hub.
    pub(super) fn toggle_tree(&mut self) {
        if self.tree.is_some() {
            self.close_tree();
        } else if self.active_session_tab() {
            self.open_tree_active();
        }
    }

    /// Esc is handled inside the window; it only needs to know a menu is not open.
    pub(super) fn tree_shortcut(&mut self, ctx: &egui::Context) -> bool {
        ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::B))
    }

    /// Whether a live (not exited) tab shows `session`.
    fn tree_has_tab(&self, session: &Path) -> bool {
        self.tabs
            .iter()
            .filter_map(Tab::term)
            .any(|t| t.session_path.as_deref() == Some(session) && !t.term.is_exited())
    }

    /// The active tab shows `session` and has a live control link.
    fn tree_link(&self, session: &Path) -> Result<(), &'static str> {
        let active = match self.tabs.get(self.active) {
            Some(Tab::Terminal(t))
                if t.role == TabRole::Session && t.session_path.as_deref() == Some(session) =>
            {
                t
            }
            _ if self.tree_has_tab(session) => {
                return Err("Switch to this session's tab to use Go here");
            }
            _ => return Err("Open the session in a tab with live control first"),
        };
        if active.term.is_exited() {
            return Err("omp has exited in this tab");
        }
        if self.ctl_link() != Link::Live {
            return Err(
                "Live control is not connected for this tab (restart it from the Agents panel)",
            );
        }
        Ok(())
    }

    fn tree_navigate_gate(&self, session: &Path) -> Result<(), &'static str> {
        self.tree_link(session)?;
        if self.tree_busy.is_some() {
            return Err("Another tree action is still running");
        }
        let idle = self
            .live_snapshot()
            .and_then(|s| s.main_idle)
            .or_else(|| {
                self.tabs
                    .get(self.active)
                    .and_then(Tab::term)
                    .map(|t| Activity::from_title(t.term.title()) == Activity::Idle)
            })
            .unwrap_or(false);
        if idle {
            Ok(())
        } else {
            Err("omp is busy — wait for it to finish")
        }
    }

    /// How a fork of `session` must run (see [`fork_route`]).
    fn tree_fork_route(&self, session: &Path) -> Result<ForkRoute, &'static str> {
        let tab = self
            .tabs
            .iter()
            .enumerate()
            .filter_map(|(i, t)| t.term().map(|t| (i, t)))
            .find(|(_, t)| t.session_path.as_deref() == Some(session) && !t.term.is_exited());
        let tab = match tab {
            None => TabFact::None,
            Some((i, _)) if i == self.active && self.tree_link(session).is_ok() => {
                TabFact::ActiveLive
            }
            Some((i, t)) if i != self.active && t.ctl_key.is_some() => TabFact::Background,
            Some(_) => TabFact::NoControl,
        };
        let run = match (self.running.latest(), &self.tree) {
            (Some(snap), ui) => {
                let id = ui.as_ref().and_then(|u| u.id.as_deref());
                if snap.holder(session).is_some()
                    || id.is_some_and(|id| snap.is_running(id, session))
                {
                    RunFact::Running {
                        control: self.tree_bg_key(session).is_some(),
                    }
                } else {
                    RunFact::Stopped
                }
            }
            (None, _) if self.running.is_blind() => RunFact::Blind,
            (None, _) => RunFact::Unknown,
        };
        fork_route(&ForkFacts {
            busy: self.tree_busy.is_some(),
            tab,
            omp: self.omp.is_some(),
            run,
        })
    }

    /// The session running `session` in a backend while no tab shows it, when its control
    /// endpoint exists.
    fn tree_bg_key(&self, session: &Path) -> Option<String> {
        let snap = self.running.latest()?;
        let name = snap.holder(session).map(str::to_owned).or_else(|| {
            let id = self.tree.as_ref()?.id.as_deref()?;
            let canonical = session_name_for(id);
            (snap.is_alive(&canonical) && snap.files_of(&canonical).is_empty()).then_some(canonical)
        })?;
        ctl_endpoint_exists(&ctl_dir(&self.ctl_ns, &name)).then_some(name)
    }

    fn tree_fork_gate(&self, session: &Path) -> Result<(), &'static str> {
        self.tree_fork_route(session).map(drop)
    }

    /// Per frame: draw the window and run what it asks for.
    pub(super) fn show_tree(&mut self, ctx: &egui::Context) {
        let Some(session) = self.tree.as_ref().map(|t| t.session.clone()) else {
            return;
        };
        let navigate = self.tree_navigate_gate(&session);
        let fork = self.tree_fork_gate(&session);
        let live = self
            .live_snapshot()
            .filter(|_| self.ctl_root() == Some(session.as_path()));
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        let Some(ui_state) = self.tree.as_mut() else {
            return;
        };
        let tree = ui_state.watcher.latest();
        let error = ui_state.watcher.error();
        let actions = tree_view::show(
            ctx,
            &mut ui_state.state,
            &TreeInput {
                tree: tree.as_ref(),
                revision: ui_state.watcher.revision(),
                error: error.as_deref(),
                live_leaf: live.as_ref().and_then(|s| s.leaf.as_deref()),
                navigate,
                fork,
                now,
            },
        );
        for a in actions {
            match a {
                TreeAction::Close => self.close_tree(),
                TreeAction::Fork { entry, what } => self.tree_fork(&session, entry, what),
                TreeAction::Navigate { entry, user } => self.tree_navigate(&session, entry, user),
                TreeAction::Copy { text, what } => self.copy_text(&text, what),
                TreeAction::Notice(text) => self.toasts.info(&self.ctx, text),
            }
        }
    }

    fn tree_fork(&mut self, session: &Path, entry: String, what: String) {
        let Some((cwd, title)) = self.tree.as_ref().map(|t| (t.cwd.clone(), t.title.clone()))
        else {
            return;
        };
        if self.tree_busy.is_some() {
            return;
        }
        let mut busy = TreeBusy {
            rid: None,
            op: TreeOp::Fork,
            cwd,
            title,
            what,
            keep: None,
            bg: None,
            seq: self.activation_seq,
        };
        let Ok(route) = self.tree_fork_route(session) else {
            return;
        };
        if route == ForkRoute::Live
            && let Some(link) = &self.ctl
        {
            busy.rid = Some(link.client.send(CtlCommand::ForkAt {
                root: session.to_path_buf(),
                entry_id: entry,
            }));
            self.tree_busy = Some(busy);
            self.toasts.info(&self.ctx, "Forking…");
            return;
        }
        if route == ForkRoute::Background {
            let Some(key) = self.tree_bg_key(session) else {
                return;
            };
            let ctx = self.ctx.clone();
            busy.keep = Some(CtlLink::new(
                key.clone(),
                CtlClient::spawn(ctl_dir(&self.ctl_ns, &key), move || ctx.request_repaint()),
            ));
            busy.bg = Some((entry, session.to_path_buf(), Instant::now()));
            self.tree_busy = Some(busy);
            self.toasts.info(&self.ctx, "Forking…");
            return;
        }
        let Some(omp) = self.omp.clone() else {
            return;
        };
        let file = session.to_path_buf();
        let started = crate::omp_worker::try_spawn(&self.ctx, &self.events_tx, move || {
            AppEvent::TreeForked(omp.fork_at(&file, &entry).map_err(|e| e.to_string()))
        });
        match started {
            Ok(()) => {
                self.tree_busy = Some(busy);
                self.toasts.info(&self.ctx, "Forking…");
            }
            Err(e) => self
                .toasts
                .error(&self.ctx, format!("Could not start the fork: {e}")),
        }
    }

    fn tree_navigate(&mut self, session: &Path, entry: String, user: bool) {
        if self.tree_navigate_gate(session).is_err() {
            return;
        }
        let Some(link) = &self.ctl else {
            return;
        };
        let rid = link.client.send(CtlCommand::Navigate {
            root: session.to_path_buf(),
            entry_id: entry,
        });
        self.tree_busy = Some(TreeBusy {
            rid: Some(rid),
            op: TreeOp::Navigate { user },
            cwd: PathBuf::new(),
            title: String::new(),
            what: String::new(),
            keep: None,
            bg: None,
            seq: self.activation_seq,
        });
    }

    /// The active tab's client is about to change: a tree request in flight over it keeps its
    /// client (the extension performs the request whatever tab is shown).
    pub(super) fn tree_detach(&mut self, old: Option<CtlLink>) {
        if let Some(b) = self
            .tree_busy
            .as_mut()
            .filter(|b| b.over_ctl() && b.keep.is_none())
        {
            b.keep = old;
        }
    }

    /// A background fork sends its request once its client is connected, or gives up.
    fn poll_tree_bg(&mut self) {
        let Some(busy) = self.tree_busy.as_mut() else {
            return;
        };
        let (Some((entry, root, since)), Some(link)) = (busy.bg.as_ref(), busy.keep.as_ref())
        else {
            return;
        };
        if link.client.connected() {
            let rid = link.client.send(CtlCommand::ForkAt {
                root: root.clone(),
                entry_id: entry.clone(),
            });
            busy.rid = Some(rid);
            busy.bg = None;
        } else if since.elapsed() > BG_CONNECT {
            self.tree_busy = None;
            self.tree_failed(
                TreeOp::Fork,
                "could not reach the running session's control socket",
            );
        } else {
            self.ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// Per frame: replies (or a lost link) of the client a tree request was left on.
    pub(super) fn poll_tree_kept(&mut self) {
        self.poll_tree_bg();
        let Some((replies, connected)) = self
            .tree_busy
            .as_ref()
            .and_then(|b| b.keep.as_ref())
            .map(|l| (l.client.take_replies(), l.client.connected()))
        else {
            return;
        };
        for (rid, reply) in replies {
            self.on_tree_reply(rid, &reply, true);
        }
        if !connected
            && let Some(busy) = self
                .tree_busy
                .take_if(|b| b.keep.is_some() && b.rid.is_some())
        {
            self.tree_failed(busy.op, "the control link to that tab was lost");
        }
    }

    /// A control reply: true when it settled the tree operation. `kept` = it came from the
    /// client left behind by a tab switch rather than the active one (request ids are per client).
    pub(super) fn on_tree_reply(&mut self, rid: u64, reply: &CtlReply, kept: bool) -> bool {
        if self
            .tree_busy
            .as_ref()
            .is_none_or(|b| b.rid != Some(rid) || b.keep.is_some() != kept)
        {
            return false;
        }
        let Some(busy) = self.tree_busy.take() else {
            return false;
        };
        if !reply.ok {
            let why = reply
                .error
                .clone()
                .unwrap_or_else(|| "omp refused".to_owned());
            self.tree_failed(busy.op, &why);
            return true;
        }
        match busy.op {
            TreeOp::Fork => match &reply.new_file {
                Some(file) => self.tree_forked(&busy, file.clone()),
                None => self.tree_failed(busy.op, "omp returned no file"),
            },
            TreeOp::Navigate { user } => {
                self.close_tree();
                self.toasts.success(
                    &self.ctx,
                    if user {
                        "Moved before that prompt — its text is back in the editor"
                    } else {
                        "Moved there — new prompts continue from this point"
                    },
                );
            }
        }
        true
    }

    /// The headless fork finished.
    pub(super) fn on_tree_forked(&mut self, result: Result<PathBuf, String>) {
        let Some(busy) = self.tree_busy.take() else {
            return;
        };
        match result {
            Ok(file) => self.tree_forked(&busy, file),
            Err(e) => self.tree_failed(TreeOp::Fork, &e),
        }
    }

    fn tree_forked(&mut self, busy: &TreeBusy, file: PathBuf) {
        self.close_tree();
        let name = self
            .backends
            .primary()
            .is_some()
            .then(|| new_session_name(SystemTime::now()));
        // Like every queued launch: focus only while the user has not switched tabs since asking.
        let focus = if busy.seq == self.activation_seq {
            Focus::Claim
        } else {
            Focus::Never
        };
        self.open_resolved_focus(
            ClosedTab {
                mux_name: name,
                cwd: busy.cwd.clone(),
                session_path: Some(file),
                title: format!("{} (fork)", busy.title),
            },
            focus,
        );
        self.toasts
            .success(&self.ctx, format!("{} — opened in a new tab", busy.what));
        self.refresh_sessions();
    }

    fn tree_failed(&mut self, op: TreeOp, why: &str) {
        let what = match op {
            TreeOp::Fork => "fork",
            TreeOp::Navigate { .. } => "go there",
        };
        let text = if why.eq_ignore_ascii_case("busy") {
            "omp is busy — try again when it is idle".to_owned()
        } else {
            why.to_owned()
        };
        self.toasts.notify(
            &self.ctx,
            ToastKind::Error,
            format!("Could not {what}: {text}"),
        );
    }
}

/// Whether a tab's control endpoint is up: its `ctl.sock` (Unix) or its named pipe (Windows,
/// where the pipe name is the endpoint itself).
fn ctl_endpoint_exists(dir: &Path) -> bool {
    if cfg!(windows) {
        dir.exists()
    } else {
        dir.join("ctl.sock").exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(tab: TabFact, run: RunFact) -> ForkFacts {
        ForkFacts {
            busy: false,
            tab,
            omp: true,
            run,
        }
    }

    #[test]
    fn closed_tab_with_a_live_session_needs_its_control_socket_to_fork() {
        assert_eq!(
            fork_route(&facts(TabFact::None, RunFact::Running { control: false })),
            Err(RUNNING)
        );
        assert_eq!(
            fork_route(&facts(TabFact::None, RunFact::Running { control: true })),
            Ok(ForkRoute::Background)
        );
    }

    #[test]
    fn idle_closed_session_forks_headless() {
        assert_eq!(
            fork_route(&facts(TabFact::None, RunFact::Stopped)),
            Ok(ForkRoute::Headless)
        );
        assert_eq!(
            fork_route(&facts(TabFact::None, RunFact::Blind)),
            Ok(ForkRoute::Headless)
        );
        assert!(fork_route(&facts(TabFact::None, RunFact::Unknown)).is_err());
    }

    #[test]
    fn open_tabs_decide_the_route() {
        let run = RunFact::Stopped;
        assert_eq!(
            fork_route(&facts(TabFact::ActiveLive, run)),
            Ok(ForkRoute::Live)
        );
        assert!(fork_route(&facts(TabFact::Background, run)).is_err());
        assert_eq!(fork_route(&facts(TabFact::NoControl, run)), Err(RUNNING));
    }

    #[test]
    fn busy_and_missing_omp_refuse() {
        let mut f = facts(TabFact::ActiveLive, RunFact::Stopped);
        f.busy = true;
        assert!(fork_route(&f).is_err());
        let mut f = facts(TabFact::None, RunFact::Stopped);
        f.omp = false;
        assert!(fork_route(&f).is_err());
    }
}
