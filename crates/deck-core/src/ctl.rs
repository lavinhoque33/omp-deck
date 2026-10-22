//! Live subagent control: the in-process omp extension (`assets/ompdeck-ctl.ts`) serves NDJSON on a
//! unix socket; [`CtlClient`] connects to it on its own thread. Protocol facts:
//! `docs/notes/omp-control-ext.md`.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Deserialize;

/// The extension source: loaded with `omp -e <file>`, inert unless `OMPDECK_CTL_DIR` is set.
pub const EXTENSION_SOURCE: &str = include_str!("../assets/ompdeck-ctl.ts");

/// The session-delete extension (see [`crate::omp::Omp::delete_session`]): inert unless
/// `OMPDECK_DEL_TARGET` and `OMPDECK_DEL_OUT` are set.
pub const DELETE_EXTENSION_SOURCE: &str = include_str!("../assets/ompdeck-del.ts");

const EXTENSION_FILE: &str = "ompdeck-ctl.ts";
const DELETE_EXTENSION_FILE: &str = "ompdeck-del.ts";
const SOCKET_FILE: &str = "ctl.sock";
const LOCK_FILE: &str = "ctl.lock";
/// Longest accepted protocol line; longer ones are dropped.
const MAX_LINE: usize = 8 * 1024 * 1024;
const PROTOCOL_VERSION: u32 = 1;
const BACKOFF_START: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_secs(2);
/// Read/poll granularity: bounds how long the thread takes to notice shutdown or queued commands.
const POLL: Duration = Duration::from_millis(50);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// True when `path` is (no-follow) a regular file owned by the euid with mode exactly 0600.
fn is_private_regular_file(path: &Path, euid: u32) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|m| m.file_type().is_file() && m.uid() == euid && m.mode() & 0o777 == 0o600)
}

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
    let mut tmp = tempfile::Builder::new()
        .prefix(".ompdeck-ext")
        .tempfile_in(dir)?;
    // The temp file was just created by this process, so its owner is the effective uid.
    let me = match euid() {
        Some(u) => u,
        None => tmp.as_file().metadata()?.uid(),
    };
    if is_private_regular_file(&path, me) && fs::read(&path).is_ok_and(|b| b == source.as_bytes()) {
        return Ok(path);
    }
    tmp.write_all(source.as_bytes())?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    tmp.persist(&path).map_err(|e| e.error)?;
    Ok(path)
}

/// The effective uid, from the `Uid:` line of `/proc/self/status` (real, effective, saved, fs).
fn euid() -> Option<u32> {
    static EUID: std::sync::LazyLock<Option<u32>> = std::sync::LazyLock::new(|| {
        let status = fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find_map(|l| l.strip_prefix("Uid:"))?;
        line.split_whitespace().nth(1)?.parse().ok()
    });
    *EUID
}

/// `sockaddr_un.sun_path` capacity including the trailing NUL.
const SUN_PATH_MAX: usize = 108;

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

/// Real directory (no-follow), owned by `euid`, not group/other-writable.
fn private_dir(m: &fs::Metadata, euid: u32) -> bool {
    m.file_type().is_dir() && m.uid() == euid && m.mode() & 0o022 == 0
}

/// The directory that holds all ctl leaves: `$XDG_RUNTIME_DIR/omp-deck/ctl` when that variable is
/// set, absolute, owned by the euid and not group/other-writable; else `$XDG_CACHE_HOME/omp-deck/ctl`.
fn ctl_root() -> PathBuf {
    if let (Some(rt), Some(me)) = (
        std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()),
        euid(),
    ) {
        let rt = PathBuf::from(rt);
        if rt.is_absolute() && fs::metadata(&rt).is_ok_and(|m| private_dir(&m, me)) {
            return rt.join("omp-deck").join("ctl");
        }
    }
    // No cache dir at all yields a relative path, which `prepare_ctl_dir` rejects.
    dirs::cache_dir()
        .unwrap_or_default()
        .join("omp-deck")
        .join("ctl")
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

/// `<ctl root>/<namespace>/<16 hex of a stable 64-bit hash of key>`; socket = `<dir>/ctl.sock`.
/// `namespace` is the tmux socket name (`omp-deck`, `omp-deck-test-…`) or `direct`, so instances on
/// different sockets never share (or prune) each other's leaves. `key` is the tmux session name or a
/// direct-mode tab key. Pure apart from validating `$XDG_RUNTIME_DIR`.
pub fn ctl_dir(namespace: &str, key: &str) -> PathBuf {
    ctl_root()
        .join(namespace_component(namespace))
        .join(leaf_name(key))
}

