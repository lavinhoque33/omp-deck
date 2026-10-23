//! Scanning of the omp session store (`<root>/<project-dir>/<ts>_<id>.jsonl`).
//!
//! Only metadata plus bounded reads: the title line and header sit at the top of the file,
//! and the first user prompt is searched for (untitled sessions only) within [`MAX_READ`].

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use serde::Deserialize;

/// Title and session header must start within this many bytes of the file start.
const HEADER_LIMIT: u64 = 16 * 1024;
/// Total bytes read per file while looking for the first prompt.
const MAX_READ: u64 = 2 * 1024 * 1024;
/// Lines longer than this are skipped without being buffered.
const MAX_LINE: usize = 256 * 1024;
const PROMPT_CHARS: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionsRoot {
    pub path: PathBuf,
}

impl SessionsRoot {
    /// `$PI_CODING_AGENT_DIR/sessions` if set and non-empty, else `$HOME/$PI_CONFIG_DIR/agent/sessions`
    /// (`PI_CONFIG_DIR` default `.omp`).
    pub fn resolve() -> SessionsRoot {
        let non_empty = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
        let path = if let Some(dir) = non_empty("PI_CODING_AGENT_DIR") {
            PathBuf::from(dir).join("sessions")
        } else {
            let home = crate::platform::home_or_root();
            let cfg = non_empty("PI_CONFIG_DIR").unwrap_or_else(|| ".omp".into());
            home.join(cfg).join("agent").join("sessions")
        };
        SessionsRoot {
            path: absolutize(path),
        }
    }

    pub fn at(path: impl Into<PathBuf>) -> SessionsRoot {
        SessionsRoot {
            path: absolutize(path.into()),
        }
    }

    pub fn exists(&self) -> bool {
        self.path.is_dir()
    }

    /// The omp agent directory: parent of the sessions root (holds `terminal-sessions/`).
    pub fn agent_dir(&self) -> PathBuf {
        self.path
            .parent()
            .map_or_else(|| self.path.clone(), Path::to_path_buf)
    }
}

/// Makes `path` absolute (against the current directory) and normalizes `.`/`..` lexically.
/// No filesystem access beyond `current_dir`; if that fails the path is kept as given.
fn absolutize(path: PathBuf) -> PathBuf {
    let joined = if path.is_relative() {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(&path),
            Err(_) => return path,
        }
    } else {
        path
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            // `pop` is a no-op at the root, matching how the OS resolves `/..`.
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Counting<R> {
        inner: R,
        read: std::rc::Rc<std::cell::Cell<u64>>,
    }

    impl<R: io::Read> io::Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read.set(self.read.get() + n as u64);
            Ok(n)
        }
    }

    #[test]
    fn newline_less_huge_line_is_not_read_past_budget() {
        let mut data = br#"{"type":"session","id":"x","cwd":"/p"}"#.to_vec();
        data.push(b'\n');
        data.extend(std::iter::repeat_n(b'a', 5 * 1024 * 1024));
        let read = std::rc::Rc::new(std::cell::Cell::new(0));
        let rd = Counting {
            inner: io::Cursor::new(data),
            read: read.clone(),
        };
        let parsed = parse_reader(rd).unwrap().unwrap();
        assert_eq!(parsed.first_prompt, None);
        assert!(read.get() <= MAX_READ, "read {} bytes", read.get());
    }

    #[test]
    fn absolutize_normalizes_lexically() {
        #[cfg(unix)]
        {
            assert_eq!(absolutize("/a/./b/../c".into()), Path::new("/a/c"));
            assert_eq!(absolutize("/../a".into()), Path::new("/a"));
        }
        #[cfg(windows)]
        {
            assert_eq!(absolutize(r"C:\a\.\b\..\c".into()), Path::new(r"C:\a\c"));
            assert_eq!(absolutize(r"C:\..\a".into()), Path::new(r"C:\a"));
        }
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(absolutize("x/../y".into()), cwd.join("y"));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEntry {
    pub id: String,
    pub path: PathBuf,
    pub cwd: PathBuf,
    pub title: Option<String>,
    pub first_prompt: Option<String>,
    pub modified: SystemTime,
}

impl SessionEntry {
    /// title, else first_prompt, else "(untitled)".
    pub fn label(&self) -> &str {
        self.title
            .as_deref()
            .or(self.first_prompt.as_deref())
            .unwrap_or("(untitled)")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectGroup {
    pub cwd: PathBuf,
    pub display: String,
    pub sessions: Vec<SessionEntry>,
}

/// `/home/u/x` → `~/x` (`~\x` on Windows); `$HOME` itself → `~`; anything else unchanged.
pub fn abbreviate_home(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() {
            "~".to_string()
        } else {
            // Native separators throughout: `~/x/y`, `~\x\y` on Windows.
            let tail: Vec<_> = rest.iter().map(|c| c.to_string_lossy()).collect();
            format!(
                "~{sep}{}",
                tail.join(std::path::MAIN_SEPARATOR_STR),
                sep = std::path::MAIN_SEPARATOR
            )
        };
    }
    path.display().to_string()
}

/// The newest top-level session file under `root` (any project dir) with mtime >= `since` whose
/// header has `"parentSession": <parent_id>`. Files older than `since` are skipped by metadata
/// alone; matching files are read for at most [`HEADER_LIMIT`] bytes. Sibling artifact
/// directories are never entered.
pub fn find_fork(root: &SessionsRoot, parent_id: &str, since: SystemTime) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for project in fs::read_dir(&root.path).ok()?.flatten() {
        let pdir = project.path();
        let Ok(files) = fs::read_dir(&pdir) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            let Ok(mtime) = meta.modified() else {
                continue;
            };
            if !meta.is_file() || mtime < since {
                continue;
            }
            if best.as_ref().is_some_and(|(t, _)| *t >= mtime) {
                continue;
            }
            match header_parent(&path) {
                Ok(Some(p)) if p == parent_id => best = Some((mtime, path)),
                Ok(_) => {}
                Err(e) => log::debug!("find_fork: skip {}: {e}", path.display()),
            }
        }
    }
    best.map(|(_, p)| p)
}

