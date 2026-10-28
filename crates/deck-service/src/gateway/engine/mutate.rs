//! Journaled mutations: validation before the journal, admission (dedup, reused and expired
//! keys), execution over the real executors (inline, on the controllers, or on worker jobs with
//! progress, cancellation and the one-at-a-time Drive lane) and the final outcome.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::sessions::LaunchJob;
use super::{Engine, Fin, JobState, Msg, Sched, lock};
use crate::controller::{RequestKey, ServiceEpoch, Submission, Submitted};
use crate::gateway::desktop;
use crate::gateway::jobs::{self, JobCtx};
use crate::gateway::journal::Admission;
use crate::gateway::protocol::{
    Accepted, Answer, Done, GatewayError, LaunchSpec, Mutation, Outcome, Progress, Role,
    ServerEvent, SessionRole, Tool, UpdatePhase,
};
use crate::settings::{Field, SettingsError, Snapshot};

/// Progress events of one job are sent at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(150);

/// What finishing a job also does.
pub(super) enum Post {
    None,
    /// Rescan the sessions and poll the backends.
    Refresh,
    /// Recompute the changes of this repository (when followed).
    Changes(PathBuf),
    /// Reload the Google account (sign-in, sign-out, backups).
    DriveAccount,
    /// The result is the app-update state after the job.
    AppView,
}

/// Throttled progress reports of a worker job.
struct Reporter {
    tx: std::sync::mpsc::Sender<Msg>,
    key: RequestKey,
    last: Mutex<Option<Instant>>,
}

impl Reporter {
    fn report(&self, progress: Progress) {
        let now = Instant::now();
        {
            let mut last = lock(&self.last);
            let finished = progress.total.is_some_and(|t| progress.done >= t);
            if !finished && last.is_some_and(|t| now.saturating_duration_since(t) < PROGRESS_EVERY)
            {
                return;
            }
            *last = Some(now);
        }
        let _ = self.tx.send(Msg::Fin(Box::new(Fin::Progress {
            key: self.key.clone(),
            progress,
        })));
    }
}

fn send(tx: &std::sync::mpsc::Sender<Msg>, fin: Fin) {
    let _ = tx.send(Msg::Fin(Box::new(fin)));
}

/// Mutations that hold the Google Drive lane.
fn uses_lane(op: &Mutation) -> bool {
    matches!(
        op,
        Mutation::DriveLoginFinish(_)
            | Mutation::DriveSignOut
            | Mutation::DriveBackup { .. }
            | Mutation::DrivePrepareRestore(_)
            | Mutation::DriveDeleteBackup(_)
            | Mutation::DriveApplyRestore(_)
            | Mutation::ProjectBackup { .. }
            | Mutation::ProjectDeleteBackup(_)
            | Mutation::ProjectPrepareRestore { .. }
            | Mutation::ProjectApplyRestore { .. }
    )
}

impl Engine {
    pub(super) fn on_mutate(
        &mut self,
        conn: u64,
        id: u64,
        key: RequestKey,
        epoch: ServiceEpoch,
        op: Mutation,
    ) {
        let Some(c) = self.conns.get(&conn) else {
            return;
        };
        if c.role == Role::Supervisor {
            self.reply(
                conn,
                id,
                Err(GatewayError::Refused(
                    "the supervisor cannot change anything".to_owned(),
                )),
            );
            return;
        }
        if key.client != c.client_id {
            self.reply(
                conn,
                id,
                Err(GatewayError::Refused(
                    "this request key belongs to another client".to_owned(),
                )),
            );
            return;
        }
        if epoch != self.epoch {
            self.reply(
                conn,
                id,
                Err(GatewayError::StaleEpoch {
                    current: self.epoch,
                }),
            );
            return;
        }
        // A key seen before is answered from its record (duplicate, reused or expired) without
        // re-validating against state that changed since it was accepted.
        let known = !matches!(self.journal.lookup(&key), Ok(None));
        if !known {
            if let Some(m) = &self.barrier
                && !op.allowed_in_maintenance()
            {
                self.reply(conn, id, Err(GatewayError::Maintenance(m.clone())));
                return;
            }
            if let Some(e) = self.precheck(&key, &op) {
                self.reply(conn, id, Err(e));
                return;
            }
        }
        match self.journal.admit(&key, &op, self.epoch) {
            Err(e) => self.reply(
                conn,
                id,
                Err(GatewayError::Failed(format!(
                    "the journal could not record this: {e}"
                ))),
            ),
            Ok(Admission::Accepted) => self.execute(conn, id, key, op),
            Ok(Admission::Duplicate(Outcome::Rejected(r))) => {
                self.reply(conn, id, Err(GatewayError::Rejected(r)));
            }
            Ok(Admission::Duplicate(outcome)) => {
                let submitted = match &op {
                    Mutation::Submit { target, .. } => self
                        .sess
                        .find(&target.key)
                        .and_then(|i| self.sess.managed[i].ctl.request_by_key(&key))
                        .map(|r| Submitted {
                            id: r.id,
                            duplicate: true,
                        }),
                    _ => None,
                };
                self.reply(
                    conn,
                    id,
                    Ok(Answer::Accepted(Accepted {
                        key: key.clone(),
                        duplicate: true,
                        submitted,
                    })),
                );
                if outcome.is_final() {
                    self.to_client(&key.client.clone(), &ServerEvent::Outcome { key, outcome });
                }
            }
            Ok(Admission::KeyReused) => self.reply(conn, id, Err(GatewayError::KeyReused)),
            Ok(Admission::Expired) => self.reply(conn, id, Err(GatewayError::Expired)),
        }
    }