/// Creates every missing component of `dir` with mode 0700, then verifies (no-follow) that the
/// parent root and the leaf are real directories owned by the euid without group/other write (leaf
/// exactly 0700) and that `<dir>/ctl.sock` fits `sun_path`. An error means: launch without control.
pub fn prepare_ctl_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let bad = |msg: String| io::Error::new(io::ErrorKind::PermissionDenied, msg);
    if !dir.is_absolute() {
        return Err(bad(format!("ctl dir {} is not absolute", dir.display())));
    }
    let sock_len = dir.join(SOCKET_FILE).as_os_str().len();
    if sock_len >= SUN_PATH_MAX {
        return Err(bad(format!(
            "socket path is {sock_len} bytes, over the sun_path limit"
        )));
    }
    let me = euid().ok_or_else(|| bad("cannot determine the effective uid".into()))?;
    let mut missing: Vec<&Path> = Vec::new();
    for p in dir.ancestors() {
        match fs::symlink_metadata(p) {
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::NotFound => missing.push(p),
            Err(e) => return Err(e),
        }
    }
    for p in missing.into_iter().rev() {
        match fs::DirBuilder::new().mode(0o700).create(p) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    // The namespace dir and the ctl root above it.
    for root in dir.ancestors().skip(1).take(2) {
        let m = fs::symlink_metadata(root)?;
        if !private_dir(&m, me) {
            return Err(bad(format!(
                "{} is not a private real directory owned by this user",
                root.display()
            )));
        }
    }
    let m = fs::symlink_metadata(dir)?;
    if !private_dir(&m, me) || m.mode() & 0o777 != 0o700 {
        return Err(bad(format!(
            "{} is not a real directory owned by this user with mode 0700",
            dir.display()
        )));
    }
    Ok(())
}

/// Start time (clock ticks since boot, `/proc/<pid>/stat` field 22) of `pid`; `None` if it is gone.
/// Fields after the `)` that closes the (arbitrary) comm start at field 3.
pub fn proc_start_time(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_ascii_whitespace().nth(19)?.parse().ok()
}

/// True when `<dir>/ctl.lock` names a process that is still the same one: `pid starttime` with a
/// matching start time (a bare legacy `pid` counts as live while that pid exists).
fn lock_owner_live(lock: &Path) -> bool {
    let Ok(text) = fs::read_to_string(lock) else {
        return false;
    };
    let mut it = text.split_ascii_whitespace();
    let Some(pid) = it.next().and_then(|p| p.parse::<u32>().ok()) else {
        return false;
    };
    match (
        it.next().and_then(|t| t.parse::<u64>().ok()),
        proc_start_time(pid),
    ) {
        (_, None) => false,
        (Some(want), Some(have)) => want == have,
        (None, Some(_)) => true,
    }
}

/// Overall time one prune call may spend probing sockets; leaves left unprobed count as live.
const PROBE_BUDGET: Duration = Duration::from_millis(500);
/// Longest wait for a single probe.
const PROBE_EACH: Duration = Duration::from_millis(250);

/// At most one probe thread exists process-wide: a connect stuck on a full backlog (live server)
/// must not accumulate threads across prunes.
static PROBE_BUSY: AtomicBool = AtomicBool::new(false);

/// Clears the busy flag when the probe thread ends (or fails to start).
struct BusyGuard(&'static AtomicBool);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn connect_ok(sock: &Path) -> bool {
    UnixStream::connect(sock).is_ok()
}

/// True when something accepts connections on `sock`. Anything unproven counts as live: a connect
/// that doesn't finish within `PROBE_EACH` / before `deadline` (full backlog), a probe thread still
/// stuck from an earlier call, or a failed thread spawn.
fn socket_accepts(
    sock: &Path,
    deadline: Instant,
    busy: &'static AtomicBool,
    connect: fn(&Path) -> bool,
) -> bool {
    let wait = PROBE_EACH.min(deadline.saturating_duration_since(Instant::now()));
    if wait.is_zero() || busy.swap(true, Ordering::AcqRel) {
        return true;
    }
    let guard = BusyGuard(busy);
    let (tx, rx) = mpsc::channel();
    let sock = sock.to_path_buf();
    let spawned = thread::Builder::new()
        .name("omp-deck-ctl-probe".into())
        .spawn(move || {
            let _guard = guard;
            let _ = tx.send(connect(&sock));
        });
    if spawned.is_err() {
        return true; // the closure (and its guard) was dropped: flag already cleared
    }
    rx.recv_timeout(wait).unwrap_or(true)
}

/// Real directory (no-follow) owned by `me`.
fn real_dir_of(path: &Path, me: u32) -> Option<fs::Metadata> {
    fs::symlink_metadata(path)
        .ok()
        .filter(|m| m.file_type().is_dir() && m.uid() == me)
}

/// Removes leaf dirs of `<ctl root>/<namespace>/` (and the `ctl.sock` / `ctl.lock` inside) whose name
/// isn't the hashed leaf of any key in `alive`. Refuses a symlinked or foreign root/namespace dir
/// (re-checked, with the leaf, right before each removal). Skips leaves whose lock owner is live
/// (pid + start time) or whose socket accepts a connection. Non-recursive beyond the two files; only
/// 16-hex euid-owned dirs.
pub fn prune_ctl_dirs(namespace: &str, alive: &HashSet<String>) {
    prune_with(
        &ctl_root(),
        namespace,
        alive,
        &PROBE_BUSY,
        connect_ok,
        PROBE_BUDGET,
    );
}

