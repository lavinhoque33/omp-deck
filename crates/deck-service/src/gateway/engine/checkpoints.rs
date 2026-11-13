//! Interest-driven checkpoint reads and journaled, identity-fenced rollback workers.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deck_core::checkpoints::{self as core, Checkpoint, CheckpointKind};
use serde::Deserialize;

use super::mutate::Post;
use super::{Engine, Fin};
use crate::controller::{RequestKey, SessionKey, SessionTarget};
use crate::gateway::protocol::{CheckpointsView, Done, GatewayError, Topic};
use crate::statusfmt::Activity;

const REFRESH: Duration = Duration::from_secs(5);
const HEADER_LIMIT: u64 = 16 * 1024;
const DIFF_LIMIT: usize = 500;

#[derive(Default)]
pub(super) struct State {
    watches: HashMap<PathBuf, Watch>,
    next_revision: u64,
    /// Reserve before preparation, so a command cannot start while identities are resolved.
    restoring: HashMap<SessionKey, RequestKey>,
    /// Independent sessions in the same repository must not restore concurrently.
    roots: HashMap<PathBuf, RequestKey>,
}

struct Watch {
    revision: u64,
    value: CheckpointsView,
    busy: bool,
    force: bool,
    next: Option<Instant>,
    turn_running: bool,
}

