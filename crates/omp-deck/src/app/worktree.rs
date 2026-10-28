//! App-side glue for git worktree sessions: the new-worktree and move dialogs, creating worktrees
//! through the gateway (which runs omp and git), finishing (ask the agent, wait, remove once
//! merged) and removing them, and the tab bookkeeping that follows a session moving into a
//! worktree. Git, omp and the removal itself run in the gateway; this module decides which tabs
//! are affected and shows the dialogs.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use deck_core::config::{ParkedBranch, RemovedWorktree};
use deck_core::sessions::{SessionEntry, abbreviate_home};
use deck_core::worktree::{
    self, BranchState, MergeTarget, Removal, Removed, TabCwd, Worktree, WorktreeStatus, blockers,
    finish_prompt, park_blockers,
};
use deck_service::controller::{Command, CommandKind};
use deck_service::gateway::protocol::{Answer, Done, Mutation, Query};
use deck_service::statusfmt::Activity;

use super::{App, PendingConfirm};
use crate::confirm::Confirm;
use crate::tabs::TabRole;
use crate::transfer;
use crate::worktree_dialog::{Kind, Outcome, Plan, WorktreeDialog, brief};
use crate::worktree_ui::{
    self, ContinuePlan, FINISH_TAB_TIMEOUT, Finishing, ParkState, Relocating, RemovePlan, Step,
    Then, Verdict,
};

/// How often the parked branches are looked at again (merged or deleted ones drop out).
const PARK_CHECK: Duration = Duration::from_secs(10 * 60);

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
    /// Removals waiting for their running sessions to move into the primary checkout.
    relocating: HashMap<PathBuf, Relocating>,
    /// omp's `worktree.base` setting, once the settings catalog was read.
    pub(super) base_setting: Option<String>,
    moves: Vec<PendingMove>,
    /// Worktrees whose removal is running.
    removing: HashSet<PathBuf>,
    /// Each tab's session file as of the last relocation check.
    seen: HashMap<u64, Option<PathBuf>>,
    /// Each tab's live folder as of the last relocation check.
    seen_cwd: HashMap<u64, Option<PathBuf>>,
    /// Sessions of removed worktrees being moved into their primary (by a leftover pass or an open).
    in_flight: HashSet<PathBuf>,
    /// The last unresolved lookup per tab (file, scan generation): not repeated until the scan changes.
    tried: HashMap<u64, (PathBuf, u64)>,
    /// The removed-worktree map was checked against the disk once.
    pruned: bool,
    /// Where each parked branch was last seen checked out.
    pub(super) park_states: Vec<ParkState>,
    /// Worktrees last seen clean and pushed but not merged: their remove action parks.
    pub(super) parkable: HashSet<PathBuf>,
    /// The next parked-branch check (`None`: at startup, once the first scan is in).
    park_next: Option<Instant>,
    /// Parked branches (primary, branch) with a continue in progress.
    continuing: HashSet<(PathBuf, String)>,
}

/// What a removed worktree's record leaves in the session list.
struct Leftover {
    worktree: PathBuf,
    primary: PathBuf,
    /// Sessions that are not running: moved into the primary.
    files: Vec<PathBuf>,
    /// Every session with the folder can be moved (none runs).
    all_movable: bool,
    /// No session has the folder anymore.
    none: bool,
}

/// The folder a tab's omp runs in now (the gateway's record), else the one it was launched in.
fn tab_dir(t: &crate::tabs::Tab) -> PathBuf {
    t.info()
        .and_then(|i| i.live_cwd.clone())
        .unwrap_or_else(|| t.cwd.clone())
}

