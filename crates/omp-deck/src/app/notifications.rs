//! App-side glue for desktop notifications: feeds tab states to the trigger rules, sends the
//! notifications, and turns clicks into "activate that tab and raise the window".

use std::time::Instant;

use deck_service::controller::SessionTarget;
use deck_service::notifications::{Effect, NotificationCenter, Observation};
use egui::{UserAttentionType, ViewportCommand};

use super::App;
use crate::notify::{Notifier, Request, Signal};
use crate::notify_panel::{self, PanelAction};
use crate::notify_view::NotifyAction;
use crate::settings_page::OmpDeckPage;
use crate::tabs::{Tab, TabRole};

pub(super) struct NotifyUi {
    notifier: Notifier,
    /// The canonical transition tracker, inbox and turn classifier (shared with every client).
    pub(super) center: NotificationCenter,
    /// Why notifications cannot be shown (once the notifier thread found out).
    pub(super) unavailable: Option<String>,
    window_attached: bool,
    /// The user pressed "Send test notification" and no result came back yet.
    test_pending: bool,
}

impl NotifyUi {
    pub(super) fn new(ctx: &egui::Context) -> NotifyUi {
        NotifyUi {
            notifier: Notifier::start(ctx.clone()),
            center: NotificationCenter::new({
                let ctx = ctx.clone();
                std::sync::Arc::new(move || ctx.request_repaint())
            }),
            unavailable: None,
            window_attached: false,
            test_pending: false,
        }
    }
}

