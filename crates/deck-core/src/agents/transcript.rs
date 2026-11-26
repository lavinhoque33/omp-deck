//! Active-branch journal transcripts, with a live tail.

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
/// `read_blob` decodes images up to this size …
const MAX_BLOB: u64 = 20 * 1024 * 1024;
/// … and skips lines longer than 20 MiB of base64 (4/3 expansion) plus envelope.
const MAX_BLOB_LINE: usize = (MAX_BLOB as usize / 3 + 1) * 4 + 64 * 1024;
/// omp's marker for an image kept in its content-addressed blob store.
const BLOB_REF: &str = "blob:sha256:";
/// `read_blob` keeps the image index of this many transcripts (the latest read).
const MAX_BLOB_INDEXES: usize = 8;
/// Read buffer of an index scan (a long session is hundreds of MiB).
const SCAN_BUF: usize = 256 * 1024;
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

/// Where a transcript's images are: blob id → byte offset of the line holding it. Built on the
/// first lookup and extended as the file grows, so an image late in a long session costs one
/// seek instead of a scan of everything before it.
#[derive(Default)]
struct BlobIndex {
    /// Offset of the first byte not indexed yet (always a line start).
    scanned: u64,
    lines: HashMap<String, u64>,
}

impl BlobIndex {
    /// Indexes what `file` gained since the last call. A last line without its newline is
    /// indexed but read again next time: omp may still be writing it.
    fn extend(&mut self, file: &mut File) -> io::Result<()> {
        file.seek(SeekFrom::Start(self.scanned))?;
        let mut reader = BufReader::with_capacity(SCAN_BUF, file);
        let mut line = Vec::new();
        loop {
            let (read, ended) = read_capped_line(&mut reader, &mut line, MAX_BLOB_LINE)?;
            if read == 0 {
                return Ok(());
            }
            for part in image_parts(&line).iter().flatten() {
                if let Some(data) = image_data(part) {
                    self.lines.entry(blob_id(data)).or_insert(self.scanned);
                }
            }
            if !ended {
                return Ok(());
            }
            self.scanned += read;
        }
    }
}

/// Indexes of the transcripts read most recently, latest first.
static BLOB_INDEXES: Mutex<Vec<(PathBuf, Arc<Mutex<BlobIndex>>)>> = Mutex::new(Vec::new());

/// The index of `transcript`; lookups in one transcript share it (and wait for each other),
/// other transcripts are not held up.
fn blob_index(transcript: &Path) -> Arc<Mutex<BlobIndex>> {
    let mut all = lock(&BLOB_INDEXES);
    let at = match all.iter().position(|(p, _)| p == transcript) {
        Some(at) => at,
        None => {
            all.truncate(MAX_BLOB_INDEXES - 1);
            all.push((transcript.to_path_buf(), Arc::default()));
            all.len() - 1
        }
    };
    all[..=at].rotate_right(1);
    Arc::clone(&all[0].1)
}

/// The content parts of a transcript line that may hold an image; `None` for any other line,
/// a truncated tail or a malformed one.
fn image_parts(line: &[u8]) -> Option<Vec<Value>> {
    memchr::memmem::find(line, b"\"image\"")?;
    let mut v: Value = serde_json::from_slice(line).ok()?;
    let content = match v.pointer_mut("/message/content") {
        Some(c) => c.take(),
        None => v.get_mut("content")?.take(),
    };
    match content {
        Value::Array(parts) => Some(parts),
        _ => None,
    }
}

/// The `data` of an image part: its base64, or omp's blob-store reference.
fn image_data(part: &Value) -> Option<&str> {
    if part.get("type").and_then(Value::as_str) != Some("image") {
        return None;
    }
    part.get("data").and_then(Value::as_str)
}

/// The `(mime, data)` of image `blob` on the line at offset `at`; `None` when that line does
/// not hold it (the file changed under the index).
fn image_at(file: &mut File, at: u64, blob: &str) -> io::Result<Option<(String, String)>> {
    file.seek(SeekFrom::Start(at))?;
    let mut line = Vec::new();
    read_capped_line(&mut BufReader::new(file), &mut line, MAX_BLOB_LINE)?;
    Ok(image_parts(&line)
        .into_iter()
        .flatten()
        .find_map(|mut part| {
            if image_data(&part).is_none_or(|data| blob_id(data) != blob) {
                return None;
            }
            let mime = part
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream")
                .to_owned();
            match part.get_mut("data").map(Value::take) {
                Some(Value::String(data)) => Some((mime, data)),
                _ => None,
            }
        }))
}

