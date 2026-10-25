//! App-side glue for the status bar: status watcher target, the model-switch confirmation fed
//! to each tab's controller, command submission and the small dialogs.
//!
//! Every command that writes into a session (status-bar actions, the palette, review text,
//! worktree move/finish) is submitted to that session's [`SessionController`] with the
//! [`SessionTarget`] the user acted on; this module only turns the controller's events into
//! dialogs and toasts.

use std::time::{Duration, Instant};

use deck_core::platform;
use deck_core::sessions::{SessionEntry, abbreviate_home};
use deck_core::status::{SessionStats, StatusTarget};
use deck_service::controller::{
    ActionFacts, Command, CommandKind, ControllerEvent, Delivery, ModelFacts, Refusal, Rejected,
    RequestId, SessionTarget, Submission,
};
use deck_service::inject::Requires;
use deck_service::session_actions::{self, Action, ActionCtx, CompactMode};
use egui::{TextEdit, Ui};
use std::path::PathBuf;

use super::{App, AppEvent, PendingConfirm};
use crate::confirm::{self, Confirm, Dialog};
use crate::model_picker::PickerAction;
use crate::statusbar::{self, BarAction, BarInput};
use crate::tabs::{Tab, TabRole, TermTab};
use crate::toast::ToastKind;

/// Terminal input is dropped for this long (renewed every frame) while a dialog is open.
const MODAL_HOLD: Duration = Duration::from_millis(250);
/// How often a deferred tab close checks whether its controller reached a job boundary.
const CLOSE_POLL: Duration = Duration::from_millis(50);

/// A text prompt opened from the bar's menus.
pub enum ActionDialog {
    Handoff { focus: String, first: bool },
    CompactFocus { focus: String, first: bool },
    Rename { title: String, first: bool },
}

impl App {
    /// Per frame, before drawing: follow the active tab, feed the bar, resolve pending switches and
    /// run the controllers' injection queues.
    pub(super) fn drive_status(&mut self, ctx: &egui::Context) {
        let want = match self.tabs.get(self.active) {
            Some(Tab::Terminal(t)) if t.role == TabRole::Session => Some(t),
            _ => None,
        };
        let want_uid = want.map(|t| t.uid);
        if want_uid != self.status_uid {
            self.status_uid = want_uid;
            self.bar_view = statusbar::BarView::default();
        }
        // Submitted every frame so /new, /fork, /move (which update the session path) also move the
        // watcher's fallback; rebuilt only when a field differs from the last submitted target.
        let unchanged = match (want, &self.status_target) {
            (None, None) => true,
            (Some(t), Some(cur)) => {
                cur.session.as_deref() == t.session_path()
                    && cur.cwd == t.cwd
                    && cur.mux.as_ref().map(|(_, n)| n) == t.mux_name.as_ref()
            }
            _ => false,
        };
        if !unchanged {
            let target = want.map(|t| StatusTarget {
                mux: t.mux.clone().zip(t.mux_name.clone()),
                session: t.session_path_owned(),
                cwd: t.cwd.clone(),
            });
            self.status.set_target(target.clone());
            self.status_target = target;
        }

        let snap = self.status.latest();
        let live = self.live_snapshot();
        let now = Instant::now();
        let mut toast = None;
        if let Some(Tab::Terminal(tab)) = self.tabs.get_mut(self.active)
            && tab.role == TabRole::Session
        {
            if let Some(s) = &snap
                && tab.copy.is_none()
                && !s.fresh
                && let Some(p) = &s.session
                && tab.session_path() != Some(p.as_path())
            {
                // /new, /fork, /move … switched this tab's session file: the controller starts a
                // new generation and drops what was queued for the old conversation.
                tab.ctl.set_session_path(Some(p.clone()));
                self.dirty = true;
            }

            let live_model = live.as_deref().and_then(|l| l.model.as_ref());
            let no_stats = SessionStats::default();
            let stats = snap.as_deref().map_or(&no_stats, |s| &s.stats);
            let state = statusbar::model_state(stats, live_model);
            self.bar_view.set_defaults(&self.omp_defaults);
            self.bar_view.set_live_model(live_model);
            self.bar_view
                .set_context(live.as_deref().and_then(|l| l.context.as_ref()));
            self.bar_view.set_usage(self.usage.summary());
            self.bar_view.update(snap.as_ref(), &tab.cwd, &self.models);
            let progress = tab.ctl.observe_model(
                now,
                ModelFacts {
                    model: state.model,
                    thinking: state.thinking,
                    configured: state.configured,
                    shown: self.bar_view.model_selector.as_deref(),
                    shown_is_default: self.bar_view.model_is_default,
                },
            );
            if let Some(w) = progress.wake_in {
                ctx.request_repaint_after(w);
            }
            if let Some(p) = progress.unconfirmed {
                let desc = p.describe(&|s| self.models.get(s).map(|m| m.name.clone()));
                toast = Some(format!(
                    "omp did not confirm the switch to {desc} — it may be unavailable or omp is busy; check the terminal"
                ));
            }
        }
        if let Some(t) = toast {
            self.toasts.error(&self.ctx, t);
        }
        self.drive_controllers(ctx);
    }