/// `parentSession` of the session header found within the first [`HEADER_LIMIT`] bytes.
fn header_parent(path: &Path) -> io::Result<Option<String>> {
    let reader =
        BufReader::with_capacity(16 * 1024, io::Read::take(File::open(path)?, HEADER_LIMIT));
    let mut rd = LineReader { r: reader, pos: 0 };
    let mut buf = Vec::new();
    while rd.next_line(&mut buf)?.is_some() {
        let Ok(line) = serde_json::from_slice::<RawLine>(&buf) else {
            continue;
        };
        if line.kind.as_deref() == Some("session") {
            return Ok(line.parent_session);
        }
    }
    Ok(None)
}
/// One-shot scan; see [`Scanner`] for the incremental variant.
pub fn scan(root: &SessionsRoot) -> Vec<ProjectGroup> {
    Scanner::new(root.clone()).scan()
}

#[derive(Clone, Debug)]
struct Parsed {
    id: String,
    cwd: PathBuf,
    title: Option<String>,
    first_prompt: Option<String>,
}

struct CacheEntry {
    len: u64,
    mtime: SystemTime,
    /// `None` = file is not a usable session (garbage); cached so it is not re-read.
    parsed: Option<Parsed>,
}

/// Incremental scanner caching parsed headers by (path, len, mtime).
pub struct Scanner {
    root: SessionsRoot,
    cache: HashMap<PathBuf, CacheEntry>,
    files_parsed: u64,
}

impl Scanner {
    pub fn new(root: SessionsRoot) -> Scanner {
        Scanner {
            root,
            cache: HashMap::new(),
            files_parsed: 0,
        }
    }

    pub fn root(&self) -> &SessionsRoot {
        &self.root
    }

    /// Number of file contents opened so far (cache misses). For tests/diagnostics.
    #[doc(hidden)]
    pub fn files_parsed(&self) -> u64 {
        self.files_parsed
    }

