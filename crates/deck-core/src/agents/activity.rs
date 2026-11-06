//! Per-session subagent activity feed (the hub's Activity tab), read-only from omp's
//! files. Each watched transcript is tailed incrementally through [`LineTail`]; rows are
//! bounded (≤ 256 per agent, ≤ 2000 merged, newest first) and polled every 500 ms.
//!
//! Row sources, mirroring omp's `agent-activity.ts` transcript rows plus what OmpDeck adds:
//! * assistant text parts → [`ActivityKind::Response`];
//! * each `toolCall` block → [`ActivityKind::Tool`] ([`ActivityStatus::Pending`] until its
//!   `toolResult` arrives, `isError` → Error else Success, `at` = max(call, result));
//! * `custom_message` `irc:incoming` → [`ActivityKind::Irc`] (`from` = `details.from`);
//! * lifecycle → [`ActivityKind::Lifecycle`]: spawn (`session_init`/header time, summary = the
//!   task gist), assistant `stopReason` error/aborted (summary = `errorMessage`),
//!   `session_exit` (summary = reason), `<id>.jsonl.tombstone` mtime (Killed).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use super::scan::{strip_preamble, task_gist};
use super::tail::{self, LineTail, from_epoch_ms, line_timestamp, line_type, simple_str};
use crate::status::lock;

/// Tail starts at most this far back; the first (partial) line is skipped.
const START_WINDOW: u64 = 256 * 1024;
/// Rows kept per agent.
const MAX_PER_AGENT: usize = 256;
/// Rows kept in the merged view.
const MAX_MERGED: usize = 2000;
/// Watcher poll cadence.
const POLL: Duration = Duration::from_millis(500);
/// `ActivityRow::summary` cap (chars).
const MAX_SUMMARY_CHARS: usize = 240;
/// A `message` line states its role / ids within this many leading bytes.
const ROLE_PEEK: usize = 600;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ActivityKind {
    Response,
    Tool,
    Irc,
    Lifecycle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ActivityStatus {
    Success,
    Error,
    Aborted,
    Pending,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ActivityRow {
    /// `"<agent>:response:<entryId>"` | `"<agent>:tool:<callId>"` | `"<agent>:irc:<n>"` |
    /// `"<agent>:life:<n>"`.
    pub id: String,
    pub agent_id: String,
    pub at: SystemTime,
    pub kind: ActivityKind,
    /// `"Response"` | tool name | `"IRC from <x>"` | `"Spawned"`/`"Error"`/`"Aborted"`/
    /// `"Exited"`/`"Killed"`.
    pub title: String,
    /// One line, whitespace collapsed, ≤ 240 chars.
    pub summary: String,
    pub status: ActivityStatus,
    /// Tool rows (anchor in the transcript).
    #[serde(default)]
    pub call_id: Option<String>,
}

/// Collapse whitespace runs to one space and cut to [`MAX_SUMMARY_CHARS`] chars.
fn summarize(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(256));
    let mut gap = true;
    for c in text.chars() {
        if c.is_whitespace() {
            if !gap {
                out.push(' ');
                gap = true;
            }
        } else {
            out.push(c);
            gap = false;
        }
    }
    let t = out.trim();
    match t.char_indices().nth(MAX_SUMMARY_CHARS) {
        Some((i, _)) => t[..i].trim_end().to_owned(),
        None => t.to_owned(),
    }
}

#[derive(Deserialize)]
struct SessionInit {
    task: Option<String>,
}

#[derive(Deserialize)]
struct MessageEntry<'a> {
    id: Option<String>,
    #[serde(borrow)]
    message: Message<'a>,
}

#[derive(Deserialize)]
struct Message<'a> {
    #[serde(borrow, default)]
    content: Option<Content<'a>>,
    #[serde(default)]
    timestamp: Option<f64>,
    #[serde(rename = "stopReason", default)]
    stop_reason: Option<String>,
    #[serde(rename = "errorMessage", default)]
    error_message: Option<String>,
    #[serde(rename = "isError", default)]
    is_error: Option<bool>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Content<'a> {
    Text(String),
    Parts(#[serde(borrow)] Vec<Part<'a>>),
}

