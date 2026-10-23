//! Live subagent control: the in-process omp extension (`assets/ompdeck-ctl.ts`) serves NDJSON on a
//! unix socket (Unix) or a named pipe (Windows); [`CtlClient`] connects to it on its own thread.
//! A tab's endpoint is a [`ctl_dir`]: a private directory holding `ctl.sock` on Unix, the pipe
//! name `\\.\pipe\ompdeck-ctl.<user>.<namespace>.<leaf>` on Windows. Protocol facts:
//! `docs/notes/omp-control-ext.md`.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::Deserialize;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as os;
#[cfg(windows)]
use windows as os;

#[cfg(unix)]
pub use unix::{ctl_dir, prepare_ctl_dir, proc_start_time, prune_ctl_dirs};
#[cfg(windows)]
pub use windows::{ctl_dir, prepare_ctl_dir, prune_ctl_dirs};

/// The extension source: loaded with `omp -e <file>`, inert unless `OMPDECK_CTL_DIR` (Unix) or
/// `OMPDECK_CTL_PIPE` (Windows) is set.
pub const EXTENSION_SOURCE: &str = include_str!("../assets/ompdeck-ctl.ts");

/// The session-delete extension (see [`crate::omp::Omp::delete_session`]): inert unless
/// `OMPDECK_DEL_TARGET` and `OMPDECK_DEL_OUT` are set.
pub const DELETE_EXTENSION_SOURCE: &str = include_str!("../assets/ompdeck-del.ts");

const EXTENSION_FILE: &str = "ompdeck-ctl.ts";
const DELETE_EXTENSION_FILE: &str = "ompdeck-del.ts";
/// Longest accepted protocol line; longer ones are dropped.
const MAX_LINE: usize = 8 * 1024 * 1024;
const PROTOCOL_VERSION: u32 = 1;
const BACKOFF_START: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_secs(2);
/// Read/poll granularity: bounds how long the thread takes to notice shutdown or queued commands.
const POLL: Duration = Duration::from_millis(50);

/// Writes the extension to `<dir>/ompdeck-ctl.ts` if missing, different, or not a private (0600,
/// euid-owned) regular non-symlink file; returns the path. Anything else at the path (a symlink, a
/// foreign-owned or wrongly-moded file) is atomically replaced; a directory is an error.
pub fn install_extension(dir: &Path) -> io::Result<PathBuf> {
    install_asset(dir, EXTENSION_FILE, EXTENSION_SOURCE)
}

/// [`install_extension`] for the session-delete extension (`<dir>/ompdeck-del.ts`), same hardening.
pub fn install_delete_extension(dir: &Path) -> io::Result<PathBuf> {
    install_asset(dir, DELETE_EXTENSION_FILE, DELETE_EXTENSION_SOURCE)
}

fn install_asset(dir: &Path, file: &str, source: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(file);
    if os::asset_is_current(&path, source)? {
        return Ok(path);
    }
    let mut tmp = tempfile::Builder::new()
        .prefix(".ompdeck-ext")
        .tempfile_in(dir)?;
    tmp.write_all(source.as_bytes())?;
    os::seal_asset(tmp.path())?;
    tmp.persist(&path).map_err(|e| e.error)?;
    Ok(path)
}

/// Stable 64-bit FNV-1a: ctl leaf names must not change across builds or runs.
fn key_hash(key: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn leaf_name(key: &str) -> String {
    format!("{:016x}", key_hash(key))
}

/// Directory component for a tmux socket name (or `direct`): ASCII alnum/`.`/`_`/`-` kept, anything
/// else becomes `_`. Short clean names pass through unchanged; a name that is longer than 32 chars,
/// was altered by sanitizing, or is dots-only/empty becomes `<≤32-char prefix>-<8 hex of the hash
/// of the full name>` so distinct names never collide. Never empty, `.` or `..`.
fn namespace_component(ns: &str) -> String {
    let mut changed = false;
    let clean: String = ns
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                changed = true;
                '_'
            }
        })
        .collect();
    if !changed && clean.len() <= 32 && !clean.bytes().all(|b| b == b'.') {
        return clean;
    }
    let h = key_hash(ns);
    let fold = (h as u32) ^ ((h >> 32) as u32);
    let prefix: String = clean.chars().take(32).collect();
    if prefix.is_empty() || prefix.bytes().all(|b| b == b'.') {
        return format!("_-{fold:08x}");
    }
    format!("{prefix}-{fold:08x}")
}