fn prune_with(
    root: &Path,
    namespace: &str,
    alive: &HashSet<String>,
    busy: &'static AtomicBool,
    connect: fn(&Path) -> bool,
    budget: Duration,
) {
    let deadline = Instant::now() + budget;
    let Some(me) = euid() else { return };
    let root = root.to_path_buf();
    let ns_dir = root.join(namespace_component(namespace));
    let checked = |root: &Path, ns: &Path| {
        real_dir_of(root, me).is_some_and(|m| m.mode() & 0o022 == 0)
            && real_dir_of(ns, me).is_some()
    };
    if !checked(&root, &ns_dir) {
        return;
    }
    let keep: HashSet<String> = alive.iter().map(|k| leaf_name(k)).collect();
    let Ok(entries) = fs::read_dir(&ns_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.len() != 16 || !name.bytes().all(|b| b.is_ascii_hexdigit()) || keep.contains(name) {
            continue;
        }
        let dir = ns_dir.join(name);
        if real_dir_of(&dir, me).is_none() {
            continue;
        }
        let lock = dir.join(LOCK_FILE);
        let sock = dir.join(SOCKET_FILE);
        if lock_owner_live(&lock)
            || (fs::symlink_metadata(&sock).is_ok()
                && socket_accepts(&sock, deadline, busy, connect))
        {
            log::debug!("prune ctl dir {}: owner is live", dir.display());
            continue;
        }
        for p in [&sock, &lock] {
            // Re-validate the whole chain just before each removal.
            if !checked(&root, &ns_dir) || real_dir_of(&dir, me).is_none() {
                return;
            }
            if fs::symlink_metadata(p).is_ok_and(|m| !m.file_type().is_dir() && m.uid() == me) {
                let _ = fs::remove_file(p);
            }
        }
        if let Err(e) = fs::remove_dir(&dir) {
            log::debug!("prune ctl dir {}: {e}", dir.display());
        }
    }
}

/// `["env", "OMPDECK_CTL_DIR=<dir>", <omp>, "-e", <ext>, <rest of omp args…>]`.
pub fn with_ctl(omp_argv: &[String], ext: &Path, dir: &Path) -> Vec<String> {
    let mut out = Vec::with_capacity(omp_argv.len() + 4);
    out.push("env".to_string());
    out.push(format!("OMPDECK_CTL_DIR={}", dir.display()));
    if let Some((program, rest)) = omp_argv.split_first() {
        out.push(program.clone());
        out.push("-e".to_string());
        out.push(ext.display().to_string());
        out.extend(rest.iter().cloned());
    }
    out
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

/// One thread per client: connects to `<dir>/ctl.sock`, reconnects with backoff (≤ 2 s) when the
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
            sock: dir.join(SOCKET_FILE),
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
    sock: PathBuf,
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
            match UnixStream::connect(&self.sock) {
                Ok(stream) => {
                    backoff = BACKOFF_START;
                    let exit = self.session(stream);
                    self.reset();
                    if matches!(exit, Exit::Shutdown) {
                        return;
                    }
                }
                Err(e) => log::trace!("ctl: connect {}: {e}", self.sock.display()),
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

    fn session(&mut self, mut stream: UnixStream) -> Exit {
        if stream.set_read_timeout(Some(POLL)).is_err()
            || stream.set_write_timeout(Some(WRITE_TIMEOUT)).is_err()
        {
            return Exit::Disconnected;
        }
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
    fn with_ctl_prefixes_argv() {
        let argv = vec!["omp".to_string(), "--resume".into(), "/s/a.jsonl".into()];
        let out = with_ctl(&argv, Path::new("/x/ext.ts"), Path::new("/run/d"));
        assert_eq!(
            out,
            [
                "env",
                "OMPDECK_CTL_DIR=/run/d",
                "omp",
                "-e",
                "/x/ext.ts",
                "--resume",
                "/s/a.jsonl"
            ]
        );
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
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
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

    static STUCK_BUSY: AtomicBool = AtomicBool::new(false);

    fn stuck_connect(_: &Path) -> bool {
        thread::sleep(Duration::from_secs(3));
        true
    }

    #[test]
    fn prune_returns_within_budget_with_stuck_probe_and_keeps_leaves() {
        let tmp = tempfile::tempdir().unwrap();
        let ns = tmp.path().join("ns");
        let leaves: Vec<PathBuf> = (0..4).map(|i| ns.join(format!("{i:016x}"))).collect();
        for l in &leaves {
            fs::create_dir_all(l).unwrap();
            fs::write(l.join(SOCKET_FILE), "").unwrap();
        }
        let run = || {
            let t = Instant::now();
            prune_with(
                tmp.path(),
                "ns",
                &HashSet::new(),
                &STUCK_BUSY,
                stuck_connect,
                Duration::from_millis(300),
            );
            t.elapsed()
        };
        let took = run();
        assert!(took < Duration::from_millis(900), "prune took {took:?}");
        assert!(leaves.iter().all(|l| l.is_dir()), "unproven leaves kept");
        // The first probe is still stuck: a second prune spawns no other and returns at once.
        assert!(STUCK_BUSY.load(Ordering::Acquire));
        assert!(run() < Duration::from_millis(100));
        assert!(leaves.iter().all(|l| l.is_dir()));
    }
}
