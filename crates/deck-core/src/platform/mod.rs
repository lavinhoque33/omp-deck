//! One home for OS differences: everything that needs `std::os::unix`, `/proc`, Win32 or a
//! per-OS convention lives here (or in the `cfg` branches of [`crate::ctl`]). The rest of the
//! crate calls these functions and never names an OS.

use std::ffi::OsStr;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Write};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::SystemTime;

#[cfg(unix)]
#[path = "unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "windows.rs"]
mod imp;

#[cfg(windows)]
pub use imp::user_tag;
#[cfg(unix)]
pub use imp::{effective_uid, process_start_ticks};
pub use imp::{
    file_identity, file_mode, open_regular_file, root_dir, set_file_mode, tree_open_files,
};

// ---- process / machine -------------------------------------------------------------------------

/// This machine's hostname; `None` when the OS reports none.
pub fn hostname() -> Option<String> {
    imp::hostname_os()
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
}

/// Wall-clock start of process `pid`; `None` when it is gone or the OS cannot tell (macOS, BSDs).
pub fn process_start(pid: u32) -> Option<SystemTime> {
    imp::process_start_os(pid)
}

/// Home directory, else the filesystem root: the cwd of last resort.
pub fn home_or_root() -> PathBuf {
    dirs::home_dir().unwrap_or_else(root_dir)
}

/// Keeps a console child from flashing a window when the GUI is a Windows-subsystem process.
/// No-op elsewhere. [`KillableCommand`] applies it itself.
pub fn no_window(cmd: &mut Command) -> &mut Command {
    imp::no_window(cmd)
}

// ---- finding programs --------------------------------------------------------------------------

/// Executable extensions Windows resolves bare program names with (`PATHEXT`, lowercase, with the
/// leading dot; the system default when unset). Empty on Unix.
pub fn path_extensions() -> Vec<String> {
    imp::path_extensions()
}

/// True for a regular file the OS would run: any execute bit (Unix) or a `PATHEXT` extension
/// (Windows).
pub fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && imp::executable(path, &m))
}

/// First hit of `name` in `dirs`: on Windows `name` itself when it already carries one of `exts`,
/// else `name` + each of `exts` in order; on Unix (`exts` empty) `name` itself. `accept` decides
/// whether a candidate counts.
pub fn find_in(
    dirs: impl IntoIterator<Item = PathBuf>,
    name: &str,
    exts: &[String],
    accept: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let has_ext = Path::new(name)
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|e| exts.iter().any(|x| x[1..].eq_ignore_ascii_case(e)));
    let candidates: Vec<String> = if exts.is_empty() || has_ext {
        vec![name.to_owned()]
    } else {
        exts.iter().map(|x| format!("{name}{x}")).collect()
    };
    dirs.into_iter()
        .flat_map(|d| candidates.iter().map(move |c| d.join(c)))
        .find(|p| accept(p))
}

/// The first executable `name` on `PATH` (with `PATHEXT` on Windows).
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    find_in(
        std::env::split_paths(&path),
        name,
        &path_extensions(),
        is_executable,
    )
}

// ---- private files and modes -------------------------------------------------------------------

/// Creates one directory readable by this user only (mode 0700; Windows: inherits the ACL of its
/// parent, which lives under the user profile).
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    imp::private_dir_builder(false).create(path)
}

/// [`create_private_dir`] with every missing ancestor.
pub fn create_private_dir_all(path: &Path) -> io::Result<()> {
    imp::private_dir_builder(true).create(path)
}

/// Creates a new file (it must not exist) for writing, mode 0600 (Windows: parent ACL).
pub fn open_private_file(path: &Path) -> io::Result<File> {
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    imp::private_file_options(&mut o);
    o.open(path)
}

/// Writes `bytes` to `path` atomically (a sibling `.tmp` renamed into place), creating parent
/// directories and the file private.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let write = || -> io::Result<()> {
        let _ = fs::remove_file(&tmp);
        let mut f = open_private_file(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    };
    write().inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// Identity of a file independent of its name: equal ids = same file (Unix device + inode;
/// Windows volume serial + file index).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileId(u64, u64);

/// A symlink or junction at `meta` (from `symlink_metadata`).
pub fn is_link(meta: &Metadata) -> bool {
    meta.file_type().is_symlink()
}

/// Bytes of `path` as the OS stores it (Unix raw bytes; Windows WTF-8): stable, for hashing and
/// percent-encoding. Never shown to users.
pub fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_encoded_bytes()
}

// ---- killable process trees --------------------------------------------------------------------

/// A [`Command`] whose whole process tree can be killed: it leads its own process group (Unix) or
/// runs in a job object (Windows).
pub struct KillableCommand {
    cmd: Command,
}

impl KillableCommand {
    pub fn new(cmd: Command) -> KillableCommand {
        KillableCommand { cmd }
    }

    /// Spawns the command. Windows: the child starts suspended, joins its kill-on-close job and
    /// only then runs (the spawn fails if it cannot join), so no descendant escapes the job; the
    /// command's creation flags are replaced. Unix: the child leads a new process group.
    pub fn spawn(mut self) -> io::Result<KillableChild> {
        let (child, tree) = imp::spawn(&mut self.cmd)?;
        Ok(KillableChild { child, tree })
    }
}

impl Deref for KillableCommand {
    type Target = Command;
    fn deref(&self) -> &Command {
        &self.cmd
    }
}

impl DerefMut for KillableCommand {
    fn deref_mut(&mut self) -> &mut Command {
        &mut self.cmd
    }
}

/// [`KillableCommand::new`] + [`spawn`](KillableCommand::spawn).
pub fn spawn_killable(cmd: Command) -> io::Result<KillableChild> {
    KillableCommand::new(cmd).spawn()
}

/// A running child plus its process tree. Dropping kills the tree and reaps the child on every
/// exit path, so no descendant outlives the owner or keeps an output pipe open.
pub struct KillableChild {
    child: Child,
    tree: imp::Tree,
}

impl KillableChild {
    /// Kills every process of the tree (the child and its descendants); the child is not reaped.
    pub fn kill_tree(&self) {
        self.tree.kill();
    }
}

impl Deref for KillableChild {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.child
    }
}

impl DerefMut for KillableChild {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

impl Drop for KillableChild {
    fn drop(&mut self) {
        self.tree.kill();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---- opening things for the user ----------------------------------------------------------------

/// Opens `target` (a path or an `http(s)`/`mailto` URL) with the desktop's default handler
/// (`xdg-open`, `ShellExecuteW`). Blocks until the opener returned: call off the UI thread.
pub fn open_path(target: &OsStr) -> io::Result<()> {
    imp::open_path_os(target)
}

/// Shows `file` in the desktop file manager, selected where the manager supports it (Windows
/// `explorer /select,`), else its parent directory is opened. Blocks like [`open_path`].
pub fn reveal(file: &Path) -> io::Result<()> {
    imp::reveal_os(file)
}

#[cfg(test)]
mod tests;
