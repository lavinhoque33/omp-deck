//! App-side glue for live subagent control: the control client of the active tab, the commands
//! the hub and popup send through it, and "restart with live control" for tabs that were started
//! without the extension.

use std::sync::Arc;
use std::time::Duration;

use deck_core::ctl::{CtlClient, CtlCommand, CtlReply, LiveSnapshot, ctl_dir};
use deck_core::tmux::Tmux;
use std::path::PathBuf;

use super::{App, AppEvent, Focus, PendingConfirm, Post, RestartTarget, SpawnReq};
use crate::agent_ctl::{self, Link, LinkFacts, Req, ReqKind, Restart, RestartFacts};
use crate::confirm::Confirm;
use crate::statusfmt::Activity;
use crate::tabs::{Anchor, ClosedTab, Tab, TabRole};
use deck_core::sessions::SessionEntry;

/// How long a restart waits for tmux to drop the old session before giving up.
const STOP_WAIT: Duration = Duration::from_secs(2);
const STOP_POLL: Duration = Duration::from_millis(50);

/// The control client of the active session tab.
pub(super) struct CtlLink {
    key: String,
    pub(super) client: CtlClient,
}

impl CtlLink {
    pub(super) fn new(key: String, client: CtlClient) -> CtlLink {
        CtlLink { key, client }
    }
}