    /// Advance every tab's controller (never blocks: waits are repaint timers), then close the
    /// tabs whose close was deferred until their controller reached a job boundary.
    fn drive_controllers(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        // While a dialog or menu is up the terminal may still own the keyboard: Enter/Esc meant for the
        // dialog must not reach omp. (Set first: a controller drive below replaces the deadline.)
        if self.confirm.is_some()
            || self.folder_dialog.is_some()
            || self.wt.dialog.is_some()
            || self.popup.is_some()
            || self.tree.is_some()
            || self.usage_open
            || self.sessions.open
            || self.palette.is_some()
            || egui::Popup::is_any_open(ctx)
        {
            let visible = self.visible_uids();
            for t in self.tabs.iter().filter_map(Tab::term) {
                if visible.contains(&t.uid) {
                    t.terminal().hold_input_until(now + MODAL_HOLD);
                }
            }
        }
        let mut events: Vec<(u64, ControllerEvent)> = Vec::new();
        let mut wake: Option<Duration> = None;
        for tab in self.tabs.iter_mut().filter_map(Tab::term_mut) {
            let driven = tab.ctl.drive(now);
            events.extend(driven.events.into_iter().map(|e| (tab.uid, e)));
            if let Some(d) = driven.wake_in {
                wake = Some(wake.map_or(d, |w| w.min(d)));
            }
        }
        if let Some(d) = wake {
            ctx.request_repaint_after(d);
        }
        self.dispatch_controller_events(events);
        self.finish_deferred_closes(ctx);
    }

    /// Carries out what the controllers reported: the draft prompt, and the end of each command.
    fn dispatch_controller_events(&mut self, events: Vec<(u64, ControllerEvent)>) {
        for (uid, event) in events {
            match event {
                ControllerEvent::Confirm { id, label, draft } => {
                    if self.confirm.is_none() {
                        self.confirm = Some((
                            Confirm {
                                title: "Clear your unsent message?".to_owned(),
                                body: format!(
                                    "The composer holds “{}”. Running {} clears it first.",
                                    deck_service::statusfmt::ellipsize_left(&draft, 80),
                                    label
                                ),
                                confirm_label: "Clear and run".to_owned(),
                                danger: true,
                            },
                            PendingConfirm::ClearDraft { uid, id },
                        ));
                    } else {
                        self.answer_draft(uid, id, false);
                    }
                }
                ControllerEvent::Ended {
                    id,
                    kind,
                    label,
                    delivery,
                } => self.command_ended(uid, id, kind, &label, &delivery),
            }
        }
    }

    /// The controller of tab `i` is ready to close: collect the outcomes it queued on the way
    /// (`can_close` ends what was still waiting) and report them, before the tab, and with it
    /// the controller, is destroyed. Nothing is written by this.
    fn drain_before_close(&mut self, i: usize) {
        let Some(Tab::Terminal(tab)) = self.tabs.get_mut(i) else {
            return;
        };
        let uid = tab.uid;
        let events: Vec<(u64, ControllerEvent)> = tab
            .ctl
            .drive(Instant::now())
            .events
            .into_iter()
            .map(|e| (uid, e))
            .collect();
        self.dispatch_controller_events(events);
    }

