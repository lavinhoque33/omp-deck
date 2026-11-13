//! Session-bound extension tools. Requests are ephemeral and never replayed on restart.
use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use deck_core::ctl::{CtlCommand, ToolEvent, ToolRequest};
use serde::Deserialize;

use super::{Engine, Fin};
use crate::controller::SessionTarget;
use crate::gateway::protocol::{ReviewReply, Role, ServerEvent, Split};

pub(super) mod children;
mod worktrees;

const CAP: usize = 4;
const NOTIFY_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Default)]
pub(super) struct AgentTools {
    pending: HashMap<u64, Pending>,
    notified: HashMap<SessionTarget, Instant>,
    children: children::Registry,
    operations: HashMap<u64, children::Operation>,
    reply_acks: Vec<(SessionTarget, u64)>,
    background_acks: Vec<(SessionTarget, u64)>,
    deferred_deliveries: VecDeque<(SessionTarget, CtlCommand)>,
    orchestration: children::orchestration::State,
    fan_out: children::fanout::Groups,
}

struct Pending {
    target: SessionTarget,
    tool_id: u64,
    root: Option<PathBuf>,
    deadline: Option<Instant>,
    review: Option<ServerEvent>,
    answering: bool,
    opens_session: bool,
    open_prompt: Option<String>,
    forward_questions: bool,
    worktree_result: Option<String>,
}

#[derive(Deserialize)]
struct OpenArgs {
    prompt: Option<String>,
    cwd: Option<PathBuf>,
    model: Option<String>,
    placement: Option<String>,
    title: Option<String>,
    worktree: Option<worktrees::OpenWorktree>,
    #[serde(default = "forward_questions_default")]
    forward_questions: bool,
}
fn forward_questions_default() -> bool {
    true
}
#[derive(Deserialize)]
struct NotifyArgs {
    title: String,
    body: Option<String>,
    #[serde(default = "info")]
    level: String,
}
fn info() -> String {
    "info".to_owned()
}
#[derive(Deserialize)]
struct ChangesArgs {
    path: Option<String>,
}
#[derive(Deserialize)]
struct ReviewArgs {
    summary: Option<String>,
    paths: Option<Vec<String>>,
}
impl AgentTools {
    fn delivery_awaiting_ack(&self, target: &SessionTarget) -> bool {
        self.reply_acks.iter().any(|(pending, _)| pending == target)
    }

    fn delivery_pending(&self, target: &SessionTarget) -> bool {
        self.delivery_awaiting_ack(target)
            || self
                .deferred_deliveries
                .iter()
                .any(|(pending, _)| pending == target)
    }

    fn admit_delivery(
        &mut self,
        target: SessionTarget,
        command: CtlCommand,
        restoring: bool,
    ) -> Option<(SessionTarget, CtlCommand)> {
        if restoring {
            self.deferred_deliveries.push_back((target, command));
            None
        } else {
            Some((target, command))
        }
    }

    fn at_capacity(&self, target: &SessionTarget) -> bool {
        self.pending
            .values()
            .filter(|p| &p.target == target)
            .count()
            >= CAP
    }
    fn cancel(&mut self, target: &SessionTarget, tool_id: u64) -> Option<ServerEvent> {
        let request = self.pending.iter().find_map(|(&request, p)| {
            (&p.target == target && p.tool_id == tool_id).then_some(request)
        })?;
        let p = self.pending.remove(&request)?;
        p.review.map(|_| ServerEvent::AgentReviewCancel { request })
    }
    fn notify_allowed(&mut self, target: &SessionTarget, now: Instant) -> bool {
        if self
            .notified
            .get(target)
            .is_some_and(|t| now.saturating_duration_since(*t) < NOTIFY_INTERVAL)
        {
            return false;
        }
        self.notified.insert(target.clone(), now);
        true
    }
}

