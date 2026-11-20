//! Per-agent incremental fold of a subagent transcript, status inference
//! (docs/notes/agent-hub.md §3) and the directory scan that yields [`AgentsSnapshot`]s.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use super::tail::{
    self, LineTail, allocation_weight, cap_bytes, line_timestamp, line_type, one_line, simple_str,
};
use super::{AgentRow, AgentStatus, AgentSummary, AgentsSnapshot};

/// Full assignment text kept per agent.
const MAX_TASK: usize = 64 * 1024;
/// [`AgentRow::task_gist`] cap (chars).
const MAX_GIST_CHARS: usize = 1000;
/// Cap of [`AgentRow::error`].
const MAX_ERROR: usize = 2000;
/// Pending (started, not yet answered) tool calls remembered per agent.
const MAX_PENDING: usize = 64;
/// Nested agent directories are followed this deep.
const MAX_DIR_DEPTH: usize = 8;
/// A `message` line states its role within this many leading bytes.
const ROLE_PEEK: usize = 600;

/// What the last meaningful message of a transcript was.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Last {
    #[default]
    Nothing,
    /// A user message, a tool result, or an assistant turn that asks for tools.
    Open,
    Stop,
    Error,
    Aborted,
    /// `yield` toolResult with `details.status == "success"`.
    YieldOk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Exit {
    /// `session_exit` kind signal/fatal/process_exit.
    Abnormal,
    /// `dispose`/`normal`.
    Normal,
}

/// Everything folded out of one transcript; reset wholesale when the file is replaced.
#[derive(Default)]
struct Facts {
    created: Option<SystemTime>,
    last_ts: Option<SystemTime>,
    has_init: bool,
    messages: u32,
    agent: Option<String>,
    resolved_model: Option<String>,
    model: Option<String>,
    /// `session_init.modelRole` when present (wins over everything below).
    init_role: Option<String>,
    /// Latest known `model_change.role` (junk roles never land here).
    change_role: Option<String>,
    assistant_model: Option<String>,
    thinking: Option<String>,
    task: Arc<str>,
    requests: u32,
    tool_calls: u32,
    tokens: u64,
    cost: f64,
    context_tokens: Option<u64>,
    last: Last,
    last_error: Option<String>,
    /// `(toolCallId, description)` of `tool_execution_start`s without a toolResult yet.
    pending: Vec<(String, String)>,
    exit: Option<Exit>,
}

pub(crate) struct AgentState {
    tail: LineTail,
    facts: Facts,
    row: Option<AgentSummary>,
    /// Position of `row` in the last published vector, if it had one.
    index: usize,
    seen: bool,
}

impl AgentState {
    pub fn new(path: PathBuf) -> Self {
        AgentState {
            tail: LineTail::new(path),
            facts: Facts::default(),
            row: None,
            index: 0,
            seen: false,
        }
    }

    /// Reads whatever was appended; `Ok(true)` when anything was consumed.
    pub fn refresh(&mut self) -> std::io::Result<bool> {
        let mut changed = false;
        loop {
            let chunk = self.tail.poll()?;
            if chunk.reset {
                self.facts = Facts::default();
                changed = true;
            }
            for line in tail::lines(&chunk.data) {
                self.facts.apply_line(line);
                changed = true;
            }
            if !chunk.more {
                return Ok(changed);
            }
        }
    }

    /// A file with only title/session lines is an incomplete stub omp itself skips.
    fn is_stub(&self) -> bool {
        !self.facts.has_init && self.facts.messages == 0
    }
}

// ---------------------------------------------------------------------------------------------
// Line folding

#[derive(Deserialize)]
struct ModelChange {
    model: Option<String>,
    role: Option<String>,
}

#[derive(Deserialize)]
struct ThinkingChange {
    #[serde(rename = "thinkingLevel")]
    level: Option<String>,
}

#[derive(Deserialize)]
struct SessionInit {
    task: Option<String>,
    agent: Option<String>,
    #[serde(rename = "resolvedModel")]
    resolved_model: Option<String>,
    #[serde(rename = "modelRole")]
    model_role: Option<String>,
}

