//! Inventory of every persisted omp session (`<root>/<project-dir>/*.jsonl`) for the session
//! manager hub: size on disk (jsonl + sibling artifact dir + `.bak` files), header facts and,
//! on demand, per-session usage statistics ([`StatsCache`]).
//!
//! [`scan`] does metadata plus bounded header reads only. Statistics parse the transcript itself
//! (omp's `stats.db` is only synced while `omp stats` runs, so it is stale) and are cached per
//! file by (path, len, mtime); an appended file is parsed incrementally from the cached offset.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::hash::Hasher;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, PoisonError, mpsc};
use std::time::SystemTime;

use memchr::memmem;
use serde::Deserialize;
use serde::de::IgnoredAny;

use crate::agents::tail;
use crate::sessions::{self, SessionsRoot};
use crate::status::TurnOutcome;

/// Sibling artifact directories are walked at most this deep.
const MAX_DEPTH: usize = 16;

// ---------------------------------------------------------------------------------------------
// Inventory

/// One top-level session file with everything the hub shows without parsing the transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InventoryEntry {
    pub id: String,
    pub path: PathBuf,
    /// Header `cwd`.
    pub cwd: PathBuf,
    pub title: Option<String>,
    pub first_prompt: Option<String>,
    /// Header timestamp.
    pub created: Option<SystemTime>,
    /// Mtime of the jsonl.
    pub modified: SystemTime,
    pub jsonl_bytes: u64,
    /// Sibling directory (recursive) plus `<file>.*.bak` files.
    pub artifact_bytes: u64,
    pub artifact_files: u32,
    /// `*.jsonl` transcripts in the sibling directory (`.bak` files excluded).
    pub subagents: u32,
    /// Header `parentSession` as an absolute path. omp writes either a path or the parent's
    /// session id (`--fork`); an id is resolved to the parent's file when it is in the store and
    /// dropped otherwise.
    pub parent: Option<PathBuf>,
    pub cwd_exists: bool,
}

impl InventoryEntry {
    /// Bytes freed by deleting the session: jsonl + sibling directory + `.bak` files.
    pub fn total_bytes(&self) -> u64 {
        self.jsonl_bytes + self.artifact_bytes
    }

    /// No title and no user prompt: an empty session omp left behind.
    pub fn is_stub(&self) -> bool {
        self.title.is_none() && self.first_prompt.is_none()
    }

    /// The sibling artifact directory (`<file>` without `.jsonl`).
    pub fn artifact_dir(&self) -> PathBuf {
        self.path.with_extension("")
    }
}

/// Every top-level `<root>/<dir>/*.jsonl` that has a session header, newest first. Includes the
/// prompt-less stubs the sidebar hides; never lists subagent transcripts. Files without a header
/// or that cannot be read are skipped (logged at debug level).
pub fn scan(root: &SessionsRoot) -> Vec<InventoryEntry> {
    let mut out: Vec<InventoryEntry> = Vec::new();
    let mut cwd_exists: HashMap<PathBuf, bool> = HashMap::new();
    let Ok(projects) = fs::read_dir(&root.path) else {
        log::debug!("sessions root {}: unreadable", root.path.display());
        return out;
    };
    for project in projects.flatten() {
        // Real directories only: a symlinked project dir could lead outside the store.
        if !project.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let pdir = project.path();
        let Ok(files) = fs::read_dir(&pdir) else {
            continue;
        };
        let mut sessions: Vec<(PathBuf, fs::Metadata)> = Vec::new();
        // `<file>.jsonl.<n>.bak` keyed by `<file>.jsonl`.
        let mut baks: HashMap<String, (u64, u32)> = HashMap::new();
        for file in files.flatten() {
            let name = file.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(stem) = name.strip_suffix(".bak") {
                if let Some(i) = stem.find(".jsonl.")
                    && let Ok(meta) = file.metadata()
                    && meta.is_file()
                {
                    let e = baks.entry(stem[..i + 6].to_string()).or_default();
                    e.0 += meta.len();
                    e.1 += 1;
                }
                continue;
            }
            if !name.ends_with(".jsonl") {
                continue;
            }
            // `DirEntry` metadata does not follow symlinks: a symlinked "session" is ignored.
            match file.metadata() {
                Ok(m) if m.is_file() => sessions.push((file.path(), m)),
                _ => {}
            }
        }
        for (path, meta) in sessions {
            let parsed = match sessions::parse_file(&path) {
                Ok(Some(p)) => p,
                Ok(None) => {
                    log::debug!("inventory: skip {}: no session header", path.display());
                    continue;
                }
                Err(e) => {
                    log::debug!("inventory: skip {}: {e}", path.display());
                    continue;
                }
            };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            let (mut artifact_bytes, mut artifact_files, mut subagents) = (0u64, 0u32, 0u32);
            walk(&path.with_extension(""), 0, &mut |p, len| {
                artifact_bytes += len;
                artifact_files = artifact_files.saturating_add(1);
                if p.extension().is_some_and(|e| e == "jsonl") {
                    subagents = subagents.saturating_add(1);
                }
            });
            if let Some((bytes, n)) = baks.get(&name) {
                artifact_bytes += bytes;
                artifact_files = artifact_files.saturating_add(*n);
            }
            let exists = *cwd_exists
                .entry(parsed.cwd.clone())
                .or_insert_with_key(|c| c.is_dir());
            out.push(InventoryEntry {
                id: parsed.id,
                created: parsed
                    .created
                    .as_deref()
                    .and_then(|t| tail::parse_iso(t.as_bytes())),
                cwd: parsed.cwd,
                title: parsed.title,
                first_prompt: parsed.first_prompt,
                modified,
                jsonl_bytes: meta.len(),
                artifact_bytes,
                artifact_files,
                subagents,
                parent: parsed.parent.map(PathBuf::from),
                cwd_exists: exists,
                path,
            });
        }
    }
    resolve_parents(&mut out);
    out.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    out
}