/// Finds image `blob` through the transcript's index: `(mime, data)`.
fn locate_blob(transcript: &Path, blob: &str) -> io::Result<Option<(String, String)>> {
    let mut file = File::open(transcript)?;
    let index = blob_index(transcript);
    let mut index = lock(&index);
    let mut rebuilt = false;
    loop {
        if file.metadata()?.len() < index.scanned {
            *index = BlobIndex::default();
        }
        rebuilt |= index.scanned == 0;
        index.extend(&mut file)?;
        let found = match index.lines.get(blob) {
            Some(&at) => image_at(&mut file, at, blob)?,
            None => None,
        };
        if found.is_some() || rebuilt {
            return Ok(found);
        }
        // Not where (or what) the index says: the file was rewritten under the same name.
        *index = BlobIndex::default();
    }
}

/// Finds the image `blob` in `transcript` and decodes it: `(mime, bytes)`.
///
/// The whole transcript is searched, whatever its size: the first lookup indexes its images
/// line by line (lines over 28 MiB are skipped unparsed), later ones only read what was
/// appended since. An image whose decoded size exceeds `max_bytes` (itself capped at 20 MiB)
/// yields `None`. A truncated last line is tolerated. Images omp externalized to its blob
/// store (`<agent dir>/blobs/<sha256>`) are read from there, found by walking up to the
/// `sessions` directory the transcript lives under.
pub fn read_blob(
    transcript: &Path,
    blob: &str,
    max_bytes: u64,
) -> io::Result<Option<(String, Vec<u8>)>> {
    use base64::Engine;
    let max = max_bytes.min(MAX_BLOB);
    let Some((mime, data)) = locate_blob(transcript, blob)? else {
        return Ok(None);
    };
    if let Some(hash) = data.strip_prefix(BLOB_REF) {
        return read_external(transcript, hash, max).map(|b| b.map(|b| (mime, b)));
    }
    if decoded_len(&data) > max {
        return Ok(None);
    }
    Ok(base64::engine::general_purpose::STANDARD
        .decode(&data)
        .ok()
        .map(|b| (mime, b)))
}

/// Reads one line into `buf` (without its `\n`); a line over `cap` bytes is consumed but left
/// empty. Returns the bytes consumed (0 at end of input) and whether a `\n` ended the line.
fn read_capped_line(
    r: &mut impl BufRead,
    buf: &mut Vec<u8>,
    cap: usize,
) -> io::Result<(u64, bool)> {
    buf.clear();
    let mut read = 0;
    let mut over = false;
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            return Ok((read, false));
        }
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
        let used = take + usize::from(done);
        r.consume(used);
        read += used as u64;
        if done {
            return Ok((read, true));
        }
    }
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