#[derive(Deserialize)]
struct MessageEntry<'a> {
    #[serde(borrow)]
    message: Message<'a>,
}

#[derive(Deserialize)]
struct Message<'a> {
    #[serde(borrow, default)]
    content: Option<ContentLite<'a>>,
    provider: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
    #[serde(rename = "stopReason")]
    stop_reason: Option<String>,
    #[serde(rename = "errorMessage")]
    error_message: Option<String>,
    #[serde(rename = "isError")]
    is_error: Option<bool>,
    details: Option<Details>,
}

/// Only the blocks' `type` (tool-call counting) or a plain/part text (first-user fallback).
#[derive(Deserialize)]
#[serde(untagged)]
enum ContentLite<'a> {
    Parts(#[serde(borrow)] Vec<PartLite<'a>>),
    Text(String),
}

#[derive(Deserialize)]
struct PartLite<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<Cow<'a, str>>,
    text: Option<String>,
}

#[derive(Deserialize)]
struct Details {
    status: Option<String>,
}

#[derive(Deserialize, Default)]
struct Usage {
    input: Option<f64>,
    output: Option<f64>,
    #[serde(rename = "cacheWrite")]
    cache_write: Option<f64>,
    #[serde(rename = "totalTokens")]
    total_tokens: Option<f64>,
    cost: Option<UsageCost>,
}

#[derive(Deserialize)]
struct UsageCost {
    total: Option<f64>,
}

#[derive(Deserialize)]
struct CustomLine {
    data: Option<CustomData>,
}

#[derive(Deserialize)]
struct CustomData {
    #[serde(rename = "toolCallId")]
    tool_call_id: Option<String>,
    #[serde(rename = "toolName")]
    tool_name: Option<String>,
    intent: Option<String>,
    args: Option<ArgsLite>,
    kind: Option<String>,
}

/// The few argument fields worth showing as "current tool"; everything else is skipped unparsed.
#[derive(Deserialize)]
struct ArgsLite {
    command: Option<serde_json::Value>,
    path: Option<serde_json::Value>,
    pattern: Option<serde_json::Value>,
    query: Option<serde_json::Value>,
    url: Option<serde_json::Value>,
}

fn parse<'a, T: Deserialize<'a>>(line: &'a [u8], what: &str) -> Option<T> {
    match serde_json::from_slice(line) {
        Ok(v) => Some(v),
        Err(e) => {
            log::debug!(
                "agents: skipping malformed {what} line ({} bytes): {e}",
                line.len()
            );
            None
        }
    }
}

impl Facts {
    fn set_task(&mut self, task: String) {
        let (task, _) = cap_bytes(task, MAX_TASK);
        if self.task.as_ref() != task.as_str() {
            self.task = Arc::from(task);
        }
    }

    fn model(&self) -> Option<&str> {
        self.model
            .as_deref()
            .or(self.resolved_model.as_deref())
            .or(self.assistant_model.as_deref())
    }

    fn model_role(&self) -> Option<&str> {
        self.init_role.as_deref().or(self.change_role.as_deref())
    }

    // Only compare the row whose fold received complete lines (or reset). Status and
    // output sidecars are checked separately, including on idle polls.
    fn matches_row(&self, row: &AgentRow, inferred: &Inferred<'_>) -> bool {
        Arc::ptr_eq(&self.task, &row.task)
            && self.created == row.created
            && self.last_ts == row.last_activity
            && self.requests == row.requests
            && self.tool_calls == row.tool_calls
            && self.tokens == row.tokens
            && self.cost.to_bits() == row.cost.to_bits()
            && self.context_tokens == row.context_tokens
            && self.agent == row.agent
            && self.model() == row.model.as_deref()
            && self.model_role() == row.model_role.as_deref()
            && self.thinking == row.thinking
            && inferred.error == row.error.as_deref()
            && inferred.current == row.current.as_deref()
    }

