//! App-side glue for the Changes panel: which repository it follows, the gateway topic that keeps
//! its listing current, diffs, the git writes (stage/unstage/discard/delete) and the
//! confirmations. The listing, the diffs and the writes run in the gateway; this module only asks
//! and shows.

use std::path::{Path, PathBuf};

use deck_core::changes::{ChangedFile, Changes, FileDiff};
use deck_service::controller::{Command, RequestId};
use deck_service::gateway::protocol::{Answer, Mutation, Query, Topic, TopicValue};
use egui::Margin;

use super::{App, PendingConfirm};
use crate::changes_view::{
    self, ChangesAction, DiffModel, DiffPane, FileKey, LoadState, ShownDiff, ViewState,
};
use crate::confirm::Confirm;
use crate::review;
use crate::tabs::{Tab, TabRole};
use crate::theme::{self, Palette};

/// Panel width until the user resizes it.
const DEFAULT_WIDTH: f32 = 460.0;
/// Paths listed in a confirmation before "and N more".
const CONFIRM_PATHS: usize = 12;
/// Shown while the gateway cannot be reached: the listing is the last one received.
const OFFLINE: &str =
    "The OmpDeck gateway is not reachable — this is the last list received; changes are paused.";

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
    /// Folder the panel follows.
    cwd: Option<PathBuf>,
    /// The folder whose gateway topic this window is subscribed to.
    followed: Option<PathBuf>,
    /// Revision of the cached listing last shown.
    seen_rev: u64,
    diff_generation: u64,
    /// The offline notice is the one shown in the panel.
    offline_shown: bool,
    /// The review request in flight (cleared when its controller reports it settled).
    pending_reviews: PendingReviews,
}