#[derive(Deserialize)]
struct Part<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<std::borrow::Cow<'a, str>>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<serde_json::Value>,
    #[serde(default)]
    intent: Option<String>,
}
#[derive(Deserialize)]
struct CustomMessage {
    #[serde(default)]
    details: Option<IrcDetails>,
}

#[derive(Deserialize)]
struct IrcDetails {
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct CustomLine {
    data: Option<ExitData>,
}

#[derive(Deserialize)]
struct ExitData {
    #[serde(default)]
    reason: Option<String>,
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

/// omp's tool summary rule (`agent-activity.ts` `F(name, args)`): `args.i` / `args.intent`
/// first, then per-tool fields (read→path, grep→`pattern · path`, glob→path, bash→command,
/// write/edit→path, `wait` gets its fixed text), else path/query/name/task/title, else compact
/// JSON.
fn tool_summary(name: &str, args: &serde_json::Value, intent: Option<&str>) -> String {
    if let Some(i) = intent.filter(|i| !i.trim().is_empty()) {
        return summarize(i);
    }
    let field = |k: &str| {
        args.get(k)
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.trim().is_empty())
    };
    if let Some(i) = field("i") {
        return summarize(i);
    }
    if let Some(i) = field("intent") {
        return summarize(i);
    }
    let summary = match name {
        "read" | "glob" | "write" | "edit" => field("path").map(str::to_owned),
        "grep" => match (field("pattern"), field("path")) {
            (Some(p), Some(path)) => Some(format!("{p} · {path}")),
            (Some(p), None) => Some(p.to_owned()),
            (None, Some(path)) => Some(path.to_owned()),
            (None, None) => None,
        },
        "bash" => field("command").map(str::to_owned),
        "wait" => Some("Waiting for background work or a peer".to_owned()),
        _ => None,
    };
    if let Some(s) = summary {
        return summarize(&s);
    }
    for k in ["path", "query", "name", "task", "title"] {
        if let Some(s) = field(k) {
            return summarize(s);
        }
    }
    match serde_json::to_string(args) {
        Ok(s) => summarize(&s),
        Err(_) => String::new(),
    }
}

/// Response summary = omp's: the text parts joined.
fn response_summary(content: Option<Content>) -> String {
    match content {
        Some(Content::Text(t)) => summarize(&t),
        Some(Content::Parts(parts)) => {
            let text: Vec<&str> = parts
                .iter()
                .filter(|p| p.kind.as_deref() == Some("text"))
                .filter_map(|p| p.text.as_deref())
                .collect();
            summarize(&text.join("\n"))
        }
        None => String::new(),
    }
}

/// One agent's folded rows: ≤ [`MAX_PER_AGENT`], oldest first internally (the merged view
/// sorts newest first).
struct AgentFeed {
    id: String,
    rows: VecDeque<ActivityRow>,
    /// Pending tool row index by `call_id`.
    pending: HashMap<String, usize>,
    /// Lifecycle sequence number (for `<agent>:life:<n>` ids).
    life: u32,
    /// IRC sequence number (for `<agent>:irc:<n>` ids).
    irc: u32,
    /// First `session_init` produced the Spawned row already.
    spawned: bool,
    /// Tombstone row emitted already.
    killed: bool,
    /// `session_exit` row emitted already.
    exited: bool,
    header_at: Option<SystemTime>,
    tombstone: Option<PathBuf>,
}

impl AgentFeed {
    fn new(id: String, tombstone: Option<PathBuf>) -> Self {
        AgentFeed {
            id,
            rows: VecDeque::new(),
            pending: HashMap::new(),
            life: 0,
            irc: 0,
            spawned: false,
            killed: false,
            exited: false,
            header_at: None,
            tombstone,
        }
    }