    /// Refusals decided before anything is journaled (a retry re-evaluates them).
    fn precheck(&self, key: &RequestKey, op: &Mutation) -> Option<GatewayError> {
        let refused = |s: &str| Some(GatewayError::Refused(s.to_owned()));
        if let Some(t) = op.target()
            && let Err(e) = self.check_target(t)
        {
            return Some(e);
        }
        let managed = |t: &crate::controller::SessionTarget| {
            self.sess.find(&t.key).map(|i| &self.sess.managed[i])
        };
        if uses_lane(op)
            && let Some(busy) = &self.drive_lane
        {
            return Some(GatewayError::Refused(format!(
                "Another Google Drive job is running ({busy}); try again."
            )));
        }
        match op {
            Mutation::Launch(spec) if spec.argv.is_empty() => refused("there is nothing to run"),
            Mutation::Close {
                target,
                kill_direct: false,
            } => match managed(target) {
                Some(m) if m.is_direct() && !m.exited() => {
                    refused("a direct program ends with its session: close it with kill")
                }
                _ => None,
            },
            Mutation::Answer { target, id, .. } => {
                match managed(target).and_then(|m| m.ctl.request(*id)) {
                    None => refused("no such request"),
                    Some(r) if r.key.client != key.client => {
                        refused("only the client that sent the command can answer it")
                    }
                    Some(_) => None,
                }
            }
            Mutation::Ctl { target, .. } => match managed(target) {
                Some(m) if m.ctl_key.is_none() => refused("this session runs without live control"),
                _ => None,
            },
            Mutation::Settings(edit)
                if edit.changes.iter().any(|s| s.field() == Field::GatewayMode) =>
            {
                refused("change the gateway mode with SetGatewayMode")
            }
            Mutation::WorktreeRemove {
                worktree,
                stop,
                moved,
                ..
            } => {
                for t in stop {
                    if let Err(e) = self.check_target(t) {
                        return Some(e);
                    }
                }
                // `moved` sessions are only exempted here, never acted on: their `/move` started
                // a new generation, so they are matched by session, not by exact target.
                self.sess
                    .managed
                    .iter()
                    .find(|m| {
                        let target = m.ctl.target();
                        !m.exited()
                            && m.current_cwd().starts_with(&worktree.path)
                            && !stop.iter().any(|t| t == target)
                            && !moved.iter().any(|t| t.key == target.key)
                    })
                    .map(|_| {
                        GatewayError::Refused(
                            "another session still runs in this worktree".to_owned(),
                        )
                    })
            }
            Mutation::RunTool(Tool::Update(_)) | Mutation::OmpUpdateCheck if self.omp.is_none() => {
                refused("omp was not found")
            }
            Mutation::UsageRefresh { .. } if self.omp.is_none() => refused("omp was not found"),
            Mutation::AppUpdateDownload => self.sched.download_refusal(),
            Mutation::AppUpdateApply => self.sched.apply_refusal(),
            Mutation::CancelJob(k) => match self.jobs.get(k) {
                None => Some(GatewayError::NoSuchJob),
                Some(j) if j.cancel.is_none() => refused("this job cannot be cancelled"),
                Some(_) => None,
            },
            _ => None,
        }
    }

    // ---- job bookkeeping ----------------------------------------------------------------------

    fn accepted(&self, conn: u64, id: u64, key: &RequestKey) {
        self.reply(
            conn,
            id,
            Ok(Answer::Accepted(Accepted {
                key: key.clone(),
                duplicate: false,
                submitted: None,
            })),
        );
    }