pub(in crate::gateway) enum Update {
    Listed {
        session: PathBuf,
        revision: u64,
        value: CheckpointsView,
    },
    Prepared {
        key: RequestKey,
        target: SessionTarget,
        target_file: PathBuf,
        target_cwd: PathBuf,
        result: Box<Result<RestorePlan, GatewayError>>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    file: PathBuf,
    session_id: String,
    cwd: PathBuf,
    repo: Option<PathBuf>,
}

#[derive(Deserialize)]
struct Header {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    cwd: Option<PathBuf>,
}

#[derive(Debug)]
pub(in crate::gateway) struct RestorePlan {
    session: PathBuf,
    target_file: PathBuf,
    target_cwd: PathBuf,
    identity: Identity,
    checkpoint: Checkpoint,
}

fn failed(error: impl std::fmt::Display) -> GatewayError {
    GatewayError::Failed(error.to_string())
}

fn refused(reason: &str) -> GatewayError {
    GatewayError::Refused(reason.to_owned())
}

/// Only the bounded session header is authoritative; filenames and directory names are not IDs.
fn identity(session: &Path) -> Result<Identity, GatewayError> {
    if !session.is_absolute() {
        return Err(refused("a session file path must be absolute"));
    }
    let file = std::fs::canonicalize(session).map_err(failed)?;
    let mut bytes = Vec::new();
    File::open(&file)
        .map_err(failed)?
        .take(HEADER_LIMIT)
        .read_to_end(&mut bytes)
        .map_err(failed)?;
    for line in bytes.split(|byte| *byte == b'\n') {
        let Ok(header) = serde_json::from_slice::<Header>(line) else {
            continue;
        };
        if header.kind != "session" {
            continue;
        }
        let session_id = header
            .id
            .filter(|id| !id.is_empty())
            .ok_or_else(|| refused("the session header has no session id"))?;
        let cwd = header
            .cwd
            .filter(|cwd| cwd.is_absolute())
            .ok_or_else(|| refused("the session header has no absolute working directory"))?;
        return Ok(Identity {
            file,
            session_id,
            repo: core::repo_root(&cwd),
            cwd,
        });
    }
    Err(refused("no session header in the first 16 KiB"))
}

fn listing(session: &Path) -> CheckpointsView {
    let mut value = CheckpointsView::default();
    match identity(session) {
        Err(error) => value.error = Some(error.to_string()),
        Ok(identity) => {
            value.session_id = Some(identity.session_id.clone());
            value.repo = identity.repo.clone();
            if let Some(repo) = identity.repo {
                match core::list(&repo, &identity.session_id) {
                    Ok(items) => value.items = items,
                    Err(error) => value.error = Some(error.to_string()),
                }
            }
        }
    }
    value
}

fn selected(identity: &Identity, seq: u32) -> Result<Checkpoint, GatewayError> {
    let repo = identity
        .repo
        .as_deref()
        .ok_or_else(|| refused("this session is not in a git repository"))?;
    core::list(repo, &identity.session_id)
        .map_err(failed)?
        .into_iter()
        .find(|checkpoint| checkpoint.seq == seq)
        .ok_or_else(|| refused("that checkpoint no longer exists for this session"))
}

/// Worker query: resolve a sequence only inside the session header's repository and namespace.
pub(in crate::gateway) fn diff(
    session: &Path,
    seq: u32,
) -> Result<(Vec<core::FileDelta>, u32), GatewayError> {
    let identity = identity(session)?;
    let checkpoint = selected(&identity, seq)?;
    let repo = identity
        .repo
        .as_deref()
        .ok_or_else(|| refused("this session is not in a git repository"))?;
    let mut rows = core::diff_stat(repo, &checkpoint.commit).map_err(failed)?;
    let total = u32::try_from(rows.len()).unwrap_or(u32::MAX);
    rows.truncate(DIFF_LIMIT);
    Ok((rows, total))
}

fn prepare(
    session: &Path,
    target_file: &Path,
    target_cwd: &Path,
    seq: u32,
) -> Result<RestorePlan, GatewayError> {
    let identity = identity(session)?;
    if std::fs::canonicalize(target_file).map_err(failed)? != identity.file {
        return Err(refused(
            "the checkpoint session does not match the target session",
        ));
    }
    let repo = identity
        .repo
        .as_ref()
        .ok_or_else(|| refused("this session is not in a git repository"))?;
    if core::repo_root(target_cwd).as_ref() != Some(repo) {
        return Err(refused(
            "the target session is no longer in the checkpoint repository",
        ));
    }
    let checkpoint = selected(&identity, seq)?;
    Ok(RestorePlan {
        session: session.to_path_buf(),
        target_file: target_file.to_path_buf(),
        target_cwd: target_cwd.to_path_buf(),
        identity,
        checkpoint,
    })
}

/// Re-resolve identity immediately before any capture or destructive operation. Never restore
/// by a caller-supplied revision or a cached list item from a previous session generation.
fn restore(plan: &RestorePlan) -> Result<Done, GatewayError> {
    if identity(&plan.session)? != plan.identity
        || std::fs::canonicalize(&plan.target_file).map_err(failed)? != plan.identity.file
        || core::repo_root(&plan.target_cwd) != plan.identity.repo
    {
        return Err(refused(
            "the checkpoint session or repository changed before rollback",
        ));
    }
    let selected = selected(&plan.identity, plan.checkpoint.seq)?;
    if selected.commit != plan.checkpoint.commit {
        return Err(refused("the checkpoint changed before rollback"));
    }
    let repo = plan
        .identity
        .repo
        .as_deref()
        .ok_or_else(|| refused("this session is not in a git repository"))?;
    let safety = core::create(
        repo,
        &plan.identity.session_id,
        CheckpointKind::Rollback,
        &format!("Before rollback to checkpoint {}", plan.checkpoint.seq),
    )
    .map_err(|error| {
        failed(format!(
            "Could not create rollback safety checkpoint: {error}"
        ))
    })?
    .map(|checkpoint| checkpoint.seq);
    let report = core::restore(repo, &plan.checkpoint.commit).map_err(|error| {
        let safety = safety.map_or_else(
            || "The current tree already has a checkpoint".to_owned(),
            |seq| format!("Safety checkpoint {seq} was saved"),
        );
        failed(format!("{safety}; rollback failed: {error}"))
    })?;
    Ok(Done::CheckpointRestored {
        safety,
        changed: report.changed,
    })
}

fn idle_refusal(activity: Activity, pending: bool, exited: bool) -> Option<GatewayError> {
    if exited {
        Some(refused("the checkpoint session has exited"))
    } else if activity != Activity::Idle || pending {
        Some(refused(
            "rollback requires an idle session with no running turn or pending command",
        ))
    } else {
        None
    }
}

impl Engine {
    pub(super) fn step_checkpoints(&mut self, wanted: &BTreeSet<Topic>, now: Instant) {
        self.checkpoints
            .watches
            .retain(|session, _| wanted.contains(&Topic::Checkpoints(session.clone())));
        let mut due = Vec::new();
        for topic in wanted {
            let Topic::Checkpoints(session) = topic else {
                continue;
            };
            let running = self.sess.managed.iter().any(|managed| {
                managed.ctl.session_path() == Some(session.as_path())
                    && managed.ctl.activity().is_busy()
            });
            let next_revision = &mut self.checkpoints.next_revision;
            let watch = self
                .checkpoints
                .watches
                .entry(session.clone())
                .or_insert_with(|| {
                    *next_revision = next_revision.wrapping_add(1);
                    Watch {
                        revision: *next_revision,
                        value: CheckpointsView::default(),
                        busy: false,
                        force: true,
                        next: None,
                        turn_running: running,
                    }
                });
            if watch.turn_running && !running {
                watch.force = true;
            }
            watch.turn_running = running;
            if !watch.busy && (watch.force || watch.next.is_none_or(|next| now >= next)) {
                watch.busy = true;
                watch.force = false;
                due.push((session.clone(), watch.revision));
            }
        }
        for (session, revision) in due {
            self.spawn_worker("checkpoints", move || {
                let value = listing(&session);
                Fin::Checkpoints(Update::Listed {
                    session,
                    revision,
                    value,
                })
            });
        }
    }