/// What the tree preview shows for one line: like [`map_line`], but for the tree's needs —
/// hidden entries become visible as notices instead of vanishing. The subagent transcript
/// behaviour of `map_line` itself is unchanged (hidden custom messages stay hidden there).
/// `custom_message` regardless of `display` → `Notice { kind: customType, text }` (advisor:
/// its qualifier + note text); `developer` and `custom`-role messages → `Notice`;
/// `branch_summary` → `Notice { kind: "branch_summary" }`; `compaction` → `Compaction`.
/// Assistant text/thinking/tool calls/error map exactly as in the transcript.
pub fn entry_items(line: &[u8]) -> Vec<TranscriptItem> {
    let mut out = Vec::new();
    map_entry(line, &mut out);
    out
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

/// Tree-preview variant of [`map_line`]: every entry kind yields what the preview pane shows,
/// WITHOUT adding `TranscriptItem` variants.
fn map_entry(line: &[u8], out: &mut Vec<TranscriptItem>) {
    match line_type(line) {
        Some("message") => map_entry_message(line, out),
        Some("custom_message") => {
            let Some(m) = parse::<CustomMessage>(line, "custom_message") else {
                return;
            };
            let kind = m.custom_type.clone().unwrap_or_else(|| "message".into());
            let text = if kind == "advisor" {
                advisor_entry_text(line)
            } else {
                cap_bytes(text_of(m.content, "\n"), MAX_NOTICE).0
            };
            if kind.starts_with("irc") {
                push_split_irc(text, out, |text| TranscriptItem::Notice {
                    kind: kind.clone(),
                    text,
                });
            } else {
                out.push(TranscriptItem::Notice { kind, text });
            }
        }
        Some("branch_summary") => {
            let summary = parse_branch_summary(line);
            out.push(TranscriptItem::Notice {
                kind: "branch_summary".to_owned(),
                text: summary,
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
        // Model/thinking/label/settings and everything else: the UI shows the row text.
        _ => {}
    }
}

/// `branch_summary` entry's `summary` string.
#[derive(Deserialize)]
struct BranchSummaryLine {
    summary: Option<String>,
}

fn parse_branch_summary(line: &[u8]) -> String {
    parse::<BranchSummaryLine>(line, "branch_summary")
        .and_then(|b| b.summary)
        .map(|s| cap_bytes(s, MAX_SUMMARY).0)
        .unwrap_or_default()
}

/// Advisor note text for a tree preview: the qualifier + note bodies omp's tree row shows.
fn advisor_entry_text(line: &[u8]) -> String {
    #[derive(Deserialize)]
    struct Details {
        #[serde(default)]
        notes: Vec<AdvisorNote>,
    }
    #[derive(Deserialize)]
    struct AdvisorNote {
        note: Option<String>,
        advisor: Option<String>,
        severity: Option<String>,
    }
    let details: Option<Details> = serde_json::from_slice::<Value>(line)
        .ok()
        .and_then(|v| v.get("details").cloned())
        .and_then(|d| serde_json::from_value(d).ok());
    let notes = details.map(|d| d.notes).unwrap_or_default();
    let mut texts = Vec::new();
    let mut advisors: Vec<String> = Vec::new();
    let mut severities: Vec<String> = Vec::new();
    for n in notes {
        if let Some(t) = n.note {
            texts.push(t);
        }
        if let Some(a) = n.advisor
            && !a.is_empty()
            && a != "default"
            && !advisors.contains(&a)
        {
            advisors.push(a);
        }
        if let Some(s) = n.severity
            && !s.is_empty()
            && !severities.contains(&s)
        {
            severities.push(s);
        }
    }
    let qualifier = advisors
        .into_iter()
        .chain(severities)
        .collect::<Vec<_>>()
        .join(", ");
    let body = texts.join(" ");
    let text = if qualifier.is_empty() {
        body
    } else if body.is_empty() {
        qualifier.clone()
    } else {
        format!("{qualifier}: {body}")
    };
    cap_bytes(text, MAX_NOTICE).0
}

/// Tree-preview message mapping: user/assistant/toolResult exactly as the transcript mapper;
/// `developer` and `custom`-role messages become `Notice` instead of staying hidden.
fn map_entry_message(line: &[u8], out: &mut Vec<TranscriptItem>) {
    let head = &line[..line.len().min(ROLE_PEEK)];
    let has = |role: &[u8]| memchr::memmem::find(head, role).is_some();
    if has(b"\"role\":\"developer\"") || has(b"\"role\":\"custom\"") {
        let parsed: Option<MessageEntry> = parse(line, "developer/custom");
        let Some(entry) = parsed else { return };
        let role = entry_role(line);
        let kind = role.unwrap_or_else(|| "developer".to_owned());
        let text = cap_bytes(text_of(entry.message.content, "\n"), MAX_NOTICE).0;
        out.push(TranscriptItem::Notice { kind, text });
        return;
    }
    map_message(line, out);
}

/// The `role` string of a `message` line (ids/roles never need unescaping).
fn entry_role(line: &[u8]) -> Option<String> {
    tail::simple_str(line, "role", ROLE_PEEK.min(line.len())).map(str::to_owned)
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

/// The live leaf as of the last entry `tail`, or as of the whole file when `tail` is absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafAnchor {
    pub leaf: Option<String>,
    pub tail: Option<String>,
}

// Visit top-level metadata only; serde skips the message and other fields without materialising
// them. The newtype borrows ordinary strings and also accepts escaped ones without guessing.
#[derive(Deserialize)]
struct EntryId<'a>(#[serde(borrow)] Cow<'a, str>);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EntryHeader<'a> {
    #[serde(borrow)]
    id: Option<EntryId<'a>>,
    #[serde(borrow)]
    parent_id: Option<EntryId<'a>>,
    #[serde(borrow, default, deserialize_with = "present_leaf")]
    active_leaf_id: Option<Option<EntryId<'a>>>,
    #[serde(rename = "type", borrow)]
    kind: Option<EntryId<'a>>,
}

fn present_leaf<'de: 'a, 'a, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Option<EntryId<'a>>>, D::Error> {
    Option::deserialize(deserializer).map(Some)
}

#[derive(Default)]
struct Journal {
    entries: Vec<Entry>,
    ids: HashMap<Box<str>, usize>,
    waiting_for_parent: HashMap<Box<str>, Vec<usize>>,
    last_self: Option<usize>,
}

struct Entry {
    parent: Option<usize>,
    effect: LeafEffect,
    offset: u64,
    /// Bytes including the newline; 0 for a line that exceeds the rendering cap.
    len: usize,
}

enum LeafEffect {
    Own,
    Empty,
    Other(Box<str>),
}

impl Journal {
    fn add(&mut self, header: EntryHeader<'_>, offset: u64, len: usize) {
        let Some(EntryId(id)) = header.id else { return };
        // The session header may carry a session UUID, not a journal entry id.
        if id.is_empty()
            || header
                .kind
                .is_none_or(|k| k.0 == "session" || k.0 == "title")
            || self.ids.contains_key(id.as_ref())
        {
            return;
        }
        let index = self.entries.len();
        let parent = header.parent_id.filter(|p| p.0 != id);
        let parent_index = parent
            .as_ref()
            .and_then(|p| self.ids.get(p.0.as_ref()).copied());
        if let (Some(EntryId(parent)), None) = (parent, parent_index) {
            self.waiting_for_parent
                .entry(parent.into_owned().into_boxed_str())
                .or_default()
                .push(index);
        }
        let effect = match header.active_leaf_id {
            Some(None) => LeafEffect::Empty,
            Some(Some(EntryId(leaf))) if leaf != id => {
                LeafEffect::Other(leaf.into_owned().into_boxed_str())
            }
            _ => {
                self.last_self = Some(index);
                LeafEffect::Own
            }
        };
        self.entries.push(Entry {
            parent: parent_index,
            effect,
            offset,
            len,
        });
        if let Some(children) = self.waiting_for_parent.remove(id.as_ref()) {
            for child in children {
                // A malformed cyclic chain is rooted rather than looping forever.
                let mut ancestor = Some(index);
                while let Some(i) = ancestor {
                    if i == child {
                        break;
                    }
                    ancestor = self.entries[i].parent;
                }
                if ancestor.is_none() {
                    self.entries[child].parent = Some(index);
                }
            }
        }
        self.ids.insert(id.into_owned().into_boxed_str(), index);
    }

    fn leaf(&self) -> Option<usize> {
        let (index, entry) = self.entries.iter().enumerate().next_back()?;
        match &entry.effect {
            LeafEffect::Own => Some(index),
            LeafEffect::Empty => None,
            LeafEffect::Other(id) => self.ids.get(id.as_ref()).copied().or(self.last_self),
        }
    }

    fn fold(&self, start: usize, mut leaf: Option<usize>) -> Option<usize> {
        for (index, entry) in self.entries.iter().enumerate().skip(start) {
            leaf = match &entry.effect {
                LeafEffect::Own => Some(index),
                LeafEffect::Empty => None,
                // An unreadable or missing target cannot replace the last known leaf.
                LeafEffect::Other(id) => self.ids.get(id.as_ref()).copied().or(leaf),
            };
        }
        leaf
    }
}

#[derive(Default)]
struct Anchors {
    latest: Option<LeafAnchor>,
    pending: bool,
}

impl Anchors {
    fn set(&mut self, anchor: LeafAnchor) -> bool {
        // An unpinned navigate reply is a new event even if it names the same leaf.
        if anchor.tail.is_some() && self.latest.as_ref() == Some(&anchor) {
            return false;
        }
        self.replace(anchor);
        true
    }

    fn replace(&mut self, anchor: LeafAnchor) {
        self.latest = Some(anchor);
        self.pending = true;
    }

    fn pending(&self) -> Option<&LeafAnchor> {
        self.latest.as_ref().filter(|_| self.pending)
    }
}

#[derive(Clone, Copy)]
struct ActivePath {
    root: usize,
    leaf: usize,
}

/// What a [`TranscriptReader::refresh`] found.
#[derive(Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TranscriptUpdate {
    /// The path changed (or the file was replaced or shrank): `items` replace the previous list.
    pub reset: bool,
    /// Items from the active path, or just its newly appended suffix when not resetting.
    pub items: Vec<TranscriptItem>,
}

/// Synchronous incremental reader behind [`TranscriptWatcher`]. It indexes journal entries,
/// waits for a partial line's newline, and renders only the active path (or its new suffix).
pub struct TranscriptReader {
    tail: LineTail,
    journal: Journal,
    /// Number of entries when we last reached EOF, or `None` during initial indexing.
    seen_entries: Option<usize>,
    leaf: Option<usize>,
    published: Option<ActivePath>,
    reset_pending: bool,
    anchors: Anchors,
}

impl TranscriptReader {
    pub fn new(path: PathBuf) -> Self {
        TranscriptReader {
            tail: LineTail::new(path),
            journal: Journal::default(),
            seen_entries: None,
            leaf: None,
            published: None,
            reset_pending: false,
            anchors: Anchors::default(),
        }
    }

    /// Queue the live leaf for the next refresh, once its tail has been read.
    pub fn anchor(&mut self, anchor: LeafAnchor) {
        self.anchors.set(anchor);
    }

    pub fn refresh(&mut self) -> io::Result<TranscriptUpdate> {
        self.refresh_until(&|| false)
    }

    fn refresh_until(&mut self, cancelled: &dyn Fn() -> bool) -> io::Result<TranscriptUpdate> {
        check_cancelled(cancelled)?;
        let mut file = crate::platform::open_regular_file(self.tail.path())?;
        loop {
            check_cancelled(cancelled)?;
            let chunk = self.tail.poll_file(&mut file)?;
            if chunk.reset {
                self.journal = Journal::default();
                self.leaf = None;
                self.published = None;
                self.seen_entries = None;
                self.reset_pending = true;
                self.anchors = Anchors::default();
            }
            if let Some(span) = chunk.skipped {
                // Index a dropped line without buffering it, so descendants can still reach its
                // parent. The span reader and tail use this same open file, even across a rename.
                let header = file.seek(SeekFrom::Start(span.start)).and_then(|_| {
                    let reader = BufReader::with_capacity(
                        SCAN_BUF,
                        CancelRead {
                            inner: (&mut file).take(span.end - span.start),
                            cancelled,
                        },
                    );
                    let mut de = serde_json::Deserializer::from_reader(reader);
                    let header = EntryHeader::deserialize(&mut de)?;
                    de.end()?;
                    Ok(header)
                });
                if let Err(e) = check_cancelled(cancelled) {
                    self.tail.rewind(span.start);
                    return Err(e);
                }
                match header {
                    Ok(header) => self.journal.add(header, span.start, 0),
                    Err(e) => {
                        log::debug!("agents: skipping unreadable oversized transcript entry: {e}")
                    }
                }
            }
            let mut start = 0;
            for end in memchr::memchr_iter(b'\n', &chunk.data) {
                let line = &chunk.data[start..end];
                if !line.is_empty()
                    && let Some(header) = parse::<EntryHeader>(line, "transcript entry")
                {
                    let len = end + 1 - start;
                    self.journal.add(
                        header,
                        chunk.offset + start as u64,
                        if line.len() <= tail::MAX_LINE { len } else { 0 },
                    );
                }
                start = end + 1;
            }
            if !chunk.more {
                break;
            }
        }
        let entries = self.journal.entries.len();
        if self.seen_entries != Some(entries) {
            self.leaf = match self.seen_entries {
                Some(start) => self.journal.fold(start, self.leaf),
                None => self.journal.leaf(),
            };
            self.seen_entries = Some(entries);
        }
        self.apply_anchor();
        self.render(file, cancelled)
    }

    fn apply_anchor(&mut self) {
        let Some(anchor) = self.anchors.pending() else {
            return;
        };
        let start = if let Some(tail) = &anchor.tail {
            let Some(&index) = self.journal.ids.get(tail.as_str()) else {
                return;
            };
            index + 1
        } else {
            self.journal.entries.len()
        };
        let leaf = match &anchor.leaf {
            Some(id) => self.journal.ids.get(id.as_str()).copied().map(Some),
            None => Some(None),
        };
        self.anchors.pending = false;
        let Some(leaf) = leaf else { return };
        self.leaf = self.journal.fold(start, leaf);
    }

    fn render(
        &mut self,
        mut file: File,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<TranscriptUpdate> {
        let mut update = TranscriptUpdate {
            reset: self.reset_pending,
            items: Vec::new(),
        };
        let old = self.published;
        let mut at = self.leaf;
        let mut suffix = Vec::new();
        // Parent links are immutable except for a missing parent that arrives late. Only the
        // published root can acquire such a link; in that case the old path cannot be a prefix.
        let boundary = old
            .filter(|p| self.journal.entries[p.root].parent.is_none())
            .map(|p| p.leaf);
        while let Some(i) = at {
            if Some(i) == boundary {
                break;
            }
            if suffix.len().is_multiple_of(256) {
                check_cancelled(cancelled)?;
            }
            suffix.push(i);
            at = self.journal.entries[i].parent;
        }
        let root = if at.is_some() {
            old.map(|p| p.root)
        } else {
            update.reset |= old.is_some();
            suffix.last().copied()
        };
        let mut line = Vec::new();
        let mut pos = None;
        for (n, i) in suffix.into_iter().rev().enumerate() {
            let entry = &self.journal.entries[i];
            if n.is_multiple_of(256) || entry.len > SCAN_BUF {
                check_cancelled(cancelled)?;
            }
            if entry.len == 0 {
                continue;
            }
            line.resize(entry.len, 0);
            let read = if pos == Some(entry.offset) {
                file.read_exact(&mut line)
            } else {
                file.seek(SeekFrom::Start(entry.offset))
                    .and_then(|_| file.read_exact(&mut line))
            };
            match read {
                Ok(()) => {
                    pos = Some(entry.offset + entry.len as u64);
                    map_line(&line, &mut update.items);
                }
                Err(e) => {
                    pos = None;
                    log::debug!(
                        "agents: cannot read transcript entry at {}: {e}",
                        entry.offset
                    );
                }
            }
        }
        check_cancelled(cancelled)?;
        self.published = root
            .zip(self.leaf)
            .map(|(root, leaf)| ActivePath { root, leaf });
        self.reset_pending = false;
        Ok(update)
    }
}

fn check_cancelled(cancelled: &dyn Fn() -> bool) -> io::Result<()> {
    if cancelled() {
        Err(io::ErrorKind::Interrupted.into())
    } else {
        Ok(())
    }
}

struct CancelRead<'a, R> {
    inner: R,
    cancelled: &'a dyn Fn() -> bool,
}

impl<R: Read> Read for CancelRead<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // serde's byte iterator retries Interrupted. Use a different I/O error to stop the
        // streaming parse; refresh_until checks cancellation again before using the result.
        if (self.cancelled)() {
            return Err(io::Error::other("transcript refresh cancelled"));
        }
        self.inner.read(buf)
    }
}

