//! "Open on desktop" from the gateway (another client, or the `--open` / `--open-url` flags of
//! the launch the gateway made because no window was open).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deck_service::gateway::protocol::Split;

use super::*;
use crate::panes::Side;

/// How long an open waits for its session to show up in the sidebar's scan.
const FIND_TIMEOUT: Duration = Duration::from_secs(20);
/// After this long without the entry the scan is asked again (once).
const RESCAN_AFTER: Duration = Duration::from_secs(2);

/// A session file the gateway asked this window to show.
pub(super) struct DesktopOpen {
    file: PathBuf,
    split: Split,
    since: Instant,
    rescanned: bool,
}

impl DesktopOpen {
    pub(super) fn new(file: PathBuf, split: Split) -> Self {
        Self {
            file,
            split,
            since: Instant::now(),
            rescanned: false,
        }
    }
}

fn side_of(split: Split) -> Option<Side> {
    match split {
        Split::None => None,
        Split::Right => Some(Side::Right),
        Split::Down => Some(Side::Down),
    }
}

impl App {
    /// `OpenSession`: show the session's tab (resuming it when closed) as `split` says, and
    /// raise the window.
    pub(super) fn on_open_session(&mut self, file: PathBuf, split: Split) {
        self.desktop_opens.push(DesktopOpen::new(file, split));
        self.raise_window(None);
        self.ctx.request_repaint();
    }

    /// `CloseSession`: detach the tab showing `file`, if one is open.
    pub(super) fn on_close_session(&mut self, file: &Path) {
        self.desktop_opens.retain(|o| o.file != file);
        if let Some(i) = self
            .tabs
            .iter()
            .position(|t| t.session_path() == Some(file))
        {
            self.request_close(i);
        }
    }

    /// Runs the waiting opens whose session the sidebar's scan knows (or that already has a
    /// tab); one that never shows up is reported.
    pub(super) fn run_desktop_opens(&mut self) {
        if self.desktop_opens.is_empty() {
            return;
        }
        for mut open in std::mem::take(&mut self.desktop_opens) {
            if let Some(entry) = self.find_entry(&open.file) {
                match side_of(open.split) {
                    Some(side) => self.split_session(&entry, side),
                    None => self.open_session(&entry),
                }
                continue;
            }
            if let Some(i) = self
                .tabs
                .iter()
                .position(|t| t.session_path() == Some(open.file.as_path()))
            {
                self.show_tab_as(i, open.split);
                continue;
            }
            let waited = open.since.elapsed();
            if waited >= FIND_TIMEOUT {
                self.toasts.error(
                    &self.ctx,
                    format!("Could not open {}: no such session", open.file.display()),
                );
                continue;
            }
            if waited >= RESCAN_AFTER && !open.rescanned {
                open.rescanned = true;
                self.refresh_sessions();
            }
            self.desktop_opens.push(open);
            self.ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    /// Focus tab `i`, or show it in a new pane when `split` asks and it has none yet.
    fn show_tab_as(&mut self, i: usize, split: Split) {
        let in_pane = self.tabs[i]
            .pane_uid()
            .is_some_and(|u| self.split.slot_of(u).is_some());
        match side_of(split) {
            Some(side) if !in_pane => self.split_tab(i, side),
            _ => self.activate(i),
        }
    }
}
