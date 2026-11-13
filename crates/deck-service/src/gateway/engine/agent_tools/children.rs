//! Owned child handles and asynchronous prompt delivery/turn scans.
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use deck_core::config::SessionView;
use deck_core::mux::MuxKind;
use deck_core::status::{SessionStatsReader, TurnOutcome, TurnScan, TurnWatch};
use serde::Deserialize;

use super::{Engine, Fin, home_path};
use crate::controller::{
    Command, Delivery, Refusal, Rejected, RequestId, RequestKey, SessionKey, SessionTarget,
    Submission,
};
use crate::gateway::engine::sessions::omp_argv;
use crate::gateway::protocol::{LaunchSpec, SessionRole};
use crate::inject::ComposeMode;
use crate::statusfmt::Activity;

mod background;
use background::{BackgroundWatch, OpenerSource};
mod questions;
use questions::QuestionWatch;
mod reports;
use reports::{ReportDelivery, ReportRate};
pub(in crate::gateway::engine) mod fanout;
pub(in crate::gateway::engine) mod orchestration;
mod worktrees;
use worktrees::{ChildFinish, ChildRelocation};

const STALE: &str = "That session has exited or switched conversations; open a new one";
const UNKNOWN: &str = "Unknown session handle";
const DRAFT: &str = "The session has unsent text in its composer; ask the user to send or clear it";
const BUSY: &str = "The session is busy; use mode follow_up or steer, or wait for it first";
const IDLE: &str = "The session is idle; use mode send";
const DIALOG: &str = "The session is waiting on a question or approval for the user";
const POLL: Duration = Duration::from_millis(500);
const DELIVERY: Duration = Duration::from_secs(30);
const REPLY_CAP: usize = 32 * 1024;
const BACKGROUND_CAP: usize = 8;

#[derive(Clone)]
pub(super) struct Baseline {
    offset: u64,
    prompt: String,
}

pub(super) struct ChildSession {
    opener_root: PathBuf,
    opener: OpenerSource,
    key: SessionKey,
    file: Option<PathBuf>,
    last: Option<Baseline>,
    background: Option<BackgroundWatch>,
    background_call: Option<u64>,
    forward_questions: bool,
    questions: QuestionWatch,
    attach: LaunchSpec,
    backend: Option<MuxKind>,
    attaching: bool,
    invalidated: bool,
    reports: ReportRate,
    last_report: Option<crate::gateway::protocol::OrchReport>,
    title: String,
    opened_at_unix_ms: u64,
    /// Whether this current path has ever materialized (fresh omp paths are announced early).
    file_seen: bool,
    fan_out: Option<String>,
    restored: bool,
    relocation: Option<ChildRelocation>,
}
impl ChildSession {
    fn matches(&self, target: &SessionTarget, file: Option<&Path>) -> bool {
        !self.invalidated
            && self.key == target.key
            && self.file.as_deref().is_none_or(|known| {
                Some(known) == file || self.relocation_file(file, Instant::now())
            })
    }
}

#[derive(Default)]
pub(super) struct Registry {
    entries: HashMap<String, ChildSession>,
    answers: HashMap<u64, questions::AnswerRequest>,
    reports: HashMap<u64, ReportDelivery>,
    finishes: HashMap<RequestKey, ChildFinish>,
    finish_sequence: u64,
}
impl Registry {
    fn handle(&self, root: &Path, key: &SessionKey) -> Option<&str> {
        self.entries.iter().find_map(|(handle, child)| {
            (!child.invalidated && child.opener_root == root && &child.key == key)
                .then_some(handle.as_str())
        })
    }
    fn owned(&self, handle: &str, root: &Path) -> Result<&ChildSession, String> {
        let child = self
            .entries
            .get(handle)
            .filter(|child| child.opener_root == root)
            .ok_or_else(|| UNKNOWN.to_owned())?;
        if child.invalidated {
            Err(STALE.to_owned())
        } else {
            Ok(child)
        }
    }
    fn background_capacity(&self, handle: &str, root: &Path) -> Result<(), String> {
        if self
            .entries
            .get(handle)
            .is_some_and(|child| child.background.is_some())
            || self
                .entries
                .values()
                .filter(|child| child.opener_root == root && child.background.is_some())
                .count()
                < BACKGROUND_CAP
        {
            Ok(())
        } else {
            Err("Too many background waits".to_owned())
        }
    }
    fn arm_background(&mut self, handle: &str, watch: BackgroundWatch) -> Result<(), String> {
        let child = self.entries.get(handle).ok_or_else(|| UNKNOWN.to_owned())?;
        if child
            .background_call
            .is_some_and(|latest| latest != watch.id)
        {
            return Ok(()); // A newer call replaced this initial inspection.
        }
        self.background_capacity(handle, &child.opener_root)?;
        self.entries.get_mut(handle).expect("child").background = Some(watch);
        Ok(())
    }

    fn drop_background_opener(&mut self, key: &SessionKey) -> Vec<SessionKey> {
        self.entries
            .values_mut()
            .filter_map(|child| {
                (&child.opener.key == key && child.background.take().is_some())
                    .then(|| child.key.clone())
            })
            .collect()
    }
    fn insert(&mut self, child: ChildSession) -> Result<String, String> {
        loop {
            let mut bytes = [0; 4];
            getrandom::fill(&mut bytes)
                .map_err(|e| format!("Cannot identify child session: {e}"))?;
            let handle = format!("s-{:08x}", u32::from_le_bytes(bytes));
            if !self.entries.contains_key(&handle) {
                self.entries.insert(handle.clone(), child);
                return Ok(handle);
            }
        }
    }
}

#[derive(Deserialize)]
struct SendArgs {
    session: String,
    prompt: String,
    mode: Option<String>,
}
#[derive(Deserialize)]
struct WaitArgs {
    session: String,
    timeout_seconds: Option<u64>,
    #[serde(default)]
    background: bool,
}

pub(super) enum Action {
    Send {
        prompt: String,
        mode: ComposeMode,
        until: Instant,
    },
    Wait {
        seconds: Option<u64>,
        until: Option<Instant>,
        background: bool,
        baseline: Option<Baseline>,
    },
}
impl Action {
    fn deadline(&self) -> Instant {
        match self {
            Self::Send { until, .. } => *until,
            Self::Wait { until, .. } => until.unwrap_or_else(|| Instant::now() + DELIVERY),
        }
    }
}

pub(super) enum Operation {
    Attach {
        handle: String,
        action: Action,
        until: Instant,
    },
    Preparing {
        handle: String,
        prompt: String,
        mode: ComposeMode,
        until: Instant,
    },
    Send(Box<SendOp>),
    Wait(Box<Waiter>),
}
pub(super) struct SendOp {
    handle: String,
    target: SessionTarget,
    client: String,
    id: RequestId,
    baseline: Baseline,
    until: Instant,
}
pub(super) struct Waiter {
    handle: String,
    until: Option<Instant>,
    seconds: Option<u64>,
    baseline: Option<Baseline>,
    background: bool,
    background_watch: bool,
    status: &'static str,
    next: Instant,
    watch: Option<TurnWatch>,
    scan: TurnScan,
    in_flight: bool,
    scanned: bool,
    exit_seen: bool,
    exact: bool,
    fallback: bool,
    file: Option<PathBuf>,
    freshness: ScanFreshness,
}

struct WaitResult {
    ok: bool,
    text: String,
    status: &'static str,
}
impl WaitResult {
    fn new(ok: bool, text: String, status: &'static str) -> Self {
        Self { ok, text, status }
    }
}