    fn push(&mut self, row: ActivityRow) {
        if self.rows.len() >= MAX_PER_AGENT {
            let dropped = self.rows.pop_front();
            // A dropped pending tool row must not leave a stale index behind.
            if let Some(front) = dropped
                && front.kind == ActivityKind::Tool
                && front.status == ActivityStatus::Pending
                && let Some(call) = front.call_id
                && self.pending.get(call.as_str()) == Some(&0)
            {
                self.pending.remove(call.as_str());
                for v in self.pending.values_mut() {
                    *v -= 1;
                }
            }
        }
        if row.kind == ActivityKind::Tool
            && row.status == ActivityStatus::Pending
            && let Some(call) = row.call_id.clone()
        {
            self.pending.insert(call, self.rows.len());
        }
        self.rows.push_back(row);
    }

    fn life_id(&mut self) -> String {
        let n = self.life;
        self.life += 1;
        format!("{}:life:{n}", self.id)
    }

    fn irc_id(&mut self) -> String {
        let n = self.irc;
        self.irc += 1;
        format!("{}:irc:{n}", self.id)
    }

    fn apply_line(&mut self, line: &[u8]) {
        let Some(kind) = line_type(line) else {
            return;
        };
        match kind {
            "session" => {
                if self.header_at.is_none() {
                    self.header_at = line_timestamp(line);
                }
            }
            "session_init" => {
                if self.spawned {
                    return;
                }
                self.spawned = true;
                let at = line_timestamp(line)
                    .or(self.header_at)
                    .unwrap_or_else(SystemTime::now);
                let task = parse::<SessionInit>(line, "session_init").and_then(|s| s.task);
                let summary = task
                    .map(|t| summarize(&task_gist(strip_preamble(&t))))
                    .unwrap_or_default();
                let id = self.life_id();
                let agent_id = self.id.clone();
                self.push(ActivityRow {
                    id,
                    agent_id,
                    at,
                    kind: ActivityKind::Lifecycle,
                    title: "Spawned".to_owned(),
                    summary,
                    status: ActivityStatus::Success,
                    call_id: None,
                });
            }
            "message" => self.apply_message(line),
            "custom_message" => self.apply_custom_message(line),
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
        } else {
            return;
        };
        if role == "assistant" {
            self.apply_assistant(line);
            return;
        }
        // toolResult: resolve the pending tool row.
        let Some(call) = simple_str(line, "toolCallId", ROLE_PEEK + 200) else {
            return;
        };
        let error = memchr::memmem::find(
            &line[..line.len().min(ROLE_PEEK + 800)],
            b"\"isError\":true",
        )
        .is_some();
        let at = line_timestamp(line);
        let idx = self.pending.remove(call).or_else(|| {
            // Fall back to a linear scan: the row may predate the watcher's start window.
            self.rows.iter().position(|r| {
                r.kind == ActivityKind::Tool
                    && r.status == ActivityStatus::Pending
                    && r.call_id.as_deref() == Some(call)
            })
        });
        if let Some(idx) = idx
            && let Some(row) = self.rows.get_mut(idx)
        {
            row.status = if error {
                ActivityStatus::Error
            } else {
                ActivityStatus::Success
            };
            if let Some(ts) = at
                && ts > row.at
            {
                row.at = ts;
            }
        }
    }

