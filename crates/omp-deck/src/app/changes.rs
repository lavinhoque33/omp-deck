//! App-side glue for the Changes panel: which repository it follows, the refresh rules, the git
//! workers (list, diff, stage/unstage/discard/delete, copy patch) and the confirmations.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deck_core::changes::{self, ChangedFile, Changes, ChangesError, FileDiff};
use egui::{Frame, Margin};

use super::{App, AppEvent, PendingConfirm};
use crate::changes_view::{
    self, ChangesAction, DiffModel, DiffPane, FileKey, LoadState, ShownDiff, ViewState,
};
use crate::confirm::Confirm;
use crate::omp_worker;
use crate::review;
use crate::tabs::{Tab, TabRole};
use crate::theme::Palette;
use deck_service::controller::{Command, RequestId};

/// How often the open panel re-reads the repository.
const REFRESH_EVERY: Duration = Duration::from_secs(3);
/// Panel width until the user resizes it.
const DEFAULT_WIDTH: f32 = 460.0;
/// Paths listed in a confirmation before "and N more".
const CONFIRM_PATHS: usize = 12;

/// What a git worker reports.
pub(super) enum ChangesEvent {
    Listed {
        generation: u64,
        result: Result<Option<Changes>, String>,
    },
    Diff {
        generation: u64,
        key: FileKey,
        result: Result<FileDiff, String>,
    },
    Wrote {
        label: &'static str,
        result: Result<(), String>,
    },
    Patch(Result<String, String>),
}

/// A destructive write waiting for the user's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WriteKind {
    Discard,
    DeleteUntracked,
}

pub(super) struct PendingWrite {
    kind: WriteKind,
    root: PathBuf,
    files: Vec<ChangedFile>,
}

/// A review handed to a tab's controller: which comments leave the review once it is delivered.
struct PendingReview {
    uid: u64,
    root: PathBuf,
    ids: Vec<u64>,
    send: bool,
    /// The controller request that carries it: only its own outcome settles this hand-off.
    request: RequestId,
}

/// Review hand-offs in flight, at most one per tab (a tab's controller refuses a second review
/// while one runs). Each completion clears exactly the comments of its own hand-off.
#[derive(Default)]
struct PendingReviews(Vec<PendingReview>);

impl PendingReviews {
    /// Records a hand-off; an older one of the same tab (it was refused or dropped) is replaced.
    fn start(&mut self, p: PendingReview) {
        self.0.retain(|q| q.uid != p.uid);
        self.0.push(p);
    }

    /// The hand-off of tab `uid` that request `request` just settled.
    fn take(&mut self, uid: u64, send: bool, request: RequestId) -> Option<PendingReview> {
        let i = self
            .0
            .iter()
            .position(|p| p.uid == uid && p.send == send && p.request == request)?;
        Some(self.0.swap_remove(i))
    }
}

#[derive(Default)]
pub(super) struct ChangesUi {
    pub(super) open: bool,
    view: ViewState,
    /// Tab the listing belongs to.
    uid: Option<u64>,
    /// Folder the last listing was requested for.
    cwd: Option<PathBuf>,
    /// Latest listing request; older answers are dropped.
    list_generation: u64,
    list_busy: bool,
    /// A refresh was asked for while one was running: run again when it lands.
    list_again: bool,
    last_started: Option<Instant>,
    diff_generation: u64,
    /// The review request in flight (cleared when its controller reports it settled).
    pending_reviews: PendingReviews,
}

impl ChangesUi {
    /// Points the panel at `(uid, cwd)`. A different tab or folder drops the old listing, its
    /// root and selection at once and invalidates every request still in flight, so nothing of the
    /// old repository stays actionable and a late answer for it is ignored. Returns whether the
    /// target changed (the caller then queues the new scan).
    fn retarget(&mut self, uid: u64, cwd: PathBuf) -> bool {
        let tab_changed = self.uid != Some(uid);
        let moved = self.cwd.as_ref() != Some(&cwd);
        if !tab_changed && !moved {
            return false;
        }
        self.uid = Some(uid);
        self.cwd = Some(cwd);
        self.view.clear_listing();
        self.view.notice = None;
        self.view.busy = None;
        self.view.load = LoadState::Loading;
        self.list_generation += 1;
        self.diff_generation += 1;
        true
    }
}