#[derive(Default)]
struct Published {
    items: Arc<Vec<TranscriptItem>>,
    epoch: u64,
}

#[derive(Default)]
struct Control {
    anchors: Anchors,
}

struct Shared {
    latest: Mutex<Published>,
    revision: AtomicU64,
    stop: AtomicBool,
    control: Mutex<Control>,
    wake: Condvar,
}

/// Follows one transcript every 500 ms.
pub struct TranscriptWatcher {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

impl TranscriptWatcher {
    pub fn spawn(path: PathBuf, on_update: impl Fn() + Send + 'static) -> TranscriptWatcher {
        Self::spawn_anchored(path, None, on_update)
    }

    /// Start with a live anchor already queued, before publishing any items.
    pub fn spawn_anchored(
        path: PathBuf,
        anchor: Option<LeafAnchor>,
        on_update: impl Fn() + Send + 'static,
    ) -> TranscriptWatcher {
        let mut control = Control::default();
        if let Some(anchor) = anchor {
            control.anchors.replace(anchor);
        }
        let shared = Arc::new(Shared {
            latest: Mutex::default(),
            revision: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            control: Mutex::new(control),
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

    /// Current root-to-leaf transcript. A branch jump can replace it.
    pub fn latest(&self) -> Arc<Vec<TranscriptItem>> {
        Arc::clone(&lock(&self.shared.latest).items)
    }

    /// Bumps on every transcript reset.
    pub fn epoch(&self) -> u64 {
        lock(&self.shared.latest).epoch
    }

    /// Read the reset epoch and the exact snapshot it describes together.
    pub fn latest_with_epoch(&self) -> (u64, Arc<Vec<TranscriptItem>>) {
        let latest = lock(&self.shared.latest);
        (latest.epoch, Arc::clone(&latest.items))
    }

    /// Queue a live leaf and wake the watcher without waiting for the regular poll.
    pub fn anchor(&self, anchor: LeafAnchor) {
        if lock(&self.shared.control).anchors.set(anchor) {
            self.shared.wake.notify_one();
        }
    }

    /// Bumps on initial publication (even if empty) and every subsequent change.
    pub fn revision(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire)
    }
}

impl Drop for TranscriptWatcher {
    fn drop(&mut self) {
        {
            // Use the wake mutex when changing the predicate, so the waiter cannot miss it.
            let _control = lock(&self.shared.control);
            self.shared.stop.store(true, Ordering::Relaxed);
            self.shared.wake.notify_all();
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run(shared: &Shared, path: PathBuf, on_update: &dyn Fn()) {
    let mut reader = TranscriptReader::new(path);
    let stopped = || shared.stop.load(Ordering::Relaxed);
    let mut warned = false;
    let mut published = false;
    loop {
        {
            let mut control = lock(&shared.control);
            if stopped() {
                return;
            }
            if let Some(anchor) = control.anchors.pending().cloned() {
                control.anchors.pending = false;
                reader.anchors.replace(anchor);
            }
        }
        match reader.refresh_until(&stopped) {
            _ if stopped() => return,
            Ok(update) => {
                warned = false;
                if update.reset && reader.anchors.latest.is_none() {
                    let mut control = lock(&shared.control);
                    if !control.anchors.pending {
                        control.anchors.latest = None;
                    }
                }
                if !published || update.reset || !update.items.is_empty() {
                    {
                        let mut latest = lock(&shared.latest);
                        if update.reset {
                            latest.items = Arc::new(update.items);
                            latest.epoch += 1;
                        } else if !update.items.is_empty() {
                            Arc::make_mut(&mut latest.items).extend(update.items);
                        }
                        shared.revision.fetch_add(1, Ordering::Release);
                    }
                    published = true;
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
        let mut control = lock(&shared.control);
        let deadline = std::time::Instant::now() + POLL;
        while !stopped() && !control.anchors.pending {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            control = shared
                .wake
                .wait_timeout(control, left)
                .map_or_else(|e| e.into_inner().0, |(g, _)| g);
        }
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;

    #[test]
    fn refresh_can_stop_during_indexing_or_rendering_without_publishing_a_partial_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut file = File::create(&path).unwrap();
        use std::io::Write as _;
        const COUNT: usize = 600;
        for i in 0..COUNT {
            let parent = i.checked_sub(1).map(|n| n.to_string());
            writeln!(
                file,
                r#"{{"type":"message","id":"{i}","parentId":{},"message":{{"role":"user","content":"prompt"}}}}"#,
                serde_json::json!(parent),
            )
            .unwrap();
        }
        drop(file);

        // After the pre-open check, stop at the first indexing chunk.
        let mut reader = TranscriptReader::new(path.clone());
        let checks = Cell::new(0);
        let cancel_after = |limit| {
            checks.set(checks.get() + 1);
            checks.get() >= limit
        };
        let err = reader.refresh_until(&|| cancel_after(2)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert!(reader.journal.entries.is_empty());

        // Count past indexing and the parent walk, then stop after rendering 256 entries.
        checks.set(0);
        let err = reader.refresh_until(&|| cancel_after(7)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert_eq!(reader.seen_entries, Some(COUNT));
        assert!(reader.published.is_none());
        assert_eq!(reader.refresh().unwrap().items.len(), COUNT);
        assert_eq!(
            TranscriptReader::new(path).refresh().unwrap().items.len(),
            COUNT
        );
    }
}