    fn apply_assistant(&mut self, line: &[u8]) {
        let Some(entry) = parse::<MessageEntry>(line, "assistant") else {
            return;
        };
        let at = entry
            .message
            .timestamp
            .and_then(from_epoch_ms)
            .or_else(|| line_timestamp(line))
            .unwrap_or_else(SystemTime::now);
        // Lifecycle for error/aborted assistant turns (OmpDeck addition: omp's parser ignores
        // these, but the hub shows errors).
        match entry.message.stop_reason.as_deref() {
            Some("aborted") | Some("error") => {
                let aborted = entry.message.stop_reason.as_deref() == Some("aborted");
                let summary = entry
                    .message
                    .error_message
                    .as_deref()
                    .map(summarize)
                    .unwrap_or_default();
                let id = self.life_id();
                let agent_id = self.id.clone();
                self.push(ActivityRow {
                    id,
                    agent_id,
                    at,
                    kind: ActivityKind::Lifecycle,
                    title: if aborted { "Aborted" } else { "Error" }.to_owned(),
                    summary,
                    status: if aborted {
                        ActivityStatus::Aborted
                    } else {
                        ActivityStatus::Error
                    },
                    call_id: None,
                });
            }
            _ => {}
        }
        match entry.message.content {
            Some(Content::Parts(parts)) => {
                let has_text = parts.iter().any(|p| {
                    p.kind.as_deref() == Some("text")
                        && p.text.as_deref().is_some_and(|t| !t.trim().is_empty())
                });
                for p in &parts {
                    if p.kind.as_deref() != Some("toolCall") {
                        continue;
                    }
                    let (Some(call), Some(name)) = (p.id.clone(), p.name.clone()) else {
                        continue;
                    };
                    if call.is_empty() || name.is_empty() {
                        continue;
                    }
                    let args = p.arguments.clone().unwrap_or(serde_json::Value::Null);
                    let summary = tool_summary(&name, &args, p.intent.as_deref());
                    // A repeated toolCall id (replayed transcript) refreshes the row.
                    if let Some(&idx) = self.pending.get(call.as_str()) {
                        if let Some(row) = self.rows.get_mut(idx) {
                            row.title = name.clone();
                            row.summary = summary;
                            row.at = at;
                        }
                        continue;
                    }
                    let agent_id = self.id.clone();
                    self.push(ActivityRow {
                        id: format!("{}:tool:{call}", self.id),
                        agent_id,
                        at,
                        kind: ActivityKind::Tool,
                        title: name,
                        summary,
                        status: ActivityStatus::Pending,
                        call_id: Some(call),
                    });
                }
                if has_text {
                    let texts: Vec<&str> = parts
                        .iter()
                        .filter(|p| p.kind.as_deref() == Some("text"))
                        .filter_map(|p| p.text.as_deref())
                        .collect();
                    let entry_id = entry
                        .id
                        .clone()
                        .unwrap_or_else(|| format!("a{}", self.life));
                    let agent_id = self.id.clone();
                    self.push(ActivityRow {
                        id: format!("{}:response:{entry_id}", self.id),
                        agent_id,
                        at,
                        kind: ActivityKind::Response,
                        title: "Response".to_owned(),
                        summary: summarize(&texts.join("\n")),
                        status: if entry.message.is_error == Some(true) {
                            ActivityStatus::Error
                        } else {
                            ActivityStatus::Success
                        },
                        call_id: None,
                    });
                }
            }
            content @ (Some(Content::Text(_)) | None) => {
                let summary = response_summary(content);
                if summary.is_empty() {
                    return;
                }
                let entry_id = entry
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("a{}", self.life));
                let agent_id = self.id.clone();
                self.push(ActivityRow {
                    id: format!("{}:response:{entry_id}", self.id),
                    agent_id,
                    at,
                    kind: ActivityKind::Response,
                    title: "Response".to_owned(),
                    summary,
                    status: if entry.message.is_error == Some(true) {
                        ActivityStatus::Error
                    } else {
                        ActivityStatus::Success
                    },
                    call_id: None,
                });
            }
        }
    }

    fn apply_custom_message(&mut self, line: &[u8]) {
        if simple_str(line, "customType", 120) != Some("irc:incoming") {
            return;
        }
        let Some(msg) = parse::<CustomMessage>(line, "irc:incoming") else {
            return;
        };
        let at = line_timestamp(line).unwrap_or_else(SystemTime::now);
        let from = msg.details.as_ref().and_then(|d| d.from.clone());
        let body = msg
            .details
            .as_ref()
            .and_then(|d| d.message.clone())
            .unwrap_or_default();
        let title = match from {
            Some(f) => format!("IRC from {f}"),
            None => "IRC".to_owned(),
        };
        let id = self.irc_id();
        let agent_id = self.id.clone();
        self.push(ActivityRow {
            id,
            agent_id,
            at,
            kind: ActivityKind::Irc,
            title,
            summary: summarize(&body),
            status: ActivityStatus::Success,
            call_id: None,
        });
    }

