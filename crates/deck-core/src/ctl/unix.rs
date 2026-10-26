//! Unix endpoint of the ctl channel: a private directory per tab holding `ctl.sock` (and the
//! extension's `ctl.lock`), hardened with owner/mode checks; plus pruning of dead leaves.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::{POLL, leaf_name, namespace_component};
use crate::platform;

/// The extension picks its endpoint from this variable.
pub(super) const ENV_VAR: &str = "OMPDECK_CTL_DIR";
/// A command line is small: a server that does not take it in this long is stuck.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const SOCKET_FILE: &str = "ctl.sock";
const LOCK_FILE: &str = "ctl.lock";

/// The extension's environment for the socket dir `dir`.
pub(super) fn env(
    dir: &Path,
    _key_file: &Path,
) -> io::Result<Vec<(&'static str, std::ffi::OsString)>> {
    Ok(vec![(ENV_VAR, dir.as_os_str().to_owned())])
}

/// The client side of an endpoint: a unix stream with the session's read/write timeouts set.
pub(super) type Conn = UnixStream;

/// Connects to `<dir>/ctl.sock`.
pub(super) fn connect(dir: &Path, _key_file: &Path) -> io::Result<Conn> {
    let stream = UnixStream::connect(dir.join(SOCKET_FILE))?;
    stream.set_read_timeout(Some(POLL))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    Ok(stream)
}

/// True when `path` is (no-follow) a regular file owned by us with mode exactly 0600 and the
/// content `source`: such an installed extension is reused as is.
pub(super) fn asset_is_current(path: &Path, source: &str) -> io::Result<bool> {
    let Some(me) = euid() else { return Ok(false) };
    Ok(is_private_regular_file(path, me) && fs::read(path).is_ok_and(|b| b == source.as_bytes()))
}

/// Makes the staged extension file private (0600) before it replaces the installed one.
pub(super) fn seal_asset(path: &Path) -> io::Result<()> {
    platform::set_file_mode(path, 0o600)
}

/// The effective uid.
fn euid() -> Option<u32> {
    platform::effective_uid()
}

/// True when `path` is (no-follow) a regular file owned by the euid with mode exactly 0600.
fn is_private_regular_file(path: &Path, euid: u32) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|m| m.file_type().is_file() && m.uid() == euid && m.mode() & 0o777 == 0o600)
}

/// `sockaddr_un.sun_path` capacity including the trailing NUL.
const SUN_PATH_MAX: usize = 108;

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
        match platform::create_private_dir(p) {
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
pub fn proc_start_time(pid: u32) -> Option<u64> {
    platform::process_start_ticks(pid)
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

#[cfg(test)]
mod tests {
    use super::*;

    static STUCK_BUSY: AtomicBool = AtomicBool::new(false);

    fn stuck_connect(_: &Path) -> bool {
        thread::sleep(Duration::from_secs(3));
        true
    }

    #[test]
    fn prune_returns_within_budget_with_stuck_probe_and_keeps_leaves() {
        // Explicit 0700: under a group-writable umask (0002) a default tempdir is 0775, which
        // `prune_with` rightly refuses, so nothing would be probed.
        let tmp = tempfile::Builder::new()
            .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .tempdir()
            .unwrap();
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