impl App {
    /// Per frame: follow the active tab's control socket (one client at a time) and settle the
    /// replies that arrived.
    pub(super) fn drive_ctl(&mut self) {
        let want = match self.tabs.get(self.active) {
            Some(Tab::Terminal(t)) if t.role == TabRole::Session => t.ctl_key.as_deref(),
            _ => None,
        };
        if want != self.ctl.as_ref().map(|l| l.key.as_str()) {
            // Requests belong to the old client; their replies will never come.
            self.ctl_reqs.clear();
            self.ctl_errors.clear();
            // A tree request over the old client keeps it until its reply arrives.
            let want = want.map(str::to_owned);
            let old = self.ctl.take();
            self.tree_detach(old);
            self.ctl = want.map(|key| {
                let ctx = self.ctx.clone();
                let client =
                    CtlClient::spawn(ctl_dir(&self.ctl_ns, &key), move || ctx.request_repaint());
                CtlLink { key, client }
            });
        }
        if self.ctl.as_ref().is_some_and(|l| l.client.connected())
            && let Some(Tab::Terminal(t)) = self.tabs.get_mut(self.active)
        {
            t.ctl_seen = true;
        }
        let replies = self
            .ctl
            .as_ref()
            .map(|l| l.client.take_replies())
            .unwrap_or_default();
        for (rid, reply) in replies {
            self.on_ctl_reply(rid, reply);
        }
        self.poll_tree_kept();
        if self.ctl_link() == Link::Connecting {
            // The grace period ends without any other event.
            self.ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    /// omp's own agent states, while the socket of the active tab is up and reports the session
    /// file the tab shows (a snapshot of another root session — `/new`, `/resume`, a stale
    /// socket — must not be overlaid).
    pub(super) fn live_snapshot(&self) -> Option<Arc<LiveSnapshot>> {
        let root = self.ctl_root()?;
        self.ctl
            .as_ref()
            .filter(|l| l.client.connected())
            .and_then(|l| l.client.latest())
            .filter(|s| s.session_file.as_deref() == Some(root))
    }

    /// The session file of the active session tab: the root commands are addressed to.
    pub(super) fn ctl_root(&self) -> Option<&std::path::Path> {
        match self.tabs.get(self.active) {
            Some(Tab::Terminal(t)) if t.role == TabRole::Session => t.session_path.as_deref(),
            _ => None,
        }
    }

    pub(super) fn ctl_link(&self) -> Link {
        let connected = self.ctl.as_ref().is_some_and(|l| l.client.connected());
        let Some(Tab::Terminal(t)) = self.tabs.get(self.active) else {
            return Link::ReadOnly;
        };
        agent_ctl::link_state(&LinkFacts {
            connected,
            ended: t.term.is_exited(),
            seen: t.ctl_seen,
            waited: t.ctl_expect.then(|| t.spawned.elapsed()),
        })
    }

    /// Whether the active tab can be restarted into a controlled session.
    pub(super) fn restart_offer(&self) -> Restart {
        let Some(Tab::Terminal(t)) = self.tabs.get(self.active) else {
            return Restart::Hidden;
        };
        if t.role != TabRole::Session || t.term.is_exited() {
            return Restart::Hidden;
        }
        agent_ctl::restart_offer(&RestartFacts {
            link: self.ctl_link(),
            tmux: self.tmux.is_some() && t.tmux_name.is_some(),
            has_session_file: t.session_path.is_some(),
            extension: self.ctl_ext.is_some(),
            idle: Activity::from_title(t.term.title()) == Activity::Idle,
        })
    }

    // ---------------------------------------------------------------- agent commands

    /// Ask before killing `id`.
    pub(super) fn request_kill_agent(&mut self, id: String) {
        // Bind the question to the tab (and session) it is asked about.
        let Some((uid, key, session)) = (match self.tabs.get(self.active) {
            Some(Tab::Terminal(t)) if t.role == TabRole::Session => t
                .ctl_key
                .clone()
                .map(|key| (t.uid, key, t.session_path.clone())),
            _ => None,
        }) else {
            self.toasts
                .error(&self.ctx, "Live control is not connected for this tab");
            return;
        };
        let running = self
            .agents_view
            .agents()
            .iter()
            .position(|a| a.id == id)
            .and_then(|i| self.agents_view.cards.get(i))
            .is_some_and(|c| c.live == Some(deck_core::ctl::LiveStatus::Running));
        let body = if running {
            format!(
                "“{id}” is working right now. Killing it aborts the step it is on and releases \
                 the agent. Its transcript stays readable."
            )
        } else {
            format!("Release “{id}”? Its transcript stays readable.")
        };
        self.confirm = Some((
            Confirm {
                title: format!("Kill agent “{id}”?"),
                body,
                confirm_label: "Kill agent".to_owned(),
                danger: true,
            },
            PendingConfirm::KillAgent {
                id,
                uid,
                key,
                session,
            },
        ));
    }

    /// The user confirmed a kill: send it through the link of the tab it was asked in, but only
    /// while that tab, its control key and its session file are unchanged.
    pub(super) fn confirm_kill_agent(
        &mut self,
        id: String,
        uid: u64,
        key: &str,
        session: Option<&std::path::Path>,
    ) {
        let same_tab = matches!(
            self.tabs.get(self.active),
            Some(Tab::Terminal(t))
                if t.uid == uid
                    && t.role == TabRole::Session
                    && !t.term.is_exited()
                    && t.ctl_key.as_deref() == Some(key)
                    && t.session_path.as_deref() == session
        );
        if !same_tab || self.ctl.as_ref().is_none_or(|l| l.key != key) {
            self.toasts.error(
                &self.ctx,
                format!("Kill of “{id}” cancelled: the session it was asked about changed"),
            );
            return;
        }
        self.send_ctl(id, ReqKind::Kill, |id, root| CtlCommand::Kill { id, root });
    }

    pub(super) fn send_ctl_revive(&mut self, id: String) {
        self.send_ctl(id, ReqKind::Revive, |id, root| CtlCommand::Revive {
            id,
            root,
        });
    }

    pub(super) fn send_ctl_chat(&mut self, agent: String, text: String) {
        self.send_ctl(agent, ReqKind::Chat, |id, root| CtlCommand::Chat {
            id,
            text,
            root,
        });
    }

    /// Send a command addressed to the active tab's current session file.
    fn send_ctl(
        &mut self,
        agent: String,
        kind: ReqKind,
        build: impl FnOnce(String, PathBuf) -> CtlCommand,
    ) {
        let Some(root) = self.ctl_root().map(std::path::Path::to_path_buf) else {
            self.toasts
                .error(&self.ctx, "Live control is not connected for this tab");
            return;
        };
        let Some(link) = &self.ctl else {
            self.toasts
                .error(&self.ctx, "Live control is not connected for this tab");
            return;
        };
        let cmd = build(agent.clone(), root);
        if kind == ReqKind::Chat {
            self.ctl_errors.remove(&agent);
        }
        let rid = link.client.send(cmd);
        self.ctl_reqs.insert(rid, Req { agent, kind });
    }

    fn on_ctl_reply(&mut self, rid: u64, reply: CtlReply) {
        if self.on_tree_reply(rid, &reply, false) {
            return;
        }
        let Some(Req { agent, kind }) = self.ctl_reqs.remove(&rid) else {
            return;
        };
        if reply.ok {
            match kind {
                ReqKind::Chat => {
                    self.ctl_errors.remove(&agent);
                    if let Some(p) = self.popup.as_mut().filter(|p| p.agent_id == agent) {
                        p.draft.clear();
                    }
                }
                ReqKind::Kill => self.toasts.success(&self.ctx, format!("Killed “{agent}”")),
                ReqKind::Revive => self.toasts.success(&self.ctx, format!("Revived “{agent}”")),
            }
            return;
        }
        let why = reply.error.unwrap_or_else(|| "omp refused".to_owned());
        let what = match kind {
            ReqKind::Kill => "kill",
            ReqKind::Revive => "revive",
            ReqKind::Chat => "message",
        };
        self.toasts
            .error(&self.ctx, format!("Could not {what} “{agent}”: {why}"));
        if kind == ReqKind::Chat {
            self.ctl_errors.insert(agent, why);
        }
    }

    // ---------------------------------------------------------------- restart with control

    pub(super) fn request_restart_live(&mut self) {
        if self.restart_offer() != Restart::Ready {
            return;
        }
        let Some(Tab::Terminal(t)) = self.tabs.get(self.active) else {
            return;
        };
        let Some(session) = t.session_path.clone() else {
            return;
        };
        let (uid, title) = (t.uid, t.title(&self.labels).to_owned());
        self.confirm = Some((
            Confirm {
                title: "Restart with live control?".to_owned(),
                body: format!(
                    "Stops the omp process behind “{title}” and resumes the same session in \
                     this tab with the control extension, so agents can be killed, messaged and \
                     revived from the hub. The conversation is kept."
                ),
                confirm_label: "Restart".to_owned(),
                danger: false,
            },
            PendingConfirm::RestartLive { uid, session },
        ));
    }

    /// Close the tab, stop its tmux session on a worker, then relaunch `--resume` in its place.
    pub(super) fn execute_restart_live(&mut self, uid: u64, session: &std::path::Path) {
        // The question was asked a while ago: the tab must still be the active, idle, read-only
        // session it was about.
        let unchanged = self.restart_offer() == Restart::Ready
            && matches!(
                self.tabs.get(self.active),
                Some(Tab::Terminal(t)) if t.uid == uid && t.session_path.as_deref() == Some(session)
            );
        if !unchanged {
            self.toasts.error(
                &self.ctx,
                "Restart cancelled: the session changed or is busy again",
            );
            return;
        }
        let Some(at) = self
            .tabs
            .iter()
            .position(|t| matches!(t, Tab::Terminal(x) if x.uid == uid))
        else {
            return;
        };
        let Some(tmux) = self.tmux.clone() else {
            return;
        };
        let Some(Tab::Terminal(t)) = self.tabs.get(at) else {
            return;
        };
        let (Some(name), Some(path)) = (t.tmux_name.clone(), t.session_path.clone()) else {
            return;
        };
        let tab = ClosedTab {
            tmux_name: Some(name.clone()),
            cwd: t.cwd.clone(),
            session_path: Some(path.clone()),
            title: t.title(&self.labels).to_owned(),
        };
        let anchor = Anchor::at(&self.tab_uids(), at);
        self.close_tab(at, false);
        self.relaunch_after_stop(tab, tmux, Some(anchor));
    }

    /// Stop `tab`'s tmux session on a worker, then relaunch it with the control extension; the
    /// session is reserved meanwhile so opening it from the sidebar does not start a second copy.
    fn relaunch_after_stop(&mut self, tab: ClosedTab, tmux: Tmux, anchor: Option<Anchor>) {
        let Some(name) = tab.tmux_name.clone() else {
            return;
        };
        let slot = (tab.session_path.clone(), Some(name.clone()));
        self.spawn.reserve(slot);
        let relaunch = tab.clone();
        let started = crate::omp_worker::try_spawn(&self.ctx, &self.events_tx, move || {
            let stopped = stop_session(&tmux, &name);
            AppEvent::RestartReady {
                anchor,
                tab,
                stopped,
            }
        });
        self.running.poke();
        match started {
            Ok(()) => self.toasts.info(&self.ctx, "Restarting session…"),
            // No worker will report back: reattach to the still-running session right away.
            Err(e) => self.on_restart_ready(anchor, relaunch, Err(e.to_string())),
        }
    }

    /// Sidebar "Restart session": ask first (an open tab may be mid-turn).
    pub(super) fn request_restart_session(&mut self, entry: &SessionEntry) {
        if self.tmux.is_none() {
            return;
        }
        let tab = self.tabs.iter().find_map(|t| match t {
            Tab::Terminal(t)
                if t.role == TabRole::Session
                    && !t.term.is_exited()
                    && t.session_path.as_deref() == Some(entry.path.as_path()) =>
            {
                t.tmux_name.clone().map(|n| {
                    let busy = Activity::from_title(t.term.title()) != Activity::Idle;
                    (t.uid, n, busy)
                })
            }
            _ => None,
        });
        let (uid, tmux_name, busy) = match tab {
            Some((uid, name, busy)) => (Some(uid), name, busy),
            None => match self.running_tmux_name(entry) {
                Some(name) => (None, name, false),
                None => {
                    self.toasts
                        .info(&self.ctx, "That session is not running anymore.");
                    return;
                }
            },
        };
        let label = entry.label().to_owned();
        let warn = if busy {
            " omp is working; restarting interrupts the current turn."
        } else {
            ""
        };
        self.confirm = Some((
            Confirm {
                title: "Restart session?".to_owned(),
                body: format!(
                    "Stops the omp process behind “{label}” and resumes the same session with \
                     the control extension. The conversation is kept.{warn}"
                ),
                confirm_label: "Restart".to_owned(),
                danger: busy,
            },
            PendingConfirm::RestartSession(RestartTarget {
                uid,
                tmux_name,
                session: entry.path.clone(),
                cwd: entry.cwd.clone(),
                title: label,
            }),
        ));
    }

    /// The user confirmed a sidebar restart: in place when the session has a tab, else open it
    /// in a new tab once the old tmux session is gone.
    pub(super) fn execute_restart_session(&mut self, target: RestartTarget) {
        let Some(tmux) = self.tmux.clone() else {
            return;
        };
        let at = self.tabs.iter().position(|t| {
            matches!(t, Tab::Terminal(x)
                if x.session_path.as_deref() == Some(target.session.as_path()) && !x.term.is_exited())
        });
        let (tab, anchor) = match (target.uid, at) {
            (Some(uid), Some(at)) if matches!(self.tabs.get(at), Some(Tab::Terminal(t)) if t.uid == uid) =>
            {
                let Some(Tab::Terminal(t)) = self.tabs.get(at) else {
                    return;
                };
                let tab = ClosedTab {
                    tmux_name: t.tmux_name.clone(),
                    cwd: t.cwd.clone(),
                    session_path: Some(target.session),
                    title: t.title(&self.labels).to_owned(),
                };
                let anchor = Anchor::at(&self.tab_uids(), at);
                self.close_tab(at, false);
                (tab, Some(anchor))
            }
            (None, None) => (
                ClosedTab {
                    tmux_name: Some(target.tmux_name),
                    cwd: target.cwd,
                    session_path: Some(target.session),
                    title: target.title,
                },
                None,
            ),
            _ => {
                self.toasts
                    .error(&self.ctx, "Restart cancelled: the session changed");
                return;
            }
        };
        self.relaunch_after_stop(tab, tmux, anchor);
    }

    pub(super) fn on_restart_ready(
        &mut self,
        anchor: Option<Anchor>,
        tab: ClosedTab,
        stopped: Result<(), String>,
    ) {
        self.spawn
            .release(&(tab.session_path.clone(), tab.tmux_name.clone()));
        if let Err(e) = stopped {
            // The old process still runs: attaching shows it again, without control.
            self.toasts
                .error(&self.ctx, format!("Could not restart: {e}"));
        }
        let argv = crate::tabs::omp_argv(tab.session_path.as_deref());
        self.spawn_tab_post(SpawnReq {
            tmux_name: tab.tmux_name,
            cwd: tab.cwd,
            session_path: tab.session_path,
            title: tab.title,
            argv,
            role: TabRole::Session,
            post: anchor.map_or(Post::None, Post::Anchor),
            focus: Focus::Always,
        });
    }
}

/// `kill-session`, then wait until tmux no longer lists it.
fn stop_session(tmux: &Tmux, name: &str) -> Result<(), String> {
    if let Err(e) = tmux.kill(name) {
        log::warn!("restart: kill {name}: {e}");
    }
    let mut waited = Duration::ZERO;
    while tmux.has_session(name) {
        if waited >= STOP_WAIT {
            return Err(format!("tmux session {name} did not stop"));
        }
        std::thread::sleep(STOP_POLL);
        waited += STOP_POLL;
    }
    Ok(())
}
