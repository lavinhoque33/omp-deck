//! Per-agent incremental fold of a subagent transcript, status inference
//! (docs/notes/agent-hub.md §3) and the directory scan that yields [`AgentsSnapshot`]s.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use super::tail::{self, LineTail, cap_bytes, line_timestamp, line_type, one_line, simple_str};
use super::{AgentStatus, AgentSummary, AgentsSnapshot};

/// Full assignment text kept per agent.
const MAX_TASK: usize = 64 * 1024;
/// `AgentSummary::task_gist` cap (chars).
const MAX_GIST_CHARS: usize = 1000;
/// Cap of [`AgentSummary::error`].
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
    assistant_model: Option<String>,
    thinking: Option<String>,
    task: String,
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
}

impl AgentState {
    pub fn new(path: PathBuf) -> Self {
        AgentState {
            tail: LineTail::new(path),
            facts: Facts::default(),
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
                if let Some(m) = parse::<ModelChange>(line, kind).and_then(|m| m.model) {
                    self.model = Some(m);
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
                        self.task = cap_bytes(task, MAX_TASK).0;
                    }
                    self.agent = init.agent.or(self.agent.take());
                    self.resolved_model = init.resolved_model.or(self.resolved_model.take());
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
                    self.task = cap_bytes(content_text(entry.message.content), MAX_TASK).0;
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
pub(crate) fn strip_preamble(task: &str) -> &str {
    let t = task.trim_start();
    let rest = if let Some(r) = t.strip_prefix("Complete the assignment below,") {
        r.trim_start().strip_prefix("thoroughly:")
    } else {
        t.strip_prefix("Complete assignment thoroughly:")
    };
    rest.map_or(t, str::trim_start)
}

/// Preamble stripped; the `# Target` section when one exists; ≤ 1000 chars.
pub(crate) fn task_gist(task: &str) -> String {
    let body = strip_preamble(task);
    let mut section: Option<Vec<&str>> = None;
    for line in body.lines() {
        match &mut section {
            None if line.trim_end() == "# Target" || line.starts_with("# Target ") => {
                section = Some(Vec::new());
            }
            None => {}
            Some(_) if line.starts_with("# ") => break,
            Some(lines) => lines.push(line),
        }
    }
    let text = match section {
        Some(lines) => lines.join("\n"),
        None => body.to_owned(),
    };
    let text = text.trim();
    match text.char_indices().nth(MAX_GIST_CHARS) {
        Some((i, _)) => text[..i].trim_end().to_owned(),
        None => text.to_owned(),
    }
}

// ---------------------------------------------------------------------------------------------
// Status inference

struct Inferred {
    status: AgentStatus,
    error: Option<String>,
    current: Option<String>,
}

/// A finished-looking agent whose last entry is younger than this may still be writing its `.md`.
const OUTPUT_GRACE: Duration = Duration::from_secs(5);

fn infer(
    f: &Facts,
    tombstone: bool,
    owner_alive: bool,
    has_output: bool,
    now: SystemTime,
) -> Inferred {
    let error = || f.last_error.clone();
    let done = |status, error| Inferred {
        status,
        error,
        current: None,
    };
    if tombstone {
        return done(AgentStatus::Aborted, error());
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
        Last::Error => done(AgentStatus::Failed, error()),
        Last::Aborted => done(AgentStatus::Aborted, error()),
        Last::Open | Last::Nothing => {
            if f.exit.is_some() || !owner_alive {
                done(AgentStatus::Interrupted, None)
            } else {
                Inferred {
                    status: AgentStatus::Running,
                    error: None,
                    current: f.pending.last().map(|(_, d)| d.clone()),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Directory scan

struct Found {
    path: PathBuf,
    id: String,
    parent: Option<String>,
    advisor: bool,
    tombstone: bool,
    output: Option<PathBuf>,
}

/// `<stem>/…` transcripts, recursively. Skips `.bak` files and non-transcripts.
fn discover(dir: &Path, parent: Option<&str>, depth: usize, out: &mut Vec<Found>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::debug!("agents: cannot list {}: {e}", dir.display());
            }
            return;
        }
    };
    let mut names: HashSet<OsString> = HashSet::new();
    let mut subdirs: Vec<(PathBuf, String)> = Vec::new();
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let name = entry.file_name();
        let Some(s) = name.to_str() else { continue };
        if ft.is_dir() {
            if depth < MAX_DIR_DEPTH {
                subdirs.push((entry.path(), s.to_owned()));
            }
        } else if ft.is_file()
            && let Some(id) = s.strip_suffix(".jsonl")
            && !s.contains(".bak")
        {
            files.push((entry.path(), id.to_owned()));
        }
        names.insert(name);
    }
    for (path, id) in files {
        let sibling = |suffix: &str| names.contains(&OsString::from(format!("{id}{suffix}")));
        let output = sibling(".md")
            .then(|| dir.join(format!("{id}.md")))
            .filter(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_file() && m.len() > 0));
        out.push(Found {
            advisor: id.starts_with("__advisor"),
            tombstone: sibling(".jsonl.tombstone"),
            parent: parent.map(str::to_owned),
            output,
            path,
            id,
        });
    }
    for (path, name) in subdirs {
        discover(&path, Some(&name), depth + 1, out);
    }
}

/// Folds `<session stem>/` into [`AgentsSnapshot`]s, keeping each transcript's tail position
/// between calls.
pub struct AgentsScanner {
    session: PathBuf,
    states: HashMap<PathBuf, AgentState>,
}

impl AgentsScanner {
    /// `session` is the parent's `.jsonl`; the artifact directory is the same path without
    /// the extension.
    pub fn new(session: PathBuf) -> Self {
        AgentsScanner {
            session,
            states: HashMap::new(),
        }
    }

    pub fn session(&self) -> &Path {
        &self.session
    }

    /// Rescans the directory and tails every transcript. `owner_alive`: the omp that owns the
    /// session still runs (inferred-Running agents of a dead owner become Interrupted).
    pub fn scan(&mut self, owner_alive: bool) -> AgentsSnapshot {
        let stem = self.session.with_extension("");
        let mut found = Vec::new();
        discover(&stem, None, 0, &mut found);
        let live: HashSet<&PathBuf> = found.iter().map(|f| &f.path).collect();
        self.states.retain(|p, _| live.contains(p));

        let mut agents = Vec::with_capacity(found.len());
        for f in found {
            let state = self
                .states
                .entry(f.path.clone())
                .or_insert_with(|| AgentState::new(f.path.clone()));
            if let Err(e) = state.refresh() {
                log::debug!("agents: cannot read {}: {e}", f.path.display());
                continue;
            }
            if state.is_stub() {
                continue;
            }
            agents.push(summarize(&state.facts, f, owner_alive));
        }
        AgentsSnapshot {
            session: self.session.clone(),
            agents: order(agents),
        }
    }
}

fn summarize(facts: &Facts, f: Found, owner_alive: bool) -> AgentSummary {
    let inferred = infer(
        facts,
        f.tombstone,
        owner_alive,
        f.output.is_some(),
        SystemTime::now(),
    );
    let duration = match (facts.created, facts.last_ts) {
        (Some(a), Some(b)) => b.duration_since(a).ok(),
        _ => None,
    };
    AgentSummary {
        id: f.id,
        parent_id: f.parent,
        depth: 0,
        path: f.path,
        advisor: f.advisor,
        agent: facts.agent.clone(),
        model: facts
            .model
            .clone()
            .or_else(|| facts.resolved_model.clone())
            .or_else(|| facts.assistant_model.clone()),
        thinking: facts.thinking.clone(),
        task_gist: task_gist(&facts.task),
        task: facts.task.clone(),
        status: inferred.status,
        error: inferred.error,
        current: inferred.current,
        created: facts.created,
        last_activity: facts.last_ts,
        duration,
        requests: facts.requests,
        tool_calls: facts.tool_calls,
        tokens: facts.tokens,
        cost: facts.cost,
        context_tokens: facts.context_tokens,
        output_path: f.output,
    }
}

/// Parent before child, newest first among siblings; `depth` is the tree depth (orphans whose
/// parent is not a listed agent are roots).
fn order(agents: Vec<AgentSummary>) -> Vec<AgentSummary> {
    let ids: HashSet<&str> = agents.iter().map(|a| a.id.as_str()).collect();
    let mut children: HashMap<Option<&str>, Vec<usize>> = HashMap::new();
    for (i, a) in agents.iter().enumerate() {
        let key = a.parent_id.as_deref().filter(|p| ids.contains(p));
        children.entry(key).or_default().push(i);
    }
    for list in children.values_mut() {
        list.sort_by(|&x, &y| {
            let (a, b) = (&agents[x], &agents[y]);
            b.created
                .cmp(&a.created)
                .then_with(|| a.id.cmp(&b.id))
                .then_with(|| a.path.cmp(&b.path))
        });
    }
    let mut order: Vec<(usize, usize)> = Vec::with_capacity(agents.len());
    let mut stack: Vec<(usize, usize)> = children
        .get(&None)
        .map(|r| r.iter().rev().map(|&i| (i, 0)).collect())
        .unwrap_or_default();
    while let Some((i, depth)) = stack.pop() {
        order.push((i, depth));
        if let Some(kids) = children.get(&Some(agents[i].id.as_str())) {
            stack.extend(kids.iter().rev().map(|&k| (k, depth + 1)));
        }
    }
    // Defensive: duplicate ids across directories or parent cycles never lose a row.
    if order.len() < agents.len() {
        let seen: HashSet<usize> = order.iter().map(|&(i, _)| i).collect();
        order.extend(
            (0..agents.len())
                .filter(|i| !seen.contains(i))
                .map(|i| (i, 0)),
        );
    }
    let mut slots: Vec<Option<AgentSummary>> = agents.into_iter().map(Some).collect();
    order
        .into_iter()
        .filter_map(|(i, depth)| {
            slots[i].take().map(|mut a| {
                a.depth = depth;
                a
            })
        })
        .collect()
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
        assert_eq!(r.error.as_deref(), Some("Interrupted"));
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
