//! Detached turn watches: no tool request or cancellation owns a registered watch.
use deck_core::ctl::CtlCommand;

use super::*;
use crate::gateway::engine::sessions::Managed;
use crate::gateway::running::RunningSnapshot;

pub(super) struct OpenerSource {
    pub(super) key: SessionKey,
    pub(super) mux_name: Option<String>,
    pub(super) backend: Option<MuxKind>,
}
impl OpenerSource {
    pub(super) fn from_managed(session: &Managed) -> Self {
        Self {
            key: session.ctl.target().key.clone(),
            mux_name: session.mux_name.clone(),
            backend: session.mux.as_ref().map(deck_core::mux::Mux::kind),
        }
    }

    // A direct PTY dies with its controller. A persistent backend needs positive exit or
    // conversation evidence; an absent socket alone is not enough.
    fn ended_detached(&self, root: &Path, running: Option<&RunningSnapshot>) -> bool {
        let Some(name) = self.mux_name.as_deref() else {
            return true;
        };
        let Some(running) = running.filter(|s| !s.blind) else {
            return false;
        };
        !running.is_alive(name)
            || self
                .backend
                .zip(running.owner_of(name))
                .is_some_and(|(expected, actual)| expected != actual)
            || (!running.files_of(name).is_empty()
                && !running
                    .files_of(name)
                    .iter()
                    .any(|file| canonical(file) == root))
    }
}

fn conversation_ended(root: &Path, file: Option<&Path>, explicit_snapshot: bool) -> bool {
    file.map_or(explicit_snapshot, |file| canonical(file) != root)
}

pub(super) struct BackgroundWatch {
    pub(super) id: u64,
    title: String,
    state: WatchState,
}
enum WatchState {
    // None only while the engine steps the waiter; ownership stays visible to
    // synchronous headless-attach admission during that step.
    Watching(Option<Box<Waiter>>),
    Held(WaitResult),
    Sent,
}
impl BackgroundWatch {
    fn new(id: u64, title: String, mut waiter: Box<Waiter>) -> Self {
        waiter.background = false;
        waiter.background_watch = true;
        Self {
            id,
            title,
            state: WatchState::Watching(Some(waiter)),
        }
    }
    fn finish(&mut self, result: WaitResult) {
        if matches!(self.state, WatchState::Watching(_)) {
            self.state = WatchState::Held(result);
        }
    }
    fn waiter_mut(&mut self) -> Option<&mut Waiter> {
        match &mut self.state {
            WatchState::Watching(waiter) => waiter.as_deref_mut(),
            WatchState::Held(_) | WatchState::Sent => None,
        }
    }
    fn take_waiter(&mut self) -> Option<Box<Waiter>> {
        match &mut self.state {
            WatchState::Watching(waiter) => waiter.take(),
            WatchState::Held(_) | WatchState::Sent => None,
        }
    }
    pub(super) fn needs_controller(&self, key: &SessionKey, child: &ChildSession) -> bool {
        key == &child.opener.key
            || (key == &child.key && matches!(self.state, WatchState::Watching(_)))
    }
    fn take_command(&mut self, handle: &str, root: &Path, eligible: bool) -> Option<CtlCommand> {
        if !eligible || !matches!(self.state, WatchState::Held(_)) {
            return None;
        }
        let WatchState::Held(result) = std::mem::replace(&mut self.state, WatchState::Sent) else {
            unreachable!()
        };
        Some(CtlCommand::Deliver {
            root: root.to_owned(),
            title: format!("Background wait: {} ({handle})", self.title),
            text: result.text,
            details: Some(serde_json::json!({"handle": handle, "status": result.status})),
        })
    }
}

impl super::super::AgentTools {
    pub(super) fn waiter_mut(&mut self, request: u64) -> Option<&mut Waiter> {
        if let Some(Operation::Wait(waiter)) = self.operations.get_mut(&request) {
            return Some(waiter);
        }
        self.children
            .entries
            .values_mut()
            .filter_map(|child| child.background.as_mut())
            .find(|watch| watch.id == request)
            .and_then(BackgroundWatch::waiter_mut)
            .or_else(|| self.fan_out.waiter_mut(request))
    }

    pub(in crate::gateway::engine) fn background_request(&self, request: u64) -> bool {
        self.children.entries.values().any(|child| {
            child
                .background
                .as_ref()
                .is_some_and(|watch| watch.id == request)
        }) || self.fan_out.contains(request)
    }
}