/// The folder the panel follows: the live cwd of the active session tab (what the status bar
/// shows), else the tab's stored cwd.
fn target_of(app: &App) -> Option<(u64, PathBuf)> {
    let Some(Tab::Terminal(t)) = app.tabs.get(app.active) else {
        return None;
    };
    let live = (t.role == TabRole::Session)
        .then(|| app.status.latest())
        .flatten()
        .map(|s| s.cwd.clone())
        .filter(|c| !c.as_os_str().is_empty());
    Some((t.uid, live.unwrap_or_else(|| t.cwd.clone())))
}

/// Confirmation text naming what is about to be lost.
pub(super) fn confirm_for(kind: WriteKind, files: &[ChangedFile]) -> Confirm {
    let n = files.len();
    let names: Vec<&str> = files
        .iter()
        .take(CONFIRM_PATHS)
        .map(|f| f.path.as_str())
        .collect();
    let more = n.saturating_sub(names.len());
    let mut list = names.join("\n");
    if more > 0 {
        list.push_str(&format!("\n… and {more} more"));
    }
    let noun = if n == 1 { "file" } else { "files" };
    match kind {
        WriteKind::Discard => Confirm {
            title: format!("Discard changes to {n} {noun}?"),
            body: format!(
                "The unstaged changes to these files are thrown away and cannot be recovered. Staged changes are kept.\n\n{list}"
            ),
            confirm_label: "Discard".to_owned(),
            danger: true,
        },
        WriteKind::DeleteUntracked => {
            let dirs = if files.iter().any(|f| f.is_dir) {
                " Untracked files inside a folder are deleted too; ignored files and nested repositories inside it are kept."
            } else {
                ""
            };
            Confirm {
                title: format!("Delete {n} untracked {noun}?"),
                body: format!(
                    "These files are not tracked by git and cannot be recovered.{dirs}\n\n{list}"
                ),
                confirm_label: "Delete".to_owned(),
                danger: true,
            }
        }
    }
}

impl App {
    /// Ctrl+Shift+G, the git chip, the palette: show or hide the panel (it replaces the agents
    /// dashboard, which would hide it).
    pub(super) fn toggle_changes_panel(&mut self) {
        if self.changes.open {
            self.changes.open = false;
        } else {
            self.changes.open = true;
            self.close_dashboard();
            // The next frame re-reads the repository at once.
            self.changes.last_started = None;
        }
        self.focus_pending = true;
        self.ctx.request_repaint();
    }

    /// The panel can follow the active tab.
    pub(super) fn changes_available(&self) -> bool {
        matches!(self.tabs.get(self.active), Some(Tab::Terminal(_)))
    }

    /// Refresh rules: on open, on tab switch, when the folder changes, after writes, every 3 s.
    pub(super) fn drive_changes(&mut self, ctx: &egui::Context) {
        if !self.changes.open {
            return;
        }
        let Some((uid, cwd)) = target_of(self) else {
            let ch = &mut self.changes;
            if ch.uid.take().is_some() {
                ch.cwd = None;
                ch.view.clear_listing();
                ch.list_generation += 1;
            }
            ch.view.load = LoadState::NoTab;
            return;
        };
        if self.changes.retarget(uid, cwd) {
            self.changes_refresh_now();
            return;
        }
        let ch = &mut self.changes;
        let due = ch.last_started.is_none_or(|t| t.elapsed() >= REFRESH_EVERY);
        if due {
            self.changes_refresh_now();
        } else if let Some(t) = ch.last_started {
            ctx.request_repaint_after(REFRESH_EVERY.saturating_sub(t.elapsed()));
        }
    }

