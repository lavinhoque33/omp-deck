//! Worker-thread plumbing for omp CLI calls: results travel back over a channel and wake the UI.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Duration;

use deck_core::omp::{ModelInfo, MoveOutcome, SettingsCatalog, UpdateStatus};
use serde_json::Value;

use crate::tabs::{Anchor, ClosedTab};

/// The tab that was open on a session being moved.
pub struct MovedTab {
    pub tab: ClosedTab,
    /// Where it sat in the strip; it is reopened there.
    pub anchor: Anchor,
    /// `Some` when it was the active tab: the activation counter after it closed, so the
    /// reopened tab only takes focus if the user has not switched since.
    pub refocus: Option<u64>,
}

/// Results of omp CLI calls, delivered to the UI thread.
pub enum OmpEvent {
    Version(Result<String, String>),
    UpdateChecked(Result<UpdateStatus, String>),
    Catalog(Result<SettingsCatalog, String>),
    /// `omp models --json` for the model dropdown.
    Models(Result<Vec<ModelInfo>, String>),
    /// `outcome`: `Ok(Some(v))` = saved value (`Null` for secrets), `Ok(None)` = reset done.
    SettingDone {
        key: String,
        outcome: Result<Option<Value>, String>,
    },
    MoveDone {
        src: PathBuf,
        dest: PathBuf,
        title: String,
        /// The tab that was open on the session, ready to respawn: on the new path after a
        /// successful move, on the original path after a failed one. `None` = no tab was open.
        tab: Option<Box<MovedTab>>,
        result: Result<MoveOutcome, String>,
    },
    /// `omp` finished deleting a session file (or failed).
    DeleteDone {
        path: PathBuf,
        title: String,
        result: Result<(), String>,
    },
    /// Result of the native folder picker; `None` = cancelled / unavailable.
    FolderPicked(Option<PathBuf>),
    /// A folder for the sidebar scope: typed in the scope menu, or from the native dialog
    /// (`result: None` = dialog cancelled / unavailable).
    ScopeChosen {
        /// Request id; only the latest request's answer is applied.
        generation: u64,
        typed: bool,
        result: Option<Result<PathBuf, String>>,
    },
    /// Filesystem verdict on the typed destination folder (see `transfer::DestCheck`).
    FolderChecked {
        generation: u64,
        result: Result<PathBuf, String>,
    },
}

/// Run `job` on a fresh thread; its event is sent and the UI repainted. A thread that cannot be
/// created is logged; callers that must not lose the job's event use [`try_spawn`].
pub fn spawn<E: Send + 'static>(
    ctx: &egui::Context,
    tx: &Sender<E>,
    job: impl FnOnce() -> E + Send + 'static,
) {
    if let Err(e) = try_spawn(ctx, tx, job) {
        log::error!("cannot start omp worker: {e}");
    }
}

/// Like [`spawn`], but reports a thread-creation failure (the job did not run).
pub fn try_spawn<E: Send + 'static>(
    ctx: &egui::Context,
    tx: &Sender<E>,
    job: impl FnOnce() -> E + Send + 'static,
) -> std::io::Result<()> {
    let (ctx, tx) = (ctx.clone(), tx.clone());
    std::thread::Builder::new()
        .name("omp-worker".into())
        .spawn(move || {
            let _ = tx.send(job());
            ctx.request_repaint();
        })
        .map(drop)
}

/// omp refuses to move a session while another omp process writes it.
pub fn is_live_writer_error(message: &str) -> bool {
    message.contains("another live omp process")
}

pub const MOVE_ATTEMPTS: u32 = 10;
pub const MOVE_RETRY_DELAY: Duration = Duration::from_millis(300);

/// Calls `attempt` up to `max` times, sleeping `delay` between tries while the error says another
/// live omp process holds the session (a just-killed omp needs a moment to exit).
pub fn retry_while_live<T>(
    max: u32,
    delay: Duration,
    mut attempt: impl FnMut() -> Result<T, String>,
) -> Result<T, String> {
    let mut tries = 1;
    loop {
        match attempt() {
            Err(e) if is_live_writer_error(&e) && tries < max => {
                tries += 1;
                std::thread::sleep(delay);
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = "Cannot pack: another live omp process is writing this session";

    #[test]
    fn retries_only_the_live_writer_error() {
        let mut calls = 0;
        let r = retry_while_live(10, Duration::ZERO, || {
            calls += 1;
            if calls < 4 {
                Err(LIVE.to_string())
            } else {
                Ok(calls)
            }
        });
        assert_eq!(r, Ok(4));

        let mut calls = 0;
        let r: Result<(), String> = retry_while_live(10, Duration::ZERO, || {
            calls += 1;
            Err("session not found".into())
        });
        assert_eq!(r, Err("session not found".into()));
        assert_eq!(calls, 1);
    }

    #[test]
    fn gives_up_after_max_attempts() {
        let mut calls = 0;
        let r: Result<(), String> = retry_while_live(10, Duration::ZERO, || {
            calls += 1;
            Err(LIVE.to_string())
        });
        assert_eq!(r, Err(LIVE.to_string()));
        assert_eq!(calls, 10);
    }
}