    /// Lists an accepted mutation as a running job (without a worker of its own).
    fn track(&mut self, key: &RequestKey, label: &'static str, post: Post) {
        self.jobs.insert(
            key.clone(),
            JobState {
                label,
                progress: None,
                cancel: None,
                lane: false,
                post,
            },
        );
        self.dirty.gateway = true;
    }

    /// Runs `f` on a worker as the job of `key`.
    fn job<F>(
        &mut self,
        key: RequestKey,
        label: &'static str,
        lane: bool,
        cancellable: bool,
        post: Post,
        f: F,
    ) where
        F: FnOnce(&JobCtx, &AtomicBool, &(dyn Fn(Progress) + Sync)) -> Result<Done, GatewayError>
            + Send
            + 'static,
    {
        let cancel = Arc::new(AtomicBool::new(false));
        if lane {
            self.drive_lane = Some(label.to_owned());
            self.dirty.drive = true;
        }
        self.jobs.insert(
            key.clone(),
            JobState {
                label,
                progress: None,
                cancel: cancellable.then(|| cancel.clone()),
                lane,
                post,
            },
        );
        self.dirty.gateway = true;
        let ctx = self.job_ctx();
        let tx = self.tx.clone();
        self.spawn_worker(label, move || {
            let reporter = Reporter {
                tx,
                key: key.clone(),
                last: Mutex::new(None),
            };
            let result = f(&ctx, &cancel, &|p| reporter.report(p));
            Fin::Mutation {
                key,
                result: result.map(Box::new),
            }
        });
    }

    /// Records the final outcome and tells the client's links.
    pub(super) fn finish_journal(&mut self, key: &RequestKey, outcome: Outcome) {
        self.journal.finish(key, outcome.clone());
        self.to_client(
            &key.client,
            &ServerEvent::Outcome {
                key: key.clone(),
                outcome,
            },
        );
    }

    /// [`Engine::job_finished`] with a failure.
    pub(super) fn job_failed(&mut self, key: RequestKey, e: GatewayError) {
        self.job_finished(key, Err::<Box<Done>, _>(e));
    }

    pub(super) fn job_finished(
        &mut self,
        key: RequestKey,
        result: Result<impl Into<Box<Done>>, GatewayError>,
    ) {
        let state = self.jobs.remove(&key);
        let mut result = result.map(Into::into);
        let mut cancelled = false;
        if let Some(s) = state {
            if s.lane {
                self.drive_lane = None;
                self.dirty.drive = true;
            }
            cancelled = s.cancel.is_some_and(|c| c.load(Ordering::Relaxed));
            match s.post {
                Post::None => {}
                Post::Refresh => {
                    self.sessions_watch.refresh_now();
                    self.running.poke();
                }
                Post::Changes(root) => self.refresh_changes(&root),
                Post::DriveAccount => self.load_drive_account(),
                Post::AppView => {
                    if let Ok(done) = &mut result {
                        **done = Done::AppUpdate(self.sched.app_view());
                    }
                    self.dirty.app = true;
                }
            }
        }
        let outcome = match result {
            Ok(done) => Outcome::Done(done),
            Err(GatewayError::Cancelled) => Outcome::Cancelled,
            Err(_) if cancelled => Outcome::Cancelled,
            Err(GatewayError::Rejected(r)) => Outcome::Rejected(r),
            Err(e) => Outcome::Failed(e.to_string()),
        };
        self.finish_journal(&key, outcome);
        self.dirty.gateway = true;
    }

    pub(super) fn job_progress(&mut self, key: RequestKey, progress: Progress) {
        let Some(j) = self.jobs.get_mut(&key) else {
            return;
        };
        j.progress = Some(progress.clone());
        if matches!(j.post, Post::AppView) {
            self.sched.download_progress(&progress);
            self.dirty.app = true;
        }
        self.dirty.gateway = true;
        self.to_client(
            &key.client.clone(),
            &ServerEvent::Progress { key, progress },
        );
    }

    /// A settings edit is committed (or refused) before its mutation is answered: a CAS
    /// conflict or I/O failure is the reply itself.
    pub(super) fn settings_finished(
        &mut self,
        conn: u64,
        id: u64,
        key: RequestKey,
        result: Result<Snapshot, SettingsError>,
    ) {
        self.jobs.remove(&key);
        self.dirty.gateway = true;
        match result {
            Ok(snap) => {
                self.accepted(conn, id, &key);
                self.refresh_settings();
                self.finish_journal(&key, Outcome::Done(Box::new(Done::Settings(snap))));
            }
            Err(e) => {
                self.journal.finish(&key, Outcome::Failed(e.to_string()));
                self.reply(conn, id, Err(GatewayError::Settings(e)));
            }
        }
    }

