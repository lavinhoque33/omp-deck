//! Parent-owned finish jobs and the narrowly sanctioned retire `/move` root change.
use super::*;
use crate::gateway::protocol::{Done, GatewayError, Mutation};
use deck_core::ctl::CtlCommand;
use deck_core::worktree;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};

#[derive(Deserialize)]
struct FinishArgs {
    session: String,
}

pub(super) struct ChildFinish {
    handle: String,
    request: Option<u64>,
    title: String,
    started: bool,
    finished: bool,
    blocked: Option<String>,
    messages: VecDeque<String>,
    until: Instant,
}
impl ChildFinish {
    pub(super) fn needs_controller(&self, key: &SessionKey, registry: &Registry) -> bool {
        registry
            .entries
            .get(&self.handle)
            .is_some_and(|child| &child.opener.key == key || (!self.finished && &child.key == key))
    }
}

pub(super) struct ChildRelocation {
    pub(super) old: PathBuf,
    destination: PathBuf,
    until: Instant,
    delivered: bool,
    new: Option<PathBuf>,
}
impl ChildRelocation {
    pub(super) fn allows(&self, file: Option<&Path>, now: Instant) -> bool {
        file.is_some_and(|file| {
            self.delivered && now < self.until && (file == self.old || self.accepts(file, now))
        })
    }
    fn accepts(&self, file: &Path, now: Instant) -> bool {
        if !self.delivered || now >= self.until {
            return false;
        }
        if let Some(known) = &self.new {
            return known == file;
        }
        file != self.old
            && file.file_name().is_some()
            && file.file_name() == self.old.file_name()
            && file.parent().and_then(Path::parent) == self.old.parent().and_then(Path::parent)
            && session_cwd(file).is_some_and(|cwd| canonical(&cwd) == self.destination)
    }
}

fn finish_request_key(epoch: crate::controller::ServiceEpoch, sequence: u64) -> RequestKey {
    RequestKey {
        client: format!("~agent-finish/{epoch}"),
        seq: sequence,
    }
}

impl Registry {
    pub(super) fn finish_references(&self, handle: &str) -> bool {
        self.finishes.values().any(|finish| finish.handle == handle)
    }

    fn next_finish_key(
        &mut self,
        epoch: crate::controller::ServiceEpoch,
    ) -> Result<RequestKey, String> {
        self.finish_sequence = self
            .finish_sequence
            .checked_add(1)
            .ok_or_else(|| "Worktree finish sequence exhausted".to_owned())?;
        Ok(finish_request_key(epoch, self.finish_sequence))
    }
}

fn finish_worktree(cwd: &Path) -> Result<worktree::Worktree, String> {
    worktree::resolve(cwd).ok_or_else(|| "That session is not running in a worktree".to_owned())
}

impl Engine {
    pub(super) fn cancel_child_finish_request(&mut self, request: u64) {
        let key = self
            .agent_tools
            .children
            .finishes
            .iter()
            .find_map(|(key, finish)| (finish.request == Some(request)).then(|| key.clone()));
        let Some(key) = key else {
            return;
        };
        let finish = self
            .agent_tools
            .children
            .finishes
            .get_mut(&key)
            .expect("finish request");
        finish.request = None;
        if !finish.started {
            self.forget_child_finish(&key);
        } else if let Some(cancel) = self.jobs.get(&key).and_then(|job| job.cancel.as_ref()) {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

// Only the small session header is needed. Filename identity plus the exact `/move` cwd
// prevent a concurrent /new, /resume or /move elsewhere from inheriting the handle.
fn session_cwd(file: &Path) -> Option<PathBuf> {
    let reader = BufReader::new(std::fs::File::open(file).ok()?.take(16 * 1024));
    for line in reader.lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(|v| v.as_str()) == Some("session") {
            return value.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);
        }
    }
    None
}

impl ChildSession {
    pub(super) fn relocation_discovering(&self, now: Instant) -> bool {
        self.relocation
            .as_ref()
            .is_some_and(|permit| now < permit.until && self.file.as_ref() == Some(&permit.old))
    }

