//! App-side glue for desktop notifications: feeds tab states to the trigger rules, sends the
//! notifications, and turns clicks into "activate that tab and raise the window".

use std::collections::HashSet;
use std::time::{Duration, Instant};

use egui::{UserAttentionType, ViewportCommand};

use super::App;
use crate::notify::tracker::{self, Sighting, Tracker};
use crate::notify::{Notifier, Request, Signal};
use crate::notify_view::NotifyAction;
use crate::statusfmt::Activity;
use crate::tabs::{Tab, TabRole, TabStatus};

/// While a tab is busy the app repaints at least this often, so the end of its turn is seen even
/// when the window is in the background.
const BUSY_WATCH: Duration = Duration::from_secs(1);

pub(super) struct NotifyUi {
    notifier: Notifier,
    tracker: Tracker,
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
            tracker: Tracker::default(),
            unavailable: None,
            window_attached: false,
            test_pending: false,
        }
    }
}

impl App {
    /// Per frame: watch the tabs, send due notifications, withdraw the ones the user caught up on,
    /// and act on clicks.
    pub(super) fn drive_notifications(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        if !self.notify.window_attached {
            self.notify.window_attached = true;
            self.notify.notifier.attach_window(frame);
        }
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        let now = Instant::now();
        let visible = self.visible_uids();
        let cfg = &self.cfg.notify;
        let mut live = HashSet::new();
        let mut due = Vec::new();
        for tab in &self.tabs {
            let Tab::Terminal(t) = tab else { continue };
            if t.role != TabRole::Session {
                continue;
            }
            live.insert(t.uid);
            let code = t.term.exit_code();
            let seen = Sighting {
                status: TabStatus::of(code, Activity::from_title(t.term.title()), false),
                exit_code: code,
                visible: visible.contains(&t.uid),
            };
            if let Some(event) = self.notify.tracker.observe(t.uid, now, seen, focused, cfg) {
                let project = t.cwd.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let (title, body) = tracker::describe(event, tab.label(&self.labels), project);
                due.push(Request {
                    tab: Some(t.uid),
                    title,
                    body,
                });
            }
        }
        for uid in self.notify.tracker.retain(&live) {
            self.notify.notifier.close(Some(uid));
        }
        for req in due {
            if let Some(uid) = req.tab {
                self.notify.tracker.shown(uid);
            }
            self.notify.notifier.show(req);
        }
        for uid in self.notify.tracker.take_seen(&visible, focused) {
            self.notify.notifier.close(Some(uid));
        }
        if self.notify.tracker.any_busy() {
            ctx.request_repaint_after(BUSY_WATCH);
        }
        for signal in self.notify.notifier.drain() {
            self.on_notify_signal(signal);
        }
    }

    fn on_notify_signal(&mut self, signal: Signal) {
        match signal {
            Signal::Clicked { tab, token } => {
                log::debug!(
                    "notification clicked: tab {tab:?}, token {}",
                    token.is_some()
                );
                self.notify.test_pending = false;
                if let Some(uid) = tab {
                    self.notify.tracker.gone(uid);
                    let index = self.tabs.iter().position(|t| t.uid() == Some(uid));
                    if let Some(i) = index {
                        self.activate(i);
                    }
                }
                self.raise_window(token.as_deref());
            }
            Signal::Gone { tab } => {
                self.notify.test_pending = false;
                if let Some(uid) = tab {
                    self.notify.tracker.gone(uid);
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
            NotifyAction::Enabled(v) => cfg.enabled = v,
            NotifyAction::TurnFinished(v) => cfg.turn_finished = v,
            NotifyAction::Attention(v) => cfg.attention = v,
            NotifyAction::Exited(v) => cfg.exited = v,
            NotifyAction::OnlyWhenUnfocused(v) => cfg.only_when_unfocused = v,
            NotifyAction::Test => {
                self.send_test_notification();
                return;
            }
        }
        self.dirty = true;
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