/// `parentSession` is an absolute path or a bare session id; ids become the parent's path.
fn resolve_parents(entries: &mut [InventoryEntry]) {
    let by_id: HashMap<String, PathBuf> = entries
        .iter()
        .map(|e| (e.id.clone(), e.path.clone()))
        .collect();
    for e in entries {
        if let Some(p) = e.parent.take() {
            e.parent = if p.is_absolute() {
                Some(p)
            } else {
                p.to_str().and_then(|id| by_id.get(id)).cloned()
            };
        }
    }
}

/// Metadata-only recursive walk (depth ≤ [`MAX_DEPTH`]); symlinks are reported as files of
/// their own length and never followed.
fn walk(dir: &Path, depth: usize, f: &mut impl FnMut(&Path, u64)) {
    if depth >= MAX_DEPTH {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            walk(&e.path(), depth + 1, f);
        } else if let Ok(m) = e.metadata() {
            f(&e.path(), m.len());
        }
    }
}

/// `*.jsonl` transcripts below `dir` (same walk rules as [`scan`]).
fn transcripts(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(dir, 0, &mut |p, _| {
        if p.extension().is_some_and(|e| e == "jsonl") {
            out.push(p.to_path_buf());
        }
    });
    out
}

// ---------------------------------------------------------------------------------------------
// Statistics

/// Usage numbers of one session. Main-file numbers come from the main jsonl only;
/// `subagent_cost` sums the sibling directory's transcripts.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionStats {
    pub user_messages: u32,
    pub assistant_turns: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost: f64,
    pub subagent_cost: f64,
    /// By use count, descending (ties by name).
    pub models: Vec<String>,
    pub compactions: u32,
    /// Timestamp of the last entry.
    pub last_activity: Option<SystemTime>,
    pub last_outcome: TurnOutcome,
}

impl Default for SessionStats {
    fn default() -> Self {
        SessionStats {
            user_messages: 0,
            assistant_turns: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost: 0.0,
            subagent_cost: 0.0,
            models: Vec::new(),
            compactions: 0,
            last_activity: None,
            last_outcome: TurnOutcome::Unknown,
        }
    }
}

/// Longest transcript line decoded; longer ones are skipped.
const MAX_LINE: usize = 64 * 1024 * 1024;
/// How far into a message entry the role is looked for.
const ROLE_PEEK: usize = 512;
const MAX_ERROR_CHARS: usize = 200;

