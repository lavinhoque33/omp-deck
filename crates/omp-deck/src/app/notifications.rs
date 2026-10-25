//! App-side glue for desktop notifications: feeds tab states to the trigger rules, sends the
//! notifications, and turns clicks into "activate that tab and raise the window".

use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime};

use egui::{UserAttentionType, ViewportCommand};

use super::App;
use crate::notify::classify;
use crate::notify::inbox::{Entry, Inbox, Kind};
use crate::notify::tracker::{self, Event, Sighting, Ticket, Tracker};
use crate::notify::{Notifier, Request, Signal};
use crate::notify_panel::{self, PanelAction};
use crate::notify_view::NotifyAction;
use crate::statusfmt::Activity;
use crate::tabs::{Tab, TabRole, TabStatus};

/// While a tab is busy the app repaints at least this often, so the end of its turn is seen even
/// when the window is in the background.
const BUSY_WATCH: Duration = Duration::from_secs(1);

pub(super) struct NotifyUi {
    notifier: Notifier,
    tracker: Tracker,
    /// What happened in tabs the user was not looking at (the bell's popover).
    pub(super) inbox: Inbox,
    classified: Receiver<Classified>,
    classified_tx: Sender<Classified>,
    /// Why notifications cannot be shown (once the notifier thread found out).
    pub(super) unavailable: Option<String>,
    window_attached: bool,
    /// The user pressed "Send test notification" and no result came back yet.
    test_pending: bool,
}

/// A finished turn's verdict from the worker; `kind` is `None` for a turn the user stopped.
struct Classified {
    ticket: Ticket,
    kind: Option<Kind>,
}

impl NotifyUi {
    pub(super) fn new(ctx: &egui::Context) -> NotifyUi {
        let (classified_tx, classified) = mpsc::channel();
        NotifyUi {
            notifier: Notifier::start(ctx.clone()),
            tracker: Tracker::default(),
            inbox: Inbox::default(),
            classified,
            classified_tx,
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
        let mut fired = Vec::new();
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
                fired.push((t.uid, event));
            }
        }
        for uid in self.notify.tracker.retain(&live) {
            self.notify.notifier.close(Some(uid));
        }
        self.notify.inbox.retain_live(&live);
        for (uid, event) in fired {
            match event {
                Event::TurnFinished => self.classify_finished_turn(uid),
                Event::Attention => self.record_notification(uid, Kind::Attention, focused),
                Event::Exited { code } => {
                    self.record_notification(uid, Kind::Exited { code }, focused);
                }
            }
        }
        while let Ok(done) = self.notify.classified.try_recv() {
            let uid = done.ticket.uid;
            // Only for the very state it was asked about: same tab, same session file, no status
            // change since (a newer attention/exit/next turn wins over a late verdict).
            let session = self
                .tabs
                .iter()
                .find_map(|t| t.term().filter(|t| t.uid == uid))
                .map(|t| t.session_path.as_deref());
            let current = session.is_some_and(|s| self.notify.tracker.is_current(&done.ticket, s));
            // And only if the user still is not looking at the tab.
            let viewed = focused && visible.contains(&uid);
            if let Some(kind) = done.kind
                && current
                && !viewed
            {
                self.record_notification(uid, kind, focused);
            }
        }
        if focused {
            self.notify.inbox.remove_viewed(&visible);
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

    /// A turn of tab `uid` just ended: find out on a worker how, then record it (see
    /// `classified` in [`drive_notifications`](Self::drive_notifications)).
    fn classify_finished_turn(&mut self, uid: u64) {
        let Some(t) = self
            .tabs
            .iter()
            .find_map(|t| t.term().filter(|t| t.uid == uid))
        else {
            return;
        };
        let session = t.session_path.clone();
        let Some(ticket) = self.notify.tracker.ticket(uid, session.clone()) else {
            return;
        };
        let busy_start = SystemTime::now()
            .checked_sub(self.notify.tracker.last_busy(uid))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let (tx, ctx) = (self.notify.classified_tx.clone(), self.ctx.clone());
        let spawned = std::thread::Builder::new()
            .name("omp-deck-turn-outcome".to_owned())
            .spawn(move || {
                let kind = classify::finished_turn(session.as_deref(), busy_start);
                if tx.send(Classified { ticket, kind }).is_ok() {
                    ctx.request_repaint();
                }
            });
        if let Err(e) = spawned {
            log::warn!("cannot start the turn classifier: {e}");
            let focused = self.ctx.input(|i| i.viewport().focused.unwrap_or(true));
            self.record_notification(uid, Kind::Finished, focused);
        }
    }

    /// Puts the event in the inbox, sets the bell marker for turn ends, and shows a desktop
    /// pop-up when the settings and the window's focus allow it.
    fn record_notification(&mut self, uid: u64, kind: Kind, focused: bool) {
        let Some(tab) = self.tabs.iter_mut().find(|t| t.uid() == Some(uid)) else {
            return;
        };
        let title = tab.label(&self.labels).to_owned();
        let Some(t) = tab.term_mut() else { return };
        let project = t
            .cwd
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_owned();
        if matches!(kind, Kind::Finished | Kind::Error(_)) {
            t.bell = true;
        }
        let (pop_title, body) = tracker::describe(&kind, &title, &project);
        self.notify
            .inbox
            .record(Entry::new(uid, kind, &title, &project, SystemTime::now()));
        if tracker::desktop_wanted(&self.cfg.notify, focused) {
            self.notify.tracker.shown(uid);
            self.notify.notifier.show(Request {
                tab: Some(uid),
                title: pop_title,
                body,
            });
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
                    self.notify.inbox.remove(uid);
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
            NotifyAction::Desktop(v) => cfg.desktop = v,
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

    /// The inbox popover asked for something.
    pub(super) fn apply_inbox_action(&mut self, action: PanelAction) {
        match action {
            PanelAction::Open(uid) => {
                self.notify.inbox.remove(uid);
                self.notify.notifier.close(Some(uid));
                self.notify.tracker.gone(uid);
                if let Some(i) = self.tabs.iter().position(|t| t.uid() == Some(uid)) {
                    self.activate(i);
                }
                self.raise_window(None);
            }
            PanelAction::Dismiss(uid) => {
                self.notify.inbox.remove(uid);
                self.notify.notifier.close(Some(uid));
                self.notify.tracker.gone(uid);
            }
            PanelAction::ClearAll => {
                let tabs: Vec<u64> = self.notify.inbox.entries().iter().map(|e| e.tab).collect();
                self.notify.inbox.clear();
                self.notify.notifier.close_all();
                for uid in tabs {
                    self.notify.tracker.gone(uid);
                }
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
