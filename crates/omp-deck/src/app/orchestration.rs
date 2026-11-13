//! App-side glue of the dashboard's Orchestration tab: follows `Topic::Orchestration` only while
//! the tab is on screen and carries out what its cards and rows ask for (open, split, finish,
//! stop).

use std::path::Path;

use deck_core::worktree::Worktree;
use deck_service::controller::SessionTarget;
use deck_service::gateway::protocol::{BackendRef, Done, Mutation, Topic, TopicValue};

use super::{App, PendingConfirm};
use crate::confirm::Confirm;
use crate::dashboard::DashTab;
use crate::dashboard::orchestration::{NodeRef, OrchAction};
use crate::panes::Side;

/// Children a "Stop all" confirmation names before "and N more".
const CONFIRM_NAMES: usize = 12;

impl App {
    /// Per frame: subscribe while the tab is shown, rebuild the cards when the tree changed,
    /// and let the gateway stop computing it once the tab is gone.
    pub(super) fn drive_orchestration(&mut self) {
        let shown = self.dash.open && self.dash.tab == DashTab::Orchestration;
        if !shown {
            if self.dash.orch.subscribed {
                self.gw.unsubscribe(&Topic::Orchestration);
                self.dash.orch.clear();
            }
            return;
        }
        if !self.dash.orch.subscribed {
            self.gw.subscribe(Topic::Orchestration);
            self.dash.orch.subscribed = true;
        }
        let rev = self.gw.revision(&Topic::Orchestration);
        if rev == 0 || rev == self.dash.orch.rev {
            return;
        }
        if let Some(TopicValue::Orchestration(view)) = self.gw.topic(&Topic::Orchestration) {
            self.dash.orch.set_view(rev, &view, &self.models);
        }
    }

    pub(super) fn apply_orch_action(&mut self, action: OrchAction) {
        match action {
            OrchAction::Open(node) => self.open_orch_node(&node),
            OrchAction::OpenSplit(node) => self.split_orch_node(&node),
            OrchAction::Finish(path) => match self.known_worktree(&path) {
                Some(wt) => self.request_finish(wt),
                None => self.toasts.info(
                    &self.ctx,
                    "That worktree is not in the sidebar yet; finish it from its session row once it shows.",
                ),
            },
            OrchAction::Stop(node) => match self.backend_name(&node) {
                Some(name) => self.confirm_stop(name, node.title),
                None => self
                    .toasts
                    .info(&self.ctx, "That session is not running anymore."),
            },
            OrchAction::StopAll(nodes) => self.confirm_stop_children(nodes),
        }
    }

    /// The open tab of `node`: its exact session generation, else any tab on its file.
    fn tab_of_node(&self, node: &NodeRef) -> Option<usize> {
        node.target
            .as_ref()
            .and_then(|t| self.tab_of_target(t))
            .or_else(|| {
                self.tabs
                    .iter()
                    .position(|t| t.session_path() == Some(node.session.as_path()))
            })
    }

    /// Focus its tab, or open the session (the dashboard steps aside either way).
    fn open_orch_node(&mut self, node: &NodeRef) {
        if let Some(i) = self.tab_of_node(node) {
            self.activate(i);
        } else if let Some(entry) = self.find_entry(&node.session) {
            self.open_session(&entry);
        } else if let Some(t) = &node.target {
            self.show_session_of(t);
        } else {
            self.toasts
                .info(&self.ctx, "That session's file is not in the sidebar.");
            return;
        }
        self.close_dashboard();
    }

    /// Open it in a pane to the right of the focused one.
    fn split_orch_node(&mut self, node: &NodeRef) {
        if let Some(entry) = self.find_entry(&node.session) {
            self.split_session(&entry, Side::Right);
        } else if let Some(i) = self.tab_of_node(node) {
            self.split_tab(i, Side::Right);
        } else {
            self.toasts
                .info(&self.ctx, "That session's file is not in the sidebar.");
            return;
        }
        self.close_dashboard();
    }