    /// A relocation exception requires at least one live snapshot, never cached paths alone.
    pub(super) fn relocation_current_file<'a>(
        &'a self,
        sources: [Option<Option<&'a Path>>; 2],
        now: Instant,
    ) -> Option<&'a Path> {
        let permit = self.relocation.as_ref()?;
        if !sources.iter().any(Option::is_some)
            || !sources
                .iter()
                .flatten()
                .all(|file| permit.allows(*file, now))
        {
            return None;
        }
        sources
            .into_iter()
            .flatten()
            .flatten()
            .find(|file| *file != permit.old)
            .or(self.file.as_deref())
    }

    pub(super) fn relocation_file(&self, file: Option<&Path>, now: Instant) -> bool {
        self.relocation
            .as_ref()
            .is_some_and(|permit| permit.allows(file, now))
    }
    fn record_relocation(&mut self, file: PathBuf) {
        if let Some(permit) = &self.relocation {
            self.attach.cwd.clone_from(&permit.destination);
        }
        if let Some(permit) = &mut self.relocation {
            permit.new = Some(file.clone());
        }
        self.attach.session_path = Some(file.clone());
        if self.file.as_ref() != Some(&file) {
            self.file_seen = false;
        }
        self.file = Some(file);
    }
}

impl Engine {
    /// Capture links at admission, before retire moves their sessions out of the worktree.
    /// The worktree engine owns the outcomes; child watches may be removed after delivery.
    pub(in crate::gateway::engine) fn child_worktree_handles(
        &self,
        wt: &worktree::Worktree,
    ) -> Vec<String> {
        self.agent_tools
            .children
            .entries
            .iter()
            .filter_map(|(handle, child)| {
                let cwd = self
                    .sess
                    .find(&child.key)
                    .map(|i| self.sess.managed[i].current_cwd())
                    .unwrap_or(&child.attach.cwd);
                (!child.invalidated && cwd.starts_with(&wt.path)).then(|| handle.clone())
            })
            .collect()
    }

    pub(in crate::gateway::engine) fn begin_child_finish(
        &mut self,
        request: u64,
        args: serde_json::Value,
        now: Instant,
    ) -> Result<(), String> {
        let args: FinishArgs = serde_json::from_value(args).map_err(|e| e.to_string())?;
        let pending = &self.agent_tools.pending[&request];
        let root = canonical(pending.root.as_deref().ok_or_else(|| UNKNOWN.to_owned())?);
        let child = self.agent_tools.children.owned(&args.session, &root)?;
        if child.key == pending.target.key {
            return Err(UNKNOWN.to_owned());
        }
        // Correlation only: the journal sequence is allocated later, at admission.
        let key = RequestKey {
            client: format!("~agent-finish-pending/{}", self.epoch),
            seq: request,
        };
        if self.sess.find(&child.key).is_none() {
            self.attach_agent_child(&args.session)?;
        }
        self.agent_tools.children.finishes.insert(
            key,
            ChildFinish {
                handle: args.session,
                request: Some(request),
                title: String::new(),
                started: false,
                finished: false,
                blocked: None,
                messages: VecDeque::new(),
                until: now + DELIVERY,
            },
        );
        self.waker.now();
        Ok(())
    }

    pub(in crate::gateway::engine) fn child_finish_injected(
        &mut self,
        key: &RequestKey,
        branch: &str,
        base: &str,
    ) {
        let Some(finish) = self.agent_tools.children.finishes.get_mut(key) else {
            return;
        };
        // A requeued merge is a new attempt; its terminal blocker must still reach the opener.
        finish.blocked = None;
        let request = finish.request.take();
        let title = finish.title.clone();
        if let Some(request) = request {
            self.finish_agent_tool(request, true, format!(
                "Finish started: {title} was asked to commit and merge {branch} into {base}. OmpDeck removes the worktree once it is clean and merged; the outcome arrives as a message."
            ));
        }
    }