impl App {
    /// Per tick (from `App::logic`, so also while the window is minimized or on another
    /// desktop): show the sessions to the notification center and carry out what it decides:
    /// desktop pop-ups, withdrawals and the tabs' bell markers.
    pub(super) fn drive_notifications(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        if !self.notify.window_attached {
            self.notify.window_attached = true;
            self.notify.notifier.attach_window(frame);
        }
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        let visible = self.visible_uids();
        let projects: Vec<(String, String)> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let t = tab.term().filter(|t| t.role == TabRole::Session)?;
                let project = t
                    .cwd
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_owned();
                Some((tab.label(&self.labels).to_owned(), project))
            })
            .collect();
        let sessions: Vec<Observation<'_>> = self
            .tabs
            .iter()
            .filter_map(Tab::term)
            .filter(|t| t.role == TabRole::Session)
            .zip(&projects)
            .map(|(t, (title, project))| Observation {
                target: t.ctl.target(),
                status: t.ctl.status(false),
                exit_code: t.terminal().exit_code(),
                session: t.session_path(),
                title,
                project,
                visible: visible.contains(&t.uid),
            })
            .collect();
        let ticked = self
            .notify
            .center
            .tick(Instant::now(), &sessions, focused, &self.cfg.notify);
        self.apply_notify_effects(ticked.effects);
        if let Some(d) = ticked.watch_in {
            ctx.request_repaint_after(d);
        }
        for signal in self.notify.notifier.drain() {
            self.on_notify_signal(signal);
        }
    }

    /// Carries out the center's decisions with this client's platform channels.
    fn apply_notify_effects(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Show {
                    target,
                    title,
                    body,
                } => self.notify.notifier.show(Request {
                    tab: Some(target),
                    title,
                    body,
                }),
                Effect::Withdraw(target) => self.notify.notifier.close(Some(target)),
                Effect::WithdrawAll => self.notify.notifier.close_all(),
                Effect::Bell(target) => {
                    if let Some(t) = self
                        .tabs
                        .iter_mut()
                        .filter_map(Tab::term_mut)
                        .find(|t| *t.ctl.target() == target)
                    {
                        t.bell = true;
                    }
                }
            }
        }
    }

    /// The tab showing exactly this session generation, if one is open: a click on a pop-up of a
    /// replaced conversation finds nothing instead of activating the new one.
    fn tab_of_target(&self, target: &SessionTarget) -> Option<usize> {
        self.tabs
            .iter()
            .position(|t| t.term().is_some_and(|t| t.ctl.target() == target))
    }

    fn on_notify_signal(&mut self, signal: Signal) {
        match signal {
            Signal::Clicked { tab, token } => {
                log::debug!(
                    "notification clicked: tab {tab:?}, token {}",
                    token.is_some()
                );
                self.notify.test_pending = false;
                if let Some(target) = tab {
                    self.notify.center.clicked(&target);
                    if let Some(i) = self.tab_of_target(&target) {
                        self.activate(i);
                    }
                }
                self.raise_window(token.as_deref());
            }
            Signal::Gone { tab } => {
                self.notify.test_pending = false;
                if let Some(target) = tab {
                    self.notify.center.gone(&target);
                }
            }
            Signal::Unavailable(why) => {
                if std::mem::take(&mut self.notify.test_pending) {
                    self.toasts
                        .error(&self.ctx, format!("Notifications are unavailable: {why}"));
                }
                self.notify.unavailable = Some(why);
            }
            #[cfg(target_os = "linux")]
            Signal::RaiseFailed => self.raise_attention(),
        }
    }

    /// Bring the window to the front: the compositor's activation token on Wayland, else the
    /// windowing system's focus request.
    fn raise_window(&mut self, token: Option<&str>) {
        self.ctx
            .send_viewport_cmd(ViewportCommand::Minimized(false));
        self.ctx.send_viewport_cmd(ViewportCommand::Focus);
        let tried = token.is_some_and(|t| self.notify.notifier.raise_with_token(t));
        if !tried {
            self.raise_attention();
        }
        self.focus_pending = true;
    }

    /// Windows' focus request is forceful enough; elsewhere ask the shell to flag the window.
    fn raise_attention(&self) {
        if !cfg!(windows) {
            self.ctx
                .send_viewport_cmd(ViewportCommand::RequestUserAttention(
                    UserAttentionType::Informational,
                ));
        }
    }

    pub(super) fn apply_notify_action(&mut self, action: NotifyAction) {
        let cfg = &mut self.cfg.notify;
        match action {
            NotifyAction::Desktop(v) => cfg.desktop = v,
            NotifyAction::TurnFinished(v) => cfg.turn_finished = v,
            NotifyAction::Attention(v) => cfg.attention = v,
            NotifyAction::Exited(v) => cfg.exited = v,
            NotifyAction::OnlyWhenUnfocused(v) => cfg.only_when_unfocused = v,
            NotifyAction::WhileViewing(v) => cfg.while_viewing = v,
            NotifyAction::Test => {
                self.send_test_notification();
                return;
            }
        }
        self.dirty = true;
    }

    /// The inbox popover asked for something.
    pub(super) fn apply_inbox_action(&mut self, action: PanelAction) {
        match action {
            PanelAction::Open(target) => {
                let effects = self.notify.center.open(&target);
                self.apply_notify_effects(effects);
                if let Some(i) = self.tab_of_target(&target) {
                    self.activate(i);
                }
                self.raise_window(None);
            }
            PanelAction::Dismiss(target) => {
                let effects = self.notify.center.dismiss(&target);
                self.apply_notify_effects(effects);
            }
            PanelAction::Settings => {
                self.open_settings_page(OmpDeckPage::Notifications);
            }
            PanelAction::ClearAll => {
                let effects = self.notify.center.clear_all();
                self.apply_notify_effects(effects);
            }
        }
    }

    /// Opens the inbox popover under the bell.
    pub(super) fn open_inbox_panel(&self) {
        egui::Popup::open_id(&self.ctx, notify_panel::popup_id());
    }

    pub(super) fn send_test_notification(&mut self) {
        self.notify.test_pending = true;
        self.notify.notifier.show(Request {
            tab: None,
            title: "OmpDeck test notification".to_owned(),
            body: "Notifications work. Click to bring OmpDeck to the front.".to_owned(),
        });
        self.toasts.info(&self.ctx, "Test notification sent");
    }
}
