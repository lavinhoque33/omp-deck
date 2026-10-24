//! App-side glue for git worktree sessions: the new-worktree and move dialogs, creating worktrees
//! through omp, finishing (ask the agent, wait, remove once merged) and removing them, and the
//! tab bookkeeping that follows a session moving into a worktree.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use deck_core::config::RemovedWorktree;
use deck_core::mux::Mux;
use deck_core::sessions::SessionEntry;
use deck_core::status;
use deck_core::worktree::{self, Removed, RepoInfo, Worktree, WorktreeError, WorktreeStatus};

use super::{App, AppEvent, PendingConfirm};
use crate::confirm::Confirm;
use crate::inject::{Job, Requires};
use crate::statusfmt::Activity;
use crate::tabs::{Tab, TabRole};
use crate::transfer;
use crate::worktree_dialog::{CheckRes, Kind, Outcome, Plan, WorktreeDialog, brief};
use crate::worktree_ui::{
    self, FINISH_LABEL, FINISH_TAB_TIMEOUT, Finishing, MOVE_LABEL, MergeTarget, Step, Verdict,
};

/// A move that was typed into a tab: the session reappears in the worktree omp creates.
const MOVE_EXPIRES: Duration = Duration::from_secs(10 * 60);

/// Why a worktree is being inspected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Purpose {
    /// "Remove worktree…": refuse unless clean and merged, else ask.
    Remove,
    /// "Finish worktree": learn the merge target, then start waiting.
    Start,
    /// The agent's turn ended while finishing.
    Check,
}

/// What a worker reports about worktrees.
pub(super) enum WtEvent {
    Repo(Result<RepoInfo, WorktreeError>),
    Checked(CheckRes),
    Folder(Option<PathBuf>),
    Created {
        dest: PathBuf,
        record_base: Option<String>,
        result: Result<(), String>,
    },
    Inspected {
        purpose: Purpose,
        wt: Worktree,
        result: Result<(String, WorktreeStatus), String>,
    },
    Removed {
        wt: Worktree,
        result: Result<Removed, String>,
    },
    /// Where the finish prompt tells the agent to merge (or why that is unknown).
    Target {
        path: PathBuf,
        result: Result<MergeTarget, String>,
    },
    /// The tabs to stop for a removal were decided (or a tab could not be judged).
    Classified {
        wt: Worktree,
        base: String,
        result: Result<Vec<u64>, String>,
    },
    /// Removed-worktree entries whose primary checkout is gone.
    Orphaned(Vec<PathBuf>),
}

/// A running session tab, snapshotted for the worker that classifies tabs.
struct TabProbe {
    uid: u64,
    label: String,
    stored: PathBuf,
    session: Option<PathBuf>,
    mux: Option<(Mux, String)>,
}

/// Each tab's live folder: the one omp wrote to its terminal breadcrumb, when the tab runs in a
/// backend and the breadcrumb belongs to its current process (not a previous owner of the pty).
/// Blocking.
fn read_live(probes: &[TabProbe], agent_dir: &Path) -> Vec<worktree_ui::TabCwd> {
    probes
        .iter()
        .map(|p| {
            let live = p.mux.as_ref().and_then(|(mux, name)| {
                let pane = mux.pane(name)?;
                let crumb = status::read_breadcrumb(agent_dir, &pane.breadcrumb_key)?;
                let written =
                    std::fs::metadata(status::breadcrumb_path(agent_dir, &pane.breadcrumb_key))
                        .and_then(|m| m.modified())
                        .ok()?;
                let fresh = written + Duration::from_secs(2) >= pane.started;
                (fresh || p.session.as_deref() == Some(crumb.session.as_path()))
                    .then_some(crumb.cwd)
            });
            worktree_ui::TabCwd {
                uid: p.uid,
                label: p.label.clone(),
                stored: p.stored.clone(),
                live,
            }
        })
        .collect()
}

struct PendingMove {
    uid: u64,
    base: Option<String>,
    since: Instant,
}

