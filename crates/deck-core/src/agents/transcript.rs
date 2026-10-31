//! Subagent transcript → [`TranscriptItem`]s (docs/notes/agent-hub.md §6), with a live tail.

use std::cell::Cell;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::markup::{Segment, bracket_note, segments};
use super::tail::{self, LineTail, cap_bytes, from_epoch_ms, line_type, one_line};
use super::{ImageRef, TranscriptItem};
use crate::status::lock;

const MAX_TOOL_RESULT: usize = 16 * 1024;
const MAX_THINKING: usize = 64 * 1024;
const MAX_ARGS: usize = 4 * 1024;
const MAX_ARG_LEAF: usize = 16 * 1024;
const MAX_ARGS_VALUE: usize = 64 * 1024;
const MAX_DETAILS: usize = 32 * 1024;
/// `read_blob` reads at most this much of a transcript …
const MAX_SCAN: u64 = 64 * 1024 * 1024;
/// … decodes images up to this size …
const MAX_BLOB: u64 = 20 * 1024 * 1024;
/// … and skips lines longer than 20 MiB of base64 (4/3 expansion) plus envelope.
const MAX_BLOB_LINE: usize = (MAX_BLOB as usize / 3 + 1) * 4 + 64 * 1024;
/// omp's marker for an image kept in its content-addressed blob store.
const BLOB_REF: &str = "blob:sha256:";
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
    details: Option<Value>,
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
    data: Option<String>,
    #[serde(rename = "mimeType")]
    mime_type: Option<String>,
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

/// Cuts every string leaf to [`MAX_ARG_LEAF`] bytes (`…` appended); `None` when the result is
/// still over [`MAX_ARGS_VALUE`] serialized.
fn structured_args(mut args: Value) -> Option<Value> {
    fn cut(v: &mut Value) {
        match v {
            Value::String(s) if s.len() > MAX_ARG_LEAF => {
                let (mut c, _) = cap_bytes(std::mem::take(s), MAX_ARG_LEAF);
                c.push('…');
                *s = c;
            }
            Value::Array(a) => a.iter_mut().for_each(cut),
            Value::Object(o) => o.values_mut().for_each(cut),
            _ => {}
        }
    }
    cut(&mut args);
    serde_json::to_vec(&args)
        .is_ok_and(|b| b.len() <= MAX_ARGS_VALUE)
        .then_some(args)
}

/// Stable id of an image: first 16 hex digits of SHA-256 over its `data` string.
fn blob_id(data: &str) -> String {
    use std::fmt::Write;
    Sha256::digest(data.as_bytes())
        .iter()
        .take(8)
        .fold(String::with_capacity(16), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Decoded size of standard base64 `data` (padding aware).
fn decoded_len(data: &str) -> u64 {
    let pad = data.bytes().rev().take_while(|b| *b == b'=').count();
    (data.len() as u64 / 4 * 3).saturating_sub(pad as u64)
}

fn image_ref(p: &Part) -> Option<ImageRef> {
    if p.kind.as_deref() != Some("image") {
        return None;
    }
    let data = p.data.as_deref()?;
    Some(ImageRef {
        blob: blob_id(data),
        mime: p
            .mime_type
            .clone()
            .unwrap_or_else(|| "application/octet-stream".into()),
        bytes: if data.starts_with(BLOB_REF) {
            0
        } else {
            decoded_len(data)
        },
    })
}

fn images_of(content: Option<&Content>) -> Vec<ImageRef> {
    match content {
        Some(Content::Parts(parts)) => parts.iter().filter_map(image_ref).collect(),
        _ => Vec::new(),
    }
}

/// Finds the image `blob` in `transcript` and decodes it: `(mime, bytes)`.
///
/// A bounded scan: at most 64 MiB of the file is read, lines over 28 MiB are skipped unparsed,
/// and an image whose decoded size exceeds `max_bytes` (itself capped at 20 MiB) yields `None`.
/// A truncated last line is tolerated. Images omp externalized to its blob store
/// (`<agent dir>/blobs/<sha256>`) are read from there, found by walking up to the `sessions`
/// directory the transcript lives under.
pub fn read_blob(
    transcript: &Path,
    blob: &str,
    max_bytes: u64,
) -> io::Result<Option<(String, Vec<u8>)>> {
    use base64::Engine;
    let max = max_bytes.min(MAX_BLOB);
    let mut file = BufReader::new(File::open(transcript)?.take(MAX_SCAN));
    let mut line = Vec::new();
    loop {
        let more = read_capped_line(&mut file, &mut line, MAX_BLOB_LINE)?;
        if !more {
            return Ok(None);
        }
        if line.is_empty() || memchr::memmem::find(&line, b"\"image\"").is_none() {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&line) else {
            continue; // truncated tail or malformed line
        };
        let content = v.pointer("/message/content").or_else(|| v.get("content"));
        let Some(parts) = content.and_then(Value::as_array) else {
            continue;
        };
        for p in parts {
            let Some(data) = p.get("data").and_then(Value::as_str) else {
                continue;
            };
            if p.get("type").and_then(Value::as_str) != Some("image") || blob_id(data) != blob {
                continue;
            }
            let mime = p
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream")
                .to_owned();
            if let Some(hash) = data.strip_prefix(BLOB_REF) {
                return read_external(transcript, hash, max).map(|b| b.map(|b| (mime, b)));
            }
            if decoded_len(data) > max {
                return Ok(None);
            }
            return Ok(base64::engine::general_purpose::STANDARD
                .decode(data)
                .ok()
                .map(|b| (mime, b)));
        }
    }
}

/// Reads one `\n`-terminated line (without it) into `buf`; a line over `cap` bytes is consumed
/// but left empty. `false` at end of input with nothing read.
fn read_capped_line(r: &mut impl BufRead, buf: &mut Vec<u8>, cap: usize) -> io::Result<bool> {
    buf.clear();
    let mut any = false;
    let mut over = false;
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        any = true;
        let (take, done) = match memchr::memchr(b'\n', chunk) {
            Some(i) => (i, true),
            None => (chunk.len(), false),
        };
        if !over {
            if buf.len() + take > cap {
                over = true;
                buf.clear();
            } else {
                buf.extend_from_slice(&chunk[..take]);
            }
        }
        r.consume(take + usize::from(done));
        if done {
            break;
        }
    }
    Ok(any)
}

/// An externalized image: `<agent dir>/blobs/<hash>` beside the `sessions` ancestor.
fn read_external(transcript: &Path, hash: &str, max: u64) -> io::Result<Option<Vec<u8>>> {
    // omp only accepts canonical 64-hex hashes; anything else could escape the blob dir.
    if hash.len() != 64 || !hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Ok(None);
    }
    let Some(agent_dir) = transcript
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "sessions"))
        .and_then(Path::parent)
    else {
        return Ok(None);
    };
    let path = agent_dir.join("blobs").join(hash);
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= max).then_some(bytes))
}