    /// A command of tab `uid` reached its final delivery state. "Delivered" means the transport
    /// accepted the bytes, not that omp (or the host's PTY) executed anything; "Unobserved" means
    /// they were queued but no acknowledgement was seen before the view closed. Only the waiting UI
    /// state (review comments, worktree moves) acts on "Delivered".
    fn command_ended(
        &mut self,
        uid: u64,
        id: RequestId,
        kind: CommandKind,
        label: &str,
        delivery: &Delivery,
    ) {
        let finish = kind == CommandKind::WorktreeFinish;
        match (kind, delivery) {
            (CommandKind::Review { send }, Delivery::Delivered) => {
                self.review_delivered(uid, send, id);
            }
            (CommandKind::Review { send }, _) => self.review_dropped(uid, send, id),
            _ => {}
        }
        // The finish prompt retries by itself; only its final failure is announced.
        let note = match delivery {
            Delivery::Refused(Refusal::NoBracketedPaste) => Some(format!(
                "{label} refused: omp has not enabled bracketed paste"
            )),
            Delivery::Refused(Refusal::Precondition(Requires::Idle)) if !finish => Some(format!(
                "{label} needs omp to be idle — try again when it finishes"
            )),
            Delivery::Refused(Refusal::DialogOpen) if !finish => {
                Some("Close omp's open dialog first".to_owned())
            }
            Delivery::Aborted if !finish => Some(format!(
                "{label} cancelled: omp started working — check the composer"
            )),
            Delivery::Stalled if !finish => Some(format!(
                "{label} cancelled: omp did not show it in time — check the composer"
            )),
            // Never a success claim and never retried: it may still be written.
            Delivery::Unobserved => Some(format!(
                "{label} may not have reached omp — check the session"
            )),
            _ => None,
        };
        if let Some(note) = note {
            self.toasts.info(&self.ctx, note);
        }
        let lost = matches!(
            delivery,
            Delivery::Refused(Refusal::Precondition(_) | Refusal::DialogOpen)
                | Delivery::Aborted
                | Delivery::Stalled
        ) || (kind == CommandKind::WorktreeMove
            && matches!(delivery, Delivery::Cancelled(_)));
        if lost {
            self.injection_lost(uid, kind);
        }
        // A cancelled or declined finish prompt never goes in on its own: stop the finish.
        if finish && matches!(delivery, Delivery::Cancelled(_) | Delivery::Declined) {
            self.finish_cancelled(uid);
        }
    }

    /// Close tab `i` (the user's close click). A tab whose controller is mid-command stays until
    /// the command ends (queued ones are dropped): closing detaches the PTY client halfway
    /// through a paste.
    pub(super) fn request_close(&mut self, i: usize) {
        let Some(Tab::Terminal(tab)) = self.tabs.get_mut(i) else {
            self.close_tab(i, true);
            return;
        };
        if tab.ctl.can_close(Instant::now()) {
            self.drain_before_close(i);
            self.close_tab(i, true);
            return;
        }
        let uid = tab.uid;
        if !self.closing.contains(&uid) {
            self.closing.push(uid);
        }
        self.ctx.request_repaint_after(CLOSE_POLL);
    }

    fn finish_deferred_closes(&mut self, ctx: &egui::Context) {
        if self.closing.is_empty() {
            return;
        }
        let now = Instant::now();
        let pending = std::mem::take(&mut self.closing);
        for uid in pending {
            let Some(i) = self
                .tabs
                .iter()
                .position(|t| t.term().is_some_and(|t| t.uid == uid))
            else {
                continue;
            };
            let ready = match self.tabs.get_mut(i) {
                Some(Tab::Terminal(t)) => t.ctl.can_close(now),
                _ => true,
            };
            if ready {
                self.drain_before_close(i);
                self.close_tab(i, true);
                // This runs after the tab strip was drawn: paint the closed state next frame.
                ctx.request_repaint();
            } else {
                self.closing.push(uid);
            }
        }
        if !self.closing.is_empty() {
            ctx.request_repaint_after(CLOSE_POLL);
        }
    }

    /// The user's answer to the draft prompt.
    pub(super) fn answer_draft(&mut self, uid: u64, id: RequestId, clear: bool) {
        if let Some(tab) = self
            .tabs
            .iter_mut()
            .filter_map(Tab::term_mut)
            .find(|t| t.uid == uid)
        {
            // An answer for a confirmation the controller no longer waits for (the session
            // changed, the request was dropped) is ignored there.
            tab.ctl.answer(id, clear);
        }
        self.ctx.request_repaint();
    }

