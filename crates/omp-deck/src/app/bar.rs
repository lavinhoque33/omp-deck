//! App-side glue for the status bar: status watcher target, optimistic model switches, the
//! per-tab injection queues, action dispatch and the small dialogs.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use deck_core::sessions::{SessionEntry, abbreviate_home};
use deck_core::status::{SessionStats, StatusTarget};
use egui::{TextEdit, Ui};

use super::{App, AppEvent, PendingConfirm, spawn_opener};
use crate::confirm::{self, Confirm, Dialog};
use crate::inject::{Env, Job, Requires};
use crate::model_picker::{self, FRESH_TIMEOUT, Pending, PickerAction, SWITCH_TIMEOUT};
use crate::session_actions::{self, Action, ActionCtx, CompactMode};
use crate::statusbar::{self, BarAction, BarInput};
use crate::statusfmt::Activity;
use crate::tabs::{Tab, TabRole};
use crate::toast::ToastKind;

/// Label of the injection job behind the model/thinking dropdown.
const MODEL_SWITCH: &str = "Model switch";
/// Terminal input is dropped for this long (renewed every frame) while a dialog is open.
const MODAL_HOLD: Duration = Duration::from_millis(250);
/// How often a deferred tab close checks whether its injector reached a job boundary.
const CLOSE_POLL: Duration = Duration::from_millis(50);

/// A text prompt opened from the bar's menus.
pub enum ActionDialog {
    Handoff { focus: String, first: bool },
    CompactFocus { focus: String, first: bool },
    Rename { title: String, first: bool },
}

