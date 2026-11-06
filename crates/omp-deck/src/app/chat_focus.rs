//! "Open in chat view": a subagent of the agents dashboard is opened in the session's chat view,
//! as omp's own agent hub does (`focus:<id>`, see `chat_view::ChatView::focus_agent`). The tab is
//! opened first when the session has none; the request is handed to the tab's chat view until it
//! reports an outcome, and dropped after [`WAIT`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deck_core::config::SessionView;

use super::App;
use crate::agent_ctl::{self, Link};
use crate::chat_view::FocusOutcome;
use crate::dashboard_model::CardKey;
use crate::tabs::{Tab, TabRole};

/// How long a request may wait for the tab, its chat view and omp before it is given up.
pub(super) const WAIT: Duration = Duration::from_secs(15);

/// While omp's live-control snapshot does not list the agent yet (a freshly opened tab registers
/// its persisted subagents a moment after its composer shows; asking earlier is an "Unknown
/// agent" toast), the request holds back; after this long it asks anyway.
const REGISTRY_GRACE: Duration = Duration::from_secs(5);

/// An "Open in chat view" waiting for its tab.
#[derive(Debug)]
pub(super) struct PendingFocus {
    session: PathBuf,
    agent: String,
    since: Instant,
}

/// The agent the request of `pending` hands to the chat view of `tab`; `ready` = omp can be asked
/// now (see [`App::focus_ready`]).
pub(super) fn request<'a>(
    pending: Option<&'a PendingFocus>,
    tab: &Tab,
    is_active: bool,
    ready: bool,
) -> Option<&'a str> {
    let p = pending?;
    (is_active && ready && tab.session_path() == Some(p.session.as_path()))
        .then_some(p.agent.as_str())
}

impl App {
    /// The view the session opens in: its tab's, else the default for new tabs.
    pub(super) fn session_view_of(&self, session: &Path) -> SessionView {
        self.tabs
            .iter()
            .find(|t| t.role == TabRole::Session && t.session_path() == Some(session))
            .map_or(self.cfg.session_view, Tab::view)
    }

    /// The view each dashboard card's session is open in.
    pub(super) fn dash_views(&self) -> HashMap<CardKey, SessionView> {
        self.dash
            .cards
            .iter()
            .map(|c| {
                let view = c
                    .session
                    .as_deref()
                    .map_or(self.cfg.session_view, |s| self.session_view_of(s));
                (c.key.clone(), view)
            })
            .collect()
    }

    /// Why the subagent `id` of `session` cannot be opened in the chat view, if it cannot.
    pub(super) fn chat_blocked(&self, session: &Path, advisor: bool) -> Option<&'static str> {
        agent_ctl::chat_blocked(advisor, self.session_view_of(session))
    }

    /// Opens the subagent `id` of `session` in the session's chat view: the tab is focused (and
    /// opened first when the session has none), the dashboard and popup close, and omp is asked to
    /// retarget its chat once the tab shows its composer.
    pub(super) fn open_in_chat_view(&mut self, session: &Path, id: &str) {
        let Some(advisor) = self.agent_is_advisor(session, id) else {
            return;
        };
        if let Some(why) = self.chat_blocked(session, advisor) {
            self.toasts.info(&self.ctx, why);
            return;
        }
        self.close_popup_topic();
        let tab = self
            .tabs
            .iter()
            .position(|t| t.role == TabRole::Session && t.session_path() == Some(session));
        match tab {
            Some(i) => self.activate(i),
            None => {
                let key = self
                    .dash
                    .cards
                    .iter()
                    .find(|c| c.session.as_deref() == Some(session))
                    .map(|c| c.key.clone());
                match key {
                    Some(key) => self.open_dash_card(&key),
                    None => {
                        self.toasts
                            .info(&self.ctx, "That session is not running: open it first");
                        return;
                    }
                }
            }
        }
        self.pending_focus = Some(PendingFocus {
            session: session.to_path_buf(),
            agent: id.to_owned(),
            since: Instant::now(),
        });
        self.ctx.request_repaint();
    }

    /// omp can be asked to view the pending agent now: the active tab's control link is not still
    /// connecting and, when it is live, lists the agent (or [`REGISTRY_GRACE`] passed).
    pub(super) fn focus_ready(&self) -> bool {
        let Some(p) = &self.pending_focus else {
            return false;
        };
        match self.ctl_link() {
            Link::Connecting => false,
            Link::Live => {
                p.since.elapsed() >= REGISTRY_GRACE
                    || self
                        .live_snapshot()
                        .is_some_and(|l| l.agents.iter().any(|a| a.id == p.agent))
            }
            Link::ReadOnly | Link::Lost | Link::Ended => true,
        }
    }

    /// The chat view's answer to the pending request.
    pub(super) fn finish_focus(&mut self, outcome: FocusOutcome) {
        self.pending_focus = None;
        if let FocusOutcome::Refused(reason) = outcome {
            self.tsp_refused(reason);
        }
    }

    /// Per frame: drops a request nobody could carry out in time.
    pub(super) fn expire_focus(&mut self) {
        let Some(p) = &self.pending_focus else {
            return;
        };
        if p.since.elapsed() < WAIT {
            // Keep frames coming while it waits for the tab to come up.
            self.ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
        let message = format!("Could not open {} in the chat view", p.agent);
        self.pending_focus = None;
        self.toasts.info(&self.ctx, message);
    }
}