impl Waiter {
    fn new(
        handle: String,
        child: &ChildSession,
        baseline: Option<Baseline>,
        seconds: Option<u64>,
        until: Option<Instant>,
        background: bool,
        now: Instant,
    ) -> Self {
        let file = child.file.clone();
        let watch = file.as_ref().zip(baseline.as_ref()).map(|(file, last)| {
            TurnWatch::new(file.clone(), last.offset, Some(last.prompt.clone()))
        });
        Self {
            handle,
            until,
            seconds,
            background,
            background_watch: false,
            status: "invalid",
            fallback: baseline.is_none(),
            baseline,
            file,
            watch,
            next: now,
            scan: empty_scan(),
            in_flight: false,
            scanned: false,
            exit_seen: false,
            exact: false,
            freshness: ScanFreshness::default(),
        }
    }
}

#[derive(Default)]
struct ScanFreshness {
    started: u64,
    completed: u64,
    idle_after: Option<u64>,
}
impl ScanFreshness {
    fn observe(&mut self, idle: bool) {
        if idle {
            self.idle_after.get_or_insert(self.started);
        } else {
            self.idle_after = None;
        }
    }
    fn start(&mut self) {
        self.started += 1;
    }
    fn finish(&mut self) {
        self.completed = self.started;
    }
    fn after_idle(&self) -> bool {
        self.idle_after
            .is_some_and(|generation| self.completed > generation)
    }
}

impl Operation {
    fn handle(&self) -> &str {
        match self {
            Self::Attach { handle, .. } | Self::Preparing { handle, .. } => handle,
            Self::Send(send) => &send.handle,
            Self::Wait(w) => &w.handle,
        }
    }
}

pub(crate) enum WorkerResult {
    Offset {
        request: u64,
        result: Result<u64, String>,
    },
    Scan {
        request: u64,
        watch: TurnWatch,
        result: io::Result<TurnScan>,
    },
    Reply {
        request: u64,
        result: Result<(String, usize), String>,
    },
}

pub(super) fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}
fn conversation_file<'a>(
    cached: Option<&'a Path>,
    live: [Option<Option<&'a Path>>; 2],
    files: &'a [PathBuf],
) -> Result<Option<&'a Path>, ()> {
    match live {
        [Some(a), Some(b)] if a != b => return Err(()),
        [Some(current), _] | [_, Some(current)] => return Ok(current),
        [None, None] => {}
    }
    if files.is_empty() || cached.is_some_and(|path| files.iter().any(|file| file == path)) {
        Ok(cached)
    } else {
        Ok(files.first().map(PathBuf::as_path))
    }
}