    /// Reads the repository on a worker (one at a time; a request during a run repeats it).
    pub(super) fn changes_refresh_now(&mut self) {
        let Some(cwd) = self
            .changes
            .cwd
            .clone()
            .or_else(|| target_of(self).map(|t| t.1))
        else {
            return;
        };
        let ch = &mut self.changes;
        if ch.cwd.is_none() {
            ch.cwd = Some(cwd.clone());
        }
        if ch.list_busy {
            ch.list_again = true;
            return;
        }
        ch.list_busy = true;
        ch.list_again = false;
        ch.last_started = Some(Instant::now());
        ch.list_generation += 1;
        let generation = ch.list_generation;
        if ch.view.load != LoadState::Ready {
            ch.view.load = LoadState::Loading;
        }
        omp_worker::spawn(&self.ctx, &self.events_tx, move || {
            AppEvent::Changes(ChangesEvent::Listed {
                generation,
                result: changes::list(&cwd).map_err(|e| e.to_string()),
            })
        });
    }

    fn changes_request_diff(&mut self) {
        let ch = &mut self.changes;
        let (Some(file), Some(key), Some(root)) = (
            ch.view.selected_file().cloned(),
            ch.view.selected.clone(),
            ch.view.root.clone(),
        ) else {
            return;
        };
        ch.diff_generation += 1;
        let generation = ch.diff_generation;
        let same = matches!(&ch.view.diff, DiffPane::Ready(d) if d.key == key);
        if !same {
            ch.view.diff = DiffPane::Loading;
        }
        let side = key.section.side();
        omp_worker::spawn(&self.ctx, &self.events_tx, move || {
            AppEvent::Changes(ChangesEvent::Diff {
                generation,
                key,
                result: changes::diff(&root, &file, side).map_err(|e| e.to_string()),
            })
        });
    }

    pub(super) fn on_changes_event(&mut self, ev: ChangesEvent) {
        match ev {
            ChangesEvent::Listed { generation, result } => {
                self.changes.list_busy = false;
                if generation == self.changes.list_generation {
                    match result {
                        Ok(listing) => {
                            let ch = &mut self.changes;
                            if listing.as_ref().map(|c| &c.root) != ch.view.root.as_ref() {
                                ch.view.selected = None;
                                ch.view.diff = DiffPane::Idle;
                            }
                            ch.view.apply_listing(listing);
                            self.changes_request_diff();
                        }
                        Err(e) => {
                            self.changes.view.clear_listing();
                            self.changes.view.load = LoadState::Failed(e);
                        }
                    }
                }
                if self.changes.list_again {
                    self.changes_refresh_now();
                }
            }
            ChangesEvent::Diff {
                generation,
                key,
                result,
            } => {
                if generation != self.changes.diff_generation
                    || self.changes.view.selected.as_ref() != Some(&key)
                {
                    return;
                }
                let view = &mut self.changes.view;
                match result {
                    Err(e) => view.diff = DiffPane::Failed(e),
                    Ok(d) => {
                        if let DiffPane::Ready(shown) = &mut view.diff
                            && shown.key == key
                            && shown.model.source() == d.text
                        {
                            (shown.binary, shown.truncated) = (d.binary, d.truncated);
                            (shown.added, shown.removed) = (d.added, d.removed);
                        } else {
                            view.diff = DiffPane::Ready(Box::new(ShownDiff {
                                key,
                                model: DiffModel::parse(d.text),
                                binary: d.binary,
                                truncated: d.truncated,
                                added: d.added,
                                removed: d.removed,
                            }));
                        }
                    }
                }
            }
            ChangesEvent::Wrote { label, result } => {
                self.changes.view.busy = None;
                match result {
                    Ok(()) => self.changes.view.notice = None,
                    Err(e) => {
                        self.toasts.error(&self.ctx, format!("{label} failed: {e}"));
                        self.changes.view.notice = Some(format!("{label} failed: {e}"));
                    }
                }
                self.changes_refresh_now();
            }
            ChangesEvent::Patch(Ok(patch)) if patch.trim().is_empty() => {
                self.toasts
                    .info(&self.ctx, "No staged or unstaged changes to copy");
            }
            ChangesEvent::Patch(Ok(patch)) => self.copy_text(&patch, "diff"),
            ChangesEvent::Patch(Err(e)) => {
                self.toasts
                    .error(&self.ctx, format!("Could not build the diff: {e}"));
            }
        }
    }

