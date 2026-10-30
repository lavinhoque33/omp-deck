//! Canonical notifications: the one notification center and inbox (they run with the GUI
//! closed), the desktop adapter that shows the pop-ups, bells to the GUIs and notification
//! clicks (routed to one GUI, or opening the app through a private activation ticket).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Instant, UNIX_EPOCH};

use super::{Engine, Fin};
use crate::controller::SessionTarget;
use crate::gateway::desktop::{self, Notifier, Signal};
use crate::gateway::protocol::{InboxEntry, InboxView, Role, ServerEvent, Topic};
use crate::notifications::{Effect, NotificationCenter, Observation};

pub(super) struct Notify {
    pub(super) center: NotificationCenter,
    pub(super) notifier: Notifier,
    /// The inbox as last published.
    published: Option<InboxView>,
    /// "Notifications are unavailable" was logged.
    unavailable_logged: bool,
}

impl Notify {
    pub(super) fn new(wake: Arc<dyn Fn() + Send + Sync>) -> Notify {
        Notify {
            center: NotificationCenter::new(wake.clone()),
            notifier: Notifier::start(wake),
            published: None,
            unavailable_logged: false,
        }
    }
}

impl Engine {
    pub(super) fn step_notifications(&mut self, now: Instant) {
        self.handle_signals();
        let guis: Vec<_> = self.conns.values().filter(|c| c.is(Role::Gui)).collect();
        // The window focus of any GUI; what the user looks at: the views of the focused GUIs,
        // else of every GUI (none focused: they still show what they show).
        let focused = guis.iter().any(|c| c.state.focused);
        let visible: HashSet<&SessionTarget> = guis
            .iter()
            .filter(|c| c.state.focused || !focused)
            .flat_map(|c| c.state.visible.iter())
            .collect();
        let labels: Vec<(String, String)> = self
            .sess
            .managed
            .iter()
            .map(|m| {
                let project = m
                    .cwd
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                (m.display_label(), project)
            })
            .collect();
        let observations: Vec<Observation<'_>> = self
            .sess
            .managed
            .iter()
            .zip(&labels)
            .map(|(m, (title, project))| {
                let target = m.ctl.target();
                Observation {
                    target,
                    status: m.ctl.status(false),
                    exit_code: m.ctl.terminal().exit_code(),
                    session: m.ctl.session_path(),
                    title,
                    project,
                    visible: visible.contains(target),
                }
            })
            .collect();
        let ticked = self
            .notify
            .center
            .tick(now, &observations, focused, &self.cfg.notify);
        drop(observations);
        if let Some(d) = ticked.watch_in {
            self.wake_within(d);
        }
        self.apply_effects(ticked.effects);
        let view = self.inbox_view();
        if self.notify.published.as_ref() != Some(&view) {
            self.notify.published = Some(view);
            self.changed(Topic::Notifications);
        }
    }

    pub(super) fn apply_effects(&mut self, effects: Vec<Effect>) {
        for e in effects {
            match e {
                Effect::Show {
                    target,
                    title,
                    body,
                    sound,
                } => self.notify.notifier.show(desktop::Request {
                    tab: Some(target),
                    title,
                    body,
                    sound,
                }),
                Effect::Withdraw(t) => self.notify.notifier.close(Some(t)),
                Effect::WithdrawAll => self.notify.notifier.close_all(),
                Effect::Bell(t) => self.to_guis(&ServerEvent::Bell(t)),
            }
        }
    }

    fn handle_signals(&mut self) {
        for signal in self.notify.notifier.drain() {
            match signal {
                Signal::Clicked {
                    tab: Some(t),
                    token,
                } => {
                    self.notify.center.clicked(&t);
                    self.activate(t, token);
                }
                // The sample notification: nothing to open.
                Signal::Clicked { tab: None, .. } | Signal::Gone { tab: None } => {}
                Signal::Gone { tab: Some(t) } => self.notify.center.gone(&t),
                Signal::Unavailable(why) => {
                    if !std::mem::replace(&mut self.notify.unavailable_logged, true) {
                        log::warn!("desktop notifications are unavailable: {why}");
                    }
                }
            }
        }
    }

    /// A click raises the session in one GUI (the focused one, else the most recent); with no
    /// GUI the app opens on it through a private activation ticket.
    fn activate(&mut self, target: SessionTarget, token: Option<String>) {
        let gui = self
            .conns
            .values()
            .filter(|c| c.is(Role::Gui))
            .max_by_key(|c| (c.state.focused, c.seq));
        if let Some(out) = gui.and_then(|c| c.local()) {
            out.event(&ServerEvent::Activate { target, token });
            return;
        }
        // Only a session this owner still runs can be shown.
        let current = self
            .sess
            .find(&target.key)
            .is_some_and(|i| *self.sess.managed[i].ctl.target() == target)
            && target.epoch == self.epoch;
        if !current {
            return;
        }
        let Some(program) = self.context.gui.clone() else {
            log::warn!("cannot open OmpDeck for a notification: the app was not found");
            return;
        };
        let context = self.context.clone();
        self.spawn_worker("activate", move || {
            let result =
                crate::gateway::activation::write_ticket(&context, &target, token.as_deref())
                    .and_then(|ticket| {
                        let mut args: Vec<std::ffi::OsString> =
                            program.args.iter().map(Into::into).collect();
                        args.push("--activate".into());
                        args.push(ticket.into_os_string());
                        deck_host::launch_detached(&program.program, args).map(reap)
                    })
                    .map_err(|e| e.to_string());
            Fin::Activated(result)
        });
    }
}

/// The opened app outlives this request: its exit is collected off the actor (no zombie on
/// Unix; Windows holds only a handle).
fn reap(launcher: deck_host::Launcher) {
    #[cfg(unix)]
    {
        let mut child = launcher;
        let spawned = std::thread::Builder::new()
            .name("gateway-gui-reaper".to_owned())
            .spawn(move || {
                let _ = child.wait();
            });
        if let Err(e) = spawned {
            log::warn!("cannot watch the opened app: {e}");
        }
    }
    #[cfg(not(unix))]
    drop(launcher);
}

impl Engine {
    pub(super) fn inbox_view(&self) -> InboxView {
        let inbox = self.notify.center.inbox();
        InboxView {
            entries: inbox
                .entries()
                .iter()
                .map(|e| InboxEntry {
                    target: e.tab.clone(),
                    kind: e.kind.clone(),
                    title: e.title.clone(),
                    project: e.project.clone(),
                    body: e.body.clone(),
                    at_unix_ms: e
                        .at
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
                    armed: e.armed,
                })
                .collect(),
            warns: inbox.warns(),
            badge: inbox.badge().map(str::to_owned),
        }
    }
}