static ROLE: LazyLock<memmem::Finder<'static>> =
    LazyLock::new(|| memmem::Finder::new(br#""message":{"role":""#));
static STAMP: LazyLock<memmem::Finder<'static>> =
    LazyLock::new(|| memmem::Finder::new(br#""timestamp":""#));

#[derive(Deserialize)]
struct Entry<'a> {
    #[serde(borrow)]
    message: Option<Msg<'a>>,
    #[serde(borrow)]
    model: Option<Cow<'a, str>>,
    usage: Option<Usage>,
    cost: Option<Cost>,
}

#[derive(Deserialize)]
struct Msg<'a> {
    #[serde(borrow)]
    model: Option<Cow<'a, str>>,
    usage: Option<Usage>,
    cost: Option<Cost>,
    #[serde(rename = "stopReason", borrow)]
    stop_reason: Option<Cow<'a, str>>,
    #[serde(rename = "errorMessage", borrow)]
    error_message: Option<Cow<'a, str>>,
    // Never decoded: only skipped.
    #[allow(dead_code)]
    content: Option<IgnoredAny>,
}

#[derive(Deserialize)]
struct Usage {
    input: Option<f64>,
    output: Option<f64>,
    #[serde(rename = "cacheRead")]
    cache_read: Option<f64>,
    #[serde(rename = "cacheWrite")]
    cache_write: Option<f64>,
    cost: Option<Cost>,
}

#[derive(Deserialize)]
struct Cost {
    total: Option<f64>,
}

/// Totals of one jsonl file.
#[derive(Clone, Default)]
struct FileAgg {
    user: u32,
    assistant: u32,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
    models: HashMap<String, u32>,
    compactions: u32,
    last_ts: Option<SystemTime>,
    outcome: Option<TurnOutcome>,
}

fn tokens(v: Option<f64>) -> u64 {
    v.filter(|v| v.is_finite() && *v > 0.0)
        .map_or(0, |v| v as u64)
}

impl FileAgg {
    fn line(&mut self, line: &[u8]) {
        let Some(rest) = line.strip_prefix(br#"{"type":""#) else {
            return;
        };
        let head = &line[..line.len().min(ROLE_PEEK)];
        if let Some(i) = STAMP.find(head)
            && let Some(t) = tail::parse_iso(&head[i + STAMP.needle().len()..])
        {
            self.last_ts = Some(t);
        }
        if rest.starts_with(b"message\"") {
            let Some(i) = ROLE.find(head) else { return };
            let role = &head[i + ROLE.needle().len()..];
            if role.starts_with(b"user\"") {
                self.user += 1;
            } else if role.starts_with(b"assistant\"") {
                self.assistant += 1;
                self.decode(line, true);
            }
        } else if rest.starts_with(b"model_usage\"") {
            self.decode(line, false);
        } else if rest.starts_with(b"compaction\"") {
            self.compactions += 1;
        }
    }

    fn decode(&mut self, line: &[u8], assistant: bool) {
        // omp writes `content` first and `api, provider, model, usage, stopReason, …` after it:
        // for a long assistant line only that tail is decoded.
        let mut scratch = Vec::new();
        let tail = if assistant && line.len() > TAIL_ONLY_OVER {
            tail_message(line, &mut scratch)
        } else {
            None
        };
        let msg = match tail {
            Some(m) => m,
            None => {
                let Ok(entry) = serde_json::from_slice::<Entry>(line) else {
                    return;
                };
                match entry.message {
                    Some(m) => m,
                    None => Msg {
                        model: entry.model,
                        usage: entry.usage,
                        cost: entry.cost,
                        stop_reason: None,
                        error_message: None,
                        content: None,
                    },
                }
            }
        };
        let (model, usage, cost, stop, error) = (
            msg.model,
            msg.usage,
            msg.cost,
            msg.stop_reason,
            msg.error_message,
        );
        if let Some(u) = &usage {
            self.input += tokens(u.input);
            self.output += tokens(u.output);
            self.cache_read += tokens(u.cache_read);
            self.cache_write += tokens(u.cache_write);
        }
        let total = usage
            .as_ref()
            .and_then(|u| u.cost.as_ref())
            .or(cost.as_ref())
            .and_then(|c| c.total)
            .filter(|c| c.is_finite());
        self.cost += total.unwrap_or(0.0);
        if let Some(m) = model.filter(|m| !m.is_empty()) {
            match self.models.get_mut(m.as_ref()) {
                Some(n) => *n += 1,
                None => {
                    self.models.insert(m.into_owned(), 1);
                }
            }
        }
        if assistant {
            self.outcome = Some(match stop.as_deref() {
                Some("error") => TurnOutcome::Error(
                    error
                        .as_deref()
                        .map(str::trim)
                        .filter(|m| !m.is_empty())
                        .map(|m| m.chars().take(MAX_ERROR_CHARS).collect()),
                ),
                Some("aborted") => TurnOutcome::Aborted,
                _ => TurnOutcome::Completed,
            });
        }
    }
}

/// Assistant lines longer than this are decoded from their tail only.
const TAIL_ONLY_OVER: usize = 16 * 1024;
/// How far from the end of a line the `api` key is searched for.
const TAIL_WINDOW: usize = 4096;

/// Decodes the fields after `content` of a long assistant entry (`…,"api":"…","usage":{…}}}`).
/// `None` when the tail does not look like one; the caller then decodes the whole line.
fn tail_message<'a>(line: &[u8], scratch: &'a mut Vec<u8>) -> Option<Msg<'a>> {
    let from = line.len() - TAIL_WINDOW;
    let at = from + memmem::rfind(&line[from..], br#","api":""#)?;
    scratch.push(b'{');
    scratch.extend_from_slice(&line[at + 1..]);
    // Trailing bytes (the entry's closing brace, the newline) are ignored.
    let msg = Msg::deserialize(&mut serde_json::Deserializer::from_slice(scratch)).ok()?;
    (msg.usage.is_some() || msg.model.is_some()).then_some(msg)
}

/// Calls `f` with every complete line (including its `\n`) of `r` and returns the bytes they
/// span plus `prefix` advanced over exactly those bytes. A last line without `\n` is not
/// delivered (nor counted or hashed); lines longer than [`MAX_LINE`] are counted and hashed but
/// not delivered. Lines that sit inside one read buffer are passed without copying.
fn for_each_line(
    r: &mut impl BufRead,
    carry: &mut Vec<u8>,
    prefix: Prefix,
    mut f: impl FnMut(&[u8]),
) -> io::Result<(u64, Prefix)> {
    carry.clear();
    let (mut consumed, mut pending, mut skipped) = (0u64, 0u64, false);
    let mut live = prefix.clone();
    let mut committed = prefix;
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            return Ok((consumed, committed));
        }
        let (take, done) = match memchr::memchr(b'\n', chunk) {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        live.feed(&chunk[..take]);
        if done && carry.is_empty() && !skipped {
            f(&chunk[..take]);
        } else if !skipped {
            if carry.len() + take > MAX_LINE {
                skipped = true;
                carry.clear();
            } else {
                carry.extend_from_slice(&chunk[..take]);
                if done {
                    f(carry);
                    carry.clear();
                }
            }
        }
        r.consume(take);
        pending += take as u64;
        if done {
            consumed += pending;
            (pending, skipped) = (0, false);
            carry.clear();
            committed = live.clone();
        }
    }
}