    pub(in crate::gateway::engine) fn child_finish_blocked(
        &mut self,
        key: &RequestKey,
        text: &str,
    ) {
        if let Some(finish) = self.agent_tools.children.finishes.get_mut(key)
            && finish.blocked.is_none()
        {
            finish.blocked = Some(text.to_owned());
            finish.messages.push_back(text.to_owned());
        }
    }

    pub(in crate::gateway::engine) fn child_finish_done(
        &mut self,
        key: &RequestKey,
        result: &Result<Done, GatewayError>,
    ) {
        let Some(finish) = self.agent_tools.children.finishes.get_mut(key) else {
            return;
        };
        finish.finished = true;
        let text = match result {
            Ok(Done::Retired { parked, .. }) => {
                let child = &self.agent_tools.children.entries[&finish.handle];
                // Retirement records the confirmed destination in cwd; the asynchronous
                // breadcrumb can still describe the worktree that has just been removed.
                let cwd = self
                    .sess
                    .find(&child.key)
                    .map(|i| self.sess.managed[i].cwd.as_path())
                    .unwrap_or(&child.attach.cwd);
                format!(
                    "Worktree {}. {} now runs in {}.",
                    if *parked { "parked" } else { "removed" },
                    finish.title,
                    home_path(cwd)
                )
            }
            Ok(_) => "Worktree finish completed.".to_owned(),
            Err(GatewayError::Cancelled) => {
                "Worktree finish cancelled; the worktree stays.".to_owned()
            }
            Err(error) => format!("Worktree finish failed: {error}"),
        };
        if let Some(request) = finish.request.take() {
            self.finish_agent_tool(request, false, text);
            self.forget_child_finish(key);
        } else {
            finish.messages.push_back(text);
        }
    }

    pub(super) fn step_child_finish_messages(&mut self, now: Instant) {
        let keys: Vec<_> = self.agent_tools.children.finishes.keys().cloned().collect();
        for mut key in keys {
            let finish = &self.agent_tools.children.finishes[&key];
            let child = &self.agent_tools.children.entries[&finish.handle];
            if !finish.started {
                let request = finish.request.expect("unstarted finish");
                if !self.agent_tools.pending.contains_key(&request) {
                    self.forget_child_finish(&key);
                    continue;
                }
                if self.sess.find(&child.key).is_none() && now < finish.until {
                    continue;
                }
                let handle = finish.handle.clone();
                match self.start_child_worktree_finish(&key, &handle) {
                    Ok(started) => key = started,
                    Err(error) => {
                        self.forget_child_finish(&key);
                        self.finish_agent_tool(request, false, error);
                        continue;
                    }
                }
            }
            let finish = &self.agent_tools.children.finishes[&key];
            let child = &self.agent_tools.children.entries[&finish.handle];
            if self.background_opener_ended(child) {
                // The job is independent once started; only its callback is discarded.
                self.forget_child_finish(&key);
                continue;
            }
            if let Some(index) = self.background_delivery_target(&child.opener_root) {
                let root = child.opener_root.clone();
                let finish = self
                    .agent_tools
                    .children
                    .finishes
                    .get_mut(&key)
                    .expect("finish");
                let title = format!("Worktree finish: {} ({})", finish.title, finish.handle);
                let messages = std::mem::take(&mut finish.messages);
                for text in messages {
                    self.deliver_child_message(
                        index,
                        CtlCommand::Deliver {
                            root: root.clone(),
                            title: title.clone(),
                            text,
                            details: None,
                        },
                    );
                }
            }
            if self
                .agent_tools
                .children
                .finishes
                .get(&key)
                .is_some_and(|finish| finish.finished && finish.messages.is_empty())
            {
                self.forget_child_finish(&key);
            }
        }
        if !self.agent_tools.children.finishes.is_empty() {
            self.wake_within(POLL);
        }
    }