    /// The worktree at `path` as the sidebar knows it (its scan or the gateway's worktree view):
    /// the finish takes exactly the sidebar's path.
    fn known_worktree(&self, path: &Path) -> Option<Worktree> {
        self.groups()
            .and_then(|groups| {
                groups
                    .iter()
                    .filter_map(|g| g.worktree.as_ref())
                    .find(|w| w.path == path)
                    .cloned()
            })
            .or_else(|| self.linked_worktree(path))
    }

    /// The backend session `node` runs in (what a stop kills).
    fn backend_name(&self, node: &NodeRef) -> Option<String> {
        let by_target = |t: &SessionTarget| match self.gw.controller(t).map(|i| i.backend.clone()) {
            Some(BackendRef::Mux { name, .. }) => Some(name),
            _ => None,
        };
        node.target.as_ref().and_then(by_target).or_else(|| {
            let entry = self.find_entry(&node.session)?;
            self.running_session_name(&entry)
        })
    }

    /// "Stop all children…": asks first, naming every session it stops.
    fn confirm_stop_children(&mut self, nodes: Vec<NodeRef>) {
        let kills: Vec<(String, String)> = nodes
            .iter()
            .filter_map(|n| Some((self.backend_name(n)?, n.title.clone())))
            .collect();
        if kills.is_empty() {
            self.toasts
                .info(&self.ctx, "None of those sessions is running anymore.");
            return;
        }
        let mut list: Vec<String> = kills
            .iter()
            .take(CONFIRM_NAMES)
            .map(|(_, title)| format!("• {title}"))
            .collect();
        let more = kills.len().saturating_sub(CONFIRM_NAMES);
        if more > 0 {
            list.push(format!("… and {more} more"));
        }
        let n = kills.len();
        let what = if n == 1 {
            "1 session".to_owned()
        } else {
            format!("{n} sessions")
        };
        self.confirm = Some((
            Confirm {
                title: format!("Stop {what}?"),
                body: format!(
                    "These are terminated and their open tabs close. Their session files stay in the sidebar, so you can resume them later.\n\n{}",
                    list.join("\n")
                ),
                confirm_label: format!("Stop {what}"),
                danger: true,
            },
            PendingConfirm::StopChildren(kills),
        ));
    }

    pub(super) fn execute_stop_children(&mut self, kills: Vec<(String, String)>) {
        let names: Vec<String> = kills.iter().map(|(n, _)| n.clone()).collect();
        let total = names.len();
        self.gw.mutate(
            Mutation::KillNames {
                names: names.clone(),
                wait: false,
            },
            move |app, r| {
                let failed = match r {
                    Ok(Done::Killed(failed)) => failed,
                    Ok(_) => Vec::new(),
                    Err(e) => names.iter().map(|n| (n.clone(), e.clone())).collect(),
                };
                let stopped: Vec<&String> = names
                    .iter()
                    .filter(|n| !failed.iter().any(|(f, _)| f == *n))
                    .collect();
                app.close_tabs_where(|t| {
                    t.mux_name()
                        .is_some_and(|m| stopped.iter().any(|s| s.as_str() == m))
                });
                if failed.is_empty() {
                    let what = if total == 1 {
                        "1 session".to_owned()
                    } else {
                        format!("{total} sessions")
                    };
                    app.toasts.success(&app.ctx, format!("Stopped {what}"));
                } else {
                    let why: Vec<String> = failed
                        .iter()
                        .map(|(n, e)| {
                            let title = kills
                                .iter()
                                .find(|(k, _)| k == n)
                                .map_or(n.as_str(), |(_, t)| t.as_str());
                            format!("“{title}”: {e}")
                        })
                        .collect();
                    app.toasts
                        .error(&app.ctx, format!("Could not stop {}", why.join("; ")));
                }
            },
        );
    }
}