    /// What the availability rules of the session actions see for `tab` (status bar and palette).
    pub(super) fn action_ctx(&self, tab: &TermTab) -> ActionCtx {
        tab.ctl.action_ctx(ActionFacts {
            last_turn_failed: self.bar_view.last_turn_failed,
            has_last_reply: self.bar_view.has_reply,
            omp_found: self.omp.is_some(),
        })
    }

    pub(super) fn show_status_bar(&mut self, ui: &mut Ui) {
        let Some(Tab::Terminal(tab)) = self.tabs.get(self.active) else {
            return;
        };
        if tab.role != TabRole::Session {
            return;
        }
        let activity = tab.ctl.activity();
        let input = BarInput {
            catalog: &self.models,
            favorites: &self.cfg.favorite_models,
            provider: self.cfg.model_provider_filter.as_deref(),
            activity,
            pending: tab.ctl.pending_switch(),
            assumed: tab.ctl.assumed_switch(),
            actions: self.action_ctx(tab),
            agents: statusbar::AgentsChip {
                counts: self.agents_view.counts,
                open: self.dash.open,
            },
        };
        let actions = statusbar::show(ui, &self.bar_view, &mut self.picker, &input);
        if actions.is_empty() {
            return;
        }
        self.focus_pending = true;
        for a in actions {
            self.apply_bar_action(a);
        }
    }

    fn apply_bar_action(&mut self, action: BarAction) {
        match action {
            BarAction::Picker(PickerAction::Refresh) => self.reload_models(),
            BarAction::Picker(PickerAction::Switch { selector, level }) => {
                self.queue_change(&selector, level.as_deref());
            }
            BarAction::Picker(PickerAction::ToggleFavorite(selector)) => {
                self.cfg.toggle_favorite(&selector);
                self.dirty = true;
            }
            BarAction::Picker(PickerAction::SetProvider(provider)) => {
                self.cfg.model_provider_filter = provider;
                self.dirty = true;
            }
            BarAction::Picker(PickerAction::Effort(level)) => self.queue_effort(level),
            BarAction::Run(a) => self.run_action(a),
            BarAction::AskHandoff => self.open_action_dialog(ActionDialog::Handoff {
                focus: String::new(),
                first: true,
            }),
            BarAction::AskCompactFocus => self.open_action_dialog(ActionDialog::CompactFocus {
                focus: String::new(),
                first: true,
            }),
            BarAction::AskRename => self.open_action_dialog(ActionDialog::Rename {
                title: String::new(),
                first: true,
            }),
            BarAction::CopyPath(p) => self.copy_text(&p, "path"),
            BarAction::OpenUrl(u) => self.open_external(u),
            BarAction::ToggleAgents => self.toggle_dashboard(true),
            BarAction::ToggleChanges => self.toggle_changes_panel(),
            BarAction::OpenTree => self.open_tree_active(),
            BarAction::OpenUsage => self.open_usage(),
        }
    }

    pub(super) fn reload_models(&mut self) {
        let Some(omp) = self.omp.clone() else {
            return;
        };
        if self.models.is_loading() {
            return;
        }
        self.models.begin();
        crate::omp_worker::spawn(&self.ctx, &self.events_tx, move || {
            AppEvent::Omp(crate::omp_worker::OmpEvent::Models(
                omp.models().map_err(|e| e.to_string()),
            ))
        });
    }

    /// The model the active session runs as far as the deck knows: an unconfirmed switch's
    /// model (omp applies it before anything queued now), else an assumed switch's model while
    /// the bar can only show omp's defaults (the same rule as the model chip).
    pub(super) fn current_model(&self) -> Option<String> {
        match self.tabs.get(self.active) {
            Some(Tab::Terminal(t)) => t.ctl.current_model(),
            _ => self.bar_view.model_selector.clone(),
        }
    }

    /// The session the status bar and palette act on now: the active session tab's current target.
    pub(super) fn active_target(&self) -> Option<SessionTarget> {
        match self.tabs.get(self.active) {
            Some(Tab::Terminal(t)) if t.role == TabRole::Session => Some(t.ctl.target().clone()),
            _ => None,
        }
    }