    fn start_child_worktree_finish(
        &mut self,
        key: &RequestKey,
        handle: &str,
    ) -> Result<RequestKey, String> {
        let index = self.valid_child(handle)?;
        let session = &self.sess.managed[index];
        let worktree = finish_worktree(session.current_cwd())?;
        let session_key = session.ctl.target().key.clone();
        let title = session.display_label();
        let op = Mutation::WorktreeFinish { worktree };
        if let Some(error) = self.worktree_precheck(&op) {
            return Err(error.to_string());
        }
        if let Some(maintenance) = &self.barrier {
            return Err(GatewayError::Maintenance(maintenance.clone()).to_string());
        }
        // Attach completion may reorder random tool requests. Allocate only now so
        // journal admission order stays monotonic even across its retention watermark.
        let admitted = self.agent_tools.children.next_finish_key(self.epoch)?;
        match self
            .journal
            .admit(&admitted, &op, None, self.epoch)
            .map_err(|error| error.to_string())?
        {
            crate::gateway::journal::Admission::Accepted => {}
            _ => return Err("The worktree finish request key was already used".to_owned()),
        }
        let mut finish = self
            .agent_tools
            .children
            .finishes
            .remove(key)
            .expect("finish");
        finish.title = title;
        finish.started = true;
        self.agent_tools
            .children
            .finishes
            .insert(admitted.clone(), finish);
        self.execute_worktree(0, 0, admitted.clone(), op);
        self.prefer_worktree_finish_session(&admitted, session_key);
        Ok(admitted)
    }

    pub(super) fn append_child_worktree(&self, row: &mut String, handle: &str) {
        let child = &self.agent_tools.children.entries[handle];
        let cwd = self
            .sess
            .find(&child.key)
            .map(|i| self.sess.managed[i].current_cwd())
            .unwrap_or(&child.attach.cwd);
        if let Some(wt) = worktree::resolve(cwd) {
            row.push_str(" | worktree ");
            row.push_str(wt.branch.as_deref().unwrap_or("HEAD"));
            row.push_str(" → ");
            row.push_str(
                self.cfg
                    .worktree_bases
                    .get(&wt.path)
                    .map_or("HEAD", String::as_str),
            );
            self.append_worktree_finish_state(row, &wt.path);
        }
    }
    fn forget_child_finish(&mut self, key: &RequestKey) {
        if let Some(finish) = self.agent_tools.children.finishes.remove(key) {
            let child = &self.agent_tools.children.entries[&finish.handle];
            let child_key = child.key.clone();
            let opener_key = child.opener.key.clone();
            self.release_idle_child_attachment(&child_key);
            self.release_idle_child_attachment(&opener_key);
        }
    }

    pub(in crate::gateway::engine) fn child_finish_refusal(
        &mut self,
        key: &RequestKey,
    ) -> Option<String> {
        let handle = self.agent_tools.children.finishes.get(key)?.handle.clone();
        self.valid_child(&handle).err()
    }

    pub(in crate::gateway::engine) fn child_retire_move_delivered(
        &mut self,
        key: &SessionKey,
        delivered: bool,
    ) {
        for child in self
            .agent_tools
            .children
            .entries
            .values_mut()
            .filter(|child| &child.key == key)
        {
            if delivered {
                if let Some(permit) = &mut child.relocation {
                    permit.delivered = true;
                }
            } else {
                child.relocation = None;
            }
        }
    }

    pub(in crate::gateway::engine) fn arm_child_retire_move(
        &mut self,
        key: &SessionKey,
        destination: &Path,
        now: Instant,
    ) {
        for child in self
            .agent_tools
            .children
            .entries
            .values_mut()
            .filter(|child| &child.key == key && !child.invalidated)
        {
            if let Some(file) = &child.file {
                child.relocation = Some(ChildRelocation {
                    old: file.clone(),
                    destination: canonical(destination),
                    until: now + Duration::from_secs(30),
                    delivered: false,
                    new: None,
                });
            }
        }
    }