    pub(super) fn checkpoints_value(&self, session: &Path) -> CheckpointsView {
        self.checkpoints
            .watches
            .get(session)
            .map(|watch| watch.value.clone())
            .unwrap_or_default()
    }

    pub(super) fn refresh_checkpoints(&mut self, session: &Path) {
        if let Some(watch) = self.checkpoints.watches.get_mut(session) {
            watch.force = true;
            self.waker.now();
        }
    }

    pub(super) fn checkpoint_restore_busy(&self, target: &SessionTarget) -> bool {
        self.checkpoints.restoring.contains_key(&target.key)
    }

    pub(super) fn checkpoint_restore_refusal(
        &self,
        target: &SessionTarget,
        session: &Path,
    ) -> Option<GatewayError> {
        if let Err(error) = self.check_target(target) {
            return Some(error);
        }
        if !session.is_absolute() {
            return Some(refused("a session file path must be absolute"));
        }
        if self.checkpoint_restore_busy(target) {
            return Some(refused(
                "a checkpoint rollback is already running for this session",
            ));
        }
        let Some(index) = self.sess.find(&target.key) else {
            return Some(refused("the checkpoint session is closed"));
        };
        let managed = &self.sess.managed[index];
        if managed.ctl.session_path().is_none() {
            return Some(refused("the target has no persisted session file"));
        }
        idle_refusal(
            managed.ctl.activity(),
            managed.checkpoint_pending_command()
                || self.checkpoint_deferred_pending(target)
                || self.checkpoint_delivery_pending(target),
            managed.exited(),
        )
    }

    pub(super) fn begin_checkpoint_restore(
        &mut self,
        key: RequestKey,
        target: SessionTarget,
        session: PathBuf,
        seq: u32,
    ) {
        let Some(index) = self.sess.find(&target.key) else {
            self.job_failed(key, refused("the checkpoint session is closed"));
            return;
        };
        let managed = &self.sess.managed[index];
        let Some(target_file) = managed.ctl.session_path().map(Path::to_path_buf) else {
            self.job_failed(key, refused("the target has no persisted session file"));
            return;
        };
        let target_cwd = managed.current_cwd().to_path_buf();
        self.checkpoints
            .restoring
            .insert(target.key.clone(), key.clone());
        self.dirty.controllers = true;
        self.track(
            &key,
            "checkpoint-restore",
            Post::CheckpointRestore(session.clone()),
        );
        self.spawn_worker("checkpoint-prepare", move || {
            let result = Box::new(prepare(&session, &target_file, &target_cwd, seq));
            Fin::Checkpoints(Update::Prepared {
                key,
                target,
                target_file,
                target_cwd,
                result,
            })
        });
    }