/// `\\.\pipe\ompdeck-ctl.<user>.<namespace>.<leaf>` for a tab: `user` separates users of one
/// machine (pipe names are global), the rest mirrors the Unix `<namespace>/<leaf>` layout.
#[cfg(any(windows, test))]
fn pipe_name(user: &str, namespace: &str, key: &str) -> String {
    format!(
        r"\\.\pipe\ompdeck-ctl.{user}.{}.{}",
        namespace_component(namespace),
        leaf_name(key)
    )
}

/// `["omp", "-e", <ext>, <rest of omp args…>]`: omp loaded with the control extension. The
/// extension finds its endpoint through [`ctl_env`], set on the process (not argv).
pub fn with_ctl(omp_argv: &[String], ext: &Path) -> Vec<String> {
    let mut out = Vec::with_capacity(omp_argv.len() + 2);
    if let Some((program, rest)) = omp_argv.split_first() {
        out.push(program.clone());
        out.push("-e".to_string());
        out.push(ext.display().to_string());
        out.extend(rest.iter().cloned());
    }
    out
}

/// The environment that points the extension at `dir` (a [`ctl_dir`]) and, on Windows, lets it
/// authenticate clients: Unix `OMPDECK_CTL_DIR`; Windows `OMPDECK_CTL_PIPE` (the pipe name) and
/// `OMPDECK_CTL_TOKEN` (see [`pipe_token`]). An error (the secret file cannot be read or created)
/// means: launch without control.
pub fn ctl_env(dir: &Path) -> io::Result<Vec<(&'static str, OsString)>> {
    os::env(dir)
}