impl Engine {
    pub(super) fn finish_child_wait(&mut self, request: u64, result: WaitResult) {
        if self.fanout_wait_failed(request, &result.text) {
            return;
        }
        if matches!(self.agent_tools.operations.get(&request),
            Some(Operation::Wait(waiter)) if waiter.background && waiter.baseline.is_some())
        {
            let Some(Operation::Wait(waiter)) = self.agent_tools.operations.remove(&request) else {
                unreachable!()
            };
            self.arm_background_wait(request, waiter);
        }
        if let Some(child) = self
            .agent_tools
            .children
            .entries
            .values_mut()
            .find(|child| {
                child
                    .background
                    .as_ref()
                    .is_some_and(|watch| watch.id == request)
            })
        {
            child
                .background
                .as_mut()
                .expect("background watch")
                .finish(result);
            let key = child.key.clone();
            self.release_idle_child_attachment(&key);
        } else {
            self.finish_agent_tool(request, result.ok, result.text);
        }
    }

    pub(super) fn arm_background_wait(&mut self, request: u64, waiter: Box<Waiter>) {
        let handle = waiter.handle.clone();
        let child = &self.agent_tools.children.entries[&handle];
        let title = self
            .sess
            .find(&child.key)
            .map(|i| self.sess.managed[i].display_label())
            .unwrap_or_else(|| child.attach.label.clone());
        let title = utf8_prefix(&title, 160).to_owned();
        let watch = BackgroundWatch::new(request, title.clone(), waiter);
        if let Err(error) = self.agent_tools.children.arm_background(&handle, watch) {
            self.finish_agent_tool(request, false, error);
            let key = self.agent_tools.children.entries[&handle].key.clone();
            self.release_idle_child_attachment(&key);
            return;
        }
        self.finish_agent_tool(request, true, format!(
            "Watching {title} ({handle}) in the background; its result will arrive as a message."
        ));
    }

    pub(in crate::gateway::engine) fn drop_background_waits(&mut self) {
        self.agent_tools.fan_out.clear();
        for child in self.agent_tools.children.entries.values_mut() {
            child.background = None;
            child.forward_questions = false;
            child.questions.stop();
        }
        self.agent_tools.children.answers.clear();
        self.agent_tools.children.reports.clear();
        self.agent_tools.children.finishes.clear();
        for child in self.agent_tools.children.entries.values_mut() {
            child.reports = ReportRate::new(Instant::now());
        }
        self.agent_tools.background_acks.clear();
    }

    pub(in crate::gateway::engine) fn drop_background_opener(&mut self, key: &SessionKey) {
        self.drop_fan_out_opener(key);
        for child in self.agent_tools.children.entries.values_mut() {
            if &child.opener.key == key {
                child.forward_questions = false;
                child.questions.stop();
            }
        }
        for child in self.agent_tools.children.drop_background_opener(key) {
            self.release_idle_child_attachment(&child);
        }
    }

    pub(super) fn background_opener_ended(&self, child: &ChildSession) -> bool {
        if let Some(i) = self.sess.find(&child.opener.key) {
            let session = &self.sess.managed[i];
            if session.exited() {
                return true;
            }
            if let Some(snapshot) = session
                .live
                .as_ref()
                .filter(|link| link.client.connected())
                .and_then(|link| link.client.latest())
            {
                return conversation_ended(
                    &child.opener_root,
                    snapshot.session_file.as_deref().map(Path::new),
                    true,
                );
            }
            self.with_current_child_file(None, i, |file| {
                file.map_or(true, |file| {
                    conversation_ended(&child.opener_root, file, false)
                })
            })
        } else {
            child
                .opener
                .ended_detached(&child.opener_root, self.running.latest().as_deref())
        }
    }

    pub(super) fn background_delivery_target(&self, root: &Path) -> Option<usize> {
        // The caller must retain its unsent result while rollback owns this session.
        self.sess
            .managed
            .iter()
            .enumerate()
            .find_map(|(i, session)| {
                let eligible = !session.exited()
                    && !self.checkpoint_restore_busy(session.ctl.target())
                    && session.live.as_ref().is_some_and(|link| {
                        link.client.connected()
                            && link
                                .client
                                .features()
                                .iter()
                                .any(|feature| feature == "deliver")
                    });
                (eligible
                    && self.with_current_child_file(None, i, |file| {
                        file.is_ok_and(|file| file.is_some_and(|file| canonical(file) == root))
                    }))
                .then_some(i)
            })
    }