/// What is cached for one jsonl file.
struct CachedFile {
    len: u64,
    mtime: SystemTime,
    /// End of the last complete line parsed.
    offset: u64,
    /// Hash of the parsed bytes `[prefix.skip, offset)`.
    prefix: Prefix,
    agg: FileAgg,
}

/// Streaming hash of a file's bytes from `skip` on (the bytes before it are left out). `skip` is
/// the length of the first line: omp rewrites the padded title there in place, which must not
/// invalidate the cached statistics. Chunking does not change the digest (SipHash buffers).
#[derive(Clone)]
struct Prefix {
    h: std::hash::DefaultHasher,
    /// File offset of the next byte fed.
    pos: u64,
    skip: u64,
}

impl Prefix {
    fn new(skip: u64) -> Prefix {
        Prefix {
            h: std::hash::DefaultHasher::new(),
            pos: 0,
            skip,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        let start = self.pos;
        self.pos += bytes.len() as u64;
        if self.pos > self.skip {
            self.h
                .write(&bytes[self.skip.saturating_sub(start) as usize..]);
        }
    }

    fn digest(&self) -> u64 {
        self.h.finish()
    }
}

/// Length of the first line including its `\n`; 0 when there is none within 64 KiB.
fn first_line_len(file: &mut File) -> io::Result<u64> {
    file.seek(SeekFrom::Start(0))?;
    let mut buf = Vec::new();
    file.by_ref().take(64 * 1024).read_to_end(&mut buf)?;
    Ok(memchr::memchr(b'\n', &buf).map_or(0, |i| i as u64 + 1))
}

/// Whether the parsed prefix of `c` is byte-for-byte what is on disk (first line aside): the
/// first line has the same length and the bytes `[skip, offset)` hash alike. Reads the prefix
/// sequentially once.
fn prefix_unchanged(file: &mut File, c: &CachedFile) -> io::Result<bool> {
    let skip = c.prefix.skip;
    if first_line_len(file)? != skip {
        return Ok(false);
    }
    let from = skip.min(c.offset);
    file.seek(SeekFrom::Start(from))?;
    let mut check = Prefix::new(skip);
    check.pos = from;
    let mut buf = vec![0u8; 256 * 1024];
    let mut left = c.offset - from;
    while left > 0 {
        let n = buf.len().min(left as usize);
        file.read_exact(&mut buf[..n])?;
        check.feed(&buf[..n]);
        left -= n as u64;
    }
    Ok(check.digest() == c.prefix.digest())
}

#[derive(Default)]
struct Shared {
    files: Mutex<HashMap<PathBuf, CachedFile>>,
    parsed_bytes: AtomicU64,
}

impl Shared {
    /// Brings the cached totals of `path` up to date and returns them.
    fn refresh(&self, path: &Path) -> io::Result<FileAgg> {
        let meta = fs::metadata(path)?;
        let (len, mtime) = (meta.len(), meta.modified()?);
        let cached = self
            .files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(path);
        let mut file = File::open(path)?;
        let mut state = match cached {
            Some(c) if c.len == len && c.mtime == mtime => {
                let agg = c.agg.clone();
                self.files
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(path.to_path_buf(), c);
                return Ok(agg);
            }
            // Appended: continue after the last complete line when the whole parsed prefix is
            // unchanged (omp only rewrites the title line in place and appends otherwise).
            Some(c)
                if len >= c.len
                    && c.offset <= len
                    && prefix_unchanged(&mut file, &c).unwrap_or(false) =>
            {
                c
            }
            _ => {
                let skip = first_line_len(&mut file)?;
                CachedFile {
                    len,
                    mtime,
                    offset: 0,
                    prefix: Prefix::new(skip),
                    agg: FileAgg::default(),
                }
            }
        };
        file.seek(SeekFrom::Start(state.offset))?;
        let mut reader = BufReader::with_capacity(1024 * 1024, file);
        let mut carry = Vec::new();
        let (used, prefix) =
            for_each_line(&mut reader, &mut carry, state.prefix.clone(), |line| {
                state.agg.line(line)
            })?;
        self.parsed_bytes.fetch_add(used, Ordering::Relaxed);
        state.offset += used;
        state.prefix = prefix;
        state.len = len;
        state.mtime = mtime;
        let agg = state.agg.clone();
        self.files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_path_buf(), state);
        Ok(agg)
    }