pub(super) struct WtUi {
    pub(super) dialog: Option<WorktreeDialog>,
    /// Worktrees (by folder) being finished.
    pub(super) finishing: HashMap<PathBuf, Finishing>,
    /// omp's `worktree.base` setting, once the settings catalog was read.
    pub(super) base_setting: Option<String>,
    moves: Vec<PendingMove>,
    /// Worktrees whose removal is running.
    removing: HashSet<PathBuf>,
    /// Each tab's session file as of the last relocation check.
    seen: HashMap<u64, Option<PathBuf>>,
    /// The last unresolved lookup per tab (file, scan generation): not repeated until the scan changes.
    tried: HashMap<u64, (PathBuf, u64)>,
    /// The removed-worktree map was checked against the disk once.
    pruned: bool,
}

impl WtUi {
    pub(super) fn new() -> WtUi {
        WtUi {
            dialog: None,
            finishing: HashMap::new(),
            base_setting: None,
            moves: Vec::new(),
            removing: HashSet::new(),
            seen: HashMap::new(),
            tried: HashMap::new(),
            pruned: false,
        }
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl App {
    // ------------------------------------------------------------ dialogs

    /// "New session in worktree…" for the project in `dir`.
    pub(super) fn open_new_worktree(&mut self, dir: PathBuf) {
        if self.omp.is_none() || self.wt.dialog.is_some() {
            return;
        }
        self.start_dialog(Kind::New { dir });
    }

    /// "Move to new worktree…" for the session open in a tab.
    pub(super) fn open_move_to_worktree(&mut self, entry: &SessionEntry) {
        if self.omp.is_none() || self.wt.dialog.is_some() {
            return;
        }
        let tab = self
            .tabs
            .iter()
            .filter_map(Tab::term)
            .find(|t| t.session_path.as_deref() == Some(entry.path.as_path()));
        let Some(tab) = tab.filter(|t| !t.term.is_exited() && t.role == TabRole::Session) else {
            self.toasts
                .info(&self.ctx, "Open the session in a tab first");
            return;
        };
        self.start_dialog(Kind::Move {
            uid: tab.uid,
            dir: tab.cwd.clone(),
            title: entry.label().to_owned(),
        });
    }

    fn start_dialog(&mut self, kind: Kind) {
        let dialog = WorktreeDialog::new(kind, unix_secs());
        let dir = dialog.dir().to_path_buf();
        self.wt.dialog = Some(dialog);
        let tx = self.events_tx.clone();
        crate::omp_worker::spawn(&self.ctx, &tx, move || {
            AppEvent::Worktree(WtEvent::Repo(worktree::repo_info(&dir)))
        });
    }

    pub(super) fn show_worktree_dialog(&mut self, ctx: &egui::Context) {
        let Some(dlg) = self.wt.dialog.as_mut() else {
            return;
        };
        if let Some(req) = dlg.take_check(self.wt.base_setting.as_deref(), self.home.as_deref()) {
            crate::omp_worker::spawn(ctx, &self.events_tx, move || {
                AppEvent::Worktree(WtEvent::Checked(req.run()))
            });
        }
        if let Some(due) = dlg.check_due_in() {
            ctx.request_repaint_after(due);
        }
        match dlg.show(ctx, self.picker_available) {
            Outcome::Open => {}
            Outcome::Cancel => {
                self.wt.dialog = None;
                self.focus_pending = true;
            }
            Outcome::Browse => {
                dlg.browsing = true;
                let start = dlg.start_dir(self.home.as_deref());
                crate::omp_worker::spawn(ctx, &self.events_tx, move || {
                    AppEvent::Worktree(WtEvent::Folder(transfer::pick_folder(&start)))
                });
            }
            Outcome::Confirm(plan) => {
                self.wt.dialog = None;
                self.focus_pending = true;
                self.run_plan(plan);
            }
        }
    }

    fn run_plan(&mut self, plan: Plan) {
        match plan {
            Plan::New {
                repo,
                branch,
                base_ref,
                record_base,
                dest,
            } => {
                let Some(omp) = self.omp.clone() else {
                    return;
                };
                self.toasts
                    .info(&self.ctx, format!("Creating worktree “{branch}”…"));
                crate::omp_worker::spawn(&self.ctx, &self.events_tx, move || {
                    let result = worktree::create(&omp, &repo, &branch, &base_ref, &dest)
                        .map_err(|e| brief(&e));
                    AppEvent::Worktree(WtEvent::Created {
                        dest,
                        record_base,
                        result,
                    })
                });
            }
            Plan::Move { uid, branch, base } => {
                let Some(tab) = self
                    .tabs
                    .iter_mut()
                    .filter_map(Tab::term_mut)
                    .find(|t| t.uid == uid && !t.term.is_exited())
                else {
                    self.toasts
                        .error(&self.ctx, "The session's tab was closed — nothing moved");
                    return;
                };
                tab.injector.push(Job::slash(
                    MOVE_LABEL,
                    &format!("/wt {branch}"),
                    Requires::Idle,
                ));
                self.wt.moves.push(PendingMove {
                    uid,
                    base,
                    since: Instant::now(),
                });
                self.ctx.request_repaint();
            }
        }
    }

    // ------------------------------------------------------------ worker results

    pub(super) fn on_wt_event(&mut self, event: WtEvent) {
        match event {
            WtEvent::Repo(result) => {
                if let Some(d) = &mut self.wt.dialog {
                    d.on_repo(result);
                }
            }
            WtEvent::Checked(res) => {
                if let Some(d) = &mut self.wt.dialog {
                    d.apply(res);
                }
            }
            WtEvent::Folder(path) => {
                if let Some(d) = &mut self.wt.dialog {
                    d.browsing = false;
                    if let Some(p) = path {
                        d.picked(&p);
                    }
                }
            }
            WtEvent::Created {
                dest,
                record_base,
                result,
            } => match result {
                Ok(()) => {
                    if let Some(base) = record_base {
                        self.cfg.worktree_bases.insert(dest.clone(), base);
                        self.dirty = true;
                    }
                    // A worktree made at the path of a removed one is a live worktree again.
                    self.cfg.removed_worktrees.remove(&dest);
                    self.dirty = true;
                    self.refresh_sessions();
                    self.new_session(dest);
                }
                Err(e) => self
                    .toasts
                    .error(&self.ctx, format!("Could not create the worktree: {e}")),
            },
            WtEvent::Inspected {
                purpose,
                wt,
                result,
            } => self.on_inspected(purpose, wt, result),
            WtEvent::Removed { wt, result } => self.on_removed(wt, result),
            WtEvent::Target { path, result } => self.on_merge_target(&path, result),
            WtEvent::Classified { wt, base, result } => self.on_classified(wt, base, result),
            WtEvent::Orphaned(paths) => {
                for p in paths {
                    self.cfg.removed_worktrees.remove(&p);
                }
                self.dirty = true;
            }
        }
    }

    /// Reads a worktree's state on a worker: its merge target (`base`, else the recorded or
    /// inferred one) and whether it is clean and merged.
    fn inspect(&self, purpose: Purpose, wt: Worktree, base: Option<String>) {
        let recorded = base.or_else(|| self.cfg.worktree_bases.get(&wt.path).cloned());
        crate::omp_worker::spawn(&self.ctx, &self.events_tx, move || {
            let result = (|| {
                let base = match recorded {
                    Some(b) => b,
                    None => worktree::infer_base(&wt).ok_or_else(|| {
                        "OmpDeck does not know which branch it is meant to merge into".to_owned()
                    })?,
                };
                let st = worktree::status(&wt, &base).map_err(|e| brief(&e))?;
                Ok((base, st))
            })();
            AppEvent::Worktree(WtEvent::Inspected {
                purpose,
                wt,
                result,
            })
        });
    }

    fn on_inspected(
        &mut self,
        purpose: Purpose,
        wt: Worktree,
        result: Result<(String, WorktreeStatus), String>,
    ) {
        let label = worktree_ui::worktree_label(&wt);
        match (purpose, result) {
            (Purpose::Remove, Ok((base, st))) => {
                let blockers = worktree_ui::blockers(&st, &base);
                if !blockers.is_empty() {
                    self.toasts.error(
                        &self.ctx,
                        format!("Cannot remove “{label}”: {}", blockers.join("; ")),
                    );
                    return;
                }
                self.confirm = Some((
                    Confirm {
                        title: "Remove worktree?".to_owned(),
                        body: format!(
                            "{} is clean and merged into {base}.\n\nThis stops the sessions running in {} and deletes {}.",
                            match &wt.branch {
                                Some(_) => format!("Branch “{label}”"),
                                None => format!("The detached worktree “{label}”"),
                            },
                            wt.path.display(),
                            if wt.branch.is_some() {
                                "the folder and the branch"
                            } else {
                                "the folder"
                            }
                        ),
                        confirm_label: "Remove".to_owned(),
                        danger: true,
                    },
                    PendingConfirm::RemoveWorktree { wt, base },
                ));
            }
            (Purpose::Start, Ok((base, _))) => self.begin_finishing(wt, base),
            (Purpose::Check, Ok((base, st))) => {
                let Some(f) = self.wt.finishing.get_mut(&wt.path) else {
                    return;
                };
                match f.on_checked(&worktree_ui::blockers(&st, &base)) {
                    Verdict::Remove => self.execute_remove(wt, base),
                    Verdict::Wait { announce } => {
                        if let Some(text) = announce {
                            self.toasts.info(&self.ctx, text);
                        }
                    }
                }
            }
            (Purpose::Check, Err(e)) => {
                if let Some(text) = self
                    .wt
                    .finishing
                    .get_mut(&wt.path)
                    .and_then(|f| f.on_check_failed(&e))
                {
                    self.toasts.error(&self.ctx, text);
                }
            }
            (_, Err(e)) => self
                .toasts
                .error(&self.ctx, format!("Worktree “{label}”: {e}")),
        }
    }

    // ------------------------------------------------------------ finishing

    pub(super) fn request_finish(&mut self, wt: Worktree) {
        if self.wt.finishing.contains_key(&wt.path) || self.wt.removing.contains(&wt.path) {
            return;
        }
        self.inspect(Purpose::Start, wt, None);
    }

    pub(super) fn cancel_finish(&mut self, path: &Path) {
        if self.wt.finishing.remove(path).is_some() {
            self.toasts
                .info(&self.ctx, "Stopped waiting — the worktree stays");
        }
    }

    pub(super) fn request_remove(&mut self, wt: Worktree) {
        if self.wt.removing.contains(&wt.path) {
            return;
        }
        self.inspect(Purpose::Remove, wt, None);
    }

    fn begin_finishing(&mut self, wt: Worktree, base: String) {
        let uid = self.tab_in(&wt).map(|t| t.uid);
        let session = self.newest_session_in(&wt).map(|e| e.path);
        if uid.is_none() && session.is_none() {
            self.toasts.error(
                &self.ctx,
                "This worktree has no session to ask — open a session in it first",
            );
            return;
        }
        let label = worktree_ui::worktree_label(&wt);
        self.toasts.info(
            &self.ctx,
            format!(
                "Finishing “{label}”: the session is asked to commit and merge into {base}; the worktree is removed once that is done"
            ),
        );
        let (path, primary, target_base) = (wt.path.clone(), wt.primary.clone(), base.clone());
        self.wt.finishing.insert(
            wt.path.clone(),
            Finishing::new(wt, base, uid, session, Instant::now()),
        );
        crate::omp_worker::spawn(&self.ctx, &self.events_tx, move || {
            let result = worktree_ui::find_merge_target(&primary, &target_base);
            AppEvent::Worktree(WtEvent::Target { path, result })
        });
        self.ctx.request_repaint();
    }

    fn on_merge_target(&mut self, path: &Path, result: Result<MergeTarget, String>) {
        let Some(f) = self.wt.finishing.get_mut(path) else {
            return;
        };
        match result {
            Ok(t) => f.target = Some(t),
            Err(e) => {
                let label = worktree_ui::worktree_label(&f.worktree);
                self.wt.finishing.remove(path);
                self.toasts
                    .error(&self.ctx, format!("Cannot finish “{label}”: {e}"));
            }
        }
    }

    /// The running session tab whose folder is inside the worktree.
    fn tab_in(&self, wt: &Worktree) -> Option<&crate::tabs::TermTab> {
        self.tabs.iter().filter_map(Tab::term).find(|t| {
            t.role == TabRole::Session && !t.term.is_exited() && worktree_ui::inside(wt, &t.cwd)
        })
    }

    fn newest_session_in(&self, wt: &Worktree) -> Option<SessionEntry> {
        let groups = self.watcher.latest()?;
        groups
            .iter()
            .filter(|g| g.worktree.as_ref().is_some_and(|w| w.path == wt.path))
            .flat_map(|g| &g.sessions)
            .max_by_key(|e| e.modified)
            .cloned()
    }

    /// Advances every finishing worktree: opens its session when no tab runs it, types the finish
    /// prompt once the composer is idle, checks the worktree after each agent turn.
    pub(super) fn drive_worktrees(&mut self, ctx: &egui::Context) {
        self.prune_removed_once();
        self.expire_moves();
        self.track_relocations();
        if self.wt.finishing.is_empty() {
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(400));
        let paths: Vec<PathBuf> = self.wt.finishing.keys().cloned().collect();
        for path in paths {
            self.drive_finish(&path);
        }
    }

    fn drive_finish(&mut self, path: &Path) {
        let Some(f) = self.wt.finishing.get(path) else {
            return;
        };
        let wt = f.worktree.clone();
        let (known_uid, started, injected) = (f.uid, f.started, f.injected());
        // The prompt has to go in within the timeout, whatever state the tab is stuck in.
        if !injected && started.elapsed() > FINISH_TAB_TIMEOUT {
            self.wt.finishing.remove(path);
            self.toasts.error(
                &self.ctx,
                format!(
                    "Stopped finishing “{}”: its session was not ready for the prompt in time",
                    worktree_ui::worktree_label(&wt)
                ),
            );
            return;
        }
        // The tab: the remembered one, else any session tab inside the worktree.
        let uid = known_uid
            .filter(|u| {
                self.tabs
                    .iter()
                    .filter_map(Tab::term)
                    .any(|t| t.uid == *u && !t.term.is_exited())
            })
            .or_else(|| self.tab_in(&wt).map(|t| t.uid));
        let Some(uid) = uid else {
            if known_uid.is_some() || injected {
                self.wt.finishing.remove(path);
                self.toasts.info(
                    &self.ctx,
                    format!(
                        "“{}” has no running session anymore — stopped waiting",
                        worktree_ui::worktree_label(&wt)
                    ),
                );
            } else {
                self.resume_for_finish(path, &wt, started);
            }
            return;
        };
        let Some(f) = self.wt.finishing.get_mut(path) else {
            return;
        };
        f.uid = Some(uid);
        let Some(tab) = self
            .tabs
            .iter_mut()
            .filter_map(Tab::term_mut)
            .find(|t| t.uid == uid)
        else {
            return;
        };
        let activity = Activity::from_title(tab.term.title());
        match f.observe(activity) {
            Step::Nothing => {}
            Step::Inject => {
                if !tab.injector.is_busy()
                    && f.may_inject(Instant::now())
                    && let Some(target) = f.target.clone()
                {
                    let prompt = worktree_ui::finish_prompt(
                        wt.branch.as_deref(),
                        &f.base,
                        &wt.path,
                        &target,
                    );
                    tab.injector
                        .push(Job::slash(FINISH_LABEL, &prompt, Requires::Idle));
                    f.mark_injected();
                }
            }
            Step::Recheck => {
                let base = f.base.clone();
                self.inspect(Purpose::Check, wt, Some(base));
            }
        }
    }

    /// No tab runs the worktree: resume its newest session (once), then wait for the tab.
    fn resume_for_finish(&mut self, path: &Path, wt: &Worktree, started: Instant) {
        if started.elapsed() > FINISH_TAB_TIMEOUT {
            self.wt.finishing.remove(path);
            self.toasts.error(
                &self.ctx,
                format!(
                    "Could not open a session in “{}” to finish it",
                    worktree_ui::worktree_label(wt)
                ),
            );
            return;
        }
        let session = self
            .wt
            .finishing
            .get_mut(path)
            .and_then(|f| f.session.take());
        if let Some(session) = session
            && let Some(entry) = self.newest_session_in(wt).filter(|e| e.path == session)
        {
            // An exited tab still shows this session: opening would only focus it.
            let exited = self.tabs.iter().position(|t| {
                t.term().is_some_and(|t| {
                    t.term.is_exited() && t.session_path.as_deref() == Some(entry.path.as_path())
                })
            });
            if let Some(i) = exited {
                self.close_tab(i, false);
            }
            self.open_session(&entry);
        }
    }

    /// The injector dropped (or never ran) a job of ours: whatever waited for it gives up.
    pub(super) fn injection_lost(&mut self, uid: u64, label: &str) {
        if label == FINISH_LABEL {
            // omp was not ready (still starting, a dialog open, working): the prompt is offered
            // again until the finish times out.
            let now = Instant::now();
            for f in self
                .wt
                .finishing
                .values_mut()
                .filter(|f| f.uid == Some(uid))
            {
                f.injection_failed(now);
            }
        } else if label == MOVE_LABEL {
            self.wt.moves.retain(|m| m.uid != uid);
        }
    }

    // ------------------------------------------------------------ removal

    /// Stops only the tabs inside the worktree, then removes it. Which tabs those are is decided
    /// on a worker from each tab's live folder (see [`worktree_ui::classify`]); after they are
    /// stopped the worker classifies the remaining tabs again and refuses to remove the folder
    /// if one still runs in it. Git itself re-checks that the worktree is clean and merged.
    pub(super) fn execute_remove(&mut self, wt: Worktree, base: String) {
        self.wt.finishing.remove(&wt.path);
        if !self.wt.removing.insert(wt.path.clone()) {
            return;
        }
        let probes = self.tab_probes(&[]);
        let agent_dir = self.root.agent_dir();
        crate::omp_worker::spawn(&self.ctx, &self.events_tx, move || {
            let result = worktree_ui::classify(&wt, &read_live(&probes, &agent_dir));
            AppEvent::Worktree(WtEvent::Classified { wt, base, result })
        });
    }

    /// Snapshot of the running session tabs except `skip` (by uid).
    fn tab_probes(&self, skip: &[u64]) -> Vec<TabProbe> {
        self.tabs
            .iter()
            .filter_map(Tab::term)
            .filter(|t| t.role == TabRole::Session && !t.term.is_exited() && !skip.contains(&t.uid))
            .map(|t| TabProbe {
                uid: t.uid,
                label: t.fallback_title.clone(),
                stored: t.cwd.clone(),
                session: t.session_path.clone(),
                mux: t.mux.clone().zip(t.mux_name.clone()),
            })
            .collect()
    }

    fn on_classified(&mut self, wt: Worktree, base: String, result: Result<Vec<u64>, String>) {
        let stop = match result {
            Ok(stop) => stop,
            Err(e) => {
                self.wt.removing.remove(&wt.path);
                let label = worktree_ui::worktree_label(&wt);
                self.toasts
                    .error(&self.ctx, format!("Cannot remove “{label}”: {e}"));
                return;
            }
        };
        let mut names = Vec::new();
        for i in (0..self.tabs.len()).rev() {
            let hit = match self.tabs.get(i) {
                Some(Tab::Terminal(t)) if stop.contains(&t.uid) => {
                    Some(t.mux_name.clone().filter(|_| t.mux.is_some()))
                }
                _ => None,
            };
            let Some(name) = hit else {
                continue;
            };
            match name {
                Some(n) => names.push(n),
                None => {
                    if let Some(t) = self.tabs.get_mut(i).and_then(Tab::term_mut) {
                        t.term.kill();
                    }
                }
            }
            self.close_tab(i, false);
        }
        let remaining = self.tab_probes(&[]);
        let backends = self.backends.clone();
        let agent_dir = self.root.agent_dir();
        crate::omp_worker::spawn(&self.ctx, &self.events_tx, move || {
            let result = (|| {
                names
                    .iter()
                    .try_for_each(|n| backends.stop(n))
                    .map_err(|e| format!("could not stop a session running in it: {e}"))?;
                if !worktree_ui::classify(&wt, &read_live(&remaining, &agent_dir))?.is_empty() {
                    return Err("a session is still running in it".to_owned());
                }
                worktree::remove(&wt, &base, true).map_err(|e| e.to_string())
            })();
            // git can fail after it already deleted the folder: nothing is left to keep, so the
            // removal counts, with the error as the note.
            let result = match result {
                Err(e) if !wt.path.exists() => Ok(Removed {
                    branch_deleted: false,
                    branch_note: Some(e),
                }),
                other => other,
            };
            AppEvent::Worktree(WtEvent::Removed { wt, result })
        });
        self.running.poke();
    }

    fn on_removed(&mut self, wt: Worktree, result: Result<Removed, String>) {
        self.wt.removing.remove(&wt.path);
        let label = worktree_ui::worktree_label(&wt);
        let removed = match result {
            Ok(r) => r,
            Err(e) => {
                self.toasts
                    .error(&self.ctx, format!("Could not remove “{label}”: {e}"));
                self.refresh_sessions();
                return;
            }
        };
        let text = worktree_ui::removal_text(&label, wt.branch.as_deref(), &removed);
        if removed.branch_note.is_some() && wt.branch.is_some() && !removed.branch_deleted {
            self.toasts.info(&self.ctx, text);
        } else {
            self.toasts.success(&self.ctx, text);
        }
        self.cfg.removed_worktrees.insert(
            wt.path.clone(),
            RemovedWorktree {
                primary: wt.primary.clone(),
                branch: wt.branch.clone(),
            },
        );
        self.cfg.worktree_bases.remove(&wt.path);
        self.cfg.folder_tabs.remove(&wt.path);
        self.cfg.recent_folders.retain(|p| *p != wt.path);
        if self
            .scope
            .as_deref()
            .is_some_and(|s| s.starts_with(&wt.path))
        {
            self.set_scope(None);
        }
        self.dirty = true;
        self.refresh_sessions();
        if self.cfg.project_sync.iter().any(|r| r.path == wt.path) && self.confirm.is_none() {
            self.confirm = Some((
                Confirm {
                    title: "Stop backing up the removed worktree?".to_owned(),
                    body: format!(
                        "{} is in the project backups list but no longer exists. Backups already in Google Drive stay.",
                        wt.path.display()
                    ),
                    confirm_label: "Remove from list".to_owned(),
                    danger: false,
                },
                PendingConfirm::DropSyncRow { path: wt.path },
            ));
        }
    }

    // ------------------------------------------------------------ moves into a worktree

    /// Once per run: forget removed worktrees whose primary repository is gone too.
    fn prune_removed_once(&mut self) {
        if self.wt.pruned {
            return;
        }
        self.wt.pruned = true;
        if self.cfg.removed_worktrees.is_empty() {
            return;
        }
        let removed = self.cfg.removed_worktrees.clone();
        crate::omp_worker::spawn(&self.ctx, &self.events_tx, move || {
            AppEvent::Worktree(WtEvent::Orphaned(worktree_ui::orphaned_removed(&removed)))
        });
    }

    fn expire_moves(&mut self) {
        self.wt.moves.retain(|m| m.since.elapsed() < MOVE_EXPIRES);
    }

    /// A tab whose session file changed (omp relocated it, `/new`, `/resume`, …) takes the folder
    /// of the new file; a pending move that landed in a worktree records its merge target.
    fn track_relocations(&mut self) {
        let generation = self.watcher.generation();
        let live: HashSet<u64> = self
            .tabs
            .iter()
            .filter_map(Tab::term)
            .map(|t| t.uid)
            .collect();
        self.wt.seen.retain(|u, _| live.contains(u));
        self.wt.tried.retain(|u, _| live.contains(u));
        let mut changed = Vec::new();
        for tab in self.tabs.iter().filter_map(Tab::term) {
            match self.wt.seen.get(&tab.uid) {
                None => {
                    self.wt.seen.insert(tab.uid, tab.session_path.clone());
                }
                Some(prev) if *prev != tab.session_path => {
                    if let Some(p) = &tab.session_path
                        && self.wt.tried.get(&tab.uid) != Some(&(p.clone(), generation))
                    {
                        changed.push((tab.uid, p.clone()));
                    }
                }
                Some(_) => {}
            }
        }
        if changed.is_empty() {
            return;
        }
        let groups = self.watcher.latest();
        for (uid, path) in changed {
            let found = groups.as_deref().and_then(|gs| {
                gs.iter()
                    .find_map(|g| g.sessions.iter().find(|e| e.path == path).map(|e| (g, e)))
            });
            let Some((group, entry)) = found else {
                self.wt.tried.insert(uid, (path, generation));
                continue;
            };
            self.wt.tried.remove(&uid);
            self.wt.seen.insert(uid, Some(path));
            if let Some(t) = self
                .tabs
                .iter_mut()
                .filter_map(Tab::term_mut)
                .find(|t| t.uid == uid)
                && t.cwd != entry.cwd
            {
                t.cwd = entry.cwd.clone();
                self.dirty = true;
            }
            if let Some(i) = self.wt.moves.iter().position(|m| m.uid == uid)
                && let Some(w) = &group.worktree
            {
                let m = self.wt.moves.remove(i);
                if let Some(base) = m.base {
                    self.cfg.worktree_bases.insert(w.path.clone(), base);
                    self.dirty = true;
                }
                self.toasts.success(
                    &self.ctx,
                    format!("Moved to worktree “{}”", worktree_ui::worktree_label(w)),
                );
            }
        }
    }
}
