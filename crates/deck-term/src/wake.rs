//! Output-driven wakeups without requiring an egui frame loop.

use std::sync::Arc;
use std::time::Duration;

/// Request a poll now (`Duration::ZERO`) or no later than the given delay. A headless
/// driver owns its timer; the callback must not block the terminal's output thread.
pub type WakeCallback = Arc<dyn Fn(Duration) + Send + Sync>;

#[derive(Clone)]
pub(crate) enum Wake {
    Gui(egui::Context),
    Headless(WakeCallback),
}

impl Wake {
    pub(crate) fn now(&self) {
        self.after(Duration::ZERO);
    }

    pub(crate) fn after(&self, delay: Duration) {
        match self {
            Self::Gui(ctx) => ctx.request_repaint_after(delay),
            Self::Headless(callback) => callback(delay),
        }
    }
}