    fn apply_custom(&mut self, line: &[u8]) {
        if simple_str(line, "customType", 120) != Some("session_exit") {
            return;
        }
        if self.exited {
            return;
        }
        self.exited = true;
        let at = line_timestamp(line).unwrap_or_else(SystemTime::now);
        let summary = parse::<CustomLine>(line, "session_exit")
            .and_then(|c| c.data)
            .and_then(|d| d.reason)
            .map(|r| summarize(&r))
            .unwrap_or_default();
        let id = self.life_id();
        let agent_id = self.id.clone();
        self.push(ActivityRow {
            id,
            agent_id,
            at,
            kind: ActivityKind::Lifecycle,
            title: "Exited".to_owned(),
            summary,
            status: ActivityStatus::Success,
            call_id: None,
        });
    }

    /// The tombstone row (mtime = kill time). Called after each refresh; idempotent.
    fn apply_sidecars(&mut self, now: SystemTime) {
        if !self.killed
            && let Some(path) = self.tombstone.clone()
            && let Ok(md) = fs::symlink_metadata(&path)
            && md.is_file()
        {
            self.killed = true;
            let at = md.modified().unwrap_or(now);
            let id = self.life_id();
            let agent_id = self.id.clone();
            self.push(ActivityRow {
                id,
                agent_id,
                at,
                kind: ActivityKind::Lifecycle,
                title: "Killed".to_owned(),
                summary: String::new(),
                status: ActivityStatus::Aborted,
                call_id: None,
            });
        }
    }
}

/// One watched transcript: its feed plus the windowed tail.
struct Watched {
    feed: AgentFeed,
    tail: LineTail,
}

impl Watched {
    fn new(id: String, path: &Path, tombstone: Option<PathBuf>) -> io::Result<Self> {
        let mut tail = LineTail::new(path.to_path_buf());
        // A fresh tail starts at `max(0, size − 256 KiB)` on a line boundary; a missing file
        // reports here and leaves the feed empty until it appears.
        tail.seek_start_window(START_WINDOW)?;
        Ok(Watched {
            feed: AgentFeed::new(id, tombstone),
            tail,
        })
    }

    /// Reads whatever was appended; `Ok(true)` when the feed changed.
    fn refresh(&mut self) -> io::Result<bool> {
        let mut changed = false;
        loop {
            let chunk = self.tail.poll()?;
            if chunk.reset {
                let tombstone = self.feed.tombstone.clone();
                let id = self.feed.id.clone();
                self.feed = AgentFeed::new(id, tombstone);
                changed = true;
            }
            for line in tail::lines(&chunk.data) {
                self.feed.apply_line(line);
                changed = true;
            }
            if !chunk.more {
                break;
            }
        }
        let before = self.feed.rows.len();
        self.feed.apply_sidecars(SystemTime::now());
        changed |= self.feed.rows.len() != before;
        Ok(changed)
    }
}

/// Synchronous incremental core behind [`ActivityWatcher`]: one bounded [`LineTail`] per agent.
pub struct AgentActivity {
    watched: HashMap<PathBuf, Watched>,
    order: Vec<PathBuf>,
}

impl AgentActivity {
    pub fn new() -> Self {
        AgentActivity {
            watched: HashMap::new(),
            order: Vec::new(),
        }
    }

    /// `agents`: `(id, transcript path)`; a transcript that vanished leaves the feed.
    /// `tombstones`: transcript path → `<id>.jsonl.tombstone` path (present even when the
    /// tombstone file itself does not exist yet).
    pub fn set_agents(
        &mut self,
        agents: Vec<(String, PathBuf)>,
        tombstones: &HashMap<PathBuf, PathBuf>,
    ) {
        let live: HashSet<&PathBuf> = agents.iter().map(|(_, p)| p).collect();
        self.watched.retain(|p, _| live.contains(p));
        self.order.retain(|p| live.contains(p));
        for (id, path) in agents {
            if !self.watched.contains_key(&path) {
                let tombstone = tombstones.get(&path).cloned();
                match Watched::new(id, &path, tombstone) {
                    Ok(w) => {
                        self.watched.insert(path.clone(), w);
                        self.order.push(path);
                    }
                    Err(e) => {
                        log::debug!("agents: cannot tail {}: {e}", path.display());
                    }
                }
            }
        }
    }