    fn heap_weight(&self) -> usize {
        let strings = [
            &self.agent,
            &self.resolved_model,
            &self.model,
            &self.init_role,
            &self.change_role,
            &self.assistant_model,
            &self.thinking,
            &self.last_error,
        ];
        allocation_weight(size_of::<[usize; 2]>() + self.task.len())
            + strings
                .into_iter()
                .flatten()
                .map(|s| allocation_weight(s.capacity()))
                .sum::<usize>()
            + allocation_weight(self.pending.capacity() * size_of::<(String, String)>())
            + self
                .pending
                .iter()
                .map(|(id, desc)| {
                    allocation_weight(id.capacity()) + allocation_weight(desc.capacity())
                })
                .sum::<usize>()
    }

    fn apply_line(&mut self, line: &[u8]) {
        let Some(kind) = line_type(line) else {
            return;
        };
        if kind == "title" {
            return;
        }
        if let Some(ts) = line_timestamp(line) {
            if kind == "session" && self.created.is_none() {
                self.created = Some(ts);
            }
            let activity = match kind {
                "message" | "custom_message" | "compaction" | "model_usage" => true,
                "custom" => simple_str(line, "customType", 120) == Some("tool_execution_start"),
                _ => false,
            };
            if activity && self.last_ts.is_none_or(|t| ts > t) {
                self.last_ts = Some(ts);
            }
        }
        match kind {
            "model_change" => {
                // One parse of a line this arm already parses for `model`; the role rides
                // along for free. Junk roles (`subagent:*`, `temporary`, `fallback`,
                // `prewalk`, …) are dropped so the hub badge never shows them.
                if let Some(change) = parse::<ModelChange>(line, kind) {
                    if let Some(m) = change.model {
                        self.model = Some(m);
                    }
                    if change.role.as_deref().is_some_and(known_role) {
                        self.change_role = change.role;
                    }
                }
            }
            "thinking_level_change" => {
                if let Some(t) = parse::<ThinkingChange>(line, kind).and_then(|t| t.level) {
                    self.thinking = Some(t);
                }
            }
            "session_init" => {
                if let Some(init) = parse::<SessionInit>(line, kind) {
                    self.has_init = true;
                    if let Some(task) = init.task {
                        self.set_task(task);
                    }
                    self.agent = init.agent.or(self.agent.take());
                    self.resolved_model = init.resolved_model.or(self.resolved_model.take());
                    // `session_init.modelRole` wins over any `model_change.role`.
                    if let Some(role) = init.model_role
                        && !role.trim().is_empty()
                    {
                        self.init_role = Some(role);
                    }
                }
            }
            "message" => self.apply_message(line),
            "custom" => self.apply_custom(line),
            _ => {}
        }
    }

    fn apply_message(&mut self, line: &[u8]) {
        let head = &line[..line.len().min(ROLE_PEEK)];
        let role = if memchr::memmem::find(head, b"\"role\":\"assistant\"").is_some() {
            "assistant"
        } else if memchr::memmem::find(head, b"\"role\":\"toolResult\"").is_some() {
            "toolResult"
        } else if memchr::memmem::find(head, b"\"role\":\"user\"").is_some() {
            "user"
        } else {
            // developer / custom / unknown: not part of the conversation flow.
            return;
        };
        self.messages += 1;
        match role {
            "assistant" => self.apply_assistant(line),
            "toolResult" => self.apply_tool_result(line),
            _ => {
                self.last = Last::Open;
                // A new user turn revives a disposed agent (follow-up chat).
                if self.exit == Some(Exit::Normal) {
                    self.exit = None;
                }
                if self.task.is_empty()
                    && let Some(entry) = parse::<MessageEntry>(line, "user")
                {
                    self.set_task(content_text(entry.message.content));
                }
            }
        }
    }