fn empty_scan() -> TurnScan {
    TurnScan {
        prompt_seen: false,
        end: None,
        reply: None,
        reply_more: 0,
        later_prompt: false,
    }
}
fn complete(
    scan: &TurnScan,
    activity: Activity,
    injecting: bool,
    exact: bool,
    fresh_idle: bool,
) -> bool {
    scan.end.is_some()
        && if activity != Activity::Busy && !injecting {
            fresh_idle
        } else {
            exact && scan.later_prompt
        }
}
fn utf8_prefix(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn with_reply(mut text: String, scan: &TurnScan, file: Option<&Path>) -> String {
    if let Some(reply) = &scan.reply {
        let prefix = utf8_prefix(reply, REPLY_CAP);
        let more = scan.reply_more + reply[prefix.len()..].chars().count();
        text.push_str("\n\n");
        text.push_str(prefix);
        if more > 0 {
            text.push_str(&format!(
                "\n[… {} more characters; full text in {}]",
                more,
                file.map_or_else(String::new, |p| p.display().to_string())
            ));
        }
    }
    text
}
fn ended(scan: &TurnScan, title: &str, cwd: &Path, file: Option<&Path>) -> String {
    let outcome = match scan.end.as_ref() {
        Some(TurnOutcome::Error(Some(message))) => format!("error: {message}"),
        Some(TurnOutcome::Error(None)) => "error".to_owned(),
        Some(TurnOutcome::Aborted) => "aborted".to_owned(),
        _ => "completed".to_owned(),
    };
    with_reply(
        format!("{title} finished ({outcome}) in {}.", home_path(cwd)),
        scan,
        file,
    )
}
fn rejected(rejected: Rejected, mode: ComposeMode) -> String {
    match rejected {
        Rejected::Busy => if mode == ComposeMode::Send {
            BUSY
        } else {
            "The session is busy delivering another input; try later"
        }
        .to_owned(),
        Rejected::Exited | Rejected::Detached => STALE.to_owned(),
        other => format!("The prompt could not be sent: {other:?}"),
    }
}

impl super::AgentTools {
    /// Registry references outlive controller ownership (notably a started finish's callback).
    fn child_registry_referenced(&self, handle: &str) -> bool {
        self.children.finish_references(handle)
            || self
                .children
                .entries
                .get(handle)
                .is_some_and(|child| self.controller_needed(&child.key))
    }

    fn controller_needed(&self, key: &SessionKey) -> bool {
        self.pending
            .values()
            .any(|pending| &pending.target.key == key)
            || self.reply_acks.iter().any(|(target, _)| &target.key == key)
            || self
                .deferred_deliveries
                .iter()
                .any(|(target, _)| &target.key == key)
            || self.fan_out.needs_controller(key, &self.children)
            || self
                .children
                .reports
                .values()
                .any(|report| report.needs_controller(key, &self.children))
            || self
                .children
                .finishes
                .values()
                .any(|finish| finish.needs_controller(key, &self.children))
            || self.children.entries.values().any(|child| {
                child.forward_questions && !child.invalidated && &child.opener.key == key
            })
            || self.children.entries.values().any(|child| {
                child
                    .background
                    .as_ref()
                    .is_some_and(|watch| watch.needs_controller(key, child))
            })
            || self.operations.values().any(|operation| {
                self.children
                    .entries
                    .get(operation.handle())
                    .is_some_and(|child| &child.key == key)
            })
    }

    fn acknowledge_reply(&mut self, key: &SessionKey, rid: u64) {
        self.reply_acks
            .retain(|(target, pending)| &target.key != key || *pending != rid);
    }

    fn release_child_ownership(&mut self, key: &SessionKey) -> bool {
        if self.controller_needed(key) {
            return false;
        }
        for child in self
            .children
            .entries
            .values_mut()
            .filter(|child| &child.key == key)
        {
            child.attaching = false;
        }
        true
    }
}

impl Engine {
    pub(in crate::gateway::engine) fn register_agent_child(
        &mut self,
        request: u64,
        opened: &SessionTarget,
    ) -> Result<Option<String>, String> {
        let p = self
            .agent_tools
            .pending
            .get(&request)
            .ok_or_else(|| UNKNOWN.to_owned())?;
        if !p.opens_session || &p.target == opened {
            return Ok(None);
        }
        let root = canonical(
            p.root
                .as_deref()
                .ok_or_else(|| "The opener has no session file".to_owned())?,
        );
        let i = self.check_target(opened).map_err(|_| STALE.to_owned())?;
        let m = &self.sess.managed[i];
        let child = ChildSession {
            opener_root: root,
            opener: OpenerSource::from_managed(
                &self.sess.managed[self
                    .sess
                    .find(&p.target.key)
                    .ok_or_else(|| UNKNOWN.to_owned())?],
            ),
            key: opened.key.clone(),
            file: m.ctl.session_path().map(ToOwned::to_owned),
            last: p
                .open_prompt
                .clone()
                .map(|prompt| Baseline { offset: 0, prompt }),
            background: None,
            background_call: None,
            forward_questions: p.forward_questions,
            questions: QuestionWatch::new(m.ctl_key.clone()),
            attach: LaunchSpec {
                agent_child: false,
                mux_name: m.mux_name.clone(),
                cwd: m.current_cwd().to_path_buf(),
                session_path: m.ctl.session_path().map(ToOwned::to_owned),
                label: m.label.clone(),
                argv: Vec::new(),
                role: SessionRole::Session,
                copy_of: None,
                view: m.view,
            },
            backend: m.mux.as_ref().map(deck_core::mux::Mux::kind),
            attaching: false,
            invalidated: false,
            reports: ReportRate::new(Instant::now()),
            last_report: None,
            title: m.display_label(),
            opened_at_unix_ms: crate::gateway::unix_ms(),
            file_seen: false,
            fan_out: None,
            restored: false,
            relocation: None,
        };
        let enabled = self
            .sess
            .find(&p.target.key)
            .and_then(|i| self.sess.managed[i].live.as_ref())
            .is_some_and(|link| {
                link.client.tools().iter().any(|name| {
                    matches!(
                        name.as_str(),
                        "ompdeck_send_prompt"
                            | "ompdeck_wait_session"
                            | "ompdeck_answer_question"
                            | "ompdeck_finish_worktree"
                            | "ompdeck_fan_out"
                    )
                })
            });
        let handle = self.agent_tools.children.insert(child)?;
        Ok(enabled.then_some(handle))
    }

    pub(in crate::gateway::engine) fn child_handle_row(&self, row: String, handle: &str) -> String {
        let child = &self.agent_tools.children.entries[handle];
        let mut row = format!("{row} | handle: {handle}");
        if let Some(group) = &child.fan_out {
            row.push_str(&format!(" | fan-out {group}"));
        }
        if child.background.is_some() {
            row.push_str(" | background wait pending");
        }
        if child.forward_questions && !child.invalidated {
            row.push_str(" | forwarding questions");
        }
        let pending = child.questions.pending_tokens();
        if !pending.is_empty() {
            row.push_str(" | pending questions: ");
            for (i, question) in pending.iter().enumerate() {
                if i > 0 {
                    row.push_str(", ");
                }
                row.push_str(question);
            }
        }
        self.append_child_worktree(&mut row, handle);
        if let Some(report) = &child.last_report {
            row.push_str(" | last report: ");
            super::append_short(&mut row, &report.text, 120);
        }
        row
    }

    pub(in crate::gateway::engine) fn child_handle(
        &self,
        root: Option<&Path>,
        key: &SessionKey,
    ) -> Option<&str> {
        let handle = self.agent_tools.children.handle(root?, key)?;
        let child = &self.agent_tools.children.entries[handle];
        let index = self.sess.find(key)?;
        let m = &self.sess.managed[index];
        (!m.exited()
            && self.with_current_child_file(Some(handle), index, |file| {
                file.is_ok_and(|file| child.matches(m.ctl.target(), file))
            }))
        .then_some(handle)
    }

    pub(in crate::gateway::engine) fn child_file_handle(
        &self,
        root: Option<&Path>,
        file: &Path,
    ) -> Option<&str> {
        let root = root?;
        if let Some((handle, _)) = self.agent_tools.children.entries.iter().find(|(_, child)| {
            child.restored
                && !child.invalidated
                && child.opener_root == root
                && child.file.as_deref() == Some(file)
        }) {
            return Some(handle);
        }
        let running = self.running.latest()?;
        self.agent_tools
            .children
            .entries
            .iter()
            .find_map(|(handle, child)| {
                (!child.invalidated
                    && child.opener_root == root
                    && child.file.as_deref() == Some(file)
                    && child.attach.mux_name.as_deref().is_some_and(|name| {
                        running.is_alive(name)
                            && child.backend == running.owner_of(name)
                            && (running.files_of(name).is_empty()
                                || running.files_of(name).iter().any(|path| path == file))
                    }))
                .then_some(handle.as_str())
            })
    }

    pub(in crate::gateway::engine) fn begin_child_tool(
        &mut self,
        request: u64,
        name: &str,
        args: serde_json::Value,
        now: Instant,
    ) -> Result<(), String> {
        let (handle, mut action) = if name == "send_prompt" {
            let a: SendArgs = serde_json::from_value(args).map_err(|e| e.to_string())?;
            if a.prompt.trim().is_empty() || a.prompt.len() > 16 * 1024 {
                return Err("prompt must contain 1 to 16384 bytes".to_owned());
            }
            let mode = match a.mode.as_deref().unwrap_or("send") {
                "send" => ComposeMode::Send,
                "follow_up" => ComposeMode::FollowUp,
                "steer" => ComposeMode::Steer,
                _ => return Err("invalid prompt mode".to_owned()),
            };
            (
                a.session,
                Action::Send {
                    prompt: a.prompt,
                    mode,
                    until: now + DELIVERY,
                },
            )
        } else {
            let a: WaitArgs = serde_json::from_value(args).map_err(|e| e.to_string())?;
            let seconds = if a.background {
                a.timeout_seconds
            } else {
                Some(a.timeout_seconds.unwrap_or(600))
            };
            let max = if a.background { 86400 } else { 3600 };
            if seconds.is_some_and(|seconds| !(1..=max).contains(&seconds)) {
                return Err(format!("timeout_seconds must be between 1 and {max}"));
            }
            (
                a.session,
                Action::Wait {
                    seconds,
                    until: seconds.map(|seconds| now + Duration::from_secs(seconds)),
                    background: a.background,
                    baseline: None,
                },
            )
        };
        let root = canonical(
            self.agent_tools.pending[&request]
                .root
                .as_deref()
                .ok_or_else(|| UNKNOWN.to_owned())?,
        );
        let child = self.agent_tools.children.owned(&handle, &root)?;
        if matches!(
            action,
            Action::Wait {
                background: true,
                ..
            }
        ) {
            self.agent_tools
                .children
                .background_capacity(&handle, &root)?;
        }
        let key = child.key.clone();
        if let Action::Wait { baseline, .. } = &mut action {
            *baseline = child.last.clone();
        }
        if self.agent_tools.pending[&request].target.key == key {
            return Err(UNKNOWN.to_owned());
        }
        if matches!(
            action,
            Action::Wait {
                background: true,
                ..
            }
        ) {
            let child = self
                .agent_tools
                .children
                .entries
                .get_mut(&handle)
                .expect("child");
            child.background = None;
            child.background_call = Some(request);
        }
        if self.sess.find(&key).is_none() {
            self.attach_agent_child(&handle)?;
            let until = action.deadline().min(now + DELIVERY);
            self.agent_tools.operations.insert(
                request,
                Operation::Attach {
                    handle,
                    action,
                    until,
                },
            );
        } else {
            self.start_child_action(request, handle, action, now)?;
        }
        self.waker.now();
        Ok(())
    }

    fn attach_agent_child(&mut self, handle: &str) -> Result<(), String> {
        if self
            .agent_tools
            .children
            .entries
            .get(handle)
            .is_some_and(|child| child.restored)
        {
            let file = self.agent_tools.children.entries[handle]
                .file
                .clone()
                .ok_or_else(|| STALE.to_owned())?;
            if let Some(index) = self
                .sess
                .managed
                .iter()
                .position(|m| m.ctl.session_path() == Some(file.as_path()) && !m.exited())
            {
                let child = self
                    .agent_tools
                    .children
                    .entries
                    .get_mut(handle)
                    .expect("restored child");
                child.key = self.sess.managed[index].ctl.target().key.clone();
                return self.validate_child_identity(handle, index);
            }
            let child = self
                .agent_tools
                .children
                .entries
                .get_mut(handle)
                .expect("restored child");
            if !child.attaching {
                let mux = self
                    .backends
                    .for_launch(None, child.attach.view)
                    .ok_or_else(|| "No persistent session backend is available".to_owned())?;
                let preferred = child
                    .attach
                    .mux_name
                    .as_deref()
                    .ok_or_else(|| STALE.to_owned())?;
                let name = self.running.latest().map_or_else(
                    || format!("{preferred}-{}", crate::gateway::unix_ms()),
                    |running| {
                        running
                            .resolve_for(
                                running.blind,
                                preferred,
                                Some(&file),
                                crate::gateway::unix_ms() as u128,
                            )
                            .into_name()
                    },
                );
                child.key = SessionKey::backend(mux, &name);
                child.backend = Some(mux.kind());
                child.attach.mux_name = Some(name);
                child.attaching = true;
                let mut spec = child.attach.clone();
                spec.argv = omp_argv(Some(&file));
                let key = child.key.clone();
                self.queue_agent_child_resume(spec, key);
            }
            return Ok(());
        }
        let child = self
            .agent_tools
            .children
            .entries
            .get_mut(handle)
            .ok_or_else(|| UNKNOWN.to_owned())?;
        let name = child
            .attach
            .mux_name
            .as_deref()
            .ok_or_else(|| STALE.to_owned())?;
        let backend = child.backend.ok_or_else(|| STALE.to_owned())?;
        let running = self
            .running
            .latest()
            .ok_or_else(|| "Session state is not ready; try again".to_owned())?;
        if !running.is_alive(name) || running.owner_of(name) != Some(backend) {
            return Err(STALE.to_owned());
        }
        if let Some(file) = &child.file {
            let files = running.files_of(name);
            if !files.is_empty() && !files.iter().any(|path| canonical(path) == *file) {
                return Err(STALE.to_owned());
            }
        }
        if !child.attaching {
            child.attaching = true;
            let mut spec = child.attach.clone();
            spec.session_path.clone_from(&child.file);
            spec.argv = omp_argv(child.file.as_deref());
            let key = child.key.clone();
            self.queue_agent_child_attach(spec, key, backend);
        }
        Ok(())
    }

    fn with_current_child_file<R>(
        &self,
        handle: Option<&str>,
        index: usize,
        read: impl FnOnce(Result<Option<&Path>, ()>) -> R,
    ) -> R {
        let m = &self.sess.managed[index];
        let question = handle.and_then(|handle| self.child_question_snapshot(handle));
        let managed = m
            .live
            .as_ref()
            .filter(|link| link.client.connected())
            .and_then(|link| link.client.latest());
        let running = self.running.latest();
        let files = m
            .mux_name
            .as_deref()
            .and_then(|name| running.as_ref().map(|snapshot| snapshot.files_of(name)))
            .unwrap_or_default();
        // Our retire /move can reach the two ctl links in opposite orders. Both
        // roots must prove that same conversation and exact destination.
        if let Some(child) = handle.and_then(|handle| self.agent_tools.children.entries.get(handle))
        {
            let sources = [
                managed
                    .as_ref()
                    .map(|snapshot| snapshot.session_file.as_deref()),
                question
                    .as_ref()
                    .map(|snapshot| snapshot.session_file.as_deref()),
            ];
            if let Some(current) = child.relocation_current_file(sources, Instant::now()) {
                return read(Ok(Some(current)));
            }
        }
        read(conversation_file(
            m.ctl.session_path(),
            [
                managed
                    .as_ref()
                    .map(|snapshot| snapshot.session_file.as_deref()),
                question
                    .as_ref()
                    .map(|snapshot| snapshot.session_file.as_deref()),
            ],
            files,
        ))
    }

    fn validate_child_identity(&mut self, handle: &str, index: usize) -> Result<(), String> {
        self.sync_child_relocations(Instant::now());
        let child = self
            .agent_tools
            .children
            .entries
            .get(handle)
            .ok_or_else(|| UNKNOWN.to_owned())?;
        let learned = self.with_current_child_file(Some(handle), index, |current| {
            let current = current?;
            if !child.matches(self.sess.managed[index].ctl.target(), current) {
                return Err(());
            }
            Ok(if child.file.is_none() {
                current.map(ToOwned::to_owned)
            } else {
                None
            })
        });
        let child = self
            .agent_tools
            .children
            .entries
            .get_mut(handle)
            .expect("child");
        match learned {
            Ok(file) => {
                if let Some(file) = file {
                    child.file = Some(file);
                }
                Ok(())
            }
            Err(()) => {
                child.invalidated = true;
                Err(STALE.to_owned())
            }
        }
    }

    fn valid_child(&mut self, handle: &str) -> Result<usize, String> {
        let child = self
            .agent_tools
            .children
            .entries
            .get(handle)
            .ok_or_else(|| UNKNOWN.to_owned())?;
        let i = self.sess.find(&child.key).ok_or_else(|| STALE.to_owned())?;
        if self.sess.managed[i].exited() {
            return Err(STALE.to_owned());
        }
        self.validate_child_identity(handle, i)?;
        self.agent_tools
            .children
            .entries
            .get_mut(handle)
            .expect("child")
            .attaching = false;
        Ok(i)
    }

    fn start_child_action(
        &mut self,
        request: u64,
        handle: String,
        action: Action,
        now: Instant,
    ) -> Result<(), String> {
        let i = self.valid_child(&handle)?;
        match action {
            Action::Send {
                prompt,
                mode,
                until,
            } => {
                let path = self.sess.managed[i]
                    .ctl
                    .session_path()
                    .map(ToOwned::to_owned);
                self.agent_tools.operations.insert(
                    request,
                    Operation::Preparing {
                        handle,
                        prompt,
                        mode,
                        until,
                    },
                );
                self.spawn_worker("agent-send-baseline", move || {
                    let result = match path {
                        Some(path) => std::fs::metadata(path)
                            .map(|m| m.len())
                            .or_else(|e| {
                                if e.kind() == io::ErrorKind::NotFound {
                                    Ok(0)
                                } else {
                                    Err(e)
                                }
                            })
                            .map_err(|e| e.to_string()),
                        None => Ok(0),
                    };
                    Fin::AgentChildren(WorkerResult::Offset { request, result })
                });
            }
            Action::Wait {
                seconds,
                until,
                background,
                baseline,
            } => {
                let child = &self.agent_tools.children.entries[&handle];
                let waiter = Waiter::new(handle, child, baseline, seconds, until, background, now);
                self.agent_tools
                    .operations
                    .insert(request, Operation::Wait(Box::new(waiter)));
            }
        }
        Ok(())
    }

    pub(in crate::gateway::engine) fn on_child_worker(&mut self, result: WorkerResult) {
        match result {
            WorkerResult::Offset { request, result } => {
                let Some(Operation::Preparing {
                    handle,
                    prompt,
                    mode,
                    until,
                }) = self.agent_tools.operations.remove(&request)
                else {
                    return;
                };
                let child_key = self
                    .agent_tools
                    .children
                    .entries
                    .get(&handle)
                    .map(|child| child.key.clone());
                let outcome = (|| {
                    if !self.agent_tool_current(request) {
                        return Err("The opener's session changed".to_owned());
                    }
                    let offset = result?;
                    if Instant::now() >= until {
                        return Err(
                            "The prompt could not be delivered within 30 seconds".to_owned()
                        );
                    }
                    let i = self.valid_child(&handle)?;
                    let target = self.sess.managed[i].ctl.target().clone();
                    if self.checkpoint_restore_busy(&target) {
                        return Err(
                            "The session is restoring a checkpoint; wait for it to finish"
                                .to_owned(),
                        );
                    }
                    let activity = self.sess.managed[i].ctl.activity();
                    if mode == ComposeMode::Send && activity == Activity::Busy {
                        return Err(BUSY.to_owned());
                    }
                    if activity == Activity::Attention {
                        return Err(DIALOG.to_owned());
                    }
                    if mode != ComposeMode::Send && activity != Activity::Busy {
                        return Err(IDLE.to_owned());
                    }
                    let client = format!("agent-tool-{request}");
                    let baseline = Baseline {
                        offset,
                        prompt: prompt.clone(),
                    };
                    let submitted = self.sess.managed[i]
                        .ctl
                        .submit(Submission {
                            key: RequestKey {
                                client: client.clone(),
                                seq: 1,
                            },
                            target: target.clone(),
                            command: Command::Compose {
                                attachments: Vec::new(),
                                text: prompt,
                                mode,
                            },
                        })
                        .map_err(|e| rejected(e, mode))?;
                    self.agent_tools.operations.insert(
                        request,
                        Operation::Send(Box::new(SendOp {
                            handle,
                            target,
                            client,
                            id: submitted.id,
                            baseline,
                            until,
                        })),
                    );
                    self.dirty.controllers = true;
                    self.waker.now();
                    Ok(())
                })();
                if let Err(error) = outcome {
                    self.finish_agent_tool(request, false, error);
                    if let Some(key) = child_key {
                        self.release_idle_child_attachment(&key);
                    }
                }
            }
            WorkerResult::Scan {
                request,
                watch,
                result,
            } => {
                let Some(waiter) = self.agent_tools.waiter_mut(request) else {
                    return;
                };
                waiter.in_flight = false;
                waiter.scanned = true;
                waiter.freshness.finish();
                waiter.exact = watch.matched_exactly();
                waiter.watch = Some(watch);
                match result {
                    Ok(scan) => waiter.scan = scan,
                    // omp announces its planned file before persisting the first turn.
                    Err(e) if e.kind() == io::ErrorKind::NotFound && !waiter.scan.prompt_seen => {}
                    Err(e) => {
                        let text = if e.kind() == io::ErrorKind::InvalidData {
                            STALE.to_owned()
                        } else {
                            format!("Could not read the session reply: {e}")
                        };
                        let status = if e.kind() == io::ErrorKind::InvalidData {
                            "invalid"
                        } else {
                            "error"
                        };
                        self.finish_child_wait(request, WaitResult::new(false, text, status));
                    }
                }
            }
            WorkerResult::Reply { request, result } => {
                let Some(waiter) = self.agent_tools.waiter_mut(request) else {
                    return;
                };
                waiter.in_flight = false;
                match result {
                    Ok((reply, more)) => {
                        waiter.scan.reply = (!reply.is_empty()).then_some(reply);
                        waiter.scan.reply_more = more;
                        waiter.fallback = false;
                        waiter.scan.prompt_seen = true;
                        waiter.scanned = true;
                    }
                    Err(error) => {
                        self.finish_child_wait(request, WaitResult::new(false, error, "error"))
                    }
                }
            }
        }
        self.waker.now();
    }

    pub(in crate::gateway::engine) fn child_operation_finished(&mut self, request: u64) {
        self.cancel_child_finish_request(request);
        self.agent_tools.children.answers.remove(&request);
        let Some(operation) = self.agent_tools.operations.remove(&request) else {
            return;
        };
        if let Operation::Send(send) = &operation
            && let Some(i) = self.sess.find(&send.target.key)
        {
            self.sess.managed[i].ctl.cancel_client(&send.client);
        }
        if let Some(key) = self
            .agent_tools
            .children
            .entries
            .get(operation.handle())
            .map(|child| child.key.clone())
        {
            self.release_idle_child_attachment(&key);
        }
    }

    fn release_idle_child_attachment(&mut self, key: &SessionKey) {
        if self.agent_tools.release_child_ownership(key) {
            self.release_agent_child_attach(key);
        }
    }

    pub(in crate::gateway::engine) fn agent_child_attach_needed(&self, key: &SessionKey) -> bool {
        self.agent_tools.controller_needed(key)
    }

    pub(in crate::gateway::engine) fn agent_tool_reply_done(
        &mut self,
        key: &SessionKey,
        rid: u64,
        ok: bool,
        root_mismatch: bool,
        error: Option<String>,
    ) {
        self.agent_tools.acknowledge_reply(key, rid);
        self.background_reply_done(key, rid, ok, root_mismatch);
        self.child_report_reply_done(key, rid, ok, root_mismatch, error);
    }

    fn step_child_waiter(
        &mut self,
        request: u64,
        waiter: &mut Waiter,
        now: Instant,
    ) -> Option<(bool, String)> {
        let Some(child) = self.agent_tools.children.entries.get(&waiter.handle) else {
            return Some((false, UNKNOWN.to_owned()));
        };
        let Some(i) = self.sess.find(&child.key) else {
            if waiter.until.is_some_and(|until| now >= until) {
                waiter.status = "timeout";
                return Some((
                    true,
                    format!(
                        "{} is still working after {} s; no reply yet",
                        child.attach.label,
                        waiter.seconds.unwrap_or_default()
                    ),
                ));
            }
            return self
                .attach_agent_child(&waiter.handle)
                .err()
                .map(|error| (false, error));
        };
        if let Err(error) = self.validate_child_identity(&waiter.handle, i) {
            return Some((false, error));
        }
        if !waiter.background
            && !waiter.background_watch
            && let Some(text) = self.take_forwarded_question(&waiter.handle)
        {
            waiter.status = "attention";
            return Some((true, text));
        }
        let forwarding_question = self.has_forwardable_question(&waiter.handle);
        let activity = self.sess.managed[i].ctl.activity();
        let refreshing_question =
            self.question_attention_refresh(&waiter.handle, activity == Activity::Attention, now);
        let m = &self.sess.managed[i];
        if waiter.file.is_none() {
            waiter.file = m.ctl.session_path().map(ToOwned::to_owned);
        }
        let injecting = m.ctl.is_busy();
        waiter
            .freshness
            .observe(activity != Activity::Busy && !injecting);
        let completed = complete(
            &waiter.scan,
            activity,
            injecting,
            waiter.exact,
            waiter.freshness.after_idle(),
        );
        if m.exited() {
            if !waiter.exit_seen {
                waiter.exit_seen = true;
                waiter.scanned = false;
                waiter.fallback = waiter.baseline.is_none();
            }
            if !waiter.in_flight && (waiter.scanned || waiter.file.is_none()) {
                waiter.status = "exited";
                let line = format!(
                    "{} exited (code {})",
                    m.display_label(),
                    m.ctl
                        .terminal()
                        .exit_code()
                        .map_or_else(|| "unknown".to_owned(), |code| code.to_string())
                );
                return Some((
                    true,
                    if waiter.scan.end.is_some() {
                        with_reply(line, &waiter.scan, waiter.file.as_deref())
                    } else {
                        line
                    },
                ));
            }
        } else if activity == Activity::Attention
            && (!waiter.background || waiter.scanned || waiter.baseline.is_none())
            && !(waiter.background && completed)
            && !forwarding_question
            && !refreshing_question
        {
            waiter.status = "attention";
            let question = m
                .live
                .as_ref()
                .filter(|link| link.client.ui_hook())
                .and_then(|link| link.client.latest())
                .and_then(|snapshot| snapshot.pending_ui.first().map(|ui| ui.title.clone()))
                .unwrap_or_default();
            return Some((
                true,
                format!(
                    "{} needs the user's attention: {question}",
                    m.display_label()
                ),
            ));
        } else if completed {
            waiter.status = match waiter.scan.end {
                Some(TurnOutcome::Error(_)) => "error",
                Some(TurnOutcome::Aborted) => "aborted",
                _ => "completed",
            };
            return Some((
                true,
                ended(
                    &waiter.scan,
                    &m.display_label(),
                    m.current_cwd(),
                    waiter.file.as_deref(),
                ),
            ));
        } else if waiter.baseline.is_none() && waiter.scanned && !waiter.in_flight {
            waiter.status = "idle";
            return Some((
                true,
                with_reply(
                    format!(
                        "{} is {activity:?} in {}.",
                        m.display_label(),
                        home_path(m.current_cwd())
                    ),
                    &waiter.scan,
                    waiter.file.as_deref(),
                ),
            ));
        } else if waiter.until.is_some_and(|until| now >= until) {
            waiter.status = "timeout";
            return Some((
                true,
                format!(
                    "{} is still working after {} s{}",
                    m.display_label(),
                    waiter.seconds.unwrap_or_default(),
                    if waiter.scan.reply.is_none() {
                        "; no reply yet"
                    } else {
                        ""
                    }
                ),
            ));
        }
        // The background tool inspects one scan, then hands this bound waiter to the
        // registry instead of waiting for another worker or retaining a pending tool.
        if waiter.background && waiter.scanned && !waiter.in_flight {
            return None;
        }
        if waiter.in_flight || now < waiter.next {
            return None;
        }
        waiter.next = now + POLL;
        if waiter.fallback {
            let path = waiter.file.clone();
            waiter.in_flight = true;
            self.spawn_worker("agent-last-reply", move || {
                let result = (|| {
                    let Some(path) = path else {
                        return Ok((String::new(), 0));
                    };
                    let mut reader = SessionStatsReader::new(path);
                    if let Err(error) = reader.refresh() {
                        if error.kind() == io::ErrorKind::NotFound {
                            return Ok((String::new(), 0));
                        }
                        return Err(error.to_string());
                    }
                    let text = reader
                        .stats()
                        .last_assistant_text
                        .as_deref()
                        .unwrap_or_default();
                    let reply = utf8_prefix(text, REPLY_CAP);
                    Ok((reply.to_owned(), text[reply.len()..].chars().count()))
                })();
                Fin::AgentChildren(WorkerResult::Reply { request, result })
            });
        } else {
            if waiter.watch.is_none()
                && let (Some(file), Some(last)) = (&waiter.file, &waiter.baseline)
            {
                waiter.watch = Some(TurnWatch::new(
                    file.clone(),
                    last.offset,
                    Some(last.prompt.clone()),
                ));
            }
            if let Some(mut watch) = waiter.watch.take() {
                waiter.in_flight = true;
                waiter.freshness.start();
                self.spawn_worker("agent-turn-scan", move || {
                    let result = watch.poll();
                    Fin::AgentChildren(WorkerResult::Scan {
                        request,
                        watch,
                        result,
                    })
                });
            }
        }
        None
    }

    fn child_attach_ready(&self, index: usize, action: &Action) -> bool {
        let m = &self.sess.managed[index];
        if matches!(action, Action::Wait { .. }) || m.exited() {
            return true;
        }
        let terminal = m.ctl.terminal();
        if !terminal.accepts_input() {
            return false;
        }
        if m.view == SessionView::Chat {
            let tsp = terminal.tsp();
            tsp.live() && tsp.with(|document| document.program().is_some())
        } else {
            m.ctl.activity() != Activity::Unknown && terminal.paste_bytes("x").is_some()
        }
    }

    pub(in crate::gateway::engine) fn step_child_tools(&mut self, now: Instant) {
        self.sync_child_relocations(now);
        for child in self
            .agent_tools
            .children
            .entries
            .values_mut()
            .filter(|c| c.restored)
        {
            if let Some(m) = self
                .sess
                .managed
                .iter()
                .find(|m| m.ctl.session_path() == child.file.as_deref() && !m.exited())
            {
                child.key = m.ctl.target().key.clone();
            }
            if let Some(m) =
                self.sess.managed.iter().find(|m| {
                    m.ctl.session_path() == Some(child.opener_root.as_path()) && !m.exited()
                })
            {
                child.opener = OpenerSource::from_managed(m);
            }
        }
        // Learn a fresh child's file even if the opener waits only after it closed the tab.
        for child in self.agent_tools.children.entries.values_mut() {
            if child.file.is_none()
                && let Some(i) = self.sess.find(&child.key)
            {
                child.file = self.sess.managed[i]
                    .ctl
                    .session_path()
                    .map(ToOwned::to_owned);
            }
        }
        self.step_child_questions(now);
        let ids: Vec<_> = self.agent_tools.operations.keys().copied().collect();
        for request in ids {
            let Some(mut operation) = self.agent_tools.operations.remove(&request) else {
                continue;
            };
            let mut result = None;
            match &mut operation {
                Operation::Attach {
                    handle,
                    action,
                    until,
                } => {
                    if self
                        .sess
                        .find(&self.agent_tools.children.entries[handle].key)
                        .is_some_and(|i| self.child_attach_ready(i, action))
                    {
                        let Operation::Attach { handle, action, .. } = operation else {
                            unreachable!()
                        };
                        let child_key = self
                            .agent_tools
                            .children
                            .entries
                            .get(&handle)
                            .map(|child| child.key.clone());
                        if let Err(error) = self.start_child_action(request, handle, action, now) {
                            self.finish_agent_tool(request, false, error);
                            if let Some(key) = child_key {
                                self.release_idle_child_attachment(&key);
                            }
                        }
                        continue;
                    }
                    if now >= *until {
                        result = Some(match action {
                            Action::Wait {
                                seconds: Some(seconds),
                                until: Some(until),
                                ..
                            } if now >= *until => (
                                true,
                                format!(
                                    "{} is still working after {seconds} s; no reply yet",
                                    self.agent_tools.children.entries[handle].attach.label
                                ),
                            ),
                            _ => (false, "Could not attach to the child session".to_owned()),
                        });
                    }
                }
                Operation::Preparing { until, .. } => {
                    if now >= *until {
                        result = Some((
                            false,
                            "The prompt could not be delivered within 30 seconds".to_owned(),
                        ));
                    }
                }
                Operation::Send(send) => {
                    let SendOp {
                        handle,
                        target,
                        id,
                        baseline,
                        until,
                        ..
                    } = send.as_mut();
                    match self.valid_child(handle) {
                        Err(error) => result = Some((false, error)),
                        Ok(i) => {
                            let ctl = &mut self.sess.managed[i].ctl;
                            if ctl.target() != target {
                                result = Some((false, STALE.to_owned()));
                            } else if let Some(record) = ctl.request(*id) {
                                match &record.delivery {
                                    Delivery::Delivered => {
                                        self.agent_tools
                                            .children
                                            .entries
                                            .get_mut(handle)
                                            .expect("child")
                                            .last = Some(baseline.clone());
                                        result = Some((
                                            true,
                                            format!(
                                                "Sent to {} ({}). Use ompdeck_wait_session to get the reply.",
                                                self.sess.managed[i].display_label(),
                                                home_path(self.sess.managed[i].current_cwd())
                                            ),
                                        ));
                                    }
                                    Delivery::AwaitingConfirm { .. } => {
                                        ctl.answer(target, *id, false);
                                        result = Some((false, DRAFT.to_owned()));
                                    }
                                    Delivery::Refused(Refusal::DialogOpen) => {
                                        result = Some((false, DIALOG.to_owned()))
                                    }
                                    Delivery::Refused(Refusal::Precondition(_)) => {
                                        result = Some((
                                            false,
                                            if ctl.activity() == Activity::Busy {
                                                BUSY
                                            } else {
                                                IDLE
                                            }
                                            .to_owned(),
                                        ))
                                    }
                                    delivery if delivery.is_final() => {
                                        result = Some((
                                            false,
                                            format!("The prompt was not delivered: {delivery:?}"),
                                        ))
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    if result.is_none() && now >= *until {
                        result = Some((
                            false,
                            "The prompt could not be delivered within 30 seconds".to_owned(),
                        ));
                    }
                }
                Operation::Wait(waiter) => {
                    result = self.step_child_waiter(request, waiter, now);
                    if waiter.background
                        && waiter.baseline.is_some()
                        && result.is_some()
                        && !matches!(waiter.status, "completed" | "error" | "aborted")
                    {
                        let status = waiter.status;
                        let (ok, text) = result.take().expect("background result");
                        let Operation::Wait(waiter) = operation else {
                            unreachable!()
                        };
                        self.arm_background_wait(request, waiter);
                        self.finish_child_wait(request, WaitResult::new(ok, text, status));
                        continue;
                    }
                    if result.is_none()
                        && waiter.background
                        && !waiter.in_flight
                        && (waiter.scanned || (waiter.baseline.is_some() && waiter.file.is_none()))
                    {
                        let Operation::Wait(waiter) = operation else {
                            unreachable!()
                        };
                        self.arm_background_wait(request, waiter);
                        continue;
                    }
                }
            }
            self.agent_tools.operations.insert(request, operation);
            if let Some((ok, text)) = result {
                self.finish_agent_tool(request, ok, text);
            }
        }
        self.step_background_waits(now);
        self.step_fan_outs(now);
        self.deliver_child_questions();
        self.step_child_reports(now);
        self.step_child_finish_messages(now);
        let pruned: Vec<_> = self
            .agent_tools
            .orchestration
            .pending_prunes
            .iter()
            .filter(|handle| !self.agent_tools.child_registry_referenced(handle))
            .cloned()
            .collect();
        for handle in pruned {
            self.agent_tools.children.entries.remove(&handle);
            self.agent_tools
                .orchestration
                .pending_prunes
                .remove(&handle);
        }
        if !self.agent_tools.operations.is_empty() {
            self.wake_within(POLL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::ServiceEpoch;
    use deck_core::config::SessionView;
    #[test]
    fn restore_defers_deliveries_and_pending_acknowledgements_block_idle_admission() {
        let mut tools = crate::gateway::engine::agent_tools::AgentTools::default();
        let target = SessionTarget {
            epoch: ServiceEpoch([5; 16]),
            key: child().key,
            generation: 1,
        };
        let command = deck_core::ctl::CtlCommand::Deliver {
            root: PathBuf::from("/opener.jsonl"),
            title: "Fan-out finished".into(),
            text: "Combined result".into(),
            details: None,
        };
        assert!(
            tools
                .admit_delivery(target.clone(), command, true)
                .is_none()
        );
        assert!(tools.delivery_pending(&target));
        assert!(!tools.delivery_awaiting_ack(&target));
        assert!(tools.controller_needed(&target.key));
        let (stored_target, command) = tools.deferred_deliveries.pop_front().unwrap();
        let (stored_target, _) = tools.admit_delivery(stored_target, command, false).unwrap();
        assert_eq!(stored_target, target);
        assert!(!tools.delivery_pending(&target));
        tools.reply_acks.push((target.clone(), 88));
        assert!(tools.delivery_pending(&target));
        assert!(tools.delivery_awaiting_ack(&target));
        let other_generation = SessionTarget {
            generation: 2,
            ..target.clone()
        };
        assert!(!tools.delivery_pending(&other_generation));
        tools.acknowledge_reply(&target.key, 88);
        assert!(!tools.delivery_pending(&target));
    }

    pub(super) fn child() -> ChildSession {
        ChildSession {
            opener_root: PathBuf::from("opener.jsonl"),
            opener: OpenerSource {
                key: SessionKey::host("test", "opener"),
                mux_name: Some("opener".into()),
                backend: Some(MuxKind::Host),
            },
            key: SessionKey::host("test", "child"),
            file: Some(PathBuf::from("child.jsonl")),
            last: None,
            background: None,
            background_call: None,
            forward_questions: false,
            questions: QuestionWatch::new(Some("child".into())),
            attach: LaunchSpec {
                agent_child: false,
                mux_name: Some("child".into()),
                cwd: PathBuf::from("."),
                session_path: None,
                label: "child".into(),
                argv: vec![],
                role: SessionRole::Session,
                copy_of: None,
                view: SessionView::Chat,
            },
            backend: Some(MuxKind::Host),
            attaching: false,
            invalidated: false,
            reports: ReportRate::new(Instant::now()),
            last_report: None,
            title: "child".into(),
            opened_at_unix_ms: 0,
            file_seen: false,
            fan_out: None,
            restored: false,
            relocation: None,
        }
    }

    #[test]
    fn shared_reply_rendering_caps_utf8_bytes_and_counts_all_omitted_characters() {
        let mut scan = empty_scan();
        scan.reply = Some(format!("a{}", "🦀".repeat(REPLY_CAP / 4 + 1)));
        scan.reply_more = 7;
        let prefix = utf8_prefix(scan.reply.as_deref().unwrap(), REPLY_CAP);
        assert!(prefix.len() <= REPLY_CAP);
        assert_eq!(prefix.len(), REPLY_CAP - 3);
        let text = with_reply("finished".into(), &scan, Some(Path::new("child.jsonl")));
        assert!(text.ends_with("\n[… 9 more characters; full text in child.jsonl]"));
        assert!(text.len() < 40 * 1024);
    }

    #[test]
    fn handles_are_owned_by_the_opener_root() {
        let mut registry = Registry::default();
        let handle = registry.insert(child()).expect("random handle");
        assert_eq!(handle.len(), 10);
        assert!(handle.starts_with("s-"));
        assert_eq!(
            registry
                .owned(&handle, Path::new("other.jsonl"))
                .err()
                .as_deref(),
            Some(UNKNOWN)
        );
        assert!(registry.owned(&handle, Path::new("opener.jsonl")).is_ok());
        assert!(
            registry
                .owned("s-unknown", Path::new("opener.jsonl"))
                .is_err()
        );
        assert_ne!(registry.insert(child()).expect("second handle"), handle);
    }

    #[test]
    fn generation_changes_keep_handles_but_file_changes_do_not() {
        let child = child();
        let mut target = SessionTarget {
            epoch: ServiceEpoch::current(),
            key: child.key.clone(),
            generation: 1,
        };
        assert!(child.matches(&target, Some(Path::new("child.jsonl"))));
        target.generation = 2;
        assert!(child.matches(&target, Some(Path::new("child.jsonl"))));
        assert!(!child.matches(&target, Some(Path::new("other.jsonl"))));
        assert!(!child.matches(&target, None));
        target.key = SessionKey::host("test", "someone-else");
        assert!(!child.matches(&target, Some(Path::new("child.jsonl"))));
    }

    #[test]
    fn first_file_discovery_keeps_the_handle() {
        let mut child = child();
        child.file = None;
        let target = SessionTarget {
            epoch: ServiceEpoch::current(),
            key: child.key.clone(),
            generation: 1,
        };
        assert!(child.matches(&target, None));
        assert!(child.matches(&target, Some(Path::new("child.jsonl"))));
    }

    #[test]
    fn wait_needs_an_end_and_idle_not_just_a_user_message() {
        let mut scan = empty_scan();
        scan.prompt_seen = true;
        assert!(!complete(&scan, Activity::Idle, false, true, true));
        scan.end = Some(TurnOutcome::Completed);
        assert!(!complete(&scan, Activity::Busy, false, true, true));
        assert!(!complete(&scan, Activity::Idle, true, true, true));
        assert!(complete(&scan, Activity::Idle, false, true, true));
    }

    #[test]
    fn a_later_prompt_can_be_busy_without_delaying_the_previous_reply() {
        let mut scan = empty_scan();
        scan.end = Some(TurnOutcome::Completed);
        scan.later_prompt = true;
        assert!(complete(&scan, Activity::Busy, true, true, true));
    }

    #[test]
    fn a_short_turn_needs_no_observed_busy_transition() {
        let mut scan = empty_scan();
        scan.end = Some(TurnOutcome::Completed);
        scan.reply = Some("already finished".into());
        assert!(complete(&scan, Activity::Idle, false, true, true));
        assert!(ended(&scan, "child", Path::new("."), None).ends_with("\n\nalready finished"));
    }

    #[test]
    fn live_conversation_identity_overrides_the_cached_controller_file() {
        let old = Path::new("old.jsonl");
        let new = Path::new("new.jsonl");
        assert_eq!(
            conversation_file(Some(old), [Some(Some(new)), None], &[]),
            Ok(Some(new))
        );
        assert_eq!(
            conversation_file(Some(old), [Some(None), None], &[]),
            Ok(None)
        );
        let running = [new.to_path_buf()];
        assert_eq!(
            conversation_file(Some(old), [None, None], &running),
            Ok(Some(new))
        );
        assert_eq!(
            conversation_file(Some(old), [None, None], &[]),
            Ok(Some(old))
        );
    }

    #[test]
    fn either_live_link_switching_conversation_invalidates_the_child() {
        let child = child();
        let old = child.file.as_deref().expect("recorded root");
        let new = Path::new("new.jsonl");
        for links in [
            [Some(Some(new)), Some(Some(old))],
            [Some(Some(old)), Some(Some(new))],
            [Some(None), Some(Some(old))],
            [Some(Some(old)), Some(None)],
        ] {
            assert_eq!(conversation_file(Some(old), links, &[]), Err(()));
        }
        assert_eq!(
            conversation_file(Some(old), [Some(Some(old)), Some(Some(old))], &[]),
            Ok(Some(old))
        );
        for links in [[None, Some(Some(new))], [Some(Some(new)), None]] {
            let current = conversation_file(Some(old), links, &[]).expect("one authoritative link");
            let target = SessionTarget {
                epoch: ServiceEpoch::current(),
                key: child.key.clone(),
                generation: 9,
            };
            assert!(!child.matches(&target, current));
        }
    }

    #[test]
    fn queued_prompt_b_does_not_finish_with_fallback_a_while_c_is_busy() {
        use std::io::Write;
        let dir = tempfile::tempdir().expect("temporary transcript");
        let path = dir.path().join("turns.jsonl");
        let user = |text| {
            serde_json::json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":text}]}}).to_string()
        };
        let assistant = |text| {
            serde_json::json!({"type":"message","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":text}]}}).to_string()
        };
        std::fs::write(
            &path,
            format!("{}\n{}\n{}\n", user("A"), assistant("reply A"), user("C")),
        )
        .expect("A finished, C started");
        let mut watch = TurnWatch::new(path.clone(), 0, Some("B".into()));
        let fallback = watch.poll().expect("fallback scan");
        assert!(fallback.end.is_some() && fallback.later_prompt);
        assert!(!watch.matched_exactly());
        assert!(!complete(
            &fallback,
            Activity::Busy,
            false,
            watch.matched_exactly(),
            true
        ));
        assert!(!complete(
            &fallback,
            Activity::Idle,
            true,
            watch.matched_exactly(),
            true
        ));
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("append transcript");
        writeln!(
            file,
            "{}\n{}\n{}",
            assistant("reply C"),
            user("B"),
            assistant("reply B")
        )
        .expect("B persisted and finished");
        let mut freshness = ScanFreshness::default();
        freshness.start();
        freshness.finish(); // Cached A/C scan; B then finishes before the next poll.
        freshness.observe(true);
        assert!(!complete(
            &fallback,
            Activity::Idle,
            false,
            false,
            freshness.after_idle()
        ));
        freshness.start();
        let exact = watch.poll().expect("exact scan");
        freshness.finish();
        assert!(watch.matched_exactly());
        assert_eq!(exact.reply.as_deref(), Some("reply B"));
        assert!(complete(
            &exact,
            Activity::Idle,
            false,
            watch.matched_exactly(),
            freshness.after_idle()
        ));
    }

    #[test]
    fn idle_requires_a_scan_started_after_the_idle_observation_without_observed_busy() {
        let mut freshness = ScanFreshness::default();
        freshness.start(); // An in-flight read began before the first idle observation.
        freshness.observe(true);
        freshness.finish();
        assert!(!freshness.after_idle());
        freshness.start();
        freshness.finish();
        assert!(freshness.after_idle());
        let mut short = ScanFreshness::default();
        short.observe(true);
        short.start();
        short.finish();
        assert!(short.after_idle());
    }

    #[test]
    fn failed_headless_preparation_releases_ownership_even_after_operation_was_removed() {
        let mut tools = super::super::AgentTools::default();
        let mut child = child();
        child.attaching = true;
        let key = child.key.clone();
        let handle = tools.children.insert(child).expect("child handle");
        tools.operations.insert(
            1,
            Operation::Preparing {
                handle: handle.clone(),
                prompt: "B".into(),
                mode: ComposeMode::Send,
                until: Instant::now() + DELIVERY,
            },
        );
        assert!(
            !tools.release_child_ownership(&key),
            "active preparation owns attachment"
        );
        tools.operations.remove(&1); // Same state as Busy/refusal or Attach→action failure.
        assert!(
            tools.release_child_ownership(&key),
            "error cleanup must release attachment"
        );
        assert!(!tools.children.entries[&handle].attaching);
    }

    #[test]
    fn a_childs_own_pending_tool_retains_its_controller_through_reply_acknowledgment() {
        let mut tools = super::super::AgentTools::default();
        let key = SessionKey::host("test", "nested-opener");
        let target = SessionTarget {
            epoch: ServiceEpoch::current(),
            key: key.clone(),
            generation: 1,
        };
        tools.pending.insert(
            1,
            super::super::Pending {
                target: target.clone(),
                tool_id: 7,
                root: Some(PathBuf::from("nested-opener.jsonl")),
                deadline: None,
                review: None,
                answering: false,
                opens_session: false,
                open_prompt: None,
                forward_questions: true,
                worktree_result: None,
            },
        );
        assert!(
            tools.operations.is_empty(),
            "no operation needs to target this child"
        );
        assert!(tools.controller_needed(&key));
        assert!(!tools.release_child_ownership(&key));
        let pending = tools.pending.remove(&1).expect("cancel request");
        assert!(
            !tools.controller_needed(&key),
            "cancelled requests release ownership"
        );

        tools.pending.insert(1, pending);
        tools.reply_acks.push((target, 42));
        tools.pending.remove(&1); // A successful request hands ownership to its queued result.
        assert!(tools.controller_needed(&key));
        assert!(!tools.release_child_ownership(&key));
        tools.acknowledge_reply(&key, 41);
        assert!(
            tools.controller_needed(&key),
            "unrelated replies do not release ownership"
        );
        tools.acknowledge_reply(&key, 42);
        assert!(!tools.controller_needed(&key));
        assert!(tools.release_child_ownership(&key));
    }
}