impl ChangesUi {
    /// Points the panel at `(uid, cwd)`. A different tab or folder drops the old listing, its
    /// root and selection at once and invalidates every request still in flight, so nothing of the
    /// old repository stays actionable and a late answer for it is ignored. Returns whether the
    /// target changed (the caller then follows the new folder's topic).
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
        self.offline_shown = false;
        self.view.busy = None;
        self.view.load = LoadState::Loading;
        self.seen_rev = 0;
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
        .then(|| app.gw.status_of(t.target()))
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
            // The next frame subscribes and reads the cache at once.
            self.changes.seen_rev = 0;
        }
        self.focus_pending = true;
        self.ctx.request_repaint();
    }

    /// The panel can follow the active tab.
    pub(super) fn changes_available(&self) -> bool {
        matches!(self.tabs.get(self.active), Some(Tab::Terminal(_)))
    }

    /// Stops following the folder's gateway topic (the gateway then stops watching it).
    fn unfollow_changes(&mut self) {
        if let Some(path) = self.changes.followed.take() {
            self.gw.unsubscribe(&Topic::Changes(path));
        }
    }

    /// Follows the gateway's listing of `cwd`; the gateway re-reads the repository on its own
    /// schedule and right after every write.
    fn follow_changes(&mut self, cwd: &Path) {
        if self.changes.followed.as_deref() == Some(cwd) {
            return;
        }
        self.unfollow_changes();
        self.gw.subscribe(Topic::Changes(cwd.to_path_buf()));
        self.changes.followed = Some(cwd.to_path_buf());
        self.changes.seen_rev = 0;
    }

    /// Per frame while the panel is open: follow the active tab's folder and show the newest
    /// listing the gateway published.
    pub(super) fn drive_changes(&mut self, _ctx: &egui::Context) {
        if !self.changes.open {
            self.unfollow_changes();
            return;
        }
        let Some((uid, cwd)) = target_of(self) else {
            self.unfollow_changes();
            let ch = &mut self.changes;
            if ch.uid.take().is_some() {
                ch.cwd = None;
                ch.view.clear_listing();
                ch.diff_generation += 1;
            }
            ch.view.load = LoadState::NoTab;
            return;
        };
        self.changes.retarget(uid, cwd.clone());
        self.follow_changes(&cwd);
        self.take_listing(&cwd);
        self.show_offline_notice();
    }

    /// The panel is read-only while the gateway is away: say so, and take the notice back when
    /// it returns.
    fn show_offline_notice(&mut self) {
        let up = self.gw.is_up();
        let ch = &mut self.changes;
        if !up && ch.view.notice.is_none() {
            ch.view.notice = Some(OFFLINE.to_owned());
            ch.offline_shown = true;
        } else if up && ch.offline_shown {
            ch.offline_shown = false;
            if ch.view.notice.as_deref() == Some(OFFLINE) {
                ch.view.notice = None;
            }
        }
    }

    /// Applies the cached listing of `cwd` when it is newer than the one shown.
    fn take_listing(&mut self, cwd: &Path) {
        let topic = Topic::Changes(cwd.to_path_buf());
        let rev = self.gw.revision(&topic);
        if rev == 0 || rev == self.changes.seen_rev {
            return;
        }
        let Some(TopicValue::Changes(result)) = self.gw.topic(&topic) else {
            return;
        };
        self.changes.seen_rev = rev;
        match result {
            Ok(listing) => {
                let ch = &mut self.changes;
                if listing.as_ref().map(|c: &Changes| &c.root) != ch.view.root.as_ref() {
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
        self.gw
            .query(Query::Diff { root, file, side }, move |app, r| {
                let result = match r {
                    Ok(Answer::Diff(d)) => Ok(d),
                    Ok(_) => Err("the gateway answered with something else".to_owned()),
                    Err(e) => Err(e),
                };
                app.on_changes_diff(generation, key, result);
            });
    }

    fn on_changes_diff(&mut self, generation: u64, key: FileKey, result: Result<FileDiff, String>) {
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

    fn on_changes_patch(&mut self, result: Result<String, String>) {
        match result {
            Ok(patch) if patch.trim().is_empty() => {
                self.toasts
                    .info(&self.ctx, "No staged or unstaged changes to copy");
            }
            Ok(patch) => self.copy_text(&patch, "diff"),
            Err(e) => {
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

    /// Sends a git write to the gateway. The listing follows from the gateway's own refresh.
    fn changes_write(
        &mut self,
        label: &'static str,
        files: Vec<ChangedFile>,
        op: fn(PathBuf, Vec<ChangedFile>) -> Mutation,
    ) {
        let Some(root) = self.changes.view.root.clone() else {
            return;
        };
        if files.is_empty() || self.changes.view.busy.is_some() {
            return;
        }
        self.changes.view.busy = Some(format!("{label}…"));
        self.gw.mutate(op(root.clone(), files), move |app, r| {
            if app.changes.view.root.as_deref() != Some(root.as_path()) {
                return;
            }
            app.changes.view.busy = None;
            match r {
                Ok(_) => app.changes.view.notice = None,
                Err(e) => {
                    app.toasts.error(&app.ctx, format!("{label} failed: {e}"));
                    app.changes.view.notice = Some(format!("{label} failed: {e}"));
                }
            }
            // The gateway re-lists after a write; read what it already has.
            app.changes.seen_rev = 0;
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
            WriteKind::Discard => {
                self.changes_write("Discard", w.files, |root, files| Mutation::Discard {
                    root,
                    files,
                });
            }
            WriteKind::DeleteUntracked => {
                self.changes_write("Delete", w.files, |root, files| Mutation::DeleteUntracked {
                    root,
                    files,
                });
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
                self.changes_write("Stage", files, |root, files| Mutation::Stage {
                    root,
                    files,
                });
            }
            ChangesAction::Unstage(ids) => {
                let files = self.changes_files(&ids);
                self.changes_write("Unstage", files, |root, files| Mutation::Unstage {
                    root,
                    files,
                });
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
                    self.gw.query(Query::Patch { root }, |app, r| {
                        let result = match r {
                            Ok(Answer::Patch(p)) => Ok(p),
                            Ok(_) => Err("the gateway answered with something else".to_owned()),
                            Err(e) => Err(e),
                        };
                        app.on_changes_patch(result);
                    });
                }
            }
            ChangesAction::Refresh => {
                self.changes.seen_rev = 0;
                self.changes_request_diff();
            }
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
            .map(|t| t.target().clone())
        else {
            self.toasts
                .error(&self.ctx, "The session tab was closed; nothing was sent");
            return;
        };
        let ids: Vec<u64> = comments.iter().map(|c| c.id).collect();
        // The hand-off is only recorded once the gateway accepted the command: until then it is
        // just a local request, and a refusal leaves the review exactly as it was.
        self.submit_command(
            &target,
            Command::Review { text, send },
            move |app, request| {
                app.changes.pending_reviews.start(PendingReview {
                    uid,
                    root,
                    ids,
                    send,
                    request,
                });
            },
        );
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
    fn open_dir(&mut self, dir: PathBuf) {
        self.gw.native(
            move || deck_core::platform::open_path(dir.as_os_str()).map_err(|e| e.to_string()),
            |app, r| {
                if let Err(e) = r {
                    app.toasts
                        .error(&app.ctx, format!("Could not open the folder: {e}"));
                }
            },
        );
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
            .show_separator_line(theme::skin::right_panel_separator_line())
            .frame(theme::skin::right_panel_frame(
                pal,
                Margin::symmetric(12, 12),
            ))
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
        ch.seen_rev = 7;
        let diff = ch.diff_generation;

        assert!(
            ch.retarget(1, PathBuf::from("/repo-b")),
            "same tab, new folder"
        );
        assert_eq!(ch.view.load, LoadState::Loading);
        assert!(ch.view.root.is_none() && ch.view.files.is_empty());
        assert!(ch.view.selected.is_none());
        assert_eq!(ch.seen_rev, 0, "the new folder's listing is read afresh");
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
        ch.seen_rev = 4;
        assert!(!ch.retarget(1, PathBuf::from("/repo-a")));
        assert_eq!(ch.view.load, LoadState::Ready);
        assert_eq!(ch.seen_rev, 4);
        assert_eq!(ch.view.files.len(), 1);
    }
}
