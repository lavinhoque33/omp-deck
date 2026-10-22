//! Subagent transcript → [`TranscriptItem`]s (docs/notes/agent-hub.md §6), with a live tail.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::TranscriptItem;
use super::tail::{self, LineTail, cap_bytes, from_epoch_ms, line_type, one_line};
use crate::status::lock;

const MAX_TOOL_RESULT: usize = 16 * 1024;
const MAX_THINKING: usize = 64 * 1024;
const MAX_ARGS: usize = 4 * 1024;
const MAX_TEXT: usize = 256 * 1024;
const MAX_NOTICE: usize = 16 * 1024;
const MAX_SUMMARY: usize = 64 * 1024;
/// A `message` line states its role within this many leading bytes.
const ROLE_PEEK: usize = 600;
const POLL: Duration = Duration::from_millis(500);

#[derive(Deserialize)]
struct MessageEntry {
    message: Message,
}

#[derive(Deserialize)]
struct Message {
    content: Option<Content>,
    attribution: Option<String>,
    timestamp: Option<f64>,
    provider: Option<String>,
    model: Option<String>,
    #[serde(rename = "stopReason")]
    stop_reason: Option<String>,
    #[serde(rename = "errorMessage")]
    error_message: Option<String>,
    #[serde(rename = "toolCallId")]
    tool_call_id: Option<String>,
    #[serde(rename = "toolName")]
    tool_name: Option<String>,
    #[serde(rename = "isError")]
    is_error: Option<bool>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Parts(Vec<Part>),
}

#[derive(Deserialize)]
struct Part {
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
    thinking: Option<String>,
    id: Option<String>,
    name: Option<String>,
    arguments: Option<Value>,
    intent: Option<String>,
}

#[derive(Deserialize)]
struct CustomMessage {
    #[serde(rename = "customType")]
    custom_type: Option<String>,
    content: Option<Content>,
    display: Option<bool>,
}

#[derive(Deserialize)]
struct Compaction {
    summary: Option<String>,
    #[serde(rename = "tokensBefore")]
    tokens_before: Option<f64>,
    #[serde(rename = "tokensAfter")]
    tokens_after: Option<f64>,
}

#[derive(Deserialize)]
struct ModelChange {
    model: Option<String>,
}

#[derive(Deserialize)]
struct CustomLine {
    data: Option<ExitData>,
}