    fn apply_assistant(&mut self, line: &[u8]) {
        let Some(entry) = parse::<MessageEntry>(line, "assistant") else {
            return;
        };
        let m = entry.message;
        self.requests += 1;
        if let Some(ContentLite::Parts(parts)) = &m.content {
            let calls = parts
                .iter()
                .filter(|p| p.kind.as_deref() == Some("toolCall"))
                .count();
            self.tool_calls += u32::try_from(calls).unwrap_or(u32::MAX);
        }
        if let Some(u) = &m.usage {
            let (i, o, w) = (num(u.input), num(u.output), num(u.cache_write));
            let total = num(u.total_tokens);
            let sum = i + o + w;
            self.tokens += if sum > 0 { sum } else { total };
            self.cost += u.cost.as_ref().and_then(|c| c.total).unwrap_or(0.0);
            if total > 0 {
                self.context_tokens = Some(total);
            }
        }
        if self.assistant_model.is_none()
            && let (Some(p), Some(id)) = (&m.provider, &m.model)
        {
            self.assistant_model = Some(format!("{p}/{id}"));
        }
        self.last = match m.stop_reason.as_deref() {
            Some("stop") => Last::Stop,
            Some("error") => Last::Error,
            Some("aborted") => Last::Aborted,
            _ => Last::Open,
        };
        if matches!(self.last, Last::Error | Last::Aborted) {
            self.last_error = m
                .error_message
                .map(|e| cap_bytes(e, MAX_ERROR).0)
                .or_else(|| self.last_error.take());
        }
    }

    fn apply_tool_result(&mut self, line: &[u8]) {
        if let Some(id) = simple_str(line, "toolCallId", ROLE_PEEK + 200) {
            self.pending.retain(|(p, _)| p != id);
        }
        self.last = Last::Open;
        if simple_str(line, "toolName", ROLE_PEEK + 400) == Some("yield")
            && let Some(entry) = parse::<MessageEntry>(line, "yield")
            && entry.message.is_error != Some(true)
            && entry.message.details.and_then(|d| d.status).as_deref() == Some("success")
        {
            self.last = Last::YieldOk;
        }
    }

    fn apply_custom(&mut self, line: &[u8]) {
        match simple_str(line, "customType", 120) {
            Some("tool_execution_start") => {
                let Some(data) =
                    parse::<CustomLine>(line, "tool_execution_start").and_then(|c| c.data)
                else {
                    return;
                };
                let (Some(id), Some(name)) = (data.tool_call_id, data.tool_name) else {
                    return;
                };
                let hint = data
                    .intent
                    .filter(|i| !i.trim().is_empty())
                    .or_else(|| data.args.and_then(args_hint));
                let desc = match hint {
                    Some(h) => format!("{name} · {}", one_line(&h, 120)),
                    None => name,
                };
                if self.pending.len() >= MAX_PENDING {
                    self.pending.remove(0);
                }
                self.pending.push((id, desc));
            }
            Some("session_exit") => {
                let kind = parse::<CustomLine>(line, "session_exit")
                    .and_then(|c| c.data)
                    .and_then(|d| d.kind);
                self.exit = Some(match kind.as_deref() {
                    Some("signal" | "fatal" | "process_exit") => Exit::Abnormal,
                    _ => Exit::Normal,
                });
            }
            _ => {}
        }
    }
}

fn num(v: Option<f64>) -> u64 {
    v.filter(|v| v.is_finite() && *v > 0.0)
        .map_or(0, |v| v as u64)
}

/// omp's known model-role keys (`config/model-roles.ts`): only these may back the hub role
/// badge. Anything else (`subagent:<id>`, `temporary`, `fallback`, `prewalk`, …) is junk the
/// badge must never show.
fn known_role(role: &str) -> bool {
    matches!(
        role,
        "default"
            | "smol"
            | "slow"
            | "vision"
            | "plan"
            | "commit"
            | "tiny"
            | "memory"
            | "task"
            | "advisor"
            | "widget"
            | "image"
            | "web"
            | "speech"
            | "dictation"
            | "judge"
    )
}

fn args_hint(a: ArgsLite) -> Option<String> {
    [a.command, a.path, a.pattern, a.query, a.url]
        .into_iter()
        .flatten()
        .find_map(|v| match v {
            serde_json::Value::String(s) if !s.trim().is_empty() => Some(s),
            _ => None,
        })
}