    pub(super) fn deliver_child_message(&mut self, index: usize, command: CtlCommand) {
        let target = self.sess.managed[index].ctl.target().clone();
        let restoring = self.checkpoint_restore_busy(&target);
        let Some((target, command)) = self.agent_tools.admit_delivery(target, command, restoring)
        else {
            return;
        };
        let session = &self.sess.managed[index];
        debug_assert_eq!(session.ctl.target(), &target);
        let rid = session
            .live
            .as_ref()
            .expect("eligible live link")
            .client
            .send(command);
        // Uncertain delivery is final: retain the issuer through the ACK, never retry.
        self.agent_tools.reply_acks.push((target.clone(), rid));
        self.agent_tools.background_acks.push((target, rid));
    }

    pub(in crate::gateway::engine) fn checkpoint_delivery_pending(
        &self,
        target: &SessionTarget,
    ) -> bool {
        self.agent_tools.delivery_pending(target)
    }

    pub(in crate::gateway::engine) fn checkpoint_delivery_awaiting_ack(
        &self,
        target: &SessionTarget,
    ) -> bool {
        self.agent_tools.delivery_awaiting_ack(target)
    }

    pub(in crate::gateway::engine) fn flush_child_deliveries(&mut self) {
        // Only unsent frames are deferred. Once sent, an uncertain ACK is never retried.
        for _ in 0..self.agent_tools.deferred_deliveries.len() {
            let (target, command) = self
                .agent_tools
                .deferred_deliveries
                .pop_front()
                .expect("queued delivery");
            let Some(index) = self.sess.find(&target.key) else {
                continue;
            };
            let session = &self.sess.managed[index];
            if session.ctl.target() != &target || session.exited() {
                continue;
            }
            if self.checkpoint_restore_busy(&target)
                || !session
                    .live
                    .as_ref()
                    .is_some_and(|link| link.client.connected())
            {
                self.agent_tools
                    .deferred_deliveries
                    .push_back((target, command));
                continue;
            }
            // The original target/generation is retained, and Deliver carries its original
            // conversation root for the extension's authoritative root check.
            self.deliver_child_message(index, command);
        }
    }

    pub(super) fn step_background_waits(&mut self, now: Instant) {
        let handles: Vec<_> = self
            .agent_tools
            .children
            .entries
            .iter()
            .filter_map(|(handle, child)| child.background.as_ref().map(|_| handle.clone()))
            .collect();
        for handle in handles {
            let child = &self.agent_tools.children.entries[&handle];
            let child_key = child.key.clone();
            let opener_key = child.opener.key.clone();
            if self.background_opener_ended(child) {
                self.agent_tools
                    .children
                    .entries
                    .get_mut(&handle)
                    .expect("child")
                    .background = None;
                self.release_idle_child_attachment(&child_key);
                self.release_idle_child_attachment(&opener_key);
                continue;
            }
            let (request, waiter) = {
                let watch = self
                    .agent_tools
                    .children
                    .entries
                    .get_mut(&handle)
                    .expect("child")
                    .background
                    .as_mut()
                    .expect("background watch");
                (watch.id, watch.take_waiter())
            };
            if let Some(mut waiter) = waiter {
                let result = self.step_child_waiter(request, &mut waiter, now);
                let status = waiter.status;
                let watch = self
                    .agent_tools
                    .children
                    .entries
                    .get_mut(&handle)
                    .expect("child")
                    .background
                    .as_mut()
                    .expect("background watch");
                watch.state = WatchState::Watching(Some(waiter));
                if let Some((ok, text)) = result {
                    watch.finish(WaitResult::new(ok, text, status));
                }
            }
            let mut watch = self
                .agent_tools
                .children
                .entries
                .get_mut(&handle)
                .expect("child")
                .background
                .take()
                .expect("background watch");
            let root = &self.agent_tools.children.entries[&handle].opener_root;
            if let Some(index) = self.background_delivery_target(root)
                && let Some(command) = watch.take_command(&handle, root, true)
            {
                self.deliver_child_message(index, command);
            } else {
                self.agent_tools
                    .children
                    .entries
                    .get_mut(&handle)
                    .expect("child")
                    .background = Some(watch);
            }
            self.release_idle_child_attachment(&child_key);
            self.release_idle_child_attachment(&opener_key);
        }
        if self
            .agent_tools
            .children
            .entries
            .values()
            .any(|child| child.background.is_some())
        {
            self.wake_within(POLL);
        }
    }

