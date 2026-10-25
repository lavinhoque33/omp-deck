//! Unix side of [`super`].

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, OsStr};
use std::fs::{self, DirBuilder, File, Metadata, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::FileId;

pub fn root_dir() -> PathBuf {
    PathBuf::from("/")
}

pub fn hostname_os() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is valid for `buf.len()` bytes; gethostname writes at most that many.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    // POSIX does not promise NUL termination on truncation.
    *buf.last_mut()? = 0;
    CStr::from_bytes_until_nul(&buf)
        .ok()
        .map(|c| c.to_string_lossy().into_owned())
}

/// The effective uid.
pub fn effective_uid() -> Option<u32> {
    // SAFETY: geteuid has no arguments and cannot fail.
    Some(unsafe { libc::geteuid() })
}

/// Start time of `pid` in clock ticks since boot (`/proc/<pid>/stat` field 22, USER_HZ = 100);
/// `None` when the process is gone or the OS has no procfs. The fields after the `)` that closes
/// the (arbitrary) comm start at field 3.
pub fn process_start_ticks(pid: u32) -> Option<u64> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_ascii_whitespace().nth(19)?.parse().ok()
}

pub fn process_start_os(pid: u32) -> Option<SystemTime> {
    const USER_HZ: u64 = 100;
    let ticks = process_start_ticks(pid)?;
    let btime: u64 = fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    Some(UNIX_EPOCH + Duration::from_millis(btime * 1000 + ticks * 1000 / USER_HZ))
}

/// For each pid of `roots`: the files held open by that process and all its descendants
/// (`/proc` on Linux; empty elsewhere). Vanished or unreadable processes are skipped.
pub fn tree_open_files(roots: &[u32]) -> Vec<Vec<PathBuf>> {
    if !cfg!(target_os = "linux") {
        return roots.iter().map(|_| Vec::new()).collect();
    }
    let children = proc_children();
    roots
        .iter()
        .map(|&root| {
            let mut files = Vec::new();
            let mut stack = vec![root];
            let mut visited = HashSet::new();
            while let Some(pid) = stack.pop() {
                if !visited.insert(pid) {
                    continue;
                }
                if let Some(kids) = children.get(&pid) {
                    stack.extend(kids);
                }
                files.extend(open_files(pid));
            }
            files
        })
        .collect()
}

/// ppid → children of every process (`/proc/*/stat`, Linux; empty elsewhere).
pub fn process_children() -> HashMap<u32, Vec<u32>> {
    proc_children()
}

/// Bytes charged to `pid`: `Pss` from `/proc/<pid>/smaps_rollup`, else `VmRSS` from
/// `/proc/<pid>/status` (smaps_rollup is unreadable for other users' processes). `None` when the
/// process is gone or has neither (kernel threads, no procfs).
pub fn process_memory(pid: u32) -> Option<u64> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let kb_field = |path: String, key: &str| -> Option<u64> {
        let text = fs::read_to_string(path).ok()?;
        let line = text.lines().find_map(|l| l.strip_prefix(key))?;
        line.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()
    };
    kb_field(format!("/proc/{pid}/smaps_rollup"), "Pss:")
        .or_else(|| kb_field(format!("/proc/{pid}/status"), "VmRSS:"))
        .map(|kb| kb.saturating_mul(1024))
}

/// ppid → children, from `/proc/*/stat`.
fn proc_children() -> HashMap<u32, Vec<u32>> {
    let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
    let Ok(rd) = fs::read_dir("/proc") else {
        return map;
    };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // "pid (comm) S ppid ..." — comm may contain spaces/parens, so split after the last ')'.
        let ppid = stat
            .rfind(')')
            .and_then(|i| stat[i + 1..].split_whitespace().nth(1))
            .and_then(|s| s.parse::<u32>().ok());
        if let Some(ppid) = ppid {
            map.entry(ppid).or_default().push(pid);
        }
    }
    map
}