    /// Selected files of the current listing (a stale index is skipped).
    fn changes_files(&self, ids: &[usize]) -> Vec<ChangedFile> {
        ids.iter()
            .filter_map(|&i| self.changes.view.files.get(i).cloned())
            .collect()
    }

    fn changes_write(
        &mut self,
        label: &'static str,
        files: Vec<ChangedFile>,
        run: fn(&Path, &[&ChangedFile]) -> Result<(), ChangesError>,
    ) {
        let Some(root) = self.changes.view.root.clone() else {
            return;
        };
        if files.is_empty() || self.changes.view.busy.is_some() {
            return;
        }
        self.changes.view.busy = Some(format!("{label}…"));
        omp_worker::spawn(&self.ctx, &self.events_tx, move || {
            let refs: Vec<&ChangedFile> = files.iter().collect();
            AppEvent::Changes(ChangesEvent::Wrote {
                label,
                result: run(&root, &refs).map_err(|e| e.to_string()),
            })
        });
    }

    fn ask_changes_write(&mut self, kind: WriteKind, ids: &[usize]) {
        let files = self.changes_files(ids);
        let Some(root) = self.changes.view.root.clone() else {
            return;
        };
        if files.is_empty() || self.confirm.is_some() {
            return;
        }
        self.confirm = Some((
            confirm_for(kind, &files),
            PendingConfirm::Changes(PendingWrite { kind, root, files }),
        ));
    }

    /// The user confirmed a discard / delete: run it unless the panel moved to another repository.
    pub(super) fn execute_changes_write(&mut self, w: PendingWrite) {
        if self.changes.view.root.as_deref() != Some(w.root.as_path()) {
            self.toasts.error(
                &self.ctx,
                "The repository changed while the question was open; nothing was changed"
                    .to_owned(),
            );
            return;
        }
        match w.kind {
            WriteKind::Discard => self.changes_write("Discard", w.files, changes::discard),
            WriteKind::DeleteUntracked => {
                self.changes_write("Delete", w.files, changes::delete_untracked);
            }
        }
    }

    fn apply_changes_action(&mut self, action: ChangesAction) {
        match action {
            ChangesAction::Select(key) => {
                if self.changes.view.selected.as_ref() != Some(&key) {
                    self.changes.view.selected = Some(key);
                    self.changes.view.diff = DiffPane::Loading;
                    self.changes_request_diff();
                }
            }
            ChangesAction::Stage(ids) => {
                let files = self.changes_files(&ids);
                self.changes_write("Stage", files, changes::stage);
            }
            ChangesAction::Unstage(ids) => {
                let files = self.changes_files(&ids);
                self.changes_write("Unstage", files, changes::unstage);
            }
            ChangesAction::Discard(ids) => self.ask_changes_write(WriteKind::Discard, &ids),
            ChangesAction::DeleteUntracked(ids) => {
                self.ask_changes_write(WriteKind::DeleteUntracked, &ids);
            }
            ChangesAction::OpenFolder(i) => {
                if let (Some(root), Some(f)) = (
                    self.changes.view.root.clone(),
                    self.changes.view.files.get(i),
                ) {
                    let full = root.join(&f.os_path);
                    let dir = if f.is_dir {
                        full
                    } else {
                        full.parent().map_or(root, Path::to_path_buf)
                    };
                    self.open_dir(dir);
                }
            }
            ChangesAction::CopyPath(i) => {
                if let (Some(root), Some(f)) =
                    (&self.changes.view.root, self.changes.view.files.get(i))
                {
                    let text = root.join(&f.os_path).display().to_string();
                    self.copy_text(&text, "path");
                }
            }
            ChangesAction::CopyDiff => {
                if let Some(root) = self.changes.view.root.clone() {
                    omp_worker::spawn(&self.ctx, &self.events_tx, move || {
                        AppEvent::Changes(ChangesEvent::Patch(
                            changes::patch(&root).map_err(|e| e.to_string()),
                        ))
                    });
                }
            }
            ChangesAction::Refresh => self.changes_refresh_now(),
            ChangesAction::InsertReview => self.hand_review(false),
            ChangesAction::SendReview => self.hand_review(true),
            ChangesAction::Close => {
                self.changes.open = false;
                self.focus_pending = true;
            }
        }
    }