    pub(super) fn finish_checkpoint_restore(&mut self, key: &RequestKey, session: &Path) {
        self.checkpoints.restoring.retain(|_, owner| owner != key);
        self.dirty.controllers = true;
        let mut roots = Vec::new();
        self.checkpoints.roots.retain(|root, owner| {
            if owner != key {
                return true;
            }
            roots.push(root.clone());
            false
        });
        for root in roots {
            self.refresh_changes(&root);
            let sessions: Vec<PathBuf> = self
                .checkpoints
                .watches
                .iter()
                .filter(|(_, watch)| watch.value.repo.as_ref() == Some(&root))
                .map(|(session, _)| session.clone())
                .collect();
            for session in sessions {
                self.refresh_checkpoints(&session);
            }
        }
        self.refresh_checkpoints(session);
    }

    pub(super) fn on_checkpoint_update(&mut self, update: Update) {
        match update {
            Update::Listed {
                session,
                revision,
                value,
            } => {
                let Some(watch) = self.checkpoints.watches.get_mut(&session) else {
                    return;
                };
                if watch.revision != revision {
                    return;
                }
                watch.busy = false;
                watch.next = Some(Instant::now() + REFRESH);
                self.waker.after(REFRESH);
                if watch.value != value {
                    watch.value = value;
                    self.changed(Topic::Checkpoints(session));
                }
            }
            Update::Prepared {
                key,
                target,
                target_file,
                target_cwd,
                result,
            } => {
                if self.checkpoints.restoring.get(&target.key) != Some(&key)
                    || !self.jobs.contains_key(&key)
                {
                    return;
                }
                let plan = match *result {
                    Ok(plan) => plan,
                    Err(error) => return self.job_failed(key, error),
                };
                if let Err(error) = self.check_target(&target) {
                    return self.job_failed(key, error);
                }
                let Some(index) = self.sess.find(&target.key) else {
                    return self.job_failed(key, refused("the checkpoint session is closed"));
                };
                let managed = &self.sess.managed[index];
                if managed.ctl.session_path() != Some(target_file.as_path())
                    || managed.current_cwd() != target_cwd
                {
                    return self
                        .job_failed(key, refused("the target session changed before rollback"));
                }
                if let Some(error) = idle_refusal(
                    managed.ctl.activity(),
                    managed.checkpoint_pending_command()
                        || self.checkpoint_deferred_pending(&target)
                        || self.checkpoint_delivery_awaiting_ack(&target),
                    managed.exited(),
                ) {
                    return self.job_failed(key, error);
                }
                let Some(repo) = plan.identity.repo.clone() else {
                    return self
                        .job_failed(key, refused("this session is not in a git repository"));
                };
                if self.checkpoints.roots.contains_key(&repo) {
                    return self.job_failed(
                        key,
                        refused("another checkpoint rollback is running in this repository"),
                    );
                }
                self.checkpoints.roots.insert(repo, key.clone());
                self.spawn_worker("checkpoint-restore", move || Fin::Mutation {
                    key,
                    result: restore(&plan).map(Box::new),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("git runs in temporary repository");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn header(file: &Path, cwd: &Path, id: &str) {
        fs::write(
            file,
            format!(
                "{}\n{}\n",
                serde_json::json!({"type": "title", "title": "Checkpoint fixture"}),
                serde_json::json!({"type": "session", "id": id, "cwd": cwd}),
            ),
        )
        .expect("temporary session header");
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().expect("temporary fixture");
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).expect("temporary repo");
        git(&repo, &["init", "-q"]);
        fs::write(repo.join("tracked"), "before\n").expect("tracked file");
        git(&repo, &["add", "--", "tracked"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Checkpoint test",
                "-c",
                "user.email=checkpoint-test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "initial",
            ],
        );
        let repo = fs::canonicalize(repo).expect("canonical repository");
        // Session storage stays outside the snapshotted project and never touches ~/.omp.
        let session = temp.path().join("filename-is-not-the-session-id.jsonl");
        header(&session, &repo, "header-session");
        (temp, repo, session)
    }

    fn capture(repo: &Path, session_id: &str) -> Checkpoint {
        core::create(repo, session_id, CheckpointKind::Turn, "fixture turn")
            .expect("capture")
            .expect("first checkpoint")
    }

    #[test]
    fn restore_requires_idle_no_turn_no_pending_command_and_a_live_process() {
        assert!(idle_refusal(Activity::Idle, false, false).is_none());
        for activity in [Activity::Busy, Activity::Attention, Activity::Unknown] {
            assert!(matches!(
                idle_refusal(activity, false, false),
                Some(GatewayError::Refused(_))
            ));
        }
        assert!(idle_refusal(Activity::Idle, true, false).is_some());
        assert!(idle_refusal(Activity::Idle, false, true).is_some());
    }

    #[test]
    fn listing_uses_bounded_header_identity_and_reports_errors() {
        let (temp, repo, session) = fixture();
        let expected = capture(&repo, "header-session");
        // Neither the filename nor the project's directory name supplies the ref namespace.
        capture(&repo, "filename-is-not-the-session-id");
        let view = listing(&session);
        assert_eq!(view.repo, Some(repo));
        assert_eq!(view.session_id.as_deref(), Some("header-session"));
        assert_eq!(view.items, vec![expected]);
        assert_eq!(view.error, None);

        let outside = temp.path().join("not-a-repository");
        fs::create_dir(&outside).expect("non-git cwd");
        header(&session, &outside, "outside-session");
        let view = listing(&session);
        assert_eq!(view.repo, None);
        assert_eq!(view.session_id.as_deref(), Some("outside-session"));
        assert!(view.items.is_empty() && view.error.is_none());

        let padded = format!(
            "{}\n{}\n",
            serde_json::json!({"type": "title", "title": "x".repeat(HEADER_LIMIT as usize)}),
            serde_json::json!({"type": "session", "id": "too-late", "cwd": outside}),
        );
        fs::write(&session, padded).expect("oversized header fixture");
        assert!(listing(&session).error.is_some());
        assert!(listing(&temp.path().join("missing.jsonl")).error.is_some());
        assert!(identity(Path::new("relative.jsonl")).is_err());
    }

    #[test]
    fn invalid_session_namespace_is_reported_instead_of_using_the_filename() {
        let (_temp, repo, session) = fixture();
        capture(&repo, "header-session");
        header(&session, &repo, "../header-session");
        let view = listing(&session);
        assert!(view.items.is_empty());
        assert!(view.error.is_some());
        assert!(diff(&session, 1).is_err());
    }

    #[test]
    fn linked_worktree_header_keeps_diff_and_rollback_out_of_the_primary_checkout() {
        let (temp, primary, session) = fixture();
        let linked = temp.path().join("linked");
        git(
            &primary,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "checkpoint-linked",
                linked.to_str().expect("temporary path"),
            ],
        );
        let linked = fs::canonicalize(linked).expect("canonical linked worktree");
        header(&session, &linked, "header-session");
        let checkpoint = capture(&linked, "header-session");
        fs::write(linked.join("tracked"), "linked changes\n").expect("linked work");
        assert_eq!(listing(&session).repo, Some(linked.clone()));
        let (rows, total) = diff(&session, checkpoint.seq).expect("linked diff");
        assert_eq!(total, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, "tracked");
        assert!(prepare(&session, &session, &primary, checkpoint.seq).is_err());
        let plan = prepare(&session, &session, &linked, checkpoint.seq).expect("linked plan");
        restore(&plan).expect("linked rollback");
        assert_eq!(
            fs::read_to_string(linked.join("tracked")).expect("linked file"),
            "before\n"
        );
        assert_eq!(
            fs::read_to_string(primary.join("tracked")).expect("primary untouched"),
            "before\n"
        );
    }

    #[test]
    fn preparation_fences_target_file_repository_and_sequence_without_capture() {
        let (temp, repo, session) = fixture();
        let checkpoint = capture(&repo, "header-session");
        let other_file = temp.path().join("different-session.jsonl");
        header(&other_file, &repo, "header-session");
        assert!(matches!(
            prepare(&session, &other_file, &repo, checkpoint.seq),
            Err(GatewayError::Refused(_))
        ));
        let other_repo = temp.path().join("other-repo");
        fs::create_dir(&other_repo).expect("other repo directory");
        git(&other_repo, &["init", "-q"]);
        assert!(prepare(&session, &session, &other_repo, checkpoint.seq).is_err());
        assert!(prepare(&session, &session, &repo, checkpoint.seq + 1).is_err());
        assert_eq!(
            core::list(&repo, "header-session").expect("list"),
            vec![checkpoint]
        );
    }

    #[test]
    fn rollback_creates_a_real_safety_copy_first_and_preserves_the_real_index() {
        let (_temp, repo, session) = fixture();
        let checkpoint = capture(&repo, "header-session");
        fs::write(repo.join("tracked"), "staged content\n").expect("staged change");
        git(&repo, &["add", "--", "tracked"]);
        fs::write(repo.join("tracked"), "working content\n").expect("unstaged change");
        fs::write(repo.join("untracked"), "keep in safety\n").expect("untracked change");
        let index_before = fs::read(repo.join(".git/index")).expect("index before restore");
        let plan = prepare(&session, &session, &repo, checkpoint.seq).expect("restore plan");
        let Done::CheckpointRestored { safety, changed } = restore(&plan).expect("rollback") else {
            panic!("wrong checkpoint result");
        };
        assert!(changed > 0);
        assert_eq!(
            fs::read_to_string(repo.join("tracked")).expect("restored file"),
            "before\n"
        );
        assert!(!repo.join("untracked").exists());
        assert_eq!(
            fs::read(repo.join(".git/index")).expect("index after restore"),
            index_before
        );
        let items = core::list(&repo, "header-session").expect("checkpoints");
        assert_eq!(safety, Some(items[0].seq));
        assert_eq!(items[0].kind, CheckpointKind::Rollback);
        assert!(items[0].seq > checkpoint.seq);
        // The returned safety checkpoint really contains the state that was just replaced.
        let safety_plan = prepare(&session, &session, &repo, items[0].seq).expect("safety plan");
        restore(&safety_plan).expect("restore safety");
        assert_eq!(
            fs::read_to_string(repo.join("tracked")).expect("recovered work"),
            "working content\n"
        );
        assert_eq!(
            fs::read_to_string(repo.join("untracked")).expect("recovered untracked"),
            "keep in safety\n"
        );
    }

    #[test]
    fn an_unchanged_tree_can_restore_without_a_duplicate_safety_ref() {
        let (_temp, repo, session) = fixture();
        let checkpoint = capture(&repo, "header-session");
        let plan = prepare(&session, &session, &repo, checkpoint.seq).expect("plan");
        let Done::CheckpointRestored { safety, changed } =
            restore(&plan).expect("unchanged rollback")
        else {
            panic!("wrong result");
        };
        assert_eq!(safety, None);
        assert_eq!(changed, 0);
        assert_eq!(
            core::list(&repo, "header-session").expect("list"),
            vec![checkpoint]
        );
    }

    #[test]
    fn rollback_revalidates_header_and_fails_closed_when_safety_capture_fails() {
        let (_temp, repo, session) = fixture();
        let checkpoint = capture(&repo, "header-session");
        fs::write(repo.join("tracked"), "must survive\n").expect("current work");
        let plan = prepare(&session, &session, &repo, checkpoint.seq).expect("plan");
        header(&session, &repo, "replaced-session");
        assert!(matches!(restore(&plan), Err(GatewayError::Refused(_))));
        assert_eq!(
            fs::read_to_string(repo.join("tracked")).expect("untouched"),
            "must survive\n"
        );
        header(&session, &repo, "header-session");
        fs::write(repo.join(".git/index"), b"invalid-index").expect("broken fixture index");
        let error = restore(&plan).expect_err("safety capture must fail");
        assert!(
            error
                .to_string()
                .contains("Could not create rollback safety checkpoint")
        );
        assert_eq!(
            fs::read_to_string(repo.join("tracked")).expect("untouched"),
            "must survive\n"
        );
        assert_eq!(
            core::list(&repo, "header-session").expect("list"),
            vec![checkpoint]
        );
    }

    #[test]
    fn restore_failure_surfaces_the_saved_safety_checkpoint() {
        let (_temp, repo, session) = fixture();
        fs::write(repo.join("blocked"), "checkpoint file\n").expect("checkpoint content");
        let checkpoint = capture(&repo, "header-session");
        fs::remove_file(repo.join("blocked")).expect("remove checkpoint-only file");
        fs::write(repo.join(".gitignore"), "/blocked\n").expect("ignore collision");
        fs::write(repo.join("blocked"), "ignored content\n").expect("ignored content");
        let plan = prepare(&session, &session, &repo, checkpoint.seq).expect("plan");
        let error = restore(&plan).expect_err("ignored collision refuses restore");
        let items = core::list(&repo, "header-session").expect("list");
        assert_eq!(items[0].kind, CheckpointKind::Rollback);
        assert!(
            error
                .to_string()
                .contains(&format!("Safety checkpoint {}", items[0].seq))
        );
        assert!(error.to_string().contains("rollback failed"));
        assert_eq!(
            fs::read_to_string(repo.join("blocked")).expect("ignored untouched"),
            "ignored content\n"
        );
    }

    #[test]
    fn diff_reads_real_untracked_changes_and_caps_the_worker_answer() {
        let (_temp, repo, session) = fixture();
        let checkpoint = capture(&repo, "header-session");
        for n in 0..=DIFF_LIMIT {
            fs::write(repo.join(format!("new-{n:04}")), "new content\n").expect("untracked file");
        }
        let (rows, total) = diff(&session, checkpoint.seq).expect("checkpoint diff");
        assert_eq!(total as usize, DIFF_LIMIT + 1);
        assert_eq!(rows.len(), DIFF_LIMIT);
        assert!(
            rows.iter()
                .all(|row| row.path.starts_with("new-") && row.status == 'A')
        );
        assert!(diff(&session, checkpoint.seq + 1).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn retargeting_the_live_working_directory_refuses_before_safety_capture() {
        use std::os::unix::fs::symlink;
        let (temp, repo, session) = fixture();
        let checkpoint = capture(&repo, "header-session");
        let alias = temp.path().join("live-cwd");
        symlink(&repo, &alias).expect("cwd alias");
        let plan = prepare(&session, &session, &alias, checkpoint.seq).expect("same repository");
        let other = temp.path().join("other-repo");
        fs::create_dir(&other).expect("other directory");
        git(&other, &["init", "-q"]);
        fs::remove_file(&alias).expect("replace cwd alias");
        symlink(&other, &alias).expect("redirect cwd");
        assert!(matches!(restore(&plan), Err(GatewayError::Refused(_))));
        assert_eq!(
            core::list(&repo, "header-session").expect("no safety side effect"),
            vec![checkpoint]
        );
    }

    #[cfg(unix)]
    #[test]
    fn equivalent_session_paths_are_allowed_but_retargeted_aliases_are_not() {
        use std::os::unix::fs::symlink;
        let (temp, repo, session) = fixture();
        let checkpoint = capture(&repo, "header-session");
        let alias = temp.path().join("session-alias.jsonl");
        symlink(&session, &alias).expect("session alias");
        let plan = prepare(&alias, &session, &repo, checkpoint.seq).expect("same canonical file");
        let replacement = temp.path().join("replacement.jsonl");
        header(&replacement, &repo, "header-session");
        fs::remove_file(&alias).expect("replace alias");
        symlink(&replacement, &alias).expect("redirect alias");
        assert!(matches!(restore(&plan), Err(GatewayError::Refused(_))));
        assert_eq!(
            core::list(&repo, "header-session").expect("no safety side effect"),
            vec![checkpoint]
        );
    }
}