    pub(super) fn background_reply_done(
        &mut self,
        key: &SessionKey,
        rid: u64,
        ok: bool,
        root_mismatch: bool,
    ) {
        let before = self.agent_tools.background_acks.len();
        self.agent_tools
            .background_acks
            .retain(|(target, pending)| &target.key != key || *pending != rid);
        if self.agent_tools.background_acks.len() != before {
            if root_mismatch {
                // The watch already transferred its result to this one-shot request.
                // An extension-side root fence is final: never re-arm or retry it.
                log::info!(
                    "Background wait opener {key:?} switched conversations; dropping result"
                );
            } else if !ok {
                log::warn!("Background wait delivery rejected by {key:?}; dropping result");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_disconnected_conversation_is_held_not_treated_as_a_switch() {
        let root = canonical(Path::new("opener.jsonl"));
        assert!(!conversation_ended(&root, None, false));
        assert!(
            conversation_ended(&root, None, true),
            "explicitly cleared conversation"
        );
        assert!(!conversation_ended(&root, Some(&root), false));
        assert!(conversation_ended(
            &root,
            Some(Path::new("other.jsonl")),
            false
        ));
    }
    use crate::controller::ServiceEpoch;
    use crate::gateway::engine::agent_tools::AgentTools;

    fn watch(id: u64, handle: &str, child: &ChildSession) -> BackgroundWatch {
        BackgroundWatch::new(
            id,
            "child".into(),
            Box::new(Waiter::new(
                handle.into(),
                child,
                child.last.clone(),
                None,
                None,
                true,
                Instant::now(),
            )),
        )
    }

    fn insert(registry: &mut Registry, id: u64) -> String {
        let handle = format!("s-{id:08x}");
        registry
            .entries
            .insert(handle.clone(), super::super::tests::child());
        handle
    }

    #[test]
    fn background_replaces_silently_and_ignores_the_old_worker() {
        let mut tools = AgentTools::default();
        let handle = insert(&mut tools.children, 1);
        let first = watch(10, &handle, &tools.children.entries[&handle]);
        tools
            .children
            .arm_background(&handle, first)
            .expect("first watch");
        let second = watch(11, &handle, &tools.children.entries[&handle]);
        tools
            .children
            .arm_background(&handle, second)
            .expect("replace");
        assert!(
            tools.waiter_mut(10).is_none(),
            "late first scan is discarded"
        );
        assert!(tools.waiter_mut(11).is_some());
        assert!(
            tools.pending.is_empty(),
            "registered watches are not pending tools"
        );
        assert!(tools.operations.is_empty());
        assert!(
            tools
                .children
                .entries
                .get_mut(&handle)
                .unwrap()
                .background
                .as_mut()
                .unwrap()
                .take_command(&handle, Path::new("opener.jsonl"), true)
                .is_none(),
            "replacement emits no message"
        );
    }

    #[test]
    fn a_superseded_initial_inspection_cannot_replace_the_newer_watch() {
        let mut registry = Registry::default();
        let handle = insert(&mut registry, 1);
        registry.entries.get_mut(&handle).unwrap().background_call = Some(2);
        let newer = watch(2, &handle, &registry.entries[&handle]);
        registry
            .arm_background(&handle, newer)
            .expect("newer watch");
        let older = watch(1, &handle, &registry.entries[&handle]);
        registry
            .arm_background(&handle, older)
            .expect("superseded inspection ends silently");
        assert_eq!(registry.entries[&handle].background.as_ref().unwrap().id, 2);
    }

    #[test]
    fn background_caps_eight_per_opener_but_allows_replacement() {
        let mut registry = Registry::default();
        for id in 0..8 {
            let handle = insert(&mut registry, id);
            let watch = watch(id, &handle, &registry.entries[&handle]);
            registry.arm_background(&handle, watch).expect("within cap");
        }
        let ninth = insert(&mut registry, 8);
        let extra = watch(8, &ninth, &registry.entries[&ninth]);
        assert_eq!(
            registry.arm_background(&ninth, extra).unwrap_err(),
            "Too many background waits"
        );
        let first = "s-00000000";
        let replacement = watch(99, first, &registry.entries[first]);
        registry
            .arm_background(first, replacement)
            .expect("replacement at cap");
        assert_eq!(registry.entries[first].background.as_ref().unwrap().id, 99);
        registry.entries.get_mut(&ninth).unwrap().opener_root = PathBuf::from("another.jsonl");
        let another = watch(100, &ninth, &registry.entries[&ninth]);
        registry
            .arm_background(&ninth, another)
            .expect("different opener has its own cap");
    }

    #[test]
    fn background_baseline_stays_bound_when_send_updates_last() {
        let dir = tempfile::tempdir().expect("temporary transcript");
        let path = dir.path().join("turns.jsonl");
        let user = |text| {
            serde_json::json!({
                "type":"message", "message":{"role":"user","content":[{"type":"text","text":text}]}
            })
            .to_string()
        };
        let reply = |text| {
            serde_json::json!({
            "type":"message", "message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":text}]}
        }).to_string()
        };
        std::fs::write(
            &path,
            format!("{}\n{}\n{}\n", user("A"), reply("reply A"), user("B")),
        )
        .expect("persisted turns");
        let mut child = super::super::tests::child();
        child.file = Some(path);
        child.last = Some(Baseline {
            offset: 0,
            prompt: "A".into(),
        });
        let mut watch = watch(1, "s-00000001", &child);
        child.last = Some(Baseline {
            offset: 100,
            prompt: "B".into(),
        });
        let waiter = watch.waiter_mut().unwrap();
        assert_eq!(waiter.baseline.as_ref().unwrap().prompt, "A");
        assert_eq!(waiter.baseline.as_ref().unwrap().offset, 0);
        let scan = waiter.watch.as_mut().unwrap().poll().expect("bound scan");
        assert_eq!(scan.reply.as_deref(), Some("reply A"));
        assert!(scan.later_prompt);
        assert!(waiter.watch.as_ref().unwrap().matched_exactly());
        assert!(
            !waiter.background,
            "registered watch cannot register another tool watch"
        );
    }

