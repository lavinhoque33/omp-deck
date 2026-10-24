//! App-side glue for the native Agent Hub: the watcher target, the side panel and the popup.

use std::path::PathBuf;
use std::time::SystemTime;

use deck_core::agents::AgentsTarget;
use egui::{Frame, Key, Margin, Modifiers};

use super::{App, AppEvent};
use crate::agent_ctl::{CtlView, Restart};
use crate::agent_hub::{self, HubAction, HubInput};
use crate::agent_popup::{self, PopupAction, PopupInput, PopupState, PopupTab};
use crate::tabs::{Tab, TabRole};
use crate::theme::Palette;
use crate::toast::ToastKind;

impl App {
    /// The active tab is an omp session (the hub follows it).
    pub(super) fn active_session_tab(&self) -> bool {
        matches!(
            self.tabs.get(self.active),
            Some(Tab::Terminal(t)) if t.role == TabRole::Session
        )
    }

    /// Per frame, before drawing: follow the active tab's session file and refresh the cached view.
    pub(super) fn drive_agents(&mut self) {
        self.drive_ctl();
        let want = match self.tabs.get(self.active) {
            Some(Tab::Terminal(t)) if t.role == TabRole::Session => {
                t.session_path.clone().map(|session| AgentsTarget {
                    session,
                    owner_alive: !t.term.is_exited(),
                })
            }
            _ => None,
        };
        if want != self.agents_target {
            // Search, status filter and collapsed groups belong to one session.
            let other_session = want.as_ref().map(|t| &t.session)
                != self.agents_target.as_ref().map(|t| &t.session);
            if other_session {
                self.hub.reset();
            }
            self.agents.set_target(want.clone());
            self.agents_target = want;
        }
        // A snapshot of another session (right after a tab or session switch) is not ours.
        let snap = self.agents.latest().filter(|s| {
            self.agents_target
                .as_ref()
                .is_some_and(|t| t.session == s.session)
        });
        let live = self.live_snapshot();
        self.agents_view
            .update(snap.as_ref(), live.as_ref(), SystemTime::now());
        if self.hub_open || self.popup.is_some() {
            self.hub.sync(&self.agents_view);
        }
        if self.popup.is_some() && self.agents_target.is_none() {
            self.popup = None;
        }
    }

    pub(super) fn toggle_agent_hub(&mut self) {
        if !self.active_session_tab() {
            return;
        }
        self.hub_open = !self.hub_open;
        if self.hub_open {
            self.changes.open = false;
        }
        self.focus_pending = true;
    }

    /// Esc closes the popup, unless a menu is open (it takes the key first). True when consumed.
    pub(super) fn consume_popup_escape(&mut self, ctx: &egui::Context) -> bool {
        self.popup.is_some()
            && !egui::Popup::is_any_open(ctx)
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
    }

    pub(super) fn close_agent_popup(&mut self) {
        self.popup = None;
        self.focus_pending = true;
    }

    fn open_agent(&mut self, id: &str, tab: PopupTab) {
        let Some(agent) = self.agents_view.agents().iter().find(|a| a.id == id) else {
            return;
        };
        self.popup = Some(PopupState::new(&self.ctx, agent, tab));
        self.ctx.request_repaint();
    }

    pub(super) fn show_agent_hub(&mut self, ui: &mut egui::Ui) {
        if !self.hub_open || !self.active_session_tab() {
            return;
        }
        let pal = Palette::of_ui(ui);
        let ctl = CtlView {
            link: self.ctl_link(),
            restart: self.restart_offer(),
            reqs: &self.ctl_reqs,
            errors: &self.ctl_errors,
        };
        let input = HubInput {
            view: &self.agents_view,
            has_session: self.agents_target.is_some(),
            selected: self.popup.as_ref().map(|p| p.agent_id.as_str()),
            ctl: &ctl,
        };
        let actions = egui::Panel::right("agent-hub")
            .resizable(true)
            .default_size(self.cfg.sidebar_width.clamp(320.0, 420.0))
            .size_range(300.0..=760.0)
            .frame(
                Frame::NONE
                    .fill(pal.panel)
                    .inner_margin(Margin::symmetric(12, 12)),
            )
            .show(ui, |ui| agent_hub::show(ui, &mut self.hub, &input))
            .inner;
        for a in actions {
            match a {
                HubAction::Open(id) => {
                    let tab = self.popup.as_ref().map_or(PopupTab::Transcript, |p| p.tab);
                    self.open_agent(&id, tab);
                }
                HubAction::Close => {
                    self.hub_open = false;
                    self.focus_pending = true;
                }
                HubAction::Kill(id) => self.request_kill_agent(id),
                HubAction::Revive(id) => self.send_ctl_revive(id),
                HubAction::Restart => self.request_restart_live(),
            }
        }
    }

    pub(super) fn show_agent_popup(&mut self, ctx: &egui::Context) {
        if self.popup.is_none() {
            return;
        }
        let ctl = CtlView {
            link: self.ctl_link(),
            restart: Restart::Hidden,
            reqs: &self.ctl_reqs,
            errors: &self.ctl_errors,
        };
        let Some(st) = self.popup.as_mut() else {
            return;
        };
        let Some(index) = self
            .agents_view
            .agents()
            .iter()
            .position(|a| a.id == st.agent_id)
        else {
            // The agent vanished (session switched or its files were removed).
            self.popup = None;
            return;
        };
        let tab = st.tab;
        let actions = agent_popup::show(
            ctx,
            st,
            &PopupInput {
                view: &self.agents_view,
                index,
                order: self.hub.order(),
                ctl: &ctl,
            },
        );
        // Links in rendered markdown go through the same allow-listed opener as terminal links.
        let urls = ctx.output_mut(|o| {
            let mut urls = Vec::new();
            o.commands.retain(|c| match c {
                egui::OutputCommand::OpenUrl(u) => {
                    urls.push(u.url.clone());
                    false
                }
                _ => true,
            });
            urls
        });
        for url in urls {
            self.open_external(url);
        }
        for a in actions {
            match a {
                PopupAction::Close => self.close_agent_popup(),
                PopupAction::Open(id) => self.open_agent(&id, tab),
                PopupAction::Copy { text, what } => self.copy_text(&text, what),
                PopupAction::OpenFile(path) => self.open_path(path),
                PopupAction::Error(message) => self.toasts.error(&self.ctx, message),
                PopupAction::Kill(id) => self.request_kill_agent(id),
                PopupAction::Revive(id) => self.send_ctl_revive(id),
                PopupAction::Chat { agent, text } => self.send_ctl_chat(agent, text),
            }
        }
    }

    /// Open a file with the desktop's default handler off the UI thread.
    fn open_path(&self, path: PathBuf) {
        let (tx, ctx) = (self.events_tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let outcome = if !path.is_absolute() || !path.is_file() {
                Err(format!("File not found: {}", path.display()))
            } else {
                deck_core::platform::open_path(path.as_os_str())
                    .map_err(|e| format!("Could not open the file: {e}"))
            };
            if let Err(msg) = outcome {
                let _ = tx.send(AppEvent::Toast(ToastKind::Error, msg));
                ctx.request_repaint();
            }
        });
    }
}