impl App {
    /// Per frame, before drawing: follow the active tab, feed the bar, resolve pending switches and
    /// run the injection queues.
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
        // Submitted every frame so /new, /fork, /move (which update `session_path`) also move the
        // watcher's fallback; rebuilt only when a field differs from the last submitted target.
        let unchanged = match (want, &self.status_target) {
            (None, None) => true,
            (Some(t), Some(cur)) => {
                cur.session == t.session_path
                    && cur.cwd == t.cwd
                    && cur.tmux.as_ref().map(|(_, n)| n) == t.tmux_name.as_ref()
            }
            _ => false,
        };
        if !unchanged {
            let target = want.map(|t| StatusTarget {
                tmux: self.tmux.clone().zip(t.tmux_name.clone()),
                session: t.session_path.clone(),
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
                && tab.session_path.as_deref() != Some(p.as_path())
            {
                // /new, /fork, /move … switched this tab's session file.
                tab.session_path = Some(p.clone());
                self.dirty = true;
            }

            let live_model = live.as_deref().and_then(|l| l.model.as_ref());
            let no_stats = SessionStats::default();
            let stats = snap.as_deref().map_or(&no_stats, |s| &s.stats);
            let state = statusbar::model_state(stats, live_model);
            let model = state.model;
            if model.is_some() {
                tab.assumed_switch = None;
            }
            if let Some(p) = &mut tab.pending_switch {
                let confirmed = p.confirmed_by(model, state.thinking, state.configured);
                let limit = if model.is_none() {
                    FRESH_TIMEOUT
                } else {
                    SWITCH_TIMEOUT
                };
                if confirmed {
                    tab.pending_switch = None;
                } else if tab.injector.is_busy() {
                    // The timer starts once the command has actually been typed.
                    p.since = now;
                    ctx.request_repaint_after(Duration::from_millis(200));
                } else if p.timed_out(now, limit) {
                    let desc = p.describe(&self.models);
                    if model.is_none() {
                        // Nothing reports the model (no file entry, no live control): assume it
                        // worked, on top of an earlier assumed switch (`/effort` keeps the model).
                        let prev = tab.assumed_switch.take();
                        tab.assumed_switch = tab.pending_switch.take().map(|mut p| {
                            if let Some(prev) = prev {
                                p.selector = p.selector.or(prev.selector);
                                p.level = p.level.or(prev.level);
                            }
                            p
                        });
                    } else {
                        tab.pending_switch = None;
                        toast = Some(format!(
                            "omp did not confirm the switch to {desc} — it may be unavailable or omp is busy; check the terminal"
                        ));
                    }
                } else {
                    ctx.request_repaint_after(limit.saturating_sub(now.duration_since(p.since)));
                }
            }
            self.bar_view.set_defaults(&self.omp_defaults);
            self.bar_view.set_live_model(live_model);
            self.bar_view
                .set_context(live.as_deref().and_then(|l| l.context.as_ref()));
            self.bar_view.set_usage(self.usage.summary());
            self.bar_view.update(snap.as_ref(), &tab.cwd, &self.models);
        }
        if let Some(t) = toast {
            self.toasts.error(&self.ctx, t);
        }
        self.drive_injectors(ctx);
    }

    /// Advance every tab's injection queue (never blocks: waits are repaint timers), then close
    /// the tabs whose close was deferred until their injector reached a job boundary.
    fn drive_injectors(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        // While a dialog or menu is up the terminal may still own the keyboard: Enter/Esc meant for the
        // dialog must not reach omp. (Set first: an injector poll below replaces the deadline.)
        if (self.confirm.is_some()
            || self.folder_dialog.is_some()
            || self.dialog.is_some()
            || self.popup.is_some()
            || self.tree.is_some()
            || self.usage_open
            || egui::Popup::is_any_open(ctx))
            && let Some(t) = self.tabs.get(self.active).and_then(Tab::term)
        {
            t.term.hold_input_until(now + MODAL_HOLD);
        }
        let mut prompts = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let mut wake: Option<Duration> = None;
        for tab in self.tabs.iter_mut().filter_map(Tab::term_mut) {
            if !tab.injector.is_busy() {
                continue;
            }
            if tab.term.is_exited() {
                tab.injector.clear();
                tab.pending_switch = None;
                continue;
            }
            let activity = Activity::from_title(tab.term.title());
            let term = &tab.term;
            let screen = || term.screen_text();
            let poll = tab.injector.poll(&Env {
                now,
                activity,
                last_ctrl_c: term.last_ctrl_c(),
                screen: &screen,
            });
            // The hold comes first so no user key can slip in between our bytes.
            if let Some(h) = poll.hold_for {
                term.hold_input_until(now + h);
            }
            for w in &poll.writes {
                term.write(w);
            }
            if let Some(c) = poll.confirm {
                prompts.push((tab.uid, c));
            }
            let dropped = poll
                .dialog_open
                .iter()
                .chain(&poll.aborted)
                .any(|l| l == MODEL_SWITCH);
            if dropped {
                tab.pending_switch = None;
            }
            for (label, why) in poll.refused {
                if why == Requires::Idle {
                    notes.push(format!(
                        "{label} needs omp to be idle — try again when it finishes"
                    ));
                }
            }
            if !poll.dialog_open.is_empty() {
                notes.push("Close omp's open dialog first".to_owned());
            }
            for label in poll.aborted {
                notes.push(format!(
                    "{label} cancelled: omp started working — check the composer"
                ));
            }
            if let Some(d) = poll.wake_in {
                wake = Some(wake.map_or(d, |w| w.min(d)));
            }
        }
        if let Some(d) = wake {
            ctx.request_repaint_after(d);
        }
        for (uid, c) in prompts {
            if self.confirm.is_none() {
                self.confirm = Some((
                    Confirm {
                        title: "Clear your unsent message?".to_owned(),
                        body: format!(
                            "The composer holds “{}”. Running {} clears it first.",
                            crate::statusfmt::ellipsize_left(&c.draft, 80),
                            c.label
                        ),
                        confirm_label: "Clear and run".to_owned(),
                        danger: true,
                    },
                    PendingConfirm::ClearDraft { uid },
                ));
            } else {
                self.answer_draft(uid, false);
            }
        }
        for note in notes {
            self.toasts.info(&self.ctx, note);
        }
        self.finish_deferred_closes(ctx);
    }

    /// Close tab `i` (the user's close click). A tab whose injector is mid-job stays until the job
    /// ends (queued jobs are dropped): closing detaches the PTY client halfway through a paste.
    pub(super) fn request_close(&mut self, i: usize) {
        let Some(Tab::Terminal(tab)) = self.tabs.get_mut(i) else {
            self.close_tab(i, true);
            return;
        };
        if tab.injector.can_close(Instant::now()) {
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
                Some(Tab::Terminal(t)) => t.term.is_exited() || t.injector.can_close(now),
                _ => true,
            };
            if ready {
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
    pub(super) fn answer_draft(&mut self, uid: u64, clear: bool) {
        if let Some(tab) = self
            .tabs
            .iter_mut()
            .filter_map(Tab::term_mut)
            .find(|t| t.uid == uid)
        {
            tab.injector.answer(clear);
            if !clear {
                tab.pending_switch = None;
            }
        }
        self.ctx.request_repaint();
    }

    pub(super) fn show_status_bar(&mut self, ui: &mut Ui) {
        let Some(Tab::Terminal(tab)) = self.tabs.get(self.active) else {
            return;
        };
        if tab.role != TabRole::Session {
            return;
        }
        let activity = Activity::from_title(tab.term.title());
        let input = BarInput {
            catalog: &self.models,
            favorites: &self.cfg.favorite_models,
            provider: self.cfg.model_provider_filter.as_deref(),
            activity,
            pending: tab.pending_switch.as_ref(),
            assumed: tab.assumed_switch.as_ref(),
            actions: ActionCtx {
                busy: activity.is_busy(),
                idle: activity == Activity::Idle,
                has_session_file: tab.session_path.is_some(),
                last_turn_failed: self.bar_view.last_turn_failed,
                has_last_reply: self.bar_view.has_reply,
                exited: tab.term.is_exited(),
                omp_found: self.omp.is_some(),
            },
            agents: statusbar::AgentsChip {
                counts: self.agents_view.counts,
                open: self.hub_open,
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
            BarAction::Picker(PickerAction::Effort(level)) => {
                let current = self.current_model();
                match current {
                    Some(m) => self.queue_change(&m, Some(&level)),
                    None => self
                        .toasts
                        .info(&self.ctx, "The current model is not known yet"),
                }
            }
            BarAction::Run(a) => self.run_action(a),
            BarAction::AskHandoff => {
                self.dialog = Some(ActionDialog::Handoff {
                    focus: String::new(),
                    first: true,
                });
            }
            BarAction::AskCompactFocus => {
                self.dialog = Some(ActionDialog::CompactFocus {
                    focus: String::new(),
                    first: true,
                });
            }
            BarAction::AskRename => {
                self.dialog = Some(ActionDialog::Rename {
                    title: String::new(),
                    first: true,
                });
            }
            BarAction::CopyPath(p) => self.copy_text(&p, "path"),
            BarAction::OpenUrl(u) => self.open_external(u),
            BarAction::ToggleAgentHub => self.toggle_agent_hub(),
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

    /// The model the active session runs as far as the deck knows: an assumed switch's model
    /// while the bar can only show omp's defaults (the same rule as the model chip).
    fn current_model(&self) -> Option<String> {
        let v = &self.bar_view;
        let assumed = match self.tabs.get(self.active) {
            Some(Tab::Terminal(t)) if v.model_selector.is_none() || v.model_is_default => {
                t.assumed_switch.as_ref().and_then(|a| a.selector.clone())
            }
            _ => None,
        };
        assumed.or_else(|| v.model_selector.clone())
    }

    /// Send `/switch` or `/effort` to the active tab and show the optimistic state.
    fn queue_change(&mut self, selector: &str, level: Option<&str>) {
        let current = self.current_model();
        let Some(change) = model_picker::plan_switch(current.as_deref(), selector, level) else {
            return;
        };
        let Some(Tab::Terminal(tab)) = self.tabs.get_mut(self.active) else {
            return;
        };
        if tab.term.is_exited() {
            return;
        }
        tab.injector
            .push(Job::slash(MODEL_SWITCH, &change.command, Requires::Any));
        tab.pending_switch = Some(Pending {
            selector: change.selector,
            level: change.level,
            since: Instant::now(),
        });
        self.ctx.request_repaint();
    }

    fn run_action(&mut self, action: Action) {
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
                let Some(job) = session_actions::job(&other) else {
                    return;
                };
                if let Some(Tab::Terminal(tab)) = self.tabs.get_mut(self.active)
                    && !tab.term.is_exited()
                {
                    tab.injector.push(job);
                    self.ctx.request_repaint();
                }
            }
        }
    }

    fn active_session(&self) -> Option<PathBuf> {
        self.tabs
            .get(self.active)
            .and_then(Tab::session_path)
            .map(PathBuf::from)
    }

    fn find_entry(&self, path: &std::path::Path) -> Option<SessionEntry> {
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

    /// `omp --export` on a worker, then open the HTML with xdg-open.
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
                Ok(path) => match spawn_opener(path.as_os_str()) {
                    Ok(()) => AppEvent::Toast(
                        ToastKind::Success,
                        format!("Exported to {}", abbreviate_home(&path)),
                    ),
                    Err(e) => AppEvent::Toast(
                        ToastKind::Error,
                        format!(
                            "Exported to {} but xdg-open failed: {e}",
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
        self.run_action(action);
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