    /// Reads whatever was appended; `Ok(true)` when any feed changed.
    pub fn refresh(&mut self) -> io::Result<bool> {
        let mut changed = false;
        let mut missing = Vec::new();
        for path in self.order.clone() {
            let Some(w) = self.watched.get_mut(&path) else {
                continue;
            };
            match w.refresh() {
                Ok(c) => changed |= c,
                Err(e)
                    if e.kind() == io::ErrorKind::NotFound
                        || e.kind() == io::ErrorKind::InvalidInput =>
                {
                    // A transcript that vanished (or is a link/FIFO) leaves the feed this
                    // refresh; it returns on the next `set_agents` when it reappears.
                    missing.push(path.clone());
                }
                Err(e) => return Err(e),
            }
        }
        for path in missing {
            self.watched.remove(&path);
            self.order.retain(|p| p != &path);
        }
        Ok(changed)
    }

    /// Newest first, ≤ [`MAX_MERGED`].
    pub fn rows(&self) -> Vec<ActivityRow> {
        let mut all: Vec<&ActivityRow> = self
            .watched
            .values()
            .flat_map(|w| w.feed.rows.iter())
            .collect();
        all.sort_by(|a, b| {
            b.at.cmp(&a.at)
                .then_with(|| a.agent_id.cmp(&b.agent_id))
                .then_with(|| a.id.cmp(&b.id))
        });
        all.into_iter().take(MAX_MERGED).cloned().collect()
    }
}

impl Default for AgentActivity {
    fn default() -> Self {
        Self::new()
    }
}

struct Shared {
    latest: Mutex<Arc<Vec<ActivityRow>>>,
    revision: AtomicU64,
    agents: Mutex<Vec<(String, PathBuf)>>,
    tombstones: Mutex<HashMap<PathBuf, PathBuf>>,
    stop: Mutex<bool>,
    wake: Condvar,
}

/// Follows a set of transcripts every 500 ms; `latest()` is newest first (≤ 2000 rows).
/// `set_agents` replaces the watched set; drop stops the thread. Does no work when no agents
/// are set.
pub struct ActivityWatcher {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

impl ActivityWatcher {
    pub fn spawn(agents: Vec<(String, PathBuf)>, on_update: impl Fn() + Send + 'static) -> Self {
        let shared = Arc::new(Shared {
            latest: Mutex::new(Arc::new(Vec::new())),
            revision: AtomicU64::new(0),
            agents: Mutex::new(agents),
            tombstones: Mutex::new(HashMap::new()),
            stop: Mutex::new(false),
            wake: Condvar::new(),
        });
        let worker = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("deck-activity".into())
            .spawn(move || run(&worker, &on_update));
        let handle = match handle {
            Ok(h) => Some(h),
            Err(e) => {
                log::error!("cannot spawn activity watcher thread: {e}");
                None
            }
        };
        ActivityWatcher { shared, handle }
    }

    /// Replaces the watched `(id, transcript)` set (and their tombstone sidecars, derived as
    /// `<transcript>.tombstone`).
    pub fn set_agents(&self, agents: Vec<(String, PathBuf)>) {
        *lock(&self.shared.agents) = agents;
        self.shared.wake.notify_all();
    }

    /// Merged rows, newest first (≤ 2000).
    pub fn latest(&self) -> Arc<Vec<ActivityRow>> {
        lock(&self.shared.latest).clone()
    }

    /// Bumps on every change.
    pub fn revision(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire)
    }
}