    /// **Insert into omp** / **Send**: queues the review on the panel's tab (looked up again now).
    /// The review is cleared only when the controller reports the request delivered
    /// ([`Self::review_delivered`]); every refusal (omp busy, a dialog open, draft declined, no
    /// bracketed paste) keeps it.
    fn hand_review(&mut self, send: bool) {
        let (Some(uid), Some(root)) = (self.changes.uid, self.changes.view.root.clone()) else {
            return;
        };
        let comments = self
            .changes
            .view
            .reviews
            .of(&root)
            .map(|r| r.comments().to_vec())
            .unwrap_or_default();
        let text = match review::build_prompt(&comments) {
            Ok(t) => t,
            Err(e) => {
                self.toasts.error(&self.ctx, e.to_string());
                return;
            }
        };
        let Some(target) = self
            .tabs
            .iter()
            .filter_map(Tab::term)
            .find(|t| t.uid == uid)
            .map(|t| t.ctl.target().clone())
        else {
            self.toasts
                .error(&self.ctx, "The session tab was closed; nothing was sent");
            return;
        };
        let Some(request) = self.submit_command(&target, Command::Review { text, send }) else {
            return;
        };
        self.changes.pending_reviews.start(PendingReview {
            uid,
            root,
            ids: comments.iter().map(|c| c.id).collect(),
            send,
            request,
        });
    }

    /// A review request ended without reaching omp (refused, declined, cancelled): its comments
    /// stay in the review.
    pub(super) fn review_dropped(&mut self, uid: u64, send: bool, request: RequestId) {
        self.changes.pending_reviews.take(uid, send, request);
    }

    /// The controller delivered a review request on tab `uid`: those comments are in omp's hands
    /// now (delivered bytes, not proof that omp acted on them).
    pub(super) fn review_delivered(&mut self, uid: u64, send: bool, request: RequestId) {
        let Some(p) = self.changes.pending_reviews.take(uid, send, request) else {
            return;
        };
        let n = p.ids.len();
        self.changes.view.reviews.of_mut(&p.root).remove_all(&p.ids);
        let what = if n == 1 {
            "1 review comment".to_owned()
        } else {
            format!("{n} review comments")
        };
        let msg = if send {
            format!("Sent {what} to omp")
        } else {
            format!("Inserted {what} into omp's composer")
        };
        self.toasts.success(&self.ctx, msg);
    }