    fn stats(&self, entry: &InventoryEntry) -> io::Result<SessionStats> {
        let main = self.refresh(&entry.path)?;
        let mut subagent_cost = 0.0;
        for t in transcripts(&entry.artifact_dir()) {
            match self.refresh(&t) {
                Ok(a) => subagent_cost += a.cost,
                Err(e) => log::debug!("inventory: transcript {}: {e}", t.display()),
            }
        }
        let mut models: Vec<(&String, &u32)> = main.models.iter().collect();
        models.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        Ok(SessionStats {
            user_messages: main.user,
            assistant_turns: main.assistant,
            input_tokens: main.input,
            output_tokens: main.output,
            cache_read_tokens: main.cache_read,
            cache_write_tokens: main.cache_write,
            cost: main.cost,
            subagent_cost,
            models: models.into_iter().map(|(m, _)| m.clone()).collect(),
            compactions: main.compactions,
            last_activity: main.last_ts,
            last_outcome: main.outcome.unwrap_or(TurnOutcome::Unknown),
        })
    }
}

/// In-memory statistics cache for the hub; lives for the process. Blocking: run it on a worker.
#[derive(Default)]
pub struct StatsCache {
    shared: Shared,
    stats: HashMap<PathBuf, SessionStats>,
}

impl StatsCache {
    pub fn new() -> StatsCache {
        StatsCache::default()
    }