impl Engine {
    pub(super) fn step_agent_tools(&mut self, now: Instant) {
        self.step_child_persistence(now);
        let mut events = Vec::new();
        for m in &mut self.sess.managed {
            if let Some(link) = &mut m.live {
                if !link.client.connected() {
                    link.client.take_tool_requests();
                    continue;
                }
                events.extend(
                    link.client
                        .take_tool_requests()
                        .into_iter()
                        .map(|event| (m.ctl.target().clone(), event)),
                );
            }
        }
        let sessions = &self.sess;
        self.agent_tools.reply_acks.retain(|(target, _)| {
            sessions.find(&target.key).is_some_and(|i| {
                let m = &sessions.managed[i];
                m.ctl.target() == target
                    && m.live.as_ref().is_some_and(|link| link.client.connected())
            })
        });
        self.agent_tools
            .background_acks
            .retain(|ack| self.agent_tools.reply_acks.contains(ack));
        let expired: Vec<_> = self
            .agent_tools
            .pending
            .iter()
            .filter_map(|(&id, p)| {
                let valid = self.agent_tool_current(id);
                (!valid || p.deadline.is_some_and(|d| d <= now)).then_some(id)
            })
            .collect();
        for id in expired {
            self.finish_agent_tool(
                id,
                false,
                "OmpDeck request expired or the session ended".to_owned(),
            );
        }
        for (target, event) in events {
            match event {
                ToolEvent::Request(request) => self.route_agent_tool(target, request, now),
                ToolEvent::Cancel(id) => {
                    let request = self.agent_tools.pending.iter().find_map(|(&request, p)| {
                        (p.target == target && p.tool_id == id).then_some(request)
                    });
                    if let Some(request) = request {
                        self.child_operation_finished(request);
                    }
                    if let Some(event) = self.agent_tools.cancel(&target, id) {
                        self.to_guis(&event);
                    }
                }
            }
        }
        self.step_child_tools(now);
        self.flush_child_deliveries();
        if let Some(deadline) = self
            .agent_tools
            .pending
            .values()
            .filter_map(|p| p.deadline)
            .min()
        {
            self.wake_within(deadline.saturating_duration_since(now));
        }
    }

    fn agent_tool_current(&self, request: u64) -> bool {
        self.agent_tools.pending.get(&request).is_some_and(|p| {
            self.sess.find(&p.target.key).is_some_and(|i| {
                let m = &self.sess.managed[i];
                matches_session(
                    m.ctl.target(),
                    m.ctl.session_path(),
                    &p.target,
                    p.root.as_deref(),
                ) && !m.exited()
                    && m.live.as_ref().is_some_and(|link| link.client.connected())
            })
        })
    }

    fn tool_result(&mut self, target: &SessionTarget, id: u64, ok: bool, text: String) {
        if let Some(i) = self.sess.find(&target.key) {
            let m = &self.sess.managed[i];
            if m.ctl.target() == target
                && let Some(link) = &m.live
            {
                let rid = link.client.send(CtlCommand::ToolResult {
                    id,
                    ok,
                    text,
                    details: None,
                });
                // Keep a tool-only issuer attached until the extension received its result.
                self.agent_tools.reply_acks.push((target.clone(), rid));
            }
        }
    }

    pub(super) fn finish_agent_tool(&mut self, request: u64, ok: bool, text: String) {
        self.child_operation_finished(request);
        let Some(p) = self.agent_tools.pending.remove(&request) else {
            return;
        };
        if p.review.is_some() {
            self.to_guis(&ServerEvent::AgentReviewCancel { request });
        }
        self.tool_result(&p.target, p.tool_id, ok, text);
    }

