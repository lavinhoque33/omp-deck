//! App-side glue for git worktree sessions: the new-worktree and move dialogs, creating worktrees
//! through the gateway (which runs omp and git), finishing (ask the agent, wait, remove once
//! merged) and removing them, and the tab bookkeeping that follows a session moving into a
//! worktree. Git, omp and the removal itself run in the gateway; this module decides which tabs
//! are affected and shows the dialogs.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use deck_core::config::RemovedWorktree;
use deck_core::sessions::SessionEntry;
use deck_core::worktree::{
    self, MergeTarget, Removed, TabCwd, Worktree, WorktreeStatus, blockers, finish_prompt,
};
use deck_service::controller::{Command, CommandKind};
use deck_service::gateway::protocol::{Answer, Done, Mutation, Query};

use super::{App, PendingConfirm};
use crate::confirm::Confirm;
use crate::tabs::{Tab, TabRole};
use crate::transfer;
use crate::worktree_dialog::{Kind, Outcome, Plan, WorktreeDialog, brief};
use crate::worktree_ui::{self, FINISH_TAB_TIMEOUT, Finishing, Step, Verdict};

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

/// The reply a query is expected to carry, or why not.
fn unexpected<T>(
    r: Result<Answer, String>,
    pick: impl FnOnce(Answer) -> Option<T>,
) -> Result<T, String> {
    match r {
        Ok(a) => pick(a).ok_or_else(|| "the gateway answered with something else".to_owned()),
        Err(e) => Err(e),
    }
}

impl App {
    // ------------------------------------------------------------ dialogs

    /// "New session in worktree…" for the project in `dir`.
    pub(super) fn open_new_worktree(&mut self, dir: PathBuf) {
        if !self.omp_found() || self.wt.dialog.is_some() {
            return;
        }
        self.start_dialog(Kind::New { dir });
    }

    /// "Move to new worktree…" for the session open in a tab.
    pub(super) fn open_move_to_worktree(&mut self, entry: &SessionEntry) {
        if !self.omp_found() || self.wt.dialog.is_some() {
            return;
        }
        let tab = self
            .tabs
            .iter()
            .filter_map(Tab::term)
            .find(|t| t.session_path() == Some(entry.path.as_path()));
        let Some(tab) = tab.filter(|t| !t.terminal().is_exited() && t.role == TabRole::Session)
        else {
            self.toasts
                .info(&self.ctx, "Open the session in a tab first");
            return;
        };
        self.start_dialog(Kind::Move {
            uid: tab.uid,
            target: tab.target().clone(),
            dir: tab.cwd.clone(),
            title: entry.label().to_owned(),
        });
    }

    fn start_dialog(&mut self, kind: Kind) {
        let dialog = WorktreeDialog::new(kind, unix_secs());
        let dir = dialog.dir().to_path_buf();
        self.wt.dialog = Some(dialog);
        self.gw.query(Query::RepoInfo(dir), |app, r| {
            let info = unexpected(r, |a| match a {
                Answer::RepoInfo(i) => Some(i),
                _ => None,
            });
            if let Some(d) = &mut app.wt.dialog {
                d.on_repo(info);
            }
        });
    }

    /// Starts one dialog check: the gateway judges the branch name, a native worker the folder.
    fn run_check(&mut self, req: crate::worktree_dialog::CheckReq) {
        let generation = req.generation;
        let branch = req.branch().to_owned();
        if branch.is_empty() {
            if let Some(d) = &mut self.wt.dialog {
                d.apply_branch(generation, Err("Enter a branch name".to_owned()));
            }
        } else {
            let repo = req.dir().to_path_buf();
            self.gw
                .query(Query::CheckBranch { repo, name: branch }, move |app, r| {
                    let verdict = unexpected(r, |a| match a {
                        Answer::CheckBranch => Some(()),
                        _ => None,
                    })
                    .map_err(|e| brief(&e));
                    if let Some(d) = &mut app.wt.dialog {
                        d.apply_branch(generation, verdict);
                    }
                });
        }
        self.gw.native(
            move || req.locate(),
            move |app, res| {
                if let Some(d) = &mut app.wt.dialog {
                    d.apply_location(generation, res);
                }
            },
        );
    }