/// Capability token of one pipe: `hex(sha256(secret || pipe name))`. A pipe has no owner or mode
/// check, so the extension serves nothing to a connection that does not send this first (see
/// [`auth_line`]); the secret is per user and stable, so a restarted app can reconnect to live tabs.
#[cfg(any(windows, test))]
fn pipe_token(secret: &[u8], pipe: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(secret);
    h.update(pipe.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The first line a client sends on a pipe: `{"t":"auth","token":"<token>"}\n`.
#[cfg(any(windows, test))]
fn auth_line(token: &str) -> Vec<u8> {
    let mut line = serde_json::json!({"t": "auth", "token": token})
        .to_string()
        .into_bytes();
    line.push(b'\n');
    line
}

/// The 32-byte per-user secret at `path`, created (private, atomically) when missing or of the
/// wrong size.
#[cfg(any(windows, test))]
fn load_or_create_secret(path: &Path) -> io::Result<[u8; 32]> {
    let read = || -> Option<[u8; 32]> { fs::read(path).ok()?.try_into().ok() };
    if let Some(secret) = read() {
        return Ok(secret);
    }
    let mut fresh = [0u8; 32];
    getrandom::fill(&mut fresh).map_err(|e| io::Error::other(format!("no randomness: {e}")))?;
    crate::platform::write_private(path, &fresh)?;
    // Another instance may have created it in the same instant: whatever is on disk now wins.
    read().ok_or_else(|| io::Error::other("ctl.key is unreadable"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LiveStatus {
    Running,
    Idle,
    Parked,
    Aborted,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LiveAgent {
    pub id: String,
    pub name: String,
    pub parent: Option<String>,
    pub status: LiveStatus,
    pub activity: Option<String>,
    pub live: bool,
    pub streaming: Option<bool>,
    pub session_file: Option<PathBuf>,
    pub model: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LiveSnapshot {
    pub pid: u32,
    pub session_file: Option<PathBuf>,
    pub main_idle: Option<bool>,
    /// The main session's in-memory leaf entry id (`sessionManager.getLeafId()`).
    pub leaf: Option<String>,
    pub agents: Vec<LiveAgent>,
    /// The main session's `/context` breakdown; `None` from older extensions or on any failure.
    pub context: Option<ContextBreakdown>,
    /// The main session's in-memory model state; `None` from older extensions or on any failure.
    pub model: Option<LiveModel>,
}

/// What the main session runs right now. Unlike the session file (written from the first message
/// on) this is known for a fresh session too.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveModel {
    /// `provider/id`.
    pub selector: String,
    /// Effective level, as the file's `thinking_level_change` (no level → "off").
    pub thinking: String,
    /// What the user picked (`auto`, a level), when omp reports it.
    pub thinking_configured: Option<String>,
}

/// One row of the `/context` category meter.
#[derive(Clone, Debug, PartialEq)]
pub struct ContextCategory {
    /// omp's id: `systemPrompt`, `systemTools`, `systemContext`, `skills`, `messages`.
    pub id: String,
    pub label: String,
    pub tokens: u64,
}

/// omp's `computeSessionContextBreakdown` (what `/context` shows) plus per-tool schema tokens.
#[derive(Clone, Debug, PartialEq)]
pub struct ContextBreakdown {
    /// Model display name.
    pub model: Option<String>,
    pub context_window: u64,
    /// Provider-anchored total; not the sum of the categories.
    pub used_tokens: u64,
    /// True when `used_tokens` comes from the last assistant message's reported usage.
    pub anchored: bool,
    pub categories: Vec<ContextCategory>,
    pub auto_compact_buffer: u64,
    pub free_tokens: u64,
    /// Absolute token count at which auto-compaction triggers; `None` when it is off.
    pub threshold_tokens: Option<u64>,
    pub snapcompact_saved: Option<u64>,
    /// Per-tool schema tokens, descending.
    pub tools: Vec<(String, u64)>,
    pub skills: u32,
    pub computed_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CtlCommand {
    Kill {
        id: String,
        root: PathBuf,
    },
    Revive {
        id: String,
        root: PathBuf,
    },
    Chat {
        id: String,
        text: String,
        root: PathBuf,
    },
    /// Branch the session file at `entry_id` into a new file (live tab untouched). Reply: `new_file`.
    ForkAt {
        root: PathBuf,
        entry_id: String,
    },
    /// Move the live tab's leaf to `entry_id` (idle only). Reply: `leaf`.
    Navigate {
        root: PathBuf,
        entry_id: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct CtlReply {
    pub ok: bool,
    pub error: Option<String>,
    /// `ForkAt`: the new session file.
    pub new_file: Option<PathBuf>,
    /// `Navigate`: the leaf after the move.
    pub leaf: Option<String>,
}

// ---- wire format -------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(tag = "t")]
enum WireMsg {
    #[serde(rename = "hello")]
    Hello { v: u32 },
    #[serde(rename = "agents")]
    Agents(Box<WireSnapshot>),
    #[serde(rename = "reply")]
    Reply {
        rid: u64,
        ok: bool,
        #[serde(default)]
        error: Option<String>,
        #[serde(default, rename = "newFile")]
        new_file: Option<PathBuf>,
        #[serde(default)]
        leaf: Option<String>,
    },
}

#[derive(Deserialize)]
struct WireSnapshot {
    #[serde(default)]
    pid: u32,
    #[serde(default, rename = "sessionFile")]
    session_file: Option<PathBuf>,
    #[serde(default, rename = "mainIdle")]
    main_idle: Option<bool>,
    #[serde(default)]
    leaf: Option<String>,
    #[serde(default)]
    agents: Vec<Lenient<WireAgent>>,
    #[serde(default)]
    context: Option<Lenient<WireContext>>,
    #[serde(default)]
    model: Option<Lenient<WireModel>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireModel {
    selector: String,
    #[serde(default)]
    thinking_level: Option<String>,
    #[serde(default)]
    configured: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireContext {
    #[serde(default)]
    model: Option<String>,
    context_window: u64,
    used_tokens: u64,
    #[serde(default)]
    anchored: bool,
    #[serde(default)]
    categories: Vec<Lenient<WireCategory>>,
    #[serde(default)]
    auto_compact_buffer: u64,
    #[serde(default)]
    free_tokens: u64,
    #[serde(default)]
    threshold_tokens: Option<u64>,
    #[serde(default)]
    snapcompact_saved: Option<u64>,
    #[serde(default)]
    tools: Vec<Lenient<WireTool>>,
    #[serde(default)]
    skills: u32,
    #[serde(default)]
    computed_at: i64,
}

#[derive(Deserialize)]
struct WireCategory {
    id: String,
    #[serde(default)]
    label: Option<String>,
    tokens: u64,
}

#[derive(Deserialize)]
struct WireTool {
    name: String,
    tokens: u64,
}

impl From<WireContext> for ContextBreakdown {
    fn from(w: WireContext) -> Self {
        ContextBreakdown {
            model: w.model,
            context_window: w.context_window,
            used_tokens: w.used_tokens,
            anchored: w.anchored,
            categories: w
                .categories
                .into_iter()
                .filter_map(|c| match c {
                    Lenient::Ok(c) => Some(ContextCategory {
                        label: c.label.unwrap_or_else(|| c.id.clone()),
                        id: c.id,
                        tokens: c.tokens,
                    }),
                    Lenient::Bad(_) => None,
                })
                .collect(),
            auto_compact_buffer: w.auto_compact_buffer,
            free_tokens: w.free_tokens,
            threshold_tokens: w.threshold_tokens,
            snapcompact_saved: w.snapcompact_saved,
            tools: w
                .tools
                .into_iter()
                .filter_map(|t| match t {
                    Lenient::Ok(t) => Some((t.name, t.tokens)),
                    Lenient::Bad(_) => None,
                })
                .collect(),
            skills: w.skills,
            computed_at_ms: w.computed_at,
        }
    }
}

/// An element that failed to parse is skipped instead of failing the whole snapshot.
#[derive(Deserialize)]
#[serde(untagged)]
enum Lenient<T> {
    Ok(T),
    Bad(serde::de::IgnoredAny),
}

#[derive(Deserialize)]
struct WireAgent {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    status: LiveStatus,
    #[serde(default)]
    activity: Option<String>,
    #[serde(default)]
    live: bool,
    #[serde(default)]
    streaming: Option<bool>,
    #[serde(default, rename = "sessionFile")]
    session_file: Option<PathBuf>,
    #[serde(default)]
    model: Option<String>,
}

impl From<WireSnapshot> for LiveSnapshot {
    fn from(w: WireSnapshot) -> Self {
        let agents = w
            .agents
            .into_iter()
            .filter_map(|a| match a {
                Lenient::Ok(a) => Some(LiveAgent {
                    name: a.name.unwrap_or_else(|| a.id.clone()),
                    id: a.id,
                    parent: a.parent,
                    status: a.status,
                    activity: a.activity,
                    live: a.live,
                    streaming: a.streaming,
                    session_file: a.session_file,
                    model: a.model,
                }),
                Lenient::Bad(_) => {
                    log::debug!("ctl: skipped malformed agent entry");
                    None
                }
            })
            .collect();
        LiveSnapshot {
            pid: w.pid,
            session_file: w.session_file,
            main_idle: w.main_idle,
            leaf: w.leaf,
            agents,
            context: match w.context {
                Some(Lenient::Ok(c)) => Some(c.into()),
                Some(Lenient::Bad(_)) => {
                    log::debug!("ctl: ignored malformed context breakdown");
                    None
                }
                None => None,
            },
            model: match w.model {
                Some(Lenient::Ok(m)) if !m.selector.is_empty() => Some(LiveModel {
                    selector: m.selector,
                    thinking: m.thinking_level.unwrap_or_else(|| "off".to_owned()),
                    thinking_configured: m.configured,
                }),
                Some(Lenient::Ok(_)) | None => None,
                Some(Lenient::Bad(_)) => {
                    log::debug!("ctl: ignored malformed model state");
                    None
                }
            },
        }
    }
}

fn encode(rid: u64, cmd: &CtlCommand) -> Vec<u8> {
    let v = match cmd {
        CtlCommand::Kill { id, root } => {
            serde_json::json!({"rid": rid, "t": "kill", "id": id, "root": root})
        }
        CtlCommand::Revive { id, root } => {
            serde_json::json!({"rid": rid, "t": "revive", "id": id, "root": root})
        }
        CtlCommand::Chat { id, text, root } => {
            serde_json::json!({"rid": rid, "t": "chat", "id": id, "text": text, "root": root})
        }
        CtlCommand::ForkAt { root, entry_id } => {
            serde_json::json!({"rid": rid, "t": "fork_at", "entryId": entry_id, "root": root})
        }
        CtlCommand::Navigate { root, entry_id } => {
            serde_json::json!({"rid": rid, "t": "navigate", "entryId": entry_id, "root": root})
        }
    };
    let mut line = v.to_string().into_bytes();
    line.push(b'\n');
    line
}

// ---- client ------------------------------------------------------------------------------------

#[derive(Default)]
struct Shared {
    connected: AtomicBool,
    shutdown: AtomicBool,
    next_id: AtomicU64,
    latest: Mutex<Option<Arc<LiveSnapshot>>>,
    replies: Mutex<Vec<(u64, CtlReply)>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// One thread per client: connects to the endpoint of `dir` (`<dir>/ctl.sock` on Unix), reconnects with backoff (≤ 2 s) when the
/// socket is absent or closed; never blocks the caller. Dropping closes the socket and joins the thread.
pub struct CtlClient {
    shared: Arc<Shared>,
    commands: Sender<(u64, CtlCommand)>,
    thread: Option<JoinHandle<()>>,
}

impl CtlClient {
    /// `on_update` runs on the client thread whenever the snapshot changes, the connection state
    /// changes, or a reply arrives.
    pub fn spawn(dir: PathBuf, on_update: impl Fn() + Send + 'static) -> CtlClient {
        let shared = Arc::new(Shared::default());
        let (tx, rx) = mpsc::channel();
        let worker = Worker {
            shared: Arc::clone(&shared),
            commands: rx,
            dir,
            on_update: Box::new(on_update),
            pending: HashSet::new(),
        };
        let thread = thread::Builder::new()
            .name("omp-deck-ctl".into())
            .spawn(move || worker.run())
            .map_err(|e| log::warn!("ctl: cannot start client thread: {e}"))
            .ok();
        CtlClient {
            shared,
            commands: tx,
            thread,
        }
    }

    /// True while a socket is open and the server's `hello` (protocol v1) was received.
    pub fn connected(&self) -> bool {
        self.shared.connected.load(Ordering::Acquire)
    }

    /// `None` until the first snapshot and after a disconnect.
    pub fn latest(&self) -> Option<Arc<LiveSnapshot>> {
        lock(&self.shared.latest).clone()
    }

    /// Queued; the reply arrives via [`take_replies`](Self::take_replies) (and `on_update` fires).
    /// Not connected → the reply is `ok: false`. Returns the request id.
    pub fn send(&self, cmd: CtlCommand) -> u64 {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        if self.commands.send((id, cmd)).is_err() {
            lock(&self.shared.replies).push((id, failure("control client stopped")));
        }
        id
    }

    pub fn take_replies(&self) -> Vec<(u64, CtlReply)> {
        std::mem::take(&mut *lock(&self.shared.replies))
    }
}

impl Drop for CtlClient {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

fn failure(msg: &str) -> CtlReply {
    CtlReply {
        ok: false,
        error: Some(msg.to_string()),
        new_file: None,
        leaf: None,
    }
}

struct Worker {
    shared: Arc<Shared>,
    commands: Receiver<(u64, CtlCommand)>,
    dir: PathBuf,
    on_update: Box<dyn Fn() + Send>,
    /// Request ids written to the current connection and not yet answered.
    pending: HashSet<u64>,
}

enum Exit {
    Shutdown,
    Disconnected,
}

impl Worker {
    fn stopping(&self) -> bool {
        self.shared.shutdown.load(Ordering::Acquire)
    }

    fn run(mut self) {
        let mut backoff = BACKOFF_START;
        while !self.stopping() {
            match os::connect(&self.dir) {
                Ok(stream) => {
                    backoff = BACKOFF_START;
                    let exit = self.session(stream);
                    self.reset();
                    if matches!(exit, Exit::Shutdown) {
                        return;
                    }
                }
                Err(e) => log::trace!("ctl: connect {}: {e}", self.dir.display()),
            }
            if !self.wait(backoff) {
                return;
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    /// Sleeps up to `d` while rejecting commands (nothing is connected). False = shutdown.
    fn wait(&mut self, d: Duration) -> bool {
        match self.commands.recv_timeout(d) {
            Ok((id, _)) => {
                lock(&self.shared.replies).push((id, failure("not connected")));
                (self.on_update)();
                !self.stopping()
            }
            Err(RecvTimeoutError::Timeout) => !self.stopping(),
            Err(RecvTimeoutError::Disconnected) => false,
        }
    }

    /// Connection ended: drop the snapshot, fail unanswered requests, tell the UI.
    fn reset(&mut self) {
        let was_connected = self.shared.connected.swap(false, Ordering::AcqRel);
        let had_snapshot = lock(&self.shared.latest).take().is_some();
        let mut changed = was_connected || had_snapshot;
        if !self.pending.is_empty() {
            let mut replies = lock(&self.shared.replies);
            replies.extend(
                self.pending
                    .drain()
                    .map(|id| (id, failure("connection closed"))),
            );
            changed = true;
        }
        if changed && !self.stopping() {
            (self.on_update)();
        }
    }

    fn session(&mut self, mut stream: os::Conn) -> Exit {
        let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
        let mut chunk = vec![0u8; 64 * 1024];
        let mut discarding = false;
        let mut hello = false;
        loop {
            if self.stopping() {
                return Exit::Shutdown;
            }
            while let Ok((id, cmd)) = self.commands.try_recv() {
                if !hello {
                    lock(&self.shared.replies).push((id, failure("not connected")));
                    (self.on_update)();
                } else if stream.write_all(&encode(id, &cmd)).is_ok() {
                    self.pending.insert(id);
                } else {
                    lock(&self.shared.replies).push((id, failure("write failed")));
                    return Exit::Disconnected;
                }
            }
            match stream.read(&mut chunk) {
                Ok(0) => return Exit::Disconnected,
                Ok(n) => {
                    if !self.feed(&chunk[..n], &mut buf, &mut discarding, &mut hello) {
                        return Exit::Disconnected;
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return Exit::Disconnected,
            }
        }
    }

    /// Splits `data` into lines (bounded by [`MAX_LINE`]) and handles each. False = protocol mismatch.
    fn feed(
        &mut self,
        mut data: &[u8],
        buf: &mut Vec<u8>,
        discarding: &mut bool,
        hello: &mut bool,
    ) -> bool {
        while !data.is_empty() {
            let Some(nl) = memchr::memchr(b'\n', data) else {
                if !*discarding {
                    if buf.len() + data.len() > MAX_LINE {
                        log::warn!("ctl: oversized line dropped");
                        buf.clear();
                        *discarding = true;
                    } else {
                        buf.extend_from_slice(data);
                    }
                }
                return true;
            };
            let (head, rest) = data.split_at(nl);
            data = &rest[1..];
            if *discarding {
                *discarding = false;
                continue;
            }
            if buf.len() + head.len() > MAX_LINE {
                log::warn!("ctl: oversized line dropped");
                buf.clear();
                continue;
            }
            let ok = if buf.is_empty() {
                self.line(head, hello)
            } else {
                buf.extend_from_slice(head);
                let ok = self.line(buf, hello);
                buf.clear();
                ok
            };
            if !ok {
                return false;
            }
        }
        true
    }

    fn line(&mut self, line: &[u8], hello: &mut bool) -> bool {
        if line.iter().all(u8::is_ascii_whitespace) {
            return true;
        }
        let msg = match serde_json::from_slice::<WireMsg>(line) {
            Ok(m) => m,
            Err(e) => {
                log::debug!("ctl: ignoring malformed line: {e}");
                return true;
            }
        };
        match msg {
            WireMsg::Hello { v } => {
                if v != PROTOCOL_VERSION {
                    log::warn!("ctl: unsupported protocol version {v}");
                    return false;
                }
                *hello = true;
                if !self.shared.connected.swap(true, Ordering::AcqRel) {
                    (self.on_update)();
                }
            }
            WireMsg::Agents(s) if *hello => {
                let snap = LiveSnapshot::from(*s);
                let changed = {
                    let mut latest = lock(&self.shared.latest);
                    if latest.as_deref() == Some(&snap) {
                        false
                    } else {
                        *latest = Some(Arc::new(snap));
                        true
                    }
                };
                if changed {
                    (self.on_update)();
                }
            }
            WireMsg::Agents(_) => {}
            WireMsg::Reply {
                rid,
                ok,
                error,
                new_file,
                leaf,
            } => {
                if self.pending.remove(&rid) {
                    let reply = CtlReply {
                        ok,
                        error,
                        new_file,
                        leaf,
                    };
                    lock(&self.shared.replies).push((rid, reply));
                    (self.on_update)();
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_ctl_loads_the_extension_without_an_env_wrapper() {
        let argv = vec!["omp".to_string(), "--resume".into(), "/s/a.jsonl".into()];
        assert_eq!(
            with_ctl(&argv, Path::new("/x/ext.ts")),
            ["omp", "-e", "/x/ext.ts", "--resume", "/s/a.jsonl"]
        );
        assert!(with_ctl(&[], Path::new("/x/ext.ts")).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn ctl_env_points_the_extension_at_the_socket_dir() {
        let env = ctl_env(Path::new("/run/d")).unwrap();
        assert_eq!(env, [("OMPDECK_CTL_DIR", OsString::from("/run/d"))]);
    }

    #[test]
    fn pipe_tokens_bind_secret_and_pipe() {
        let secret = [7u8; 32];
        let t = pipe_token(&secret, r"\\.\pipe\ompdeck-ctl.u.ns.leaf");
        assert_eq!(t.len(), 64);
        assert!(
            t.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        // Pinned: sha256(secret || pipe) in lowercase hex.
        let mut input = secret.to_vec();
        input.extend_from_slice(br"\\.\pipe\ompdeck-ctl.u.ns.leaf");
        use sha2::{Digest, Sha256};
        let want: String = Sha256::digest(&input)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(t, want);
        assert_ne!(t, pipe_token(&secret, r"\\.\pipe\ompdeck-ctl.u.ns.other"));
        assert_ne!(t, pipe_token(&[8u8; 32], r"\\.\pipe\ompdeck-ctl.u.ns.leaf"));
    }

    #[test]
    fn auth_line_is_one_json_line_with_the_token() {
        let line = auth_line("abc");
        assert_eq!(line, b"{\"t\":\"auth\",\"token\":\"abc\"}\n");
    }

    #[test]
    fn the_secret_is_created_once_and_reused() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("omp-deck/ctl.key");
        let first = load_or_create_secret(&path).unwrap();
        assert_ne!(first, [0u8; 32]);
        assert_eq!(load_or_create_secret(&path).unwrap(), first);
        assert_eq!(fs::read(&path).unwrap(), first);
        // A truncated key file is replaced, not trusted.
        fs::write(&path, b"short").unwrap();
        let second = load_or_create_secret(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), second);
    }

    #[test]
    fn pipe_names_are_per_user_namespace_and_key() {
        let name = pipe_name("0123456789abcdef", "omp-deck", "omp-abc");
        assert_eq!(
            name,
            format!(
                r"\\.\pipe\ompdeck-ctl.0123456789abcdef.omp-deck.{}",
                leaf_name("omp-abc")
            )
        );
        assert_ne!(name, pipe_name("0123456789abcdef", "omp-deck", "omp-abd"));
        assert_ne!(name, pipe_name("0123456789abcdef", "other", "omp-abc"));
        assert_ne!(name, pipe_name("fedcba9876543210", "omp-deck", "omp-abc"));
        // Windows limits a pipe name to 256 characters.
        let long = pipe_name("0123456789abcdef", &"n".repeat(500), &"k".repeat(500));
        assert!(long.len() < 256, "{}", long.len());
    }

    #[test]
    fn leaf_hash_is_pinned_fnv1a() {
        // Leaf names must never change across builds: FNV-1a 64 reference vectors.
        assert_eq!(leaf_name(""), "cbf29ce484222325");
        assert_eq!(leaf_name("a"), "af63dc4c8601ec8c");
    }

    #[test]
    fn install_extension_writes_once_and_repairs() {
        let tmp = tempfile::tempdir().unwrap();
        let p = install_extension(tmp.path()).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), EXTENSION_SOURCE);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::write(&p, "stale").unwrap();
        install_extension(tmp.path()).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), EXTENSION_SOURCE);
    }

    #[test]
    fn namespace_components_are_distinct_and_bounded() {
        assert_eq!(namespace_component("omp-deck"), "omp-deck");
        assert_eq!(namespace_component("direct"), "direct");
        let long_a = format!("{}a", "x".repeat(60));
        let long_b = format!("{}b", "x".repeat(60));
        let names = [
            long_a.as_str(),
            long_b.as_str(),
            "a/b",
            "a_b",
            "a b",
            "",
            ".",
            "..",
            "_",
        ];
        let comps: HashSet<String> = names.iter().map(|n| namespace_component(n)).collect();
        assert_eq!(comps.len(), names.len(), "{comps:?}");
        for c in &comps {
            assert!(
                c.len() <= 41 && c != "." && c != ".." && !c.is_empty(),
                "{c}"
            );
            assert!(!c.contains('/'));
        }
        assert!(namespace_component(&long_a).starts_with(&"x".repeat(32)));
    }
}