#[derive(Deserialize)]
struct ExitData {
    reason: Option<String>,
    kind: Option<String>,
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

fn text_of(content: Option<Content>, sep: &str) -> String {
    match content {
        Some(Content::Text(t)) => t,
        Some(Content::Parts(parts)) => parts
            .into_iter()
            .filter_map(|p| match p.kind.as_deref() {
                Some("text") => p.text,
                Some("image") => Some("[image]".to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(sep),
        None => String::new(),
    }
}

fn count(v: Option<f64>) -> Option<u64> {
    v.filter(|v| v.is_finite() && *v >= 0.0).map(|v| v as u64)
}

/// First string among the usual argument keys, one line.
fn call_summary(args: &Value) -> String {
    ["command", "path", "pattern", "query", "url", "file", "i"]
        .iter()
        .filter_map(|k| args.get(k).and_then(Value::as_str))
        .find(|s| !s.trim().is_empty())
        .map(|s| one_line(s, 200))
        .unwrap_or_default()
}

/// Maps one transcript line to zero or more items; unknown/hidden/malformed lines add nothing.
fn map_line(line: &[u8], out: &mut Vec<TranscriptItem>) {
    match line_type(line) {
        Some("message") => map_message(line, out),
        Some("custom_message") => {
            let Some(m) = parse::<CustomMessage>(line, "custom_message") else {
                return;
            };
            if m.display != Some(true) {
                return;
            }
            let text = cap_bytes(text_of(m.content, "\n"), MAX_NOTICE).0;
            out.push(TranscriptItem::Notice {
                kind: m.custom_type.unwrap_or_else(|| "message".into()),
                text,
            });
        }
        Some("compaction") => {
            if let Some(c) = parse::<Compaction>(line, "compaction") {
                out.push(TranscriptItem::Compaction {
                    tokens_before: count(c.tokens_before),
                    tokens_after: count(c.tokens_after),
                    summary: cap_bytes(c.summary.unwrap_or_default(), MAX_SUMMARY).0,
                });
            }
        }
        Some("model_change") => {
            if let Some(model) = parse::<ModelChange>(line, "model_change").and_then(|m| m.model) {
                out.push(TranscriptItem::ModelChange { model });
            }
        }
        Some("custom") => {
            if tail::simple_str(line, "customType", 120) != Some("session_exit") {
                return;
            }
            let data = parse::<CustomLine>(line, "session_exit").and_then(|c| c.data);
            let reason = match data {
                Some(ExitData {
                    reason: Some(r),
                    kind: Some(k),
                }) => format!("{r} ({k})"),
                Some(ExitData {
                    reason: Some(r), ..
                }) => r,
                _ => "exit".to_owned(),
            };
            out.push(TranscriptItem::Exit { reason });
        }
        _ => {}
    }
}

fn map_message(line: &[u8], out: &mut Vec<TranscriptItem>) {
    let head = &line[..line.len().min(ROLE_PEEK)];
    let has = |role: &[u8]| memchr::memmem::find(head, role).is_some();
    let role = if has(b"\"role\":\"assistant\"") {
        "assistant"
    } else if has(b"\"role\":\"toolResult\"") {
        "toolResult"
    } else if has(b"\"role\":\"user\"") {
        "user"
    } else {
        return; // developer / custom: hidden bookkeeping
    };
    let Some(entry) = parse::<MessageEntry>(line, role) else {
        return;
    };
    let m = entry.message;
    let at = m.timestamp.and_then(from_epoch_ms);
    match role {
        "user" => out.push(TranscriptItem::User {
            text: cap_bytes(text_of(m.content, "\n"), MAX_TEXT).0,
            from_agent: m.attribution.as_deref() == Some("agent"),
            at,
        }),
        "toolResult" => {
            let (text, truncated) = cap_bytes(text_of(m.content, "\n"), MAX_TOOL_RESULT);
            out.push(TranscriptItem::ToolResult {
                call_id: m.tool_call_id.unwrap_or_default(),
                name: m.tool_name.unwrap_or_default(),
                text,
                is_error: m.is_error == Some(true),
                truncated,
            });
        }
        _ => {
            let model = match (&m.provider, &m.model) {
                (Some(p), Some(id)) => Some(format!("{p}/{id}")),
                (None, Some(id)) => Some(id.clone()),
                _ => None,
            };
            match m.content {
                Some(Content::Text(text)) => out.push(TranscriptItem::Assistant {
                    text: cap_bytes(text, MAX_TEXT).0,
                    model: model.clone(),
                    at,
                }),
                Some(Content::Parts(parts)) => {
                    for p in parts {
                        match p.kind.as_deref() {
                            Some("thinking") => {
                                if let Some(t) = p.thinking.filter(|t| !t.trim().is_empty()) {
                                    out.push(TranscriptItem::Thinking {
                                        text: cap_bytes(t, MAX_THINKING).0,
                                    });
                                }
                            }
                            Some("text") => {
                                if let Some(t) = p.text.filter(|t| !t.trim().is_empty()) {
                                    out.push(TranscriptItem::Assistant {
                                        text: cap_bytes(t, MAX_TEXT).0,
                                        model: model.clone(),
                                        at,
                                    });
                                }
                            }
                            Some("toolCall") => {
                                let args = p.arguments.unwrap_or(Value::Null);
                                let pretty =
                                    serde_json::to_string_pretty(&args).unwrap_or_default();
                                out.push(TranscriptItem::ToolCall {
                                    call_id: p.id.unwrap_or_default(),
                                    name: p.name.unwrap_or_default(),
                                    intent: p.intent.filter(|i| !i.trim().is_empty()),
                                    summary: call_summary(&args),
                                    args: cap_bytes(pretty, MAX_ARGS).0,
                                });
                            }
                            _ => {}
                        }
                    }
                }
                None => {}
            }
            let failed = match m.stop_reason.as_deref() {
                Some("error") => Some("error"),
                Some("aborted") => Some("aborted"),
                _ => None,
            };
            if let Some(default) = failed {
                out.push(TranscriptItem::Error {
                    message: m
                        .error_message
                        .map_or_else(|| default.to_owned(), |e| cap_bytes(e, MAX_NOTICE).0),
                });
            }
        }
    }
}

/// What a [`TranscriptReader::refresh`] found.
#[derive(Debug, Default, PartialEq)]
pub struct TranscriptUpdate {
    /// The file was replaced or shrank: `items` replace everything read before.
    pub reset: bool,
    /// Items for the newly read complete lines.
    pub items: Vec<TranscriptItem>,
}

/// Synchronous incremental reader behind [`TranscriptWatcher`]; a half-written last line waits
/// for its newline, an inode change or shrink starts over.
pub struct TranscriptReader {
    tail: LineTail,
}

impl TranscriptReader {
    pub fn new(path: PathBuf) -> Self {
        TranscriptReader {
            tail: LineTail::new(path),
        }
    }

    pub fn refresh(&mut self) -> io::Result<TranscriptUpdate> {
        let mut update = TranscriptUpdate::default();
        loop {
            let chunk = self.tail.poll()?;
            if chunk.reset {
                update.reset = true;
                update.items.clear();
            }
            for line in tail::lines(&chunk.data) {
                map_line(line, &mut update.items);
            }
            if !chunk.more {
                return Ok(update);
            }
        }
    }
}

struct Shared {
    latest: Mutex<Arc<Vec<TranscriptItem>>>,
    revision: AtomicU64,
    stop: Mutex<bool>,
    wake: Condvar,
}

/// Follows one transcript every 500 ms.
pub struct TranscriptWatcher {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

impl TranscriptWatcher {
    pub fn spawn(path: PathBuf, on_update: impl Fn() + Send + 'static) -> TranscriptWatcher {
        let shared = Arc::new(Shared {
            latest: Mutex::new(Arc::new(Vec::new())),
            revision: AtomicU64::new(0),
            stop: Mutex::new(false),
            wake: Condvar::new(),
        });
        let worker = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("deck-transcript".into())
            .spawn(move || run(&worker, path, &on_update));
        let handle = match handle {
            Ok(h) => Some(h),
            Err(e) => {
                log::error!("cannot spawn transcript watcher thread: {e}");
                None
            }
        };
        TranscriptWatcher { shared, handle }
    }

    /// Full list so far (append-only growth).
    pub fn latest(&self) -> Arc<Vec<TranscriptItem>> {
        lock(&self.shared.latest).clone()
    }

    /// Bumps on every change.
    pub fn revision(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire)
    }
}

impl Drop for TranscriptWatcher {
    fn drop(&mut self) {
        *lock(&self.shared.stop) = true;
        self.shared.wake.notify_all();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run(shared: &Shared, path: PathBuf, on_update: &dyn Fn()) {
    let mut reader = TranscriptReader::new(path);
    let mut warned = false;
    loop {
        match reader.refresh() {
            Ok(update) => {
                warned = false;
                if update.reset || !update.items.is_empty() {
                    {
                        let mut latest = lock(&shared.latest);
                        if update.reset {
                            *latest = Arc::new(update.items);
                        } else {
                            Arc::make_mut(&mut latest).extend(update.items);
                        }
                    }
                    shared.revision.fetch_add(1, Ordering::Release);
                    on_update();
                }
            }
            Err(e) => {
                if !warned {
                    log::debug!("agents: cannot read transcript: {e}");
                    warned = true;
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