fn content_text(c: Option<ContentLite>) -> String {
    match c {
        Some(ContentLite::Text(t)) => t,
        Some(ContentLite::Parts(parts)) => parts
            .into_iter()
            .filter(|p| p.kind.as_deref() == Some("text"))
            .filter_map(|p| p.text)
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------------------------
// Task text

/// Removes omp's assignment preamble (both real wordings).
pub fn strip_preamble(task: &str) -> &str {
    let t = task.trim_start();
    let rest = if let Some(r) = t.strip_prefix("Complete the assignment below,") {
        r.trim_start().strip_prefix("thoroughly:")
    } else {
        t.strip_prefix("Complete assignment thoroughly:")
    };
    rest.map_or(t, str::trim_start)
}

/// The body of the `# Target` section, if there is one.
pub(crate) fn target_section(body: &str) -> Option<&str> {
    let mut start = None;
    let mut off = 0;
    for line in body.split_inclusive('\n') {
        let l = line.trim_end_matches(['\n', '\r']);
        match start {
            None if l.trim_end() == "# Target" || l.starts_with("# Target ") => {
                start = Some(off + line.len());
            }
            Some(s) if l.starts_with("# ") => return Some(&body[s..off]),
            _ => {}
        }
        off += line.len();
    }
    start.map(|s| &body[s..])
}

/// Preamble stripped; the `# Target` section when one exists; ≤ 1000 chars.
pub(crate) fn task_gist(task: &str) -> String {
    let body = strip_preamble(task);
    let text = target_section(body).unwrap_or(body).trim();
    match text.char_indices().nth(MAX_GIST_CHARS) {
        Some((i, _)) => text[..i].trim_end().to_owned(),
        None => text.to_owned(),
    }
}

// ---------------------------------------------------------------------------------------------
// Status inference

struct Inferred<'a> {
    status: AgentStatus,
    error: Option<&'a str>,
    current: Option<&'a str>,
}

/// A finished-looking agent whose last entry is younger than this may still be writing its `.md`.
const OUTPUT_GRACE: Duration = Duration::from_secs(5);

fn infer(
    f: &Facts,
    tombstone: bool,
    owner_alive: bool,
    has_output: bool,
    now: SystemTime,
) -> Inferred<'_> {
    let error = f.last_error.as_deref();
    let done = |status, error| Inferred {
        status,
        error,
        current: None,
    };
    if tombstone {
        return done(AgentStatus::Aborted, error);
    }
    let finished = matches!(f.last, Last::Stop | Last::YieldOk);
    if f.exit == Some(Exit::Abnormal) && !finished && !matches!(f.last, Last::Error | Last::Aborted)
    {
        return done(AgentStatus::Interrupted, None);
    }
    match f.last {
        Last::Stop | Last::YieldOk => {
            let fresh = f
                .last_ts
                .is_some_and(|t| now.duration_since(t).is_ok_and(|age| age < OUTPUT_GRACE));
            if !has_output && f.exit.is_none() && owner_alive && fresh {
                Inferred {
                    status: AgentStatus::Running,
                    error: None,
                    current: None,
                }
            } else {
                done(AgentStatus::Done, None)
            }
        }
        Last::Error => done(AgentStatus::Failed, error),
        Last::Aborted => done(AgentStatus::Aborted, error),
        Last::Open | Last::Nothing => {
            if f.exit.is_some() || !owner_alive {
                done(AgentStatus::Interrupted, None)
            } else {
                Inferred {
                    status: AgentStatus::Running,
                    error: None,
                    current: f.pending.last().map(|(_, d)| d.as_str()),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Directory scan

struct Found<'a> {
    id: &'a str,
    parent: Option<&'a str>,
    tombstone: bool,
    output: Option<&'a Path>,
}

/// `<stem>/…` transcripts, recursively. Skips `.bak` files and non-transcripts.
fn discover(
    dir: &Path,
    parent: Option<&str>,
    depth: usize,
    visit: &mut impl FnMut(PathBuf, Found<'_>),
) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::debug!("agents: cannot list {}: {e}", dir.display());
            }
            return;
        }
    };
    let mut sidecars = HashSet::new();
    let mut subdirs = Vec::new();
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let name = entry.file_name();
        let Some(s) = name.to_str() else { continue };
        if ft.is_dir() && depth < MAX_DIR_DEPTH {
            subdirs.push((entry.path(), s.to_owned()));
        }
        if ft.is_file() && s.ends_with(".jsonl") && !s.contains(".bak") {
            files.push((entry.path(), name));
        } else if s.ends_with(".md") || s.ends_with(".jsonl.tombstone") {
            sidecars.insert(name);
        }
    }
    let mut sibling = OsString::new();
    let mut output_path = None;
    for (path, name) in files {
        let id = name
            .to_str()
            .and_then(|s| s.strip_suffix(".jsonl"))
            .expect("discovered transcript name");
        sibling.clear();
        sibling.push(id);
        sibling.push(".jsonl.tombstone");
        let tombstone = sidecars.contains(&sibling);
        sibling.clear();
        sibling.push(id);
        sibling.push(".md");
        let output = if sidecars.contains(&sibling) {
            let output_path = output_path.get_or_insert_with(|| dir.join(&sibling));
            output_path.set_file_name(&sibling);
            fs::symlink_metadata(output_path.as_path())
                .is_ok_and(|m| m.is_file() && m.len() > 0)
                .then_some(output_path.as_path())
        } else {
            None
        };
        visit(
            path,
            Found {
                id,
                parent,
                tombstone,
                output,
            },
        );
    }
    for (path, name) in subdirs {
        discover(&path, Some(&name), depth + 1, visit);
    }
}