    // ---- execution ----------------------------------------------------------------------------

    fn execute(&mut self, conn: u64, id: u64, key: RequestKey, op: Mutation) {
        // Targets were validated before admission; the controller still exists (nothing ran
        // in between on this thread).
        let index = op.target().and_then(|t| self.sess.find(&t.key));
        match op {
            Mutation::Launch(spec) => {
                self.track(&key, "launch", Post::None);
                self.accepted(conn, id, &key);
                self.sess.queue(LaunchJob {
                    spec,
                    key: Some(key),
                    exact: false,
                    restore: false,
                    tool: None,
                });
                self.pump_launches();
            }
            Mutation::Restart(_) => {
                let Some(i) = index else {
                    return self.vanished(conn, id, key);
                };
                self.track(&key, "restart", Post::Refresh);
                self.accepted(conn, id, &key);
                self.begin_restart(i, key);
            }
            Mutation::Stop(_) => {
                let Some(i) = index else {
                    return self.vanished(conn, id, key);
                };
                self.track(&key, "stop", Post::Refresh);
                self.accepted(conn, id, &key);
                self.stop_session(i, key);
            }
            Mutation::Kill(_) => {
                let Some(i) = index else {
                    return self.vanished(conn, id, key);
                };
                self.track(&key, "kill", Post::Refresh);
                self.accepted(conn, id, &key);
                self.kill_session(i, Some(key));
            }
            Mutation::KillNames { names, wait } => {
                // Only the named backend sessions; a controller of one sees its terminal end,
                // exactly as after `Kill`.
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "kill-names",
                    false,
                    false,
                    Post::Refresh,
                    move |ctx, _, _| jobs::kill_names(&ctx.backends, &names, wait),
                );
            }
            Mutation::Close { kill_direct, .. } => {
                let Some(i) = index else {
                    return self.vanished(conn, id, key);
                };
                self.track(&key, "close", Post::None);
                self.accepted(conn, id, &key);
                self.begin_close(i, key, kill_direct);
            }
            Mutation::Submit { target, command } => {
                let Some(i) = index else {
                    return self.vanished(conn, id, key);
                };
                let session = target.key.clone();
                let submitted = self.sess.managed[i].ctl.submit(Submission {
                    key: key.clone(),
                    target,
                    command,
                });
                self.dirty.controllers = true;
                match submitted {
                    Ok(s) => {
                        self.sess.submits.insert(key.clone(), session);
                        self.reply(
                            conn,
                            id,
                            Ok(Answer::Accepted(Accepted {
                                key,
                                duplicate: false,
                                submitted: Some(s),
                            })),
                        );
                        // The first step runs now, not at the next tick.
                        self.waker.now();
                    }
                    Err(r) => {
                        self.journal.finish(&key, Outcome::Rejected(r.clone()));
                        self.reply(conn, id, Err(GatewayError::Rejected(r)));
                    }
                }
            }
            Mutation::Answer {
                target,
                id: rid,
                clear,
            } => {
                let Some(i) = index else {
                    return self.vanished(conn, id, key);
                };
                let applied = self.sess.managed[i].ctl.answer(&target, rid, clear);
                self.dirty.controllers = true;
                self.accepted(conn, id, &key);
                self.finish_journal(&key, Outcome::Done(Box::new(Done::Answered(applied))));
                self.waker.now();
            }
            Mutation::SetSessionPath { path, .. } => {
                let Some(i) = index else {
                    return self.vanished(conn, id, key);
                };
                self.accepted(conn, id, &key);
                let target = self.set_session_path(i, path);
                self.finish_journal(&key, Outcome::Done(Box::new(Done::Session(target))));
            }
            Mutation::ForkAt { file, entry_id } => {
                let running = self.ctl_holder(&file);
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "fork",
                    false,
                    false,
                    Post::Refresh,
                    move |ctx, _, _| jobs::fork_at(ctx, &file, &entry_id, running.as_deref()),
                );
            }
            Mutation::Copy { file, dest_cwd } => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "copy",
                    false,
                    false,
                    Post::Refresh,
                    move |ctx, _, _| jobs::copy_session(ctx, &file, &dest_cwd),
                );
            }
            Mutation::Move {
                file,
                dest_cwd,
                stop,
            } => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "move",
                    false,
                    false,
                    Post::Refresh,
                    move |ctx, _, _| jobs::move_session(ctx, &file, &dest_cwd, &stop),
                );
            }
            Mutation::Delete { targets, stop } => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "delete",
                    false,
                    false,
                    Post::Refresh,
                    move |ctx, _, _| jobs::delete_sessions(ctx, &targets, &stop),
                );
            }
            Mutation::ExportHtml { file, dest } => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "export-html",
                    false,
                    false,
                    Post::None,
                    move |ctx, _, _| jobs::export_html(ctx, &file, &dest),
                );
            }
            Mutation::Ctl { command, .. } => {
                let Some(i) = index else {
                    return self.vanished(conn, id, key);
                };
                self.track(&key, "ctl", Post::None);
                self.accepted(conn, id, &key);
                self.queue_ctl(i, key, command);
            }
            Mutation::Stage { root, files } => {
                self.accepted(conn, id, &key);
                let post = Post::Changes(root.clone());
                self.job(key, "git-stage", false, false, post, move |_, _, _| {
                    jobs::stage(&root, &files)
                });
            }
            Mutation::Unstage { root, files } => {
                self.accepted(conn, id, &key);
                let post = Post::Changes(root.clone());
                self.job(key, "git-unstage", false, false, post, move |_, _, _| {
                    jobs::unstage(&root, &files)
                });
            }
            Mutation::Discard { root, files } => {
                self.accepted(conn, id, &key);
                let post = Post::Changes(root.clone());
                self.job(key, "git-discard", false, false, post, move |_, _, _| {
                    jobs::discard(&root, &files)
                });
            }
            Mutation::DeleteUntracked { root, files } => {
                self.accepted(conn, id, &key);
                let post = Post::Changes(root.clone());
                self.job(
                    key,
                    "git-delete-untracked",
                    false,
                    false,
                    post,
                    move |_, _, _| jobs::delete_untracked(&root, &files),
                );
            }
            Mutation::WorktreeCreate {
                repo,
                branch,
                base_ref,
                dest,
            } => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "worktree-create",
                    false,
                    false,
                    Post::None,
                    move |ctx, _, _| jobs::worktree_create(ctx, &repo, &branch, &base_ref, &dest),
                );
            }
            Mutation::WorktreeRemove {
                worktree,
                base,
                how,
                stop,
                moved: _,
            } => {
                // Backend sessions are killed by the job; direct programs end here.
                let mut names = Vec::new();
                for t in &stop {
                    if let Some(i) = self.sess.find(&t.key) {
                        let m = &mut self.sess.managed[i];
                        match &m.mux_name {
                            Some(n) if m.mux.is_some() => names.push(n.clone()),
                            _ => m.ctl.terminal_mut().kill(),
                        }
                    }
                }
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "worktree-remove",
                    false,
                    false,
                    Post::Refresh,
                    move |ctx, _, _| jobs::worktree_remove(ctx, &worktree, &base, how, &names),
                );
            }
            Mutation::WorktreeAddExisting { repo, branch, dest } => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "worktree-add-existing",
                    false,
                    false,
                    Post::None,
                    move |ctx, _, _| jobs::worktree_add_existing(ctx, &repo, &branch, &dest),
                );
            }
            Mutation::SwitchBranch { dir, branch } => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "git-switch",
                    false,
                    false,
                    Post::None,
                    move |_, _, _| jobs::switch_branch(&dir, &branch),
                );
            }
            Mutation::RelocateSessions { files, dest } => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "relocate-sessions",
                    false,
                    false,
                    Post::Refresh,
                    move |ctx, _, _| jobs::relocate_sessions(ctx, &files, &dest),
                );
            }
            Mutation::OmpSet {
                key: setting,
                value,
            } => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "omp-set",
                    false,
                    false,
                    Post::None,
                    move |ctx, _, _| jobs::omp_set(ctx, &setting, &value),
                );
            }
            Mutation::OmpReset { key: setting } => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "omp-reset",
                    false,
                    false,
                    Post::None,
                    move |ctx, _, _| jobs::omp_reset(ctx, &setting),
                );
            }
            Mutation::AgentEdit {
                path,
                revision,
                edit,
            } => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "agent-edit",
                    false,
                    false,
                    Post::None,
                    move |ctx, _, _| jobs::agent_edit(ctx, &path, &revision, &edit),
                );
            }
            Mutation::AgentCreate { dir, agent } => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "agent-create",
                    false,
                    false,
                    Post::None,
                    move |ctx, _, _| jobs::agent_create(ctx, &dir, &agent),
                );
            }
            Mutation::AgentCustomize { dir, name } => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "agent-customize",
                    false,
                    false,
                    Post::None,
                    move |ctx, _, _| jobs::agent_customize(ctx, &dir, &name),
                );
            }
            Mutation::AgentDelete { path, revision } => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "agent-delete",
                    false,
                    false,
                    Post::None,
                    move |ctx, _, _| jobs::agent_delete(ctx, &path, &revision),
                );
            }
            Mutation::RunTool(tool) => {
                let (argv, label) = match &tool {
                    Tool::Update(kind) => (deck_core::omp::update_argv(*kind), "omp update"),
                    Tool::Install => (deck_core::omp::install_argv(), "Install omp"),
                };
                self.track(&key, "run-tool", Post::None);
                self.accepted(conn, id, &key);
                self.sess.queue(LaunchJob {
                    spec: LaunchSpec {
                        mux_name: None,
                        cwd: deck_core::platform::home_or_root(),
                        session_path: None,
                        label: label.to_owned(),
                        argv,
                        role: SessionRole::Tool,
                        copy_of: None,
                    },
                    key: Some(key),
                    exact: false,
                    restore: false,
                    tool: Some(tool),
                });
                self.pump_launches();
            }
            Mutation::Settings(edit) => {
                // Answered once committed (see `settings_finished`).
                self.track(&key, "settings", Post::None);
                let settings = self.settings.clone();
                self.spawn_worker("settings", move || Fin::Settings {
                    result: settings.submit(edit).wait(),
                    conn,
                    id,
                    key,
                });
            }
            Mutation::SetGatewayMode(mode) => {
                self.track(&key, "gateway-mode", Post::None);
                self.accepted(conn, id, &key);
                self.mode_queue.push_back((key, mode));
                self.pump_mode();
            }
            Mutation::DriveLoginBegin => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "drive-login",
                    false,
                    false,
                    Post::None,
                    move |ctx, _, _| jobs::drive_login_begin(ctx),
                );
            }
            Mutation::DriveLoginFinish(handle) => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "drive-login-finish",
                    true,
                    false,
                    Post::DriveAccount,
                    move |ctx, _, _| jobs::drive_login_finish(ctx, handle),
                );
            }
            Mutation::DriveLoginCancel(handle) => {
                let ok = self.handles.cancel_login(handle);
                self.accepted(conn, id, &key);
                self.inline_result(key, ok, "that sign-in is gone");
            }
            Mutation::DriveSignOut => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "drive-sign-out",
                    true,
                    false,
                    Post::DriveAccount,
                    move |ctx, _, _| jobs::drive_sign_out(ctx),
                );
            }
            Mutation::DriveBackup { force } => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "drive-backup",
                    true,
                    false,
                    Post::DriveAccount,
                    move |ctx, _, _| jobs::drive_backup(ctx, force),
                );
            }
            Mutation::DrivePrepareRestore(file) => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "drive-prepare-restore",
                    true,
                    false,
                    Post::None,
                    move |ctx, _, _| jobs::drive_prepare_restore(ctx, &file),
                );
            }
            Mutation::DriveApplyRestore(handle) => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "drive-restore",
                    true,
                    false,
                    Post::Refresh,
                    move |ctx, _, _| jobs::drive_apply_restore(ctx, handle),
                );
            }
            Mutation::DriveDeleteBackup(file) => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "drive-delete-backup",
                    true,
                    false,
                    Post::DriveAccount,
                    move |ctx, _, _| jobs::drive_delete_backup(ctx, &file),
                );
            }
            Mutation::DriveDiscardRestore(handle) => {
                let ok = self.handles.discard_restore(handle);
                self.accepted(conn, id, &key);
                self.inline_result(key, ok, "that restore is gone");
            }
            Mutation::ProjectBackup {
                project,
                parts,
                excluded,
                force,
            } => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "project-backup",
                    true,
                    true,
                    Post::None,
                    move |ctx, cancel, progress| {
                        jobs::project_backup(
                            ctx, &project, parts, &excluded, force, cancel, progress,
                        )
                    },
                );
            }
            Mutation::ProjectDeleteBackup(backup) => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "project-delete-backup",
                    true,
                    false,
                    Post::None,
                    move |ctx, _, _| jobs::project_delete_backup(ctx, &backup),
                );
            }
            Mutation::ProjectPrepareRestore {
                backup,
                target,
                parts,
            } => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "project-prepare-restore",
                    true,
                    true,
                    Post::None,
                    move |ctx, cancel, progress| {
                        jobs::project_prepare_restore(
                            ctx, &backup, &target, parts, cancel, progress,
                        )
                    },
                );
            }
            Mutation::ProjectApplyRestore {
                handle,
                conflicts,
                stop,
            } => {
                self.accepted(conn, id, &key);
                self.job(
                    key,
                    "project-restore",
                    true,
                    true,
                    Post::Refresh,
                    move |ctx, cancel, progress| {
                        jobs::project_apply_restore(
                            ctx, handle, &conflicts, &stop, cancel, progress,
                        )
                    },
                );
            }
            Mutation::ProjectDiscardRestore(handle) => {
                let ok = self.handles.discard_project_restore(handle);
                self.accepted(conn, id, &key);
                self.inline_result(key, ok, "that restore is gone");
            }
            Mutation::AppUpdateCheck => {
                self.sched.begin_app_check();
                self.dirty.app = true;
                self.accepted(conn, id, &key);
                let tx = self.tx.clone();
                self.job(
                    key,
                    "app-update-check",
                    false,
                    false,
                    Post::AppView,
                    move |_, _, _| {
                        let r = deck_core::selfupdate::latest().map_err(|e| e.to_string());
                        let out = r
                            .as_ref()
                            .map(|_| Done::Unit)
                            .map_err(|e| GatewayError::Failed(e.clone()));
                        send(&tx, Sched::AppChecked(r).into());
                        out
                    },
                );
            }
            Mutation::AppUpdateDownload => {
                let Some((release, asset)) = self.sched.download_plan() else {
                    // Validated before admission; nothing ran in between.
                    return self.vanished(conn, id, key);
                };
                self.sched.begin_download();
                self.dirty.app = true;
                self.accepted(conn, id, &key);
                let tx = self.tx.clone();
                self.job(
                    key,
                    "app-update-download",
                    false,
                    true,
                    Post::AppView,
                    move |_, cancel, progress| {
                        let r = download(&release, &asset, cancel, progress);
                        let out = r
                            .as_ref()
                            .map(|_| Done::Unit)
                            .map_err(|e| GatewayError::Failed(e.clone()));
                        send(&tx, Sched::AppDownloaded(r).into());
                        out
                    },
                );
            }
            Mutation::AppUpdateApply => {
                let Some((kind, file, dir)) = self.sched.apply_plan() else {
                    return self.vanished(conn, id, key);
                };
                self.accepted(conn, id, &key);
                let tx = self.tx.clone();
                self.job(
                    key,
                    "app-update-apply",
                    false,
                    false,
                    Post::AppView,
                    move |_, _, _| {
                        let r = deck_core::selfupdate::apply(&kind, &file);
                        // Installed: the download is spent. A failure (a manual command names the
                        // file) keeps it.
                        if r.is_ok() {
                            let _ = std::fs::remove_dir_all(&dir);
                        }
                        let out = r
                            .as_ref()
                            .map(|_| Done::Unit)
                            .map_err(|e| GatewayError::Failed(e.to_string()));
                        let phase = r.map_err(|e| match e {
                            deck_core::selfupdate::UpdateError::NeedsManual { command } => {
                                UpdatePhase::NeedsManual { command }
                            }
                            e => UpdatePhase::Failed(e.to_string()),
                        });
                        send(&tx, Sched::AppApplied(phase).into());
                        out
                    },
                );
            }
            Mutation::InboxOpen(t) => {
                self.accepted(conn, id, &key);
                let effects = self.notify.center.open(&t);
                self.apply_effects(effects);
                self.finish_journal(&key, Outcome::Done(Box::new(Done::Unit)));
            }
            Mutation::InboxDismiss(t) => {
                self.accepted(conn, id, &key);
                let effects = self.notify.center.dismiss(&t);
                self.apply_effects(effects);
                self.finish_journal(&key, Outcome::Done(Box::new(Done::Unit)));
            }
            Mutation::InboxClear => {
                self.accepted(conn, id, &key);
                let effects = self.notify.center.clear_all();
                self.apply_effects(effects);
                self.finish_journal(&key, Outcome::Done(Box::new(Done::Unit)));
            }
            Mutation::InboxTest => {
                self.accepted(conn, id, &key);
                self.notify.notifier.show(desktop::Request {
                    tab: None,
                    title: "OmpDeck".to_owned(),
                    body: "Desktop notifications are working.".to_owned(),
                    sound: self.cfg.notify.sound.then_some(desktop::Sound::Test),
                });
                self.finish_journal(&key, Outcome::Done(Box::new(Done::Unit)));
            }
            Mutation::CancelJob(target) => {
                let applied = self
                    .jobs
                    .get(&target)
                    .and_then(|j| j.cancel.as_ref())
                    .map(|c| c.store(true, Ordering::Relaxed))
                    .is_some();
                self.accepted(conn, id, &key);
                self.inline_result(key, applied, "that job already ended");
            }
            Mutation::UsageRefresh { force } => {
                self.accepted(conn, id, &key);
                if self.sched.usage_fetching() {
                    // The running fetch is the refresh.
                    self.finish_journal(&key, Outcome::Done(Box::new(Done::Unit)));
                    return;
                }
                self.sched.begin_usage_fetch(Instant::now());
                self.dirty.usage = true;
                let tx = self.tx.clone();
                self.job(
                    key,
                    "usage-refresh",
                    false,
                    false,
                    Post::None,
                    move |ctx, _, _| {
                        let r = fetch_usage(ctx, force);
                        let out = r
                            .as_ref()
                            .map(|_| Done::Unit)
                            .map_err(|e| GatewayError::Failed(e.clone()));
                        send(&tx, Sched::Usage(r).into());
                        out
                    },
                );
            }
            Mutation::OmpUpdateCheck => {
                self.sched.begin_omp_check();
                self.dirty.omp = true;
                self.accepted(conn, id, &key);
                let tx = self.tx.clone();
                self.job(
                    key,
                    "omp-update-check",
                    false,
                    false,
                    Post::None,
                    move |ctx, _, _| {
                        let r = match &ctx.omp {
                            Some(o) => o.check_update().map_err(|e| e.to_string()),
                            None => Err("omp was not found".to_owned()),
                        };
                        let out = r
                            .as_ref()
                            .map(|_| Done::Unit)
                            .map_err(|e| GatewayError::Failed(e.clone()));
                        send(&tx, Sched::OmpChecked(r).into());
                        out
                    },
                );
            }
            Mutation::RefreshSessions => {
                self.sessions_watch.refresh_now();
                self.running.poke();
                self.accepted(conn, id, &key);
                self.finish_journal(&key, Outcome::Done(Box::new(Done::Unit)));
            }
        }
    }

    /// Finishes an inline mutation that either applied or found nothing to apply.
    fn inline_result(&mut self, key: RequestKey, applied: bool, gone: &str) {
        let outcome = if applied {
            Outcome::Done(Box::new(Done::Unit))
        } else {
            Outcome::Failed(gone.to_owned())
        };
        self.finish_journal(&key, outcome);
    }

    /// The admitted mutation's subject disappeared between validation and execution (cannot
    /// happen on this thread; kept total so no admitted key stays Running).
    fn vanished(&mut self, conn: u64, id: u64, key: RequestKey) {
        let e = GatewayError::Refused("that session is closed".to_owned());
        self.journal.finish(&key, Outcome::Failed(e.to_string()));
        self.reply(conn, id, Err(e));
    }

    /// The live-control key of a running session holding `file`, for a fork through its
    /// socket.
    fn ctl_holder(&self, file: &std::path::Path) -> Option<String> {
        if let Some(m) = self
            .sess
            .managed
            .iter()
            .find(|m| !m.exited() && m.ctl.session_path() == Some(file) && m.ctl_key.is_some())
        {
            return m.ctl_key.clone();
        }
        let snap = self.running.latest()?;
        let holder = snap.holder(file)?;
        snap.has_ctl(holder).then(|| holder.to_owned())
    }
}