    pub(in crate::gateway::engine) fn sync_child_relocations(&mut self, now: Instant) {
        let updates: Vec<_> = self
            .agent_tools
            .children
            .entries
            .iter()
            .filter_map(|(handle, child)| {
                let permit = child.relocation.as_ref()?;
                if child.invalidated || now >= permit.until {
                    return None;
                }
                let index = self.sess.find(&child.key)?;
                let session = &self.sess.managed[index];
                let managed = session
                    .live
                    .as_ref()
                    .filter(|link| link.client.connected())
                    .and_then(|link| link.client.latest());
                let questions = self.child_question_snapshot(handle);
                let file = [
                    managed
                        .as_ref()
                        .and_then(|snapshot| snapshot.session_file.as_deref()),
                    questions
                        .as_ref()
                        .and_then(|snapshot| snapshot.session_file.as_deref()),
                    session.ctl.session_path(),
                ]
                .into_iter()
                .flatten()
                .find(|file| permit.accepts(file, now))?;
                (child.file.as_deref() != Some(file)).then(|| (handle.clone(), file.to_owned()))
            })
            .collect();
        for (handle, file) in updates {
            self.agent_tools
                .children
                .entries
                .get_mut(&handle)
                .expect("child")
                .record_relocation(file);
        }
        for child in self.agent_tools.children.entries.values_mut() {
            if child
                .relocation
                .as_ref()
                .is_some_and(|permit| now >= permit.until)
            {
                child.relocation = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_finish_callback_keeps_registry_identity_without_owning_the_child_controller() {
        let mut tools = crate::gateway::engine::agent_tools::AgentTools::default();
        let child = super::super::tests::child();
        let child_key = child.key.clone();
        let handle = "s-retained";
        tools.children.entries.insert(handle.into(), child);
        tools.children.finishes.insert(
            RequestKey {
                client: "finish-test".into(),
                seq: 1,
            },
            ChildFinish {
                handle: handle.into(),
                request: None,
                title: "Retired".into(),
                started: true,
                finished: true,
                blocked: None,
                messages: VecDeque::new(),
                until: Instant::now() + Duration::from_secs(10),
            },
        );
        assert!(!tools.controller_needed(&child_key));
        assert!(tools.child_registry_referenced(handle));
        tools.children.finishes.clear();
        assert!(!tools.child_registry_referenced(handle));
    }

    #[test]
    fn relocation_preserves_discovery_until_the_new_file_can_materialize() {
        let mut child = super::super::tests::child();
        let now = Instant::now();
        child.file_seen = true;
        child.relocation = Some(ChildRelocation {
            old: child.file.clone().unwrap(),
            destination: PathBuf::from("primary"),
            until: now + Duration::from_secs(30),
            delivered: true,
            new: None,
        });
        assert!(child.relocation_discovering(now));
        child.record_relocation(PathBuf::from("primary/child.jsonl"));
        assert!(!child.file_seen);
        assert!(!child.relocation_discovering(now));
    }

    #[test]
    fn finish_sequences_cross_the_journal_retention_window_and_keep_the_epoch_fence() {
        use crate::controller::ServiceEpoch;
        use crate::gateway::journal::{Admission, Journal};
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(&dir.path().join("journal")).unwrap();
        let epoch = ServiceEpoch([1; 16]);
        let mut registry = Registry::default();
        let operation = Mutation::WorktreeFinish {
            worktree: worktree::Worktree {
                path: dir.path().join("child"),
                primary: dir.path().to_owned(),
                branch: Some("wt/retention".to_owned()),
            },
        };
        let mut first = None;
        for sequence in 1..=600 {
            let key = registry.next_finish_key(epoch).unwrap();
            assert_eq!(key.seq, sequence);
            assert!(matches!(
                journal.admit(&key, &operation, None, epoch).unwrap(),
                Admission::Accepted
            ));
            if first.is_none() {
                first = Some(key);
            }
        }
        let first = first.unwrap();
        assert!(matches!(journal.lookup(&first), Err(Admission::Expired)));
        let restarted_epoch = ServiceEpoch([2; 16]);
        let restarted = Registry::default()
            .next_finish_key(restarted_epoch)
            .unwrap();
        assert_eq!(restarted.seq, 1);
        assert_ne!(first.client, restarted.client);
        assert!(matches!(
            journal
                .admit(&restarted, &operation, None, restarted_epoch)
                .unwrap(),
            Admission::Accepted
        ));
    }

    #[test]
    fn absent_relocation_snapshots_use_normal_controller_and_running_file_evidence() {
        let mut child = super::super::tests::child();
        let old = child.file.clone().unwrap();
        let replacement = PathBuf::from("replacement.jsonl");
        let now = Instant::now();
        for delivered in [false, true] {
            child.relocation = Some(ChildRelocation {
                old: old.clone(),
                destination: PathBuf::from("primary"),
                until: now + Duration::from_secs(30),
                delivered,
                new: None,
            });
            assert_eq!(child.relocation_current_file([None, None], now), None);
            assert_eq!(
                conversation_file(Some(&replacement), [None, None], &[]).unwrap(),
                Some(replacement.as_path())
            );
            assert_eq!(
                conversation_file(Some(&old), [None, None], std::slice::from_ref(&replacement))
                    .unwrap(),
                Some(replacement.as_path())
            );
            assert_eq!(
                child.relocation_current_file([Some(Some(&old)), None], now),
                delivered.then_some(old.as_path())
            );
        }
    }

    #[test]
    fn relocation_preserves_handle_only_for_expected_destination_and_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("primary");
        std::fs::create_dir(&destination).unwrap();
        let old = dir.path().join("old").join("same.jsonl");
        let new_dir = dir.path().join("new");
        std::fs::create_dir(&new_dir).unwrap();
        let new = new_dir.join("same.jsonl");
        std::fs::write(
            &new,
            format!(
                "{{\"type\":\"session\",\"cwd\":{}}}\n",
                serde_json::to_string(&destination).unwrap()
            ),
        )
        .unwrap();
        let now = Instant::now();
        let mut child = super::super::tests::child();
        child.file = Some(old.clone());
        child.relocation = Some(ChildRelocation {
            old,
            destination: destination.clone(),
            until: now + DELIVERY,
            delivered: true,
            new: None,
        });
        child.relocation.as_mut().unwrap().delivered = false;
        assert!(!child.relocation_file(Some(&new), now));
        child.relocation.as_mut().unwrap().delivered = true;
        std::fs::write(&new, "{\"type\":\"session\",\"cwd\":\"/elsewhere\"}\n").unwrap();
        assert!(!child.relocation_file(Some(&new), now));
        std::fs::write(
            &new,
            format!(
                "{{\"type\":\"session\",\"cwd\":{}}}\n",
                serde_json::to_string(&destination).unwrap()
            ),
        )
        .unwrap();
        assert!(child.relocation_file(Some(&new), now));
        assert!(!child.relocation_file(Some(&dir.path().join("other.jsonl")), now));
        assert!(!child.relocation_file(Some(&new), now + DELIVERY));
        child.record_relocation(new.clone());
        let mut registry = Registry::default();
        let handle = registry.insert(child).unwrap();
        assert_eq!(
            registry
                .owned(&handle, Path::new("opener.jsonl"))
                .unwrap()
                .file
                .as_deref(),
            Some(new.as_path())
        );
        assert!(
            !registry.entries[&handle].relocation_file(Some(&dir.path().join("other.jsonl")), now)
        );
    }
    #[test]
    fn finish_ownership_rejects_other_roots_and_invalidated_children() {
        let mut registry = Registry::default();
        let handle = registry.insert(super::super::tests::child()).unwrap();
        assert_eq!(
            registry
                .owned(&handle, Path::new("another.jsonl"))
                .err()
                .unwrap(),
            UNKNOWN
        );
        registry.entries.get_mut(&handle).unwrap().invalidated = true;
        assert_eq!(
            registry
                .owned(&handle, Path::new("opener.jsonl"))
                .err()
                .unwrap(),
            STALE
        );
    }
    #[test]
    fn finishing_a_primary_or_nonrepository_is_refused() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            finish_worktree(root.path()).unwrap_err(),
            "That session is not running in a worktree"
        );
        std::fs::create_dir(root.path().join(".git")).unwrap();
        assert_eq!(
            finish_worktree(root.path()).unwrap_err(),
            "That session is not running in a worktree"
        );
    }
}