impl WtUi {
    pub(super) fn new() -> WtUi {
        WtUi {
            dialog: None,
            finishing: HashMap::new(),
            relocating: HashMap::new(),
            base_setting: None,
            moves: Vec::new(),
            removing: HashSet::new(),
            in_flight: HashSet::new(),
            seen: HashMap::new(),
            seen_cwd: HashMap::new(),
            tried: HashMap::new(),
            pruned: false,
            park_states: Vec::new(),
            parkable: HashSet::new(),
            park_next: None,
            continuing: HashSet::new(),
        }
    }
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The refusal of a switch in a dirty checkout, worded for the toast; `None` for other errors.
fn dirty_reason(error: &str, dir: &Path) -> Option<String> {
    error
        .contains("uncommitted or untracked changes")
        .then(|| format!("{} has uncommitted or untracked changes", dir.display()))
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
        let tab = self.tabs.iter().find(|t| {
            t.session_path()
                .is_some_and(|p| worktree_ui::same_session(p, &entry.path))
        });
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
                let merged_blockers = blockers(&st, &base);
                if merged_blockers.is_empty() {
                    self.wt.parkable.remove(&wt.path);
                    self.confirm = Some((
                        Confirm {
                            title: "Remove worktree?".to_owned(),
                            body: format!(
                                "{} is clean and merged into {base}.\n\nIts sessions move into {}. Tabs running in {} move there too once omp is idle and keep running; a tab still busy after 30 s is stopped. This deletes {}.",
                                match &wt.branch {
                                    Some(_) => format!("Branch “{label}”"),
                                    None => format!("The detached worktree “{label}”"),
                                },
                                abbreviate_home(&wt.primary),
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
                        PendingConfirm::RemoveWorktree {
                            wt,
                            base,
                            how: Removal::Merged {
                                delete_branch: true,
                            },
                        },
                    ));
                    return;
                }
                if st.merged {
                    // Merged, but something else stops the removal (uncommitted work).
                    self.toasts.error(
                        &self.ctx,
                        format!("Cannot remove “{label}”: {}", merged_blockers.join("; ")),
                    );
                    return;
                }
                let park = park_blockers(&st);
                if !park.is_empty() {
                    self.wt.parkable.remove(&wt.path);
                    self.toasts.error(
                        &self.ctx,
                        format!(
                            "Cannot remove “{label}”: {}. Commit and push the work to park it, or merge it first.",
                            park.join("; ")
                        ),
                    );
                    return;
                }
                let branch = wt.branch.clone().unwrap_or_default();
                let upstream = st
                    .upstream
                    .clone()
                    .unwrap_or_else(|| "its upstream".to_owned());
                self.wt.parkable.insert(wt.path.clone());
                self.confirm = Some((
                    Confirm {
                        title: "Park worktree?".to_owned(),
                        body: format!(
                            "Branch “{branch}” is clean and pushed to {upstream}, but not merged into {base} yet (for example, its PR is still open).\n\nThe local branch is kept and never deleted by OmpDeck — delete it by hand when you are done. Sessions move into {}; tabs running in {} move there too once omp is idle and keep running (a tab still busy after 30 s is stopped). This removes the folder {}. Continue later from a session's menu.",
                            abbreviate_home(&wt.primary),
                            wt.path.display(),
                            wt.path.display()
                        ),
                        confirm_label: "Park".to_owned(),
                        danger: false,
                    },
                    PendingConfirm::RemoveWorktree {
                        wt,
                        base,
                        how: Removal::Park,
                    },
                ));
            }
            (Purpose::Start, Ok((base, _))) => self.begin_finishing(wt, base),
            (Purpose::Check, Ok((base, st))) => {
                let Some(f) = self.wt.finishing.get_mut(&wt.path) else {
                    return;
                };
                match f.on_checked(&blockers(&st, &base)) {
                    Verdict::Remove => self.execute_remove(
                        wt,
                        base,
                        Removal::Merged {
                            delete_branch: true,
                        },
                    ),
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
    fn tab_in(&self, wt: &Worktree) -> Option<&crate::tabs::Tab> {
        self.tabs.iter().find(|t| {
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
        self.check_parked(ctx);
        if !self.wt.relocating.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(300));
            self.drive_relocations();
        }
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
        let Some((activity, busy, session)) = self.tabs.iter().find(|t| t.uid == uid).map(|t| {
            (
                t.activity(),
                t.is_busy(),
                // The tab was just found through its session file: its current target is the
                // conversation the worktree belongs to.
                t.target().clone(),
            )
        }) else {
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
                t.terminal().is_exited()
                    && t.session_path()
                        .is_some_and(|p| worktree_ui::same_session(p, &entry.path))
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
        } else if kind == CommandKind::MoveTo {
            // The `/move` was not accepted by omp: offered again after a pause until the wait
            // runs out, then the tab is stopped like before.
            let now = Instant::now();
            for r in self.wt.relocating.values_mut() {
                r.paused(uid, now);
            }
        }
    }

    // ------------------------------------------------------------ removal

    /// The running session tabs as the removal classifier sees them: the folder each was launched
    /// in and the one its omp runs in now (the gateway's controller record).
    fn tab_cwds(&self) -> Vec<TabCwd> {
        self.tabs
            .iter()
            .filter(|t| t.role == TabRole::Session && !t.terminal().is_exited())
            .map(|t| TabCwd {
                uid: t.uid,
                label: t.fallback_title.clone(),
                stored: t.cwd.clone(),
                live: t.info().and_then(|i| i.live_cwd.clone()),
            })
            .collect()
    }

    /// Moves the running sessions inside the worktree into its primary checkout (omp's `/move`,
    /// the tabs keep running there), then stops the tabs that did not move and removes it. Git
    /// itself re-checks that the worktree is clean and merged (or, parking, clean and pushed).
    pub(super) fn execute_remove(&mut self, wt: Worktree, base: String, how: Removal) {
        self.wt.finishing.remove(&wt.path);
        if !self.wt.removing.insert(wt.path.clone()) {
            return;
        }
        let moving: Vec<(u64, Option<PathBuf>)> = self
            .tabs
            .iter()
            .filter(|t| {
                t.role == TabRole::Session
                    && !t.terminal().is_exited()
                    && worktree_ui::inside(&wt, &tab_dir(t))
            })
            .map(|t| (t.uid, t.session_path_owned()))
            .collect();
        // Tabs that move out are recorded by id (their file changes under them); the gateway
        // adds every session it relocated.
        let groups = self.groups();
        let sessions: Vec<String> = moving
            .iter()
            .filter_map(|(_, f)| f.as_deref())
            .filter_map(|f| {
                groups
                    .as_deref()?
                    .iter()
                    .flat_map(|g| &g.sessions)
                    .find(|e| worktree_ui::same_session(&e.path, f))
                    .map(|e| e.id.clone())
            })
            .collect();
        let plan = RemovePlan {
            worktree: wt,
            base,
            how,
            sessions,
        };
        if moving.is_empty() {
            self.classify_removal(plan, Vec::new());
            return;
        }
        self.toasts.info(
            &self.ctx,
            format!(
                "Moving the open sessions of “{}” into {} once omp is idle (a tab still busy after 30 s is stopped)…",
                worktree_ui::worktree_label(&plan.worktree),
                abbreviate_home(&plan.worktree.primary)
            ),
        );
        let r = Relocating::new(Then::Remove(plan), moving, Instant::now());
        self.wt.relocating.insert(r.key().to_path_buf(), r);
        self.ctx.request_repaint();
    }

    /// Advances every move that waits for running sessions: types `/move` into idle ones,
    /// notices tabs that arrived (or went away), and carries on once all did or the wait ran out.
    fn drive_relocations(&mut self) {
        let paths: Vec<PathBuf> = self.wt.relocating.keys().cloned().collect();
        for path in paths {
            let Some(r) = self.wt.relocating.get(&path) else {
                continue;
            };
            let (dest, pending) = (r.dest().to_path_buf(), r.pending());
            let groups = self.groups();
            let mut left: Vec<(u64, Option<PathBuf>)> = Vec::new();
            let mut gone = Vec::new();
            let mut send = Vec::new();
            for uid in pending {
                match self.tabs.iter().find(|t| t.uid == uid) {
                    None => gone.push(uid),
                    Some(t) if t.terminal().is_exited() => gone.push(uid),
                    Some(t) => {
                        let file_changed = matches!(
                            (r.file_of(uid), t.session_path()),
                            (Some(before), Some(now)) if before != now
                        );
                        let scanned = t.session_path().and_then(|p| {
                            groups.as_deref()?.iter().find_map(|g| {
                                g.sessions
                                    .iter()
                                    .find(|e| worktree_ui::same_session(&e.path, p))
                            })
                        });
                        if file_changed
                            || r.arrived(
                                t.info().and_then(|i| i.live_cwd.as_deref()),
                                &t.cwd,
                                scanned.map(|e| e.cwd.as_path()),
                            )
                        {
                            left.push((uid, scanned.map(|e| e.path.clone())));
                        } else if t.activity() == Activity::Idle
                            && !t.is_busy()
                            && r.may_send(uid, Instant::now())
                        {
                            send.push((uid, t.target().clone()));
                        }
                    }
                }
            }
            let Some(r) = self.wt.relocating.get_mut(&path) else {
                continue;
            };
            for uid in gone {
                r.gone(uid);
            }
            for (uid, _) in &left {
                r.left(*uid);
            }
            // The tab's recorded file may still be the one from before the move: point it (and
            // the gateway) at the file the scan lists now.
            for (uid, new) in &left {
                if let Some(new) = new {
                    self.fix_tab_path(*uid, new);
                }
            }
            // A tab that moved now runs in the destination: that is its folder from here on
            // (restored tabs relaunch there).
            for t in self
                .tabs
                .iter_mut()
                .filter(|t| left.iter().any(|(u, _)| *u == t.uid))
            {
                if t.cwd != dest {
                    t.cwd = dest.clone();
                    self.dirty = true;
                }
            }
            let Some(r) = self.wt.relocating.get_mut(&path) else {
                continue;
            };
            for (uid, _) in &send {
                r.paused(*uid, Instant::now());
            }
            let finished = r.finished(Instant::now());
            for (uid, target) in send {
                let key = path.clone();
                self.submit_command(
                    &target,
                    Command::MoveTo { dest: dest.clone() },
                    move |app, _| {
                        if let Some(r) = app.wt.relocating.get_mut(&key) {
                            r.accepted(uid);
                        }
                    },
                );
            }
            if finished && let Some(r) = self.wt.relocating.remove(&path) {
                let moved = r.moved_uids().to_vec();
                let stayed = r.pending().len();
                match r.then {
                    Then::Remove(plan) => self.classify_removal(plan, moved),
                    Then::Continue(plan) => self.finish_continue(plan, &moved, stayed),
                }
            }
        }
    }

    /// Records `new` as tab `uid`'s session file (here and in the gateway) when it differs.
    fn fix_tab_path(&mut self, uid: u64, new: &Path) {
        let Some(t) = self.tabs.iter_mut().find(|t| t.uid == uid) else {
            return;
        };
        if t.session_path() == Some(new) {
            return;
        }
        t.hint_session_path(Some(new.to_path_buf()));
        let target = t.target().clone();
        self.gw.mutate(
            Mutation::SetSessionPath {
                target,
                path: Some(new.to_path_buf()),
            },
            |_, result| {
                if let Err(e) = result {
                    log::debug!("setting the session file failed: {e}");
                }
            },
        );
        self.dirty = true;
    }

    /// Stops only the tabs still inside the worktree, then removes it. Which tabs those are is
    /// decided off the UI thread from each tab's live folder (see [`worktree::classify`]); the
    /// gateway stops them, checks again that no other session still runs in the folder, moves
    /// every session file of the worktree into the primary and removes the folder. `moved` are
    /// the tabs that already left through `/move`: never stopped, even while their live folder
    /// still reads as the worktree.
    fn classify_removal(&mut self, plan: RemovePlan, moved: Vec<u64>) {
        let mut tabs = self.tab_cwds();
        tabs.retain(|t| !moved.contains(&t.uid));
        let asked = plan.worktree.clone();
        self.gw.native(
            move || worktree::classify(&asked, &tabs),
            move |app, result| app.on_classified(plan, moved, result),
        );
    }

    fn on_classified(
        &mut self,
        plan: RemovePlan,
        moved: Vec<u64>,
        result: Result<Vec<u64>, String>,
    ) {
        let stop: Vec<u64> = match result {
            Ok(stop) => stop.into_iter().filter(|u| !moved.contains(u)).collect(),
            Err(e) => {
                self.wt.removing.remove(&plan.worktree.path);
                let label = worktree_ui::worktree_label(&plan.worktree);
                self.toasts
                    .error(&self.ctx, format!("Cannot remove “{label}”: {e}"));
                return;
            }
        };
        let targets_of = |app: &App, uids: &[u64]| -> Vec<_> {
            app.tabs
                .iter()
                .filter(|t| uids.contains(&t.uid))
                .map(|t| t.target().clone())
                .collect()
        };
        let targets = targets_of(self, &stop);
        let moved_targets = targets_of(self, &moved);
        for i in (0..self.tabs.len()).rev() {
            if matches!(self.tabs.get(i), Some(t) if stop.contains(&t.uid)) {
                self.close_tab(i, false);
            }
        }
        let count = moved_targets.len();
        self.gw.mutate(
            Mutation::WorktreeRemove {
                worktree: plan.worktree.clone(),
                base: plan.base.clone(),
                how: plan.how,
                stop: targets,
                moved: moved_targets,
            },
            move |app, r| {
                let result = r.and_then(|d| match d {
                    Done::Removed { removed, sessions } => Ok((removed, sessions)),
                    _ => Err("the gateway answered with something else".to_owned()),
                });
                app.on_removed(plan, result, count);
            },
        );
    }

    fn on_removed(
        &mut self,
        plan: RemovePlan,
        result: Result<(Removed, Vec<String>), String>,
        moved: usize,
    ) {
        let wt = plan.worktree;
        self.wt.removing.remove(&wt.path);
        let label = worktree_ui::worktree_label(&wt);
        let (removed, relocated) = match result {
            Ok(r) => r,
            Err(e) => {
                self.toasts
                    .error(&self.ctx, format!("Could not remove “{label}”: {e}"));
                self.refresh_sessions();
                return;
            }
        };
        let parked = plan.how == Removal::Park;
        let mut text = if parked {
            format!(
                "Parked “{label}”: the folder is removed, the branch is kept (OmpDeck never deletes it)"
            )
        } else {
            worktree_ui::removal_text(&label, wt.branch.as_deref(), &removed)
        };
        if moved > 0 {
            text += &format!(
                "; {moved} running session{} moved into {}",
                if moved == 1 { "" } else { "s" },
                abbreviate_home(&wt.primary)
            );
        }
        if !parked
            && removed.branch_note.is_some()
            && wt.branch.is_some()
            && !removed.branch_deleted
        {
            self.toasts.info(&self.ctx, text);
        } else {
            self.toasts.success(&self.ctx, text);
        }
        self.wt.parkable.remove(&wt.path);
        if parked && let Some(branch) = wt.branch.clone() {
            self.record_parked(ParkedBranch {
                primary: wt.primary.clone(),
                branch,
                base: plan.base.clone(),
                sessions: {
                    let mut ids = relocated;
                    for id in plan.sessions {
                        if !ids.contains(&id) {
                            ids.push(id);
                        }
                    }
                    ids
                },
                parked_ms: unix_millis(),
            });
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

    // ------------------------------------------------------------ parked branches

    fn parked_of(&self, entry: &SessionEntry) -> Option<ParkedBranch> {
        self.cfg
            .parked_branches
            .iter()
            .find(|r| r.sessions.contains(&entry.id))
            .cloned()
    }

    /// Remembers a parked branch (replacing the record of the same branch; its sessions merge).
    fn record_parked(&mut self, mut rec: ParkedBranch) {
        if let Some(old) = self
            .cfg
            .parked_branches
            .iter()
            .position(|r| r.primary == rec.primary && r.branch == rec.branch)
        {
            let old = self.cfg.parked_branches.remove(old);
            for id in old.sessions {
                if !rec.sessions.contains(&id) {
                    rec.sessions.push(id);
                }
            }
        }
        self.set_park_state(&rec.primary, &rec.branch, None);
        self.cfg.parked_branches.push(rec);
        self.dirty = true;
    }

    fn drop_parked(&mut self, primary: &Path, branch: &str) {
        self.cfg
            .parked_branches
            .retain(|r| !(r.primary == primary && r.branch == branch));
        self.wt
            .park_states
            .retain(|s| !(s.primary == primary && s.branch == branch));
        self.dirty = true;
    }

    fn set_park_state(&mut self, primary: &Path, branch: &str, checked_out: Option<PathBuf>) {
        match self
            .wt
            .park_states
            .iter_mut()
            .find(|s| s.primary == primary && s.branch == branch)
        {
            Some(s) => s.checked_out = checked_out,
            None => self.wt.park_states.push(ParkState {
                primary: primary.to_path_buf(),
                branch: branch.to_owned(),
                checked_out,
            }),
        }
    }

    fn query_branch(
        &mut self,
        rec: &ParkedBranch,
        then: impl FnOnce(&mut App, Result<BranchState, String>) + 'static,
    ) {
        let q = Query::BranchState {
            primary: rec.primary.clone(),
            branch: rec.branch.clone(),
            base: rec.base.clone(),
        };
        self.gw.query(q, move |app, r| {
            let state = unexpected(r, |a| match a {
                Answer::BranchState(s) => Some(s),
                _ => None,
            })
            .map_err(|e| brief(&e));
            then(app, state);
        });
    }

    /// Startup and every ten minutes: looks at each parked branch through the gateway. A record
    /// goes when its branch no longer exists, or is merged and checked out nowhere. Branches are
    /// never deleted here.
    fn check_parked(&mut self, ctx: &egui::Context) {
        if self.cfg.parked_branches.is_empty() {
            self.wt.park_next = None;
            self.wt.park_states.clear();
            return;
        }
        let now = Instant::now();
        match self.wt.park_next {
            Some(at) if now < at => {
                ctx.request_repaint_after(at - now);
                return;
            }
            _ => {}
        }
        if self.groups().is_none() {
            return;
        }
        self.wt.park_next = Some(now + PARK_CHECK);
        ctx.request_repaint_after(PARK_CHECK);
        for rec in self.cfg.parked_branches.clone() {
            let asked = rec.clone();
            self.query_branch(&asked, move |app, state| app.on_park_checked(&rec, state));
        }
    }

    fn on_park_checked(&mut self, rec: &ParkedBranch, state: Result<BranchState, String>) {
        let st = match state {
            Ok(st) => st,
            Err(e) => {
                log::debug!("parked branch {}: {e}", rec.branch);
                // The gateway may not be up yet: look again soon.
                self.wt.park_next = Some(Instant::now() + Duration::from_secs(30));
                return;
            }
        };
        if !st.exists || (st.merged && st.checked_out.is_none()) {
            self.drop_parked(&rec.primary, &rec.branch);
        } else {
            self.set_park_state(&rec.primary, &rec.branch, st.checked_out);
        }
    }

    /// "Continue in worktree": a worktree on the parked branch at its default location, the
    /// branch's sessions move into it, then the clicked one opens.
    pub(super) fn continue_in_worktree(&mut self, entry: SessionEntry) {
        let Some(rec) = self.parked_of(&entry) else {
            self.toasts
                .error(&self.ctx, "This session is not parked anymore");
            return;
        };
        if !self.omp_found()
            || !self
                .wt
                .continuing
                .insert((rec.primary.clone(), rec.branch.clone()))
        {
            return;
        }
        let asked = rec.clone();
        self.query_branch(&asked, move |app, state| {
            let key = (rec.primary.clone(), rec.branch.clone());
            match app.vet_branch(&rec, state) {
                Some(()) => app.locate_continue(entry, rec),
                None => {
                    app.wt.continuing.remove(&key);
                }
            }
        });
    }

    /// The branch must exist and be checked out nowhere to get its own worktree. Notes the
    /// answer; explains a refusal.
    fn vet_branch(&mut self, rec: &ParkedBranch, state: Result<BranchState, String>) -> Option<()> {
        let st = match state {
            Ok(st) => st,
            Err(e) => {
                self.toasts.error(
                    &self.ctx,
                    format!("Cannot look at branch {}: {e}", rec.branch),
                );
                return None;
            }
        };
        if !st.exists {
            self.drop_parked(&rec.primary, &rec.branch);
            self.toasts.error(
                &self.ctx,
                format!(
                    "Branch {} no longer exists — it is not parked anymore",
                    rec.branch
                ),
            );
            return None;
        }
        self.set_park_state(&rec.primary, &rec.branch, st.checked_out.clone());
        if let Some(at) = st.checked_out {
            self.toasts.error(
                &self.ctx,
                format!(
                    "Branch {} is checked out in {}",
                    rec.branch,
                    if at == rec.primary {
                        "the main checkout".to_owned()
                    } else {
                        abbreviate_home(&at)
                    }
                ),
            );
            return None;
        }
        Some(())
    }

    fn locate_continue(&mut self, entry: SessionEntry, rec: ParkedBranch) {
        let (setting, primary, branch) = (
            self.wt.base_setting.clone(),
            rec.primary.clone(),
            rec.branch.clone(),
        );
        self.gw.native(
            move || {
                let dest = worktree::default_location(
                    &worktree::managed_base(setting.as_deref()),
                    &primary,
                    &branch,
                );
                crate::worktree_dialog::check_destination(&dest).map(|()| dest)
            },
            move |app, located| match located {
                Ok(dest) => app.add_existing(entry, rec, dest),
                Err(e) => {
                    app.wt
                        .continuing
                        .remove(&(rec.primary.clone(), rec.branch.clone()));
                    app.toasts.error(
                        &app.ctx,
                        format!("Cannot create a worktree for {}: {}", rec.branch, brief(&e)),
                    );
                }
            },
        );
    }

    fn add_existing(&mut self, entry: SessionEntry, rec: ParkedBranch, dest: PathBuf) {
        self.toasts.info(
            &self.ctx,
            format!("Creating a worktree on “{}”…", rec.branch),
        );
        let asked = Mutation::WorktreeAddExisting {
            repo: rec.primary.clone(),
            branch: rec.branch.clone(),
            dest: dest.clone(),
        };
        self.gw.mutate(asked, move |app, r| {
            app.wt
                .continuing
                .remove(&(rec.primary.clone(), rec.branch.clone()));
            match r {
                Ok(_) => app.on_worktree_added(entry, rec, dest),
                Err(e) => app.toasts.error(
                    &app.ctx,
                    format!("Could not create the worktree: {}", brief(&e)),
                ),
            }
        });
    }

    /// The worktree exists: the branch is no longer parked, its open sessions move in (`/move`),
    /// its closed ones are relocated headlessly, and the clicked session is opened or focused.
    fn on_worktree_added(&mut self, entry: SessionEntry, rec: ParkedBranch, dest: PathBuf) {
        self.cfg
            .worktree_bases
            .insert(dest.clone(), rec.base.clone());
        self.cfg.removed_worktrees.remove(&dest);
        self.drop_parked(&rec.primary, &rec.branch);
        let members: Vec<(String, PathBuf)> = self
            .groups()
            .map(|gs| {
                gs.iter()
                    .flat_map(|g| &g.sessions)
                    .filter(|e| rec.sessions.contains(&e.id))
                    .map(|e| (e.id.clone(), e.path.clone()))
                    .collect()
            })
            .unwrap_or_default();
        // A closed tab still showing a session would keep a path that is about to move.
        for i in (0..self.tabs.len()).rev() {
            let stale = self.tabs.get(i).is_some_and(|t| {
                t.terminal().is_exited()
                    && t.session_path().is_some_and(|p| {
                        members.iter().any(|(_, m)| worktree_ui::same_session(m, p))
                    })
            });
            if stale {
                self.close_tab(i, false);
            }
        }
        let tab_of = |app: &App, path: &Path| {
            app.tabs
                .iter()
                .find(|t| {
                    !t.terminal().is_exited()
                        && t.session_path()
                            .is_some_and(|p| worktree_ui::same_session(p, path))
                })
                .map(|t| t.uid)
        };
        let moving: Vec<(u64, Option<PathBuf>)> = self
            .tabs
            .iter()
            .filter(|t| {
                t.role == TabRole::Session
                    && !t.terminal().is_exited()
                    && t.session_path().is_some_and(|p| {
                        members.iter().any(|(_, m)| worktree_ui::same_session(m, p))
                    })
            })
            .map(|t| (t.uid, t.session_path_owned()))
            .collect();
        let focus = tab_of(self, &entry.path);
        let snap = self.running_snapshot();
        let files: Vec<PathBuf> = members
            .iter()
            .filter(|(id, path)| !snap.is_running(id, path) && tab_of(self, path).is_none())
            .map(|(_, path)| path.clone())
            .collect();
        let clicked_closed = files.contains(&entry.path);
        if !files.is_empty() {
            self.relocate_closed(files, dest.clone(), clicked_closed.then(|| entry.clone()));
        } else if focus.is_none() {
            // Running elsewhere (no tab): attach as it is.
            self.open_session(&entry);
        }
        if moving.is_empty() {
            self.toasts.success(
                &self.ctx,
                format!("Continuing “{}” in {}", rec.branch, abbreviate_home(&dest)),
            );
        } else {
            self.toasts.info(
                &self.ctx,
                format!(
                    "Moving the open sessions of “{}” into {} once omp is idle (a tab still busy after 30 s stays where it is)…",
                    rec.branch,
                    abbreviate_home(&dest)
                ),
            );
            let plan = ContinuePlan {
                record: rec,
                dest,
                focus,
            };
            let r = Relocating::new(Then::Continue(plan), moving, Instant::now());
            self.wt.relocating.insert(r.key().to_path_buf(), r);
        }
        self.dirty = true;
        self.refresh_sessions();
        self.ctx.request_repaint();
    }

    /// Moves closed sessions into `dest`; `open` (the clicked one, when closed) opens there.
    fn relocate_closed(&mut self, files: Vec<PathBuf>, dest: PathBuf, open: Option<SessionEntry>) {
        let batch: Vec<PathBuf> = files
            .iter()
            .filter(|f| self.wt.in_flight.insert((*f).clone()))
            .cloned()
            .collect();
        if batch.is_empty() {
            return;
        }
        let done = batch.clone();
        self.gw.mutate(
            Mutation::RelocateSessions {
                files: batch,
                dest: dest.clone(),
            },
            move |app, r| {
                for f in &done {
                    app.wt.in_flight.remove(f);
                }
                let results = match r {
                    Ok(Done::Relocated(results)) => results,
                    Ok(_) => Vec::new(),
                    Err(e) => {
                        app.toasts.error(
                            &app.ctx,
                            format!("Could not move the parked sessions: {}", brief(&e)),
                        );
                        if let Some(entry) = open {
                            app.open_session(&entry);
                        }
                        return;
                    }
                };
                if let Some(why) = results.iter().find_map(|(_, r)| r.as_ref().err()) {
                    app.toasts.error(
                        &app.ctx,
                        format!("Could not move a parked session: {}", brief(why)),
                    );
                }
                app.refresh_sessions();
                if let Some(entry) = open {
                    let moved = results
                        .into_iter()
                        .find(|(old, _)| *old == entry.path)
                        .and_then(|(_, r)| r.ok());
                    match moved {
                        Some(new_path) => app.open_entry_at(&entry, new_path, dest),
                        None => app.open_session(&entry),
                    }
                }
            },
        );
    }

    /// The tabs of a continued branch moved (or the wait ran out): focus the clicked one.
    fn finish_continue(&mut self, plan: ContinuePlan, moved: &[u64], stayed: usize) {
        if stayed > 0 {
            self.toasts.info(
                &self.ctx,
                format!(
                    "{stayed} tab{} of “{}” did not move: still busy or not accepting /move",
                    if stayed == 1 { "" } else { "s" },
                    plan.record.branch
                ),
            );
        } else {
            self.toasts.success(
                &self.ctx,
                format!(
                    "Continuing “{}” in {}",
                    plan.record.branch,
                    abbreviate_home(&plan.dest)
                ),
            );
        }
        if let Some(uid) = plan.focus.filter(|u| moved.contains(u))
            && let Some(i) = self.tabs.iter().position(|t| t.uid == uid)
        {
            self.activate(i);
        }
        self.refresh_sessions();
    }

    /// "Continue in main checkout": the branch is checked out in the main checkout after a
    /// confirmation that names the sessions running there.
    pub(super) fn continue_in_main(&mut self, entry: SessionEntry) {
        let Some(rec) = self.parked_of(&entry) else {
            self.toasts
                .error(&self.ctx, "This session is not parked anymore");
            return;
        };
        let asked = rec.clone();
        self.query_branch(&asked, move |app, state| {
            let st = match state {
                Ok(st) => st,
                Err(e) => {
                    app.toasts.error(
                        &app.ctx,
                        format!("Cannot look at branch {}: {e}", rec.branch),
                    );
                    return;
                }
            };
            if !st.exists {
                app.drop_parked(&rec.primary, &rec.branch);
                app.toasts.error(
                    &app.ctx,
                    format!(
                        "Branch {} no longer exists — it is not parked anymore",
                        rec.branch
                    ),
                );
                return;
            }
            app.set_park_state(&rec.primary, &rec.branch, st.checked_out.clone());
            match st.checked_out {
                Some(at) if at == rec.primary => app.open_session(&entry),
                Some(at) => app.toasts.error(
                    &app.ctx,
                    format!(
                        "Branch {} is checked out in {}",
                        rec.branch,
                        abbreviate_home(&at)
                    ),
                ),
                None => {
                    let target = rec.branch.clone();
                    app.confirm_switch(entry, rec.primary, target, true);
                }
            }
        });
    }

    /// "Switch main checkout back to <base>": the parked branch stays; nothing opens.
    pub(super) fn switch_main_back(&mut self, entry: SessionEntry) {
        let Some(rec) = self.parked_of(&entry) else {
            self.toasts
                .error(&self.ctx, "This session is not parked anymore");
            return;
        };
        self.confirm_switch(entry, rec.primary, rec.base, false);
    }

    /// Asks the gateway which live backend sessions exist, then warns about every session that
    /// runs in the main checkout (tab or not) before switching its branch.
    fn confirm_switch(
        &mut self,
        entry: SessionEntry,
        primary: PathBuf,
        target: String,
        open: bool,
    ) {
        self.gw.query(Query::RunningDetail, move |app, r| {
            let running = match r {
                Ok(Answer::RunningDetail(d)) => d.sessions,
                _ => Vec::new(),
            };
            app.show_switch_confirm(entry, primary, target, open, &running);
        });
    }

    fn show_switch_confirm(
        &mut self,
        entry: SessionEntry,
        primary: PathBuf,
        target: String,
        open: bool,
        running: &[deck_core::mux::RunningSession],
    ) {
        let groups = self.groups();
        let entries = || {
            groups
                .iter()
                .flat_map(|g| g.iter())
                .flat_map(|g| &g.sessions)
        };
        let tab_label = |path: &Path| -> Option<String> {
            self.tabs
                .iter()
                .find(|t| {
                    t.session_path()
                        .is_some_and(|p| worktree_ui::same_session(p, path))
                })
                .map(|t| t.fallback_title.clone())
        };
        let mut live: Vec<worktree_ui::LiveSession> = Vec::new();
        if let Some(infos) = self.gw.controllers() {
            for i in infos.iter().filter(|i| {
                i.role == deck_service::gateway::protocol::SessionRole::Session
                    && i.exit_code.is_none()
            }) {
                let name = match &i.backend {
                    deck_service::gateway::protocol::BackendRef::Mux { name, .. } => {
                        Some(name.clone())
                    }
                    deck_service::gateway::protocol::BackendRef::Direct => None,
                };
                let files: Vec<PathBuf> = i.session_path.iter().cloned().collect();
                // Only the fallback: `main_checkout_sessions` names known sessions by title.
                let label = i
                    .session_path
                    .as_deref()
                    .and_then(tab_label)
                    .or_else(|| i.title.clone())
                    .unwrap_or_else(|| i.label.clone());
                live.push(worktree_ui::LiveSession {
                    name,
                    files,
                    dir: Some(i.live_cwd.clone().unwrap_or_else(|| i.cwd.clone())),
                    label,
                });
            }
        }
        for r in running {
            let label = r
                .files
                .iter()
                .find_map(|f| tab_label(f))
                .or_else(|| r.title.clone())
                .unwrap_or_else(|| r.name.clone());
            live.push(worktree_ui::LiveSession {
                name: Some(r.name.clone()),
                files: r.files.clone(),
                dir: r.cwd.clone(),
                label,
            });
        }
        let worktrees: Vec<PathBuf> = self
            .groups()
            .map(|gs| {
                gs.iter()
                    .filter_map(|g| g.worktree.as_ref())
                    .map(|w| w.path.clone())
                    .collect()
            })
            .unwrap_or_default();
        let entries: Vec<&SessionEntry> = entries().collect();
        let tabs =
            worktree_ui::main_checkout_sessions(&live, &primary, &worktrees, &entry.path, &entries);
        let seen = if tabs.is_empty() {
            "No other session runs in the main checkout.".to_owned()
        } else {
            format!(
                "These sessions run in the main checkout and will see branch {target}:\n{}",
                tabs.iter()
                    .map(|t| format!("• {t}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        let (title, lead, confirm_label) = if open {
            (
                "Continue in main checkout?",
                format!(
                    "Branch “{target}” is checked out in the main checkout {}, and this session opens there.",
                    abbreviate_home(&primary)
                ),
                "Switch",
            )
        } else {
            (
                "Switch main checkout back?",
                format!(
                    "The main checkout {} switches back to {target}; the parked branch stays.",
                    abbreviate_home(&primary)
                ),
                "Switch back",
            )
        };
        self.confirm = Some((
            Confirm {
                title: title.to_owned(),
                body: format!(
                    "{lead}\n\n{seen}\n\nA checkout with uncommitted changes is refused; nothing is discarded."
                ),
                confirm_label: confirm_label.to_owned(),
                danger: false,
            },
            PendingConfirm::SwitchMain {
                entry,
                primary,
                target,
                open,
            },
        ));
    }

    pub(super) fn execute_switch_main(
        &mut self,
        entry: SessionEntry,
        primary: PathBuf,
        target: String,
        open: bool,
    ) {
        let rec = self.parked_of(&entry);
        let op = Mutation::SwitchBranch {
            dir: primary.clone(),
            branch: target.clone(),
        };
        self.gw.mutate(op, move |app, r| match r {
            Ok(_) => {
                if let Some(rec) = rec {
                    let at = open.then(|| primary.clone());
                    app.set_park_state(&rec.primary, &rec.branch, at);
                }
                app.toasts
                    .success(&app.ctx, format!("The main checkout is on {target} now"));
                if open {
                    app.open_session(&entry);
                }
            }
            Err(e) => app.toasts.error(
                &app.ctx,
                format!(
                    "Cannot switch the main checkout to {target}: {}",
                    dirty_reason(&e, &primary).unwrap_or_else(|| brief(&e))
                ),
            ),
        });
    }

    // ------------------------------------------------------------ moves into a worktree

    /// Once per run: forget removed worktrees whose primary repository is gone too.
    fn prune_removed_once(&mut self) {
        if self.wt.pruned {
            return;
        }
        if self.cfg.removed_worktrees.is_empty() {
            self.wt.pruned = true;
            return;
        }
        // The leftovers are looked for in the first scan.
        let Some(groups) = self.groups() else {
            return;
        };
        self.wt.pruned = true;
        let snap = self.running_snapshot();
        let open: Vec<PathBuf> = self
            .tabs
            .iter()
            .filter_map(|t| t.session_path_owned())
            .collect();
        let leftovers: Vec<Leftover> = self
            .cfg
            .removed_worktrees
            .iter()
            .map(|(path, r)| {
                let all: Vec<&SessionEntry> = groups
                    .iter()
                    .flat_map(|g| &g.sessions)
                    .filter(|e| e.cwd == *path)
                    .collect();
                let files: Vec<PathBuf> = all
                    .iter()
                    .filter(|e| !snap.is_running(&e.id, &e.path) && !open.contains(&e.path))
                    .map(|e| e.path.clone())
                    .collect();
                Leftover {
                    worktree: path.clone(),
                    primary: r.primary.clone(),
                    all_movable: files.len() == all.len(),
                    none: all.is_empty(),
                    files,
                }
            })
            .collect();
        let removed = self.cfg.removed_worktrees.clone();
        self.gw.native(
            move || worktree_ui::orphaned_removed(&removed),
            move |app, paths| {
                for p in &paths {
                    app.cfg.removed_worktrees.remove(p);
                }
                app.dirty = true;
                for l in leftovers
                    .into_iter()
                    .filter(|l| !paths.contains(&l.worktree))
                {
                    app.relocate_leftover(l);
                }
            },
        );
    }

    /// Sessions that still carry a removed worktree's folder: move the idle ones into the primary
    /// checkout; the record goes once nothing of it remains.
    fn relocate_leftover(&mut self, l: Leftover) {
        if l.none {
            self.cfg.removed_worktrees.remove(&l.worktree);
            self.dirty = true;
            return;
        }
        if l.files.is_empty() {
            return;
        }
        let n = l.files.len();
        // Files an open is moving right now are left to it.
        let files: Vec<PathBuf> = l
            .files
            .into_iter()
            .filter(|f| self.wt.in_flight.insert(f.clone()))
            .collect();
        if files.is_empty() {
            return;
        }
        let (worktree, all_movable) = (l.worktree, l.all_movable && files.len() == n);
        let batch = files.clone();
        self.gw.mutate(
            Mutation::RelocateSessions {
                files,
                dest: l.primary,
            },
            move |app, r| {
                for f in &batch {
                    app.wt.in_flight.remove(f);
                }
                let results = match r {
                    Ok(Done::Relocated(results)) => results,
                    Ok(_) => {
                        log::debug!("relocating leftovers: unexpected answer");
                        return;
                    }
                    Err(e) => {
                        app.toasts.error(
                            &app.ctx,
                            format!(
                                "Could not move sessions of a removed worktree: {}",
                                brief(&e)
                            ),
                        );
                        return;
                    }
                };
                let failed: Vec<&String> = results
                    .iter()
                    .filter_map(|(_, r)| r.as_ref().err())
                    .collect();
                if let Some(why) = failed.first() {
                    app.toasts.error(
                        &app.ctx,
                        format!(
                            "Could not move {} of {n} sessions of a removed worktree: {}",
                            failed.len(),
                            brief(why)
                        ),
                    );
                } else if all_movable {
                    app.cfg.removed_worktrees.remove(&worktree);
                    app.dirty = true;
                }
                app.refresh_sessions();
            },
        );
    }

    /// A session of a removed worktree is opened: omp moves it into the primary checkout first,
    /// then the tab opens on the moved file there. When the move fails the session opens as
    /// before (launched in the primary) with an error toast; when its file is gone by then
    /// (another move got it first) the fresh scan is searched for the session by id.
    pub(super) fn open_relocated(&mut self, entry: &SessionEntry, primary: PathBuf) {
        if !self.wt.in_flight.insert(entry.path.clone()) {
            self.toasts.info(
                &self.ctx,
                "This session is being moved — try again in a moment",
            );
            return;
        }
        let entry = entry.clone();
        self.gw.mutate(
            Mutation::RelocateSessions {
                files: vec![entry.path.clone()],
                dest: primary.clone(),
            },
            move |app, r| {
                app.wt.in_flight.remove(&entry.path);
                let moved = match r {
                    Ok(Done::Relocated(results)) => results
                        .into_iter()
                        .next()
                        .map_or_else(|| Err("nothing was moved".to_owned()), |(_, r)| r),
                    Ok(_) => Err("the gateway answered with something else".to_owned()),
                    Err(e) => Err(e),
                };
                match moved {
                    Ok(new_path) => {
                        app.refresh_sessions();
                        app.open_entry_at(&entry, new_path, primary);
                    }
                    Err(e) => {
                        app.toasts.error(
                            &app.ctx,
                            format!(
                                "Could not move the session into {}: {}",
                                abbreviate_home(&primary),
                                brief(&e)
                            ),
                        );
                        let old = entry.path.clone();
                        app.gw.native(
                            move || old.exists(),
                            move |app, exists| app.open_after_failed_move(&entry, primary, exists),
                        );
                    }
                }
            },
        );
    }

    /// The move of `entry` failed: open its file where it is, or — when it no longer exists —
    /// the session with the same id from the latest scan.
    fn open_after_failed_move(&mut self, entry: &SessionEntry, primary: PathBuf, exists: bool) {
        if exists {
            self.open_entry_at(entry, entry.path.clone(), primary);
            return;
        }
        self.refresh_sessions();
        let moved = self.groups().and_then(|gs| {
            gs.iter()
                .flat_map(|g| &g.sessions)
                .find(|e| e.id == entry.id && e.path != entry.path)
                .cloned()
        });
        match moved {
            Some(found) => self.open_entry_at(&found, found.path.clone(), found.cwd.clone()),
            None => self.toasts.error(
                &self.ctx,
                "The session's file is gone — rescan and try again",
            ),
        }
    }

    fn expire_moves(&mut self) {
        self.wt.moves.retain(|m| m.since.elapsed() < MOVE_EXPIRES);
    }

    /// A tab whose session file changed (omp relocated it, `/new`, `/resume`, …) takes the folder
    /// of the new file; a pending move that landed in a worktree records its merge target.
    fn track_relocations(&mut self) {
        let generation = self.gw.sessions_generation();
        let live: HashSet<u64> = self.tabs.iter().map(|t| t.uid).collect();
        self.wt.seen.retain(|u, _| live.contains(u));
        self.wt.seen_cwd.retain(|u, _| live.contains(u));
        self.wt.tried.retain(|u, _| live.contains(u));
        let mut changed = Vec::new();
        for tab in self.tabs.iter() {
            let live_cwd = tab.info().and_then(|i| i.live_cwd.clone());
            let path = tab.session_path_owned();
            let (Some(prev_path), Some(prev_cwd)) = (
                self.wt.seen.get(&tab.uid).cloned(),
                self.wt.seen_cwd.get(&tab.uid).cloned(),
            ) else {
                self.wt.seen.insert(tab.uid, path);
                self.wt.seen_cwd.insert(tab.uid, live_cwd);
                continue;
            };
            // A move shows as a new session file, or as a new live folder with the recorded
            // file still the old one.
            let cwd_moved = live_cwd.is_some() && live_cwd != prev_cwd;
            if (prev_path != path || cwd_moved)
                && let Some(p) = path
                && self.wt.tried.get(&tab.uid) != Some(&(p.clone(), generation))
            {
                changed.push((tab.uid, p, live_cwd));
            } else if !cwd_moved {
                self.wt.seen_cwd.insert(tab.uid, live_cwd);
            }
        }
        if changed.is_empty() {
            return;
        }
        let groups = self.groups();
        for (uid, path, live_cwd) in changed {
            // By file name: the recorded path may predate the move.
            let found = groups.as_deref().and_then(|gs| {
                gs.iter().find_map(|g| {
                    g.sessions
                        .iter()
                        .find(|e| e.path == path)
                        .or_else(|| {
                            g.sessions
                                .iter()
                                .find(|e| worktree_ui::same_session(&e.path, &path))
                        })
                        .map(|e| (g, e))
                })
            });
            let Some((group, entry)) = found else {
                self.wt.tried.insert(uid, (path, generation));
                continue;
            };
            self.wt.tried.remove(&uid);
            self.wt.seen_cwd.insert(uid, live_cwd.clone());
            if entry.path != path {
                self.fix_tab_path(uid, &entry.path);
            }
            self.wt.seen.insert(uid, Some(entry.path.clone()));
            let folder = live_cwd.unwrap_or_else(|| entry.cwd.clone());
            if let Some(t) = self.tabs.iter_mut().find(|t| t.uid == uid)
                && t.cwd != folder
            {
                t.cwd = folder;
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