    pub(super) fn show_worktree_dialog(&mut self, ctx: &egui::Context) {
        let Some(dlg) = self.wt.dialog.as_mut() else {
            return;
        };
        let req = dlg.take_check(self.wt.base_setting.as_deref(), self.home.as_deref());
        if let Some(due) = dlg.check_due_in() {
            ctx.request_repaint_after(due);
        }
        if let Some(req) = req {
            self.run_check(req);
        }
        let Some(dlg) = self.wt.dialog.as_mut() else {
            return;
        };
        match dlg.show(ctx, self.picker_available) {
            Outcome::Open => {}
            Outcome::Cancel => {
                self.wt.dialog = None;
                self.focus_pending = true;
            }
            Outcome::Browse => {
                dlg.browsing = true;
                let start = dlg.start_dir(self.home.as_deref());
                self.gw.native(
                    move || transfer::pick_folder(&start),
                    |app, path| {
                        if let Some(d) = &mut app.wt.dialog {
                            d.browsing = false;
                            if let Some(p) = path {
                                d.picked(&p);
                            }
                        }
                    },
                );
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
                if !self.omp_found() {
                    return;
                }
                self.toasts
                    .info(&self.ctx, format!("Creating worktree “{branch}”…"));
                let created = dest.clone();
                self.gw.mutate(
                    Mutation::WorktreeCreate {
                        repo,
                        branch,
                        base_ref,
                        dest,
                    },
                    move |app, r| app.on_worktree_created(created, record_base, r.map(drop)),
                );
            }
            Plan::Move {
                uid,
                target,
                branch,
                base,
            } => {
                let open = self
                    .tabs
                    .iter()
                    .filter_map(Tab::term)
                    .any(|t| t.uid == uid && !t.terminal().is_exited());
                if !open {
                    self.toasts
                        .error(&self.ctx, "The session's tab was closed — nothing moved");
                    return;
                }
                // Bound to the session the dialog was opened on: if it moved on to another
                // conversation meanwhile, the controller refuses and nothing is typed. The move
                // is only tracked once the gateway accepted the command.
                self.submit_command(&target, Command::WorktreeMove { branch }, move |app, _| {
                    app.wt.moves.push(PendingMove {
                        uid,
                        base,
                        since: Instant::now(),
                    });
                });
            }
        }
    }

    fn on_worktree_created(
        &mut self,
        dest: PathBuf,
        record_base: Option<String>,
        result: Result<(), String>,
    ) {
        match result {
            Ok(()) => {
                if let Some(base) = record_base {
                    self.cfg.worktree_bases.insert(dest.clone(), base);
                }
                // A worktree made at the path of a removed one is a live worktree again.
                self.cfg.removed_worktrees.remove(&dest);
                self.dirty = true;
                self.refresh_sessions();
                self.new_session(dest);
            }
            Err(e) => self.toasts.error(
                &self.ctx,
                format!("Could not create the worktree: {}", brief(&e)),
            ),
        }
    }

    // ------------------------------------------------------------ inspection