    #[test]
    fn background_holds_until_an_eligible_link_then_delivers_once() {
        let child = super::super::tests::child();
        let mut watch = watch(1, "s-00000001", &child);
        watch.finish(WaitResult::new(
            true,
            "child finished.\n\nreply".into(),
            "completed",
        ));
        assert!(
            watch
                .take_command("s-00000001", &child.opener_root, false)
                .is_none()
        );
        assert!(
            watch
                .take_command("s-00000001", &child.opener_root, false)
                .is_none()
        );
        let Some(CtlCommand::Deliver {
            root,
            title,
            text,
            details,
        }) = watch.take_command("s-00000001", &child.opener_root, true)
        else {
            panic!("eligible link receives held delivery");
        };
        assert_eq!(
            root, child.opener_root,
            "delivery binds to the original opener conversation"
        );
        assert_eq!(title, "Background wait: child (s-00000001)");
        assert_eq!(text, "child finished.\n\nreply");
        assert_eq!(
            details,
            Some(serde_json::json!({"handle":"s-00000001","status":"completed"}))
        );
        assert!(
            watch
                .take_command("s-00000001", &child.opener_root, true)
                .is_none()
        );
        watch.finish(WaitResult::new(false, STALE.into(), "invalid"));
        assert!(
            watch
                .take_command("s-00000001", &child.opener_root, true)
                .is_none(),
            "accepted delivery never re-sends"
        );
    }

    #[test]
    fn background_drops_when_the_registered_opener_ends() {
        let mut registry = Registry::default();
        let first = insert(&mut registry, 1);
        let second = insert(&mut registry, 2);
        registry.entries.get_mut(&second).unwrap().opener.key =
            SessionKey::host("test", "other-opener");
        for (id, handle) in [(1, &first), (2, &second)] {
            let mut watch = watch(id, handle, &registry.entries[handle]);
            watch.finish(WaitResult::new(true, "held".into(), "completed"));
            registry.arm_background(handle, watch).expect("armed");
        }
        let opener = registry.entries[&first].opener.key.clone();
        assert_eq!(registry.drop_background_opener(&opener).len(), 1);
        assert!(
            registry.entries[&first].background.is_none(),
            "held result drops silently"
        );
        assert!(
            registry.entries[&second].background.is_some(),
            "same root is not the registered source"
        );
    }

