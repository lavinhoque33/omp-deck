//! App-side glue for deleting subagents from the agents dashboard: which cards may do it, the
//! confirmation bound to the session tab it was asked in, and the `DeleteAgents` command sent to
//! that tab's own control socket (the card need not be the active tab).
//!
//! omp stops the agents and removes their files; the dashboard follows through its own file
//! polling (the fleet watcher re-reads each session every 1–3 s) and, for the active tab, the
//! live snapshot, so the deleted rows disappear on their own.

use std::collections::HashMap;
use std::path::PathBuf;

use deck_core::ctl::{CtlCommand, CtlReply, DeleteTarget};

use super::{App, PendingConfirm};
use crate::agent_manage::{self, Gate, NO_LIVE_CONTROL, OPEN_TO_MANAGE, Plan, Scope};
use crate::dashboard_model::CardKey;
use crate::tabs::{Tab, TabRole};

/// A delete the user was asked to confirm, with the tab it was asked in: it is sent only while
/// that tab, its control key and its session file are unchanged.
pub(super) struct DeleteAsk {
    key: CardKey,
    uid: u64,
    ctl_key: String,
    session: PathBuf,
    targets: Vec<DeleteTarget>,
    /// Clean-up leaves whatever turned out to be running; an explicit delete aborts it.
    keep_running: bool,
}

/// A session tab whose control socket can take commands now.
fn controllable(t: &Tab) -> bool {
    t.role == TabRole::Session
        && !t.terminal().is_exited()
        && t.session_path().is_some()
        && t.info()
            .is_some_and(|i| i.ctl_key.is_some() && i.live_control)
}

impl App {
    /// What each dashboard card's agents can be deleted through, by card.
    pub(super) fn delete_gates(&self) -> HashMap<CardKey, Gate> {
        self.dash
            .cards
            .iter()
            .map(|c| {
                let gate = match &c.key {
                    CardKey::Backend(_) => Gate::Blocked(OPEN_TO_MANAGE),
                    CardKey::Tab(_) if self.dash.deleting.contains(&c.key) => Gate::Busy,
                    CardKey::Tab(uid) => {
                        if self.tabs.iter().any(|t| t.uid == *uid && controllable(t)) {
                            Gate::Ready
                        } else {
                            Gate::Blocked(NO_LIVE_CONTROL)
                        }
                    }
                };
                (c.key.clone(), gate)
            })
            .collect()
    }

    /// Ask before deleting `plan`'s agents of the session card `key` shows.
    pub(super) fn request_delete_agents(
        &mut self,
        key: CardKey,
        session: PathBuf,
        plan: Plan,
        scope: Scope,
    ) {
        let CardKey::Tab(uid) = key else {
            self.toasts.error(&self.ctx, OPEN_TO_MANAGE);
            return;
        };
        let ctl_key = self
            .tabs
            .iter()
            .find(|t| {
                t.uid == uid && controllable(t) && t.session_path() == Some(session.as_path())
            })
            .and_then(|t| t.info().and_then(|i| i.ctl_key.clone()));
        let Some(ctl_key) = ctl_key else {
            self.toasts.error(&self.ctx, NO_LIVE_CONTROL);
            return;
        };
        if plan.is_empty() {
            return;
        }
        let question = agent_manage::confirm(&scope, &plan);
        self.confirm = Some((
            question,
            PendingConfirm::DeleteAgents(DeleteAsk {
                key,
                uid,
                ctl_key,
                session,
                targets: plan.targets,
                keep_running: scope == Scope::CleanUp,
            }),
        ));
    }

    /// The user confirmed: send the delete to the tab it was asked in, if that tab is still the
    /// same live session.
    pub(super) fn confirm_delete_agents(&mut self, ask: DeleteAsk) {
        let DeleteAsk {
            key,
            uid,
            ctl_key,
            session,
            targets,
            keep_running,
        } = ask;
        let target = self
            .tabs
            .iter()
            .find(|t| {
                t.uid == uid
                    && controllable(t)
                    && t.info().and_then(|i| i.ctl_key.as_deref()) == Some(ctl_key.as_str())
                    && t.session_path() == Some(session.as_path())
            })
            .map(|t| t.target().clone());
        let Some(target) = target else {
            self.toasts.error(
                &self.ctx,
                "Delete cancelled: the session it was asked about changed",
            );
            return;
        };
        self.dash.deleting.insert(key.clone());
        let command = CtlCommand::DeleteAgents {
            root: session,
            agents: targets,
            keep_running,
        };
        self.ctl_command_to(target, command, move |app, reply| {
            app.on_delete_reply(&key, reply);
        });
    }

    fn on_delete_reply(&mut self, key: &CardKey, reply: Result<CtlReply, String>) {
        self.dash.deleting.remove(key);
        let verdict = agent_manage::verdict(reply);
        if verdict.ok {
            self.dash.forget_deleted(key, &verdict.deleted);
            self.toasts.success(&self.ctx, verdict.text);
        } else {
            self.toasts.error(&self.ctx, verdict.text);
        }
        self.ctx.request_repaint();
    }
}