enum Change {
    Unchanged,
    Row,
    Order,
}

impl AgentState {
    fn sync_row(&mut self, found: Found<'_>, owner_alive: bool) -> Change {
        let facts_changed = match self.refresh() {
            Ok(changed) if !self.is_stub() => changed,
            result => {
                if let Err(e) = result {
                    log::debug!("agents: cannot read {}: {e}", self.tail.path().display());
                }
                return if self.row.take().is_some() {
                    Change::Order
                } else {
                    Change::Unchanged
                };
            }
        };
        let previous = self.row.as_deref();
        // Replay may assign the same text to a fresh Arc; keep one shared allocation.
        if facts_changed
            && let Some(old) = previous
            && !Arc::ptr_eq(&self.facts.task, &old.task)
            && self.facts.task == old.task
        {
            self.facts.task = Arc::clone(&old.task);
        }
        let inferred = infer(
            &self.facts,
            found.tombstone,
            owner_alive,
            found.output.is_some(),
            SystemTime::now(),
        );
        if let Some(old) = previous
            && inferred.status == old.status
            && found.output == old.output_path.as_deref()
            && (!facts_changed || self.facts.matches_row(old, &inferred))
        {
            return Change::Unchanged;
        }
        // Path, id and parent are fixed for this state. Only membership and created
        // can change positions (or depths) in the ordered snapshot.
        let order_changed = previous.is_none_or(|old| old.created != self.facts.created);
        self.row = Some(Arc::new(summarize(
            &self.facts,
            found,
            self.tail.path(),
            inferred,
            previous,
        )));
        if order_changed {
            Change::Order
        } else {
            Change::Row
        }
    }
}

/// Folds `<session stem>/` into [`AgentsSnapshot`]s, keeping each transcript's tail position
/// between calls.
pub struct AgentsScanner {
    session: PathBuf,
    states: HashMap<PathBuf, AgentState>,
    agents: Arc<Vec<AgentSummary>>,
}

impl AgentsScanner {
    /// `session` is the parent's `.jsonl`; the artifact directory is the same path without
    /// the extension.
    pub fn new(session: PathBuf) -> Self {
        AgentsScanner {
            session,
            states: HashMap::new(),
            agents: Arc::default(),
        }
    }

    pub fn session(&self) -> &Path {
        &self.session
    }

    /// Rescans the directory and tails every transcript. `owner_alive`: the omp that owns the
    /// session still runs (inferred-Running agents of a dead owner become Interrupted).
    pub fn scan(&mut self, owner_alive: bool) -> AgentsSnapshot {
        let stem = self.session.with_extension("");
        let mut reorder = false;
        let mut updates = Vec::new();
        discover(&stem, None, 0, &mut |path, found| {
            let state = self
                .states
                .entry(path)
                .or_insert_with_key(|path| AgentState::new(path.clone()));
            state.seen = true;
            match state.sync_row(found, owner_alive) {
                Change::Order => reorder = true,
                Change::Row if !reorder => {
                    let row = state.row.as_ref().expect("updated row");
                    updates.push((state.index, Arc::clone(row)));
                }
                _ => {}
            }
        });
        self.states.retain(|_, state| {
            let seen = std::mem::take(&mut state.seen);
            reorder |= !seen && state.row.is_some();
            seen
        });

        if reorder {
            drop(updates);
            self.reorder();
        } else if !updates.is_empty() {
            let agents = Arc::make_mut(&mut self.agents);
            for (i, row) in updates {
                agents[i] = row;
            }
        }
        AgentsSnapshot {
            session: self.session.clone(),
            agents: Arc::clone(&self.agents),
        }
    }