    /// Opens a text prompt for the active session. The dialog is bound to the session (and
    /// generation) it was opened on: if that session changed or went away by the time the user
    /// confirms, nothing is sent.
    pub(super) fn open_action_dialog(&mut self, dialog: ActionDialog) {
        let Some(target) = self.active_target() else {
            return;
        };
        self.dialog = Some(dialog);
        self.dialog_target = Some(target);
    }

    /// Submits `command` to the session `target` names. Returns the accepted request, or tells
    /// the user why not (a refusal that only restates a no-op stays silent).
    pub(super) fn submit_command(
        &mut self,
        target: &SessionTarget,
        command: Command,
    ) -> Option<RequestId> {
        // Actions and model switches that find omp gone stay silent, as the bar always did.
        let quiet_exit = matches!(
            command,
            Command::Action(_) | Command::SwitchModel { .. } | Command::SetEffort { .. }
        );
        let key = self.request_keys.next_key();
        let outcome = match self
            .tabs
            .iter_mut()
            .filter_map(Tab::term_mut)
            .find(|t| t.ctl.target().key == target.key)
        {
            Some(tab) => tab.ctl.submit(Submission {
                key,
                target: target.clone(),
                command,
            }),
            None => {
                self.toasts
                    .error(&self.ctx, "The session's tab was closed — nothing was sent");
                return None;
            }
        };
        match outcome {
            Ok(done) => {
                self.ctx.request_repaint();
                Some(done.id)
            }
            Err(why) => {
                self.report_rejected(&why, quiet_exit);
                None
            }
        }
    }

    fn report_rejected(&mut self, why: &Rejected, quiet_exit: bool) {
        match why {
            // Nothing to do: the request restated the state the session already has.
            Rejected::NoChange | Rejected::NotInjected => {}
            Rejected::Exited if quiet_exit => {}
            Rejected::Exited => self
                .toasts
                .error(&self.ctx, "omp has exited in this tab; nothing was sent"),
            Rejected::ModelUnknown => self
                .toasts
                .info(&self.ctx, "The current model is not known yet"),
            other => self.toasts.error(&self.ctx, other.to_string()),
        }
    }

    /// Send `/switch` or `/effort` to the active tab; the controller shows the optimistic state.
    pub(super) fn queue_change(&mut self, selector: &str, level: Option<&str>) {
        let Some(target) = self.active_target() else {
            return;
        };
        self.submit_command(
            &target,
            Command::SwitchModel {
                selector: selector.to_owned(),
                level: level.map(str::to_owned),
            },
        );
    }

    /// `/effort` on the active session's current model.
    pub(super) fn queue_effort(&mut self, level: String) {
        let Some(target) = self.active_target() else {
            return;
        };
        self.submit_command(&target, Command::SetEffort { level });
    }

    pub(super) fn run_action(&mut self, action: Action) {
        match action {
            Action::Fork => self.fork_active(),
            Action::CopyLast => match self
                .status
                .latest()
                .and_then(|s| s.stats.last_assistant_text.clone())
            {
                Some(text) => self.copy_text(&text, "last response"),
                None => self
                    .toasts
                    .info(&self.ctx, "There is no assistant reply to copy yet"),
            },
            Action::ExportHtml => self.export_active(),
            other => {
                if let Some(target) = self.active_target() {
                    self.submit_command(&target, Command::Action(other));
                }
            }
        }
    }

    pub(super) fn active_session(&self) -> Option<PathBuf> {
        self.tabs
            .get(self.active)
            .and_then(Tab::session_path)
            .map(PathBuf::from)
    }

    pub(super) fn find_entry(&self, path: &std::path::Path) -> Option<SessionEntry> {
        let groups = self.watcher.latest()?;
        groups
            .iter()
            .flat_map(|g| g.sessions.iter())
            .find(|e| e.path == path)
            .cloned()
    }

    /// Fork = `omp --fork <file>` in a new tab, linked like a copy once its file appears.
    fn fork_active(&mut self) {
        if self.omp.is_none() {
            return;
        }
        let Some(path) = self.active_session() else {
            return;
        };
        match self.find_entry(&path) {
            Some(entry) => {
                let dest = entry.cwd.clone();
                self.copy_session(&entry, &dest);
            }
            None => {
                self.refresh_sessions();
                self.toasts.info(
                    &self.ctx,
                    "That session is not in the sidebar yet — try again in a moment",
                );
            }
        }
    }