/// Worker: `omp usage` (after invalidating its cache when forced).
pub(super) fn fetch_usage(
    ctx: &JobCtx,
    force: bool,
) -> Result<deck_core::usage::UsageSnapshot, String> {
    let omp = ctx
        .omp
        .as_ref()
        .ok_or_else(|| "omp was not found".to_owned())?;
    deck_core::usage::fetch_usage(omp, force, deck_core::usage::DEFAULT_FETCH_TIMEOUT)
        .map_err(|e| e.to_string())
}

/// Worker: downloads and verifies the release asset into a private temporary directory.
fn download(
    release: &deck_core::selfupdate::Release,
    asset: &str,
    cancel: &AtomicBool,
    progress: &(dyn Fn(Progress) + Sync),
) -> Result<(PathBuf, PathBuf), String> {
    let dir = std::env::temp_dir().join(format!(
        "omp-deck-update-{}-{}",
        std::process::id(),
        crate::gateway::unix_ms()
    ));
    crate::gateway::private_dir(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let result = deck_core::selfupdate::download(
        release,
        asset,
        &dir,
        &|done, total| {
            progress(Progress {
                stage: "download".to_owned(),
                done,
                total,
            });
        },
        cancel,
    );
    match result {
        Ok(file) => Ok((dir, file)),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(e.to_string())
        }
    }
}