impl Drop for ActivityWatcher {
    fn drop(&mut self) {
        *lock(&self.shared.stop) = true;
        self.shared.wake.notify_all();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run(shared: &Shared, on_update: &dyn Fn()) {
    let mut core = AgentActivity::new();
    let mut warned = false;
    loop {
        let agents = lock(&shared.agents).clone();
        // Tombstones ride next to their transcripts (`<id>.jsonl.tombstone`).
        let tombstones: HashMap<PathBuf, PathBuf> = agents
            .iter()
            .map(|(_, p)| {
                let mut t = p.as_os_str().to_owned();
                t.push(".tombstone");
                (p.clone(), PathBuf::from(t))
            })
            .collect();
        *lock(&shared.tombstones) = tombstones.clone();
        core.set_agents(agents, &tombstones);
        if !core.watched.is_empty() {
            match core.refresh() {
                Ok(changed) => {
                    warned = false;
                    if changed {
                        let rows = core.rows();
                        let mut latest = lock(&shared.latest);
                        if latest.as_slice() != rows.as_slice() {
                            *latest = Arc::new(rows);
                            shared.revision.fetch_add(1, Ordering::Release);
                            drop(latest);
                            on_update();
                        }
                    }
                }
                Err(e) => {
                    if !warned {
                        log::debug!("agents: cannot read activity transcripts: {e}");
                        warned = true;
                    }
                }
            }
        }
        let mut stop = lock(&shared.stop);
        let deadline = std::time::Instant::now() + POLL;
        while !*stop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            stop = shared
                .wake
                .wait_timeout(stop, left)
                .map_or_else(|e| e.into_inner().0, |(g, _)| g);
        }
        if *stop {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn agent_lines(id: &str, task: &str) -> Vec<String> {
        vec![
            r#"{"type":"title","v":1,"title":""}"#.to_owned(),
            r#"{"type":"session","version":3,"id":"01a1","timestamp":"2026-10-21T10:00:00.000Z","cwd":"/x","parentSession":"/x/sess.jsonl"}"#.to_owned(),
            format!(
                r#"{{"type":"session_init","id":"si","parentId":null,"timestamp":"2026-10-21T10:00:00.020Z","task":{task},"tools":[],"agent":"task","resolvedModel":"a/b"}}"#
            ),
            r#"{"type":"message","id":"u","parentId":null,"timestamp":"2026-10-21T10:00:00.100Z","message":{"role":"user","content":[{"type":"text","text":"hi"}],"timestamp":1792576800100}}"#.to_owned(),
            format!(
                r#"{{"type":"message","id":"a1","parentId":null,"timestamp":"2026-10-21T10:00:05.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"First line.\nSecond  line."}},{{"type":"toolCall","id":"tu{idd}","name":"read","arguments":{{"path":"src/foo.rs"}},"intent":"Reading foo"}}],"provider":"a","model":"b","usage":{{"totalTokens":3}},"stopReason":"toolUse","timestamp":1792576805000}}}}"#,
                idd = id
            ),
        ]
    }

    fn write_lines(path: &Path, lines: &[String]) {
        let mut f = fs::File::create(path).unwrap();
        for l in lines {
            f.write_all(l.as_bytes()).unwrap();
            f.write_all(b"\n").unwrap();
        }
    }

    #[test]
    fn response_and_pending_tool_rows() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("A.jsonl");
        write_lines(&p, &agent_lines("1", r#""Do the thing.""#));
        let mut feed = AgentFeed::new("A".to_owned(), None);
        for line in fs::read(&p).unwrap().split(|b| *b == b'\n') {
            if !line.is_empty() {
                feed.apply_line(line);
            }
        }
        assert!(feed.rows.iter().any(|r| r.kind == ActivityKind::Lifecycle
            && r.title == "Spawned"
            && r.summary == "Do the thing."));
        let resp = feed
            .rows
            .iter()
            .find(|r| r.kind == ActivityKind::Response)
            .unwrap();
        assert_eq!(resp.title, "Response");
        assert_eq!(resp.summary, "First line. Second line.");
        assert_eq!(resp.status, ActivityStatus::Success);
        assert_eq!(resp.id, "A:response:a1");
        let tool = feed
            .rows
            .iter()
            .find(|r| r.kind == ActivityKind::Tool)
            .unwrap();
        assert_eq!(tool.title, "read");
        assert_eq!(tool.summary, "Reading foo");
        assert_eq!(tool.status, ActivityStatus::Pending);
        assert_eq!(tool.call_id.as_deref(), Some("tu1"));
        assert_eq!(tool.id, "A:tool:tu1");
    }

    #[test]
    fn tool_result_resolves_pending_with_error_and_time() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("A.jsonl");
        write_lines(&p, &agent_lines("9", r#""t""#));
        let mut feed = AgentFeed::new("A".to_owned(), None);
        for line in fs::read(&p).unwrap().split(|b| *b == b'\n') {
            if !line.is_empty() {
                feed.apply_line(line);
            }
        }
        let call_at = feed
            .rows
            .iter()
            .find(|r| r.kind == ActivityKind::Tool)
            .unwrap()
            .at;
        let result = br#"{"type":"message","id":"t","parentId":null,"timestamp":"2026-10-21T10:00:06.000Z","message":{"role":"toolResult","toolCallId":"tu9","toolName":"read","content":[{"type":"text","text":"boom"}],"isError":true,"timestamp":1792576806000}}"#;
        feed.apply_line(result);
        let tool = feed
            .rows
            .iter()
            .find(|r| r.kind == ActivityKind::Tool)
            .unwrap();
        assert_eq!(tool.status, ActivityStatus::Error);
        assert!(tool.at >= call_at);
    }

    #[test]
    fn lifecycle_error_and_exit_rows() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("A.jsonl");
        let mut lines = agent_lines("1", r#""t""#);
        lines.push(
            r#"{"type":"message","id":"e","parentId":null,"timestamp":"2026-10-21T10:01:00.000Z","message":{"role":"assistant","content":[],"provider":"a","model":"b","stopReason":"error","errorMessage":"bad","timestamp":1792576860000}}"#.to_owned(),
        );
        lines.push(
            r#"{"type":"custom","customType":"session_exit","data":{"reason":"dispose","kind":"normal"},"id":"x","parentId":null,"timestamp":"2026-10-21T10:01:01.000Z"}"#.to_owned(),
        );
        write_lines(&p, &lines);
        let mut feed = AgentFeed::new("A".to_owned(), None);
        for line in fs::read(&p).unwrap().split(|b| *b == b'\n') {
            if !line.is_empty() {
                feed.apply_line(line);
            }
        }
        let err = feed.rows.iter().find(|r| r.title == "Error").unwrap();
        assert_eq!(err.kind, ActivityKind::Lifecycle);
        assert_eq!(err.status, ActivityStatus::Error);
        assert_eq!(err.summary, "bad");
        let exit = feed.rows.iter().find(|r| r.title == "Exited").unwrap();
        assert_eq!(exit.summary, "dispose");
    }

    #[test]
    fn irc_row_uses_details_from() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("A.jsonl");
        let mut lines = agent_lines("1", r#""t""#);
        lines.push(
            r#"{"type":"custom_message","customType":"irc:incoming","content":"<irc>hi</irc>","display":true,"details":{"id":"1","from":"Main","message":"  Please\n review   this"},"timestamp":"2026-10-21T10:02:00.000Z"}"#.to_owned(),
        );
        write_lines(&p, &lines);
        let mut feed = AgentFeed::new("A".to_owned(), None);
        for line in fs::read(&p).unwrap().split(|b| *b == b'\n') {
            if !line.is_empty() {
                feed.apply_line(line);
            }
        }
        let irc = feed
            .rows
            .iter()
            .find(|r| r.kind == ActivityKind::Irc)
            .unwrap();
        assert_eq!(irc.title, "IRC from Main");
        assert_eq!(irc.summary, "Please review this");
        assert!(irc.id.starts_with("A:irc:"));
    }

    #[test]
    fn tool_summary_rule_matches_omp() {
        let args = serde_json::json!({"pattern": "foo", "path": "src"});
        assert_eq!(tool_summary("grep", &args, None), "foo · src");
        let args = serde_json::json!({"command": "cargo test"});
        assert_eq!(tool_summary("bash", &args, None), "cargo test");
        let args = serde_json::json!({"path": "a.rs"});
        assert_eq!(tool_summary("read", &args, Some("My intent")), "My intent");
        let args = serde_json::json!({"path": "a.rs"});
        assert_eq!(tool_summary("read", &args, None), "a.rs");
        let args = serde_json::json!({"x": 1});
        assert_eq!(tool_summary("frobnicate", &args, None), r#"{"x":1}"#);
    }
}
