//! Session lifecycle operations that run outside omp's composer, shared by every client.
//!
//! Action families and what executes them (no second dispatcher: clients call these routes):
//!
//! | Family | Executor |
//! |---|---|
//! | Composer/hotkey commands: handoff, compact ×4, rename/auto-name, context, retry, plan mode, omp's agent hub, stop; model/effort switch; review insert/send; worktree move/finish prompt | [`crate::controller::SessionController::submit`] (the session's one [`crate::inject::Injector`]) |
//! | Export HTML | [`crate::session_actions::export_html`] (`omp --export`) |
//! | Fork / copy | `deck_core::omp::fork_argv` (launched as a new session), `deck_core::omp::Omp::copy_session`, `deck_core::sessions::find_fork` |
//! | Fork at entry / navigate / subagent kill, revive, chat | `deck_core::ctl` (`CtlCommand`) over the session's control socket |
//! | Stop / kill a running session | [`crate::backend::Backends::stop`] / [`crate::backend::Backends::kill`] |
//! | Move to another folder | [`stop_then_move`] (`deck_core::omp::Omp::move_session` after stopping) |
//! | Delete | [`stop_then_delete`] (`deck_core::omp::Omp::delete_sessions` after stopping) |
//! | Create / attach / restart | `deck_core::mux::Mux` launch commands; host sessions through [`crate::host_remote::HostRemote`], read-only viewers through [`crate::host_remote::HostObserver`] |
//! | Worktree create/inspect/remove, project sync, backup, CLI updates | `deck_core::{worktree, gdrive, omp, selfupdate}` |
//!
//! Native shell, clipboard, window raising and file dialogs are client effects, not operations.

use std::path::Path;
use std::time::Duration;

use deck_core::omp::{DeleteError, DeleteTarget, MoveOutcome, Omp};

use crate::backend::Backends;

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

/// Kill every backend session in `names` (best effort, logged). Blocking.
pub fn kill_all(backends: &Backends, names: &[String], why: &str) {
    for name in names {
        if let Err(e) = backends.kill(name) {
            log::warn!("kill {name} before {why}: {e}");
        }
    }
}

/// User-facing text of a failed delete.
pub fn delete_error_text(e: &DeleteError) -> String {
    match e {
        DeleteError::Live => "it is open in another omp process".to_owned(),
        other => other.to_string(),
    }
}

/// Kill what runs the session (`kill_names`), then move `src` to `dest`, retrying while omp
/// still sees the dying process. Blocking: run on a worker.
pub fn stop_then_move(
    omp: &Omp,
    backends: &Backends,
    kill_names: &[String],
    src: &Path,
    dest: &Path,
) -> Result<MoveOutcome, String> {
    kill_all(backends, kill_names, "move");
    retry_while_live(MOVE_ATTEMPTS, MOVE_RETRY_DELAY, || {
        omp.move_session(src, dest).map_err(|e| e.to_string())
    })
}

/// Kill what runs the sessions (`kill_names`), then let omp delete `targets`. `on_result` runs
/// exactly once per target path, as soon as its outcome is known; a target omp never reported is
/// an error. Blocking: run on a worker.
pub fn stop_then_delete(
    omp: &Omp,
    backends: &Backends,
    kill_names: &[String],
    targets: &[DeleteTarget],
    mut on_result: impl FnMut(&Path, Result<(), String>),
) {
    kill_all(backends, kill_names, "delete");
    let mut reported = std::collections::HashSet::new();
    omp.delete_sessions(targets, |path, result| {
        if reported.insert(path.to_path_buf()) {
            on_result(path, result.map_err(|e| delete_error_text(&e)));
        }
    });
    for t in targets {
        if reported.insert(t.path.clone()) {
            on_result(&t.path, Err("omp did not report a result".to_owned()));
        }
    }
}

/// The first of `names` some backend still lists (a session that survived a failed operation).
/// Blocking.
pub fn surviving_session(backends: &Backends, names: &[String]) -> Option<String> {
    names.iter().find(|n| backends.has_session(n)).cloned()
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