    /// `omp --export` on a worker, then open the HTML with the desktop's default handler.
    fn export_active(&mut self) {
        let (Some(omp), Some(jsonl)) = (&self.omp, self.active_session()) else {
            return;
        };
        let program = omp.program().to_path_buf();
        let (tx, ctx) = (self.events_tx.clone(), self.ctx.clone());
        self.toasts.info(&self.ctx, "Exporting session to HTML…");
        std::thread::spawn(move || {
            let dest = session_actions::exports_dir();
            let event = match session_actions::export_html(&program, &jsonl, &dest) {
                Ok(path) => match platform::open_path(path.as_os_str()) {
                    Ok(()) => AppEvent::Toast(
                        ToastKind::Success,
                        format!("Exported to {}", abbreviate_home(&path)),
                    ),
                    Err(e) => AppEvent::Toast(
                        ToastKind::Error,
                        format!(
                            "Exported to {} but opening it failed: {e}",
                            abbreviate_home(&path)
                        ),
                    ),
                },
                Err(e) => AppEvent::Toast(ToastKind::Error, e),
            };
            let _ = tx.send(event);
            ctx.request_repaint();
        });
    }

    // ------------------------------------------------------------ dialogs

    pub(super) fn show_action_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.dialog.as_mut() else {
            return;
        };
        let result = match dialog {
            ActionDialog::Handoff { focus, first } => confirm::show_dialog(
                ctx,
                &Dialog {
                    id: "handoff",
                    title: "Hand off to a fresh context",
                    width: 440.0,
                    confirm_label: "Hand off",
                    confirm_enabled: true,
                    danger: false,
                    enter_confirms: false,
                },
                |ui| {
                    ui.label("omp summarizes the session into a handoff document and continues from it. Optionally tell it what to focus on.");
                    focus_editor(ui, focus, first, true, "Focus instructions (optional)");
                },
            ),
            ActionDialog::CompactFocus { focus, first } => confirm::show_dialog(
                ctx,
                &Dialog {
                    id: "compact-focus",
                    title: "Compact with focus",
                    width: 440.0,
                    confirm_label: "Compact",
                    confirm_enabled: !focus.trim().is_empty(),
                    danger: false,
                    enter_confirms: false,
                },
                |ui| {
                    ui.label("What should the compacted context keep?");
                    focus_editor(ui, focus, first, true, "e.g. the auth refactor decisions");
                },
            ),
            ActionDialog::Rename { title, first } => confirm::show_dialog(
                ctx,
                &Dialog {
                    id: "rename",
                    title: "Rename session",
                    width: 420.0,
                    confirm_label: "Rename",
                    confirm_enabled: !title.trim().is_empty(),
                    danger: false,
                    enter_confirms: true,
                },
                |ui| {
                    focus_editor(ui, title, first, false, "New title");
                },
            ),
        };
        let Some(ok) = result else {
            return;
        };
        self.focus_pending = true;
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        let target = self.dialog_target.take();
        if !ok {
            return;
        }
        let action = match dialog {
            ActionDialog::Handoff { focus, .. } => {
                Action::Handoff(Some(focus).filter(|f| !f.trim().is_empty()))
            }
            ActionDialog::CompactFocus { focus, .. } => Action::Compact(CompactMode::Focus(focus)),
            ActionDialog::Rename { title, .. } => Action::Rename(title),
        };
        if let Some(target) = target {
            self.submit_command(&target, Command::Action(action));
        }
    }
}

/// Text field for the dialogs; grabs focus the first time it is shown.
fn focus_editor(ui: &mut Ui, text: &mut String, first: &mut bool, multiline: bool, hint: &str) {
    ui.add_space(6.0);
    let edit = if multiline {
        TextEdit::multiline(text).desired_rows(4)
    } else {
        TextEdit::singleline(text)
    };
    let r = ui.add(
        edit.hint_text(hint)
            .desired_width(f32::INFINITY)
            .margin(egui::vec2(10.0, 8.0)),
    );
    if *first {
        r.request_focus();
        *first = false;
    }
}