    fn reorder(&mut self) {
        let mut visible: Vec<_> = self
            .states
            .values_mut()
            .filter(|s| s.row.is_some())
            .collect();
        let positions = order(&visible);
        let mut agents = Vec::with_capacity(positions.len());
        for (index, (i, depth)) in positions.into_iter().enumerate() {
            let state = &mut *visible[i];
            state.index = index;
            let row = state.row.as_mut().expect("visible row");
            if row.depth != depth {
                Arc::make_mut(row).depth = depth;
            }
            agents.push(Arc::clone(row));
        }
        self.agents = Arc::new(agents);
    }

    /// Conservative retained-size estimate for parking; not used on live polls.
    pub(super) fn cache_weight(&self) -> usize {
        // Include sparse buckets, control bytes, and allocation overhead.
        let map = self
            .states
            .capacity()
            .saturating_mul(2 * (size_of::<(PathBuf, AgentState)>() + 1));
        let base = size_of::<Self>()
            + allocation_weight(self.session.capacity())
            + allocation_weight(map)
            + allocation_weight(size_of::<Vec<AgentSummary>>() + size_of::<[usize; 2]>())
            + allocation_weight(self.agents.capacity() * size_of::<AgentSummary>());
        self.states.iter().fold(base, |total, (path, state)| {
            total.saturating_add(
                allocation_weight(path.capacity())
                    + state.tail.heap_weight()
                    + state.facts.heap_weight()
                    + state.row.as_deref().map_or(0, row_heap_weight),
            )
        })
    }
}

fn summarize(
    facts: &Facts,
    f: Found<'_>,
    path: &Path,
    inferred: Inferred<'_>,
    previous: Option<&AgentRow>,
) -> AgentRow {
    let duration = match (facts.created, facts.last_ts) {
        (Some(a), Some(b)) => b.duration_since(a).ok(),
        _ => None,
    };
    AgentRow {
        id: f.id.to_owned(),
        parent_id: f.parent.map(str::to_owned),
        depth: previous.map_or(0, |row| row.depth),
        path: path.to_owned(),
        advisor: f.id.starts_with("__advisor"),
        agent: facts.agent.clone(),
        model: facts.model().map(str::to_owned),
        model_role: facts.model_role().map(str::to_owned),
        thinking: facts.thinking.clone(),
        task_gist: previous
            .filter(|row| Arc::ptr_eq(&facts.task, &row.task))
            .map_or_else(|| task_gist(&facts.task), |row| row.task_gist.clone()),
        task: Arc::clone(&facts.task),
        status: inferred.status,
        error: inferred.error.map(str::to_owned),
        current: inferred.current.map(str::to_owned),
        created: facts.created,
        last_activity: facts.last_ts,
        duration,
        requests: facts.requests,
        tool_calls: facts.tool_calls,
        tokens: facts.tokens,
        cost: facts.cost,
        context_tokens: facts.context_tokens,
        output_path: f.output.map(Path::to_owned),
    }
}

fn row_heap_weight(row: &AgentRow) -> usize {
    let strings = [
        &row.parent_id,
        &row.agent,
        &row.model,
        &row.model_role,
        &row.thinking,
        &row.error,
        &row.current,
    ];
    // The row and its fold share the task allocation, counted in Facts.
    allocation_weight(size_of::<AgentRow>() + size_of::<[usize; 2]>())
        + allocation_weight(row.id.capacity())
        + allocation_weight(row.path.capacity())
        + allocation_weight(row.task_gist.capacity())
        + row
            .output_path
            .as_ref()
            .map_or(0, |p| allocation_weight(p.capacity()))
        + strings
            .into_iter()
            .flatten()
            .map(|s| allocation_weight(s.capacity()))
            .sum::<usize>()
}