fn open_files(pid: u32) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return Vec::new(); // gone or permission denied
    };
    rd.flatten()
        .filter_map(|e| fs::read_link(e.path()).ok())
        .collect()
}

pub fn no_window(cmd: &mut Command) -> &mut Command {
    cmd
}

pub fn path_extensions() -> Vec<String> {
    Vec::new()
}

pub fn executable(_path: &Path, meta: &Metadata) -> bool {
    meta.permissions().mode() & 0o111 != 0
}

pub fn private_dir_builder(recursive: bool) -> DirBuilder {
    let mut b = DirBuilder::new();
    b.recursive(recursive).mode(0o700);
    b
}

pub fn private_file_options(o: &mut OpenOptions) {
    o.mode(0o600);
}

pub fn file_mode(_path: &Path, meta: &Metadata) -> u32 {
    meta.permissions().mode() & 0o777
}

pub fn set_file_mode(path: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(path, Permissions::from_mode(mode))
}

pub fn file_identity(file: &File) -> io::Result<FileId> {
    let m = file.metadata()?;
    Ok(FileId(m.dev(), m.ino()))
}

/// Opens a regular file for reading without following a symlink and without ever blocking on a
/// FIFO: lstat says regular, `O_NONBLOCK` keeps a swapped-in FIFO from hanging the open, and the
/// opened descriptor must be the lstat'd file.
pub fn open_regular_file(path: &Path) -> io::Result<File> {
    let lmd = fs::symlink_metadata(path)?;
    if !lmd.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    let md = file.metadata()?;
    if (md.dev(), md.ino()) != (lmd.dev(), lmd.ino()) || !md.file_type().is_file() {
        return Err(io::Error::other("file changed while opening"));
    }
    Ok(file)
}

/// Process-group handle: the child leads its own group, so its pid is the group id.
pub struct Tree {
    pgid: i32,
}

/// Starts `cmd` as the leader of its own process group, which [`Tree::kill`] signals as a whole.
pub fn spawn(cmd: &mut Command) -> io::Result<(Child, Tree)> {
    cmd.process_group(0);
    let child = cmd.spawn()?;
    let pgid = i32::try_from(child.id()).unwrap_or(0);
    Ok((child, Tree { pgid }))
}

impl Tree {
    pub fn kill(&self) {
        if self.pgid > 0 {
            // SAFETY: plain syscall with integer arguments; no memory is accessed.
            unsafe {
                libc::kill(-self.pgid, libc::SIGKILL);
            }
        }
    }
}

/// `xdg-open <target>`, reaped; a non-zero exit (no handler, handler failed) is an error.
pub fn open_path_os(target: &OsStr) -> io::Result<()> {
    let status = Command::new("xdg-open")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?
        .wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("exited with {status}")))
    }
}

/// xdg-open has no "select this file": the containing directory is opened.
pub fn reveal_os(file: &Path) -> io::Result<()> {
    open_path_os(
        file.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(file)
            .as_os_str(),
    )
}

/// What keeps a [`super::NamedLock`] held: the bound abstract socket.
#[cfg(target_os = "linux")]
pub type Held = std::os::unix::net::UnixDatagram;
/// What keeps a [`super::NamedLock`] held: the file whose open description owns the `flock`.
#[cfg(not(target_os = "linux"))]
pub type Held = File;

#[cfg(target_os = "linux")]
pub fn try_named_lock(name: &str, _path: &Path) -> io::Result<Option<Held>> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};
    let address = SocketAddr::from_abstract_name(name.as_bytes())?;
    match UnixDatagram::bind_addr(&address) {
        Ok(socket) => Ok(Some(socket)),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn try_named_lock(_name: &str, path: &Path) -> io::Result<Option<Held>> {
    use std::os::fd::AsRawFd;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    // SAFETY: `file` owns a live descriptor for the duration of the call; a non-blocking
    // `flock` only changes the advisory lock attached to that open file description.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let e = io::Error::last_os_error();
    if e.kind() == io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(e)
    }
}