    /// Open a folder with the desktop's file manager off the UI thread.
    fn open_dir(&self, dir: PathBuf) {
        let (tx, ctx) = (self.events_tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            if let Err(e) = deck_core::platform::open_path(dir.as_os_str()) {
                let _ = tx.send(AppEvent::Toast(
                    crate::toast::ToastKind::Error,
                    format!("Could not open the folder: {e}"),
                ));
                ctx.request_repaint();
            }
        });
    }

    pub(super) fn show_changes_panel(&mut self, ui: &mut egui::Ui) {
        if !self.changes.open || !self.changes_available() || self.dash.open {
            return;
        }
        let pal = Palette::of_ui(ui);
        let stored = self.cfg.changes_panel_width;
        let width = if stored > 0.0 { stored } else { DEFAULT_WIDTH };
        let shown = egui::Panel::right("changes")
            .resizable(true)
            .default_size(width)
            .size_range(320.0..=900.0)
            .frame(
                Frame::NONE
                    .fill(pal.panel)
                    .inner_margin(Margin::symmetric(12, 12)),
            )
            .show(ui, |ui| changes_view::show(ui, &mut self.changes.view));
        // A comment editor owns the keyboard: the terminal must not take it back while the user
        // types (every other place that asks for terminal focus runs before this, or on a click).
        if self.changes.view.editor.as_ref().is_some_and(|e| e.active) {
            self.focus_pending = false;
        }
        for a in shown.inner {
            self.apply_changes_action(a);
        }
        let now = shown.response.rect.width();
        if (now - width).abs() > 1.0 && !ui.input(|i| i.pointer.any_down()) {
            self.cfg.changes_panel_width = now;
            self.dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deck_core::changes::Change;
    use std::ffi::OsString;

    fn loaded(root: &str) -> ChangesUi {
        let mut ch = ChangesUi::default();
        assert!(ch.retarget(1, PathBuf::from(root)));
        ch.view.apply_listing(Some(Changes {
            root: PathBuf::from(root),
            branch: Some("main".into()),
            files: vec![ChangedFile {
                path: "a.txt".into(),
                os_path: OsString::from("a.txt"),
                orig_path: None,
                staged: None,
                unstaged: Some(Change::Modified),
                untracked: false,
                conflicted: false,
                is_dir: false,
            }],
        }));
        ch
    }

    #[test]
    fn a_cwd_move_drops_the_old_repository_and_invalidates_requests() {
        let mut ch = loaded("/repo-a");
        assert_eq!(ch.view.load, LoadState::Ready);
        let (list, diff) = (ch.list_generation, ch.diff_generation);

        assert!(
            ch.retarget(1, PathBuf::from("/repo-b")),
            "same tab, new folder"
        );
        assert_eq!(ch.view.load, LoadState::Loading);
        assert!(ch.view.root.is_none() && ch.view.files.is_empty());
        assert!(ch.view.selected.is_none());
        assert!(
            ch.list_generation > list,
            "an in-flight listing is now stale"
        );
        assert!(ch.diff_generation > diff);
    }

    #[test]
    fn each_tabs_review_handoff_completes_on_its_own() {
        let p = |uid, root: &str, ids: Vec<u64>| PendingReview {
            uid,
            root: PathBuf::from(root),
            ids,
            send: true,
            request: RequestId(uid),
        };
        let mut pending = PendingReviews::default();
        pending.start(p(1, "/a", vec![1, 2]));
        pending.start(p(2, "/b", vec![7]));
        // Tab 2's hand-off did not overwrite tab 1's.
        let done = pending.take(1, true, RequestId(1)).unwrap();
        assert_eq!((done.root, done.ids), (PathBuf::from("/a"), vec![1, 2]));
        assert!(
            pending.take(1, true, RequestId(1)).is_none(),
            "already completed"
        );
        assert!(
            pending.take(2, false, RequestId(2)).is_none(),
            "other kind of job"
        );
        assert_eq!(pending.take(2, true, RequestId(2)).unwrap().ids, vec![7]);
        // A retry on the same tab replaces its refused predecessor.
        pending.start(p(3, "/c", vec![1]));
        pending.start(p(3, "/c", vec![1, 2]));
        assert_eq!(pending.take(3, true, RequestId(3)).unwrap().ids, vec![1, 2]);
        assert!(pending.take(3, true, RequestId(3)).is_none());
    }

    #[test]
    fn the_same_target_changes_nothing() {
        let mut ch = loaded("/repo-a");
        let list = ch.list_generation;
        assert!(!ch.retarget(1, PathBuf::from("/repo-a")));
        assert_eq!(ch.view.load, LoadState::Ready);
        assert_eq!(ch.list_generation, list);
        assert_eq!(ch.view.files.len(), 1);
    }
}