    pub fn scan(&mut self) -> Vec<ProjectGroup> {
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut by_cwd: HashMap<PathBuf, Vec<SessionEntry>> = HashMap::new();

        let projects = match fs::read_dir(&self.root.path) {
            Ok(rd) => rd,
            Err(e) => {
                log::debug!("sessions root {}: {e}", self.root.path.display());
                self.cache.clear();
                return Vec::new();
            }
        };
        for project in projects.flatten() {
            let pdir = project.path();
            if !pdir.is_dir() {
                continue;
            }
            let files = match fs::read_dir(&pdir) {
                Ok(rd) => rd,
                Err(e) => {
                    log::debug!("skip project dir {}: {e}", pdir.display());
                    continue;
                }
            };
            for file in files.flatten() {
                let path = file.path();
                if path.extension().is_none_or(|e| e != "jsonl") {
                    continue;
                }
                let Ok(meta) = fs::metadata(&path) else {
                    continue;
                };
                if !meta.is_file() {
                    continue;
                }
                let Ok(mtime) = meta.modified() else {
                    continue;
                };
                let len = meta.len();
                let fresh = self
                    .cache
                    .get(&path)
                    .is_some_and(|c| c.len == len && c.mtime == mtime);
                if !fresh {
                    self.files_parsed += 1;
                    let parsed = match parse_file(&path) {
                        Ok(p) => {
                            if p.is_none() {
                                log::debug!("skip {}: no session header", path.display());
                            }
                            p
                        }
                        Err(e) => {
                            log::debug!("skip {}: {e}", path.display());
                            None
                        }
                    };
                    self.cache
                        .insert(path.clone(), CacheEntry { len, mtime, parsed });
                }
                if let Some(Some(p)) = self.cache.get(&path).map(|c| c.parsed.as_ref())
                    && (p.title.is_some() || p.first_prompt.is_some())
                {
                    by_cwd.entry(p.cwd.clone()).or_default().push(SessionEntry {
                        id: p.id.clone(),
                        path: path.clone(),
                        cwd: p.cwd.clone(),
                        title: p.title.clone(),
                        first_prompt: p.first_prompt.clone(),
                        modified: mtime,
                    });
                }
                seen.insert(path);
            }
        }
        self.cache.retain(|k, _| seen.contains(k));

        let mut groups: Vec<ProjectGroup> = by_cwd
            .into_iter()
            .map(|(cwd, mut sessions)| {
                sessions.sort_by(|a, b| {
                    b.modified
                        .cmp(&a.modified)
                        .then_with(|| a.path.cmp(&b.path))
                });
                ProjectGroup {
                    display: abbreviate_home(&cwd),
                    cwd,
                    sessions,
                }
            })
            .collect();
        // Sessions are non-empty by construction.
        groups.sort_by(|a, b| {
            b.sessions[0]
                .modified
                .cmp(&a.sessions[0].modified)
                .then_with(|| a.cwd.cmp(&b.cwd))
        });
        groups
    }
}

// ---------------------------------------------------------------------------------------------
// Bounded parsing

struct LineInfo {
    /// Byte offset of the line start.
    start: u64,
    /// The line exceeded `MAX_LINE`; the buffer holds only its prefix and must be ignored.
    overflow: bool,
}

/// Newline-delimited reader that never buffers more than `MAX_LINE` bytes of a line.
struct LineReader<R: BufRead> {
    r: R,
    pos: u64,
}

impl<R: BufRead> LineReader<R> {
    fn next_line(&mut self, buf: &mut Vec<u8>) -> io::Result<Option<LineInfo>> {
        buf.clear();
        let start = self.pos;
        let mut overflow = false;
        let mut any = false;
        loop {
            let chunk = self.r.fill_buf()?;
            if chunk.is_empty() {
                break;
            }
            any = true;
            let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
                Some(i) => (i + 1, true),
                None => (chunk.len(), false),
            };
            let room = MAX_LINE.saturating_sub(buf.len());
            if take > room {
                overflow = true;
            }
            buf.extend_from_slice(&chunk[..take.min(room)]);
            self.r.consume(take);
            self.pos += take as u64;
            if done {
                break;
            }
        }
        Ok(any.then_some(LineInfo { start, overflow }))
    }
}

#[derive(Deserialize)]
struct RawLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "parentSession")]
    parent_session: Option<String>,
    title: Option<String>,
    message: Option<RawMessage>,
}

