//! App-side glue for the Usage view and the status-bar usage chip: the fetch workers, the
//! schedule (see [`crate::usage_state`]), and the modal's open/close/refresh.

use std::time::{Instant, SystemTime};

use deck_core::usage::{
    ACTIVITY_DAYS, DEFAULT_FETCH_TIMEOUT, activity, fetch_usage, stats_db_path,
};
use egui::{Key, Modifiers};

use super::{App, AppEvent};
use crate::omp_worker;
use crate::usage_state::UsageEvent;
use crate::usage_view::{self, UsageAction, ViewInput};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0))
}

impl App {
    /// Per frame: apply finished fetches, start due ones, keep the "resets in" labels fresh.
    pub(super) fn drive_usage(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        if self.usage.recompute_due(now) {
            self.usage.recompute(now, now_ms());
        }
        if self.usage.auto_due(now) {
            self.start_usage_fetch(false);
        }
        if let Some(wait) = self.usage.next_due(now) {
            ctx.request_repaint_after(wait);
        }
    }

    /// The Usage view's keyboard shortcut (Ctrl+Shift+U).
    pub(super) fn usage_shortcut(&self, ctx: &egui::Context) -> bool {
        ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::U))
    }

    pub(super) fn toggle_usage(&mut self) {
        if self.usage_open {
            self.close_usage();
        } else {
            self.open_usage();
        }
    }

    pub(super) fn open_usage(&mut self) {
        self.usage_open = true;
        let now = Instant::now();
        if self.usage.open_due(now) {
            self.start_usage_fetch(false);
        }
        if self.usage.activity_due(now) {
            self.start_activity_read();
        }
    }

    pub(super) fn close_usage(&mut self) {
        self.usage_open = false;
        self.focus_pending = true;
    }

    /// `omp usage --json` on a worker; `invalidate` first drops omp's report cache.
    fn start_usage_fetch(&mut self, invalidate: bool) {
        let Some(omp) = self.omp.clone() else {
            return;
        };
        self.usage.begin_fetch(Instant::now());
        omp_worker::spawn(&self.ctx, &self.events_tx, move || {
            AppEvent::Usage(UsageEvent::Fetched(
                fetch_usage(&omp, invalidate, DEFAULT_FETCH_TIMEOUT).map_err(|e| e.to_string()),
            ))
        });
    }

    /// `stats.db` (read-only) on a worker.
    fn start_activity_read(&mut self) {
        self.usage.begin_activity();
        omp_worker::spawn(&self.ctx, &self.events_tx, move || {
            AppEvent::Usage(UsageEvent::Activity(
                activity(&stats_db_path(), ACTIVITY_DAYS).map_err(|e| e.to_string()),
            ))
        });
    }

    pub(super) fn on_usage_event(&mut self, ev: UsageEvent) {
        self.usage.apply(ev, Instant::now(), now_ms());
    }

    /// Per frame: draw the Usage view while it is open and run what it asks for.
    pub(super) fn show_usage(&mut self, ctx: &egui::Context) {
        if !self.usage_open {
            return;
        }
        let now = Instant::now();
        let input = ViewInput {
            load: self.usage.load(),
            summary: self.usage.summary(),
            activity: self.usage.activity(),
            error: self.usage.error(),
            activity_error: self.usage.activity_error(),
            activity_loading: self.usage.activity_loading(),
            fetching: self.usage.fetching(),
            age_secs: self.usage.age_secs(now),
        };
        let actions = usage_view::show(ctx, &mut self.usage_ui, &input);
        // "Updated Xs ago" ticks while the view is open.
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
        for a in actions {
            match a {
                UsageAction::Close => self.close_usage(),
                UsageAction::Refresh => {
                    if self.usage.can_refresh() {
                        self.start_usage_fetch(true);
                        if self.usage.activity_due(now) {
                            self.start_activity_read();
                        }
                    }
                }
                UsageAction::InstallOmp => {
                    self.close_usage();
                    self.open_install_tab();
                }
            }
        }
    }
}