/// Parent before child, newest first among siblings; `depth` is the tree depth (orphans whose
/// parent is not a listed agent are roots).
fn order(visible: &[&mut AgentState]) -> Vec<(usize, usize)> {
    let row = |i: usize| visible[i].row.as_deref().expect("visible row");
    let ids: HashSet<&str> = (0..visible.len()).map(|i| row(i).id.as_str()).collect();
    let mut children: HashMap<Option<&str>, Vec<usize>> = HashMap::new();
    for i in 0..visible.len() {
        let key = row(i).parent_id.as_deref().filter(|p| ids.contains(p));
        children.entry(key).or_default().push(i);
    }
    let compare = |&x: &usize, &y: &usize| {
        let (a, b) = (row(x), row(y));
        b.created
            .cmp(&a.created)
            .then_with(|| a.id.cmp(&b.id))
            .then_with(|| a.path.cmp(&b.path))
    };
    for list in children.values_mut() {
        list.sort_unstable_by(compare);
    }
    let mut order = Vec::with_capacity(visible.len());
    let mut stack: Vec<_> = children
        .remove(&None)
        .into_iter()
        .flatten()
        .rev()
        .map(|i| (i, 0))
        .collect();
    while let Some((i, depth)) = stack.pop() {
        order.push((i, depth));
        // Consume each children list once. Duplicate ids can otherwise revisit the
        // same descendants or turn an apparently rooted tree into a cycle.
        if let Some(kids) = children.remove(&Some(row(i).id.as_str())) {
            stack.extend(kids.into_iter().rev().map(|k| (k, depth + 1)));
        }
    }
    if order.len() < visible.len() {
        let mut unrooted: Vec<_> = children.into_values().flatten().collect();
        unrooted.sort_unstable_by(compare);
        order.extend(unrooted.into_iter().map(|i| (i, 0)));
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gist_prefers_target_and_strips_both_preambles() {
        let t = "Complete assignment thoroughly:\n\n# Goal\nignored\n# Target\nDo the thing.\nMore.\n# Change\nsteps";
        assert_eq!(task_gist(t), "Do the thing.\nMore.");
        let t2 = "Complete the assignment below,   thoroughly:\nPlain task";
        assert_eq!(task_gist(t2), "Plain task");
        assert_eq!(task_gist("no preamble"), "no preamble");
        assert_eq!(task_gist(&"x".repeat(2000)).chars().count(), 1000);
    }

    #[test]
    fn exit_signal_then_synthetic_abort_stays_aborted() {
        let f = Facts {
            last: Last::Aborted,
            exit: Some(Exit::Abnormal),
            last_error: Some("Interrupted".into()),
            ..Facts::default()
        };
        let r = infer(&f, false, true, false, SystemTime::now());
        assert_eq!(r.status, AgentStatus::Aborted);
        assert_eq!(r.error, Some("Interrupted"));
    }

    #[test]
    fn fresh_finish_without_output_is_still_running() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let f = Facts {
            last: Last::Stop,
            last_ts: Some(now - Duration::from_secs(2)),
            ..Facts::default()
        };
        assert_eq!(
            infer(&f, false, true, false, now).status,
            AgentStatus::Running
        );
        assert_eq!(infer(&f, false, true, true, now).status, AgentStatus::Done);
        assert_eq!(
            infer(&f, false, false, false, now).status,
            AgentStatus::Done
        );
        let later = now + Duration::from_secs(10);
        assert_eq!(
            infer(&f, false, true, false, later).status,
            AgentStatus::Done
        );
        let exited = Facts {
            exit: Some(Exit::Normal),
            ..f
        };
        assert_eq!(
            infer(&exited, false, true, false, now).status,
            AgentStatus::Done
        );
    }

    #[test]
    fn iso_timestamps() {
        let t = tail::parse_iso(b"2026-10-21T14:19:13.489Z").unwrap();
        let ms = t
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        assert_eq!(ms, 1_792_592_353_489);
        assert!(tail::parse_iso(b"garbage").is_none());
    }
}