#[derive(Deserialize)]
struct RawMessage {
    role: Option<String>,
    content: Option<RawContent>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawContent {
    Text(String),
    Parts(Vec<RawPart>),
}

#[derive(Deserialize)]
struct RawPart {
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
}

fn prompt_text(content: RawContent) -> Option<String> {
    let joined = match content {
        RawContent::Text(t) => t,
        RawContent::Parts(parts) => parts
            .into_iter()
            .filter(|p| p.kind.as_deref().is_none_or(|k| k == "text"))
            .filter_map(|p| p.text)
            .collect::<Vec<_>>()
            .join(" "),
    };
    let collapsed = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    Some(collapsed.chars().take(PROMPT_CHARS).collect())
}

/// `cwd` of the session header (bounded read).
pub(crate) fn header_cwd(path: &Path) -> Option<PathBuf> {
    parse_file(path).ok().flatten().map(|p| p.cwd)
}

fn parse_file(path: &Path) -> io::Result<Option<Parsed>> {
    parse_reader(File::open(path)?)
}

/// Reads at most `MAX_READ` bytes from `reader` in total, whatever the line structure.
fn parse_reader<R: io::Read>(reader: R) -> io::Result<Option<Parsed>> {
    let reader = BufReader::with_capacity(64 * 1024, io::Read::take(reader, MAX_READ));
    let mut rd = LineReader { r: reader, pos: 0 };
    let mut buf = Vec::new();
    let mut header: Option<(String, PathBuf)> = None;
    let mut title: Option<String> = None;
    let mut prompt: Option<String> = None;

    while let Some(info) = rd.next_line(&mut buf)? {
        if info.start >= MAX_READ || (header.is_none() && info.start >= HEADER_LIMIT) {
            break;
        }
        if info.overflow {
            continue;
        }
        // A truncated last line (live append) or garbage simply fails to parse.
        let Ok(line) = serde_json::from_slice::<RawLine>(&buf) else {
            continue;
        };
        match line.kind.as_deref() {
            Some("title") if info.start < HEADER_LIMIT && title.is_none() => {
                title = line
                    .title
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty());
            }
            Some("session") if header.is_none() => {
                if let (Some(id), Some(cwd)) = (line.id, line.cwd) {
                    header = Some((id, PathBuf::from(cwd)));
                }
            }
            Some("message") if header.is_some() && prompt.is_none() => {
                if let Some(msg) = line.message
                    && msg.role.as_deref() == Some("user")
                    && let Some(content) = msg.content
                {
                    prompt = prompt_text(content);
                }
            }
            _ => {}
        }
        if header.is_some() && (title.is_some() || prompt.is_some()) {
            break;
        }
    }
    Ok(header.map(|(id, cwd)| Parsed {
        id,
        cwd,
        title,
        first_prompt: prompt,
    }))
}

// ---------------------------------------------------------------------------------------------
// Background watcher

enum Msg {
    Refresh,
    Stop,
}

struct Shared {
    snapshot: Mutex<Option<Arc<Vec<ProjectGroup>>>>,
    generation: AtomicU64,
    root_exists: AtomicBool,
}

pub struct SessionWatcher {
    shared: Arc<Shared>,
    tx: mpsc::Sender<Msg>,
    handle: Option<JoinHandle<()>>,
}

impl SessionWatcher {
    pub fn spawn(
        root: SessionsRoot,
        interval: Duration,
        on_update: impl Fn() + Send + 'static,
    ) -> SessionWatcher {
        let shared = Arc::new(Shared {
            snapshot: Mutex::new(None),
            generation: AtomicU64::new(0),
            root_exists: AtomicBool::new(false),
        });
        let (tx, rx) = mpsc::channel();
        let worker = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("deck-sessions".into())
            .spawn(move || {
                let root_path = root.path.clone();
                let mut scanner = Scanner::new(root);
                let mut first = true;
                loop {
                    let groups = scanner.scan();
                    worker
                        .root_exists
                        .store(root_path.is_dir(), Ordering::Release);
                    {
                        let mut slot = worker
                            .snapshot
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        let changed = first || slot.as_deref() != Some(&groups);
                        first = false;
                        if changed {
                            *slot = Some(Arc::new(groups));
                            worker.generation.fetch_add(1, Ordering::Release);
                            drop(slot);
                            on_update();
                        }
                    }
                    match rx.recv_timeout(interval) {
                        Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                        Ok(Msg::Refresh) | Err(RecvTimeoutError::Timeout) => {}
                    }
                    // Coalesce queued refreshes; honor a queued stop.
                    while let Ok(m) = rx.try_recv() {
                        if matches!(m, Msg::Stop) {
                            return;
                        }
                    }
                }
            });
        let handle = match handle {
            Ok(h) => Some(h),
            Err(e) => {
                log::error!("cannot spawn session watcher thread: {e}");
                None
            }
        };
        SessionWatcher { shared, tx, handle }
    }

    /// `None` until the first scan finishes.
    pub fn latest(&self) -> Option<Arc<Vec<ProjectGroup>>> {
        self.shared
            .snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::Acquire)
    }

    pub fn refresh_now(&self) {
        let _ = self.tx.send(Msg::Refresh);
    }

    /// Whether the sessions root existed at the scanner thread's last pass. `false` until the
    /// first scan has finished.
    pub fn root_exists(&self) -> bool {
        self.shared.root_exists.load(Ordering::Acquire)
    }
}

impl Drop for SessionWatcher {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