    #[test]
    fn absent_opener_link_is_not_an_ended_opener() {
        let child = super::super::tests::child();
        assert!(!child.opener.ended_detached(&child.opener_root, None));
        let mut blind = RunningSnapshot::default();
        blind.blind = true;
        assert!(
            !child
                .opener
                .ended_detached(&child.opener_root, Some(&blind))
        );
        let running = RunningSnapshot::new(["opener".into()].into_iter().collect(), HashMap::new())
            .with_owners([("opener".into(), MuxKind::Host)].into_iter().collect());
        assert!(
            !child
                .opener
                .ended_detached(&child.opener_root, Some(&running))
        );
        let switched = RunningSnapshot::new(
            ["opener".into()].into_iter().collect(),
            [("opener".into(), vec![PathBuf::from("other.jsonl")])]
                .into_iter()
                .collect(),
        );
        assert!(
            child
                .opener
                .ended_detached(&child.opener_root, Some(&switched))
        );
        assert!(
            child
                .opener
                .ended_detached(&child.opener_root, Some(&RunningSnapshot::default()))
        );
    }

    #[test]
    fn detached_direct_pty_opener_has_ended_even_without_a_running_snapshot() {
        let mut child = super::super::tests::child();
        child.opener.mux_name = None;
        child.opener.backend = None;
        assert!(child.opener.ended_detached(&child.opener_root, None));
        let mut blind = RunningSnapshot::default();
        blind.blind = true;
        assert!(
            child
                .opener
                .ended_detached(&child.opener_root, Some(&blind))
        );
    }

    #[test]
    fn invalid_background_handle_fires_once_without_overwriting_held_result() {
        let mut child = super::super::tests::child();
        let mut watch = watch(1, "s-00000001", &child);
        child.invalidated = true;
        let target = SessionTarget {
            epoch: ServiceEpoch::current(),
            key: child.key.clone(),
            generation: 1,
        };
        assert!(!child.matches(&target, child.file.as_deref()));
        watch.finish(WaitResult::new(false, STALE.into(), "invalid"));
        watch.finish(WaitResult::new(true, "later exit".into(), "exited"));
        let Some(CtlCommand::Deliver { text, details, .. }) =
            watch.take_command("s-00000001", &child.opener_root, true)
        else {
            panic!("invalid handle must deliver its first result");
        };
        assert_eq!(text, STALE);
        assert_eq!(details.unwrap()["status"], "invalid");
        assert!(
            watch
                .take_command("s-00000001", &child.opener_root, true)
                .is_none()
        );
    }

    #[test]
    fn background_attachment_ownership_transfers_from_child_to_delivery_ack() {
        let mut tools = AgentTools::default();
        let handle = insert(&mut tools.children, 1);
        let child_key = tools.children.entries[&handle].key.clone();
        let opener_key = tools.children.entries[&handle].opener.key.clone();
        let watch = watch(1, &handle, &tools.children.entries[&handle]);
        tools
            .children
            .arm_background(&handle, watch)
            .expect("armed");
        assert!(tools.controller_needed(&child_key));
        assert!(tools.controller_needed(&opener_key));
        let waiter = tools
            .children
            .entries
            .get_mut(&handle)
            .unwrap()
            .background
            .as_mut()
            .unwrap()
            .take_waiter()
            .expect("polling waiter");
        assert!(
            tools.controller_needed(&child_key),
            "headless attach admission sees ownership during polling"
        );
        tools
            .children
            .entries
            .get_mut(&handle)
            .unwrap()
            .background
            .as_mut()
            .unwrap()
            .state = WatchState::Watching(Some(waiter));
        tools
            .children
            .entries
            .get_mut(&handle)
            .unwrap()
            .background
            .as_mut()
            .unwrap()
            .finish(WaitResult::new(true, "done".into(), "completed"));
        assert!(
            !tools.controller_needed(&child_key),
            "held text no longer needs the child"
        );
        assert!(tools.controller_needed(&opener_key));
        let target = SessionTarget {
            epoch: ServiceEpoch::current(),
            key: opener_key.clone(),
            generation: 1,
        };
        tools.reply_acks.push((target, 42));
        tools.children.entries.get_mut(&handle).unwrap().background = None;
        assert!(
            tools.controller_needed(&opener_key),
            "delivery ACK owns headless opener"
        );
        tools.acknowledge_reply(&opener_key, 42);
        assert!(!tools.controller_needed(&opener_key));
    }
}