    /// Reads a worktree's state through the gateway: its merge target (`base`, else the recorded
    /// or inferred one) and whether it is clean and merged.
    fn inspect(&mut self, purpose: Purpose, wt: Worktree, base: Option<String>) {
        let recorded = base.or_else(|| self.cfg.worktree_bases.get(&wt.path).cloned());
        match recorded {
            Some(base) => self.inspect_status(purpose, wt, base),
            None => {
                let asked = wt.clone();
                self.gw.query(Query::InferBase(asked), move |app, r| {
                    let inferred = unexpected(r, |a| match a {
                        Answer::InferBase(b) => Some(b),
                        _ => None,
                    });
                    match inferred {
                        Ok(Some(base)) => app.inspect_status(purpose, wt, base),
                        Ok(None) => app.on_inspected(
                            purpose,
                            wt,
                            Err(
                                "OmpDeck does not know which branch it is meant to merge into"
                                    .to_owned(),
                            ),
                        ),
                        Err(e) => app.on_inspected(purpose, wt, Err(brief(&e))),
                    }
                });
            }
        }
    }

    fn inspect_status(&mut self, purpose: Purpose, wt: Worktree, base: String) {
        let q = Query::WorktreeStatus {
            worktree: wt.clone(),
            base: base.clone(),
        };
        self.gw.query(q, move |app, r| {
            let status = unexpected(r, |a| match a {
                Answer::WorktreeStatus(s) => Some(s),
                _ => None,
            })
            .map(|s| (base, s))
            .map_err(|e| brief(&e));
            app.on_inspected(purpose, wt, status);
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
                let blockers = blockers(&st, &base);
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
                match f.on_checked(&blockers(&st, &base)) {
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
        self.gw.query(
            Query::MergeTarget {
                primary,
                base: target_base,
            },
            move |app, r| {
                let found = unexpected(r, |a| match a {
                    Answer::MergeTarget(t) => Some(t),
                    _ => None,
                });
                app.on_merge_target(&path, found);
            },
        );
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
            t.role == TabRole::Session
                && !t.terminal().is_exited()
                && worktree_ui::inside(wt, &t.cwd)
        })
    }

    fn newest_session_in(&self, wt: &Worktree) -> Option<SessionEntry> {
        let groups = self.groups()?;
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
                    .any(|t| t.uid == *u && !t.terminal().is_exited())
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
        let Some((activity, busy, session)) = self
            .tabs
            .iter()
            .filter_map(Tab::term)
            .find(|t| t.uid == uid)
            .map(|t| {
                (
                    t.activity(),
                    t.is_busy(),
                    // The tab was just found through its session file: its current target is the
                    // conversation the worktree belongs to.
                    t.target().clone(),
                )
            })
        else {
            return;
        };
        match f.observe(activity) {
            Step::Nothing => {}
            Step::Inject => {
                let command = match f.target.clone() {
                    Some(target) if !busy && f.may_inject(Instant::now()) => {
                        Some(Command::WorktreeFinish {
                            prompt: finish_prompt(wt.branch.as_deref(), &f.base, &wt.path, &target),
                        })
                    }
                    _ => None,
                };
                if let Some(command) = command {
                    // Until the gateway accepts the command it is only locally pending: the retry
                    // pause keeps this frame loop from submitting it again meanwhile.
                    f.injection_failed(Instant::now());
                    let key = path.to_path_buf();
                    self.submit_command(&session, command, move |app, _| {
                        if let Some(f) = app.wt.finishing.get_mut(&key) {
                            f.mark_injected();
                        }
                    });
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
                    t.terminal().is_exited() && t.session_path() == Some(entry.path.as_path())
                })
            });
            if let Some(i) = exited {
                self.close_tab(i, false);
            }
            self.open_session(&entry);
        }
    }

    /// The finish prompt of tab `uid` was definitively cancelled or declined: it was never (or no
    /// longer will be) typed, so the wait for omp's turn would never end. The finish stops; the
    /// prompt is not sent again on its own.
    pub(super) fn finish_cancelled(&mut self, uid: u64) {
        let paths: Vec<PathBuf> = self
            .wt
            .finishing
            .iter()
            .filter(|(_, f)| f.uid == Some(uid))
            .map(|(p, _)| p.clone())
            .collect();
        for path in paths {
            if let Some(f) = self.wt.finishing.remove(&path) {
                self.toasts.info(
                    &self.ctx,
                    format!(
                        "Stopped finishing “{}”: the finish prompt was not sent — the worktree stays",
                        worktree_ui::worktree_label(&f.worktree)
                    ),
                );
            }
        }
    }

    /// The injector dropped (or never ran) a job of ours: whatever waited for it gives up.
    pub(super) fn injection_lost(&mut self, uid: u64, kind: CommandKind) {
        if kind == CommandKind::WorktreeFinish {
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
        } else if kind == CommandKind::WorktreeMove {
            self.wt.moves.retain(|m| m.uid != uid);
        }
    }

    // ------------------------------------------------------------ removal

    /// The running session tabs as the removal classifier sees them: the folder each was launched
    /// in and the one its omp runs in now (the gateway's controller record).
    fn tab_cwds(&self) -> Vec<TabCwd> {
        self.tabs
            .iter()
            .filter_map(Tab::term)
            .filter(|t| t.role == TabRole::Session && !t.terminal().is_exited())
            .map(|t| TabCwd {
                uid: t.uid,
                label: t.fallback_title.clone(),
                stored: t.cwd.clone(),
                live: t.info().and_then(|i| i.live_cwd.clone()),
            })
            .collect()
    }

    /// Stops only the tabs inside the worktree, then removes it. Which tabs those are is decided
    /// off the UI thread from each tab's live folder (see [`worktree::classify`]); the gateway
    /// stops them, checks again that no other session still runs in the folder, and removes it.
    /// Git itself re-checks that the worktree is clean and merged.
    pub(super) fn execute_remove(&mut self, wt: Worktree, base: String) {
        self.wt.finishing.remove(&wt.path);
        if !self.wt.removing.insert(wt.path.clone()) {
            return;
        }
        let tabs = self.tab_cwds();
        let asked = wt.clone();
        self.gw.native(
            move || worktree::classify(&asked, &tabs),
            move |app, result| app.on_classified(wt, base, result),
        );
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
        let targets: Vec<_> = self
            .tabs
            .iter()
            .filter_map(Tab::term)
            .filter(|t| stop.contains(&t.uid))
            .map(|t| t.target().clone())
            .collect();
        for i in (0..self.tabs.len()).rev() {
            if matches!(self.tabs.get(i), Some(Tab::Terminal(t)) if stop.contains(&t.uid)) {
                self.close_tab(i, false);
            }
        }
        let asked = wt.clone();
        self.gw.mutate(
            Mutation::WorktreeRemove {
                worktree: asked,
                base,
                delete_branch: true,
                stop: targets,
            },
            move |app, r| {
                let result = r.and_then(|d| match d {
                    Done::Removed(r) => Ok(r),
                    _ => Err("the gateway answered with something else".to_owned()),
                });
                app.on_removed(wt, result);
            },
        );
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
        self.gw.native(
            move || worktree_ui::orphaned_removed(&removed),
            |app, paths| {
                for p in paths {
                    app.cfg.removed_worktrees.remove(&p);
                }
                app.dirty = true;
            },
        );
    }

    fn expire_moves(&mut self) {
        self.wt.moves.retain(|m| m.since.elapsed() < MOVE_EXPIRES);
    }

    /// A tab whose session file changed (omp relocated it, `/new`, `/resume`, …) takes the folder
    /// of the new file; a pending move that landed in a worktree records its merge target.
    fn track_relocations(&mut self) {
        let generation = self.gw.sessions_generation();
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
                    self.wt.seen.insert(tab.uid, tab.session_path_owned());
                }
                Some(prev) if prev.as_deref() != tab.session_path() => {
                    if let Some(p) = tab.session_path_owned()
                        && self.wt.tried.get(&tab.uid) != Some(&(p.clone(), generation))
                    {
                        changed.push((tab.uid, p));
                    }
                }
                Some(_) => {}
            }
        }
        if changed.is_empty() {
            return;
        }
        let groups = self.groups();
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
