//! Unix domain socket endpoints: `<base>/<ns>/<name>.sock`, a `<name>.lock` flock held by the
//! daemon for its lifetime (first instance wins), base and namespace directories 0700 and
//! owned by the current user.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::namespace::Namespace;

pub(crate) struct Endpoint {
    name: String,
    base: PathBuf,
    dir: PathBuf,
    socket: PathBuf,
    lock: PathBuf,
}

impl Endpoint {
    pub fn new(ns: &Namespace, name: &str) -> Result<Endpoint> {
        let dir = ns.dir();
        let socket = dir.join(format!("{name}.sock"));
        // SAFETY: sockaddr_un is plain old data; all-zero is a valid value.
        let limit = unsafe { std::mem::zeroed::<libc::sockaddr_un>() }
            .sun_path
            .len();
        let len = socket.as_os_str().len();
        // sun_path needs room for the terminating NUL.
        if len >= limit {
            return Err(Error::PathTooLong {
                path: socket,
                len,
                limit: limit - 1,
            });
        }
        Ok(Endpoint {
            name: name.to_owned(),
            base: ns.base().to_path_buf(),
            lock: dir.join(format!("{name}.lock")),
            dir,
            socket,
        })
    }
}

/// Verifies `path` is a real directory owned by us without group/other access.
fn check_private_dir(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    let insecure = |reason: String| Error::InsecureDir {
        path: path.to_path_buf(),
        reason,
    };
    if !meta.file_type().is_dir() {
        return Err(insecure("not a directory".into()));
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    if meta.uid() != uid {
        return Err(insecure(format!(
            "owned by uid {} instead of {uid}",
            meta.uid()
        )));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(insecure(format!(
            "mode {:o} grants group/other access",
            meta.mode() & 0o777
        )));
    }
    Ok(())
}

/// Checks the base and namespace directories if they exist. `Ok(false)` if either is missing.
fn existing_dirs_ok(base: &Path, dir: &Path) -> Result<bool> {
    for path in [base, dir] {
        match check_private_dir(path) {
            Ok(()) => {}
            Err(Error::Io(e)) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

fn create_private_dir(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    check_private_dir(path)
}

/// Creates (or verifies) the base and namespace directories.
pub(crate) fn prepare(endpoint: &Endpoint) -> Result<()> {
    if let Some(parent) = endpoint.base.parent() {
        fs::create_dir_all(parent)?;
    }
    create_private_dir(&endpoint.base)?;
    create_private_dir(&endpoint.dir)
}

pub(crate) fn connect(endpoint: &Endpoint) -> Result<Stream> {
    if !existing_dirs_ok(&endpoint.base, &endpoint.dir)? {
        return Err(Error::NotRunning(endpoint.name.clone()));
    }
    match UnixStream::connect(&endpoint.socket) {
        Ok(s) => Ok(Stream(s)),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Err(Error::NotRunning(endpoint.name.clone()))
        }
        Err(e) => Err(e.into()),
    }
}

/// Session names with a socket in the namespace directory (liveness not checked).
pub(crate) fn list_names(ns: &Namespace) -> Result<Vec<String>> {
    let dir = ns.dir();
    if !existing_dirs_ok(ns.base(), &dir)? {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let file = entry.file_name();
        if let Some(name) = file.to_str().and_then(|f| f.strip_suffix(".sock")) {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// The daemon's listening socket plus its instance lock.
pub(crate) struct Listener {
    inner: UnixListener,
    _lock: File,
    socket: PathBuf,
    lock_path: PathBuf,
}

impl Listener {
    /// Takes the instance lock and binds the socket (replacing a stale one).
    pub fn bind(endpoint: &Endpoint) -> Result<Listener> {
        prepare(endpoint)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC)
            .open(&endpoint.lock)?;
        // SAFETY: valid open fd for the duration of the call.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = io::Error::last_os_error();
            return Err(if e.kind() == io::ErrorKind::WouldBlock {
                Error::AlreadyRunning(endpoint.name.clone())
            } else {
                Error::Os {
                    call: "flock",
                    source: e,
                }
            });
        }
        // We hold the lock, so any existing socket is stale.
        match fs::remove_file(&endpoint.socket) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let inner = UnixListener::bind(&endpoint.socket)?;
        Ok(Listener {
            inner,
            _lock: lock,
            socket: endpoint.socket.clone(),
            lock_path: endpoint.lock.clone(),
        })
    }

    pub fn accept(&self) -> io::Result<Stream> {
        self.inner.accept().map(|(s, _)| Stream(s))
    }

    /// Removes the endpoint so new clients see the session as gone.
    pub fn remove(&self) {
        let _ = fs::remove_file(&self.socket);
        let _ = fs::remove_file(&self.lock_path);
    }
}

/// A connected byte stream; clones share the connection.
pub(crate) struct Stream(UnixStream);

impl Stream {
    pub fn try_clone(&self) -> io::Result<Stream> {
        self.0.try_clone().map(Stream)
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.set_read_timeout(timeout)
    }

    /// Ends the connection in both directions; blocked reads on clones return EOF.
    pub fn shutdown(&self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }

    /// Ends the connection without waiting for the peer (same as [`Stream::shutdown`] here:
    /// never blocks).
    pub fn abort(&self) {
        self.shutdown();
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