    /// Last computed statistics of the session file at `path`.
    pub fn get(&self, path: &Path) -> Option<&SessionStats> {
        self.stats.get(path)
    }

    /// Statistics of `entry`: its main file and every transcript in the sibling directory are
    /// re-read only where (len, mtime) changed, and an appended file only from the cached offset.
    /// A truncated last line is left for the next call.
    pub fn compute(&mut self, entry: &InventoryEntry) -> io::Result<SessionStats> {
        let s = self.shared.stats(entry)?;
        self.stats.insert(entry.path.clone(), s.clone());
        Ok(s)
    }

    /// [`StatsCache::compute`] for many entries on up to `threads` threads (clamped to 1..=16).
    /// Entries are started in slice order; `on_done` runs on the calling thread as each one
    /// finishes (so results stream in progressively). Stops starting new entries once `cancel`
    /// is set; entries that were not started are not reported.
    pub fn compute_many(
        &mut self,
        entries: &[InventoryEntry],
        threads: usize,
        cancel: &AtomicBool,
        mut on_done: impl FnMut(&InventoryEntry, io::Result<&SessionStats>),
    ) {
        let threads = threads.clamp(1, 16).min(entries.len());
        if threads == 0 {
            return;
        }
        let next = AtomicUsize::new(0);
        let shared = &self.shared;
        let stats = &mut self.stats;
        let (tx, rx) = mpsc::channel::<(usize, io::Result<SessionStats>)>();
        std::thread::scope(|s| {
            for _ in 0..threads {
                let tx = tx.clone();
                let next = &next;
                s.spawn(move || {
                    while !cancel.load(Ordering::Relaxed) {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(entry) = entries.get(i) else { break };
                        if tx.send((i, shared.stats(entry))).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(tx);
            for (i, result) in rx {
                let entry = &entries[i];
                match result {
                    Ok(s) => {
                        let s = stats.entry(entry.path.clone()).insert_entry(s);
                        on_done(entry, Ok(s.get()));
                    }
                    Err(e) => on_done(entry, Err(e)),
                }
            }
        });
    }

    /// Drops everything cached for sessions that are not in `entries` (deleted or moved).
    pub fn prune(&mut self, entries: &[InventoryEntry]) {
        let mains: HashSet<&Path> = entries.iter().map(|e| e.path.as_path()).collect();
        let dirs: HashSet<PathBuf> = entries.iter().map(InventoryEntry::artifact_dir).collect();
        self.stats.retain(|p, _| mains.contains(p.as_path()));
        self.shared
            .files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|p, _| {
                mains.contains(p.as_path()) || p.ancestors().skip(1).any(|a| dirs.contains(a))
            });
    }

    /// Bytes of jsonl content parsed so far (cache misses and appended tails). For tests.
    #[doc(hidden)]
    pub fn parsed_bytes(&self) -> u64 {
        self.shared.parsed_bytes.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agg(lines: &[&str]) -> FileAgg {
        let mut a = FileAgg::default();
        for l in lines {
            a.line(l.as_bytes());
        }
        a
    }

    #[test]
    fn cost_prefers_usage_cost_and_falls_back_to_message_cost() {
        let a = agg(&[
            r#"{"type":"message","id":"a","timestamp":"2026-01-01T00:00:00.000Z","message":{"role":"assistant","content":"x","model":"m1","usage":{"input":10,"output":5,"cacheRead":2,"cacheWrite":1,"cost":{"total":0.5}},"stopReason":"stop"}}"#,
            r#"{"type":"message","id":"b","timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"assistant","content":"x","model":"m1","usage":{"input":1,"output":1},"cost":{"total":0.25},"stopReason":"error","errorMessage":" boom "}}"#,
            r#"{"type":"model_usage","id":"c","timestamp":"2026-01-01T00:00:02.000Z","model":"m2","usage":{"input":3,"output":0,"cost":{"total":0.125}}}"#,
        ]);
        assert_eq!((a.assistant, a.input, a.output), (2, 14, 6));
        assert_eq!((a.cache_read, a.cache_write), (2, 1));
        assert!((a.cost - 0.875).abs() < 1e-12);
        assert_eq!(a.models.get("m1"), Some(&2));
        assert_eq!(a.models.get("m2"), Some(&1));
        assert_eq!(a.outcome, Some(TurnOutcome::Error(Some("boom".into()))));
        assert!(a.last_ts.is_some());
    }

    #[test]
    fn escaped_role_text_in_a_prompt_is_not_a_message() {
        let a = agg(&[
            r#"{"type":"message","id":"u","timestamp":"2026-01-01T00:00:00.000Z","message":{"role":"user","content":"say \"message\":{\"role\":\"assistant\"","timestamp":1}}"#,
            r#"{"type":"message","id":"t","timestamp":"2026-01-01T00:00:00.000Z","message":{"role":"toolResult","content":"x"}}"#,
        ]);
        assert_eq!((a.user, a.assistant), (1, 0));
    }

    #[test]
    fn long_assistant_lines_decode_the_same_from_their_tail() {
        // 40 KB of content whose tool-call arguments carry their own "api" key.
        let content = format!(
            r#"[{{"type":"toolCall","arguments":{{"api":"decoy","x":"{}"}}}}]"#,
            "y".repeat(40_000)
        );
        let line = format!(
            r#"{{"type":"message","id":"a","timestamp":"2026-01-01T00:00:00.000Z","message":{{"role":"assistant","content":{content},"api":"anthropic","provider":"p","model":"m-long","usage":{{"input":11,"output":7,"cacheRead":3,"cacheWrite":1,"cost":{{"total":0.75}}}},"stopReason":"error","errorMessage":"nope","timestamp":2}}}}"#
        );
        assert!(line.len() > TAIL_ONLY_OVER);
        let mut scratch = Vec::new();
        assert!(tail_message(line.as_bytes(), &mut scratch).is_some());
        let tailed = agg(&[&line]);
        assert_eq!(
            (
                tailed.input,
                tailed.output,
                tailed.cache_read,
                tailed.cache_write
            ),
            (11, 7, 3, 1)
        );
        assert!((tailed.cost - 0.75).abs() < 1e-12);
        assert_eq!(tailed.models.get("m-long"), Some(&1));
        assert_eq!(
            tailed.outcome,
            Some(TurnOutcome::Error(Some("nope".into())))
        );

        // An error text that pushes `api` out of the window falls back to the full decode.
        let long_err = line.replace("\"nope\"", &format!("\"{}\"", "e".repeat(6000)));
        let mut scratch = Vec::new();
        assert!(tail_message(long_err.as_bytes(), &mut scratch).is_none());
        let full = agg(&[&long_err]);
        assert_eq!((full.input, full.output), (11, 7));
        assert_eq!(full.models.get("m-long"), Some(&1));
    }

    #[test]
    fn partial_last_line_is_not_consumed_and_split_lines_are_joined() {
        let data = b"{\"type\":\"compaction\"}\n{\"type\":\"compac";
        let mut seen = Vec::new();
        let (used, _) = for_each_line(&mut &data[..], &mut Vec::new(), Prefix::new(0), |l| {
            seen.push(l.to_vec())
        })
        .unwrap();
        assert_eq!(used, 22);
        assert_eq!(seen, [b"{\"type\":\"compaction\"}\n".to_vec()]);

        // A line spanning several reads (tiny buffer) arrives whole.
        let data = b"{\"type\":\"model_usage\"}\n{\"type\":\"compaction\"}\n";
        let mut r = BufReader::with_capacity(5, &data[..]);
        let mut seen = 0;
        let (used, tiny) = for_each_line(&mut r, &mut Vec::new(), Prefix::new(0), |l| {
            assert!(l.starts_with(b"{\"type\":\"") && l.ends_with(b"}\n"));
            seen += 1;
        })
        .unwrap();
        assert_eq!((seen, used), (2, data.len() as u64));
        // The digest does not depend on how the bytes were chunked.
        let (_, whole) =
            for_each_line(&mut &data[..], &mut Vec::new(), Prefix::new(0), |_| {}).unwrap();
        assert_eq!(tiny.digest(), whole.digest());
        // The first line is left out of the hash, a partial last line is not in it.
        let (_, skipped) =
            for_each_line(&mut &data[..], &mut Vec::new(), Prefix::new(23), |_| {}).unwrap();
        let mut tail = Prefix::new(0);
        tail.feed(b"{\"type\":\"compaction\"}\n");
        assert_eq!(skipped.digest(), tail.digest());
    }
}