fn call_summary(args: &Value) -> String {
    ["command", "path", "pattern", "query", "url", "file", "i"]
        .iter()
        .filter_map(|k| args.get(k).and_then(Value::as_str))
        .find(|s| !s.trim().is_empty())
        .map(|s| one_line(s, 200))
        .unwrap_or_default()
}

/// Splits `text` around its `<irc>` blocks: each becomes a [`TranscriptItem::Irc`] (a bracketed
/// note line just before it rides along as its note), every other stretch goes through `other`.
fn push_split_irc(
    text: String,
    out: &mut Vec<TranscriptItem>,
    other: impl Fn(String) -> TranscriptItem,
) {
    let segs = segments(&text);
    if !segs
        .iter()
        .any(|s| matches!(s, Segment::Tagged(t) if t.name == "irc"))
    {
        out.push(other(text));
        return;
    }
    // `note`: a bracketed-note text segment waiting to see whether a block follows it.
    let mut note: Option<&str> = None;
    let mut rest = String::new();
    let flush = |rest: &mut String, out: &mut Vec<TranscriptItem>| {
        let t = rest.trim();
        if !t.is_empty() {
            out.push(other(t.to_owned()));
        }
        rest.clear();
    };
    for seg in &segs {
        let irc = match seg {
            Segment::Tagged(t) if t.name == "irc" => t,
            seg => {
                rest.extend(note.take());
                match seg {
                    Segment::Text(s) if bracket_note(s).is_some() => note = Some(s),
                    Segment::Text(s) => rest.push_str(s),
                    Segment::Tagged(t) => {
                        rest.push_str(t.raw);
                        rest.push('\n');
                    }
                }
                continue;
            }
        };
        flush(&mut rest, out);
        let (agent, body) = match irc.attr("agent") {
            Some(a) => (Some(a.to_owned()), irc.body),
            None => incoming_line(irc.body),
        };
        out.push(TranscriptItem::Irc {
            from: irc.attr("from").map(str::to_owned),
            agent,
            text: body.trim().to_owned(),
            note: note.take().and_then(bracket_note).map(str::to_owned),
        });
    }
    rest.extend(note);
    flush(&mut rest, out);
}

/// An `irc:incoming` body opens with "Incoming IRC message from agent `X`:"; that line becomes
/// the agent.
fn incoming_line(body: &str) -> (Option<String>, &str) {
    let body = body.trim_start();
    let (first, rest) = body.split_once('\n').unwrap_or((body, ""));
    let agent = first
        .trim()
        .strip_prefix("Incoming IRC message from agent `")
        .and_then(|a| a.strip_suffix("`:"));
    match agent {
        Some(a) => (Some(a.to_owned()), rest),
        None => (None, body),
    }
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
            let kind = m.custom_type.unwrap_or_else(|| "message".into());
            if kind.starts_with("irc") {
                push_split_irc(text, out, |text| TranscriptItem::Notice {
                    kind: kind.clone(),
                    text,
                });
            } else {
                out.push(TranscriptItem::Notice { kind, text });
            }
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
        "user" => {
            let from_agent = m.attribution.as_deref() == Some("agent");
            // Attachments ride on the first item the message yields.
            let images = Cell::new(images_of(m.content.as_ref()));
            push_split_irc(
                cap_bytes(text_of(m.content, "\n"), MAX_TEXT).0,
                out,
                |text| TranscriptItem::User {
                    text,
                    from_agent,
                    at,
                    images: images.take(),
                },
            );
        }
        "toolResult" => {
            let images = images_of(m.content.as_ref());
            let (text, truncated) = cap_bytes(text_of(m.content, "\n"), MAX_TOOL_RESULT);
            let details = m
                .details
                .filter(|d| serde_json::to_vec(d).is_ok_and(|b| b.len() <= MAX_DETAILS));
            out.push(TranscriptItem::ToolResult {
                call_id: m.tool_call_id.unwrap_or_default(),
                name: m.tool_name.unwrap_or_default(),
                text,
                is_error: m.is_error == Some(true),
                truncated,
                details,
                images,
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
                                    args_value: structured_args(args),
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
#[derive(Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
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
