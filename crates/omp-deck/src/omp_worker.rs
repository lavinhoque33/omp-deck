//! Worker-thread plumbing for omp CLI calls: results travel back over a channel and wake the UI.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

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
    /// `Omp::models` (catalog + metrics) for the model dropdown.
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
