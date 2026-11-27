//! Bookmarks follow their tab. A bookmark is an omp session pin, keyed by session id; omp moves a
//! conversation onto a new session file on a rewind (Esc Esc, `/branch`, `/rewind`), a turn's Edit
//! or Retry, an entry fork, a `/btw` branch, `/fork` and a continuation, and a rewind's Undo
//! resumes the old file. When a tab follows such a move ([`Engine::follow_session_files`]) the pin
//! moves with it ([`deck_core::pins::followed_bookmark`]): the new id is pinned first, then the
//! old one unpinned, both through omp, so a failure in between leaves both bookmarked, never none.
//! Moves run one at a time in the order the tabs moved (a quick rewind + Undo applies in order).

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deck_core::omp::Omp;
use deck_core::pins;
use deck_core::sessions::{self, Lineage};

use super::{Engine, Fin};

/// How long the new file's header is waited for: omp's `/fork` names its file before writing it.
const HEADER_WAIT: Duration = Duration::from_secs(3);
const HEADER_POLL: Duration = Duration::from_millis(100);

/// A bookmark move's result: the `(unpinned, pinned)` ids, `None` when nothing moved.
pub(crate) type Followed = Result<Option<(String, String)>, String>;

#[derive(Default)]
pub(super) struct Bookmarks {
    /// Tab moves `(from file, to file)` not checked yet, oldest first.
    queue: VecDeque<(PathBuf, PathBuf)>,
    busy: bool,
    /// Counts the pin writes this gateway made (published as `SessionsView::pins_revision`).
    pub(super) revision: u64,
}

impl Engine {
    /// A tab's omp moved from the session file `from` to `to`.
    pub(super) fn tab_followed(&mut self, from: PathBuf, to: PathBuf) {
        self.bookmarks.queue.push_back((from, to));
        self.next_bookmark_follow();
    }

    fn next_bookmark_follow(&mut self) {
        if self.bookmarks.busy {
            return;
        }
        // Pins are written through omp only.
        let Some(omp) = self.omp.clone() else {
            self.bookmarks.queue.clear();
            return;
        };
        let Some((from, to)) = self.bookmarks.queue.pop_front() else {
            return;
        };
        self.bookmarks.busy = true;
        let agent_dir = self.root.agent_dir();
        self.spawn_worker("bookmark", move || {
            Fin::BookmarkFollow(follow(&omp, &agent_dir, &from, &to))
        });
    }

    pub(super) fn on_bookmark_follow(&mut self, result: Followed) {
        self.bookmarks.busy = false;
        match result {
            Ok(None) => {}
            Ok(Some((from, to))) => {
                log::info!("bookmark moved with its tab: {from} → {to}");
                self.bookmarks.revision += 1;
            }
            Err(e) => {
                // The pin may have been written before the failure.
                log::warn!("moving a bookmark with its tab failed: {e}");
                self.bookmarks.revision += 1;
            }
        }
        self.next_bookmark_follow();
    }
}

/// Moves the bookmark of `from` to `to` when [`pins::followed_bookmark`] says so.
fn follow(omp: &Omp, agent_dir: &Path, from: &Path, to: &Path) -> Followed {
    let from_lineage = match sessions::lineage(from) {
        Ok(Some(l)) => l,
        // Gone (`/delete`, `/move`) or never written: nothing to carry.
        Ok(None) => return Ok(None),
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("reading {}: {e}", from.display())),
    };
    let pinned = pins::read(agent_dir);
    // The common case decides without waiting for the new file.
    if !pinned.contains(&from_lineage.id) {
        return Ok(None);
    }
    let Some(to_lineage) = wait_for_header(to) else {
        return Ok(None);
    };
    let Some((unpin, pin)) =
        pins::followed_bookmark((from, &from_lineage), (to, &to_lineage), &pinned)
    else {
        return Ok(None);
    };
    omp.set_pinned(agent_dir, &pin, true)
        .map_err(|e| format!("bookmarking {pin}: {e}"))?;
    omp.set_pinned(agent_dir, &unpin, false)
        .map_err(|e| format!("removing the bookmark of {unpin} (now also on {pin}): {e}"))?;
    Ok(Some((unpin, pin)))
}

/// The header of `path` once it is written, waiting at most [`HEADER_WAIT`].
fn wait_for_header(path: &Path) -> Option<Lineage> {
    let end = Instant::now() + HEADER_WAIT;
    loop {
        if let Ok(Some(l)) = sessions::lineage(path) {
            return Some(l);
        }
        if Instant::now() >= end {
            return None;
        }
        std::thread::sleep(HEADER_POLL);
    }
}
