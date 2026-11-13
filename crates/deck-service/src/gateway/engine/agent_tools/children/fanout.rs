//! A fan-out is one pending tool and one root-fenced, one-shot combined result.
use super::*;
use crate::gateway::protocol::{ServerEvent, Split};
use deck_core::ctl::CtlCommand;

#[derive(Deserialize)]
struct Args {
    tasks: Vec<Task>,
    #[serde(default = "yes")]
    worktree: bool,
    base: Option<String>,
    cwd: Option<PathBuf>,
    #[serde(default = "yes")]
    forward_questions: bool,
    timeout_seconds: Option<u64>,
}
fn yes() -> bool {
    true
}
#[derive(Deserialize)]
struct Task {
    prompt: String,
    title: Option<String>,
    model: Option<String>,
    branch: Option<String>,
}
#[derive(Default)]
pub(in crate::gateway::engine) struct Groups {
    groups: HashMap<String, Group>,
}
struct Group {
    request: u64,
    opener: SessionTarget,
    root: PathBuf,
    opener_root: PathBuf,
    cwd: PathBuf,
    validated: bool,
    worktree: bool,
    base: Option<String>,
    forward: bool,
    timeout: Option<u64>,
    entries: Vec<Entry>,
    returned: bool,
}
struct Entry {
    id: u64,
    task: Task,
    handle: Option<String>,
    branch: Option<String>,
    cwd: PathBuf,
    started: bool,
    deadline: Option<Instant>,
    waiter: Option<Box<Waiter>>,
    outcome: Option<(&'static str, String)>,
    attention: bool,
}
impl Groups {
    // Validation reserves a group slot, but can only release creation for the
    // same still-pending opener conversation that dispatched the worker.
    fn validated(
        &mut self,
        request: u64,
        opener: &SessionTarget,
        root: &Path,
        pending: Option<(&SessionTarget, &Path)>,
    ) -> Option<String> {
        if pending != Some((opener, root)) {
            return None;
        }
        self.groups.iter_mut().find_map(|(id, group)| {
            if group.request != request
                || &group.opener != opener
                || group.opener_root != root
                || group.validated
            {
                return None;
            }
            group.validated = true;
            Some(id.clone())
        })
    }
    pub(super) fn waiter_mut(&mut self, id: u64) -> Option<&mut Waiter> {
        self.groups
            .values_mut()
            .flat_map(|g| &mut g.entries)
            .find(|e| e.id == id)
            .and_then(|e| e.waiter.as_deref_mut())
    }
    pub(in crate::gateway::engine) fn contains(&self, id: u64) -> bool {
        self.groups
            .values()
            .any(|g| g.entries.iter().any(|e| e.id == id))
    }
    pub(super) fn needs_controller(&self, key: &SessionKey, registry: &Registry) -> bool {
        self.groups.values().any(|g| {
            &g.opener.key == key
                || g.entries.iter().any(|e| {
                    e.handle
                        .as_ref()
                        .and_then(|h| registry.entries.get(h))
                        .is_some_and(|c| &c.key == key)
                })
        })
    }
    pub(super) fn clear(&mut self) {
        self.groups.clear();
    }
}
impl Engine {
    pub(super) fn fanout_wait_failed(&mut self, request: u64, text: &str) -> bool {
        let Some(entry) = self
            .agent_tools
            .fan_out
            .groups
            .values_mut()
            .flat_map(|g| &mut g.entries)
            .find(|e| e.id == request)
        else {
            return false;
        };
        if entry.outcome.is_none() {
            entry.outcome = Some(("failed", cap(text, 2048)));
        }
        true
    }
    pub(in crate::gateway::engine) fn begin_fan_out(
        &mut self,
        request: u64,
        args: serde_json::Value,
        cwd: PathBuf,
    ) -> Result<(), String> {
        let args: Args = serde_json::from_value(args).map_err(|e| e.to_string())?;
        if !(1..=8).contains(&args.tasks.len()) {
            return Err("tasks must contain 1 to 8 tasks".into());
        }
        if args.tasks.iter().any(|t| {
            t.prompt.trim().is_empty()
                || t.prompt.len() > 16 * 1024
                || t.title.as_ref().is_some_and(|s| s.chars().count() > 80)
        }) {
            return Err(
                "Each task needs a prompt of 1..16384 bytes and title of at most 80 characters"
                    .into(),
            );
        }
        if args
            .timeout_seconds
            .is_some_and(|s| !(1..=86400).contains(&s))
        {
            return Err("timeout_seconds must be 1..86400".into());
        }
        let cwd = args.cwd.unwrap_or(cwd);
        if !cwd.is_absolute() {
            return Err("cwd must be an existing absolute directory".into());
        }
        let pending = &self.agent_tools.pending[&request];
        // Keep the admitted identity unchanged for the worker completion fence.
        let root = pending
            .root
            .clone()
            .ok_or("The opener has no session file")?;
        if self
            .agent_tools
            .fan_out
            .groups
            .values()
            .filter(|g| g.opener_root == root || g.root == root)
            .count()
            >= 2
        {
            return Err("At most two active fan-outs per opener".into());
        }
        let id = format!("f-{request:016x}");
        let mut ids = std::collections::HashSet::new();
        let mut entries = Vec::new();
        for task in args.tasks {
            let child_id = loop {
                let mut bytes = [0; 8];
                getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
                let n = u64::from_le_bytes(bytes);
                if n != request
                    && !self.agent_tools.pending.contains_key(&n)
                    && !self.agent_tools.background_request(n)
                    && !self.agent_tools.fan_out.contains(n)
                    && ids.insert(n)
                {
                    break n;
                }
            };
            entries.push(Entry {
                id: child_id,
                task,
                handle: None,
                branch: None,
                cwd: cwd.clone(),
                started: false,
                deadline: None,
                waiter: None,
                outcome: None,
                attention: false,
            });
        }
        let opener = pending.target.clone();
        let validation_cwd = cwd.clone();
        let validation_root = root.clone();
        self.agent_tools.fan_out.groups.insert(
            id,
            Group {
                request,
                opener: pending.target.clone(),
                opener_root: root.clone(),
                root,
                cwd,
                validated: false,
                worktree: args.worktree,
                base: args.base,
                forward: args.forward_questions,
                timeout: args.timeout_seconds,
                entries,
                returned: false,
            },
        );
        self.spawn_worker("agent-fanout-validation", move || {
            Fin::AgentFanoutValidated {
                request,
                opener,
                result: validate_cwd(&validation_cwd).map(|()| canonical(&validation_root)),
                root: validation_root,
            }
        });
        Ok(())
    }
    pub(in crate::gateway::engine) fn fanout_validated(
        &mut self,
        request: u64,
        opener: SessionTarget,
        root: PathBuf,
        result: Result<PathBuf, String>,
    ) {
        if !self.agent_tool_current(request) {
            return;
        }
        let pending = self
            .agent_tools
            .pending
            .get(&request)
            .and_then(|p| p.root.as_deref().map(|root| (&p.target, root)));
        let Some(id) = self
            .agent_tools
            .fan_out
            .validated(request, &opener, &root, pending)
        else {
            return;
        };
        let result = result.and_then(|canonical_root| {
            if self
                .agent_tools
                .fan_out
                .groups
                .iter()
                .filter(|(other_id, g)| *other_id != &id && g.root == canonical_root)
                .count()
                >= 2
            {
                return Err("At most two active fan-outs per opener".into());
            }
            self.agent_tools
                .fan_out
                .groups
                .get_mut(&id)
                .expect("fan-out")
                .root = canonical_root;
            Ok(())
        });
        if let Err(error) = result {
            self.agent_tools.fan_out.groups.remove(&id);
            self.finish_agent_tool(request, false, error);
        }
        self.waker.now();
    }
    pub(in crate::gateway::engine) fn fanout_open_current(&self, request: u64) -> Option<bool> {
        self.agent_tools
            .fan_out
            .groups
            .values()
            .find(|g| g.entries.iter().any(|e| e.id == request))
            .map(|g| self.agent_tool_current(g.request))
    }
    pub(in crate::gateway::engine) fn fanout_worktree_opened(
        &mut self,
        request: u64,
        mut event: ServerEvent,
        result: Result<(PathBuf, String, String), String>,
    ) -> bool {
        let Some(id) = self.agent_tools.fan_out.groups.iter().find_map(|(id, g)| {
            g.entries
                .iter()
                .any(|e| e.id == request)
                .then(|| id.clone())
        }) else {
            return false;
        };
        match result {
            Err(error) => self.fanout_open_failed(&id, request, error),
            Ok((path, branch, _)) => {
                let g = self
                    .agent_tools
                    .fan_out
                    .groups
                    .get_mut(&id)
                    .expect("fan-out");
                let entry = g
                    .entries
                    .iter_mut()
                    .find(|e| e.id == request)
                    .expect("entry");
                entry.branch = Some(branch);
                entry.cwd = path.clone();
                if let ServerEvent::AgentOpenSession { cwd, .. } = &mut event {
                    *cwd = path;
                }
                self.fanout_send_open(&id, request, event);
            }
        }
        true
    }
    fn fanout_open_failed(&mut self, id: &str, request: u64, error: String) {
        if let Some(entry) = self
            .agent_tools
            .fan_out
            .groups
            .get_mut(id)
            .and_then(|g| g.entries.iter_mut().find(|e| e.id == request))
        {
            entry.outcome = Some(("failed", cap(&error, 2048)));
            entry.deadline = None;
        }
        self.waker.now();
    }
    fn fanout_send_open(&mut self, id: &str, request: u64, event: ServerEvent) {
        if self.to_desktop(&event) {
            if let Some(entry) = self
                .agent_tools
                .fan_out
                .groups
                .get_mut(id)
                .and_then(|g| g.entries.iter_mut().find(|e| e.id == request))
            {
                entry.deadline = Some(Instant::now() + DELIVERY);
            }
        } else {
            self.fanout_open_failed(id, request, "OmpDeck's window is not open".into());
        }
    }
    pub(in crate::gateway::engine) fn fanout_open_done(
        &mut self,
        request: u64,
        target: &SessionTarget,
        ok: bool,
        text: String,
        opened: Option<SessionTarget>,
    ) -> bool {
        let Some(id) = self.agent_tools.fan_out.groups.iter().find_map(|(id, g)| {
            (&g.opener == target && g.entries.iter().any(|e| e.id == request)).then(|| id.clone())
        }) else {
            return false;
        };
        if !self.agent_tool_current(self.agent_tools.fan_out.groups[&id].request) {
            return true;
        }
        let mut group = self
            .agent_tools
            .fan_out
            .groups
            .remove(&id)
            .expect("fan-out");
        let entry = group
            .entries
            .iter_mut()
            .find(|e| e.id == request)
            .expect("entry");
        if entry.handle.is_some() || entry.outcome.is_some() || !entry.started {
            self.agent_tools.fan_out.groups.insert(id, group);
            return true;
        }
        let result = (|| {
            if !ok {
                return Err(text);
            }
            let opened = opened.ok_or("OmpDeck did not identify the opened session")?;
            let pending = self
                .agent_tools
                .pending
                .get_mut(&group.request)
                .ok_or("The opener ended")?;
            pending.opens_session = true;
            pending.open_prompt = Some(entry.task.prompt.clone());
            pending.forward_questions = group.forward;
            let handle = self
                .register_agent_child(group.request, &opened)?
                .ok_or("The child did not provide a session handle")?;
            let child = self
                .agent_tools
                .children
                .entries
                .get_mut(&handle)
                .expect("registered child");
            child.fan_out = Some(id.clone());
            let mut waiter = Waiter::new(
                handle.clone(),
                child,
                child.last.clone(),
                group.timeout,
                group
                    .timeout
                    .map(|s| Instant::now() + Duration::from_secs(s)),
                false,
                Instant::now(),
            );
            waiter.background_watch = true;
            entry.waiter = Some(Box::new(waiter));
            entry.handle = Some(handle);
            entry.deadline = None;
            Ok(())
        })();
        if let Err(error) = result {
            entry.outcome = Some(("failed", cap(&error, 2048)));
            entry.deadline = None;
        }
        self.agent_tools.fan_out.groups.insert(id, group);
        self.waker.now();
        true
    }
    pub(in crate::gateway::engine) fn drop_fan_out_opener(&mut self, key: &SessionKey) {
        let mut release = Vec::new();
        self.agent_tools.fan_out.groups.retain(|_, g| {
            let keep = &g.opener.key != key;
            if !keep {
                for e in &g.entries {
                    if let Some(c) = e
                        .handle
                        .as_ref()
                        .and_then(|h| self.agent_tools.children.entries.get(h))
                    {
                        release.push(c.key.clone());
                    }
                }
            }
            keep
        });
        for key in release {
            self.release_idle_child_attachment(&key);
        }
    }
    pub(in crate::gateway::engine) fn step_fan_outs(&mut self, now: Instant) {
        let ids: Vec<_> = self.agent_tools.fan_out.groups.keys().cloned().collect();
        for id in ids {
            let mut group = self
                .agent_tools
                .fan_out
                .groups
                .remove(&id)
                .expect("fan-out");
            let ended = if !group.returned {
                !self.agent_tool_current(group.request)
            } else {
                self.sess.find(&group.opener.key).is_some_and(|i| {
                    let m = &self.sess.managed[i];
                    m.exited() || m.ctl.session_path().is_some_and(|p| p != group.opener_root)
                }) || group
                    .entries
                    .iter()
                    .filter_map(|e| e.handle.as_ref())
                    .filter_map(|h| self.agent_tools.children.entries.get(h))
                    .any(|c| self.background_opener_ended(c))
            };
            if ended {
                for e in &group.entries {
                    if let Some(c) = e
                        .handle
                        .as_ref()
                        .and_then(|h| self.agent_tools.children.entries.get(h))
                    {
                        let key = c.key.clone();
                        self.release_idle_child_attachment(&key);
                    }
                }
                continue;
            }
            if !group.validated {
                self.agent_tools.fan_out.groups.insert(id, group);
                continue;
            }
            for entry in &mut group.entries {
                if entry.handle.is_none() && entry.deadline.is_some_and(|d| now >= d) {
                    entry.outcome = Some(("failed", "Opening the child timed out".into()));
                    entry.deadline = None;
                }
                if entry.outcome.is_some() {
                    continue;
                }
                if let Some(mut waiter) = entry.waiter.take() {
                    let result = if waiter.until.is_some_and(|d| now >= d) {
                        waiter.status = "timeout";
                        Some((true, "Initial turn timed out".into()))
                    } else {
                        self.step_child_waiter(entry.id, &mut waiter, now)
                    };
                    if let Some((_, text)) = result {
                        if waiter.status == "attention" {
                            if !entry.attention
                                && let Some(index) = self.background_delivery_target(&group.root)
                            {
                                self.deliver_child_message(
                                    index,
                                    CtlCommand::Deliver {
                                        root: group.root.clone(),
                                        title: format!(
                                            "Fan-out {id}: {} needs your attention",
                                            entry.task.title.as_deref().unwrap_or("child")
                                        ),
                                        text: String::new(),
                                        details: None,
                                    },
                                );
                                entry.attention = true;
                            }
                        } else {
                            let status = match waiter.status {
                                "completed" => "done",
                                "timeout" => "timeout",
                                "exited" => "exited",
                                _ => "failed",
                            };
                            let reply = waiter.scan.reply.as_deref().unwrap_or(&text);
                            entry.outcome = Some((status, cap(reply, 2048)));
                        }
                    }
                    entry.waiter = Some(waiter);
                }
            }
            if !group.returned {
                if let Some(entry) = group.entries.iter_mut().find(|e| !e.started) {
                    entry.started = true;
                    let event = ServerEvent::AgentOpenSession {
                        request: entry.id,
                        opener: group.opener.clone(),
                        cwd: group.cwd.clone(),
                        prompt: Some(entry.task.prompt.clone()),
                        model: entry.task.model.clone(),
                        placement: Split::None,
                        title: entry.task.title.clone(),
                    };
                    let request = entry.id;
                    let worktree = group
                        .worktree
                        .then(|| super::super::worktrees::OpenWorktree {
                            branch: entry.task.branch.clone(),
                            base: group.base.clone(),
                        });
                    self.agent_tools.fan_out.groups.insert(id.clone(), group);
                    if let Some(args) = worktree {
                        self.begin_agent_worktree_open(request, event, args);
                    } else {
                        self.fanout_send_open(&id, request, event);
                    }
                    continue;
                }
                if group
                    .entries
                    .iter()
                    .all(|e| e.handle.is_some() || e.outcome.is_some())
                {
                    let opened = group.entries.iter().filter(|e| e.handle.is_some()).count();
                    let mut lines = Vec::new();
                    for (i, e) in group.entries.iter().enumerate() {
                        lines.push(if let Some(h) = &e.handle {
                            format!(
                                "{}. {h} — {} — {}",
                                i + 1,
                                e.task.title.as_deref().unwrap_or("child"),
                                e.branch
                                    .clone()
                                    .unwrap_or_else(|| e.cwd.display().to_string())
                            )
                        } else {
                            format!(
                                "{}. failed: {}",
                                i + 1,
                                e.outcome
                                    .as_ref()
                                    .map_or("Opening failed", |o| o.1.as_str())
                            )
                        });
                    }
                    lines.push(
                        "Their combined result arrives as one message when all have finished."
                            .into(),
                    );
                    group.returned = true;
                    self.finish_agent_tool(group.request, opened > 0, lines.join("\n"));
                    if opened == 0 {
                        continue;
                    }
                }
            }
            if group.returned
                && group.entries.iter().all(|e| e.outcome.is_some())
                && let Some(index) = self.background_delivery_target(&group.root)
            {
                let done = group
                    .entries
                    .iter()
                    .filter(|e| e.outcome.as_ref().is_some_and(|o| o.0 == "done"))
                    .count();
                let headers: Vec<_> = group
                    .entries
                    .iter()
                    .map(|e| {
                        let status = e.outcome.as_ref().expect("ended").0;
                        cap(
                            &format!(
                                "{} — {} — {status} — {}",
                                e.handle.as_deref().unwrap_or("(not opened)"),
                                e.task.title.as_deref().unwrap_or("child"),
                                e.branch.as_deref().unwrap_or("(no branch)")
                            ),
                            1024,
                        )
                    })
                    .collect();
                // Reserve every child's identity/status before allocating reply space.
                let mut remaining = (16usize * 1024)
                    .saturating_sub(headers.iter().map(|h| h.len() + 3).sum::<usize>());
                let mut text = String::with_capacity(16 * 1024);
                for (i, (entry, header)) in group.entries.iter().zip(&headers).enumerate() {
                    let reply = cap(
                        &entry.outcome.as_ref().expect("ended").1,
                        2048.min(remaining / (group.entries.len() - i)),
                    );
                    remaining = remaining.saturating_sub(reply.len());
                    text.push_str(header);
                    text.push('\n');
                    text.push_str(&reply);
                    text.push_str("\n\n");
                }
                self.deliver_child_message(
                    index,
                    CtlCommand::Deliver {
                        root: group.root.clone(),
                        title: format!(
                            "Fan-out {id} finished ({done}/{} done)",
                            group.entries.len()
                        ),
                        text,
                        details: Some(serde_json::json!({"fan_out":id})),
                    },
                );
                for e in &group.entries {
                    if let Some(c) = e
                        .handle
                        .as_ref()
                        .and_then(|h| self.agent_tools.children.entries.get(h))
                    {
                        let key = c.key.clone();
                        self.release_idle_child_attachment(&key);
                    }
                }
                continue;
            }
            self.agent_tools.fan_out.groups.insert(id, group);
        }
        if !self.agent_tools.fan_out.groups.is_empty() {
            self.wake_within(POLL);
        }
    }
}
fn validate_cwd(cwd: &Path) -> Result<(), String> {
    if cwd.is_absolute() && cwd.is_dir() {
        Ok(())
    } else {
        Err("cwd must be an existing absolute directory".into())
    }
}
fn cap(text: &str, limit: usize) -> String {
    const MARK: &str = "\n[… truncated]";
    if text.len() <= limit {
        text.to_owned()
    } else {
        format!(
            "{}{MARK}",
            utf8_prefix(text, limit.saturating_sub(MARK.len()))
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_validation_rejects_missing_cwd_and_non_directory() {
        let dir = tempfile::tempdir().expect("temporary fan-out cwd");
        assert!(validate_cwd(dir.path()).is_ok());
        assert!(validate_cwd(&dir.path().join("missing")).is_err());
        let file = dir.path().join("file");
        std::fs::write(&file, b"not a directory").expect("temporary file");
        assert!(validate_cwd(&file).is_err());
        assert!(validate_cwd(Path::new("relative")).is_err());
    }

    #[test]
    fn validation_completion_is_fenced_by_pending_request_opener_and_root() {
        let opener = SessionTarget {
            key: SessionKey::host("omp-deck-test-fanout", "opener"),
            epoch: crate::controller::ServiceEpoch::current(),
            generation: 1,
        };
        let root = PathBuf::from("normalized-session.jsonl");
        let mut groups = Groups::default();
        groups.groups.insert(
            "fanout".into(),
            Group {
                request: 17,
                opener: opener.clone(),
                root: root.clone(),
                opener_root: root.clone(),
                cwd: PathBuf::new(),
                validated: false,
                worktree: false,
                base: None,
                forward: true,
                timeout: None,
                entries: Vec::new(),
                returned: false,
            },
        );
        let mut replacement = opener.clone();
        replacement.generation += 1;
        let other_root = Path::new("replacement-session.jsonl");
        // Delayed success after cancellation or a conversation switch cannot
        // release child creation, even before the next actor step.
        assert!(groups.validated(17, &opener, &root, None).is_none());
        assert!(
            groups
                .validated(17, &opener, &root, Some((&replacement, &root)))
                .is_none()
        );
        assert!(
            groups
                .validated(17, &opener, &root, Some((&opener, other_root)))
                .is_none()
        );
        assert!(
            groups
                .validated(18, &opener, &root, Some((&opener, &root)))
                .is_none()
        );
        assert!(
            groups
                .validated(17, &replacement, &root, Some((&replacement, &root)))
                .is_none()
        );
        assert!(
            groups
                .validated(17, &opener, other_root, Some((&opener, other_root)))
                .is_none()
        );
        assert!(!groups.groups["fanout"].validated);
        assert_eq!(
            groups.validated(17, &opener, &root, Some((&opener, &root))),
            Some("fanout".into())
        );
        assert!(groups.groups["fanout"].validated);
        assert!(
            groups
                .validated(17, &opener, &root, Some((&opener, &root)))
                .is_none()
        );
        groups.clear();
        assert!(
            groups
                .validated(17, &opener, &root, Some((&opener, &root)))
                .is_none()
        );
    }

    #[test]
    fn combined_reply_caps_include_the_utf8_safe_marker() {
        let reply = "🦀".repeat(4096);
        let child = cap(&reply, 2048);
        assert!(child.len() <= 2048);
        assert!(child.ends_with("[… truncated]"));
        let total = cap(&format!("{}{}", reply, reply), 16 * 1024);
        assert!(total.len() <= 16 * 1024);
        assert!(total.ends_with("[… truncated]"));
        assert_eq!(cap("complete", 2048), "complete");
    }
}