    fn route_agent_tool(&mut self, target: SessionTarget, req: ToolRequest, now: Instant) {
        let Some(i) = self.sess.find(&target.key) else {
            return;
        };
        let m = &self.sess.managed[i];
        if !m.live.as_ref().is_some_and(|link| link.client.connected()) {
            return;
        }
        if !matches_session(
            m.ctl.target(),
            m.ctl.session_path(),
            &target,
            req.root.as_deref(),
        ) || m.exited()
        {
            self.tool_result(&target, req.id, false, "The session changed".to_owned());
            return;
        }
        if req.name != "fan_out" && self.agent_tools.at_capacity(&target) {
            self.tool_result(
                &target,
                req.id,
                false,
                "too many pending OmpDeck requests".to_owned(),
            );
            return;
        }
        let cwd = m.current_cwd().to_path_buf();
        let request = loop {
            let mut bytes = [0; 8];
            if let Err(error) = getrandom::fill(&mut bytes) {
                self.tool_result(
                    &target,
                    req.id,
                    false,
                    format!("Cannot identify OmpDeck request: {error}"),
                );
                return;
            }
            let request = u64::from_le_bytes(bytes);
            if !self.agent_tools.pending.contains_key(&request)
                && !self.agent_tools.background_request(request)
            {
                break request;
            }
        };
        self.agent_tools.pending.insert(
            request,
            Pending {
                target: target.clone(),
                tool_id: req.id,
                root: req.root,
                deadline: None,
                review: None,
                answering: false,
                opens_session: false,
                open_prompt: None,
                forward_questions: true,
                worktree_result: None,
            },
        );
        let result: Result<Option<ServerEvent>, String> = (|| {
            Ok(match req.name.as_str() {
                "open_session" => {
                    let a: OpenArgs =
                        serde_json::from_value(req.args).map_err(|e| e.to_string())?;
                    let cwd = a.cwd.unwrap_or(cwd);
                    if !cwd.is_absolute() || !cwd.is_dir() {
                        return Err("cwd must be an existing absolute directory".to_owned());
                    }
                    if a.prompt.as_ref().is_some_and(|p| p.len() > 16 * 1024)
                        || a.title.as_ref().is_some_and(|t| t.chars().count() > 80)
                    {
                        return Err("prompt or title is too long".to_owned());
                    }
                    let placement = match a.placement.as_deref().unwrap_or("tab") {
                        "tab" => Split::None,
                        "split_right" => Split::Right,
                        "split_down" => Split::Down,
                        _ => return Err("invalid placement".to_owned()),
                    };
                    let pending = self.agent_tools.pending.get_mut(&request).expect("pending");
                    pending.opens_session = true;
                    pending.open_prompt.clone_from(&a.prompt);
                    pending.forward_questions = a.forward_questions;
                    let event = ServerEvent::AgentOpenSession {
                        request,
                        opener: target.clone(),
                        cwd,
                        prompt: a.prompt,
                        model: a.model,
                        placement,
                        title: a.title,
                    };
                    if let Some(worktree) = a.worktree {
                        self.begin_agent_worktree_open(request, event, worktree);
                        None
                    } else {
                        Some(event)
                    }
                }
                "notify" => {
                    let a: NotifyArgs =
                        serde_json::from_value(req.args).map_err(|e| e.to_string())?;
                    if a.title.chars().count() > 120
                        || a.body.as_ref().is_some_and(|b| b.len() > 2048)
                        || !matches!(a.level.as_str(), "info" | "success" | "warning" | "error")
                    {
                        return Err("invalid agent notification".to_owned());
                    }
                    let text = if !self.cfg.notify.agent_messages {
                        "Agent notifications are turned off"
                    } else if !self.agent_tools.notify_allowed(&target, now) {
                        "Rate limited; try later"
                    } else {
                        // Refresh the canonical visibility/config before emitting an agent event.
                        self.step_notifications(now);
                        let effects = self.notify.center.agent_message(
                            target.clone(),
                            a.title,
                            a.body,
                            a.level,
                        );
                        self.apply_effects(effects);
                        "Posted"
                    };
                    self.finish_agent_tool(request, true, text.to_owned());
                    None
                }
                "show_changes" => {
                    let a: ChangesArgs =
                        serde_json::from_value(req.args).map_err(|e| e.to_string())?;
                    if a.path.as_deref().is_some_and(|p| !relative_path(p)) {
                        return Err("path must be repository-relative".to_owned());
                    }
                    Some(ServerEvent::AgentShowChanges {
                        request,
                        target: target.clone(),
                        path: a.path,
                    })
                }
                "request_review" => {
                    let a: ReviewArgs =
                        serde_json::from_value(req.args).map_err(|e| e.to_string())?;
                    let paths = a.paths.unwrap_or_default();
                    if a.summary.as_ref().is_some_and(|s| s.len() > 4096)
                        || paths.len() > 50
                        || paths.iter().any(|p| !relative_path(p))
                    {
                        return Err("invalid review summary or paths".to_owned());
                    }
                    Some(ServerEvent::AgentReviewRequest {
                        request,
                        activate: false,
                        target: target.clone(),
                        summary: a.summary,
                        paths,
                    })
                }
                "fan_out" => {
                    self.begin_fan_out(request, req.args, cwd)?;
                    None
                }
                "send_prompt" | "wait_session" => {
                    self.begin_child_tool(request, &req.name, req.args, now)?;
                    None
                }
                "answer_question" => {
                    self.begin_answer_question(request, req.args, now)?;
                    None
                }
                "finish_worktree" => {
                    self.begin_child_finish(request, req.args, now)?;
                    None
                }
                "report" => {
                    self.begin_child_report(request, req.args, now)?;
                    None
                }
                "sessions" => {
                    let rows = self.agent_session_rows(&target);
                    self.spawn_worker("agent-sessions", move || {
                        let text = rows
                            .into_iter()
                            .map(|(mut row, path)| {
                                let snippet = path
                                    .and_then(|p| {
                                        let mut r = deck_core::status::SessionStatsReader::new(p);
                                        r.refresh().ok()?;
                                        let stats = r.stats();
                                        if row.contains("model: unknown")
                                            && let Some(model) = &stats.model
                                        {
                                            row = row.replacen(
                                                "model: unknown",
                                                &format!("model: {model}"),
                                                1,
                                            );
                                        }
                                        stats.last_assistant_text.clone()
                                    })
                                    .unwrap_or_default();
                                row.push_str(" | last reply: ");
                                row.extend(snippet.chars().take(300));
                                row
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        Fin::AgentTool {
                            request,
                            result: Ok(text),
                        }
                    });
                    None
                }
                _ => return Err("Unknown OmpDeck tool".to_owned()),
            })
        })();
        match result {
            Ok(Some(mut event)) => {
                let review = matches!(event, ServerEvent::AgentReviewRequest { .. });
                let delivered = if review {
                    self.conns.values().any(|c| c.is(Role::Gui))
                } else {
                    self.to_desktop(&event)
                };
                if !delivered {
                    self.finish_agent_tool(
                        request,
                        false,
                        "OmpDeck's window is not open".to_owned(),
                    );
                } else if review {
                    self.send_agent_review(&mut event);
                    self.agent_tools
                        .pending
                        .get_mut(&request)
                        .expect("pending")
                        .review = Some(event);
                } else {
                    self.agent_tools
                        .pending
                        .get_mut(&request)
                        .expect("pending")
                        .deadline = Some(now + Duration::from_secs(30));
                }
            }
            Err(text) => self.finish_agent_tool(request, false, text),
            Ok(None) => {}
        }
    }

    pub(super) fn agent_tool_done(
        &mut self,
        request: u64,
        target: &SessionTarget,
        ok: bool,
        text: String,
        opened: Option<SessionTarget>,
    ) {
        if self.agent_tools.fan_out.contains(request) {
            self.fanout_open_done(request, target, ok, text, opened);
            return;
        }
        let Some(p) = self.agent_tools.pending.get(&request) else {
            return;
        };
        if &p.target != target || p.review.is_some() {
            return;
        }
        let mut text = text;
        if ok && let Some(line) = &p.worktree_result {
            text.push('\n');
            text.push_str(line);
        }
        if ok && let Some(opened) = opened {
            match self.register_agent_child(request, &opened) {
                Ok(Some(handle)) => text.push_str(&format!(
                    "\nHandle: {handle}: use it with ompdeck_send_prompt and ompdeck_wait_session. Use ompdeck_answer_question to answer forwarded questions."
                )),
                Ok(None) => {}
                Err(error) => {
                    self.finish_agent_tool(request, false, error);
                    return;
                }
            }
        }
        self.finish_agent_tool(request, ok, text);
    }

    pub(super) fn agent_review_reply(
        &mut self,
        request: u64,
        target: &SessionTarget,
        reply: ReviewReply,
    ) {
        let Some(p) = self.agent_tools.pending.get_mut(&request) else {
            return;
        };
        if &p.target != target || p.review.is_none() || p.answering {
            return;
        }
        p.answering = true;
        match reply {
            ReviewReply::NoComments => {
                self.finish_agent_tool(request, true, "The user had no comments".to_owned())
            }
            ReviewReply::Dismissed => self.finish_agent_tool(
                request,
                true,
                "The user dismissed the review request".to_owned(),
            ),
            ReviewReply::Comments { comments, general } => {
                let target = p.target.clone();
                let Some(i) = self.sess.find(&target.key) else {
                    return;
                };
                let root = self.sess.managed[i].current_cwd().to_path_buf();
                self.spawn_worker("agent-review", move || Fin::AgentTool {
                    request,
                    result: super::mutate::build_review(&root, &comments, general.as_deref())
                        .map_err(|e| e.to_string()),
                });
            }
        }
    }

    fn send_agent_review(&self, event: &mut ServerEvent) {
        let designated = self.desktop_conn();
        for (&conn, client) in &self.conns {
            if client.is(Role::Gui)
                && let Some(out) = client.local()
            {
                if let ServerEvent::AgentReviewRequest { activate, .. } = event {
                    *activate = designated == Some(conn);
                }
                out.event(event);
            }
        }
        // Persist only the non-activating form: all replays are passive.
        if let ServerEvent::AgentReviewRequest { activate, .. } = event {
            *activate = false;
        }
    }

    pub(super) fn resend_agent_reviews(&self, conn: u64) {
        if let Some(c) = self.conns.get(&conn)
            && c.is(Role::Gui)
            && let Some(out) = c.local()
        {
            let mut pending = Vec::new();
            for p in self.agent_tools.pending.values() {
                if let Some(event) = &p.review {
                    out.event(event);
                    if let ServerEvent::AgentReviewRequest { request, .. } = event {
                        pending.push(*request);
                    }
                }
            }
            out.event(&ServerEvent::AgentReviewsReplayed { pending });
        }
    }

    pub(super) fn stop_agent_tools(&mut self) {
        self.drop_background_waits();
        let requests: Vec<_> = self.agent_tools.operations.keys().copied().collect();
        for request in requests {
            self.child_operation_finished(request);
        }
        let mut acknowledgements = HashMap::new();
        for (request, pending) in std::mem::take(&mut self.agent_tools.pending) {
            if pending.review.is_some() {
                self.to_guis(&ServerEvent::AgentReviewCancel { request });
            }
            if let Some(i) = self.sess.find(&pending.target.key) {
                let m = &self.sess.managed[i];
                if m.ctl.target() == &pending.target
                    && let Some(link) = &m.live
                {
                    let id = link.client.send(CtlCommand::ToolResult {
                        id: pending.tool_id,
                        ok: false,
                        text: "OmpDeck is shutting down".to_owned(),
                        details: None,
                    });
                    acknowledgements.entry(i).or_insert_with(Vec::new).push(id);
                }
            }
        }
        // Shutdown only: let the extension receive the explicit restart verdict before Drop
        // stops the ctl workers. Never wait on an unresponsive session indefinitely.
        let deadline = Instant::now() + Duration::from_secs(1);
        while !acknowledgements.is_empty() && Instant::now() < deadline {
            acknowledgements.retain(|&i, ids| {
                let Some(link) = &self.sess.managed[i].live else {
                    return false;
                };
                for (id, reply) in link.client.take_replies() {
                    if reply.ok {
                        ids.retain(|pending| *pending != id);
                    }
                }
                !ids.is_empty() && link.client.connected()
            });
            if !acknowledgements.is_empty() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn agent_session_rows(&self, opener: &SessionTarget) -> Vec<(String, Option<PathBuf>)> {
        let opener_root = self
            .sess
            .find(&opener.key)
            .and_then(|i| self.sess.managed[i].ctl.session_path())
            .map(children::canonical);
        let mut rows: Vec<_> = self
            .sess
            .managed
            .iter()
            .filter(|m| m.role == crate::gateway::protocol::SessionRole::Session)
            .take(30)
            .map(|m| {
                let target = m.ctl.target();
                let state = if m.exited() {
                    "exited"
                } else {
                    match m.ctl.activity() {
                        crate::statusfmt::Activity::Busy => "busy",
                        crate::statusfmt::Activity::Attention => "needs attention",
                        _ => "idle",
                    }
                };
                let open = self.views.values().any(|v| &v.target == target);
                let model = m.ctl.current_model();
                let handle = self.child_handle(opener_root.as_deref(), &target.key);
                let mut row = format!(
                    "{} | {} | {} | model: {:?} | tab open: {} | opened by this session: {}",
                    m.display_label(),
                    home_path(m.current_cwd()),
                    state,
                    model,
                    open,
                    handle.is_some()
                );
                if let Some(snapshot) = m.live.as_ref().and_then(|link| link.client.latest())
                    && let Some(progress) = &snapshot.progress
                {
                    append_progress(&mut row, progress);
                }
                let row = match handle {
                    Some(handle) => self.child_handle_row(row, handle),
                    None => row,
                };
                (row, m.ctl.session_path().map(ToOwned::to_owned))
            })
            .collect();
        if let Some(groups) = self.sessions_watch.latest() {
            for session in groups.iter().flat_map(|g| &g.sessions) {
                if rows.len() >= 30 {
                    break;
                }
                if rows
                    .iter()
                    .any(|(_, path)| path.as_ref() == Some(&session.path))
                {
                    continue;
                }
                let state = self.running.latest().map_or("exited", |running| {
                    if !running.is_running(&session.id, &session.path) {
                        return "exited";
                    }
                    match running
                        .holder(&session.path)
                        .map(|n| running.activity_of(n))
                    {
                        Some(crate::statusfmt::Activity::Busy) => "busy",
                        Some(crate::statusfmt::Activity::Attention) => "needs attention",
                        _ => "idle",
                    }
                });
                let handle = self.child_file_handle(opener_root.as_deref(), &session.path);
                let row = format!(
                    "{} | {} | {} | model: unknown | tab open: false | opened by this session: {}",
                    session.label(),
                    home_path(&session.cwd),
                    state,
                    handle.is_some()
                );
                let row = match handle {
                    Some(handle) => self.child_handle_row(row, handle),
                    None => row,
                };
                rows.push((row, Some(session.path.clone())));
            }
        }
        rows
    }
}
fn append_progress(row: &mut String, progress: &deck_core::ctl::Progress) {
    if progress.phases.iter().any(|phase| !phase.tasks.is_empty()) {
        let (done, total) = progress.counts();
        let _ = write!(row, " | todos: {done}/{total}");
        if let Some(task) = progress.current() {
            row.push_str(" (now: ");
            append_short(row, &task.content, 80);
            row.push(')');
        }
    }
    if let Some(goal) = &progress.goal {
        use deck_core::ctl::GoalStatus;
        let status = match goal.status {
            GoalStatus::Active => "active",
            GoalStatus::Paused => "paused",
            GoalStatus::BudgetLimited => "budget-limited",
            GoalStatus::Complete => "complete",
            GoalStatus::Dropped => "dropped",
            GoalStatus::Unknown => "unknown",
        };
        let _ = write!(row, " | goal: {status} — ");
        append_short(row, &goal.objective, 80);
    }
}

fn append_short(out: &mut String, text: &str, cap: usize) {
    out.extend(
        text.chars()
            .take(cap)
            .map(|ch| if ch.is_control() { ' ' } else { ch }),
    );
}

fn relative_path(path: &str) -> bool {
    !path.is_empty()
        && !std::path::Path::new(path).is_absolute()
        && !std::path::Path::new(path).components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir
                    | std::path::Component::Prefix(_)
                    | std::path::Component::RootDir
            )
        })
}
fn home_path(path: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

fn matches_session(
    current: &SessionTarget,
    root: Option<&std::path::Path>,
    target: &SessionTarget,
    request_root: Option<&std::path::Path>,
) -> bool {
    current == target && root == request_root
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::SessionKey;
    fn target(generation: u64) -> SessionTarget {
        SessionTarget {
            key: SessionKey::host("omp-deck-test-tools", "tools-test"),
            epoch: crate::controller::ServiceEpoch::current(),
            generation,
        }
    }
    #[test]
    fn pending_cap_is_session_bound_and_cancel_is_once() {
        let mut tools = AgentTools::default();
        let t = target(1);
        for id in 1..=4 {
            tools.pending.insert(
                id,
                Pending {
                    target: t.clone(),
                    tool_id: id,
                    root: None,
                    deadline: None,
                    review: Some(ServerEvent::AgentReviewRequest {
                        request: id,
                        activate: false,
                        target: t.clone(),
                        summary: None,
                        paths: Vec::new(),
                    }),
                    answering: false,
                    opens_session: false,
                    open_prompt: None,
                    forward_questions: true,
                    worktree_result: None,
                },
            );
        }
        assert!(tools.at_capacity(&t));
        assert!(!tools.at_capacity(&target(2)));
        assert!(tools.cancel(&target(2), 1).is_none());
        assert!(matches!(
            tools.cancel(&t, 1),
            Some(ServerEvent::AgentReviewCancel { request: 1 })
        ));
        assert!(tools.cancel(&t, 1).is_none());
        assert!(!tools.at_capacity(&t));
    }
    #[test]
    fn requests_are_fenced_by_generation_and_root() {
        let root = std::path::Path::new("/test/session.jsonl");
        assert!(matches_session(
            &target(1),
            Some(root),
            &target(1),
            Some(root)
        ));
        assert!(!matches_session(
            &target(1),
            Some(root),
            &target(2),
            Some(root)
        ));
        assert!(!matches_session(&target(1), Some(root), &target(1), None));
    }
    #[test]
    fn notification_rate_limit_is_per_session_and_ten_seconds() {
        let mut tools = AgentTools::default();
        let now = Instant::now();
        assert!(tools.notify_allowed(&target(1), now));
        assert!(!tools.notify_allowed(&target(1), now + Duration::from_secs(9)));
        assert!(tools.notify_allowed(&target(2), now));
        assert!(tools.notify_allowed(&target(1), now + Duration::from_secs(10)));
    }
    #[test]
    fn open_session_forwards_questions_by_default_and_allows_opt_out() {
        let default: OpenArgs = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(default.forward_questions);
        let disabled: OpenArgs =
            serde_json::from_value(serde_json::json!({"forward_questions": false})).unwrap();
        assert!(!disabled.forward_questions);
        assert!(
            serde_json::from_value::<OpenArgs>(serde_json::json!({"forward_questions": "false"}),)
                .is_err()
        );
    }
}
